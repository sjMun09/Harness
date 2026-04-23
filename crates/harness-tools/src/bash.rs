//! `Bash` tool — argv-mode default, env allowlist, fresh pgid, cancel-safe
//! graceful kill. PLAN §3.1 / §8.2.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use crate::bg_registry::BgRegistry;
use crate::common::{fence_tool_output, head_tail, parse_input, HEAD_TAIL_CAP};
use crate::proc::graceful_kill_pgid;

/// Env allowlist — PLAN §8.2. Everything else
/// (`ANTHROPIC_API_KEY`, `AWS_*`, `GITHUB_TOKEN`, ...) is stripped from
/// the child env before exec.
///
/// Rule for adding to this list: **paths and non-secret toggles only**.
/// If the variable's *value* could itself be a credential (API key, token,
/// password, vault path, connection string), it must NEVER be allowlisted
/// here — users can whitelist at invocation (`FOO=bar harness ask ...`) or
/// via a per-repo hook.
pub const DEFAULT_ENV_ALLOW: &[&str] = &[
    // core shell / identity
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "TERM",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TZ",
    // XDG base dirs
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    // color / tty hints
    "COLORTERM",
    "NO_COLOR",
    "CLICOLOR",
    "CLICOLOR_FORCE",
    "FORCE_COLOR",
    // ssh-agent — needed for git-over-ssh and ansible ssh-read
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    // git identity (non-secret)
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "GIT_CONFIG_GLOBAL",
    // language toolchain roots — paths only, never credentials
    "JAVA_HOME",
    "MAVEN_HOME",
    "GRADLE_HOME",
    "NODE_ENV",
    "NVM_DIR",
    "PNPM_HOME",
    "VIRTUAL_ENV",
    "PYTHONPATH",
    "PYENV_ROOT",
    "POETRY_HOME",
    "CARGO_HOME",
    "RUSTUP_HOME",
    // docker/k8s context paths (non-secret)
    "DOCKER_HOST",
    "DOCKER_CONFIG",
];

pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const MAX_TIMEOUT_SECS: u64 = 600;

/// Env-var escape hatch that opts a session into `mode: "shell"` execution.
/// Default-deny: shell mode is a pipe/redirect/compound-command superset of
/// the argv mode that shlex-prefix `Bash(...)` rules were designed for — an
/// operator who writes `Bash(git status)` expecting single-command argv
/// dispatch would be surprised to see `git status; rm -rf ~` fall into the
/// same rule bucket.
///
/// Shell-mode commands bypass the argv-level shlex decomposition that
/// permission rules (harness-perm) rely on for prefix matching.
///
/// Kept for headless / CI use — interactive TTY sessions should prefer the
/// per-call `allow_shell_mode` input flag: the model declares shell-mode
/// intent in the `tool_use` JSON, the Ask prompt surfaces the flag, and the
/// operator approves/denies per command. The env var is a process-global
/// yes that cannot be revoked mid-session; the input flag is per-call so
/// logs / hooks can audit each shell composition on its own merits.
pub const SHELL_MODE_ENV_VAR: &str = "HARNESS_BASH_ALLOW_SHELL_MODE";

