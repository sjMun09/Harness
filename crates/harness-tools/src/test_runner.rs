//! `Test` tool — runner matrix wrapper. PLAN §3.2 / §4.1.
//!
//! Wraps `cargo test` / `mvn test` / `pytest` / `vitest` / `jest` /
//! `playwright test` (and user-defined Custom) with:
//!   - per-runner failure-summary parsing surfaced at the top of the result
//!   - head 4 KiB + tail 4 KiB of combined stdout/stderr
//!   - full log streamed to disk, path appended
//!   - optional auto-retry capped at 3 (PLAN §4.1)
//!   - shared Bash-style env scrub + fresh pgid + cancel-safe kill
//!
//! Rationale for a dedicated tool (not just Bash): the legacy-refactor loop
//! needs to ask the model to verify a refactor, and the model shouldn't have
//! to re-derive "which runner and flags" on every turn. HARNESS.md
//! `## Test Commands` + this runner enum gives a stable, typed surface.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::bash::DEFAULT_ENV_ALLOW;
use crate::common::{fence_tool_output, head_tail, parse_input, HEAD_TAIL_CAP};
use crate::proc::graceful_kill_pgid;

pub const DEFAULT_TIMEOUT_SECS: u64 = 600;
pub const MAX_TIMEOUT_SECS: u64 = 1800;
pub const MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TestRunner {
    CargoTest,
    MvnTest,
    Pytest,
    Vitest,
    Jest,
    Playwright,
    /// Free-form: use the `command` field as a full shell command line.
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestInput {
    pub runner: TestRunner,
    #[serde(default)]
    pub args: Vec<String>,
    /// For `runner=custom`: full command line (argv-split). Ignored for
    /// typed runners (use `args` instead).
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// 1..=3. Non-zero exit triggers re-run; zero exit short-circuits.
    #[serde(default)]
    pub attempts: Option<u32>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Default)]
pub struct TestTool;

#[async_trait]
impl Tool for TestTool {
    fn name(&self) -> &str {
        "Test"
    }

    fn description(&self) -> &'static str {
        "Run a project's test suite (cargo/maven/pytest/vitest/jest/playwright or a custom command) and return a failure-focused summary."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "description": "Run a test suite (cargo/maven/pytest/vitest/jest/playwright or custom). Streams full output to disk and returns a head+tail slice with a parsed failure summary on top. Use `runner=custom` + `command` for runners not in the enum. Retries up to `attempts` (cap 3) on non-zero exit.",
            "properties": {
                "runner": {
                    "type": "string",
                    "enum": ["cargo_test", "mvn_test", "pytest", "vitest", "jest", "playwright", "custom"]
                },
                "args":         { "type": "array", "items": { "type": "string" } },
                "command":      { "type": "string", "description": "Full command line for runner=custom." },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_SECS },
                "attempts":     { "type": "integer", "minimum": 1, "maximum": MAX_ATTEMPTS, "default": 1 },
                "description":  { "type": "string" }
            },
            "required": ["runner"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<TestInput>(input.clone()) {
            Ok(ti) => {
                let label = runner_label(ti.runner);
                let argstr = if ti.args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", ti.args.join(" "))
                };
                Preview {
                    summary_line: format!("Test[{label}]{argstr}"),
                    detail: ti.description,
                }
            }
            Err(e) => Preview {
                summary_line: "Test <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let ti: TestInput = parse_input(input, "Test")?;
        let timeout_secs = ti
            .timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        let attempts = ti.attempts.unwrap_or(1).clamp(1, MAX_ATTEMPTS);

        let argv = build_argv(&ti)?;
        let wait = Duration::from_secs(timeout_secs);

        let mut attempts_log: Vec<AttemptResult> = Vec::new();
        let mut last_combined = String::new();
        let mut last_status_label = String::new();
        let mut detail_path: Option<PathBuf> = None;

        for attempt in 1..=attempts {
            let outcome = run_once(&argv, &ctx, wait).await?;
            last_combined = outcome.combined;
            last_status_label = outcome.status_label;
            let parsed = parse_failure_summary(ti.runner, &last_combined);
            attempts_log.push(AttemptResult {
                attempt,
                status_label: last_status_label.clone(),
                summary: parsed.clone(),
                exit_ok: outcome.exit_ok,
            });
            // Persist the FINAL attempt's full log to disk.
            detail_path = write_detail(ctx.session_id.as_str(), &last_combined).await;
            if outcome.exit_ok {
                break;
            }
        }

        let summary = render_summary(
            ti.runner,
            &argv,
            &attempts_log,
            &last_combined,
            &last_status_label,
            detail_path.as_ref(),
        );
        let summary = fence_tool_output("Test", None, &summary);
        Ok(ToolOutput {
            summary,
            detail_path,
            stream: None,
        })
    }
}

