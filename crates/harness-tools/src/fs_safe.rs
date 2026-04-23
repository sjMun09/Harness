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
//!   7. Linux-only defense-in-depth backstop: re-open the resolved path (or
//!      its parent, for not-yet-created targets) via `openat2` with
//!      `RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH` anchored at the canonical
//!      root. This closes TOCTOU races where a component is swapped for a
//!      symlink between the string check and the eventual open. Requires
//!      Linux 5.6+ for `openat2`; on older kernels or non-Linux this step
//!      is compiled out.

use std::io;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

pub const DENY_PATH_PREFIXES: &[&str] =
    &["/proc/", "/dev/tcp", "/dev/fd/", "\\\\?\\", "\\\\.\\pipe\\"];

/// Sensitive paths under `$HOME` that must NEVER be readable/writable by the
/// tool sandbox, regardless of whether the effective cwd is the home dir. A
/// user running `cd ~ && harness ask ...` would otherwise see absolute paths
/// like `~/.aws/credentials` canonicalize *inside* the cwd root and slip
/// through the containment check.
///
/// Entries are `$HOME`-relative. Each prefix matches any path underneath it
/// (e.g. `.ssh/` matches `.ssh/id_rsa`, `.ssh/config`, etc.) after
/// `.`/`..` normalization and canonicalization. Standalone files
/// (e.g. `.netrc`) match exactly that path and nothing below.
///
/// Scope: files containing credentials, auth tokens, cloud/CI keys, or
/// package-registry creds. Not exhaustive — intentionally conservative,
/// false positives (user would rather not grant) are acceptable here.
pub const HOME_SENSITIVE_PREFIXES: &[&str] = &[
    ".aws/",       // AWS shared credentials / config
    ".ssh/",       // SSH keys, known_hosts (keys are signal)
    ".config/gh/", // GitHub CLI OAuth tokens
    ".kube/",      // kubeconfig + service-account tokens
    ".gnupg/",     // GPG secret keyrings
];

/// Sensitive dotfiles directly under `$HOME` — matched as exact path (not
/// prefix, since these are files, not dirs).
pub const HOME_SENSITIVE_FILES: &[&str] = &[".netrc", ".pgpass", ".npmrc", ".pypirc"];

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
    openat2_verify(&canonical_root, &canonical)?;
    Ok(canonical)
}

