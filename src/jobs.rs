//! Runtime state: job records, logs, duration history.
//!
//! Records are flat `key=value` files written atomically, so a crashed writer
//! never leaves a half-parsed record behind.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Done,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Done => "done",
        }
    }
    fn parse(s: &str) -> State {
        if s == "running" {
            State::Running
        } else {
            State::Done
        }
    }
}

#[derive(Clone, Debug)]
pub struct Job {
    pub key: String,
    pub state: State,
    pub pid: u32,
    pub started: u64,
    pub finished: u64,
    pub exit: i32,
    pub class: String,
    pub target: String,
    pub args: Vec<String>,
    /// Process actually spawned for the build (scope or cargo), so a kill can
    /// reach the work rather than the gbb parent.
    pub child_pid: u32,
}

impl Job {
    pub fn started_running(
        key: &str,
        pid: u32,
        class: &str,
        target: &Path,
        args: &[String],
    ) -> Job {
        Job {
            key: key.to_string(),
            state: State::Running,
            pid,
            started: now_epoch(),
            finished: 0,
            exit: 0,
            class: class.to_string(),
            target: target.to_string_lossy().into_owned(),
            args: args.to_vec(),
            child_pid: 0,
        }
    }

    /// A `running` record whose process is gone is stale, not authoritative.
    pub fn is_live(&self) -> bool {
        self.state == State::Running && pid_alive(self.pid)
    }

    pub fn age_secs(&self) -> u64 {
        now_epoch().saturating_sub(self.started)
    }

    pub fn finish(&mut self, exit: i32) {
        self.state = State::Done;
        self.finished = now_epoch();
        self.exit = exit;
    }
}

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn pid_alive(pid: u32) -> bool {
    pid > 0 && Path::new("/proc").join(pid.to_string()).exists()
}

/// System-wide swap in use. Reported alongside a job because it explains why a
/// build is slow; it is not attributable to the job without per-job accounting.
pub fn swap_used_gb() -> u64 {
    let Ok(text) = fs::read_to_string("/proc/meminfo") else {
        return 0;
    };
    let mut total = 0u64;
    let mut free = 0u64;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("SwapTotal:") {
            total = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("SwapFree:") {
            free = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }
    }
    total.saturating_sub(free) / 1024 / 1024
}

fn base_runtime() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("gbb")
    } else {
        PathBuf::from(format!("/run/user/{}", uid())).join("gbb")
    }
}

pub fn runtime_dir() -> PathBuf {
    let dir = base_runtime();
    if fs::create_dir_all(&dir).is_err() {
        let fallback = env::temp_dir().join(format!("gbb-{}", uid()));
        let _ = fs::create_dir_all(&fallback);
        return fallback;
    }
    dir
}

pub fn cache_dir() -> PathBuf {
    let dir = match env::var_os("XDG_CACHE_HOME") {
        Some(d) => PathBuf::from(d).join("gbb"),
        None => home().join(".cache").join("gbb"),
    };
    let _ = fs::create_dir_all(dir.join("jobs"));
    let _ = fs::create_dir_all(dir.join("logs"));
    dir
}

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn uid() -> u32 {
    // /proc/self/status is always available on Linux; avoids a libc dependency.
    static CACHE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("Uid:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse().ok())
            })
            .unwrap_or(0)
    })
}

pub fn slot_dir() -> PathBuf {
    let dir = runtime_dir().join("slots");
    let _ = fs::create_dir_all(&dir);
    dir
}

pub fn job_path(key: &str) -> PathBuf {
    cache_dir().join("jobs").join(format!("{key}.job"))
}

pub fn log_path(key: &str) -> PathBuf {
    cache_dir().join("logs").join(format!("{key}.log"))
}

pub fn write(job: &Job) -> io::Result<()> {
    let path = job_path(&job.key);
    let tmp = path.with_extension("tmp");
    let body = format!(
        "key={}\nstate={}\npid={}\nchild_pid={}\nstarted={}\nfinished={}\nexit={}\nclass={}\ntarget={}\nargs={}\n",
        job.key,
        job.state.as_str(),
        job.pid,
        job.child_pid,
        job.started,
        job.finished,
        job.exit,
        job.class,
        job.target,
        job.args.join("\u{1f}")
    );
    fs::write(&tmp, body)?;
    fs::rename(&tmp, &path)
}

pub fn read(key: &str) -> Option<Job> {
    let text = fs::read_to_string(job_path(key)).ok()?;
    parse_record(&text)
}

