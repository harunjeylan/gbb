//! Cheap input digest: stat-walk of build-relevant files.
//!
//! Content hashing 186k lines on every invocation would cost more than it saves,
//! so we hash (relative path, mtime, size). Any build-relevant edit changes at
//! least one of those.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::Path;

/// Directories that never feed a Rust build.
const SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    ".git",
    ".hg",
    ".svn",
    "dist",
    "build",
    ".next",
    ".turbo",
    ".svelte-kit",
    ".cache",
    ".venv",
    "__pycache__",
];

/// Files that always affect the build, regardless of extension.
const ALWAYS_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "rust-toolchain.toml",
    "rust-toolchain",
];

const BUILD_EXTS: &[&str] = &["rs", "sql", "proto", "toml"];

pub fn workspace_digest(root: &Path) -> io::Result<String> {
    let mut entries = Vec::new();
    walk(root, root, &mut entries)?;
    // Deterministic order regardless of readdir order.
    entries.sort_unstable();

    let mut a = DefaultHasher::new();
    let mut b = DefaultHasher::new();
    0u8.hash(&mut a);
    1u8.hash(&mut b);
    for e in &entries {
        e.hash(&mut a);
        e.hash(&mut b);
    }
    Ok(format!("{:016x}{:016x}", a.finish(), b.finish()))
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, u64, u64)>) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };

        if file_type.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if SKIP_DIRS.contains(&name.as_ref()) || name.starts_with(".gbb") {
                continue;
            }
            walk(&path, root, out)?;
            continue;
        }

        if !file_type.is_file() || !is_build_relevant(&path) {
            continue;
        }

        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();

        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let size = meta.len();
        let mtime = mtime_nanos(&meta);
        out.push((rel, size, mtime));
    }
    Ok(())
}

fn is_build_relevant(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    if ALWAYS_FILES.contains(&name.as_ref()) {
        return true;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => BUILD_EXTS.contains(&ext),
        None => false,
    }
}

fn mtime_nanos(meta: &fs::Metadata) -> u64 {
    use std::time::UNIX_EPOCH;
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Two-level hash used for job keys: argv + target + input digest.
pub fn job_key(target: &Path, class: &str, args: &[String], digest: &str) -> String {
    let mut a = DefaultHasher::new();
    let mut b = DefaultHasher::new();
    2u8.hash(&mut a);
    3u8.hash(&mut b);

    for part in [target.to_string_lossy().as_ref(), class, digest] {
        part.hash(&mut a);
        part.hash(&mut b);
    }
    for arg in args {
        arg.hash(&mut a);
        arg.hash(&mut b);
    }
    format!("{:016x}{:016x}", a.finish(), b.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn digest_is_stable_and_changes_with_content() {
        let base = std::env::temp_dir().join(format!("gbb-digest-{}", std::process::id()));
        let src = base.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(base.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::write(src.join("main.rs"), "fn main() {}\n").unwrap();

        let first = workspace_digest(&base).unwrap();
        let second = workspace_digest(&base).unwrap();
        assert_eq!(first, second);

        fs::write(src.join("main.rs"), "fn main() { let _ = 1; }\n").unwrap();
        let changed = workspace_digest(&base).unwrap();
        assert_ne!(first, changed);

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn target_dir_is_ignored() {
        let base = std::env::temp_dir().join(format!("gbb-digest-t-{}", std::process::id()));
        let target = base.join("target").join("debug");
        fs::create_dir_all(&target).unwrap();
        fs::write(base.join("Cargo.toml"), "[package]\n").unwrap();

        let before = workspace_digest(&base).unwrap();
        fs::write(target.join("artifact.rs"), "junk").unwrap();
        let after = workspace_digest(&base).unwrap();
        assert_eq!(before, after);

        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn job_key_depends_on_args() {
        let t = Path::new("/tmp/x/target");
        let k1 = job_key(t, "light", &["check".into()], "abc");
        let k2 = job_key(t, "light", &["test".into()], "abc");
        assert_ne!(k1, k2);
    }
}
