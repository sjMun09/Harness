//! `DiffExec` tool — A/B differential. PLAN §4.2.
//!
//! **MVP scope = Docker-fallback path only.** The caller hands in two already-
//! rendered files (e.g. before / after Freemarker→SQL outputs, or two API
//! response JSONs); the tool normalizes them per `mode` and emits a unified
//! diff + summary. Full DiffExec (fixture spin-up, testcontainers DB, 4-sample
//! parameter injection, savepoint rollback) is a separate multi-day effort
//! blocked on a testcontainers integration layer — that remains iter-2+.
//!
//! The degrade path this file implements is **explicitly documented** in
//! PLAN §4.2: "Docker 부재 시 fallback: DB 실행 생략, 렌더드 SQL 문자열
//! diff 만 수행 (semantic 검증 불가 경고)". So this isn't a workaround — it's
//! the intended behaviour when no container runtime is reachable.
//!
//! Why a tool and not just `diff`:
//!   - SQL-aware normalization (comments, whitespace, keyword case) so a
//!     refactor that only rewrites formatting reports `identical`.
//!   - Single JSON surface so the model can call it from within a turn without
//!     shelling out — composes with ImportTrace / MyBatisDynamicParser.
//!   - Emits an explicit "NOT SEMANTICALLY EXECUTED" banner so the model is
//!     reminded this is a necessary-not-sufficient check.

use std::path::Path;

use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use similar::{ChangeTag, TextDiff};

use crate::common::{head_tail, parse_input, HEAD_TAIL_CAP};
use crate::fs_safe::{canonicalize_within, PathError};

/// Hard cap on each input's size. Parsing + normalizing 10 MiB SQL twice is
/// already pathological; the tool refuses beyond this.
pub const MAX_INPUT_BYTES: u64 = 10 * 1024 * 1024;

