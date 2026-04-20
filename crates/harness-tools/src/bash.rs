use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Env allowlist — PLAN §8.2. Everything else
/// (`ANTHROPIC_API_KEY`, `AWS_*`, `GITHUB_TOKEN`, ...) is stripped from
/// the child env before exec.
pub const DEFAULT_ENV_ALLOW: &[&str] = &["PATH", "HOME", "LANG", "TERM", "USER"];

#[allow(dead_code)]
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const MAX_TIMEOUT_SECS: u64 = 600;

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
}

#[derive(Debug, Default)]
pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "mode": { "type": "string", "enum": ["argv", "shell"], "default": "argv" },
                "timeout_secs": { "type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_SECS },
                "description": { "type": "string" },
                "run_in_background": { "type": "boolean", "default": false }
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

    async fn call(&self, _input: Value, _ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        // Iter 1 body:
        //   parse input; reject Shell unless explicit opt-in;
        //   tokio::process::Command::new(argv[0]).args(...).env_clear()
        //     .envs(filter DEFAULT_ENV_ALLOW from parent env);
        //   setsid -> new pgid (unix pre_exec); on ctx.cancel ->
        //     killpg(-pgid, SIGTERM); 2s later SIGKILL.
        //   stdout+stderr merged, head 4KB + tail 4KB, /tmp path in summary.
        Err(ToolError::Other("Bash::call not yet implemented".into()))
    }
}
