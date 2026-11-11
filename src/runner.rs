//! Spawn cargo inside an isolated scope, mirror its output, and watch it.
//!
//! Output is captured so a second agent that reuses this job's result can read
//! why it failed, even though it never owned the build. While the job runs, a
//! sampler reports duration against the learned baseline and the resources the
//! job is actually consuming, so an agent can notice an abnormal run instead of
//! waiting forever.

use crate::classify::Class;
use crate::jobs::{self, Baseline};
use crate::settings::Settings;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub struct Outcome {
    pub exit: i32,
    pub secs: u64,
}

pub struct Running {
    child: Child,
    key: String,
    started: Instant,
    baseline: Baseline,
    settings: Settings,
    pump: Vec<JoinHandle<()>>,
}

/// Start cargo and hand back a handle. Nothing is watched or reported yet; that
/// happens in `wait`, so the caller can record the real child pid first.
pub fn start(
    args: &[String],
    log: &Path,
    class: Class,
    target_override: Option<&Path>,
    settings: &Settings,
    key: &str,
    baseline: Baseline,
) -> io::Result<Running> {
    let log_file = Arc::new(Mutex::new(open_log(log)?));
    let mut command = build_command(args, class, target_override, settings, key);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_console = Arc::new(Mutex::new(io::stdout()));
    let err_console = Arc::new(Mutex::new(io::stderr()));

    let log_a = Arc::clone(&log_file);
    let t1 = thread::spawn(move || tee(stdout, log_a, out_console));
    let log_b = Arc::clone(&log_file);
    let t2 = thread::spawn(move || tee(stderr, log_b, err_console));

    Ok(Running {
        child,
        key: key.to_string(),
        started: Instant::now(),
        baseline,
        settings: settings.clone(),
        pump: vec![t1, t2],
    })
}

impl Running {
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn wait(mut self) -> io::Result<Outcome> {
        // A channel lets the sampler wake the instant cargo exits, instead of
        // holding every short build open for one sample interval.
        let (stop_tx, stop_rx) = mpsc::channel();
        let watcher = {
            let key = self.key.clone();
            let baseline = self.baseline;
            let settings = self.settings.clone();
            let started = self.started;
            let root = self.child.id();
            thread::spawn(move || {
                watch(root, started, &key, baseline, &settings, stop_rx);
            })
        };

        let status = self.child.wait();
        let _ = stop_tx.send(());
        let _ = watcher.join();
        for handle in self.pump.drain(..) {
            let _ = handle.join();
        }
        let status = status?;

        let exit = match status.code() {
            Some(code) => code,
            None => 128 + status.signal().unwrap_or(0),
        };

        Ok(Outcome {
            exit,
            secs: self.started.elapsed().as_secs(),
        })
    }
}

/// A build that has not settled yet is not stalled, and one early sample is
/// not enough to know its CPU share. Both would produce false alarms.
const GRACE_SECS: u64 = 30;

/// Sample the process tree and report abnormal duration or resource use.
fn watch(
    root: u32,
    started: Instant,
    key: &str,
    baseline: Baseline,
    settings: &Settings,
    stop: Receiver<()>,
) {
    let sample = Duration::from_secs(settings.sample_secs);
    let notice_every = Duration::from_secs(settings.notice_secs);
    let mut previous_cpu: Option<(u64, Instant)> = None;
    let mut last_notice = Instant::now();

    while let Err(RecvTimeoutError::Timeout) = stop.recv_timeout(sample) {
        let stats = tree_stats(root);
        if stats.members == 0 {
            continue;
        }

        let now = Instant::now();
        let Some((ticks, at)) = previous_cpu.replace((stats.cpu_ticks, now)) else {
            // The first sample has no window to divide by, so it is only a
            // baseline for the next one.
            continue;
        };
        let cpu_pct = cpu_percent(ticks, stats.cpu_ticks, now - at);
        let elapsed = started.elapsed().as_secs();
        let rss_mb = stats.rss_kb / 1024;
        let swap_gb = jobs::swap_used_gb();

        let verdict = diagnose(
            &Sample {
                elapsed,
                cpu_pct,
                rss_mb,
                swap_gb,
                rustc: stats.rustc,
                p90: baseline.p90,
                samples: baseline.samples,
            },
            settings,
        );
        jobs::write_health(
            key,
            &jobs::Health {
                state: verdict.state.to_string(),
                elapsed,
                cpu_pct,
                rss_mb,
                swap_gb,
                rustc: stats.rustc,
                p90: baseline.p90,
                updated: jobs::now_epoch(),
            },
        );

        if verdict.state != "running" && last_notice.elapsed() >= notice_every {
            last_notice = Instant::now();
            eprintln!("{}", verdict.render(key));
        }
    }
}

