//! Resolve the workspace root and the build directory cargo will actually use.
//!
//! The lock identity must match what cargo locks, otherwise `gbb` and a bare
//! `cargo` invocation disagree and we serialize nothing.

use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub struct Resolved {
    /// Workspace root: the directory holding the outermost `[workspace]`.
    pub root: PathBuf,
    /// The build directory cargo will use for this invocation.
    pub target: PathBuf,
}

pub fn resolve(cwd: &Path) -> io::Result<Resolved> {
    let root = find_workspace_root(cwd)?;

    let target = match env::var_os("CARGO_TARGET_DIR") {
        Some(v) if !v.is_empty() => {
            let p = PathBuf::from(v);
            if p.is_absolute() {
                p
            } else {
                cwd.join(p)
            }
        }
        _ => match config_target_dir(cwd)? {
            Some(p) => p,
            None => root.join("target"),
        },
    };

    Ok(Resolved {
        root,
        target: absolutize(&target),
    })
}

/// Walk up to the outermost Cargo.toml that declares a `[workspace]`.
/// Falls back to the nearest Cargo.toml (a standalone package).
fn find_workspace_root(cwd: &Path) -> io::Result<PathBuf> {
    let mut nearest: Option<PathBuf> = None;
    let mut outermost_workspace: Option<PathBuf> = None;

    for dir in cwd.ancestors() {
        let manifest = dir.join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        if nearest.is_none() {
            nearest = Some(dir.to_path_buf());
        }
        if let Ok(text) = fs::read_to_string(&manifest) {
            if text.contains("[workspace]") {
                outermost_workspace = Some(dir.to_path_buf());
            }
        }
    }

    outermost_workspace.or(nearest).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no Cargo.toml found above {}", cwd.display()),
        )
    })
}

/// `build.target-dir` from the nearest `.cargo/config.toml` walking up from cwd,
/// which is the same order cargo resolves it. Command-line/env values are
/// handled by the caller.
fn config_target_dir(cwd: &Path) -> io::Result<Option<PathBuf>> {
    for dir in cwd.ancestors() {
        for name in ["config.toml", "config"] {
            let path = dir.join(".cargo").join(name);
            if !path.is_file() {
                continue;
            }
            if let Ok(text) = fs::read_to_string(&path) {
                if let Some(value) = parse_target_dir(&text) {
                    let p = PathBuf::from(value);
                    return Ok(Some(if p.is_absolute() {
                        p
                    } else {
                        dir.to_path_buf().join(p)
                    }));
                }
            }
        }
    }
    Ok(None)
}

fn parse_target_dir(text: &str) -> Option<String> {
    let mut in_build_section = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_build_section = line.trim_end_matches(']').trim_end() == "[build";
            continue;
        }
        if !in_build_section || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "target-dir" {
                let v = value.trim().trim_matches('"').trim_matches('\'');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        env::current_dir()
            .map(|cwd| cwd.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_target_dir_from_build_section() {
        let text = "[build]\ntarget-dir = \"/tmp/x\"\n\n[net]\noffline = true\n";
        assert_eq!(parse_target_dir(text).as_deref(), Some("/tmp/x"));
    }

    #[test]
    fn ignores_target_dir_outside_build_section() {
        let text = "[net]\ntarget-dir = \"/tmp/x\"\n";
        assert_eq!(parse_target_dir(text), None);
    }

    #[test]
    fn finds_outermost_workspace_root() {
        let base = std::env::temp_dir().join(format!("gbb-ws-{}", std::process::id()));
        let inner = base.join("crates").join("a");
        fs::create_dir_all(&inner).unwrap();
        fs::write(
            base.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/a\"]\n",
        )
        .unwrap();
        fs::write(
            inner.join("Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let resolved = resolve(&inner).unwrap();
        assert_eq!(resolved.root, base);
        assert_eq!(resolved.target, base.join("target"));
        fs::remove_dir_all(&base).ok();
    }
}
