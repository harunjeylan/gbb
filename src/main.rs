//! gbb — cargo build broker for parallel agents.
//!
//! Guarantees:
//!   * one build per build directory (cargo's own lock is per profile dir),
//!   * global memory-aware slots shared across every checkout,
//!   * identical concurrent requests collapse into one build,
//!   * a blocked agent exits 75 immediately instead of hanging on a lock.

mod cache;
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
  gbb status | doctor | cache | gc | wait <key> | help | version

OPTIONS (before the cargo subcommand)
  --wait <secs>    block up to this long for a slot/lock (default 0 = fail fast)
  --no-dedupe      never join or reuse an identical job
  --slot <class>   force slot class: light | heavy
  --priority       same as --slot heavy (merge gates)
  --isolated       use a private build dir (default: on)
  --shared         use the shared build dir and wait for other jobs instead
                   of taking a private, cache-seeded one

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
  GBB_AGENT           stable id for --isolated build dirs (default: cwd hash)
  GBB_NO_CACHE        disable seeding/snapshotting the shared build cache
  GBB_CACHE_GB        cache byte budget (default 80)
  GBB_SNAPSHOT_MAX_GB largest build dir worth snapshotting (default 30)
  GBB_SNAPSHOT_MIN_SECS minimum age before an identical digest is re-snapshotted (default 300)
";

struct Opts {
    wait: Duration,
    dedupe: bool,
    isolated: bool,
    class: Option<Class>,
    cargo_args: Vec<String>,
}

fn main() -> ExitCode {
    install_broken_pipe_guard();
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
        "cache" => report(cache_command(&argv[1..])),
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

/// `gbb status | head` must not print a panic backtrace. Rust ignores SIGPIPE,
/// so a closed stdout turns every `println!` into a panic; exit quietly instead.
fn install_broken_pipe_guard() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let message = payload
            .downcast_ref::<String>()
            .map(|s| s.as_str())
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        if message.contains("Broken pipe") {
            // Unix convention for "killed by SIGPIPE". Exiting 0 would report a
            // truncated pipeline as success, which is worse than the panic.
            std::process::exit(141);
        }
        default_hook(info);
    }));
}

