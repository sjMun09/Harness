//! End-to-end integration tests for `harness ask`.
//!
//! These tests spin up an in-process fake Anthropic Messages API server
//! (hand-rolled HTTP/1.1 over `tokio::net::TcpListener` — no extra HTTP
//! framework dependency beyond what the workspace already pulls in) and
//! run the compiled `harness` binary against it via `assert_cmd`.
//!
//! The tests exercise the full binary: argument parsing, provider wiring,
//! SSE parsing, turn-loop tool dispatch, and line-mode rendering. The fake
//! server speaks just enough of Anthropic's SSE wire protocol for the turn
//! loop to reach a natural stop.
//!
//! Isolation: each test sets `HOME` / `XDG_*` / `ANTHROPIC_API_KEY` /
//! `HARNESS_LOG` explicitly so it cannot touch the developer's real
//! credentials or session store.

#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;

use assert_cmd::cargo::CommandCargoExt;
use tempfile::TempDir;

mod fake_anthropic;
#[allow(unused_imports)]
use fake_anthropic::{FakeServer, Script};

/// Wait this long for the child process to finish before concluding the test
/// has deadlocked. Generous because debug builds are slow to link.
const CHILD_TIMEOUT: Duration = Duration::from_secs(30);

/// Guard that reaps a child process on Drop so a panic in the middle of a
/// test never leaves a zombie `harness` behind.
struct ChildGuard(Option<std::process::Child>);

impl ChildGuard {
    fn new(c: std::process::Child) -> Self {
        Self(Some(c))
    }
    fn take(mut self) -> std::process::Child {
        self.0.take().expect("child already taken")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Build a `std::process::Command` for the `harness` binary with a pristine
/// environment pointing at `tmp` for HOME/XDG state and at `server_url` for
/// the provider base URL.
fn harness_cmd(tmp: &TempDir, server_url: &str) -> StdCommand {
    let mut cmd = StdCommand::cargo_bin("harness").expect("cargo_bin harness");
    // Scrub leaked env — assert_cmd carries over the parent env by default.
    for key in [
        "OPENAI_API_KEY",
        "HARNESS_ANTHROPIC_BASE_URL",
        "HARNESS_MODEL",
        "HARNESS_LOG",
        "RUST_LOG",
    ] {
        cmd.env_remove(key);
    }
    cmd.env("ANTHROPIC_API_KEY", "test-fake-key")
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path().join("config"))
        .env("XDG_DATA_HOME", tmp.path().join("data"))
        .env("XDG_STATE_HOME", tmp.path().join("state"))
        // Pin the working dir so the CLI does not read the outer repo's
        // `.harness/settings.json`.
        .current_dir(tmp.path())
        .arg("--trust-cwd")
        .arg("--dangerously-skip-permissions")
        .arg("--auth")
        .arg("api-key")
        .arg("--base-url")
        .arg(server_url);
    cmd
}

fn wait_with_timeout(mut child: std::process::Child) -> std::process::Output {
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child.wait_with_output().expect("wait_with_output");
            }
            Ok(None) => {
                if start.elapsed() >= CHILD_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("child did not finish within {CHILD_TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Test 1 — final assistant text echoed to stdout.
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn ask_returns_final_assistant_text() {
    let tmp = TempDir::new().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    // One HTTP call → one scripted SSE stream with a single text block.
    let (server, addr) =
        rt.block_on(async { FakeServer::start(vec![Script::text_only("hello from fake")]).await });

    let url = format!("http://{addr}");
    let mut cmd = harness_cmd(&tmp, &url);
    cmd.arg("ask").arg("hi");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let child = cmd.spawn().expect("spawn harness");
    let guard = ChildGuard::new(child);
    let output = wait_with_timeout(guard.take());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "exit={:?} stdout={stdout} stderr={stderr}",
        output.status
    );
    assert!(
        stdout.contains("hello from fake"),
        "expected final text in stdout, got: {stdout}\nstderr={stderr}"
    );

    // Keep the server alive until we've collected output; shut it down now.
    rt.block_on(server.shutdown());
}

// ────────────────────────────────────────────────────────────────────────────
// Test 2 — server requests a Read tool call, harness executes it, server
// then returns final text. Assert both (a) final text visible on stdout and
// (b) the line-mode "⏺ Read(...)" marker emitted on stderr.
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn ask_executes_tool_call_then_final_text() {
    let tmp = TempDir::new().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    // Pre-seed a file the Read tool is allowed to open. It must live under
    // cwd because fs_safe::canonicalize_within rejects paths outside.
    let target_file = tmp.path().join("e2e.txt");
    std::fs::write(&target_file, "e2e payload\n").unwrap();
    let target_rel = "e2e.txt"; // relative path resolves under cwd=tmp

    let (server, addr) = rt.block_on(async {
        FakeServer::start(vec![
            Script::tool_use(
                "toolu_1",
                "Read",
                &format!(r#"{{"file_path":"{target_rel}"}}"#),
            ),
            Script::text_only("done"),
        ])
        .await
    });

    let url = format!("http://{addr}");
    let mut cmd = harness_cmd(&tmp, &url);
    cmd.arg("ask").arg("please read the file");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let child = cmd.spawn().expect("spawn harness");
    let guard = ChildGuard::new(child);
    let output = wait_with_timeout(guard.take());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "exit={:?} stdout={stdout}\n--stderr--\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("done"),
        "expected final text in stdout, got: {stdout}\nstderr={stderr}"
    );
    // Line-mode marker lives on stderr. Look for the `⏺ Read(` prefix; the
    // suffix is the preview produced by ReadTool::preview (`Read <file>`).
    assert!(
        stderr.contains("⏺ Read("),
        "expected ⏺ Read(...) marker on stderr, got: {stderr}"
    );

    rt.block_on(server.shutdown());
}

// ────────────────────────────────────────────────────────────────────────────
// Test — `harness ask -` reads the prompt from stdin (hyphen sentinel).
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn ask_reads_prompt_from_stdin_hyphen() {
    let tmp = TempDir::new().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    let (server, addr) =
        rt.block_on(async { FakeServer::start(vec![Script::text_only("stdin ok")]).await });

    let url = format!("http://{addr}");
    let mut cmd = harness_cmd(&tmp, &url);
    cmd.arg("ask").arg("-");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn harness");
    {
        let mut si = child.stdin.take().expect("stdin pipe");
        si.write_all(b"please explain this long prompt from a file\n")
            .unwrap();
    }
    let output = wait_with_timeout(child);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "status={:?} stdout={stdout} stderr={stderr}",
        output.status
    );
    assert!(stdout.contains("stdin ok"), "stdout was: {stdout}");

    rt.block_on(server.shutdown());
}

// ────────────────────────────────────────────────────────────────────────────
// Test — `harness ask` with no positional arg auto-reads piped stdin.
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn ask_auto_reads_piped_stdin_without_hyphen() {
    let tmp = TempDir::new().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    let (server, addr) =
        rt.block_on(async { FakeServer::start(vec![Script::text_only("auto ok")]).await });

    let url = format!("http://{addr}");
    let mut cmd = harness_cmd(&tmp, &url);
    cmd.arg("ask");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn harness");
    {
        let mut si = child.stdin.take().expect("stdin pipe");
        si.write_all(b"auto-detected prompt body\n").unwrap();
    }
    let output = wait_with_timeout(child);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "status={:?} stdout={stdout} stderr={stderr}",
        output.status
    );
    assert!(stdout.contains("auto ok"), "stdout was: {stdout}");

