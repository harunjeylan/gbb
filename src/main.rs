//! gbb — cargo build broker for parallel agents.
//!
//! Guarantees:
//!   * one build per build directory (cargo's own lock is per profile dir),
//!   * global memory-aware slots shared across every checkout,
//!   * identical concurrent requests collapse into one build,
//!   * a blocked agent exits 75 immediately instead of hanging on a lock.

mod classify;
mod digest;
mod gc;
mod jobs;
mod runner;
mod slots;
mod target;

use classify::Class;
use std::env;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

const EXIT_BUSY: u8 = 75;
const EXIT_FAIL: u8 = 1;
const EXIT_USAGE: u8 = 2;

const HELP: &str = "\
gbb — cargo build broker for parallel agents

USAGE
  gbb [options] <cargo args...>
  gbb status | doctor | gc | wait <key> | help | version

OPTIONS (before the cargo subcommand)
  --wait <secs>    block up to this long for a slot/lock (default 0 = fail fast)
  --no-dedupe      never join or reuse an identical job
  --slot <class>   force slot class: light | heavy
  --priority       same as --slot heavy (merge gates)

EXIT CODES
  0 ok · 1 build/test failure · 2 usage · 75 busy (retry later or `gbb wait <key>`)

ENV
  GBB_WAIT            default for --wait
  GBB_LIGHT_SLOTS     concurrent light jobs (default 4)
  GBB_HEAVY_SLOTS     concurrent heavy jobs (default 2)
  GBB_JOBS            CARGO_BUILD_JOBS for launched jobs
  GBB_SLICE           systemd slice (default dev.slice)
  GBB_NO_SYSTEMD      launch cargo directly instead of under systemd-run
  GBB_NO_SCCACHE      do not inject RUSTC_WRAPPER=sccache
  GBB_REUSE_SECS      reuse a just-finished identical job (default 120)
";

struct Opts {
    wait: Duration,
    dedupe: bool,
    class: Option<Class>,
    cargo_args: Vec<String>,
}

