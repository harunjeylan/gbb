//! Shared build cache for every checkout of the same repository.
//!
//! Cargo target dirs are portable: a copied `target/` keeps its fingerprints and
//! cargo rebuilds only the units whose inputs actually changed. This machine is
//! ext4 (no reflink, no copy-on-write), so a snapshot is a real copy. That is
//! still cheap next to a full rebuild: measured 13 GB of deps in a few seconds
//! on NVMe, against 10+ minutes of compilation.
//!
//! Layout: `<cache>/targets/<workspace>/<profile>/<digest>/{target,meta}`.
//! `incremental/` is skipped on purpose: it is machine-local, huge (11 GB here),
//! and cargo rebuilds it from scratch without losing artifact reuse.

use crate::jobs;
use crate::settings::Settings;
use std::collections::hash_map::DefaultHasher;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Directories that are never worth copying between build dirs.
const SKIP_DIRS: &[&str] = &["incremental", "tmp", "parallel", "agents"];

pub struct Entry {
    pub dir: PathBuf,
    pub digest: String,
    pub created: u64,
    pub bytes: u64,
    pub manifest: String,
    pub profile: String,
    pub workspace: String,
}

pub fn enabled(settings: &Settings) -> bool {
    settings.cache_enabled
}

/// Disk kept free for builds themselves, never handed to the cache.
fn reserve_gb(settings: &Settings) -> u64 {
    settings.reserve_gb
}

fn cache_gb(settings: &Settings) -> u64 {
    settings.cache_gb
}

fn snapshot_max_bytes(settings: &Settings) -> u64 {
    settings.snapshot_max_gb * 1024 * 1024 * 1024
}

fn snapshot_min_secs(settings: &Settings) -> u64 {
    settings.snapshot_min_secs
}

/// A snapshot holds the build-dir lock, so its cost is a wait imposed on every
/// other command in that directory. This is the cap on that wait.
fn snapshot_max_secs(settings: &Settings) -> u64 {
    settings.snapshot_max_secs
}

/// Copy throughput in MB/s: learned from the last snapshot, overridable.
fn copy_mbps(settings: &Settings) -> u64 {
    fs::read_to_string(jobs::cache_dir().join("copy_mbps"))
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .filter(|n| *n > 1)
        .or(settings.copy_mbps)
        .unwrap_or(40)
}

fn record_copy_mbps(bytes: u64, secs: u64) {
    if secs == 0 || bytes < 1024 * 1024 {
        return;
    }
    let mbps = (bytes / (1024 * 1024)).max(1) / secs;
    let _ = fs::write(jobs::cache_dir().join("copy_mbps"), mbps.to_string());
}

fn estimated_copy_secs(bytes: u64, settings: &Settings) -> u64 {
    bytes / (copy_mbps(settings).max(1) * 1024 * 1024)
}

fn dir_size(root: &Path) -> u64 {
    let mut total = 0;
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIP_DIRS.contains(&name.as_ref()) || name.starts_with(".gbb") {
                continue;
            }
            total += dir_size(&entry.path());
        } else if file_type.is_file() {
            total += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    total
}

/// Identity of the repository, stable across worktrees: the git common dir.
pub fn workspace_key(root: &Path) -> String {
    let common = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    let identity = match common {
        Some(path) => {
            let p = PathBuf::from(path);
            let p = if p.is_absolute() { p } else { root.join(p) };
            fs::canonicalize(&p)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        }
        None => root.to_string_lossy().into_owned(),
    };
    format!("{:016x}", hash(&identity))
}

/// cargo names the dev profile directory `debug`.
pub fn profile_dir(profile: &str) -> &str {
    if profile == "dev" {
        "debug"
    } else {
        profile
    }
}

fn targets_root(workspace: &str, profile: &str) -> PathBuf {
    jobs::cache_dir()
        .join("targets")
        .join(workspace)
        .join(profile_dir(profile))
}

fn meta_path(dir: &Path) -> PathBuf {
    dir.join("meta")
}