fn runner_label(r: TestRunner) -> &'static str {
    match r {
        TestRunner::CargoTest => "cargo_test",
        TestRunner::MvnTest => "mvn_test",
        TestRunner::Pytest => "pytest",
        TestRunner::Vitest => "vitest",
        TestRunner::Jest => "jest",
        TestRunner::Playwright => "playwright",
        TestRunner::Custom => "custom",
    }
}

fn build_argv(ti: &TestInput) -> Result<Vec<String>, ToolError> {
    match ti.runner {
        TestRunner::CargoTest => Ok(prepend(&["cargo", "test"], &ti.args)),
        TestRunner::MvnTest => Ok(prepend(&["mvn", "test"], &ti.args)),
        TestRunner::Pytest => Ok(prepend(&["pytest"], &ti.args)),
        TestRunner::Vitest => Ok(prepend(&["vitest", "run"], &ti.args)),
        TestRunner::Jest => Ok(prepend(&["jest"], &ti.args)),
        TestRunner::Playwright => Ok(prepend(&["playwright", "test"], &ti.args)),
        TestRunner::Custom => {
            let cmd = ti.command.as_deref().ok_or_else(|| {
                ToolError::Validation("Test: runner=custom requires `command`".into())
            })?;
            let parts = shlex::split(cmd).ok_or_else(|| {
                ToolError::Validation(format!("Test: could not shlex-split {cmd:?}"))
            })?;
            if parts.is_empty() {
                return Err(ToolError::Validation("Test: empty custom command".into()));
            }
            let mut v = parts;
            v.extend(ti.args.iter().cloned());
            Ok(v)
        }
    }
}

fn prepend(prefix: &[&str], tail: &[String]) -> Vec<String> {
    let mut v: Vec<String> = prefix.iter().map(|s| (*s).to_string()).collect();
    v.extend(tail.iter().cloned());
    v
}

struct AttemptResult {
    attempt: u32,
    status_label: String,
    summary: Option<String>,
    exit_ok: bool,
}

struct RunOutcome {
    combined: String,
    status_label: String,
    exit_ok: bool,
}

async fn run_once(argv: &[String], ctx: &ToolCtx, wait: Duration) -> Result<RunOutcome, ToolError> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(&ctx.cwd);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    scrub_env(&mut cmd);
    set_new_pgid(&mut cmd);

    let mut child = cmd.spawn().map_err(ToolError::Io)?;
    let pgid = child.id().map(|p| p as i32);
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();

    let cancel = ctx.cancel.clone();
    let run = async move {
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let read_out = async {
            if let Some(s) = stdout.as_mut() {
                let _ = s.read_to_end(&mut out).await;
            }
        };
        let read_err = async {
            if let Some(s) = stderr.as_mut() {
                let _ = s.read_to_end(&mut err).await;
            }
        };
        tokio::join!(read_out, read_err);
        let status = child.wait().await;
        (out, err, status)
    };

    tokio::select! {
        biased;
        () = cancel.cancelled() => {
            if let Some(pg) = pgid {
                graceful_kill_pgid(pg).await;
            }
            Err(ToolError::Cancelled)
        }
        res = timeout(wait, run) => {
            match res {
                Ok((out, err, Ok(status))) => {
                    let combined = combine_output(&out, &err);
                    let exit_ok = status.success();
                    let status_label = status
                        .code()
                        .map_or_else(|| "exit sig".into(), |c| format!("exit {c}"));
                    Ok(RunOutcome { combined, status_label, exit_ok })
                }
                Ok((_, _, Err(e))) => Err(ToolError::Io(e)),
                Err(_) => {
                    if let Some(pg) = pgid {
                        graceful_kill_pgid(pg).await;
                    }
                    Err(ToolError::Timeout(wait))
                }
            }
        }
    }
}