/// Pure health decision, so the thresholds can be tested without processes.
pub struct Sample {
    pub elapsed: u64,
    pub cpu_pct: u64,
    pub rss_mb: u64,
    pub swap_gb: u64,
    pub rustc: u32,
    pub p90: u64,
    pub samples: usize,
}

pub struct Verdict {
    pub state: &'static str,
    pub detail: String,
}

impl Verdict {
    fn render(&self, key: &str) -> String {
        format!(
            "gbb: {} job {} — {}",
            self.state.to_uppercase(),
            &key[..key.len().min(12)],
            self.detail
        )
    }
}

pub fn diagnose(input: &Sample, settings: &Settings) -> Verdict {
    let measured = input.samples > 0 && input.p90 > 0;
    let ratio = measured.then(|| input.elapsed as f64 / input.p90.max(1) as f64);
    let past_expectation = !measured || input.elapsed >= input.p90;
    let settled = input.elapsed >= GRACE_SECS;

    if settled && past_expectation && input.rustc == 0 && input.cpu_pct < settings.stall_cpu_pct {
        return Verdict {
            state: "stalled",
            detail: format!(
                "{}s with {}% cpu and no compiler child — sleeping, waiting on a lock/network, or blocked; `gbb kill` stops it",
                input.elapsed, input.cpu_pct
            ),
        };
    }

    if settled && past_expectation && input.swap_gb >= settings.swap_warn_gb && input.cpu_pct < 60 {
        return Verdict {
            state: "thrashing",
            detail: format!(
                "{}s, {}% cpu, {} MB rss, system swap {} GB — the machine is paging; retrying `--shared` or lowering CARGO_BUILD_JOBS will be faster",
                input.elapsed, input.cpu_pct, input.rss_mb, input.swap_gb
            ),
        };
    }

    if settled && ratio.is_some_and(|r| r * 100.0 >= settings.slow_factor_pct as f64) {
        return Verdict {
            state: "slow",
            detail: format!(
                "{:.1}x usual (p90 {}s, running {}s), {}% cpu, {} MB rss, {} compiler children",
                ratio.unwrap_or(0.0),
                input.p90,
                input.elapsed,
                input.cpu_pct,
                input.rss_mb,
                input.rustc
            ),
        };
    }

    Verdict {
        state: "running",
        detail: format!(
            "{}s, {}% cpu, {} MB rss{}",
            input.elapsed,
            input.cpu_pct,
            input.rss_mb,
            if measured {
                format!(", expected {}", human(input.p90))
            } else {
                ", no baseline yet".to_string()
            }
        ),
    }
}

pub fn human(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

struct TreeStats {
    rss_kb: u64,
    cpu_ticks: u64,
    rustc: u32,
    members: usize,
}

/// One `/proc/<pid>/stat` read per process: parent, CPU ticks and RSS all live
/// in the same file, so a tree sample never re-reads a process.
struct Proc {
    pid: u32,
    ppid: u32,
    cpu_ticks: u64,
    rss_pages: u64,
    is_rustc: bool,
}

/// Sum resident memory and CPU time over a process tree, counting compilers.
fn tree_stats(root: u32) -> TreeStats {
    let Some(procs) = read_proc() else {
        return empty_stats();
    };

    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for proc in &procs {
        children.entry(proc.ppid).or_default().push(proc.pid);
    }

    let mut members = HashSet::from([root]);
    let mut queue = VecDeque::from([root]);
    while let Some(pid) = queue.pop_front() {
        for child in children.get(&pid).into_iter().flatten() {
            if members.insert(*child) {
                queue.push_back(*child);
            }
        }
    }

    let mut stats = TreeStats {
        members: members.len(),
        ..empty_stats()
    };
    for proc in &procs {
        if !members.contains(&proc.pid) {
            continue;
        }
        stats.rss_kb += proc.rss_pages * 4;
        stats.cpu_ticks += proc.cpu_ticks;
        if proc.is_rustc {
            stats.rustc += 1;
        }
    }
    stats
}

fn empty_stats() -> TreeStats {
    TreeStats {
        rss_kb: 0,
        cpu_ticks: 0,
        rustc: 0,
        members: 0,
    }
}

/// Parse every live process from `/proc`. The comm field may contain spaces and
/// parentheses, so fields after it are read from the last `)`.
fn read_proc() -> Option<Vec<Proc>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_string_lossy().parse::<u32>().ok() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(proc) = parse_proc(pid, &stat) else {
            continue;
        };
        out.push(proc);
    }
    Some(out)
}