/// True when this build dir has never been populated for the profile.
pub fn is_cold(build_dir: &Path, profile: &str) -> bool {
    !build_dir.join(profile_dir(profile)).is_dir()
}

pub fn entries(workspace: &str, profile: &str) -> Vec<Entry> {
    let root = targets_root(workspace, profile);
    let Ok(read) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = read
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| read_meta(&e.path(), workspace, profile))
        .collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.created));
    out
}

fn read_meta(dir: &Path, workspace: &str, profile: &str) -> Option<Entry> {
    let text = fs::read_to_string(meta_path(dir)).ok()?;
    let mut digest = String::new();
    let mut created = 0u64;
    let mut bytes = 0u64;
    let mut manifest = String::new();

    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "digest" => digest = v.to_string(),
            "created" => created = v.parse().unwrap_or(0),
            "bytes" => bytes = v.parse().unwrap_or(0),
            "manifest" => manifest = v.to_string(),
            _ => {}
        }
    }
    if digest.is_empty() {
        return None;
    }
    Some(Entry {
        dir: dir.to_path_buf(),
        digest,
        created,
        bytes,
        manifest,
        profile: profile_dir(profile).to_string(),
        workspace: workspace.to_string(),
    })
}

/// Restore the newest snapshot into a cold build dir. Returns its digest.
pub fn seed(
    build_dir: &Path,
    workspace: &str,
    profile: &str,
    settings: &Settings,
) -> io::Result<Option<String>> {
    if !enabled(settings) {
        return Ok(None);
    }
    let Some(newest) = entries(workspace, profile).into_iter().next() else {
        return Ok(None);
    };
    let from = newest.dir.join("target");
    if !from.is_dir() {
        return Ok(None);
    }

    let needed_gb = newest.bytes / (1024 * 1024 * 1024) + 1;
    let free = crate::gc::free_gb();
    if free < reserve_gb(settings) + needed_gb {
        eprintln!(
            "gbb: cache seed skipped ({free} GB free, need {} GB)",
            reserve_gb(settings) + needed_gb
        );
        return Ok(None);
    }

    if build_dir.exists() {
        // Only ever fill an empty shell; never merge into a live build dir.
        // Every build dir holds our own lock file, so ignore those.
        let has_content = fs::read_dir(build_dir)
            .map(|entries| {
                entries.flatten().any(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    !name.starts_with(".gbb") && name != "CACHEDIR.TAG"
                })
            })
            .unwrap_or(true);
        if has_content {
            return Ok(None);
        }
    }

    fs::create_dir_all(build_dir)?;
    copy_tree(&from, build_dir, usize::MAX)?;
    Ok(Some(newest.digest))
}

