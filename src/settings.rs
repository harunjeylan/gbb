//! One place that reads the environment and one record that carries the result.
//!
//! Before this module existed, 26 variables were parsed where they were consumed,
//! two of them by two different modules, and an invalid value silently became the
//! default. Callers now receive values, not variable names.

use std::env;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Settings {
    pub wait: Duration,
    pub join_wait: Duration,
    pub lock_wait: Duration,
    pub reuse_secs: u64,
    pub reserve_gb: u64,

    pub light_slots: usize,
    pub heavy_slots: usize,
    pub job_mem_gb: u64,

    pub name: Option<String>,

    pub cache_enabled: bool,
    pub cache_gb: u64,
    pub snapshot_max_gb: u64,
    pub snapshot_min_secs: u64,
    pub snapshot_max_secs: u64,
    pub copy_mbps: Option<u64>,
    pub store_force: bool,

    pub no_systemd: bool,
    pub slice: String,
    pub jobs: Option<String>,
    pub no_sccache: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            wait: Duration::ZERO,
            join_wait: Duration::from_secs(900),
            lock_wait: Duration::from_secs(240),
            reuse_secs: 120,
            reserve_gb: 25,

            light_slots: 6,
            heavy_slots: 3,
            job_mem_gb: 4,

            name: None,

            cache_enabled: true,
            cache_gb: 80,
            snapshot_max_gb: 30,
            snapshot_min_secs: 300,
            snapshot_max_secs: 60,
            copy_mbps: None,
            store_force: false,

            no_systemd: false,
            slice: "dev.slice".to_string(),
            jobs: None,
            no_sccache: false,
        }
    }
}

impl Settings {
    /// Read the environment once. Everything downstream sees plain values.
    pub fn load() -> Settings {
        let default = Settings::default();
        Settings {
            wait: Duration::from_secs(env_u64("GBB_WAIT", default.wait.as_secs())),
            join_wait: Duration::from_secs(env_u64("GBB_JOIN_WAIT", default.join_wait.as_secs())),
            lock_wait: Duration::from_secs(env_u64("GBB_LOCK_WAIT", default.lock_wait.as_secs())),
            reuse_secs: env_u64("GBB_REUSE_SECS", default.reuse_secs),
            reserve_gb: env_u64("GBB_RESERVE_GB", default.reserve_gb),

            light_slots: env_usize("GBB_LIGHT_SLOTS", default.light_slots),
            heavy_slots: env_usize("GBB_HEAVY_SLOTS", default.heavy_slots),
            job_mem_gb: env_u64("GBB_JOB_MEM_GB", default.job_mem_gb),

            name: first_env(&["GBB_NAME", "GBB_AGENT"]),

            cache_enabled: !env_flag("GBB_NO_CACHE"),
            cache_gb: env_u64("GBB_CACHE_GB", default.cache_gb),
            snapshot_max_gb: env_u64("GBB_SNAPSHOT_MAX_GB", default.snapshot_max_gb),
            snapshot_min_secs: env_u64("GBB_SNAPSHOT_MIN_SECS", default.snapshot_min_secs),
            snapshot_max_secs: env_u64("GBB_SNAPSHOT_MAX_SECS", default.snapshot_max_secs),
            copy_mbps: env::var("GBB_COPY_MBPS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|n| *n > 1),
            store_force: env_flag("GBB_STORE_FORCE"),

            no_systemd: env_flag("GBB_NO_SYSTEMD"),
            slice: env::var("GBB_SLICE").unwrap_or(default.slice),
            jobs: env::var("GBB_JOBS").ok().filter(|v| !v.is_empty()),
            no_sccache: env_flag("GBB_NO_SCCACHE"),
        }
    }
}

/// An env switch counts only when set to something non-empty and not "0", so
/// `export GBB_NO_CACHE=` in a shell profile cannot silently change behaviour.
pub fn env_flag(name: &str) -> bool {
    env::var_os(name)
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false)
}

pub fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Zero or an unparseable value is not a usable count of slots.
fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

fn first_env(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_a_valid_configuration() {
        let s = Settings::default();
        assert!(s.light_slots > 0 && s.heavy_slots > 0);
        assert!(s.job_mem_gb > 0);
        assert!(s.snapshot_max_secs > 0);
        assert!(s.cache_gb > s.snapshot_max_gb);
    }

    #[test]
    fn invalid_numbers_fall_back_instead_of_becoming_zero() {
        std::env::set_var("GBB_SETTINGS_TEST_SLOTS", "0");
        assert_eq!(env_usize("GBB_SETTINGS_TEST_SLOTS", 6), 6);
        std::env::set_var("GBB_SETTINGS_TEST_SLOTS", "nonsense");
        assert_eq!(env_usize("GBB_SETTINGS_TEST_SLOTS", 6), 6);
        std::env::remove_var("GBB_SETTINGS_TEST_SLOTS");
    }

    #[test]
    fn flags_ignore_empty_and_zero() {
        std::env::set_var("GBB_SETTINGS_TEST_FLAG", "");
        assert!(!env_flag("GBB_SETTINGS_TEST_FLAG"));
        std::env::set_var("GBB_SETTINGS_TEST_FLAG", "0");
        assert!(!env_flag("GBB_SETTINGS_TEST_FLAG"));
        std::env::set_var("GBB_SETTINGS_TEST_FLAG", "1");
        assert!(env_flag("GBB_SETTINGS_TEST_FLAG"));
        std::env::remove_var("GBB_SETTINGS_TEST_FLAG");
    }
}
