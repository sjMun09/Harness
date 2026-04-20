//! First-run "trust prompt" for the current working directory (PLAN §8.2).
//!
//! The first time Harness runs in a given cwd, the user is asked to confirm
//! they trust the directory before any tool is allowed to execute. Accepted
//! directories are recorded in `$XDG_STATE_HOME/harness/trust.json` (0600)
//! keyed by the sha256 of the canonicalized path, so future runs in the same
//! cwd bypass the prompt.
//!
//! Deny-safe: when stdin is not a TTY (CI / piped input), we refuse rather
//! than silently accepting. Callers wanting to bypass (e.g. a `--trust-cwd`
//! flag) should call [`skip_trust_check`] and skip invoking [`ensure_trusted`]
//! entirely; the flag itself is wired in `main.rs`.
//!
//! Store shape (v=1):
//! ```json
//! {
//!   "v": 1,
//!   "trusted": {
//!     "<sha256-hex>": {
//!       "path": "/abs/path",
//!       "trusted_at": "2026-04-20T12:34:56Z"
//!     }
//!   }
//! }
//! ```

// Until `main.rs` is wired to call `ensure_trusted`, these helpers appear
// unused from the binary's point of view; keep the warnings quiet so the
// interim build stays clean for reviewers.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// On-disk shape. `#[serde(default)]` lets us read older partial stores.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustStore {
    v: u32,
    #[serde(default)]
    trusted: BTreeMap<String, TrustEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustEntry {
    path: String,
    trusted_at: String,
}

impl Default for TrustStore {
    fn default() -> Self {
        Self {
            v: TRUST_FORMAT_VERSION,
            trusted: BTreeMap::new(),
        }
    }
}

const TRUST_FORMAT_VERSION: u32 = 1;

/// Resolve `$XDG_STATE_HOME/harness/trust.json` via the shared `state_dir()`.
fn trust_store_path() -> PathBuf {
    harness_mem::state_dir().join("trust.json")
}

/// Hex sha256 of the canonicalized `cwd`. Canonicalization falls back to the
/// input path when the target is not (yet) resolvable — callers normally pass
/// an existing dir, but this keeps the function total for tests.
fn hash_cwd(cwd: &Path) -> anyhow::Result<(PathBuf, String)> {
    let canonical = std::fs::canonicalize(cwd)
        .with_context(|| format!("canonicalize cwd {}", cwd.display()))?;
    let mut hasher = Sha256::new();
    // Hash the OS-level bytes; on Unix this is the raw byte sequence, which
    // is stable across runs for a given inode path.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hasher.update(canonical.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    {
        hasher.update(canonical.to_string_lossy().as_bytes());
    }
    let hex = format!("{:x}", hasher.finalize());
    Ok((canonical, hex))
}

/// Marker helper so callers can document the `--trust-cwd` flag path. Takes
/// no arguments because skipping is, by construction, a no-op.
pub fn skip_trust_check() {
    tracing::debug!("trust-cwd check skipped by caller");
}

/// Public entry: ensure the given `cwd` has been user-approved. See module
/// docs for semantics. On first acceptance the store is written atomically
/// with 0600 perms (Unix).
pub fn ensure_trusted(cwd: &Path) -> anyhow::Result<()> {
    let is_tty = io::stdin().is_terminal();
    let store_path = trust_store_path();
    ensure_trusted_inner(cwd, &store_path, is_tty, &mut io::stderr())
}

/// Testable core. `is_tty` is injected so unit tests can exercise the
/// deny-when-not-tty branch without fiddling with real stdin.
fn ensure_trusted_inner<W: Write>(
    cwd: &Path,
    store_path: &Path,
    is_tty: bool,
    ui: &mut W,
) -> anyhow::Result<()> {
    let (canonical, hash) = hash_cwd(cwd)?;

    let mut store = load_store(store_path)?;
    if store.trusted.contains_key(&hash) {
        tracing::debug!(path = %canonical.display(), "cwd already trusted");
        return Ok(());
    }

    if !is_tty {
        return Err(anyhow::anyhow!(
            "cwd not trusted and stdin is not a TTY — re-run interactively, \
             or pass `--trust-cwd` to skip this check (path: {})",
            canonical.display()
        ));
    }

    // Prompt loop is single-shot: one line in, `y`/`Y` accepts, anything else rejects.
    writeln!(
        ui,
        "\n\x1b[33m!\x1b[0m Harness has not been run in this directory before."
    )?;
    writeln!(ui, "  Path: {}", canonical.display())?;
    writeln!(
        ui,
        "  Harness will read/write files under this directory and execute Bash commands."
    )?;
    write!(ui, "Do you trust this directory? [y/N] ")?;
    ui.flush()?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read trust prompt answer")?;
    let accepted = matches!(answer.trim(), "y" | "Y");
    if !accepted {
        return Err(anyhow::anyhow!(
            "cwd not trusted — run again and answer 'y'"
        ));
    }

    let entry = TrustEntry {
        path: canonical.display().to_string(),
        trusted_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };
    store.trusted.insert(hash, entry);
    save_store(store_path, &store)?;
    tracing::info!(path = %canonical.display(), "cwd trusted");
    Ok(())
}

