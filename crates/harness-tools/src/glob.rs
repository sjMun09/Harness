//! `Glob` tool — `ignore::WalkBuilder` + `globset::GlobMatcher`, sorted by
//! mtime desc, capped at 1000. PLAN §3.1.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{head_tail, parse_input, HEAD_TAIL_CAP};
use crate::fs_safe::{canonicalize_within, PathError};

pub const MAX_GLOB_RESULTS: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobInput {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Default)]
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &'static str {
        "List filesystem paths matching a glob pattern (e.g. `src/**/*.rs`), sorted by modification time."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path":    { "type": "string" }
            },
            "required": ["pattern"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<GlobInput>(input.clone()) {
            Ok(gi) => Preview {
                summary_line: format!("Glob {}", gi.pattern),
                detail: gi.path,
            },
            Err(e) => Preview {
                summary_line: "Glob <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let gi: GlobInput = parse_input(input, "Glob")?;
        let search_root = match gi.path.as_deref() {
            Some(p) => {
                canonicalize_within(&ctx.cwd, Path::new(p)).map_err(path_error_to_tool_error)?
            }
            None => ctx.cwd.canonicalize().map_err(ToolError::Io)?,
        };

        let matcher = Glob::new(&gi.pattern)
            .map_err(|e| ToolError::Validation(format!("invalid glob: {e}")))?
            .compile_matcher();

        let cancel = ctx.cancel.clone();
        let root_for_walk = search_root.clone();
        let results: Vec<(PathBuf, SystemTime)> =
            tokio::task::spawn_blocking(move || walk_and_match(&root_for_walk, &matcher, &cancel))
                .await
                .map_err(|e| ToolError::Other(format!("glob join: {e}")))?;

        let total = results.len();
        let mut sorted = results;
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        sorted.truncate(MAX_GLOB_RESULTS);

        let mut body = String::new();
        for (p, _) in &sorted {
            body.push_str(&p.display().to_string());
            body.push('\n');
        }
        if body.ends_with('\n') {
            body.pop();
        }

        let header = format!(
            "{} match{} (showing {}){}",
            total,
            if total == 1 { "" } else { "es" },
            sorted.len(),
            if total > MAX_GLOB_RESULTS {
                format!(", capped at {MAX_GLOB_RESULTS}")
            } else {
                String::new()
            }
        );
        let summary = if body.is_empty() {
            header
        } else {
            format!("{header}\n{body}")
        };

        Ok(ToolOutput {
            summary: head_tail(&summary, HEAD_TAIL_CAP * 4),
            detail_path: None,
            stream: None,
        })
    }
}

fn walk_and_match(
    root: &Path,
    matcher: &GlobMatcher,
    cancel: &tokio_util::sync::CancellationToken,
) -> Vec<(PathBuf, SystemTime)> {
    let mut out = Vec::new();
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .ignore(true)
        .add_custom_ignore_filename(".harnessignore")
        .follow_links(false)
        .build();

    for entry in walker {
        if cancel.is_cancelled() {
            break;
        }
        let Ok(entry) = entry else { continue };
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_file() {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(path);
        if !matcher.is_match(rel) && !matcher.is_match(path) {
            continue;
        }
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        out.push((path.to_path_buf(), mtime));
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
    async fn finds_rust_files() {
        let dir = tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join("src"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("src/a.rs"), "")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("src/b.rs"), "")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("c.txt"), "")
            .await
            .unwrap();

        let out = GlobTool
            .call(serde_json::json!({ "pattern": "**/*.rs" }), ctx(dir.path()))
            .await
            .unwrap();
        assert!(out.summary.contains("a.rs"));
        assert!(out.summary.contains("b.rs"));
        assert!(!out.summary.contains("c.txt"));
    }

    #[tokio::test]
    async fn empty_match_reports_zero() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "")
            .await
            .unwrap();
        let out = GlobTool
            .call(
                serde_json::json!({ "pattern": "**/*.nope" }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.starts_with("0 matches"));
    }

    #[tokio::test]
    async fn harnessignore_excludes_node_modules() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join(".harnessignore"), "node_modules/\n")
            .await
            .unwrap();
        tokio::fs::create_dir_all(dir.path().join("node_modules"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("node_modules/x.js"), "")
            .await
            .unwrap();
        tokio::fs::create_dir_all(dir.path().join("src"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("src/a.js"), "")
            .await
            .unwrap();

        let out = GlobTool
            .call(serde_json::json!({ "pattern": "**/*.js" }), ctx(dir.path()))
            .await
            .unwrap();
        assert!(out.summary.contains("a.js"));
        assert!(!out.summary.contains("x.js"));
    }

    #[tokio::test]
    async fn invalid_pattern_is_validation_error() {
        let dir = tempdir().unwrap();
        let err = GlobTool
            .call(serde_json::json!({ "pattern": "[" }), ctx(dir.path()))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }
}
