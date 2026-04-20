//! `Rollback` tool — revert every staged path from the current tx. PLAN §3.2.
//!
//! No inputs. The tool finds the `TxHandle` on `ctx.tx`, calls `rollback`,
//! and renders a per-path report. After the call the tx becomes empty but
//! stays open — subsequent Edits re-stage into the same revert point. This
//! is what makes the "single revert point" semantics usable across retries:
//! a failed refactor can be reverted, the model can try again, and a second
//! Rollback still gets back to the original.

use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde_json::Value;

use crate::common::fence_tool_output;

#[derive(Debug, Default)]
pub struct RollbackTool;

#[async_trait]
impl Tool for RollbackTool {
    fn name(&self) -> &str {
        "Rollback"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "description": "Revert every file touched by Edit/Write since the session started (or since the last Rollback). Use after a multi-file refactor fails verification — restores originals in one pass and deletes files that didn't exist pre-refactor. Idempotent: a second call after a successful Rollback is a no-op.",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn preview(&self, _input: &Value) -> Preview {
        let count = ctx_staged_hint();
        Preview {
            summary_line: format!("Rollback ({count})"),
            detail: None,
        }
    }

    async fn call(&self, _input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let tx = ctx.tx.as_ref().ok_or_else(|| {
            ToolError::Validation(
                "Rollback: no transaction wired — this binary ran without session staging".into(),
            )
        })?;

        let staged_before = tx.staged_count();
        if staged_before == 0 {
            return Ok(ToolOutput {
                summary: fence_tool_output(
                    "Rollback",
                    None,
                    "No staged edits — nothing to revert.\n",
                ),
                detail_path: None,
                stream: None,
            });
        }

        let report = tx
            .rollback()
            .await
            .map_err(|e| ToolError::Other(format!("rollback failed: {e}")))?;

        let mut body = String::new();
        use std::fmt::Write;
        let _ = writeln!(body, "ROLLBACK of {staged_before} staged path(s)");
        let _ = writeln!(
            body,
            "  restored={} deleted={} failures={}",
            report.restored.len(),
            report.deleted.len(),
            report.failures.len()
        );
        if !report.restored.is_empty() {
            let _ = writeln!(body, "--- restored ---");
            for p in &report.restored {
                let _ = writeln!(body, "  {}", p.display());
            }
        }
        if !report.deleted.is_empty() {
            let _ = writeln!(body, "--- deleted ---");
            for p in &report.deleted {
                let _ = writeln!(body, "  {}", p.display());
            }
        }
        if !report.failures.is_empty() {
            let _ = writeln!(body, "--- failures ---");
            for (p, err) in &report.failures {
                let _ = writeln!(body, "  {}: {err}", p.display());
            }
        }
        Ok(ToolOutput {
            summary: fence_tool_output("Rollback", None, &body),
            detail_path: None,
            stream: None,
        })
    }
}

fn ctx_staged_hint() -> &'static str {
    // Preview doesn't see `ctx` in the current Tool trait — a per-call count
    // would need schema changes. Static label is fine; the tool_result
    // reports the exact number.
    "tx"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx::Transaction;
    use harness_core::tx::TxHandle;
    use harness_core::HookDispatcher;
    use harness_perm::PermissionSnapshot;
    use harness_proto::SessionId;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn ctx_with_tx(cwd: &Path, tx: Option<Arc<dyn harness_core::tx::TxHandle>>) -> ToolCtx {
        ToolCtx {
            cwd: cwd.to_path_buf(),
            session_id: SessionId::new("t"),
            cancel: CancellationToken::new(),
            permission: PermissionSnapshot::default(),
            hooks: HookDispatcher::default(),
            subagent: None,
            depth: 0,
            tx,
        }
    }

    #[tokio::test]
    async fn rollback_without_tx_is_validation_error() {
        let dir = tempdir().unwrap();
        let err = RollbackTool
            .call(serde_json::json!({}), ctx_with_tx(dir.path(), None))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn rollback_with_empty_tx_reports_nothing_to_revert() {
        let dir = tempdir().unwrap();
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();
        let out = RollbackTool
            .call(
                serde_json::json!({}),
                ctx_with_tx(dir.path(), Some(tx.as_handle())),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("nothing to revert"));
        assert!(out
            .summary
            .contains("<untrusted_tool_output tool=\"Rollback\""));
    }

    #[tokio::test]
    async fn rollback_restores_staged_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.txt");
        tokio::fs::write(&file, b"before").await.unwrap();
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();
        tx.stage(&file).await.unwrap();
        tokio::fs::write(&file, b"after").await.unwrap();

        let out = RollbackTool
            .call(
                serde_json::json!({}),
                ctx_with_tx(dir.path(), Some(tx.as_handle())),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("restored=1"));
        assert_eq!(tokio::fs::read(&file).await.unwrap(), b"before");
    }

    #[tokio::test]
    async fn rollback_reports_counts_in_summary() {
        let dir = tempdir().unwrap();
        // one existing, one created
        let existing = dir.path().join("exists.txt");
        let created = dir.path().join("new.txt");
        tokio::fs::write(&existing, b"orig").await.unwrap();
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();
        tx.stage(&existing).await.unwrap();
        tx.stage(&created).await.unwrap(); // tombstone
        tokio::fs::write(&existing, b"changed").await.unwrap();
        tokio::fs::write(&created, b"new body").await.unwrap();

        let out = RollbackTool
            .call(
                serde_json::json!({}),
                ctx_with_tx(dir.path(), Some(tx.as_handle())),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("restored=1"));
        assert!(out.summary.contains("deleted=1"));
        assert_eq!(tokio::fs::read(&existing).await.unwrap(), b"orig");
        assert!(!created.exists());
    }
}