fn main() -> ExitCode {
    let argv: Vec<String> = env::args().skip(1).collect();
    let Some(first) = argv.first().cloned() else {
        print!("{HELP}");
        return ExitCode::from(EXIT_USAGE);
    };

    match first.as_str() {
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        "version" | "--version" | "-V" => {
            println!("gbb {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "status" => report(status()),
        "doctor" => report(doctor()),
        "gc" => report(gc_command(&argv[1..])),
        "wait" => report(wait_command(&argv[1..])),
        _ => {
            let (opts, err) = parse(&argv);
            if let Some(message) = err {
                eprintln!("gbb: {message}\n\n{HELP}");
                return ExitCode::from(EXIT_USAGE);
            }
            report(run(opts))
        }
    }
}

fn report(result: io::Result<u8>) -> ExitCode {
    match result {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("gbb: {e}");
            ExitCode::from(EXIT_FAIL)
        }
    }
}

fn parse(argv: &[String]) -> (Opts, Option<String>) {
    let mut opts = Opts {
        wait: env::var("GBB_WAIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::ZERO),
        dedupe: true,
        class: None,
        cargo_args: Vec::new(),
    };

    let mut index = 0;
    while index < argv.len() {
        let arg = &argv[index];
        match arg.as_str() {
            "--wait" => {
                index += 1;
                let Some(value) = argv.get(index) else {
                    return (opts, Some("--wait needs a number of seconds".into()));
                };
                match value.parse::<u64>() {
                    Ok(secs) => opts.wait = Duration::from_secs(secs),
                    Err(_) => return (opts, Some(format!("invalid --wait value: {value}"))),
                }
            }
            "--no-dedupe" => opts.dedupe = false,
            "--priority" => opts.class = Some(Class::Heavy),
            "--slot" => {
                index += 1;
                let Some(value) = argv.get(index) else {
                    return (opts, Some("--slot needs light or heavy".into()));
                };
                match Class::parse(value) {
                    Some(class) => opts.class = Some(class),
                    None => return (opts, Some(format!("invalid --slot value: {value}"))),
                }
            }
            _ => {
                opts.cargo_args.extend_from_slice(&argv[index..]);
                break;
            }
        }
        index += 1;
    }

    if opts.cargo_args.is_empty() {
        return (opts, Some("no cargo command given".into()));
    }
    (opts, None)
}

fn run(opts: Opts) -> io::Result<u8> {
    let cwd = env::current_dir()?;
    let resolved = match target::resolve(&cwd) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gbb: {e}");
            return Ok(EXIT_USAGE);
        }
    };

    let class = opts
        .class
        .unwrap_or_else(|| classify::classify(&opts.cargo_args));
    let cmd = classify::subcommand(&opts.cargo_args).unwrap_or("").to_string();

    let join_wait = Duration::from_secs(env_u64("GBB_JOIN_WAIT", 900));
    let lock_wait = if opts.wait > Duration::ZERO {
        opts.wait
    } else {
        Duration::from_secs(env_u64("GBB_LOCK_WAIT", 240))
    };

    // 1. An identical command already owns this build directory: join it.
    //    Compare argv, not the input digest: the first build may create
    //    Cargo.lock, which would change the digest for every later agent.
    if opts.dedupe {
        if let Some((owner_key, _pid, _started, owner_args)) = jobs::read_owner(&resolved.target) {
            let same_command = owner_args == opts.cargo_args.join(" ");
            let live = jobs::read(&owner_key).map(|j| j.is_live()).unwrap_or(false);
            if same_command && live {
                eprintln!(
                    "gbb: identical command already running here (key {}, pid {}) — joining instead of duplicating",
                    short(&owner_key),
                    _pid
                );
                return wait_for_job(&owner_key, join_wait);
            }
        }
    }

    // 2. Global memory-aware slot.
    let Some(_slot) = slots::acquire(class, opts.wait)? else {
        return Ok(busy(
            class,
            &cmd,
            &resolved.target,
            None,
            "slot",
            "no free slot; nothing was started",
        ));
    };

    // 3. One build per build directory, at cargo's own profile granularity.
    let profile = profile_of(&opts.cargo_args);
    let lock_path = profile_lock_path(&resolved.target, &profile);
    let _lock = match acquire_build_lock(&lock_path, lock_wait)? {
        Some(file) => file,
        None => {
            return Ok(busy(
                class,
                &cmd,
                &resolved.target,
                None,
                "build-dir",
                "another gbb job owns this build directory and did not finish in time",
            ))
        }
    };

    if let Some(foreign) = foreign_cargo_lock(&resolved.target) {
        eprintln!("gbb: warning: bare cargo appears to hold {}", foreign.display());
        eprintln!("gbb:          it will not be serialized against; prefer `gbb <cmd>` everywhere");
    }

    // 4. Digest is only meaningful once the tree is quiet, i.e. after the lock.
    let started = Instant::now();
    let digest = digest::workspace_digest(&resolved.root)?;
    let digest_ms = started.elapsed().as_millis();
    let key = digest::job_key(&resolved.target, class.as_str(), &opts.cargo_args, &digest);

    // 4. Someone with the same key may have finished while we queued.
    if opts.dedupe && reuse_allowed(&cmd, &opts.cargo_args) {
        if let Some(job) = jobs::read(&key) {
            let window = env::var("GBB_REUSE_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(120u64);
            let finished_ago = jobs::now_epoch().saturating_sub(job.finished);
            if job.state == jobs::State::Done && job.finished > 0 && finished_ago <= window {
                eprintln!(
                    "gbb: reusing identical job finished {}s ago (key {}, exit {})",
                    finished_ago,
                    short(&key),
                    job.exit
                );
                replay(&key);
                return Ok(job.exit as u8);
            }
        }
    }

    // 5. Build.
    jobs::prune_logs(24 * 60 * 60);
    let log = jobs::log_path(&key);
    let mut job = jobs::Job::started_running(
        &key,
        std::process::id(),
        class.as_str(),
        &resolved.target,
        &opts.cargo_args,
    );
    jobs::write(&job)?;
    let _ = jobs::write_owner(&resolved.target, &job);

    let outcome = runner::run(&opts.cargo_args, &log, class)?;

    job.finish(outcome.exit);
    jobs::write(&job)?;
    jobs::clear_owner(&resolved.target);
    jobs::record_duration(class.as_str(), &cmd, outcome.secs);

    eprintln!(
        "gbb: {class} {cmd} finished in {}s (exit {}, digest {}ms)",
        outcome.secs, outcome.exit, digest_ms
    );

    Ok(outcome.exit as u8)
}

fn busy(
    class: Class,
    cmd: &str,
    target: &Path,
    key: Option<&str>,
    reason: &str,
    detail: &str,
) -> u8 {
    let (used, total) = slots::occupancy(class);
    let eta = slots::eta_text(class, cmd);
    let owner = jobs::read_owner(target);

    eprintln!("gbb: BUSY — {detail}");
    eprintln!(
        "gbb:   {class} slot {used}/{total} in use, build dir {}",
        target.display()
    );
    if let Some((owner_key, pid, started, args)) = &owner {
        let age = jobs::now_epoch().saturating_sub(*started);
        eprintln!(
            "gbb:   holder pid {pid} `cargo {args}` running {age}s (key {})",
            short(owner_key)
        );
    }

    // Point the agent at a job it can actually wait on.
    let wait_key = key
        .map(|k| k.to_string())
        .or_else(|| owner.map(|(k, _, _, _)| k));

    match wait_key {
        Some(wait_key) => {
            println!("GBB_BUSY reason={reason} key={wait_key} slot={class}{eta}");
            println!("  → do not retry more than 3x, do not run bare cargo, work on something else,");
            println!("    then: gbb wait {wait_key}");
        }
        None => {
            println!("GBB_BUSY reason={reason} slot={class}{eta}");
            println!("  → do not retry more than 3x, do not run bare cargo, work on something else,");
            println!("    then retry with: gbb --wait 300 <same cargo command>");
        }
    }
    EXIT_BUSY
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Reusing a finished job is only safe when it produced artifacts rather than
/// observations. Test/run/bench must execute again: flaky and side-effectful
/// results are not cacheable, even with an identical input digest.
fn reuse_allowed(cmd: &str, args: &[String]) -> bool {
    if args.iter().any(|a| a == "--no-run" || a == "--no-exec") {
        return true;
    }
    !matches!(cmd, "test" | "run" | "bench" | "nextest" | "doctest")
}

fn wait_for_job(key: &str, timeout: Duration) -> io::Result<u8> {
    let deadline = Instant::now() + timeout;
    eprintln!("gbb: waiting for job {} (timeout {}s)", short(key), timeout.as_secs());
    loop {
        match jobs::read(key) {
            Some(job) if job.state == jobs::State::Done => {
                replay(key);
                return Ok(job.exit as u8);
            }
            Some(job) if !job.is_live() => {
                eprintln!("gbb: job {} died without recording a result", short(key));
                return Ok(EXIT_FAIL);
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            eprintln!("gbb: still running after {}s; giving up without starting a duplicate", timeout.as_secs());
            return Ok(EXIT_BUSY);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Show the job's captured output to an agent that did not run it.
fn replay(key: &str) {
    let log = jobs::log_path(key);
    let lines = runner::tail(&log, 200);
    if lines.is_empty() {
        return;
    }
    eprintln!("gbb: ---- output of job {} (last {} lines) ----", short(key), lines.len());
    for line in lines {
        eprintln!("{line}");
    }
}

fn short(key: &str) -> &str {
    &key[..key.len().min(12)]
}

fn profile_of(args: &[String]) -> String {
    let mut profile = "dev".to_string();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "--release" || arg == "-r" {
            profile = "release".into();
        } else if arg == "--profile" {
            if let Some(next) = iter.peek() {
                profile = (*next).clone();
            }
        } else if let Some(rest) = arg.strip_prefix("--profile=") {
            profile = rest.to_string();
        }
    }
    profile
}

fn profile_lock_path(target: &Path, profile: &str) -> PathBuf {
    target.join(format!(".gbb-{profile}.lock"))
}

fn acquire_build_lock(path: &Path, timeout: Duration) -> io::Result<Option<File>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(path)?;

    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// Best-effort detection of a bare `cargo` holding its own build lock.
fn foreign_cargo_lock(target: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(target).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let lock = path.join(".cargo-build-lock");
        if !lock.is_file() {
            continue;
        }
        if let Ok(file) = File::open(&lock) {
            if file.try_lock().is_err() {
                return Some(lock);
            }
            let _ = file.unlock();
        }
    }
    None
}

fn status() -> io::Result<u8> {
    println!("gbb {}", env!("CARGO_PKG_VERSION"));
    for class in [Class::Light, Class::Heavy] {
        let (used, total) = slots::occupancy(class);
        println!("{:<5} slots {used}/{total}", class.as_str());
    }
    println!();
    let running = jobs::running_jobs();
    if running.is_empty() {
        println!("no gbb jobs running");
    } else {
        println!("{:<14} {:<6} {:>7}  args", "key", "class", "age");
        for job in running {
            println!(
                "{:<14} {:<6} {:>6}s  cargo {}",
                short(&job.key),
                job.class,
                job.age_secs(),
                job.args.join(" ")
            );
        }
    }
    println!();
    println!("free disk: {} GB", gc::free_gb());
    Ok(0)
}

fn doctor() -> io::Result<u8> {
    let cwd = env::current_dir()?;
    let resolved = target::resolve(&cwd)?;
    let started = Instant::now();
    let digest = digest::workspace_digest(&resolved.root)?;
    println!("workspace root : {}", resolved.root.display());
    println!("build dir      : {}", resolved.target.display());
    println!("input digest   : {} ({}ms)", short(&digest), started.elapsed().as_millis());
    println!("runtime dir    : {}", jobs::runtime_dir().display());
    println!("cache dir      : {}", jobs::cache_dir().display());
    println!(
        "slots          : light {}/{} · heavy {}/{}",
        slots::occupancy(Class::Light).0,
        slots::occupancy(Class::Light).1,
        slots::occupancy(Class::Heavy).0,
        slots::occupancy(Class::Heavy).1
    );
    for bin in ["cargo", "systemd-run", "sccache", "mold", "ld.mold"] {
        println!("{:<14} : {}", bin, runner::which(bin).map(|p| p.display().to_string()).unwrap_or_else(|| "missing".into()));
    }
    println!("free disk      : {} GB", gc::free_gb());
    Ok(0)
}

fn wait_command(argv: &[String]) -> io::Result<u8> {
    let Some(prefix) = argv.first() else {
        eprintln!("gbb: usage: gbb wait <key|prefix>");
        return Ok(EXIT_USAGE);
    };
    let candidates: Vec<jobs::Job> = jobs::running_jobs()
        .into_iter()
        .filter(|j| j.key.starts_with(prefix))
        .collect();
    match candidates.len() {
        0 => {
            eprintln!("gbb: no running job matches {prefix}");
            Ok(EXIT_FAIL)
        }
        1 => wait_for_job(&candidates[0].key, Duration::from_secs(900)),
        _ => {
            eprintln!("gbb: prefix {prefix} is ambiguous:");
            for job in candidates {
                eprintln!("  {}  cargo {}", job.key, job.args.join(" "));
            }
            Ok(EXIT_USAGE)
        }
    }
}

fn gc_command(argv: &[String]) -> io::Result<u8> {
    let mut apply = false;
    let mut days = 7u64;
    let mut include_current = false;
    let mut index = 0;
    while index < argv.len() {
        match argv[index].as_str() {
            "--apply" => apply = true,
            "--include-current" => include_current = true,
            "--days" => {
                index += 1;
                days = argv
                    .get(index)
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "--days needs a number"))?;
            }
            other => {
                eprintln!("gbb: unknown gc option {other}");
                return Ok(EXIT_USAGE);
            }
        }
        index += 1;
    }

    let cwd = env::current_dir()?;
    let resolved = target::resolve(&cwd)?;
    let candidates = gc::collect(&resolved.root);
    let free = gc::free_gb();

    println!("free disk: {free} GB");
    println!(
        "{:<70} {:>9} {:>6} {:>6} {:>7}  eligible",
        "worktree", "size", "idle", "dirty", "target"
    );
    let mut reclaimable = 0u64;
    for candidate in &candidates {
        let eligible = candidate.eligible(days, include_current);
        if eligible {
            reclaimable += candidate.size_mb;
        }
        println!(
            "{:<70} {:>7} MB {:>4}d {:>6} {:>7}  {}",
            trim(&candidate.path.to_string_lossy(), 70),
            candidate.size_mb,
            candidate.idle_days,
            candidate.dirty,
            candidate.has_target,
            if eligible { "yes" } else { "no" }
        );
    }

    if !apply {
        println!("\nreclaimable now: {} GB (dry run — add --apply to delete)", reclaimable / 1024);
        if free < 250 {
            println!("warning: free disk below the 250 GB floor");
        }
        return Ok(0);
    }

    let (removed, freed) = gc::apply(&candidates, days, include_current);
    println!("\nremoved {removed} target dirs, freed {} GB", freed / 1024);
    Ok(0)
}

fn trim(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}