fn parse_proc(pid: u32, stat: &str) -> Option<Proc> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = &stat[open + 1..close];
    let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
    if rest.len() < 22 {
        return None;
    }
    let ppid = rest[1].parse().ok()?;
    let utime: u64 = rest[11].parse().ok()?;
    let stime: u64 = rest[12].parse().ok()?;
    let rss_pages: u64 = rest[21].parse().ok()?;
    Some(Proc {
        pid,
        ppid,
        cpu_ticks: utime + stime,
        rss_pages,
        is_rustc: comm == "rustc",
    })
}

fn cpu_percent(previous: u64, current: u64, window: Duration) -> u64 {
    let secs = window.as_secs_f64();
    if secs <= 0.0 || current < previous {
        return 0;
    }
    // /proc counts 100 ticks per second per core, so ticks/seconds is already
    // percent-of-one-core (400% means four cores busy).
    ((current - previous) as f64 / secs).round() as u64
}

fn open_log(log: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(log)
}

fn tee<R, W>(source: Option<R>, log: Arc<Mutex<File>>, console: Arc<Mutex<W>>)
where
    R: io::Read,
    W: Write + Send + 'static,
{
    let Some(source) = source else {
        return;
    };
    let mut reader = BufReader::new(source);
    let mut buf = Vec::with_capacity(8 * 1024);

    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {
                if let Ok(mut f) = log.lock() {
                    let _ = f.write_all(&buf);
                }
                if let Ok(mut c) = console.lock() {
                    let _ = c.write_all(&buf);
                    let _ = c.flush();
                }
            }
            Err(_) => break,
        }
    }
}

/// Cargo, optionally under systemd so a runaway build can never take the
/// desktop's memory hostage. The unit is named after the job, which is what
/// makes `gbb kill` and per-job resource reads exact.
fn build_command(
    args: &[String],
    class: Class,
    target_override: Option<&std::path::Path>,
    settings: &Settings,
    key: &str,
) -> Command {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let systemd = !settings.no_systemd && which("systemd-run").is_some();
    let slice = settings.slice.clone();

    let mut envs: Vec<(String, String)> = Vec::new();
    if let Some(jobs) = &settings.jobs {
        envs.push(("CARGO_BUILD_JOBS".into(), jobs.clone()));
    }
    if let Some(dir) = target_override {
        envs.push((
            "CARGO_TARGET_DIR".into(),
            dir.to_string_lossy().into_owned(),
        ));
    }
    envs.extend(sccache_env(settings));

    let mut cmd = if systemd {
        let mut c = Command::new("systemd-run");
        c.arg("--user")
            .arg("--scope")
            .arg("--collect")
            .arg("--quiet")
            .arg(format!("--unit=gbb-{}.scope", &key[..key.len().min(12)]))
            .arg(format!("--slice={slice}"))
            .arg("--")
            .arg("env");
        for (k, v) in &envs {
            c.arg(format!("{k}={v}"));
        }
        c.arg(&cargo);
        c
    } else {
        let mut c = Command::new(&cargo);
        for (k, v) in &envs {
            c.env(k, v);
        }
        c
    };

    for arg in args {
        cmd.arg(arg);
    }

    if class == Class::Heavy {
        // Heavy jobs are the only ones worth colourising; keeps logs readable.
        cmd.env(
            "CARGO_TERM_COLOR",
            std::env::var("CARGO_TERM_COLOR").unwrap_or_else(|_| "always".into()),
        );
    }

    cmd
}

/// sccache is a force multiplier across worktrees, but it cannot cache bins,
/// dylibs, or proc-macros and refuses incremental builds, so it is opt-in by
/// presence and opt-out by env.
fn sccache_env(settings: &Settings) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if settings.no_sccache || which("sccache").is_none() {
        return out;
    }
    out.push(("RUSTC_WRAPPER".into(), "sccache".into()));
    if std::env::var("SCCACHE_DIR").is_err() {
        out.push((
            "SCCACHE_DIR".into(),
            jobs::cache_dir()
                .join("sccache")
                .to_string_lossy()
                .into_owned(),
        ));
    }
    if std::env::var("SCCACHE_CACHE_SIZE").is_err() {
        out.push(("SCCACHE_CACHE_SIZE".into(), "60G".into()));
    }
    out.push(("CARGO_INCREMENTAL".into(), "0".into()));
    out
}