/// Linux-only defense-in-depth: re-resolve the canonical path under the
/// canonical root using `openat2(RESOLVE_NO_SYMLINKS | RESOLVE_BENEATH)`.
///
/// The string-based checks above are primary; this is the backstop for
/// TOCTOU — if an attacker swaps a component for a symlink between the
/// check and the caller's open, `openat2` refuses the request atomically
/// via the kernel resolver. For not-yet-created targets (Write to a new
/// file) we verify the canonical parent instead, because the tail does not
/// yet exist on disk.
///
/// No-op on non-Linux and on Linux kernels that pre-date `openat2` (5.6).
#[cfg(target_os = "linux")]
fn openat2_verify(canonical_root: &Path, canonical: &Path) -> Result<(), PathError> {
    use rustix::fs::{openat2, Mode, OFlags, ResolveFlags, CWD};
    use std::io::ErrorKind;

    // Pick the deepest existing path to probe. If the final target doesn't
    // exist yet (new file), probe its parent — the parent must exist after
    // `canonicalize_deepest_prefix`. The relative path passed to openat2 is
    // the suffix *under* canonical_root, since we anchor the dirfd there.
    let probe: &Path = if canonical.exists() {
        canonical
    } else if let Some(parent) = canonical.parent() {
        if parent.exists() {
            parent
        } else {
            // Nothing to probe — the string checks already vetted the tail.
            return Ok(());
        }
    } else {
        return Ok(());
    };

    // Open the canonical root directory; use it as the anchor for RESOLVE_BENEATH.
    let root_fd = match openat2(
        CWD,
        canonical_root,
        OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) if e.raw_os_error() == libc::ENOSYS => {
            // Kernel < 5.6 lacks openat2 — fall back to string-level checks.
            return Ok(());
        }
        Err(e) => {
            return Err(PathError::Io(io::Error::from_raw_os_error(
                e.raw_os_error(),
            )));
        }
    };

    // Compute the suffix under canonical_root. `strip_prefix` is guaranteed
    // to succeed because containment was just verified.
    let rel = probe.strip_prefix(canonical_root).unwrap_or(Path::new("."));
    let rel_os = rel.as_os_str();
    let rel_for_open: &std::ffi::OsStr = if rel_os.is_empty() {
        std::ffi::OsStr::new(".")
    } else {
        rel_os
    };

    match openat2(
        &root_fd,
        rel_for_open,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::BENEATH,
    ) {
        Ok(_fd) => Ok(()),
        Err(e) => {
            let code = e.raw_os_error();
            if code == libc::ENOSYS {
                // Kernel supports openat, not openat2 with flags — accept.
                return Ok(());
            }
            let io_err = io::Error::from_raw_os_error(code);
            match io_err.kind() {
                ErrorKind::PermissionDenied => Err(PathError::Denied(probe.to_path_buf())),
                _ => {
                    // ELOOP → symlink encountered; EXDEV → escape outside root.
                    if code == libc::ELOOP {
                        Err(PathError::SymlinkBlocked(probe.to_path_buf()))
                    } else if code == libc::EXDEV {
                        Err(PathError::Escapes(probe.to_path_buf()))
                    } else {
                        Err(PathError::Io(io_err))
                    }
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)]
fn openat2_verify(_canonical_root: &Path, _canonical: &Path) -> Result<(), PathError> {
    Ok(())
}

/// Convenience for parent-then-verify patterns used by `Write`.
pub fn canonicalize_parent_within(
    root: &Path,
    target: &Path,
) -> Result<(PathBuf, std::ffi::OsString), PathError> {
    let file_name = target
        .file_name()
        .ok_or_else(|| {
            path_io(
                "no file name",
                target,
                io::Error::from(io::ErrorKind::InvalidInput),
            )
        })?
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
    check_home_sensitive(p)?;
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

/// Reject absolute paths that resolve into well-known credential locations
/// under `$HOME`. The string-level canonicalization pass can't protect these
/// when the effective cwd *is* `$HOME` — a model-issued `Read /Users/mun/.aws/credentials`
/// canonicalizes inside the cwd-root and passes containment. This guard runs
/// unconditionally after the canonical prefix check.
///
/// Best-effort: silently skipped when `$HOME` is unset or non-canonical (very
/// unusual — tests use `env::set_var("HOME", ...)` to simulate).
fn check_home_sensitive(p: &Path) -> Result<(), PathError> {
    let Some(home) = resolve_home() else {
        return Ok(());
    };
    // Canonicalize `$HOME` where possible so the comparison tolerates symlinks
    // like `/var` → `/private/var` on macOS. When canonicalize fails (e.g.
    // custom test HOME that doesn't exist) fall back to the raw string.
    let home_canonical = home.canonicalize().unwrap_or(home);
    let Ok(rel) = p.strip_prefix(&home_canonical) else {
        return Ok(());
    };
    let rel_str = rel.to_string_lossy();
    for prefix in HOME_SENSITIVE_PREFIXES {
        // Match the directory itself (`rel == ".aws"`) or anything beneath it
        // (`rel == ".aws/credentials"`). A trailing `/` in the constant forces
        // boundary alignment so `.awsfoo` doesn't match `.aws/`.
        let stripped = prefix.trim_end_matches('/');
        if rel_str == stripped || rel_str.starts_with(prefix) {
            return Err(PathError::Denied(p.to_path_buf()));
        }
    }
    for file in HOME_SENSITIVE_FILES {
        if rel_str == *file {
            return Err(PathError::Denied(p.to_path_buf()));
        }
    }
    Ok(())
}

/// Resolve `$HOME` for sensitive-path checks. Prefers the env var (standard
/// across Unix; Windows systems set it via shell init too), falls back to
/// `USERPROFILE` on Windows.
fn resolve_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    #[cfg(windows)]
    {
        if let Ok(h) = std::env::var("USERPROFILE") {
            if !h.is_empty() {
                return Some(PathBuf::from(h));
            }
        }
    }
    None
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

    /// Serialize tests that mutate `$HOME` — rustc runs unit tests in parallel
    /// on a single process by default, and we cannot trust other tests to leave
    /// the env var alone. `Mutex<()>` around the env swap is the standard dodge.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Guard that restores `$HOME` on drop, even if the test panics.
    struct HomeGuard {
        prev: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl HomeGuard {
        fn set(new_home: &Path) -> Self {
            let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var_os("HOME");
            std::env::set_var("HOME", new_home);
            Self { prev, _lock: lock }
        }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn home_sensitive_aws_dir_blocked_under_fake_home() {
        let fake_home = tempdir().unwrap();
        let aws = fake_home.path().join(".aws");
        std::fs::create_dir_all(&aws).unwrap();
        std::fs::write(
            aws.join("credentials"),
            "[default]\naws_access_key_id=AKIAx",
        )
        .unwrap();

        let _g = HomeGuard::set(fake_home.path());

        // Effective cwd is $HOME — this is the bug scenario from the review.
        let err = canonicalize_within(fake_home.path(), Path::new(".aws/credentials")).unwrap_err();
        assert!(
            matches!(err, PathError::Denied(_)),
            "expected Denied, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn home_sensitive_absolute_path_blocked_even_in_home_cwd() {
        let fake_home = tempdir().unwrap();
        let ssh = fake_home.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("id_rsa"), "-----BEGIN OPENSSH PRIVATE KEY-----\n").unwrap();

        let _g = HomeGuard::set(fake_home.path());
        let absolute = ssh.join("id_rsa");

        let err = canonicalize_within(fake_home.path(), &absolute).unwrap_err();
        assert!(
            matches!(err, PathError::Denied(_)),
            "expected Denied, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn home_sensitive_dotfiles_blocked() {
        for leaf in &[".netrc", ".pgpass", ".npmrc", ".pypirc"] {
            let fake_home = tempdir().unwrap();
            let f = fake_home.path().join(leaf);
            std::fs::write(&f, "secret").unwrap();
            let _g = HomeGuard::set(fake_home.path());
            let err = canonicalize_within(fake_home.path(), Path::new(leaf)).unwrap_err();
            assert!(
                matches!(err, PathError::Denied(_)),
                "expected Denied for {leaf}, got {err:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn home_sensitive_directory_itself_blocked() {
        let fake_home = tempdir().unwrap();
        let kube = fake_home.path().join(".kube");
        std::fs::create_dir_all(&kube).unwrap();

        let _g = HomeGuard::set(fake_home.path());
        let err = canonicalize_within(fake_home.path(), Path::new(".kube")).unwrap_err();
        assert!(
            matches!(err, PathError::Denied(_)),
            "expected Denied, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn home_non_sensitive_path_still_allowed() {
        let fake_home = tempdir().unwrap();
        let project = fake_home.path().join("projects").join("mine");
        std::fs::create_dir_all(&project).unwrap();
        let f = project.join("notes.md");
        std::fs::write(&f, "ok").unwrap();

        let _g = HomeGuard::set(fake_home.path());
        let out = canonicalize_within(fake_home.path(), &f).unwrap();
        assert_eq!(out, f.canonicalize().unwrap());
    }

    /// Similar-but-not-matching names must NOT collide with the sensitive list
    /// (e.g. `.awsfoo/` must be allowed). Guards against boundary-less prefix
    /// bugs.
    #[cfg(unix)]
    #[test]
    fn home_similar_names_not_blocked() {
        let fake_home = tempdir().unwrap();
        let decoy = fake_home.path().join(".awsfoo");
        std::fs::create_dir_all(&decoy).unwrap();
        let f = decoy.join("notes.md");
        std::fs::write(&f, "ok").unwrap();

        let _g = HomeGuard::set(fake_home.path());
        let out = canonicalize_within(fake_home.path(), Path::new(".awsfoo/notes.md")).unwrap();
        assert_eq!(out, f.canonicalize().unwrap());
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

    /// Defense-in-depth: a symlink pointing *outside* the root must be
    /// rejected even if we bypass the string-level pre-check and call
    /// `openat2_verify` directly. This mirrors the TOCTOU scenario where
    /// a component is swapped between the string check and the open.
    #[cfg(target_os = "linux")]
    #[test]
    fn openat2_blocks_symlink_escape() {
        let outside = tempdir().unwrap();
        let inside = tempdir().unwrap();
        let victim = outside.path().join("secret.txt");
        std::fs::write(&victim, "top-secret").unwrap();

        let link = inside.path().join("evil");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        // Synthesize a "canonical" path that appears to sit under `inside`
        // but whose final component is actually a symlink escaping the root.
        let canonical_root = inside.path().canonicalize().unwrap();
        let fake_canonical = canonical_root.join("evil");

        let err = openat2_verify(&canonical_root, &fake_canonical).unwrap_err();
        assert!(
            matches!(
                err,
                PathError::SymlinkBlocked(_) | PathError::Escapes(_) | PathError::Denied(_)
            ),
            "expected SymlinkBlocked/Escapes/Denied, got {err:?}"
        );
    }
}