/// Copying a 13 GB build dir is minutes on a loaded disk, so snapshots happen
/// in a detached child that only runs when no other job is building.
pub fn spawn_store(
    build_dir: &Path,
    workspace: &str,
    profile: &str,
    digest: &str,
    settings: &Settings,
) {
    if !enabled(settings) {
        return;
    }

    let others = jobs::running_jobs()
        .iter()
        .filter(|job| job.pid != std::process::id())
        .count();
    if others > 0 && !settings.store_force {
        eprintln!("gbb: cache snapshot deferred ({others} other job(s) running)");
        return;
    }

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let log = jobs::cache_dir().join("store.log");

    let mut command = Command::new(exe);
    command
        .arg("cache")
        .arg("--store")
        .env("GBB_STORE_BUILD_DIR", build_dir)
        .env("GBB_STORE_WORKSPACE", workspace)
        .env("GBB_STORE_PROFILE", profile)
        .env("GBB_STORE_DIGEST", digest)
        .stdin(Stdio::null());

    match OpenOptions::new().create(true).append(true).open(&log) {
        Ok(file) => {
            let clone = file.try_clone();
            command.stdout(file);
            if let Ok(file) = clone {
                command.stderr(file);
            }
        }
        Err(_) => {
            command.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    match command.spawn() {
        Ok(_) => eprintln!(
            "gbb: caching build dir in background (log: {})",
            log.display()
        ),
        Err(e) => eprintln!("gbb: cache snapshot skipped: {e}"),
    }
}

/// Snapshot a build dir after a successful build, unless an identical one exists.
pub fn store(
    build_dir: &Path,
    workspace: &str,
    profile: &str,
    digest: &str,
    settings: &Settings,
) -> io::Result<Option<PathBuf>> {
    if !enabled(settings) || !build_dir.is_dir() {
        return Ok(None);
    }
    if !build_dir.join(profile_dir(profile)).is_dir() {
        return Ok(None);
    }

    let existing = entries(workspace, profile);
    if let Some(newest) = existing.first() {
        let fresh_enough =
            jobs::now_epoch().saturating_sub(newest.created) < snapshot_min_secs(settings);
        if newest.digest == digest && fresh_enough {
            return Ok(None);
        }
    }

    let destination = targets_root(workspace, profile).join(digest);
    if destination.is_dir() {
        return Ok(None);
    }

    // Snapshots must never be the reason a build cannot run.
    if crate::gc::free_gb() < reserve_gb(settings) + 2 {
        let _ = prune(cache_gb(settings) * 1024 * 1024 * 1024, true);
        let free = crate::gc::free_gb();
        if free < reserve_gb(settings) + 2 {
            let reserve = reserve_gb(settings);
            eprintln!("gbb: cache snapshot skipped ({free} GB free, reserve {reserve} GB)");
            return Ok(None);
        }
    }

    // The copy holds the build-dir lock, so a slow disk turns a snapshot into the
    // very wait gbb exists to remove. Refuse the snapshot instead.
    let size = dir_size(build_dir);
    let estimate = estimated_copy_secs(size, settings);
    if estimate > snapshot_max_secs(settings) {
        eprintln!(
            "gbb: cache snapshot skipped ({} GB would hold the build dir ~{}s)",
            size / (1024 * 1024 * 1024),
            estimate
        );
        return Ok(None);
    }

    let staging = targets_root(workspace, profile).join(format!(".staging-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    let copy_started = std::time::Instant::now();
    let copied = copy_tree(
        build_dir,
        &staging.join("target"),
        snapshot_max_bytes(settings) as usize,
    );
    match copied {
        Ok(bytes) => {
            record_copy_mbps(bytes, copy_started.elapsed().as_secs());
            let manifest = manifest(&staging.join("target"));
            let meta = format!(
                "digest={digest}\ncreated={}\nbytes={bytes}\nmanifest={manifest}\n",
                jobs::now_epoch()
            );
            fs::write(meta_path(&staging), meta)?;
            if let Err(e) = fs::rename(&staging, &destination) {
                let _ = fs::remove_dir_all(&staging);
                // Another worker may have published this exact digest between
                // the check above and the rename. Losing that race is success.
                let benign = matches!(e.raw_os_error(), Some(17) | Some(39));
                if benign {
                    return Ok(Some(destination));
                }
                return Err(e);
            }
            prune(cache_gb(settings) * 1024 * 1024 * 1024, true)?;
            Ok(Some(destination))
        }
        Err(e) if e.kind() == io::ErrorKind::FileTooLarge => {
            let _ = fs::remove_dir_all(&staging);
            Ok(None)
        }
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

/// Fingerprint of the restored artifacts, so equal builds collapse to one entry.
fn manifest(target: &Path) -> String {
    let mut files: Vec<(String, u64, u64)> = Vec::new();
    collect_manifest(target, target, &mut files);
    files.sort_unstable();

    let mut hasher = DefaultHasher::new();
    let mut hasher2 = DefaultHasher::new();
    for entry in &files {
        entry.hash(&mut hasher);
        entry.hash(&mut hasher2);
    }
    format!("{:016x}{:016x}", hasher.finish(), hasher2.finish())
}

fn collect_manifest(dir: &Path, root: &Path, out: &mut Vec<(String, u64, u64)>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            collect_manifest(&path, root, out);
        } else if file_type.is_file() {
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            // Dep-info and rustc metadata embed absolute paths, so they differ
            // between checkouts of the same commit. They identify nothing.
            if rel.ends_with(".d") || rel.ends_with(".rustc_info.json") {
                continue;
            }
            // Path + size, deliberately not mtime: two copies of the same build
            // have different mtimes and must still dedupe to one snapshot.
            out.push((rel, meta.len(), 0));
        }
    }
}

/// Recursive copy that skips machine-local directories and refuses to blow the
/// snapshot budget. Returns the copied byte count.
fn copy_tree(src: &Path, dst: &Path, max_bytes: usize) -> io::Result<u64> {
    fs::create_dir_all(dst)?;
    let mut total = 0u64;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let file_type = entry.file_type()?;
        let to = dst.join(&name);

        if file_type.is_dir() {
            let name_str = name.to_string_lossy();
            if SKIP_DIRS.contains(&name_str.as_ref()) || name_str.starts_with(".gbb") {
                continue;
            }
            total += copy_tree(&entry.path(), &to, max_bytes)?;
        } else if file_type.is_file() {
            let len = entry.metadata()?.len();
            if total + len > max_bytes as u64 {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "snapshot budget",
                ));
            }
            fs::copy(entry.path(), &to)?;
            total += len;
        } else if file_type.is_symlink() {
            let target = fs::read_link(entry.path())?;
            if target.is_absolute() {
                // Absolute links point into the source checkout. Recreating one
                // would carry a path from another worktree into this seed.
                match fs::metadata(entry.path()) {
                    Ok(meta) if meta.is_file() => {
                        let len = meta.len();
                        if total + len > max_bytes as u64 {
                            return Err(io::Error::new(
                                io::ErrorKind::FileTooLarge,
                                "snapshot budget",
                            ));
                        }
                        fs::copy(entry.path(), &to)?;
                        total += len;
                    }
                    // Directory links are skipped rather than followed: following
                    // them can escape the tree or loop.
                    _ => continue,
                }
            } else {
                std::os::unix::fs::symlink(target, &to)?;
            }
        }
    }
    Ok(total)
}

/// Delete byte-budget overruns (oldest first) and collapse identical snapshots.
pub fn prune(budget_bytes: u64, apply: bool) -> io::Result<(u64, u64)> {
    let mut all = listing();

    // 1. Duplicates by artifact manifest: newest wins.
    let mut removed_bytes = 0u64;
    let mut removed = 0u64;
    let mut seen: std::collections::HashMap<String, &Entry> = std::collections::HashMap::new();
    let mut doomed: Vec<PathBuf> = Vec::new();
    for entry in &all {
        match seen.get(&entry.manifest) {
            Some(kept) if kept.created >= entry.created => {
                doomed.push(entry.dir.clone());
                removed_bytes += entry.bytes;
                removed += 1;
            }
            Some(older) => {
                doomed.push(older.dir.clone());
                removed_bytes += older.bytes;
                removed += 1;
                seen.insert(entry.manifest.clone(), entry);
            }
            None => {
                seen.insert(entry.manifest.clone(), entry);
            }
        }
    }

    // 2. Byte budget: drop oldest, keep the newest per profile.
    let mut live: Vec<&Entry> = all.iter().filter(|e| !doomed.contains(&e.dir)).collect();
    let mut total: u64 = live.iter().map(|e| e.bytes).sum();
    if total > budget_bytes {
        live.sort_by_key(|e| e.created);
        while total > budget_bytes {
            let Some(oldest) = live.first() else { break };
            // Never prune the newest snapshot of a profile: that is the seed.
            let newest_for_profile = live
                .iter()
                .filter(|e| e.profile == oldest.profile && e.workspace == oldest.workspace)
                .count();
            if newest_for_profile <= 1 {
                live.remove(0);
                continue;
            }
            doomed.push(oldest.dir.clone());
            total -= oldest.bytes;
            removed_bytes += oldest.bytes;
            removed += 1;
            live.remove(0);
        }
    }

    if apply {
        for dir in &doomed {
            let _ = fs::remove_dir_all(dir);
        }
        all.retain(|e| !doomed.contains(&e.dir));
    }
    Ok((removed, removed_bytes))
}

pub fn listing() -> Vec<Entry> {
    let mut out = Vec::new();
    let base = jobs::cache_dir().join("targets");
    let Ok(workspaces) = fs::read_dir(&base) else {
        return out;
    };
    for ws in workspaces.flatten() {
        if !ws.path().is_dir() {
            continue;
        }
        let ws_name = ws.file_name().to_string_lossy().into_owned();
        let Ok(profiles) = fs::read_dir(ws.path()) else {
            continue;
        };
        for profile in profiles.flatten() {
            if !profile.path().is_dir() {
                continue;
            }
            let profile_name = profile.file_name().to_string_lossy().into_owned();
            let Ok(entries) = fs::read_dir(profile.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(e) = read_meta(&entry.path(), &ws_name, &profile_name) {
                        out.push(e);
                    }
                }
            }
        }
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.created));
    out
}

