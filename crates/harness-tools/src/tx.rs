//! `Transaction` — staging-dir backed impl of `harness_core::tx::TxHandle`.
//! PLAN §3.2.
//!
//! Layout on disk (all paths relative to `ctx.cwd`):
//! ```text
//!   .harness/transactions/<tx_id>/
//!       original/<repo-relative path>   (bytes before first stage)
//!       tombstone/<repo-relative path>  (empty marker; path did not exist)
//! ```
//!
//! Why shadow-backup instead of write-redirection: the agent's next Test run
//! executes against the real on-disk state, so new edits must land on the
//! actual files. The "staging directory" in PLAN §3.2 is the undo side, not
//! a copy-on-write overlay.
//!
//! Idempotent staging = first-write-wins for originals. If a turn edits
//! `foo.xml` twice, the first stage captures the pre-refactor state and the
//! second stage is a no-op. This is what makes a single revert point correct.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use harness_core::tx::{OptTx, RollbackReport, TxError, TxHandle};
use std::sync::Arc;

const ORIGINAL_SUBDIR: &str = "original";
const TOMBSTONE_SUBDIR: &str = "tombstone";

#[derive(Debug, Clone, Copy)]
enum EntryKind {
    /// Real file existed pre-tx; backup is at `original/<rel>`.
    Original,
    /// Path did not exist pre-tx; rollback = delete if created.
    Tombstone,
}

#[derive(Debug)]
pub struct Transaction {
    base: PathBuf,
    staging: PathBuf,
    tx_id: String,
    entries: Mutex<HashMap<PathBuf, EntryKind>>,
    closed: Mutex<bool>,
}

impl Transaction {
    /// Open a fresh transaction rooted at `base` (typically `ctx.cwd`).
    ///
    /// Staging dir is created eagerly so `stage` can copy into it without
    /// having to first-time initialize. The `tx_id` is a monotonic wall-clock
    /// nanosecond + pid so concurrent harness runs against the same cwd don't
    /// collide.
    pub async fn open(base: PathBuf) -> Result<Arc<Self>, TxError> {
        // Canonicalize so `relative_to_base` sees the same prefix that
        // Edit/Write's `canonicalize_within` produces. Without this, macOS
        // symlinks like `/var/folders` → `/private/var/folders` break the
        // strip_prefix check for cwd-relative writes.
        let base = tokio::fs::canonicalize(&base).await.unwrap_or(base);
        let tx_id = {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{ts:x}-{}", std::process::id())
        };
        let staging = base.join(".harness").join("transactions").join(&tx_id);
        tokio::fs::create_dir_all(staging.join(ORIGINAL_SUBDIR)).await?;
        tokio::fs::create_dir_all(staging.join(TOMBSTONE_SUBDIR)).await?;
        Ok(Arc::new(Self {
            base,
            staging,
            tx_id,
            entries: Mutex::new(HashMap::new()),
            closed: Mutex::new(false),
        }))
    }

    pub fn as_handle(self: &Arc<Self>) -> Arc<dyn TxHandle> {
        self.clone()
    }

    #[must_use]
    pub fn tx_id(&self) -> &str {
        &self.tx_id
    }

    #[must_use]
    pub fn staging_dir(&self) -> &Path {
        &self.staging
    }

    async fn relative_to_base(&self, real: &Path) -> Result<PathBuf, TxError> {
        // Fast path: caller already produced a canonical path (Edit/Write do).
        if let Ok(rel) = real.strip_prefix(&self.base) {
            return check_rel(rel, real);
        }
        // Slow path: caller passed a symlink-form path (e.g. /var/folders
        // on macOS before the /private prefix is resolved). Canonicalize
        // either the path itself or, for tombstones, its parent.
        let canonical = match tokio::fs::canonicalize(real).await {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let parent = real
                    .parent()
                    .ok_or_else(|| TxError::PathEscape(real.display().to_string()))?;
                let name = real
                    .file_name()
                    .ok_or_else(|| TxError::PathEscape(real.display().to_string()))?;
                let canon_parent = tokio::fs::canonicalize(parent).await?;
                canon_parent.join(name)
            }
            Err(e) => return Err(TxError::Io(e)),
        };
        let rel = canonical
            .strip_prefix(&self.base)
            .map_err(|_| TxError::PathEscape(real.display().to_string()))?;
        check_rel(rel, real)
    }

    fn check_open(&self) -> Result<(), TxError> {
        if *self.closed.lock().expect("tx closed mutex poisoned") {
            return Err(TxError::Closed);
        }
        Ok(())
    }
}

