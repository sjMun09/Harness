//! `Grep` tool — `grep_regex` + `grep_searcher` under `ignore::WalkBuilder`.
//! PLAN §3.1.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use globset::Glob;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::SearcherBuilder;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{fence_tool_output, head_tail, parse_input, HEAD_TAIL_CAP};
use crate::fs_safe::{canonicalize_within, PathError};

pub const MAX_GREP_RESULTS: usize = 1000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrepMode {
    Content,
    FilesWithMatches,
    Count,
}

impl Default for GrepMode {
    fn default() -> Self {
        Self::FilesWithMatches
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepInput {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub output_mode: GrepMode,
    #[serde(default, rename = "-i")]
    pub case_insensitive: bool,
    #[serde(default, rename = "-n")]
    pub line_numbers: Option<bool>,
}

#[derive(Debug, Default)]
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regex (ripgrep), returning matching lines, file lists, or counts."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern":     { "type": "string" },
                "path":        { "type": "string" },
                "glob":        { "type": "string" },
                "output_mode": { "type": "string", "enum": ["content","files_with_matches","count"] },
                "-i":          { "type": "boolean" },
                "-n":          { "type": "boolean" }
            },
            "required": ["pattern"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<GrepInput>(input.clone()) {
            Ok(gi) => Preview {
                summary_line: format!("Grep {}", gi.pattern),
                detail: gi.path,
            },
            Err(e) => Preview {
                summary_line: "Grep <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let gi: GrepInput = parse_input(input, "Grep")?;
        let search_root = match gi.path.as_deref() {
            Some(p) => {
                canonicalize_within(&ctx.cwd, Path::new(p)).map_err(path_error_to_tool_error)?
            }
            None => ctx.cwd.canonicalize().map_err(ToolError::Io)?,
        };

        let file_matcher = match gi.glob.as_deref() {
            Some(g) => Some(
                Glob::new(g)
                    .map_err(|e| ToolError::Validation(format!("invalid glob: {e}")))?
                    .compile_matcher(),
            ),
            None => None,
        };

        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(gi.case_insensitive)
            .build(&gi.pattern)
            .map_err(|e| ToolError::Validation(format!("invalid regex: {e}")))?;

        let cancel = ctx.cancel.clone();
        let mode = gi.output_mode;
        let show_line_nums = gi.line_numbers.unwrap_or(true);
        let root_for_walk = search_root.clone();

        let out_lines = tokio::task::spawn_blocking(move || {
            run_search(
                &root_for_walk,
                &matcher,
                file_matcher.as_ref(),
                mode,
                show_line_nums,
                &cancel,
            )
        })
        .await
        .map_err(|e| ToolError::Other(format!("grep join: {e}")))?;

        let body = out_lines.join("\n");
        let summary = if body.is_empty() {
            "0 results".to_string()
        } else {
            body
        };
        let truncated = head_tail(&summary, HEAD_TAIL_CAP * 4);
        Ok(ToolOutput {
            summary: fence_tool_output("Grep", None, &truncated),
            detail_path: None,
            stream: None,
        })
    }
}

fn run_search(
    root: &Path,
    matcher: &grep_regex::RegexMatcher,
    file_matcher: Option<&globset::GlobMatcher>,
    mode: GrepMode,
    show_line_nums: bool,
    cancel: &tokio_util::sync::CancellationToken,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let count = AtomicUsize::new(0);
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .ignore(true)
        .add_custom_ignore_filename(".harnessignore")
        .follow_links(false)
        .build();

    let mut searcher = SearcherBuilder::new().line_number(show_line_nums).build();

    for entry in walker {
        if cancel.is_cancelled() {
            break;
        }
        if count.load(Ordering::Relaxed) >= MAX_GREP_RESULTS {
            break;
        }
        let Ok(entry) = entry else { continue };
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_file() {
            continue;
        }
        let path: &Path = entry.path();
        if let Some(gm) = file_matcher {
            let rel = path.strip_prefix(root).unwrap_or(path);
            if !gm.is_match(rel) && !gm.is_match(path) {
                continue;
            }
        }
        match mode {
            GrepMode::FilesWithMatches => {
                let mut matched = false;
                let _ = searcher.search_path(
                    matcher,
                    path,
                    UTF8(|_, _| {
                        matched = true;
                        Ok(false)
                    }),
                );
                if matched {
                    out.push(path.display().to_string());
                    count.fetch_add(1, Ordering::Relaxed);
                }
            }
            GrepMode::Count => {
                let mut n: u64 = 0;
                let _ = searcher.search_path(
                    matcher,
                    path,
                    UTF8(|_, _| {
                        n += 1;
                        Ok(true)
                    }),
                );
                if n > 0 {
                    out.push(format!("{}:{n}", path.display()));
                    count.fetch_add(1, Ordering::Relaxed);
                }
            }
            GrepMode::Content => {
                let pb: PathBuf = path.to_path_buf();
                let mut hits: Vec<String> = Vec::new();
                let _ = searcher.search_path(
                    matcher,
                    path,
                    UTF8(|lnum, line| {
                        let trimmed = line.trim_end_matches('\n');
                        hits.push(format!("{}:{lnum}:{trimmed}", pb.display()));
                        Ok(hits.len() < MAX_GREP_RESULTS)
                    }),
                );
                for h in hits {
                    if count.load(Ordering::Relaxed) >= MAX_GREP_RESULTS {
                        break;
                    }
                    out.push(h);
                    count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
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
    async fn files_with_matches_mode() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "TODO: fix")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.txt"), "no marker")
            .await
            .unwrap();
        let out = GrepTool
            .call(serde_json::json!({ "pattern": "TODO" }), ctx(dir.path()))
            .await
            .unwrap();
        assert!(out.summary.contains("a.txt"));
        assert!(!out.summary.contains("b.txt"));
        assert!(out.summary.contains("<untrusted_tool_output tool=\"Grep\""));
    }

    #[tokio::test]
    async fn count_mode_reports_counts() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "x\nx\nno\n")
            .await
            .unwrap();
        let out = GrepTool
            .call(
                serde_json::json!({ "pattern": "x", "output_mode": "count" }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("a.txt:2"));
        assert!(out.summary.contains("<untrusted_tool_output tool=\"Grep\""));
    }

    #[tokio::test]
    async fn content_mode_with_line_numbers() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "one\nTODO: fix\nthree\n")
            .await
            .unwrap();
        let out = GrepTool
            .call(
                serde_json::json!({ "pattern": "TODO", "output_mode": "content" }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains(":2:TODO: fix"));
        assert!(out.summary.contains("<untrusted_tool_output tool=\"Grep\""));
    }

    #[tokio::test]
    async fn fence_tag_present_in_grep_output() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join("z.txt"), "needle")
            .await
            .unwrap();
        let out = GrepTool
            .call(serde_json::json!({ "pattern": "needle" }), ctx(dir.path()))
            .await
            .unwrap();
        assert!(out.summary.contains("<untrusted_tool_output tool=\"Grep\""));
        assert!(out.summary.contains("</untrusted_tool_output>"));
    }

    #[tokio::test]
    async fn case_insensitive_flag() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "HELLO")
            .await
            .unwrap();
        let out = GrepTool
            .call(
                serde_json::json!({ "pattern": "hello", "-i": true }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("a.txt"));
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
        tokio::fs::write(dir.path().join("node_modules/x.js"), "needle")
            .await
            .unwrap();
        tokio::fs::create_dir_all(dir.path().join("src"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("src/a.js"), "needle")
            .await
            .unwrap();

        let out = GrepTool
            .call(serde_json::json!({ "pattern": "needle" }), ctx(dir.path()))
            .await
            .unwrap();
        assert!(out.summary.contains("a.js"));
        assert!(!out.summary.contains("x.js"));
    }

    #[tokio::test]
    async fn invalid_regex_is_validation() {
        let dir = tempdir().unwrap();
        let err = GrepTool
            .call(
                serde_json::json!({ "pattern": "[unclosed" }),
                ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }
}