    rt.block_on(server.shutdown());
}

// ────────────────────────────────────────────────────────────────────────────
// Test — `harness ask -` with empty stdin fails with a clear message and does
// not reach the provider.
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn ask_empty_stdin_errors_out() {
    let tmp = TempDir::new().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    let (server, addr) =
        rt.block_on(async { FakeServer::start(vec![Script::text_only("unreachable")]).await });

    let url = format!("http://{addr}");
    let mut cmd = harness_cmd(&tmp, &url);
    cmd.arg("ask").arg("-");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn harness");
    {
        let mut si = child.stdin.take().expect("stdin pipe");
        si.write_all(b"   \n\t\n").unwrap();
    }
    let output = wait_with_timeout(child);

    let stderr = String::from_utf8_lossy(&output.stderr);
    // anyhow error path maps to ExitCode::FAILURE (=1); verify we are *not*
    // getting 130 (SIGINT) or any other code by accident.
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1 (ExitCode::FAILURE), got {:?}; stderr={stderr}",
        output.status.code()
    );
    assert!(
        stderr.contains("stdin prompt was empty"),
        "expected stdin-empty error on stderr, got: {stderr}"
    );

    rt.block_on(server.shutdown());
}

// ────────────────────────────────────────────────────────────────────────────
// Test — `--metrics-json PATH` writes a strict 14-field run-metrics record
// with correct types after the turn loop finishes.
// (13 schema fields + ensure `wall_ms` is non-negative and `input_tokens`
// reflects the fake server's `usage.input_tokens`.)
// ────────────────────────────────────────────────────────────────────────────

