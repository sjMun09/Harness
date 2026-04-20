//! `Edit` tool — exact string replace with unique-check + optional replace_all.
//! PLAN §3.1.

use std::path::Path;

use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use similar::{ChangeTag, TextDiff};
use tokio::io::AsyncWriteExt;

use crate::common::{head_tail, parse_input, HEAD_TAIL_CAP};
use crate::fs_safe::{canonicalize_within, PathError};
use crate::tx::stage_if_present;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditInput {
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Default)]
pub struct EditTool;

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path":   { "type": "string" },
                "old_string":  { "type": "string" },
                "new_string":  { "type": "string" },
                "replace_all": { "type": "boolean", "default": false }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<EditInput>(input.clone()) {
            Ok(ei) => Preview {
                summary_line: format!(
                    "Edit {} ({})",
                    ei.file_path,
                    if ei.replace_all { "all" } else { "unique" }
                ),
                detail: None,
            },
            Err(e) => Preview {
                summary_line: "Edit <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let ei: EditInput = parse_input(input, "Edit")?;
        if ei.old_string.is_empty() {
            return Err(ToolError::Validation("old_string must be non-empty".into()));
        }
        if ei.old_string == ei.new_string {
            return Err(ToolError::Validation(
                "old_string equals new_string — nothing to edit".into(),
            ));
        }

        let canonical = canonicalize_within(&ctx.cwd, Path::new(&ei.file_path))
            .map_err(path_error_to_tool_error)?;

        let original = tokio::fs::read_to_string(&canonical).await?;
        let occurrences = original.matches(&ei.old_string).count();

        let replaced = if ei.replace_all {
            if occurrences == 0 {
                return Err(ToolError::Validation("old_string not found in file".into()));
            }
            original.replace(&ei.old_string, &ei.new_string)
        } else {
            match occurrences {
                0 => return Err(ToolError::Validation("old_string not found in file".into())),
                1 => original.replacen(&ei.old_string, &ei.new_string, 1),
                n => {
                    return Err(ToolError::Validation(format!(
                        "old_string matches {n} times; pass replace_all=true or add more context"
                    )))
                }
            }
        };

        stage_if_present(&ctx.tx, &canonical)
            .await
            .map_err(|e| ToolError::Other(format!("tx stage failed: {e}")))?;
        write_atomic(&canonical, replaced.as_bytes()).await?;

        let diff = render_unified_diff(&original, &replaced, &canonical.display().to_string());
        let summary = format!(
            "edited {} ({} replacement{})\n{}",
            canonical.display(),
            if ei.replace_all { occurrences } else { 1 },
            if ei.replace_all && occurrences != 1 {
                "s"
            } else {
                ""
            },
            diff,
        );
        Ok(ToolOutput {
            summary: head_tail(&summary, HEAD_TAIL_CAP * 4),
            detail_path: None,
            stream: None,
        })
    }
}

async fn write_atomic(final_path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
    let name = final_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = parent.join(format!(".{name}.harness.{}.tmp", std::process::id()));
    {
        let mut f = tokio::fs::File::create(&tmp).await?;
        f.write_all(bytes).await?;
        f.flush().await?;
        f.sync_all().await?;
    }
    if let Err(e) = tokio::fs::rename(&tmp, final_path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

fn render_unified_diff(before: &str, after: &str, path: &str) -> String {
    let diff = TextDiff::from_lines(before, after);
    let mut out = String::new();
    out.push_str(&format!("--- {path}\n+++ {path}\n"));
    for group in diff.grouped_ops(3) {
        for op in group {
            for change in diff.iter_changes(&op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                    ChangeTag::Equal => ' ',
                };
                out.push(sign);
                out.push_str(change.value());
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

fn path_error_to_tool_error(e: PathError) -> ToolError {
    match e {
        PathError::Denied(_) | PathError::SymlinkBlocked(_) | PathError::Escapes(_) => {
            ToolError::PermissionDenied(e.to_string())
        }
        PathError::Io(io) => ToolError::Io(io),
        PathError::Toctou(_) => ToolError::Other(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::HookDispatcher;
    use harness_perm::PermissionSnapshot;
    use harness_proto::SessionId;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    fn ctx(cwd: &Path) -> ToolCtx {
        ToolCtx {
            cwd: cwd.to_path_buf(),
            session_id: SessionId::new("t"),
            cancel: CancellationToken::new(),
            permission: PermissionSnapshot::default(),
            hooks: HookDispatcher::default(),
            subagent: None,
            depth: 0,
            tx: None,
        }
    }

    #[tokio::test]
    async fn replaces_unique_occurrence() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("a.txt");
        tokio::fs::write(&f, "hello world").await.unwrap();
        EditTool
            .call(
                serde_json::json!({
                    "file_path": "a.txt",
                    "old_string": "world",
                    "new_string": "there"
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        let got = tokio::fs::read_to_string(&f).await.unwrap();
        assert_eq!(got, "hello there");
    }

    #[tokio::test]
    async fn rejects_nonunique_without_replace_all() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("a.txt");
        tokio::fs::write(&f, "x x x").await.unwrap();
        let err = EditTool
            .call(
                serde_json::json!({
                    "file_path": "a.txt",
                    "old_string": "x",
                    "new_string": "y"
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn replace_all_works() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("a.txt");
        tokio::fs::write(&f, "x x x").await.unwrap();
        EditTool
            .call(
                serde_json::json!({
                    "file_path": "a.txt",
                    "old_string": "x",
                    "new_string": "y",
                    "replace_all": true
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&f).await.unwrap(), "y y y");
    }

    #[tokio::test]
    async fn auto_stages_into_tx_when_wired() {
        use crate::tx::Transaction;
        use harness_core::tx::TxHandle;
        let dir = tempdir().unwrap();
        let f = dir.path().join("a.txt");
        tokio::fs::write(&f, "before-only").await.unwrap();
        let tx = Transaction::open(dir.path().to_path_buf()).await.unwrap();

        let mut c = ctx(dir.path());
        c.tx = Some(tx.as_handle());
        EditTool
            .call(
                serde_json::json!({
                    "file_path": "a.txt",
                    "old_string": "before",
                    "new_string": "after"
                }),
                c,
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read_to_string(&f).await.unwrap(), "after-only");
        assert_eq!(tx.staged_count(), 1);
        tx.rollback().await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&f).await.unwrap(), "before-only");
    }

    #[tokio::test]
    async fn not_found_errors() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("a.txt");
        tokio::fs::write(&f, "abc").await.unwrap();
        let err = EditTool
            .call(
                serde_json::json!({
                    "file_path": "a.txt",
                    "old_string": "xyz",
                    "new_string": "def"
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }
}
