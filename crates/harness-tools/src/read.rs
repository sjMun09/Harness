//! `Read` tool — mmap, `cat -n`, binary sniff, line cap. PLAN §3.1.

use std::path::Path;

use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{fence_tool_output, head_tail, parse_input, HEAD_TAIL_CAP};
use crate::fs_safe::{canonicalize_within, PathError};

pub const MAX_READ_LINES: u64 = 20_000;
pub const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
/// Bytes sampled from the head to sniff binary content.
const BINARY_SNIFF_BYTES: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadInput {
    pub file_path: String,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

#[derive(Debug, Default)]
pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Absolute path to read" },
                "offset": { "type": "integer", "minimum": 1 },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_READ_LINES }
            },
            "required": ["file_path"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<ReadInput>(input.clone()) {
            Ok(ri) => Preview {
                summary_line: format!("Read {}", ri.file_path),
                detail: None,
            },
            Err(e) => Preview {
                summary_line: "Read <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let ri: ReadInput = parse_input(input, "Read")?;
        let canonical = canonicalize_within(&ctx.cwd, Path::new(&ri.file_path))
            .map_err(path_error_to_tool_error)?;

        let meta = tokio::fs::metadata(&canonical).await?;
        if meta.len() > MAX_FILE_BYTES {
            return Err(ToolError::Validation(format!(
                "file too large: {} bytes (cap {MAX_FILE_BYTES})",
                meta.len()
            )));
        }

        let bytes = tokio::fs::read(&canonical).await?;
        if is_binary(&bytes) {
            let raw = format!(
                "[binary file {}: {} bytes, preview suppressed]",
                canonical.display(),
                bytes.len()
            );
            return Ok(ToolOutput {
                summary: fence_tool_output("Read", Some(&ri.file_path), &raw),
                detail_path: None,
                stream: None,
            });
        }

        let text = String::from_utf8_lossy(&bytes);
        let rendered = render_cat_n(&text, ri.offset, ri.limit);
        let truncated = head_tail(&rendered, HEAD_TAIL_CAP * 4);
        Ok(ToolOutput {
            summary: fence_tool_output("Read", Some(&ri.file_path), &truncated),
            detail_path: None,
            stream: None,
        })
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    let sample_len = bytes.len().min(BINARY_SNIFF_BYTES);
    if sample_len == 0 {
        return false;
    }
    let sample = &bytes[..sample_len];
    if sample.contains(&0) {
        return true;
    }
    // Reject if ≥30% of bytes are outside printable ASCII / common whitespace /
    // UTF-8 continuation-safe range.
    let nonprint = sample
        .iter()
        .filter(|&&b| {
            !(b == b'\t' || b == b'\n' || b == b'\r' || (0x20..=0x7e).contains(&b) || b >= 0x80)
        })
        .count();
    nonprint * 10 >= sample.len() * 3
}

fn render_cat_n(text: &str, offset: Option<u64>, limit: Option<u64>) -> String {
    let start = offset.unwrap_or(1).saturating_sub(1) as usize;
    let take = limit.unwrap_or(2000).min(MAX_READ_LINES) as usize;

    let mut out = String::new();
    for (i, line) in text.split('\n').enumerate().skip(start).take(take) {
        use std::fmt::Write;
        let _ = writeln!(out, "{:>6}\t{line}", i + 1);
    }
    // Strip the trailing newline we just added via `writeln!`.
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
    async fn reads_text_file_with_line_numbers() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("a.txt");
        tokio::fs::write(&f, "one\ntwo\nthree\n").await.unwrap();
        let out = ReadTool
            .call(serde_json::json!({ "file_path": "a.txt" }), ctx(dir.path()))
            .await
            .unwrap();
        assert!(out.summary.contains("     1\tone"));
        assert!(out.summary.contains("     2\ttwo"));
        assert!(out.summary.contains("     3\tthree"));
        assert!(out.summary.contains("<untrusted_tool_output tool=\"Read\""));
    }

    #[tokio::test]
    async fn suppresses_binary_content() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("bin.dat");
        tokio::fs::write(&f, b"\x00\x01\x02binary").await.unwrap();
        let out = ReadTool
            .call(
                serde_json::json!({ "file_path": "bin.dat" }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("[binary file"));
        assert!(out.summary.contains("<untrusted_tool_output tool=\"Read\""));
    }

    #[tokio::test]
    async fn fence_tag_present_in_text_output() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("hello.txt");
        tokio::fs::write(&f, "hello world\n").await.unwrap();
        let out = ReadTool
            .call(
                serde_json::json!({ "file_path": "hello.txt" }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("<untrusted_tool_output tool=\"Read\""));
        assert!(out.summary.contains("</untrusted_tool_output>"));
    }

    #[tokio::test]
    async fn fence_tag_present_in_binary_output() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("data.bin");
        tokio::fs::write(&f, b"\x00\x01binary data").await.unwrap();
        let out = ReadTool
            .call(
                serde_json::json!({ "file_path": "data.bin" }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("<untrusted_tool_output tool=\"Read\""));
        assert!(out.summary.contains("</untrusted_tool_output>"));
    }

    #[tokio::test]
    async fn rejects_parent_escape() {
        let dir = tempdir().unwrap();
        let err = ReadTool
            .call(
                serde_json::json!({ "file_path": "../escape" }),
                ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ToolError::PermissionDenied(_) | ToolError::Io(_)
        ));
    }
}
