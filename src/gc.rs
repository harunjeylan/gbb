//! Disk cleanup for the 100+ GB of per-worktree `target/` directories.
//!
//! Deletion is manual by design: build state belongs to whichever agent created
//! it, and silently dropping it costs more than the disk it frees.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Candidate {
    pub path: PathBuf,
    pub size_mb: u64,
    pub idle_days: u64,
    pub dirty: bool,
    pub has_target: bool,
    pub current: bool,
}

impl Candidate {
    pub fn eligible(&self, max_idle_days: u64, include_current: bool) -> bool {
        self.has_target
            && !self.dirty
            && self.idle_days >= max_idle_days
            && (include_current || !self.current)
    }
}

pub fn collect(repo: &Path) -> Vec<Candidate> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let current = fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());

    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let mut path: Option<PathBuf> = None;
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(rest));
            }
        }
        let Some(path) = path else { continue };

        let target = path.join("target");
        let has_target = target.is_dir();
        if !has_target {
            continue;
        }

        let canonical = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        out.push(Candidate {
            size_mb: du_mb(&target),
            idle_days: idle_days(&path),
            dirty: is_dirty(&path),
            has_target,
            current: canonical == current,
            path,
        });
    }

    out.sort_by_key(|c| std::cmp::Reverse(c.size_mb));
    out
}

pub fn free_gb() -> u64 {
    let output = Command::new("df").arg("-Pk").arg("/").output();
    let Ok(output) = output else { return 0 };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(line) = text.lines().nth(1) else {
        return 0;
    };
    let avail_kb: u64 = line
        .split_whitespace()
        .nth(3)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    avail_kb / 1024 / 1024
}

pub fn apply(candidates: &[Candidate], max_idle_days: u64, include_current: bool) -> (u64, u64) {
    let mut freed_mb = 0u64;
    let mut removed = 0u64;
    for candidate in candidates {
        if !candidate.eligible(max_idle_days, include_current) {
            continue;
        }
        let target = candidate.path.join("target");
        if fs::remove_dir_all(&target).is_ok() {
            freed_mb += candidate.size_mb;
            removed += 1;
        }
    }
    (removed, freed_mb)
}

fn du_mb(path: &Path) -> u64 {
    let output = Command::new("du").arg("-sm").arg(path).output();
    let Ok(output) = output else { return 0 };
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn is_dirty(repo: &Path) -> bool {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output();
    match output {
        Ok(o) => !o.stdout.is_empty(),
        // Unknown state must never be treated as clean.
        Err(_) => true,
    }
}

fn idle_days(repo: &Path) -> u64 {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "-1", "--format=%ct"])
        .output();
    let Ok(output) = output else { return 0 };
    let text = String::from_utf8_lossy(&output.stdout);
    let Ok(commit_time) = text.trim().parse::<u64>() else {
        return 0;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(commit_time) / 86_400
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(idle_days: u64, dirty: bool, current: bool) -> Candidate {
        Candidate {
            path: PathBuf::from("/tmp/x"),
            size_mb: 100,
            idle_days,
            dirty,
            has_target: true,
            current,
        }
    }

    #[test]
    fn dirty_worktree_is_never_eligible() {
        assert!(!candidate(30, true, false).eligible(7, false));
    }

    #[test]
    fn recent_worktree_is_not_eligible() {
        assert!(!candidate(2, false, false).eligible(7, false));
    }

    #[test]
    fn current_worktree_requires_opt_in() {
        assert!(!candidate(30, false, true).eligible(7, false));
        assert!(candidate(30, false, true).eligible(7, true));
    }
}
