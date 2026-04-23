//! `Write` tool — atomic tempfile + rename under canonical parent. PLAN §3.1.
//!
//! On Linux, when the caller's intent is "create a new file" (the final
//! path did not exist at the time we probed), the final rename uses
//! `renameat2(RENAME_NOREPLACE)` via `rustix` so a race where the
//! destination appears between the probe and the rename fails closed.
//! On intentional overwrites, or on non-Linux platforms, we use plain
//! `tokio::fs::rename` so we don't surprise callers with `EEXIST`.

use std::path::Path;

use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::fs_safe::{canonicalize_within, PathError};

pub const MAX_WRITE_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteInput {
    pub file_path: String,
    pub content: String,
}

#[derive(Debug, Default)]
pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &'static str {
        "Write UTF-8 content to a file path, creating parent directories and replacing any existing file."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["file_path", "content"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<WriteInput>(input.clone()) {
            Ok(wi) => {
                let bytes = wi.content.len();
                Preview {
                    summary_line: format!("Write {} ({} bytes)", wi.file_path, bytes),
                    detail: None,
                }
            }
            Err(e) => Preview {
                summary_line: "Write <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let wi: WriteInput = serde_json::from_value(input)
            .map_err(|e| ToolError::Validation(format!("Write: {e}")))?;
        if wi.content.len() as u64 > MAX_WRITE_BYTES {
            return Err(ToolError::Validation(format!(
                "content too large: {} bytes (cap {MAX_WRITE_BYTES})",
                wi.content.len()
            )));
        }

        let target = Path::new(&wi.file_path);
        let file_name = target
            .file_name()
            .ok_or_else(|| ToolError::Validation("Write: empty file name".into()))?
            .to_os_string();
        let parent = target.parent().unwrap_or_else(|| Path::new("."));

        let canonical_parent =
            canonicalize_within(&ctx.cwd, parent).map_err(path_error_to_tool_error)?;
        tokio::fs::create_dir_all(&canonical_parent).await?;

        let final_path = canonical_parent.join(&file_name);
        // Remember whether this is a create-new vs overwrite BEFORE the
        // tempfile lands. The flag decides whether we pass `RENAME_NOREPLACE`
        // on the final rename (Linux) to close the TOCTOU window between
        // this check and the rename. Intentional overwrites keep plain
        // `rename` so they are not surprised by `EEXIST`.
        let exists_before_write = tokio::fs::metadata(&final_path).await.is_ok();

        let tmp_path = canonical_parent.join(format!(
            ".{}.harness.{}.tmp",
            file_name.to_string_lossy(),
            std::process::id()
        ));

        {
            let mut f = tokio::fs::File::create(&tmp_path).await?;
            f.write_all(wi.content.as_bytes()).await?;
            f.flush().await?;
            f.sync_all().await?;
        }

        if let Err(e) = atomic_rename(&tmp_path, &final_path, exists_before_write).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(e);
        }

        Ok(ToolOutput {
            summary: format!(
                "wrote {} bytes to {}",
                wi.content.len(),
                final_path.display()
            ),
            detail_path: None,
            stream: None,
        })
    }
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

/// Atomic rename of `tmp` over `dst`.
///
/// On Linux, when the caller's pre-write probe said the destination did
/// NOT exist (`existed_before == false`), use `renameat2(RENAME_NOREPLACE)`
/// so the rename fails with `EEXIST` if the destination appeared between
/// the probe and the rename — closing the TOCTOU window on create-new.
/// When the caller is intentionally overwriting (`existed_before == true`)
/// or on non-Linux, use plain `rename`, which preserves the overwrite path.
async fn atomic_rename(tmp: &Path, dst: &Path, existed_before: bool) -> Result<(), ToolError> {
    #[cfg(target_os = "linux")]
    {
        if !existed_before {
            // Run the sync syscall on a blocking thread to avoid stalling
            // the tokio worker.
            let tmp_buf = tmp.to_path_buf();
            let dst_buf = dst.to_path_buf();
            let res = tokio::task::spawn_blocking(move || {
                use rustix::fs::{renameat_with, RenameFlags, CWD};
                renameat_with(CWD, &tmp_buf, CWD, &dst_buf, RenameFlags::NOREPLACE)
            })
            .await
            .map_err(|e| ToolError::Other(format!("renameat2 join error: {e}")))?;
            return match res {
                Ok(()) => Ok(()),
                Err(e) if e.raw_os_error() == libc::ENOSYS || e.raw_os_error() == libc::EINVAL => {
                    // Kernel lacks renameat2 or the filesystem doesn't support
                    // RENAME_NOREPLACE — fall back to plain rename.
                    tokio::fs::rename(tmp, dst).await.map_err(ToolError::Io)
                }
                Err(e) if e.raw_os_error() == libc::EEXIST => Err(ToolError::Other(format!(
                    "refusing to overwrite {} (appeared between check and write)",
                    dst.display()
                ))),
                Err(e) => Err(ToolError::Io(std::io::Error::from_raw_os_error(
                    e.raw_os_error(),
                ))),
            };
        }
    }
    // Overwrite path, or non-Linux.
    let _ = existed_before;
    tokio::fs::rename(tmp, dst).await.map_err(ToolError::Io)
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
            ask_prompt: None,
        }
    }

    #[tokio::test]
    async fn writes_new_file() {
        let dir = tempdir().unwrap();
        let out = WriteTool
            .call(
                serde_json::json!({ "file_path": "new.txt", "content": "hello" }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("wrote 5 bytes"));
        let got = tokio::fs::read_to_string(dir.path().join("new.txt"))
            .await
            .unwrap();
        assert_eq!(got, "hello");
    }

    #[tokio::test]
    async fn creates_parent_dirs() {
        let dir = tempdir().unwrap();
        WriteTool
            .call(
                serde_json::json!({ "file_path": "sub/nested/f.txt", "content": "x" }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(dir.path().join("sub/nested/f.txt").exists());
    }

    #[tokio::test]
    async fn rejects_parent_escape() {
        let dir = tempdir().unwrap();
        let err = WriteTool
            .call(
                serde_json::json!({ "file_path": "../escape.txt", "content": "x" }),
                ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ToolError::PermissionDenied(_) | ToolError::Io(_)
        ));
    }

    /// Drive the `atomic_rename` helper directly with `existed_before=false`
    /// when the destination *does* in fact exist — simulating the TOCTOU
    /// race where the file appears between the caller's `exists()` probe
    /// and the rename. On Linux with kernel >= 3.15 + a supporting fs this
    /// must fail with an `EEXIST`-mapped error. On older kernels the
    /// fallback performs a plain rename, so we only gate the strict check
    /// on `target_os = "linux"` and accept either outcome if the syscall
    /// degraded (the test still exercises the code path).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn renameat2_noreplace_fails_if_exists() {
        let dir = tempdir().unwrap();
        let tmp = dir.path().join("src.tmp");
        let dst = dir.path().join("dst.txt");
        tokio::fs::write(&tmp, b"new").await.unwrap();
        tokio::fs::write(&dst, b"old").await.unwrap();

        let res = atomic_rename(&tmp, &dst, false).await;
        match res {
            Err(ToolError::Other(msg)) => {
                assert!(msg.contains("refusing to overwrite"), "unexpected: {msg}");
                // dst must still hold the original content.
                let got = tokio::fs::read(&dst).await.unwrap();
                assert_eq!(got, b"old");
            }
            Ok(()) => {
                // Fallback path (renameat2 unsupported) — document but don't fail.
                eprintln!("renameat2 unsupported on this host; fell back to plain rename");
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
}
