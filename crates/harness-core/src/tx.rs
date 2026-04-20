//! Transactional rollback contracts. PLAN §3.2.
//!
//! A **transaction** lets the agent stage the pre-write state of every file
//! it touches so a multi-file refactor can be reverted from a *single* revert
//! point when Tests fail. The actual staging-dir bookkeeping lives in
//! `harness-tools::tx::Transaction`; this module is the trait both sides
//! agree on so `Edit`/`Write`/`Rollback` can call through `ToolCtx` without
//! pulling `harness-tools` into the kernel.
//!
//! Shape mirrors `subagent::SubagentHost`: trait in `harness-core`, concrete
//! implementation in a higher crate, `OptTx = Option<Arc<dyn TxHandle>>` on
//! `ToolCtx` so tests and no-tx binaries can leave it `None`.
//!
//! Invariants the `stage` implementer MUST preserve:
//!   - `stage(path)` is idempotent: the FIRST call per `path` captures the
//!     on-disk state, later calls are no-ops. This is what gives us
//!     "revert-to-original" instead of "revert-to-last-known".
//!   - Missing files are recorded as tombstones so `rollback` deletes any
//!     file that was created during the transaction.
//!   - Rollback is best-effort: per-path failures are collected, not fatal.
//!     A single corrupted backup should not strand the rest of the refactor.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum TxError {
    #[error("transaction I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("path outside transaction base: {0}")]
    PathEscape(String),
    #[error("transaction already committed")]
    Closed,
}

/// Per-path outcome of a rollback. Intentionally loose — the `Rollback` tool
/// formats this into a human-readable tool_result; no caller programmatically
/// depends on the enum shape.
#[derive(Debug, Clone, Default)]
pub struct RollbackReport {
    /// Files whose pre-tx contents were restored.
    pub restored: Vec<PathBuf>,
    /// Files that did not exist before the tx and were deleted on rollback.
    pub deleted: Vec<PathBuf>,
    /// Paths where rollback itself failed (I/O errors, etc.).
    pub failures: Vec<(PathBuf, String)>,
}

#[async_trait]
pub trait TxHandle: Send + Sync + std::fmt::Debug {
    /// Snapshot the current on-disk state of `real_path` into the transaction
    /// backing store if not already captured. Idempotent.
    ///
    /// `real_path` MUST be absolute (callers canonicalize before calling).
    /// If the file does not exist, the implementer records a tombstone so
    /// rollback deletes any subsequently-created file at that path.
    async fn stage(&self, real_path: &Path) -> Result<(), TxError>;

    /// Restore every staged path. Files that exist pre-tx are overwritten
    /// with their original contents; tombstoned paths are deleted.
    /// Per-path failures are reported in the return value; the tx itself
    /// is considered "reset to empty" after this call regardless.
    async fn rollback(&self) -> Result<RollbackReport, TxError>;

    /// Discard staged state without restoring. After commit, subsequent
    /// `stage` / `rollback` calls return `TxError::Closed`.
    async fn commit(&self) -> Result<(), TxError>;

    /// Number of paths currently staged. Used by the `Rollback` tool to
    /// render an accurate preview line.
    fn staged_count(&self) -> usize;
}

pub type OptTx = Option<Arc<dyn TxHandle>>;