#[test]
fn ask_writes_metrics_json_on_success() {
    let tmp = TempDir::new().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    let (server, addr) =
        rt.block_on(async { FakeServer::start(vec![Script::text_only("metrics ok")]).await });

    let metrics_path = tmp.path().join("metrics.json");
    let url = format!("http://{addr}");
    let mut cmd = harness_cmd(&tmp, &url);
    cmd.arg("ask")
        .arg("--metrics-json")
        .arg(&metrics_path)
        .arg("hi");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let child = cmd.spawn().expect("spawn harness");
    let guard = ChildGuard::new(child);
    let output = wait_with_timeout(guard.take());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "exit={:?} stdout={stdout} stderr={stderr}",
        output.status
    );

    // The metrics file must exist (atomic rename happened) and the `.tmp`
    // sibling must NOT — write_atomic cleaned up.
    assert!(
        metrics_path.exists(),
        "metrics.json missing at {}",
        metrics_path.display()
    );
    assert!(
        !tmp.path().join("metrics.json.tmp").exists(),
        "leftover .tmp file after atomic rename"
    );

    let body = std::fs::read_to_string(&metrics_path).expect("read metrics.json");
    let v: serde_json::Value = serde_json::from_str(&body).expect("parse metrics.json");

    // All 13 schema fields present with correct types. `cache_*` may be
    // either integer or null per spec.
    assert_eq!(v["schema_version"], 1, "schema_version");
    assert_eq!(v["tool"], "harness", "tool");
    assert!(v["model"].is_string(), "model not string: {:?}", v["model"]);
    assert_eq!(v["provider"], "anthropic", "provider");
    assert!(
        v["wall_ms"].is_number(),
        "wall_ms not number: {:?}",
        v["wall_ms"]
    );
    assert!(
        v["api_ms"].is_null() || v["api_ms"].is_number(),
        "api_ms bad type: {:?}",
        v["api_ms"]
    );
    assert_eq!(v["exit_code"], 0, "exit_code");
    assert!(
        v["input_tokens"].is_number(),
        "input_tokens not number: {:?}",
        v["input_tokens"]
    );
    assert!(
        v["output_tokens"].is_number(),
        "output_tokens not number: {:?}",
        v["output_tokens"]
    );
    let cr = &v["cache_read_tokens"];
    assert!(
        cr.is_null() || cr.is_number(),
        "cache_read_tokens bad type: {cr:?}"
    );
    let cc = &v["cache_creation_tokens"];
    assert!(
        cc.is_null() || cc.is_number(),
        "cache_creation_tokens bad type: {cc:?}"
    );
    assert!(
        v["num_turns"].is_number(),
        "num_turns not number: {:?}",
        v["num_turns"]
    );
    // prompt_sha256("hi") per the stdlib sha256 vector.
    assert_eq!(
        v["prompt_sha256"],
        "8f434346648f6b96df89dda901c5176b10a6d83961dd3c1ac88b59b2dc327aa4",
        "prompt_sha256 mismatch"
    );
    assert!(
        v["session_id"].is_string(),
        "session_id not string: {:?}",
        v["session_id"]
    );

    // Fake server reports usage {input_tokens:1} on message_start and
    // {output_tokens:5} on message_delta → exactly one assistant turn.
    assert_eq!(v["input_tokens"].as_u64(), Some(1), "input_tokens value");
    assert_eq!(v["output_tokens"].as_u64(), Some(5), "output_tokens value");
    assert_eq!(v["num_turns"].as_u64(), Some(1), "num_turns value");

    rt.block_on(server.shutdown());
}

// ────────────────────────────────────────────────────────────────────────────
// Test 3 — SIGINT on the child yields exit code 130. Unix-only because
// `nix::sys::signal::kill` does not apply on Windows.
// ────────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn ask_cancels_on_sigint() {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let tmp = TempDir::new().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    // First turn asks for a Bash tool_use that sleeps — the engine will sit
    // inside the tool dispatch long enough for us to SIGINT it.
    let (server, addr) = rt.block_on(async {
        FakeServer::start(vec![Script::tool_use(
            "toolu_sleep",
            "Bash",
            r#"{"command":"sleep 20"}"#,
        )])
        .await
    });

    let url = format!("http://{addr}");
    let mut cmd = harness_cmd(&tmp, &url);
    cmd.arg("ask").arg("sleep for a while please");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn harness");
    let pid = Pid::from_raw(child.id() as i32);

    // Wait for the child to reach the tool-dispatch point. We detect this by
    // tailing stderr until we see the line-mode `⏺ Bash(` marker.
    let mut buf = [0u8; 4096];
    let mut seen = String::new();
    let start = std::time::Instant::now();
    let stderr = child.stderr.as_mut().expect("stderr pipe");
    while start.elapsed() < Duration::from_secs(20) && !seen.contains("⏺ Bash(") {
        match stderr.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => panic!("stderr read: {e}"),
        }
    }
    assert!(
        seen.contains("⏺ Bash("),
        "did not see Bash tool start on stderr before timing out: {seen}"
    );

    // Fire SIGINT — the CLI's ctrl_c watcher cancels the turn.
    kill(pid, Signal::SIGINT).expect("kill SIGINT");

    let output = wait_with_timeout({
        // Rebuild an owned child, since `wait_with_output` needs ownership.
        // `spawn()` already put stdout/stderr into piped mode; we drained
        // some of stderr above, which is fine — the rest will appear in
        // output.stderr below.
        child
    });

    let stderr_all = {
        let mut s = seen.clone();
        s.push_str(&String::from_utf8_lossy(&output.stderr));
        s
    };
    let code = output.status.code();
    assert_eq!(
        code,
        Some(130),
        "expected exit 130 (SIGINT), got {code:?}\nstderr={stderr_all}"
    );

    rt.block_on(server.shutdown());
}
