//! Path safety — PLAN §8.2.
//!
//! MVP strategy (portable, conservative):
//!   1. Logical `..`/`.` normalize (no syscall).
//!   2. Canonicalize the deepest existing prefix — resolves symlinks, fails
//!      on dangling references. For non-existent targets (e.g. `Write` to a
//!      new file), re-attach the uncreated tail under a canonical parent.
//!   3. Containment check: canonical result must sit under canonical root.
//!   4. Component-wise symlink check on the original logical path — reject
//!      if any intermediate component is a symlink (TOCTOU narrowing).
//!   5. `DENY_PATH_PREFIXES` scan on the final canonical path.
//!   6. NTFS ADS `:` mid-component reject (Windows — Unix tolerates `:`
//!      in names but a `:` never appears in legitimate Unix path segments,
//!      so this is a no-op in practice and cheap to keep).
//!
//! Iter 2 will swap step 2 on Linux with `openat2(RESOLVE_BENEATH |
//! RESOLVE_NO_SYMLINKS)` and on macOS with `O_NOFOLLOW + fstat` dev/inode
//! re-check. Both require `unsafe` and/or `rustix`; outside the MVP gate.

use std::io;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

pub const DENY_PATH_PREFIXES: &[&str] =
    &["/proc/", "/dev/tcp", "/dev/fd/", "\\\\?\\", "\\\\.\\pipe\\"];

#[derive(Debug, Error)]
pub enum PathError {
    #[error("path escapes root: {0}")]
    Escapes(PathBuf),
    #[error("symlink traversal blocked: {0}")]
    SymlinkBlocked(PathBuf),
    #[error("deny-listed path: {0}")]
    Denied(PathBuf),
    #[error("TOCTOU re-check failed (dev/inode drift): {0}")]
    Toctou(PathBuf),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Resolve `target` relative to `root` into a canonical path that is
/// guaranteed to live under `root`, with no symlink traversal and no
/// deny-listed prefix.
///
/// Works for both existing targets (Read/Edit) and not-yet-created targets
/// (Write) — the last non-existent tail is rejoined under a canonical parent.
pub fn canonicalize_within(root: &Path, target: &Path) -> Result<PathBuf, PathError> {
    let canonical_root = root
        .canonicalize()
        .map_err(|e| path_io("canonicalize root", root, e))?;

    let joined = if target.is_absolute() {
        target.to_path_buf()
    } else {
        canonical_root.join(target)
    };
    let normalized = logical_normalize(&joined);

    let (canonical_prefix, tail) = canonicalize_deepest_prefix(&normalized)?;
    let canonical = join_logical(&canonical_prefix, &tail);

    if !canonical.starts_with(&canonical_root) {
        return Err(PathError::Escapes(canonical));
    }
    reject_symlink_traversal(&normalized, &canonical_root)?;
    check_deny_list(&canonical)?;
    Ok(canonical)
}

/// Convenience for parent-then-verify patterns used by `Write`.
pub fn canonicalize_parent_within(
    root: &Path,
    target: &Path,
) -> Result<(PathBuf, std::ffi::OsString), PathError> {
    let file_name = target
        .file_name()
        .ok_or_else(|| path_io("no file name", target, io::Error::from(io::ErrorKind::InvalidInput)))?
        .to_os_string();
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let canonical_parent = canonicalize_within(root, parent)?;
    Ok((canonical_parent, file_name))
}

/// Returns `Err(PathError::Denied)` if `p` matches a deny-listed prefix or
/// contains an NTFS ADS-style `:` in any component. Call after canonicalize.
pub fn check_deny_list(p: &Path) -> Result<(), PathError> {
    let s = p.to_string_lossy();
    for prefix in DENY_PATH_PREFIXES {
        if s.starts_with(prefix) {
            return Err(PathError::Denied(p.to_path_buf()));
        }
    }
    // NTFS ADS — component body contains `:`. Unix legit paths don't.
    if cfg!(windows) {
        for comp in p.components() {
            if let Component::Normal(os) = comp {
                if os.to_string_lossy().contains(':') {
                    return Err(PathError::Denied(p.to_path_buf()));
                }
            }
        }
    }
    Ok(())
}

/// Logical normalize — strips `.` and resolves `..` without touching the fs.
fn logical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Split the normalized path into the longest existing canonical prefix and
/// the uncreated remainder. Creates zero files — pure lookup.
fn canonicalize_deepest_prefix(p: &Path) -> Result<(PathBuf, PathBuf), PathError> {
    // Try the whole path first.
    if let Ok(c) = p.canonicalize() {
        return Ok((c, PathBuf::new()));
    }
    // Walk up until we find an existing ancestor.
    let mut tail_rev: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cursor = p;
    loop {
        let parent = match cursor.parent() {
            Some(pp) if !pp.as_os_str().is_empty() => pp,
            _ => {
                return Err(PathError::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("no canonical ancestor for {}", p.display()),
                )))
            }
        };
        if let Some(name) = cursor.file_name() {
            tail_rev.push(name);
        }
        if let Ok(c) = parent.canonicalize() {
            let mut tail = PathBuf::new();
            for name in tail_rev.iter().rev() {
                tail.push(name);
            }
            return Ok((c, tail));
        }
        cursor = parent;
    }
}

fn join_logical(base: &Path, tail: &Path) -> PathBuf {
    if tail.as_os_str().is_empty() {
        base.to_path_buf()
    } else {
        base.join(tail)
    }
}

/// Walk the logical path — reject if any existing component within the root
/// is a symlink. Unix-only symlink check (Windows reparse points out of scope).
fn reject_symlink_traversal(logical: &Path, root: &Path) -> Result<(), PathError> {
    let mut cursor = PathBuf::new();
    for comp in logical.components() {
        cursor.push(comp.as_os_str());
        if !cursor.starts_with(root) {
            continue;
        }
        match std::fs::symlink_metadata(&cursor) {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(PathError::SymlinkBlocked(cursor));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(e) => return Err(PathError::Io(e)),
        }
    }
    Ok(())
}

fn path_io(ctx: &str, path: &Path, source: io::Error) -> PathError {
    PathError::Io(io::Error::new(
        source.kind(),
        format!("{ctx} {}: {source}", path.display()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn containment_rejects_parent_escape() {
        let dir = tempdir().unwrap();
        let err = canonicalize_within(dir.path(), Path::new("../escape.txt")).unwrap_err();
        assert!(matches!(err, PathError::Escapes(_) | PathError::Io(_)));
    }

    #[test]
    fn existing_file_resolves() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("a.txt");
        std::fs::write(&f, "x").unwrap();
        let out = canonicalize_within(dir.path(), Path::new("a.txt")).unwrap();
        assert_eq!(out, f.canonicalize().unwrap());
    }

    #[test]
    fn nonexistent_tail_ok_for_write() {
        let dir = tempdir().unwrap();
        let out = canonicalize_within(dir.path(), Path::new("newfile.txt")).unwrap();
        assert_eq!(out, dir.path().canonicalize().unwrap().join("newfile.txt"));
    }

    #[test]
    fn deny_list_catches_proc() {
        // Synthetic path — don't hit real /proc on macOS.
        let err = check_deny_list(Path::new("/proc/self/mem")).unwrap_err();
        assert!(matches!(err, PathError::Denied(_)));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_in_path_rejected() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("real.txt");
        std::fs::write(&target, "x").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = canonicalize_within(dir.path(), Path::new("link.txt")).unwrap_err();
        assert!(matches!(err, PathError::SymlinkBlocked(_)));
    }
}
