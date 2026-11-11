//! Spawn cargo inside an isolated scope and mirror its output to console + log.
//!
//! Output is captured so a second agent that reuses this job's result can read
//! why it failed, even though it never owned the build.

use crate::classify::Class;
use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

pub struct Outcome {
    pub exit: i32,
    pub secs: u64,
}

pub fn run(
    args: &[String],
    log: &Path,
    class: Class,
    target_override: Option<&std::path::Path>,
) -> io::Result<Outcome> {
    let started = Instant::now();
    let log_file = Arc::new(Mutex::new(open_log(log)?));

    let mut command = build_command(args, class, target_override);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let console_out = Arc::new(Mutex::new(io::stdout()));
    let console_err = Arc::new(Mutex::new(io::stderr()));

    let log_a = Arc::clone(&log_file);
    let out_a = Arc::clone(&console_out);
    let t1 = thread::spawn(move || tee(stdout, log_a, out_a));

    let log_b = Arc::clone(&log_file);
    let err_b = Arc::clone(&console_err);
    let t2 = thread::spawn(move || tee(stderr, log_b, err_b));

    let status = child.wait()?;
    let _ = t1.join();
    let _ = t2.join();

    let exit = match status.code() {
        Some(code) => code,
        None => 128 + status.signal().unwrap_or(0),
    };

    Ok(Outcome {
        exit,
        secs: started.elapsed().as_secs(),
    })
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
/// desktop's memory hostage.
fn build_command(
    args: &[String],
    class: Class,
    target_override: Option<&std::path::Path>,
) -> Command {
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let systemd = !env_flag("GBB_NO_SYSTEMD") && which("systemd-run").is_some();
    let slice = env::var("GBB_SLICE").unwrap_or_else(|_| "dev.slice".to_string());

    let mut envs: Vec<(String, String)> = Vec::new();
    if let Ok(jobs) = env::var("GBB_JOBS") {
        envs.push(("CARGO_BUILD_JOBS".into(), jobs));
    }
    if let Some(dir) = target_override {
        envs.push((
            "CARGO_TARGET_DIR".into(),
            dir.to_string_lossy().into_owned(),
        ));
    }
    envs.extend(sccache_env());

    let mut cmd = if systemd {
        let mut c = Command::new("systemd-run");
        c.arg("--user")
            .arg("--scope")
            .arg("--collect")
            .arg("--quiet")
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
            env::var("CARGO_TERM_COLOR").unwrap_or_else(|_| "always".into()),
        );
    }

    cmd
}

/// sccache is a force multiplier across worktrees, but it cannot cache bins,
/// dylibs, or proc-macros and refuses incremental builds, so it is opt-in by
/// presence and opt-out by env.
fn sccache_env() -> Vec<(String, String)> {
    let mut out = Vec::new();
    if env_flag("GBB_NO_SCCACHE") || which("sccache").is_none() {
        return out;
    }
    out.push(("RUSTC_WRAPPER".into(), "sccache".into()));
    if env::var("SCCACHE_DIR").is_err() {
        out.push((
            "SCCACHE_DIR".into(),
            crate::jobs::cache_dir()
                .join("sccache")
                .to_string_lossy()
                .into_owned(),
        ));
    }
    if env::var("SCCACHE_CACHE_SIZE").is_err() {
        out.push(("SCCACHE_CACHE_SIZE".into(), "60G".into()));
    }
    out.push(("CARGO_INCREMENTAL".into(), "0".into()));
    out
}

pub fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| candidate.is_file())
}

/// An env switch counts only when set to something non-empty, so
/// `export GBB_NO_SYSTEMD=` in a shell profile cannot silently disable safety.
fn env_flag(name: &str) -> bool {
    env::var_os(name)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Last `n` lines of a job log, for reuse reporting.
pub fn tail(log: &Path, n: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(log) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_returns_last_lines() {
        let path = env::temp_dir().join(format!("gbb-tail-{}.log", std::process::id()));
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        assert_eq!(tail(&path, 2), vec!["c".to_string(), "d".to_string()]);
        std::fs::remove_file(&path).ok();
    }
}
