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
}

impl Job {
    pub fn started_running(key: &str, pid: u32, class: &str, target: &Path, args: &[String]) -> Job {
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
        "key={}\nstate={}\npid={}\nstarted={}\nfinished={}\nexit={}\nclass={}\ntarget={}\nargs={}\n",
        job.key,
        job.state.as_str(),
        job.pid,
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

    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "key" => key = v.to_string(),
            "state" => state = State::parse(v),
            "pid" => pid = v.parse().unwrap_or(0),
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
pub fn record_duration(class: &str, cmd: &str, secs: u64) {
    let path = cache_dir().join("durations.txt");
    let line = format!("{class}\t{cmd}\t{secs}\n");
    use std::io::Write;
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn median_duration(class: &str, cmd: &str) -> Option<u64> {
    let path = cache_dir().join("durations.txt");
    let text = fs::read_to_string(path).ok()?;
    let mut values: Vec<u64> = text
        .lines()
        .filter_map(|l| {
            let mut parts = l.split('\t');
            let c = parts.next()?;
            let m = parts.next()?;
            let s = parts.next()?.parse().ok()?;
            (c == class && m == cmd).then_some(s)
        })
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

/// Keep the log directory bounded; runs are cheap to reproduce.
pub fn prune_logs(max_age_secs: u64) {
    let dir = cache_dir().join("logs");
    let Ok(entries) = fs::read_dir(&dir) else {
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
        };
        let text = format!(
            "key={}\nstate={}\npid={}\nstarted={}\nfinished={}\nexit={}\nclass={}\ntarget={}\nargs={}\n",
            job.key,
            job.state.as_str(),
            job.pid,
            job.started,
            job.finished,
            job.exit,
            job.class,
            job.target,
            job.args.join("\u{1f}")
        );
        let parsed = parse_record(&text).expect("roundtrip");
        assert_eq!(parsed.args, job.args);
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