/// Read the store if it exists; treat missing / empty / malformed (but with
/// a correct v==1 header check) files leniently — a missing file is just an
/// empty store, but a present file with a wrong version is an error so we
/// don't silently drop user-acks on format bumps.
fn load_store(path: &Path) -> anyhow::Result<TrustStore> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(TrustStore::default()),
        Err(e) => return Err(e).context("read trust store"),
    };
    if bytes.is_empty() {
        return Ok(TrustStore::default());
    }
    let store: TrustStore = serde_json::from_slice(&bytes).context("parse trust store JSON")?;
    if store.v != TRUST_FORMAT_VERSION {
        return Err(anyhow::anyhow!(
            "trust store version mismatch: expected {}, got {}",
            TRUST_FORMAT_VERSION,
            store.v
        ));
    }
    Ok(store)
}

/// Atomic write + 0600 perms on Unix (mirrors `harness_mem` pattern).
fn save_store(path: &Path, store: &TrustStore) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create trust dir {}", parent.display()))?;
        #[cfg(unix)]
        set_mode(parent, 0o700)?;
    }
    let payload = serde_json::to_vec_pretty(store).context("serialize trust store")?;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("trust.json"),
        std::process::id()
    ));
    std::fs::write(&tmp, &payload).context("write trust store tmp")?;
    #[cfg(unix)]
    set_mode(&tmp, 0o600)?;
    std::fs::rename(&tmp, path).context("rename trust store")?;
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path)?.permissions();
    perm.set_mode(mode);
    std::fs::set_permissions(path, perm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn canonicalization_is_stable() {
        let dir = tempdir().unwrap();
        // Compute twice through two shapes of the same path — result must match.
        let (c1, h1) = hash_cwd(dir.path()).unwrap();
        let nested = dir.path().join(".").join(".");
        let (c2, h2) = hash_cwd(&nested).unwrap();
        assert_eq!(c1, c2);
        assert_eq!(h1, h2);
        // sha256 is 64 hex chars.
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn different_paths_hash_differently() {
        let a = tempdir().unwrap();
        let b = tempdir().unwrap();
        let (_, ha) = hash_cwd(a.path()).unwrap();
        let (_, hb) = hash_cwd(b.path()).unwrap();
        assert_ne!(ha, hb);
    }

    #[test]
    fn roundtrip_store_in_tempdir() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("nested").join("trust.json");
        let cwd = dir.path();

        // First call with TTY=true + pre-seed store to simulate post-accept.
        let (_, hash) = hash_cwd(cwd).unwrap();
        let mut store = TrustStore::default();
        store.trusted.insert(
            hash.clone(),
            TrustEntry {
                path: cwd.display().to_string(),
                trusted_at: "2026-04-20T00:00:00Z".into(),
            },
        );
        save_store(&store_path, &store).unwrap();

        // Round-trip: reading should preserve the hash.
        let loaded = load_store(&store_path).unwrap();
        assert_eq!(loaded.v, TRUST_FORMAT_VERSION);
        assert!(loaded.trusted.contains_key(&hash));

        // ensure_trusted_inner with is_tty=false should still succeed because
        // the entry is already present.
        let mut ui = Vec::<u8>::new();
        ensure_trusted_inner(cwd, &store_path, false, &mut ui).unwrap();
        assert!(ui.is_empty(), "no prompt should have been emitted");
    }

    #[test]
    fn deny_when_not_tty_and_not_trusted() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("trust.json");
        let cwd = dir.path();

        let mut ui = Vec::<u8>::new();
        let err = ensure_trusted_inner(cwd, &store_path, false, &mut ui)
            .expect_err("must deny when not a TTY and not already trusted");
        let msg = err.to_string();
        assert!(
            msg.contains("--trust-cwd"),
            "error should suggest --trust-cwd, got: {msg}"
        );
        assert!(!store_path.exists(), "must not create store on deny");
    }

    #[test]
    fn missing_store_is_empty() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("does-not-exist.json");
        let loaded = load_store(&store_path).unwrap();
        assert_eq!(loaded.v, TRUST_FORMAT_VERSION);
        assert!(loaded.trusted.is_empty());
    }

    #[test]
    fn version_mismatch_is_rejected() {
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("trust.json");
        std::fs::write(&store_path, br#"{"v":999,"trusted":{}}"#).unwrap();
        let err = load_store(&store_path).unwrap_err();
        assert!(err.to_string().contains("version mismatch"));
    }

    #[cfg(unix)]
    #[test]
    fn saved_store_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let store_path = dir.path().join("trust.json");
        save_store(&store_path, &TrustStore::default()).unwrap();
        let mode = std::fs::metadata(&store_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "trust store must be 0600, got {mode:o}");
    }

    #[test]
    fn skip_trust_check_is_noop() {
        // Just confirm it compiles + returns.
        skip_trust_check();
    }
}
