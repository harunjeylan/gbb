//! Memory-aware admission control: global slots shared by every checkout.
//!
//! Disk locks are per build directory, but RAM is shared by the whole machine,
//! so the number of concurrent heavy jobs is capped globally.

use crate::classify::Class;
use crate::jobs;
use crate::settings::Settings;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

pub struct Slot {
    _file: File,
}

fn count_for(class: Class, settings: &Settings) -> usize {
    match class {
        Class::Light => settings.light_slots,
        Class::Heavy => settings.heavy_slots,
    }
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

/// One goldenbee-style link peaks around 5-6 GB. Admission is therefore limited
/// by actual available memory, not only by the configured slot count, so the box
/// does not swap itself to death when several builds start together.
fn job_memory_bytes(settings: &Settings) -> u64 {
    settings.job_mem_gb.max(1) * 1024 * 1024 * 1024
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

pub fn admitted(class: Class, settings: &Settings) -> usize {
    let total = count_for(class, settings);
    if class != Class::Heavy {
        return total;
    }
    let by_memory = (mem_available_bytes() / job_memory_bytes(settings)).max(1) as usize;
    total.min(by_memory)
}

/// Try to take any free slot of the class. Blocks up to `timeout` polling.
pub fn acquire(class: Class, timeout: Duration, settings: &Settings) -> io::Result<Option<Slot>> {
    let deadline = Instant::now() + timeout;
    loop {
        let admitted_now = admitted(class, settings);
        for index in 0..admitted_now {
            let path = path_for(class, index);
            let file = open(&path)?;
            match file.try_lock() {
                Ok(()) => return Ok(Some(Slot { _file: file })),
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
pub fn occupancy(class: Class, settings: &Settings) -> (usize, usize) {
    let mut used = 0;
    for index in 0..count_for(class, settings) {
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
    (used, admitted(class, settings))
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
        let settings = Settings::default();
        assert!(count_for(Class::Light, &settings) >= count_for(Class::Heavy, &settings));
    }

    #[test]
    fn memory_cap_never_exceeds_the_configured_slots() {
        let settings = Settings {
            heavy_slots: 2,
            job_mem_gb: 1,
            ..Settings::default()
        };
        assert!(admitted(Class::Heavy, &settings) <= 2);
    }

    #[test]
    fn occupancy_never_exceeds_total() {
        let settings = Settings::default();
        let (used, total) = occupancy(Class::Light, &settings);
        assert!(used <= total);
        assert!(total > 0);
    }
}