/// Cargo exits 0-255, or dies by signal (reported by the runner as 128+sig).
/// Anything outside that range must not silently alias success or busy.
fn exit_code(code: i32) -> u8 {
    match code {
        0..=255 => code as u8,
        _ => EXIT_FAIL,
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
        isolated: true,
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
            "--isolated" => opts.isolated = true,
            "--shared" => opts.isolated = false,
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
    let cmd = classify::subcommand(&opts.cargo_args)
        .unwrap_or("")
        .to_string();

    let join_wait = Duration::from_secs(env_u64("GBB_JOIN_WAIT", 900));
    let lock_wait = if opts.wait > Duration::ZERO {
        opts.wait
    } else {
        Duration::from_secs(env_u64("GBB_LOCK_WAIT", 240))
    };

    // Isolation is the default because it is what removes waiting, but a
    // private build dir costs disk. Never start one that the disk cannot hold.
    let reserve_gb = env_u64("GBB_RESERVE_GB", 25);
    let mut isolated = opts.isolated;
    if isolated {
        let free = gc::free_gb();
        if free < reserve_gb + 12 {
            eprintln!(
                "gbb: only {free} GB free (reserve {reserve_gb} GB) — falling back to the shared build dir"
            );
            isolated = false;
        }
    }

    // 1. An identical command already owns this build directory: join it.
    //    Compare argv, not the input digest: the first build may create
    //    Cargo.lock, which would change the digest for every later agent.
    //    Private build dirs are not joined; they exist to run alongside.
    if opts.dedupe && !isolated {
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

    // 2. Pick the build directory: private by default, shared with --shared.
    let mut build_dir = resolved.target.clone();
    let mut _isolated_claim: Option<File> = None;
    if isolated {
        match claim_isolated(&resolved.target, &isolated_id(&cwd))? {
            Some((dir, claim)) => {
                eprintln!("gbb: isolated build dir {}", dir.display());
                build_dir = dir;
                _isolated_claim = Some(claim);
            }
            None => {
                return Ok(busy(
                    class,
                    &cmd,
                    &resolved.target,
                    None,
                    "isolated",
                    "every isolated build dir for this agent id is busy",
                ))
            }
        }
    }

    // 3. One build per build directory, at cargo's own profile granularity.
    let profile = profile_of(&opts.cargo_args);
    let lock_path = profile_lock_path(&build_dir, &profile);
    let _lock = match acquire_build_lock(&lock_path, lock_wait, &build_dir)? {
        Some(file) => file,
        None => {
            return Ok(busy(
                class,
                &cmd,
                &build_dir,
                None,
                "build-dir",
                "another gbb job owns this build directory and did not finish in time",
            ))
        }
    };

    // 4. Memory is admitted only once this run is about to consume it. Queueing
    //    for the build dir must not occupy a slot the whole time.
    let Some(_slot) = slots::acquire(class, opts.wait)? else {
        return Ok(busy(
            class,
            &cmd,
            &build_dir,
            None,
            "slot",
            "no free slot; nothing was started",
        ));
    };

    if let Some(foreign) = foreign_cargo_lock(&build_dir) {
        eprintln!(
            "gbb: warning: bare cargo appears to hold {}",
            foreign.display()
        );
        eprintln!("gbb:          it will not be serialized against; prefer `gbb <cmd>` everywhere");
    }

    // 5. A cold build dir is seeded from the newest shared snapshot, so cargo
    //    recompiles only what changed instead of everything.
    let workspace = cache::workspace_key(&resolved.root);
    if cache::is_cold(&build_dir, &profile) {
        match cache::seed(&build_dir, &workspace, &profile) {
            Ok(Some(seed_digest)) => eprintln!(
                "gbb: cold build dir seeded from cache (digest {})",
                short(&seed_digest)
            ),
            Ok(None) => {}
            Err(e) => eprintln!("gbb: cache seed skipped: {e}"),
        }
    }

    // 6. Digest is only meaningful once the tree is quiet, i.e. after the lock.
    let started = Instant::now();
    let digest = digest::workspace_digest(&resolved.root)?;
    let digest_ms = started.elapsed().as_millis();
    let key = digest::job_key(&build_dir, class.as_str(), &opts.cargo_args, &digest);

    // 7. Someone with the same key may have finished while we queued.
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
                return Ok(exit_code(job.exit));
            }
        }
    }

    // 8. Build.
    jobs::prune_logs(24 * 60 * 60);
    let log = jobs::log_path(&key);
    let mut job = jobs::Job::started_running(
        &key,
        std::process::id(),
        class.as_str(),
        &build_dir,
        &opts.cargo_args,
    );
    jobs::write(&job)?;
    let _ = jobs::write_owner(&build_dir, &job);

    let target_override = isolated.then_some(build_dir.as_path());
    let outcome = runner::run(&opts.cargo_args, &log, class, target_override)?;

    job.finish(outcome.exit);
    jobs::write(&job)?;
    jobs::clear_owner(&build_dir);
    jobs::record_duration(class.as_str(), &cmd, outcome.secs);

    // 9. Refresh the shared cache so the next cold checkout starts warm.
    //    Copies take minutes on a loaded disk, so this is a detached child.
    if outcome.exit == 0 && reuse_allowed(&cmd, &opts.cargo_args) {
        cache::spawn_store(&build_dir, &workspace, &profile, &digest);
    }

    eprintln!(
        "gbb: {class} {cmd} finished in {}s (exit {}, digest {}ms)",
        outcome.secs, outcome.exit, digest_ms
    );

    Ok(exit_code(outcome.exit))
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
            println!(
                "  → do not retry more than 3x, do not run bare cargo, work on something else,"
            );
            println!("    then: gbb wait {wait_key}");
        }
        None => {
            println!("GBB_BUSY reason={reason} slot={class}{eta}");
            println!(
                "  → do not retry more than 3x, do not run bare cargo, work on something else,"
            );
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

/// An env switch counts only when set to something non-empty, so
/// `export GBB_NO_CACHE=` in a shell profile cannot silently change behaviour.
fn env_flag(name: &str) -> bool {
    env::var_os(name)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
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
    let started = Instant::now();
    let deadline = started + timeout;
    let mut announced = 0u64;
    eprintln!(
        "gbb: waiting for job {} (timeout {}s, Ctrl-C keeps the owner building)",
        short(key),
        timeout.as_secs()
    );
    loop {
        let elapsed = started.elapsed().as_secs();
        if elapsed >= 10 && elapsed / 10 > announced {
            announced = elapsed / 10;
            eprintln!("gbb: still waiting for job {} ({elapsed}s)", short(key));
        }
        match jobs::read(key) {
            Some(job) if job.state == jobs::State::Done => {
                replay(key);
                return Ok(exit_code(job.exit));
            }
            Some(job) if !job.is_live() => {
                eprintln!("gbb: job {} died without recording a result", short(key));
                return Ok(EXIT_FAIL);
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            eprintln!(
                "gbb: still running after {}s; giving up without starting a duplicate",
                timeout.as_secs()
            );
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
    eprintln!(
        "gbb: ---- output of job {} (last {} lines) ----",
        short(key),
        lines.len()
    );
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

fn acquire_build_lock(
    path: &Path,
    timeout: Duration,
    build_dir: &Path,
) -> io::Result<Option<File>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(path)?;

    let started = Instant::now();
    let deadline = started + timeout;
    let mut announced = false;
    loop {
        match file.try_lock() {
            Ok(()) => {
                if announced {
                    eprintln!("gbb: build dir free after {}s", started.elapsed().as_secs());
                }
                return Ok(Some(file));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                if !announced && started.elapsed() >= Duration::from_secs(2) {
                    // Silence during a long wait looks like a hang; name the holder.
                    match jobs::read_owner(build_dir) {
                        Some((owner_key, pid, owner_started, args)) => {
                            let age = jobs::now_epoch().saturating_sub(owner_started);
                            eprintln!(
                                "gbb: waiting for build dir: `cargo {args}` (pid {pid}, {age}s old, key {})",
                                short(&owner_key)
                            );
                        }
                        None => eprintln!("gbb: waiting for build-dir lock {}", path.display()),
                    }
                    eprintln!("gbb:   gives up after {}s", timeout.as_secs());
                    announced = true;
                }
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// Stable id for a private build dir: GBB_NAME (or legacy GBB_AGENT) lets
/// several processes deliberately share one warm dir; otherwise the checkout
/// identifies the dir and a concurrent second run takes the next free suffix.
fn isolated_id(cwd: &Path) -> String {
    for var in ["GBB_NAME", "GBB_AGENT"] {
        if let Ok(name) = env::var(var) {
            let name = name.trim();
            if !name.is_empty() {
                return sanitize(name);
            }
        }
    }
    format!("w{:08x}", hash_str(&cwd.to_string_lossy()))
}

fn sanitize(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "agent".to_string()
    } else {
        trimmed.chars().take(40).collect()
    }
}

fn hash_str(s: &str) -> u32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    (hasher.finish() & 0xffff_ffff) as u32
}

/// Take a private build dir under `<target>/parallel/`. A claim lock keeps two
/// concurrent runs apart while repeated runs in the same checkout stay warm.
fn claim_isolated(target_root: &Path, base: &str) -> io::Result<Option<(PathBuf, File)>> {
    let root = target_root.join("parallel");
    std::fs::create_dir_all(&root)?;

    for suffix in 0..8 {
        let name = if suffix == 0 {
            base.to_string()
        } else {
            format!("{base}-{}", suffix + 1)
        };
        let dir = root.join(&name);
        std::fs::create_dir_all(&dir)?;
        let claim = dir.join(".gbb-claim.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&claim)?;
        match file.try_lock() {
            Ok(()) => return Ok(Some((dir, file))),
            Err(std::fs::TryLockError::WouldBlock) => continue,
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
    Ok(None)
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
    println!(
        "input digest   : {} ({}ms)",
        short(&digest),
        started.elapsed().as_millis()
    );
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
        println!(
            "{:<14} : {}",
            bin,
            runner::which(bin)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "missing".into())
        );
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

fn cache_command(argv: &[String]) -> io::Result<u8> {
    if argv.iter().any(|arg| arg == "--store") {
        return store_from_environment();
    }

    let apply = argv.iter().any(|a| a == "--apply");
    let budget_gb = env_u64("GBB_CACHE_GB", 80);
    let budget = budget_gb * 1024 * 1024 * 1024;
    let (removable, removable_bytes) = cache::prune(budget, false)?;
    let total = cache::total_bytes();

    println!(
        "cache: {:.1} GB of {budget_gb} GB budget",
        total as f64 / 1073741824.0
    );
    println!();
    println!(
        "{:<12} {:<8} {:<18} {:>9} {:>5}",
        "workspace", "profile", "digest", "size", "age"
    );
    for entry in cache::listing() {
        let age_days = jobs::now_epoch().saturating_sub(entry.created) / 86_400;
        println!(
            "{:<12} {:<8} {:<18} {:>6.1} GB {:>4}d",
            short(&entry.workspace),
            entry.profile,
            short(&entry.digest),
            entry.bytes as f64 / 1073741824.0,
            age_days
        );
    }

    if !apply {
        println!();
        println!(
            "would remove {removable} duplicate/over-budget snapshots ({:.1} GB); add --apply",
            removable_bytes as f64 / 1073741824.0
        );
        return Ok(0);
    }

    let (removed, bytes) = cache::prune(budget, true)?;
    println!();
    println!(
        "removed {removed} snapshots, freed {:.1} GB",
        bytes as f64 / 1073741824.0
    );
    Ok(0)
}

/// Detached snapshot worker: only runs when it can take the build-dir lock, so
/// it never copies a directory that another build is rewriting.
fn store_from_environment() -> io::Result<u8> {
    let Some(build_dir) = env::var_os("GBB_STORE_BUILD_DIR").map(PathBuf::from) else {
        return Ok(0);
    };
    let workspace = env::var("GBB_STORE_WORKSPACE").unwrap_or_default();
    let profile = env::var("GBB_STORE_PROFILE").unwrap_or_else(|_| "dev".into());
    let digest = env::var("GBB_STORE_DIGEST").unwrap_or_default();
    if workspace.is_empty() || digest.is_empty() {
        return Ok(0);
    }

    let lock_path = profile_lock_path(&build_dir, &profile);
    let Some(_lock) = acquire_build_lock(&lock_path, Duration::ZERO, &build_dir)? else {
        eprintln!("gbb: cache snapshot skipped (build dir busy)");
        return Ok(0);
    };

    match cache::store(&build_dir, &workspace, &profile, &digest)? {
        Some(path) => eprintln!("gbb: cached build dir → {}", path.display()),
        None => eprintln!("gbb: cache snapshot not needed"),
    }
    Ok(0)
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
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "--days needs a number")
                    })?;
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
    let mut candidates = gc::collect(&resolved.root);
    candidates.extend(gc::parallel_dirs(&resolved.target));
    let free = gc::free_gb();

    println!("free disk: {free} GB");
    println!(
        "{:<70} {:>9} {:>6} {:>6} {:>7}  eligible",
        "path", "size", "idle", "dirty", "kind"
    );
    let mut reclaimable = 0u64;
    for candidate in &candidates {
        let eligible = candidate.eligible(days, include_current);
        if eligible {
            reclaimable += candidate.size_mb;
        }
        let kind = match candidate.kind {
            gc::Kind::WorktreeTarget => "worktree",
            gc::Kind::ParallelDir => "parallel",
        };
        println!(
            "{:<70} {:>7} MB {:>4}d {:>6} {:>7}  {}",
            trim(&candidate.path.to_string_lossy(), 70),
            candidate.size_mb,
            candidate.idle_days,
            candidate.dirty,
            kind,
            if eligible { "yes" } else { "no" }
        );
    }

    if !apply {
        println!(
            "\nreclaimable now: {} GB (dry run — add --apply to delete)",
            reclaimable / 1024
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_outside_u8_do_not_alias_success_or_busy() {
        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(101), 101);
        assert_eq!(exit_code(255), 255);
        assert_eq!(exit_code(256), EXIT_FAIL);
        assert_eq!(exit_code(331), EXIT_FAIL);
        assert_eq!(exit_code(-1), EXIT_FAIL);
    }

    #[test]
    fn profile_detection_reads_release_and_custom_profiles() {
        let args = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(profile_of(&args(&["test"])), "dev");
        assert_eq!(profile_of(&args(&["build", "--release"])), "release");
        assert_eq!(profile_of(&args(&["build", "-r"])), "release");
        assert_eq!(profile_of(&args(&["build", "--profile", "bench"])), "bench");
        assert_eq!(profile_of(&args(&["build", "--profile=fast"])), "fast");
    }

    #[test]
    fn sanitize_keeps_identifiers_bounded() {
        assert_eq!(sanitize("agent/one two"), "agent-one-two");
        assert_eq!(sanitize("///"), "agent");
        assert_eq!(sanitize(&"x".repeat(80)).len(), 40);
    }
}
