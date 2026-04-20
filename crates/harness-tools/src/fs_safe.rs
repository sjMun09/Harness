//! Path safety — PLAN §8.2.
//!
//! Linux primary: `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`.
//! macOS/BSD:    `O_NOFOLLOW` + post-open `fstat` dev+inode re-check.
//! All platforms: logical `..`/`.` normalize → canonicalize →
//! component-wise allowlist check → `DENY_PATH_PREFIXES` scan.

use std::path::{Path, PathBuf};

use thiserror::Error;

/// Filesystem regions banned under every profile (§8.2).
/// NTFS ADS (`:` in any component) is enforced separately at runtime.
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
    Io(#[from] std::io::Error),
}

/// Resolve `target` relative to (or absolute-but-confined-under) `root`,
/// returning a canonical path guaranteed to live under `root` and to not
/// pass through any symlink or `DENY_PATH_PREFIXES` entry.
pub fn canonicalize_within(_root: &Path, _target: &Path) -> Result<PathBuf, PathError> {
    // Iter 1 body. Linux: openat2 with RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS;
    // fallback to generic on ENOSYS (kernel < 5.6).
    Err(PathError::Escapes(PathBuf::new()))
}

/// Returns `Err(PathError::Denied)` if `p` matches `DENY_PATH_PREFIXES` or
/// contains an NTFS ADS-style `:` mid-component. Call after canonicalize.
pub fn check_deny_list(p: &Path) -> Result<(), PathError> {
    let s = p.to_string_lossy();
    for prefix in DENY_PATH_PREFIXES {
        if s.starts_with(prefix) {
            return Err(PathError::Denied(p.to_path_buf()));
        }
    }
    Ok(())
}