fn parse_record(text: &str) -> Option<Job> {
    let mut key = String::new();
    let mut state = State::Done;
    let mut pid = 0u32;
    let mut started = 0u64;
    let mut finished = 0u64;
    let mut exit = 0i32;
    let mut class = String::new();
    let mut target = String::new();
    let mut args: Vec<String> = Vec::new();
    let mut child_pid = 0u32;

    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "key" => key = v.to_string(),
            "state" => state = State::parse(v),
            "pid" => pid = v.parse().unwrap_or(0),
            "child_pid" => child_pid = v.parse().unwrap_or(0),
            "started" => started = v.parse().unwrap_or(0),
            "finished" => finished = v.parse().unwrap_or(0),
            "exit" => exit = v.parse().unwrap_or(0),
            "class" => class = v.to_string(),
            "target" => target = v.to_string(),
            "args" => {
                args = if v.is_empty() {
                    Vec::new()
                } else {
                    v.split('\u{1f}').map(|s| s.to_string()).collect()
                }
            }
            _ => {}
        }
    }

    if key.is_empty() {
        return None;
    }
    Some(Job {
        key,
        state,
        pid,
        started,
        finished,
        exit,
        class,
        target,
        args,
        child_pid,
    })
}

pub fn running_jobs() -> Vec<Job> {
    let dir = cache_dir().join("jobs");
    let mut jobs = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return jobs;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(key) = name.strip_suffix(".job") else {
            continue;
        };
        if let Some(job) = read(key) {
            if job.is_live() {
                jobs.push(job);
            }
        }
    }
    jobs.sort_by_key(|j| j.started);
    jobs
}

/// The job currently owning a build directory, written by the owner so a
/// blocked agent gets a useful message instead of cargo's bare lock wait.
pub fn owner_path(target: &Path) -> PathBuf {
    target.join(".gbb.job")
}

pub fn write_owner(target: &Path, job: &Job) -> io::Result<()> {
    if !target.exists() {
        return Ok(());
    }
    let body = format!(
        "key={}\npid={}\nstarted={}\nclass={}\nargs={}\n",
        job.key,
        job.pid,
        job.started,
        job.class,
        job.args.join(" ")
    );
    fs::write(owner_path(target), body)
}

pub fn read_owner(target: &Path) -> Option<(String, u32, u64, String)> {
    let text = fs::read_to_string(owner_path(target)).ok()?;
    let mut key = String::new();
    let mut pid = 0;
    let mut started = 0;
    let mut args = String::new();
    for line in text.lines() {
        let (k, v) = line.split_once('=')?;
        match k {
            "key" => key = v.to_string(),
            "pid" => pid = v.parse().unwrap_or(0),
            "started" => started = v.parse().unwrap_or(0),
            "args" => args = v.to_string(),
            _ => {}
        }
    }
    if key.is_empty() {
        return None;
    }
    Some((key, pid, started, args))
}

pub fn clear_owner(target: &Path) {
    let _ = fs::remove_file(owner_path(target));
}

/// Median duration in seconds for a class+command pair, used for ETA text.
pub fn record_duration(class: &str, cmd: &str, bucket: u8, secs: u64) {
    let path = cache_dir().join("durations.txt");
    let line = format!("{class}\t{cmd}\t{bucket}\t{secs}\n");
    use std::io::Write;
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// What a command normally costs at a given level of concurrency.
///
/// A p90 measured while one build ran is a lie when four are running, so
/// samples are bucketed at 1, 2 and 3+ concurrent heavy jobs. Falls back to the
/// all-buckets distribution when a bucket has no samples yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Baseline {
    pub p50: u64,
    pub p90: u64,
    pub samples: usize,
}