pub fn total_bytes() -> u64 {
    listing().iter().map(|e| e.bytes).sum()
}

fn hash(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_dir_maps_dev_to_debug() {
        assert_eq!(profile_dir("dev"), "debug");
        assert_eq!(profile_dir("release"), "release");
        assert_eq!(profile_dir("bench"), "bench");
    }

    #[test]
    fn copy_tree_skips_incremental() {
        let base = std::env::temp_dir().join(format!("gbb-cache-{}", std::process::id()));
        let src = base.join("src");
        fs::create_dir_all(src.join("incremental")).unwrap();
        fs::write(src.join("incremental").join("chunk"), b"x").unwrap();
        fs::create_dir_all(src.join("deps")).unwrap();
        fs::write(src.join("deps").join("libx.rlib"), b"artifact").unwrap();
        fs::write(src.join("CACHEDIR.TAG"), b"tag").unwrap();

        let dst = base.join("dst");
        let bytes = copy_tree(&src, &dst, usize::MAX).unwrap();
        assert!(dst.join("deps").join("libx.rlib").is_file());
        assert!(dst.join("CACHEDIR.TAG").is_file());
        assert!(!dst.join("incremental").exists());
        assert_eq!(bytes, 8 + 3);

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn copy_tree_enforces_budget() {
        let base = std::env::temp_dir().join(format!("gbb-budget-{}", std::process::id()));
        let src = base.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.bin"), vec![0u8; 4096]).unwrap();

        let err = copy_tree(&src, &base.join("dst"), 1024).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn absolute_symlinks_are_materialised_not_carried_over() {
        let base = std::env::temp_dir().join(format!("gbb-link-{}", std::process::id()));
        let src = base.join("src");
        fs::create_dir_all(&src).unwrap();
        let payload = base.join("payload.txt");
        fs::write(&payload, b"from-elsewhere").unwrap();
        std::os::unix::fs::symlink(&payload, src.join("abs-link")).unwrap();
        std::os::unix::fs::symlink("payload.txt", src.join("rel-link")).unwrap();

        let dst = base.join("dst");
        copy_tree(&src, &dst, usize::MAX).unwrap();

        assert!(dst.join("abs-link").is_file());
        assert_eq!(fs::read(dst.join("abs-link")).unwrap(), b"from-elsewhere");
        assert!(dst
            .join("rel-link")
            .symlink_metadata()
            .unwrap()
            .is_symlink());

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn dir_size_skips_incremental_and_counts_files() {
        let base = std::env::temp_dir().join(format!("gbb-size-{}", std::process::id()));
        let src = base.join("src");
        fs::create_dir_all(src.join("incremental")).unwrap();
        fs::write(src.join("incremental").join("chunk"), vec![0u8; 1024]).unwrap();
        fs::create_dir_all(src.join("deps")).unwrap();
        fs::write(src.join("deps").join("a.rlib"), vec![0u8; 2048]).unwrap();

        assert_eq!(dir_size(&src), 2048);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn copy_estimate_grows_with_size() {
        let settings = Settings::default();
        let small = estimated_copy_secs(64 * 1024 * 1024, &settings);
        let big = estimated_copy_secs(8 * 1024 * 1024 * 1024, &settings);
        assert!(big > small, "{big} should exceed {small}");
    }
}