/// Default diff head/tail cap — diffs larger than this are truncated for the
/// tool result (the full diff still lives only in-memory this call).
pub const DEFAULT_DIFF_CAP: usize = HEAD_TAIL_CAP * 4;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffMode {
    /// SQL-aware normalization (comments + whitespace + keyword case) then
    /// unified line diff. Default because SQL is the primary target of this
    /// tool in the legacy-refactor workflow.
    #[default]
    Sql,
    /// Raw byte-equal unified diff. Use when comparing API response bodies,
    /// rendered HTML, or any payload where whitespace is significant.
    Text,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffExecInput {
    pub before_path: String,
    pub after_path: String,
    #[serde(default)]
    pub mode: DiffMode,
}

#[derive(Debug, Default)]
pub struct DiffExecTool;

#[async_trait]
impl Tool for DiffExecTool {
    fn name(&self) -> &str {
        "DiffExec"
    }

    fn description(&self) -> &'static str {
        "Diff two rendered files (text or SQL-normalized) and emit a unified diff plus summary; use to verify refactors are formatting-only."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "description": "A/B differential of two already-rendered files. Emits a unified diff + summary. SQL mode normalizes comments/whitespace/keyword case first so formatting-only refactors report identical. NOT a semantic executor — pairs with MyBatisDynamicParser for AST-level equivalence and (future iter) testcontainers DB runs for real execution.",
            "properties": {
                "before_path": {"type": "string", "description": "Path to the 'before' rendered file."},
                "after_path":  {"type": "string", "description": "Path to the 'after' rendered file."},
                "mode": {
                    "type": "string",
                    "enum": ["sql", "text"],
                    "default": "sql",
                    "description": "sql = normalize comments + whitespace + keyword case; text = raw diff."
                }
            },
            "required": ["before_path", "after_path"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<DiffExecInput>(input.clone()) {
            Ok(di) => Preview {
                summary_line: format!(
                    "DiffExec {} vs {} ({})",
                    di.before_path,
                    di.after_path,
                    mode_label(di.mode),
                ),
                detail: None,
            },
            Err(e) => Preview {
                summary_line: "DiffExec <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let di: DiffExecInput = parse_input(input, "DiffExec")?;
        let before_canon = canonicalize_within(&ctx.cwd, Path::new(&di.before_path))
            .map_err(path_error_to_tool_error)?;
        let after_canon = canonicalize_within(&ctx.cwd, Path::new(&di.after_path))
            .map_err(path_error_to_tool_error)?;

        let before_meta = tokio::fs::metadata(&before_canon).await?;
        let after_meta = tokio::fs::metadata(&after_canon).await?;
        if before_meta.len() > MAX_INPUT_BYTES {
            return Err(ToolError::Validation(format!(
                "before file too large: {} bytes (cap {MAX_INPUT_BYTES})",
                before_meta.len()
            )));
        }
        if after_meta.len() > MAX_INPUT_BYTES {
            return Err(ToolError::Validation(format!(
                "after file too large: {} bytes (cap {MAX_INPUT_BYTES})",
                after_meta.len()
            )));
        }

        let before_bytes = tokio::fs::read(&before_canon).await?;
        let after_bytes = tokio::fs::read(&after_canon).await?;
        let before_text = String::from_utf8_lossy(&before_bytes).into_owned();
        let after_text = String::from_utf8_lossy(&after_bytes).into_owned();

        let (before_norm, after_norm) = match di.mode {
            DiffMode::Sql => (normalize_sql(&before_text), normalize_sql(&after_text)),
            DiffMode::Text => (before_text, after_text),
        };

        let rendered = render_diff(
            &di.before_path,
            &di.after_path,
            di.mode,
            before_bytes.len(),
            after_bytes.len(),
            &before_norm,
            &after_norm,
        );

        Ok(ToolOutput {
            summary: head_tail(&rendered, DEFAULT_DIFF_CAP),
            detail_path: None,
            stream: None,
        })
    }
}

fn mode_label(m: DiffMode) -> &'static str {
    match m {
        DiffMode::Sql => "sql-normalized",
        DiffMode::Text => "text",
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

/// SQL-aware normalization.
///
/// Order matters: comments first (so `-- SELECT foo` doesn't leak a keyword
/// into the uppercase pass), then whitespace, then keyword case. We emit one
/// line per `;` so the unified diff is clause-level and reads naturally.
///
/// NOT a SQL parser — a refactor that changes only string-literal content,
/// identifier case, or operator spacing around `=` will still surface in the
/// diff. That's intentional: this is a necessary-condition check, not a
/// semantic equivalence prover.
pub fn normalize_sql(input: &str) -> String {
    let without_block = strip_block_comments(input);
    let without_line = strip_line_comments(&without_block);
    let collapsed = collapse_whitespace(&without_line);
    let uppercased = uppercase_keywords(&collapsed);
    // One statement per line: split on `;`, trim each, drop empties.
    let mut out = String::with_capacity(uppercased.len());
    for stmt in uppercased.split(';') {
        let s = stmt.trim();
        if s.is_empty() {
            continue;
        }
        out.push_str(s);
        out.push_str(";\n");
    }
    out
}

fn strip_block_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // Scan until `*/` or EOF.
            let mut j = i + 2;
            while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                j += 1;
            }
            i = (j + 2).min(bytes.len());
            // Replace the whole comment with a single space to preserve tokenization.
            out.push(' ');
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn strip_line_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for line in input.split_inclusive('\n') {
        if let Some(idx) = find_line_comment_start(line) {
            out.push_str(&line[..idx]);
            // Preserve newline if original had one.
            if line.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Find the start of a `--` line comment that is NOT inside a quoted string.
fn find_line_comment_start(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i + 1 < bytes.len() {
        let c = bytes[i];
        if c == b'\'' && !in_double {
            in_single = !in_single;
        } else if c == b'"' && !in_single {
            in_double = !in_double;
        } else if !in_single && !in_double && c == b'-' && bytes[i + 1] == b'-' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn collapse_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_ws = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

const SQL_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "EXISTS",
    "LIKE",
    "BETWEEN",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "OUTER",
    "FULL",
    "CROSS",
    "ON",
    "USING",
    "GROUP",
    "BY",
    "HAVING",
    "ORDER",
    "ASC",
    "DESC",
    "LIMIT",
    "OFFSET",
    "UNION",
    "ALL",
    "DISTINCT",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "AS",
    "WITH",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "CREATE",
    "TABLE",
    "INDEX",
    "VIEW",
    "ALTER",
    "DROP",
    "IF",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
    "DEFAULT",
    "UNIQUE",
    "CONSTRAINT",
];

/// Uppercase SQL keywords — identifier-safe: word boundary on both sides.
fn uppercase_keywords(input: &str) -> String {
    // Build a lookup once per call. Uppercase keyword set.
    // `SQL_KEYWORDS` is already uppercase at compile time.
    let kw: std::collections::HashSet<&'static str> = SQL_KEYWORDS.iter().copied().collect();
    let mut out = String::with_capacity(input.len());
    let mut buf = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for ch in input.chars() {
        if ch == '\'' && !in_double {
            flush_word(&mut buf, &kw, &mut out);
            in_single = !in_single;
            out.push(ch);
            continue;
        }
        if ch == '"' && !in_single {
            flush_word(&mut buf, &kw, &mut out);
            in_double = !in_double;
            out.push(ch);
            continue;
        }
        if in_single || in_double {
            out.push(ch);
            continue;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' {
            buf.push(ch);
        } else {
            flush_word(&mut buf, &kw, &mut out);
            out.push(ch);
        }
    }
    flush_word(&mut buf, &kw, &mut out);
    out
}

fn flush_word(buf: &mut String, kw: &std::collections::HashSet<&str>, out: &mut String) {
    if buf.is_empty() {
        return;
    }
    let upper = buf.to_ascii_uppercase();
    if kw.contains(upper.as_str()) {
        out.push_str(&upper);
    } else {
        out.push_str(buf);
    }
    buf.clear();
}

fn render_diff(
    before_path: &str,
    after_path: &str,
    mode: DiffMode,
    before_raw_len: usize,
    after_raw_len: usize,
    before_norm: &str,
    after_norm: &str,
) -> String {
    let diff = TextDiff::from_lines(before_norm, after_norm);
    let mut added = 0u32;
    let mut removed = 0u32;
    let mut body = String::new();
    use std::fmt::Write;
    for change in diff.iter_all_changes() {
        let sigil = match change.tag() {
            ChangeTag::Equal => " ",
            ChangeTag::Insert => {
                added = added.saturating_add(1);
                "+"
            }
            ChangeTag::Delete => {
                removed = removed.saturating_add(1);
                "-"
            }
        };
        let line = change.value();
        let trimmed = line.strip_suffix('\n').unwrap_or(line);
        let _ = writeln!(body, "{sigil}{trimmed}");
    }

    let mut out = String::new();
    let _ = writeln!(out, "MODE: {}", mode_label(mode));
    let _ = writeln!(out, "BEFORE: {before_path} ({before_raw_len} bytes raw)");
    let _ = writeln!(out, "AFTER:  {after_path} ({after_raw_len} bytes raw)");
    let _ = writeln!(
        out,
        "WARNING: NOT SEMANTICALLY EXECUTED — rendered-text diff only. Pair with \
MyBatisDynamicParser for AST equivalence and DB run for semantic proof."
    );
    if added == 0 && removed == 0 {
        let _ = writeln!(out, "RESULT: identical (after normalization)");
        return out;
    }
    let _ = writeln!(out, "RESULT: +{added} / -{removed} lines");
    let _ = writeln!(out, "---");
    out.push_str(&body);
    out
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

    #[test]
    fn normalize_strips_line_comments() {
        let s = "SELECT 1 -- ignored\nFROM t";
        let out = normalize_sql(s);
        assert!(!out.contains("ignored"));
        assert!(out.contains("SELECT 1"));
        assert!(out.contains("FROM T") || out.contains("FROM t"));
    }

    #[test]
    fn normalize_strips_block_comments() {
        let s = "SELECT /* this is ignored */ 1 FROM t";
        let out = normalize_sql(s);
        assert!(!out.contains("ignored"));
    }

    #[test]
    fn normalize_uppercases_keywords_only() {
        let s = "select userName from userTable where id=1";
        let out = normalize_sql(s);
        assert!(out.contains("SELECT"));
        assert!(out.contains("FROM"));
        assert!(out.contains("WHERE"));
        // Identifiers preserved case.
        assert!(out.contains("userName"));
        assert!(out.contains("userTable"));
    }

    #[test]
    fn normalize_leaves_string_literals_alone() {
        let s = "SELECT 'hello from world' FROM t";
        let out = normalize_sql(s);
        // The word "from" inside the literal MUST not be uppercased.
        assert!(out.contains("'hello from world'"));
    }

    #[test]
    fn normalize_ignores_comment_marker_inside_string() {
        let s = "SELECT 'it -- works' FROM t";
        let out = normalize_sql(s);
        assert!(out.contains("'it -- works'"));
    }

    #[test]
    fn normalize_collapses_whitespace() {
        let a = normalize_sql("SELECT   a,\n  b\nFROM   t");
        let b = normalize_sql("SELECT a, b FROM t");
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn identical_files_report_identical() {
        let dir = tempdir().unwrap();
        let sql = "SELECT a FROM t WHERE id = 1";
        tokio::fs::write(dir.path().join("a.sql"), sql)
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.sql"), sql)
            .await
            .unwrap();
        let out = DiffExecTool
            .call(
                serde_json::json!({"before_path": "a.sql", "after_path": "b.sql"}),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("identical"));
    }

    #[tokio::test]
    async fn sql_mode_treats_whitespace_and_case_diff_as_identical() {
        let dir = tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("before.sql"),
            "select a from t where id = 1",
        )
        .await
        .unwrap();
        tokio::fs::write(
            dir.path().join("after.sql"),
            "SELECT  a\nFROM t\nWHERE  id = 1",
        )
        .await
        .unwrap();
        let out = DiffExecTool
            .call(
                serde_json::json!({
                    "before_path": "before.sql",
                    "after_path": "after.sql",
                    "mode": "sql"
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(
            out.summary.contains("identical"),
            "summary was: {}",
            out.summary
        );
    }

    #[tokio::test]
    async fn text_mode_shows_whitespace_diff() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join("a"), "hello world")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b"), "hello  world")
            .await
            .unwrap();
        let out = DiffExecTool
            .call(
                serde_json::json!({
                    "before_path": "a",
                    "after_path": "b",
                    "mode": "text"
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(!out.summary.contains("identical"));
        assert!(out.summary.contains("+1") || out.summary.contains("-1"));
    }

    #[tokio::test]
    async fn sql_mode_shows_real_semantic_diff() {
        let dir = tempdir().unwrap();
        tokio::fs::write(
            dir.path().join("before.sql"),
            "SELECT a FROM t WHERE id = 1",
        )
        .await
        .unwrap();
        tokio::fs::write(dir.path().join("after.sql"), "SELECT a FROM t WHERE id = 2")
            .await
            .unwrap();
        let out = DiffExecTool
            .call(
                serde_json::json!({
                    "before_path": "before.sql",
                    "after_path": "after.sql"
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(!out.summary.contains("identical"));
        assert!(out.summary.contains("id = 2") || out.summary.contains("ID = 2"));
    }

    #[tokio::test]
    async fn warning_banner_always_present() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.sql"), "SELECT 1")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("b.sql"), "SELECT 1")
            .await
            .unwrap();
        let out = DiffExecTool
            .call(
                serde_json::json!({"before_path": "a.sql", "after_path": "b.sql"}),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("NOT SEMANTICALLY EXECUTED"));
    }

    #[tokio::test]
    async fn rejects_missing_before() {
        let dir = tempdir().unwrap();
        tokio::fs::write(dir.path().join("b.sql"), "SELECT 1")
            .await
            .unwrap();
        let err = DiffExecTool
            .call(
                serde_json::json!({"before_path": "missing.sql", "after_path": "b.sql"}),
                ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ToolError::Io(_) | ToolError::PermissionDenied(_)
        ));
    }

    #[tokio::test]
    async fn rejects_parent_escape() {
        let dir = tempdir().unwrap();
        let err = DiffExecTool
            .call(
                serde_json::json!({"before_path": "../x", "after_path": "../y"}),
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
