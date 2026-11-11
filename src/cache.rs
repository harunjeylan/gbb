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
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Directories that are never worth copying between build dirs.
const SKIP_DIRS: &[&str] = &["incremental", "tmp", "agents"];

pub struct Entry {
    pub dir: PathBuf,
    pub digest: String,
    pub created: u64,
    pub bytes: u64,
    pub manifest: String,
    pub profile: String,
    pub workspace: String,
}

pub fn enabled() -> bool {
    !crate::env_flag("GBB_NO_CACHE")
}

fn cache_gb() -> u64 {
    crate::env_u64("GBB_CACHE_GB", 80)
}

fn snapshot_max_bytes() -> u64 {
    crate::env_u64("GBB_SNAPSHOT_MAX_GB", 30) * 1024 * 1024 * 1024
}

fn snapshot_min_secs() -> u64 {
    crate::env_u64("GBB_SNAPSHOT_MIN_SECS", 300)
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
pub fn seed(build_dir: &Path, workspace: &str, profile: &str) -> io::Result<Option<String>> {
    if !enabled() {
        return Ok(None);
    }
    let Some(newest) = entries(workspace, profile).into_iter().next() else {
        return Ok(None);
    };
    let from = newest.dir.join("target");
    if !from.is_dir() {
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

/// Snapshot a build dir after a successful build, unless an identical one exists.
pub fn store(
    build_dir: &Path,
    workspace: &str,
    profile: &str,
    digest: &str,
) -> io::Result<Option<PathBuf>> {
    if !enabled() || !build_dir.is_dir() {
        return Ok(None);
    }
    if !build_dir.join(profile_dir(profile)).is_dir() {
        return Ok(None);
    }

    let existing = entries(workspace, profile);
    if let Some(newest) = existing.first() {
        let fresh_enough = jobs::now_epoch().saturating_sub(newest.created) < snapshot_min_secs();
        if newest.digest == digest && fresh_enough {
            return Ok(None);
        }
    }

    let destination = targets_root(workspace, profile).join(digest);
    if destination.is_dir() {
        return Ok(None);
    }

    let staging = targets_root(workspace, profile).join(format!(".staging-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    let copied = copy_tree(
        build_dir,
        &staging.join("target"),
        snapshot_max_bytes() as usize,
    );
    match copied {
        Ok(bytes) => {
            let manifest = manifest(&staging.join("target"));
            let meta = format!(
                "digest={digest}\ncreated={}\nbytes={bytes}\nmanifest={manifest}\n",
                jobs::now_epoch()
            );
            fs::write(meta_path(&staging), meta)?;
            if let Err(e) = fs::rename(&staging, &destination) {
                let _ = fs::remove_dir_all(&staging);
                return Err(e);
            }
            prune(cache_gb() * 1024 * 1024 * 1024, true)?;
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
            std::os::unix::fs::symlink(target, &to)?;
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
}