#[async_trait]
impl TxHandle for Transaction {
    async fn stage(&self, real_path: &Path) -> Result<(), TxError> {
        self.check_open()?;
        let rel = self.relative_to_base(real_path).await?;
        // Early-exit if already staged — this is what makes the tx idempotent.
        {
            let entries = self.entries.lock().expect("tx entries mutex poisoned");
            if entries.contains_key(&rel) {
                return Ok(());
            }
        }
        match tokio::fs::metadata(real_path).await {
            Ok(meta) if meta.is_file() => {
                let dst = self.staging.join(ORIGINAL_SUBDIR).join(&rel);
                if let Some(parent) = dst.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::copy(real_path, &dst).await?;
                self.entries
                    .lock()
                    .expect("tx entries mutex poisoned")
                    .insert(rel, EntryKind::Original);
                Ok(())
            }
            Ok(_) => {
                // Directory or other non-file — out of scope for this tx; treat as tombstone.
                let marker = self.staging.join(TOMBSTONE_SUBDIR).join(&rel);
                if let Some(parent) = marker.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&marker, b"").await?;
                self.entries
                    .lock()
                    .expect("tx entries mutex poisoned")
                    .insert(rel, EntryKind::Tombstone);
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let marker = self.staging.join(TOMBSTONE_SUBDIR).join(&rel);
                if let Some(parent) = marker.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&marker, b"").await?;
                self.entries
                    .lock()
                    .expect("tx entries mutex poisoned")
                    .insert(rel, EntryKind::Tombstone);
                Ok(())
            }
            Err(e) => Err(TxError::Io(e)),
        }
    }

    async fn rollback(&self) -> Result<RollbackReport, TxError> {
        self.check_open()?;
        let snapshot: Vec<(PathBuf, EntryKind)> = {
            let mut guard = self.entries.lock().expect("tx entries mutex poisoned");
            let out = guard.iter().map(|(k, v)| (k.clone(), *v)).collect();
            guard.clear();
            out
        };
        let mut report = RollbackReport::default();
        for (rel, kind) in snapshot {
            let real = self.base.join(&rel);
            match kind {
                EntryKind::Original => {
                    let src = self.staging.join(ORIGINAL_SUBDIR).join(&rel);
                    if let Err(e) = restore_file(&src, &real).await {
                        report.failures.push((real, e.to_string()));
                    } else {
                        report.restored.push(real);
                    }
                }
                EntryKind::Tombstone => match tokio::fs::remove_file(&real).await {
                    Ok(()) => report.deleted.push(real),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // Already gone — still a valid "deleted" outcome.
                        report.deleted.push(real);
                    }
                    Err(e) => report.failures.push((real, e.to_string())),
                },
            }
        }
        Ok(report)
    }

    async fn commit(&self) -> Result<(), TxError> {
        {
            let mut closed = self.closed.lock().expect("tx closed mutex poisoned");
            if *closed {
                return Err(TxError::Closed);
            }
            *closed = true;
        }
        self.entries
            .lock()
            .expect("tx entries mutex poisoned")
            .clear();
        // Best-effort cleanup. If the staging dir is gone or partially corrupt,
        // don't surface an error — commit's job is purely to drop state.
        let _ = tokio::fs::remove_dir_all(&self.staging).await;
        Ok(())
    }

    fn staged_count(&self) -> usize {
        self.entries
            .lock()
            .expect("tx entries mutex poisoned")
            .len()
    }
}

fn check_rel(rel: &Path, original: &Path) -> Result<PathBuf, TxError> {
    for comp in rel.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => return Err(TxError::PathEscape(original.display().to_string())),
        }
    }
    Ok(rel.to_path_buf())
}

async fn restore_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Atomic overwrite via rename from a sibling tempfile. This preserves
    // the property that a concurrent Test reading `dst` never sees a
    // half-written file during rollback.
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(
        ".{name}.harness.rollback.{}.tmp",
        std::process::id()
    ));
    tokio::fs::copy(src, &tmp).await?;
    if let Err(e) = tokio::fs::rename(&tmp, dst).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