pub fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}

/// Last `n` lines of a job log, for reuse reporting. Reads backwards from the
/// end so a multi-megabyte log costs only the bytes actually replayed.
pub fn tail(log: &Path, n: usize) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let mut file = match File::open(log) {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    let mut remaining = match file.metadata() {
        Ok(meta) => meta.len(),
        Err(_) => return Vec::new(),
    };

    const CHUNK: u64 = 8 * 1024;
    let mut bytes = Vec::new();
    let mut newlines = 0usize;
    while remaining > 0 && newlines <= n {
        let read = CHUNK.min(remaining);
        remaining -= read;
        if file.seek(SeekFrom::Start(remaining)).is_err() {
            return Vec::new();
        }
        let mut chunk = vec![0u8; read as usize];
        if file.read_exact(&mut chunk).is_err() {
            return Vec::new();
        }
        newlines += chunk.iter().filter(|byte| **byte == b'\n').count();
        chunk.extend_from_slice(&bytes);
        bytes = chunk;
    }

    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(elapsed: u64, cpu: u64, rustc: u32, p90: u64) -> Sample {
        Sample {
            elapsed,
            cpu_pct: cpu,
            rss_mb: 3000,
            swap_gb: 0,
            rustc,
            p90,
            samples: if p90 > 0 { 5 } else { 0 },
        }
    }

    #[test]
    fn normal_run_is_quiet() {
        let settings = Settings::default();
        assert_eq!(
            diagnose(&sample(30, 400, 2, 120), &settings).state,
            "running"
        );
    }

    #[test]
    fn twice_the_p90_is_slow() {
        let settings = Settings::default();
        assert_eq!(diagnose(&sample(300, 400, 2, 120), &settings).state, "slow");
    }

    #[test]
    fn no_cpu_and_no_compiler_is_stalled() {
        let settings = Settings::default();
        let verdict = diagnose(&sample(400, 0, 0, 120), &settings);
        assert_eq!(verdict.state, "stalled");
        assert!(verdict.detail.contains("blocked"));
    }

    #[test]
    fn swap_with_low_cpu_is_thrashing() {
        let settings = Settings::default();
        let mut input = sample(400, 20, 2, 120);
        input.swap_gb = 8;
        assert_eq!(diagnose(&input, &settings).state, "thrashing");
    }

    #[test]
    fn no_baseline_never_reports_slow() {
        let settings = Settings::default();
        let verdict = diagnose(&sample(3600, 400, 2, 0), &settings);
        assert_eq!(verdict.state, "running");
        assert!(verdict.detail.contains("no baseline"));
    }

    #[test]
    fn young_jobs_are_not_diagnosed() {
        let settings = Settings::default();
        assert_eq!(diagnose(&sample(10, 0, 0, 120), &settings).state, "running");
    }

    #[test]
    fn tail_returns_last_lines() {
        let path = std::env::temp_dir().join(format!("gbb-tail-{}.log", std::process::id()));
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        assert_eq!(tail(&path, 2), vec!["c".to_string(), "d".to_string()]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tail_reads_a_small_file_whole_and_a_large_one_from_the_end() {
        let path = std::env::temp_dir().join(format!("gbb-tail-big-{}.log", std::process::id()));
        let mut body = String::new();
        for i in 0..10_000 {
            body.push_str(&format!("line {i}\n"));
        }
        std::fs::write(&path, body).unwrap();
        let lines = tail(&path, 3);
        assert_eq!(lines, vec!["line 9997", "line 9998", "line 9999"]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn cpu_percent_is_share_of_one_core() {
        // Four cores busy for five seconds = 2000 ticks.
        assert_eq!(cpu_percent(0, 2000, Duration::from_secs(5)), 400);
        assert_eq!(cpu_percent(100, 100, Duration::from_secs(5)), 0);
    }

    #[test]
    fn proc_stat_fields_survive_spaces_and_parens_in_comm() {
        let mut rest = vec!["0"; 22];
        rest[0] = "S";
        rest[1] = "7"; // ppid
        rest[11] = "40"; // utime
        rest[12] = "2"; // stime
        rest[21] = "500"; // rss pages
        let stat = format!("42 (a b) c) {}", rest.join(" "));
        let proc = parse_proc(42, &stat).expect("parse");
        assert_eq!(proc.pid, 42);
        assert_eq!(proc.ppid, 7);
        assert_eq!(proc.cpu_ticks, 42);
        assert_eq!(proc.rss_pages, 500);
        assert!(!proc.is_rustc);
    }
}