/// Check the env-var opt-in. Truthy values: `1`, `true`, `yes`, `on`
/// (case-insensitive). Anything else — empty, unset, `0`, `false` — is deny.
fn shell_mode_env_allowed() -> bool {
    std::env::var(SHELL_MODE_ENV_VAR)
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

fn shell_mode_denied_error() -> ToolError {
    ToolError::Validation(format!(
        "Bash `mode: \"shell\"` is disabled. Shell mode (sh -c) bypasses the \
         shlex-prefix permission rules used by `Bash(<prefix>)` — a wildcard \
         `Bash(*)` allow would otherwise cover arbitrary compound commands \
         (`rm -rf ~`, shell redirects, etc.). To opt in, either \
         (a) set input `allow_shell_mode: true` so the Ask prompt can \
         surface the intent per-call, or \
         (b) for headless runs, export `{SHELL_MODE_ENV_VAR}=1`. \
         Otherwise rewrite the command in argv mode (the default)."
    ))
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BashMode {
    /// Direct exec of argv[0] with args — no shell. §8.2 default.
    #[default]
    Argv,
    /// Opt-in `sh -c <command>` mode.
    Shell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashInput {
    pub command: String,
    #[serde(default)]
    pub mode: BashMode,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub run_in_background: bool,
    /// Per-call opt-in for `mode: "shell"`. The model sets this to `true`
    /// when it genuinely needs pipes / redirects / compound commands. The
    /// value is visible to the engine's Ask prompt and to hook scripts so
    /// operators can audit each shell-composition request on its own; a
    /// process-global env-var opt-in (`HARNESS_BASH_ALLOW_SHELL_MODE=1`)
    /// also satisfies the gate for headless / CI runs.
    #[serde(default)]
    pub allow_shell_mode: bool,
    /// Per-call opt-in HOME sandbox. When `true`, the tool creates a fresh
    /// per-call `TempDir` and rewrites `HOME` + the XDG base-dir variables
    /// (`XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`)
    /// in the child env so a model-issued `cat ~/.ssh/id_rsa` (etc.) cannot
    /// reach the real user's dotfiles. Defense-in-depth — the primary gate
    /// is still the Ask/Allow rules for `Bash(...)`. Foreground calls only;
    /// `run_in_background: true` + `sandbox_home: true` is rejected as
    /// Validation (tempdir lifetime vs. bg drainer). See
    /// `docs/security/home-env.md` for the rationale.
    #[serde(default)]
    pub sandbox_home: bool,
}

#[derive(Debug, Default)]
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &'static str {
        "Execute a shell command (argv or shell mode) with a timeout; supports background jobs tracked by shell id."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command":           { "type": "string" },
                "mode":              { "type": "string", "enum": ["argv", "shell"], "default": "argv" },
                "timeout_secs":      { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_SECS },
                "description":       { "type": "string" },
                "run_in_background": { "type": "boolean", "default": false },
                "allow_shell_mode":  {
                    "type": "boolean",
                    "default": false,
                    "description": "Required when mode='shell'. Declare intent per-call so the operator's Ask prompt can audit shell composition (pipes, redirects, compound commands)."
                },
                "sandbox_home": {
                    "type": "boolean",
                    "default": false,
                    "description": "Opt-in HOME sandbox — rewrite $HOME (and XDG_*_HOME) to a fresh per-call tempdir so the child cannot reach ~/.ssh, ~/.aws, ~/.config/gh, dotfile credentials, etc. Foreground only."
                }
            },
            "required": ["command"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<BashInput>(input.clone()) {
            Ok(bi) => {
                let mode = match bi.mode {
                    BashMode::Argv => "argv",
                    BashMode::Shell => "shell",
                };
                Preview {
                    summary_line: format!("Bash[{mode}] {}", bi.command),
                    detail: bi.description,
                }
            }
            Err(e) => Preview {
                summary_line: "Bash <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let bi: BashInput = parse_input(input, "Bash")?;

        // Shell-mode gate: block *before* dispatching to spawn_background so
        // the deny message is uniform for fg + bg callers. Two opt-ins:
        // per-call `allow_shell_mode: true` (preferred for interactive) or
        // the process-global env var (kept for headless / CI).
        if matches!(bi.mode, BashMode::Shell) && !bi.allow_shell_mode && !shell_mode_env_allowed() {
            return Err(shell_mode_denied_error());
        }

        // HOME sandbox is foreground-only — a tempdir must outlive the
        // child, and threading that lifetime through the bg drainer /
        // registry would expand the diff well past defense-in-depth scope.
        if bi.sandbox_home && bi.run_in_background {
            return Err(ToolError::Validation(
                "`sandbox_home: true` is not supported with `run_in_background: true`. \
                 Run the command in the foreground, or omit sandbox_home."
                    .into(),
            ));
        }

        if bi.run_in_background {
            return spawn_background(bi, ctx).await;
        }

        let timeout_secs = bi
            .timeout_secs
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .min(MAX_TIMEOUT_SECS);
        let wait = Duration::from_secs(timeout_secs);

        let mut cmd = match bi.mode {
            BashMode::Argv => build_argv_command(&bi.command)?,
            BashMode::Shell => build_shell_command(&bi.command),
        };
        cmd.current_dir(&ctx.cwd);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        scrub_env(&mut cmd);
        // Keep the guard bound until the child exits — `TempDir::drop`
        // recursively removes the sandbox directory, so letting it drop
        // before `child.wait()` would yank HOME out from under a live
        // process. The `_` prefix silences unused-var warnings in the
        // sandbox-off path without dropping the value.
        let _home_sandbox = if bi.sandbox_home {
            Some(apply_home_sandbox(&mut cmd).map_err(ToolError::Io)?)
        } else {
            None
        };
        set_new_pgid(&mut cmd);

        let mut child = cmd.spawn().map_err(ToolError::Io)?;
        let child_pid = child.id();
        let pgid = child_pid.map(|p| p as i32);

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
                        let detail_path = write_detail(ctx.session_id.as_str(), &combined).await;
                        let header = format!(
                            "exit {} ({} stdout / {} stderr bytes)",
                            status.code().map_or_else(|| "sig".to_string(), |c| c.to_string()),
                            out.len(),
                            err.len(),
                        );
                        let mut summary = head_tail(&combined, HEAD_TAIL_CAP);
                        if !summary.is_empty() {
                            summary = format!("{header}\n{summary}");
                        } else {
                            summary = header;
                        }
                        if let Some(p) = detail_path.as_ref() {
                            summary.push_str(&format!("\n[full log: {}]", p.display()));
                        }
                        let summary = fence_tool_output("Bash", None, &summary);
                        Ok(ToolOutput { summary, detail_path, stream: None })
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
}

fn build_argv_command(command: &str) -> Result<Command, ToolError> {
    let parts = shlex::split(command)
        .ok_or_else(|| ToolError::Validation(format!("bash: could not shlex-split {command:?}")))?;
    if parts.is_empty() {
        return Err(ToolError::Validation("bash: empty command".into()));
    }
    let mut cmd = Command::new(&parts[0]);
    cmd.args(&parts[1..]);
    Ok(cmd)
}

fn build_shell_command(command: &str) -> Command {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(command);
    cmd
}

fn scrub_env(cmd: &mut Command) {
    cmd.env_clear();
    for k in DEFAULT_ENV_ALLOW {
        if let Ok(v) = std::env::var(k) {
            cmd.env(k, v);
        }
    }
}

/// Env vars that should be redirected into the HOME sandbox when
/// `sandbox_home: true`. Covers `HOME` itself plus the XDG base-dir
/// variables that `DEFAULT_ENV_ALLOW` also forwards — if we rewrote `HOME`
/// but left `XDG_CONFIG_HOME=/Users/<me>/.config` untouched, the child
/// could still read `~/.config/gh/hosts.yml` through XDG discovery.
const SANDBOXED_HOME_VARS: &[&str] = &[
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_CACHE_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
];

/// Build a fresh `TempDir` and point `cmd`'s `HOME` (+ XDG base dirs) at
/// it. Callers must keep the returned guard in scope until the spawned
/// child exits — `TempDir::drop` unlinks the directory recursively, which
/// would yank the sandbox out from under a live process.
///
/// Assumes `scrub_env` has already populated the child env: this call
/// overrides the allowlisted values rather than re-building the env.
fn apply_home_sandbox(cmd: &mut Command) -> std::io::Result<tempfile::TempDir> {
    let dir = tempfile::Builder::new()
        .prefix("harness-bash-home-")
        .tempdir()?;
    let path = dir.path();
    for k in SANDBOXED_HOME_VARS {
        cmd.env(k, path);
    }
    Ok(dir)
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

/// Spawn a backgrounded Bash child: new session (`setsid`), piped stdio
/// handed to the BgRegistry drainer, and return `shell_id` immediately.
/// Uses `proc::configure_session_and_pdeathsig` (Linux adds PR_SET_PDEATHSIG)
/// rather than the foreground `process_group(0)` shortcut. PLAN §3.2.
async fn spawn_background(bi: BashInput, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
    let mut cmd = match bi.mode {
        BashMode::Argv => build_argv_command(&bi.command)?,
        BashMode::Shell => build_shell_command(&bi.command),
    };
    cmd.current_dir(&ctx.cwd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    scrub_env(&mut cmd);
    crate::proc::configure_session_and_pdeathsig(&mut cmd);

    let child = cmd
        .spawn()
        .map_err(|e| ToolError::Other(format!("spawn bg bash failed: {e}")))?;
    let pid = child.id().unwrap_or(0);

    // Bg jobs must outlive the spawning tool call — use a standalone cancel
    // token the registry controls via `KillShell`, not `ctx.cancel`.
    let cancel = tokio_util::sync::CancellationToken::new();
    let shell_id = BgRegistry::global().register(bi.command.clone(), child, cancel);

    Ok(ToolOutput {
        summary: format!("started shell_id={shell_id} pid={pid}"),
        detail_path: None,
        stream: None,
    })
}

async fn write_detail(session_id: &str, body: &str) -> Option<PathBuf> {
    if body.is_empty() {
        return None;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let path = std::env::temp_dir().join(format!("harness-bash-{session_id}-{ts}.log"));
    tokio::fs::write(&path, body).await.ok()?;
    Some(path)
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

    /// Every existing shell-mode test relies on `HARNESS_BASH_ALLOW_SHELL_MODE`.
    /// Tests run in parallel by default, so share a mutex so we don't
    /// race with the opt-out test below that wants the var *unset*.
    static SHELL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct ShellEnvGuard {
        prev: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl ShellEnvGuard {
        fn allow() -> Self {
            let lock = SHELL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var_os(SHELL_MODE_ENV_VAR);
            std::env::set_var(SHELL_MODE_ENV_VAR, "1");
            Self { prev, _lock: lock }
        }
        fn deny() -> Self {
            let lock = SHELL_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var_os(SHELL_MODE_ENV_VAR);
            std::env::remove_var(SHELL_MODE_ENV_VAR);
            Self { prev, _lock: lock }
        }
    }
    impl Drop for ShellEnvGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(SHELL_MODE_ENV_VAR, v),
                None => std::env::remove_var(SHELL_MODE_ENV_VAR),
            }
        }
    }

    #[tokio::test]
    async fn echo_hello() {
        let dir = tempdir().unwrap();
        let out = BashTool
            .call(
                serde_json::json!({ "command": "echo hello" }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("hello"));
        assert!(out.summary.contains("exit 0"));
        assert!(out.summary.contains("<untrusted_tool_output tool=\"Bash\""));
    }

    #[tokio::test]
    async fn fence_tag_present_in_bash_output() {
        let dir = tempdir().unwrap();
        let out = BashTool
            .call(
                serde_json::json!({ "command": "echo fenced" }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("<untrusted_tool_output tool=\"Bash\""));
        assert!(out.summary.contains("</untrusted_tool_output>"));
    }

    #[tokio::test]
    async fn env_is_scrubbed() {
        let _shell = ShellEnvGuard::allow();
        std::env::set_var("SECRET_XYZ", "leak-me");
        let dir = tempdir().unwrap();
        let out = BashTool
            .call(
                serde_json::json!({
                    "command": "sh -c 'echo ${SECRET_XYZ:-absent}'",
                    "mode": "shell"
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap();
        assert!(
            out.summary.contains("absent"),
            "scrub failed: {}",
            out.summary
        );
        std::env::remove_var("SECRET_XYZ");
    }

    #[tokio::test]
    async fn timeout_fires_for_slow_command() {
        let _shell = ShellEnvGuard::allow();
        let dir = tempdir().unwrap();
        let err = BashTool
            .call(
                serde_json::json!({
                    "command": "sleep 5",
                    "mode": "shell",
                    "timeout_secs": 1
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Timeout(_)));
    }

    #[tokio::test]
    async fn invalid_shlex_is_validation() {
        let dir = tempdir().unwrap();
        let err = BashTool
            .call(
                serde_json::json!({ "command": "\"unclosed" }),
                ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn cancel_short_circuits() {
        let _shell = ShellEnvGuard::allow();
        let dir = tempdir().unwrap();
        let ctx_ = ctx(dir.path());
        let token = ctx_.cancel.clone();
        let handle = tokio::spawn(async move {
            BashTool
                .call(
                    serde_json::json!({ "command": "sleep 10", "mode": "shell" }),
                    ctx_,
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
        let err = handle.await.unwrap().unwrap_err();
        assert!(matches!(err, ToolError::Cancelled));
    }

    fn parse_shell_id(summary: &str) -> String {
        let prefix = "started shell_id=";
        let after = &summary[summary.find(prefix).unwrap() + prefix.len()..];
        let end = after.find(' ').unwrap_or(after.len());
        after[..end].to_string()
    }

    fn parse_pid(summary: &str) -> i32 {
        let key = "pid=";
        let after = &summary[summary.find(key).unwrap() + key.len()..];
        let end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        after[..end].parse().unwrap_or(0)
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bg_spawn_returns_shell_id() {
        let _shell = ShellEnvGuard::allow();
        let dir = tempdir().unwrap();
        let started = std::time::Instant::now();
        let out = BashTool
            .call(
                serde_json::json!({
                    "command": "sh -c 'echo hello; sleep 0.1'",
                    "mode": "shell",
                    "run_in_background": true
                }),
                ctx(dir.path()),
            )
            .await
            .expect("bg spawn ok");
        let elapsed = started.elapsed();
        assert!(out.summary.starts_with("started shell_id="));
        assert!(
            elapsed < Duration::from_millis(500),
            "bg spawn was slow: {elapsed:?}"
        );
        let id = parse_shell_id(&out.summary);
        let _ = crate::bg_registry::BgRegistry::global().kill(&id);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_output_drains_new_output() {
        let _shell = ShellEnvGuard::allow();
        let dir = tempdir().unwrap();
        let out = BashTool
            .call(
                serde_json::json!({
                    "command": "sh -c 'for i in 1 2 3; do echo $i; sleep 0.05; done'",
                    "mode": "shell",
                    "run_in_background": true
                }),
                ctx(dir.path()),
            )
            .await
            .expect("bg spawn");
        let id = parse_shell_id(&out.summary);

        let bo = crate::bash_output::BashOutputTool::default();
        tokio::time::sleep(Duration::from_millis(120)).await;
        let r1 = bo
            .call(serde_json::json!({ "shell_id": id }), ctx(dir.path()))
            .await
            .expect("first poll");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let r2 = bo
            .call(serde_json::json!({ "shell_id": id }), ctx(dir.path()))
            .await
            .expect("second poll");

        let combined = format!("{}{}", r1.summary, r2.summary);
        for tok in ["1", "2", "3"] {
            assert!(combined.contains(tok), "missing {tok} in {combined}");
        }
        assert!(
            combined.contains("exited"),
            "should have exited: {combined}"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_output_filter_regex() {
        let _shell = ShellEnvGuard::allow();
        let dir = tempdir().unwrap();
        let out = BashTool
            .call(
                serde_json::json!({
                    "command": "sh -c 'printf \"ok\\nerror: x\\nok\\n\"'",
                    "mode": "shell",
                    "run_in_background": true
                }),
                ctx(dir.path()),
            )
            .await
            .expect("bg spawn");
        let id = parse_shell_id(&out.summary);
        tokio::time::sleep(Duration::from_millis(150)).await;

        let bo = crate::bash_output::BashOutputTool::default();
        let res = bo
            .call(
                serde_json::json!({ "shell_id": id, "filter": "^error" }),
                ctx(dir.path()),
            )
            .await
            .expect("filter poll");
        assert!(res.summary.contains("error: x"));
        assert!(
            !res.summary.contains("\nok\n"),
            "ok lines leaked: {}",
            res.summary
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kill_shell_terminates_child() {
        let _shell = ShellEnvGuard::allow();
        let dir = tempdir().unwrap();
        let out = BashTool
            .call(
                serde_json::json!({
                    "command": "sh -c 'sleep 30'",
                    "mode": "shell",
                    "run_in_background": true
                }),
                ctx(dir.path()),
            )
            .await
            .expect("bg spawn");
        let id = parse_shell_id(&out.summary);
        let pid: i32 = parse_pid(&out.summary);

        let ks = crate::kill_shell::KillShellTool::default();
        let res = ks
            .call(serde_json::json!({ "shell_id": id }), ctx(dir.path()))
            .await
            .expect("kill ok");
        assert!(res.summary.starts_with("killed shell_id="));

        let bo = crate::bash_output::BashOutputTool::default();
        let mut killed = false;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let r = bo
                .call(serde_json::json!({ "shell_id": id }), ctx(dir.path()))
                .await
                .expect("poll");
            if r.summary.contains("status=killed") {
                killed = true;
                break;
            }
        }
        assert!(killed, "job never reported killed status");

        if pid > 0 {
            use nix::sys::signal::kill;
            use nix::unistd::Pid;
            let r = kill(Pid::from_raw(pid), None);
            assert!(r.is_err(), "pid {pid} still alive after KillShell");
        }
    }

    /// Default state: shell mode must be rejected without the env opt-in,
    /// even when the command itself is harmless. The error must be
    /// `Validation` so hooks / logs can distinguish a denied invocation
    /// from an IO / timeout failure.
    #[tokio::test]
    async fn shell_mode_denied_by_default() {
        let _shell = ShellEnvGuard::deny();
        let dir = tempdir().unwrap();
        let err = BashTool
            .call(
                serde_json::json!({
                    "command": "echo hi",
                    "mode": "shell",
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(
                    msg.contains(SHELL_MODE_ENV_VAR),
                    "error should reference the env var: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /// The `run_in_background` path must apply the same gate — operators
    /// can't sidestep the deny by flipping the background flag.
    #[tokio::test]
    async fn shell_mode_denied_in_background_too() {
        let _shell = ShellEnvGuard::deny();
        let dir = tempdir().unwrap();
        let err = BashTool
            .call(
                serde_json::json!({
                    "command": "echo hi",
                    "mode": "shell",
                    "run_in_background": true,
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    /// Per-call opt-in via `allow_shell_mode: true` must satisfy the gate
    /// even when the env var is off — this is the path an interactive
    /// operator approves through the Ask prompt.
    #[tokio::test]
    async fn shell_mode_allowed_per_call_flag() {
        let _shell = ShellEnvGuard::deny();
        let dir = tempdir().unwrap();
        let out = BashTool
            .call(
                serde_json::json!({
                    "command": "echo per-call",
                    "mode": "shell",
                    "allow_shell_mode": true,
                }),
                ctx(dir.path()),
            )
            .await
            .expect("per-call flag should open the gate");
        assert!(out.summary.contains("per-call"));
        assert!(out.summary.contains("exit 0"));
    }

    /// Backward-compat: the env var alone (no per-call flag) still works
    /// for headless runs.
    #[tokio::test]
    async fn shell_mode_allowed_with_env() {
        let _shell = ShellEnvGuard::allow();
        let dir = tempdir().unwrap();
        let out = BashTool
            .call(
                serde_json::json!({
                    "command": "echo opted-in",
                    "mode": "shell",
                }),
                ctx(dir.path()),
            )
            .await
            .expect("shell mode allowed with env var");
        assert!(out.summary.contains("opted-in"));
        assert!(out.summary.contains("exit 0"));
    }

    /// Argv mode is the default and must keep working with the env var unset —
    /// the gate is specifically for `mode: "shell"` invocations.
    #[tokio::test]
    async fn argv_mode_unaffected_by_shell_gate() {
        let _shell = ShellEnvGuard::deny();
        let dir = tempdir().unwrap();
        let out = BashTool
            .call(
                serde_json::json!({ "command": "echo argv-ok" }),
                ctx(dir.path()),
            )
            .await
            .expect("argv mode must work with shell gate closed");
        assert!(out.summary.contains("argv-ok"));
    }

    /// `HOME` mutation races the same way as the shell-mode env var —
    /// serialize via a mutex so parallel tests don't step on each other.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct HomeGuard {
        prev: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl HomeGuard {
        fn set(new_home: &Path) -> Self {
            let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var_os("HOME");
            std::env::set_var("HOME", new_home);
            Self { prev, _lock: lock }
        }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// Sandbox off (default): the child observes the real `$HOME`. Keeping
    /// `$HOME` on the allowlist is the whole point of the "opt-in" framing —
    /// if this flips silently, most dev toolchains (git global config, cargo,
    /// npm, ...) would break.
    #[cfg(unix)]
    #[tokio::test]
    async fn sandbox_home_off_passes_real_home_through() {
        let _shell = ShellEnvGuard::allow();
        let real_home = tempdir().unwrap();
        let _h = HomeGuard::set(real_home.path());
        let dir = tempdir().unwrap();
        let out = BashTool
            .call(
                serde_json::json!({
                    "command": "sh -c 'echo $HOME'",
                    "mode": "shell",
                }),
                ctx(dir.path()),
            )
            .await
            .expect("sandbox-off should run");
        // Child must print the real HOME that the parent had set.
        let needle = real_home.path().to_string_lossy().into_owned();
        assert!(
            out.summary.contains(needle.as_str()),
            "expected child HOME={needle}, got: {}",
            out.summary
        );
    }

    /// Sandbox on: the child sees a fresh tempdir for `$HOME` — different
    /// from the parent's `$HOME` — and `~/.ssh` under that tempdir contains
    /// nothing. The "nothing found" assertion is the crux: a model-issued
    /// `cat ~/.ssh/id_rsa` becomes a deterministic miss.
    #[cfg(unix)]
    #[tokio::test]
    async fn sandbox_home_on_rewrites_home_to_tempdir() {
        let _shell = ShellEnvGuard::allow();
        // Pretend the real user has a populated ~/.ssh — the sandbox must
        // mask this from the child.
        let real_home = tempdir().unwrap();
        let real_ssh = real_home.path().join(".ssh");
        std::fs::create_dir_all(&real_ssh).unwrap();
        std::fs::write(
            real_ssh.join("id_rsa"),
            "-----BEGIN OPENSSH PRIVATE KEY-----\n",
        )
        .unwrap();
        let _h = HomeGuard::set(real_home.path());
        let dir = tempdir().unwrap();

        let out = BashTool
            .call(
                serde_json::json!({
                    "command": "sh -c 'echo HOME=$HOME; ls -A $HOME/.ssh 2>/dev/null | wc -l'",
                    "mode": "shell",
                    "sandbox_home": true,
                }),
                ctx(dir.path()),
            )
            .await
            .expect("sandbox-on should run");

        // Child HOME must NOT be the real one.
        let real = real_home.path().to_string_lossy().into_owned();
        let home_line = out
            .summary
            .lines()
            .find(|l| l.starts_with("HOME="))
            .expect("expected HOME= line in output");
        assert!(
            !home_line.contains(real.as_str()),
            "sandbox leaked real HOME: {home_line}"
        );
        // Child HOME must be a tempdir path (macOS wraps TMPDIR, Linux
        // typically /tmp — accept either `/var/folders/` or `/tmp/`).
        assert!(
            home_line.contains("harness-bash-home-"),
            "HOME does not point at the sandbox prefix: {home_line}"
        );
        // `ls ~/.ssh` inside the child must find zero entries. `wc -l`
        // pads its output with whitespace on BSD and strips it on GNU;
        // normalize by trimming whitespace before checking.
        let wc_line = out
            .summary
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .next()
            .expect("expected a numeric wc -l line in output");
        assert_eq!(
            wc_line, 0,
            "expected empty .ssh listing, got count {wc_line}: {}",
            out.summary
        );
    }

    /// `sandbox_home: true` + `run_in_background: true` must be rejected
    /// up-front — the tempdir lifetime is tied to the foreground call and
    /// silently ignoring the flag in bg mode would be a worse bug than the
    /// one this flag is defending against.
    #[tokio::test]
    async fn sandbox_home_rejected_with_run_in_background() {
        let dir = tempdir().unwrap();
        let err = BashTool
            .call(
                serde_json::json!({
                    "command": "echo hi",
                    "sandbox_home": true,
                    "run_in_background": true,
                }),
                ctx(dir.path()),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::Validation(msg) => {
                assert!(
                    msg.contains("sandbox_home") && msg.contains("background"),
                    "error must name both flags: {msg}"
                );
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /// Argv mode must also honor the sandbox — many real commands (`git`,
    /// `cargo`) go through argv, so shell-only protection would be a gap.
    #[cfg(unix)]
    #[tokio::test]
    async fn sandbox_home_works_in_argv_mode() {
        let _shell = ShellEnvGuard::deny();
        let real_home = tempdir().unwrap();
        let _h = HomeGuard::set(real_home.path());
        let dir = tempdir().unwrap();

        // `printenv HOME` — pure argv, no shell.
        let out = BashTool
            .call(
                serde_json::json!({
                    "command": "printenv HOME",
                    "sandbox_home": true,
                }),
                ctx(dir.path()),
            )
            .await
            .expect("argv-mode sandbox should run");

        let real = real_home.path().to_string_lossy().into_owned();
        assert!(
            !out.summary.contains(real.as_str()),
            "argv-mode sandbox leaked real HOME: {}",
            out.summary
        );
        assert!(
            out.summary.contains("harness-bash-home-"),
            "argv-mode HOME does not point at sandbox prefix: {}",
            out.summary
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bg_registry_unknown_shell_id_errors() {
        let dir = tempdir().unwrap();
        let bo = crate::bash_output::BashOutputTool::default();
        let r = bo
            .call(
                serde_json::json!({ "shell_id": "bash_does_not_exist" }),
                ctx(dir.path()),
            )
            .await;
        assert!(r.is_err(), "expected Err for unknown shell_id");

        let ks = crate::kill_shell::KillShellTool::default();
        let r2 = ks
            .call(
                serde_json::json!({ "shell_id": "bash_does_not_exist" }),
                ctx(dir.path()),
            )
            .await;
        assert!(r2.is_err(), "expected Err for unknown shell_id");
    }
}