/// Convenience: call `tx.stage(path)` only if a tx is wired. Ergonomic
/// helper for `Edit`/`Write` which both need the same prelude.
pub(crate) async fn stage_if_present(tx: &OptTx, real: &Path) -> Result<(), TxError> {
    if let Some(handle) = tx.as_ref() {
        handle.stage(real).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::tx::TxHandle;
    use tempfile::tempdir;

    #[tokio::test]
    async fn stage_backs_up_original_bytes() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.txt");
        tokio::fs::write(&file, b"before").await.unwrap();
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();
        tx.stage(&file).await.unwrap();
        tokio::fs::write(&file, b"after").await.unwrap();

        let backup = tx.staging.join(ORIGINAL_SUBDIR).join("a.txt");
        assert!(backup.exists());
        assert_eq!(tokio::fs::read(&backup).await.unwrap(), b"before");
    }

    #[tokio::test]
    async fn stage_is_idempotent_first_wins() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.txt");
        tokio::fs::write(&file, b"state-1").await.unwrap();
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();
        tx.stage(&file).await.unwrap();
        tokio::fs::write(&file, b"state-2").await.unwrap();
        tx.stage(&file).await.unwrap(); // no-op; captures state-1, not state-2
        let backup = tx.staging.join(ORIGINAL_SUBDIR).join("a.txt");
        assert_eq!(tokio::fs::read(&backup).await.unwrap(), b"state-1");
    }

    #[tokio::test]
    async fn rollback_restores_original_bytes() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.txt");
        tokio::fs::write(&file, b"before").await.unwrap();
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();
        tx.stage(&file).await.unwrap();
        tokio::fs::write(&file, b"after").await.unwrap();
        let report = tx.rollback().await.unwrap();
        assert_eq!(report.restored.len(), 1);
        assert_eq!(report.deleted.len(), 0);
        assert_eq!(report.failures.len(), 0);
        assert_eq!(tokio::fs::read(&file).await.unwrap(), b"before");
    }

    #[tokio::test]
    async fn rollback_deletes_created_files() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("new.txt");
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();
        tx.stage(&file).await.unwrap(); // stages tombstone
        tokio::fs::write(&file, b"created").await.unwrap();
        let report = tx.rollback().await.unwrap();
        assert_eq!(report.deleted.len(), 1);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn rollback_restores_multiple_files_in_one_pass() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), b"a1")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.txt"), b"b1")
            .await
            .unwrap();
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();
        tx.stage(&dir.path().join("a.txt")).await.unwrap();
        tx.stage(&dir.path().join("b.txt")).await.unwrap();
        tokio::fs::write(dir.path().join("a.txt"), b"a2")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.txt"), b"b2")
            .await
            .unwrap();
        let report = tx.rollback().await.unwrap();
        assert_eq!(report.restored.len(), 2);
        assert_eq!(
            tokio::fs::read(dir.path().join("a.txt")).await.unwrap(),
            b"a1"
        );
        assert_eq!(
            tokio::fs::read(dir.path().join("b.txt")).await.unwrap(),
            b"b1"
        );
    }

    #[tokio::test]
    async fn rollback_clears_staged_entries() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.txt");
        tokio::fs::write(&file, b"x").await.unwrap();
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();
        tx.stage(&file).await.unwrap();
        assert_eq!(tx.staged_count(), 1);
        tx.rollback().await.unwrap();
        assert_eq!(tx.staged_count(), 0);
    }

    #[tokio::test]
    async fn commit_makes_rollback_fail_with_closed() {
        let dir = tempdir().unwrap();
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();
        tx.commit().await.unwrap();
        let err = tx.rollback().await.unwrap_err();
        assert!(matches!(err, TxError::Closed));
    }

    #[tokio::test]
    async fn commit_removes_staging_dir() {
        let dir = tempdir().unwrap();
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();
        let staging = tx.staging.clone();
        assert!(staging.exists());
        tx.commit().await.unwrap();
        assert!(!staging.exists());
    }

    #[tokio::test]
    async fn stage_rejects_path_outside_base() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();
        let err = tx.stage(&outside.path().join("a.txt")).await.unwrap_err();
        assert!(matches!(err, TxError::PathEscape(_)));
    }

    #[tokio::test]
    async fn stage_preserves_nested_directory_structure() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("sub").join("nested").join("f.txt");
        tokio::fs::create_dir_all(nested.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&nested, b"deep").await.unwrap();
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();
        tx.stage(&nested).await.unwrap();
        let backup = tx
            .staging
            .join(ORIGINAL_SUBDIR)
            .join("sub")
            .join("nested")
            .join("f.txt");
        assert!(backup.exists());
        assert_eq!(tokio::fs::read(&backup).await.unwrap(), b"deep");
    }
}