pub fn baseline(class: &str, cmd: &str, bucket: u8) -> Baseline {
    let Ok(text) = fs::read_to_string(cache_dir().join("durations.txt")) else {
        return Baseline::default();
    };

    let mut exact: Vec<u64> = Vec::new();
    let mut any: Vec<u64> = Vec::new();
    for line in text.lines() {
        let mut parts = line.split('\t');
        let (Some(c), Some(m), Some(third)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if c != class || m != cmd {
            continue;
        }
        // Older records have no bucket: `class cmd secs`.
        let (sample_bucket, secs) = match (third.parse::<u8>(), parts.next()) {
            (Ok(b), Some(s)) => (b, s.parse::<u64>().ok()),
            _ => (0, third.parse::<u64>().ok()),
        };
        let Some(secs) = secs else {
            continue;
        };
        any.push(secs);
        if sample_bucket == bucket {
            exact.push(secs);
        }
    }

    // Bucket 0 asks for the whole distribution (used for a generic ETA).
    let mut values = if bucket != 0 && exact.len() >= 2 {
        exact
    } else {
        any
    };
    if values.is_empty() {
        return Baseline::default();
    }
    values.sort_unstable();
    let at = |pct: usize| {
        let index = (values.len() * pct / 100).min(values.len() - 1);
        values[index]
    };
    Baseline {
        p50: at(50),
        p90: at(90),
        samples: values.len(),
    }
}

pub fn median_duration(class: &str, cmd: &str) -> Option<u64> {
    let baseline = baseline(class, cmd, 0);
    (baseline.samples > 0).then_some(baseline.p50)
}

/// Live resource view of one running job, written by its sampler.
#[derive(Clone, Debug, Default)]
pub struct Health {
    pub state: String,
    pub elapsed: u64,
    pub cpu_pct: u64,
    pub rss_mb: u64,
    pub swap_gb: u64,
    pub rustc: u32,
    pub p90: u64,
    pub updated: u64,
}

pub fn health_path(key: &str) -> PathBuf {
    cache_dir().join("jobs").join(format!("{key}.health"))
}

pub fn write_health(key: &str, health: &Health) {
    let body = format!(
        "state={}\nelapsed={}\ncpu_pct={}\nrss_mb={}\nswap_gb={}\nrustc={}\np90={}\nupdated={}\n",
        health.state,
        health.elapsed,
        health.cpu_pct,
        health.rss_mb,
        health.swap_gb,
        health.rustc,
        health.p90,
        health.updated
    );
    let path = health_path(key);
    let tmp = path.with_extension("health.tmp");
    if fs::write(&tmp, body).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
}

/// Health is only meaningful while the sampler keeps refreshing it. A crashed
/// gbb leaves the last line behind; age it out instead of reporting a lie.
pub fn read_health(key: &str) -> Option<Health> {
    let text = fs::read_to_string(health_path(key)).ok()?;
    let mut health = Health::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "state" => health.state = v.to_string(),
            "elapsed" => health.elapsed = v.parse().unwrap_or(0),
            "cpu_pct" => health.cpu_pct = v.parse().unwrap_or(0),
            "rss_mb" => health.rss_mb = v.parse().unwrap_or(0),
            "swap_gb" => health.swap_gb = v.parse().unwrap_or(0),
            "rustc" => health.rustc = v.parse().unwrap_or(0),
            "p90" => health.p90 = v.parse().unwrap_or(0),
            "updated" => health.updated = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    let fresh =
        health.updated > 0 && now_epoch().saturating_sub(health.updated) <= HEALTH_STALE_SECS;
    (fresh && !health.state.is_empty()).then_some(health)
}

const HEALTH_STALE_SECS: u64 = 120;

pub fn clear_health(key: &str) {
    let _ = fs::remove_file(health_path(key));
    let _ = fs::remove_file(health_path(key).with_extension("health.tmp"));
}

/// Old job records are never read again, but `running_jobs` still stats every
/// one of them. Bound both directories, at most once an hour: a build pays the
/// scan once, not on every invocation.
pub fn prune_state(max_log_age_secs: u64, max_job_age_secs: u64) {
    let marker = cache_dir().join("prune.stamp");
    let now = now_epoch();
    if let Some(last) = fs::read_to_string(&marker)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
    {
        if now.saturating_sub(last) < 3600 {
            return;
        }
    }
    let _ = fs::write(&marker, now.to_string());

    prune_files(&cache_dir().join("logs"), max_log_age_secs);
    prune_files(&cache_dir().join("jobs"), max_job_age_secs);
}

fn prune_files(dir: &Path, max_age_secs: u64) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let now = now_epoch();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let age = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| now.saturating_sub(d.as_secs()))
            .unwrap_or(0);
        if age > max_age_secs {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_roundtrip_preserves_args_with_spaces() {
        let job = Job {
            key: "k1".into(),
            state: State::Done,
            pid: 42,
            started: 100,
            finished: 200,
            exit: 1,
            class: "heavy".into(),
            target: "/tmp/x/target".into(),
            args: vec!["test".into(), "-p".into(), "goldenbee".into(), "a b".into()],
            child_pid: 43,
        };
        let text = format!(
            "key={}\nstate={}\npid={}\nchild_pid={}\nstarted={}\nfinished={}\nexit={}\nclass={}\ntarget={}\nargs={}\n",
            job.key,
            job.state.as_str(),
            job.pid,
            job.child_pid,
            job.started,
            job.finished,
            job.exit,
            job.class,
            job.target,
            job.args.join("\u{1f}")
        );
        let parsed = parse_record(&text).expect("roundtrip");
        assert_eq!(parsed.args, job.args);
        assert_eq!(parsed.child_pid, 43);
        assert_eq!(parsed.exit, 1);
        assert_eq!(parsed.state, State::Done);
    }

    #[test]
    fn dead_pid_is_not_live() {
        let mut job = Job::started_running("k", 999_999_999, "light", Path::new("/tmp"), &[]);
        assert!(!job.is_live());
        job.pid = std::process::id();
        assert!(job.is_live());
    }
}
