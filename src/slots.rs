//! Memory-aware admission control: global slots shared by every checkout.
//!
//! Disk locks are per build directory, but RAM is shared by the whole machine,
//! so the number of concurrent heavy jobs is capped globally.

use crate::classify::Class;
use crate::jobs;
use std::env;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

pub struct Slot {
    _file: File,
}

fn count_for(class: Class) -> usize {
    let (var, default) = match class {
        Class::Light => ("GBB_LIGHT_SLOTS", 6usize),
        Class::Heavy => ("GBB_HEAVY_SLOTS", 3usize),
    };
    env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// One goldenbee-style link peaks around 5-6 GB. Admission is therefore limited
/// by actual available memory, not only by the configured slot count, so the box
/// does not swap itself to death when several builds start together.
fn job_memory_bytes() -> u64 {
    env::var("GBB_JOB_MEM_GB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(4)
        * 1024
        * 1024
        * 1024
}

fn mem_available_bytes() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else {
        return u64::MAX;
    };
    text.lines()
        .find(|line| line.starts_with("MemAvailable:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(u64::MAX)
}

pub fn admitted(class: Class) -> usize {
    let total = count_for(class);
    if class != Class::Heavy {
        return total;
    }
    let by_memory = (mem_available_bytes() / job_memory_bytes()).max(1) as usize;
    total.min(by_memory)
}

fn path_for(class: Class, index: usize) -> PathBuf {
    jobs::slot_dir().join(format!("{}-{index}.lock", class.as_str()))
}

fn open(path: &PathBuf) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(path)
}

/// Try to take any free slot of the class. Blocks up to `timeout` polling.
pub fn acquire(class: Class, timeout: Duration) -> io::Result<Option<Slot>> {
    let deadline = Instant::now() + timeout;
    loop {
        let admitted_now = admitted(class);
        for index in 0..admitted_now {
            let path = path_for(class, index);
            let file = open(&path)?;
            match file.try_lock() {
                Ok(()) => {
                    return Ok(Some(Slot { _file: file }));
                }
                Err(std::fs::TryLockError::WouldBlock) => continue,
                Err(std::fs::TryLockError::Error(e)) => return Err(e),
            }
        }

        if Instant::now() >= deadline {
            return Ok(None);
        }
        sleep(Duration::from_millis(250));
    }
}

/// Slots currently held and the number the machine can admit right now.
pub fn occupancy(class: Class) -> (usize, usize) {
    let mut used = 0;
    for index in 0..count_for(class) {
        let path = path_for(class, index);
        if let Ok(file) = open(&path) {
            match file.try_lock() {
                Ok(()) => {
                    let _ = file.unlock();
                }
                Err(_) => used += 1,
            }
        }
    }
    (used, admitted(class))
}

/// ETA for a queued job, from observed durations. Never a guess from thin air.
pub fn eta_text(class: Class, cmd: &str) -> String {
    match jobs::median_duration(class.as_str(), cmd) {
        Some(secs) if secs >= 60 => format!(" eta=~{}m{}s", secs / 60, secs % 60),
        Some(secs) => format!(" eta=~{secs}s"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_counts_default_by_class() {
        assert!(count_for(Class::Light) >= count_for(Class::Heavy));
    }

    #[test]
    fn occupancy_never_exceeds_total() {
        let (used, total) = occupancy(Class::Light);
        assert!(used <= total);
        assert!(total > 0);
    }
}