fn scrub_env(cmd: &mut Command) {
    cmd.env_clear();
    for k in DEFAULT_ENV_ALLOW {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }
}

#[cfg(unix)]
fn set_new_pgid(cmd: &mut Command) {
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn set_new_pgid(_cmd: &mut Command) {}

fn combine_output(out: &[u8], err: &[u8]) -> String {
    let mut s = String::new();
    if !out.is_empty() {
        s.push_str(&String::from_utf8_lossy(out));
    }
    if !err.is_empty() {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("--- stderr ---\n");
        s.push_str(&String::from_utf8_lossy(err));
    }
    s
}

async fn write_detail(session_id: &str, body: &str) -> Option<PathBuf> {
    if body.is_empty() {
        return None;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let path = std::env::temp_dir().join(format!("harness-test-{session_id}-{ts}.log"));
    tokio::fs::write(&path, body).await.ok()?;
    Some(path)
}

/// Per-runner "last-occurrence wins" summary line extraction.
///
/// These are deliberately lightweight — the goal is to surface "X passed / Y
/// failed" to the model at the top of the output so it doesn't have to scan a
/// 4 KB tail to decide whether the run was green. When a runner changes its
/// output format, the fallback is graceful: we just omit the parsed summary.
fn parse_failure_summary(runner: TestRunner, combined: &str) -> Option<String> {
    let pattern = match runner {
        // cargo: `test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s`
        TestRunner::CargoTest => {
            r"test result: (?:ok|FAILED)\. \d+ passed; \d+ failed(?:; \d+ ignored)?"
        }
        // maven surefire: `Tests run: 12, Failures: 1, Errors: 0, Skipped: 0`
        TestRunner::MvnTest => r"Tests run: \d+, Failures: \d+, Errors: \d+(?:, Skipped: \d+)?",
        // pytest: `===== 3 failed, 2 passed in 0.12s =====` or `===== 5 passed in 0.04s =====`
        TestRunner::Pytest => {
            r"={2,}[^=\n]*?(?:\d+ (?:failed|passed|error|skipped)[^=\n]*)+ in [0-9.]+s[^=\n]*={2,}"
        }
        // vitest: `Test Files  1 failed | 2 passed (3)` or `Tests  5 passed (5)`
        TestRunner::Vitest => r"(?:Test Files|Tests)\s+[0-9]+[^\n]*",
        // jest: `Tests:       1 failed, 2 passed, 3 total`
        TestRunner::Jest => r"Tests:\s+[^\n]*\d+ total",
        // playwright: `  5 passed (1.2s)` or `  1 failed`
        TestRunner::Playwright => {
            r"\d+ (?:failed|passed|skipped|flaky)(?:[^\n]*\d+ (?:failed|passed|skipped|flaky))*"
        }
        TestRunner::Custom => return None,
    };
    let re = Regex::new(pattern).ok()?;
    re.find_iter(combined)
        .last()
        .map(|m| m.as_str().trim().to_string())
}

fn render_summary(
    runner: TestRunner,
    argv: &[String],
    attempts: &[AttemptResult],
    last_combined: &str,
    last_status: &str,
    detail_path: Option<&PathBuf>,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "RUNNER: {}", runner_label(runner));
    let _ = writeln!(out, "COMMAND: {}", argv.join(" "));
    let _ = writeln!(out, "ATTEMPTS: {}/{MAX_ATTEMPTS}", attempts.len());
    for a in attempts {
        let verdict = if a.exit_ok { "PASS" } else { "FAIL" };
        match a.summary.as_ref() {
            Some(s) => {
                let _ = writeln!(out, "  #{} {verdict} ({}) — {s}", a.attempt, a.status_label);
            }
            None => {
                let _ = writeln!(out, "  #{} {verdict} ({})", a.attempt, a.status_label);
            }
        }
    }
    let _ = writeln!(out, "FINAL: {last_status}");
    if let Some(p) = detail_path {
        let _ = writeln!(out, "FULL LOG: {}", p.display());
    }
    let _ = writeln!(out, "---");
    out.push_str(&head_tail(last_combined, HEAD_TAIL_CAP));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::HookDispatcher;
    use harness_perm::PermissionSnapshot;
    use harness_proto::SessionId;
    use std::path::Path;
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
    fn build_argv_cargo_prepends_test_subcommand() {
        let ti = TestInput {
            runner: TestRunner::CargoTest,
            args: vec!["-p".into(), "harness-core".into()],
            command: None,
            timeout_secs: None,
            attempts: None,
            description: None,
        };
        let argv = build_argv(&ti).unwrap();
        assert_eq!(argv, vec!["cargo", "test", "-p", "harness-core"]);
    }

    #[test]
    fn build_argv_custom_splits_command_and_appends_args() {
        let ti = TestInput {
            runner: TestRunner::Custom,
            args: vec!["--suite".into(), "smoke".into()],
            command: Some("bun run e2e".into()),
            timeout_secs: None,
            attempts: None,
            description: None,
        };
        let argv = build_argv(&ti).unwrap();
        assert_eq!(argv, vec!["bun", "run", "e2e", "--suite", "smoke"]);
    }

    #[test]
    fn build_argv_custom_requires_command() {
        let ti = TestInput {
            runner: TestRunner::Custom,
            args: vec![],
            command: None,
            timeout_secs: None,
            attempts: None,
            description: None,
        };
        let err = build_argv(&ti).unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[test]
    fn parse_cargo_pass() {
        let out = "running 5 tests\n... lots of output ...\ntest result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
        let got = parse_failure_summary(TestRunner::CargoTest, out).unwrap();
        assert!(got.starts_with("test result: ok"));
        assert!(got.contains("5 passed; 0 failed"));
    }

    #[test]
    fn parse_cargo_fail() {
        let out = "\ntest result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.12s\n";
        let got = parse_failure_summary(TestRunner::CargoTest, out).unwrap();
        assert!(got.contains("FAILED"));
        assert!(got.contains("2 failed"));
    }

    #[test]
    fn parse_maven_surefire() {
        let out = "Running com.ex.FooTest\nTests run: 12, Failures: 1, Errors: 0, Skipped: 0\n";
        let got = parse_failure_summary(TestRunner::MvnTest, out).unwrap();
        assert!(got.contains("Tests run: 12"));
        assert!(got.contains("Failures: 1"));
    }

    #[test]
    fn parse_pytest_mixed() {
        let out = "================= 3 failed, 2 passed, 1 skipped in 0.42s =================\n";
        let got = parse_failure_summary(TestRunner::Pytest, out).unwrap();
        assert!(got.contains("3 failed"));
        assert!(got.contains("2 passed"));
    }

    #[test]
    fn parse_pytest_all_green() {
        let out = "===================== 5 passed in 0.04s =====================\n";
        let got = parse_failure_summary(TestRunner::Pytest, out).unwrap();
        assert!(got.contains("5 passed"));
    }

    #[test]
    fn parse_jest_summary() {
        let out = "Tests:       1 failed, 2 passed, 3 total\nSnapshots:   0 total\n";
        let got = parse_failure_summary(TestRunner::Jest, out).unwrap();
        assert!(got.contains("1 failed"));
        assert!(got.contains("3 total"));
    }

    #[test]
    fn parse_vitest_summary() {
        let out = " Test Files  1 failed | 2 passed (3)\n Tests       5 passed (5)\n";
        let got = parse_failure_summary(TestRunner::Vitest, out).unwrap();
        // Last-occurrence wins → the `Tests` line is picked up.
        assert!(got.contains("passed"));
    }

    #[test]
    fn parse_playwright_counts() {
        let out = "Running 10 tests using 2 workers\n  5 passed (1.2s)\n";
        let got = parse_failure_summary(TestRunner::Playwright, out).unwrap();
        assert!(got.contains("5 passed"));
    }

    #[test]
    fn parse_unknown_format_returns_none() {
        let out = "some totally unrelated output with no test summary line";
        assert!(parse_failure_summary(TestRunner::CargoTest, out).is_none());
    }

    #[test]
    fn parse_custom_runner_always_none() {
        let out = "Tests run: 1, Failures: 0, Errors: 0\n";
        assert!(parse_failure_summary(TestRunner::Custom, out).is_none());
    }

    #[tokio::test]
    async fn custom_runner_end_to_end_success() {
        let dir = tempdir().unwrap();
        let out = TestTool
            .call(
                serde_json::json!({
                    "runner": "custom",
                    "command": "true"
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("RUNNER: custom"));
        assert!(out.summary.contains("PASS"));
        assert!(out.summary.contains("FINAL: exit 0"));
        assert!(out.summary.contains("<untrusted_tool_output tool=\"Test\""));
        // detail_path should exist since `true` writes nothing → actually combined is empty,
        // so write_detail skips. Assert it's None in that case.
        assert!(out.detail_path.is_none());
    }

    #[tokio::test]
    async fn custom_runner_failure_retries_then_gives_up() {
        let dir = tempdir().unwrap();
        let out = TestTool
            .call(
                serde_json::json!({
                    "runner": "custom",
                    "command": "false",
                    "attempts": 3
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("ATTEMPTS: 3/3"));
        assert!(out.summary.contains("FAIL"));
        assert!(out.summary.contains("FINAL: exit 1"));
    }

    #[tokio::test]
    async fn retry_stops_after_first_pass() {
        let dir = tempdir().unwrap();
        let out = TestTool
            .call(
                serde_json::json!({
                    "runner": "custom",
                    "command": "true",
                    "attempts": 3
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(
            out.summary.contains("ATTEMPTS: 1/3"),
            "expected single attempt, got: {}",
            out.summary
        );
    }

    #[tokio::test]
    async fn attempts_clamped_above_3() {
        let dir = tempdir().unwrap();
        let out = TestTool
            .call(
                serde_json::json!({
                    "runner": "custom",
                    "command": "false",
                    "attempts": 99
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("ATTEMPTS: 3/3"));
    }

    #[tokio::test]
    async fn full_log_path_emitted_when_output_present() {
        let dir = tempdir().unwrap();
        let out = TestTool
            .call(
                serde_json::json!({
                    "runner": "custom",
                    "command": "sh -c 'echo hello; exit 0'"
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("FULL LOG:"));
        assert!(out.detail_path.is_some());
    }

    #[tokio::test]
    async fn timeout_fires_on_slow_runner() {
        let dir = tempdir().unwrap();
        let err = TestTool
            .call(
                serde_json::json!({
                    "runner": "custom",
                    "command": "sleep 5",
                    "timeout_secs": 1
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
    }

    #[tokio::test]
    async fn cancel_short_circuits() {
        let dir = tempdir().unwrap();
        let ctx_ = ctx(dir.path());
        let token = ctx_.cancel.clone();
        let handle = tokio::spawn(async move {
            TestTool
                .call(
                    serde_json::json!({
                        "runner": "custom",
                        "command": "sleep 10"
                    }),
                    ctx_,
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
        let err = handle.await.unwrap().unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }

    #[tokio::test]
    async fn custom_without_command_is_validation_error() {
        let dir = tempdir().unwrap();
        let err = TestTool
            .call(serde_json::json!({"runner": "custom"}), ctx(dir.path()))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }
}
