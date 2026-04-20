//! `KillShell` tool — terminate a background shell. PLAN §3.2.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
//!
//! Trips the cancel token on the registered job; the drainer task observes
//! it and runs the SIGTERM → SIGKILL escalation via `proc::graceful_kill_pgid`.

use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::bg_registry::BgRegistry;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KillShellInput {
    pub shell_id: String,
}

#[derive(Debug, Default)]
pub struct KillShellTool;

#[async_trait]
impl Tool for KillShellTool {
    fn name(&self) -> &str {
        "KillShell"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "shell_id": { "type": "string" }
            },
            "required": ["shell_id"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<KillShellInput>(input.clone()) {
            Ok(ks) => Preview {
                summary_line: format!("KillShell shell_id={}", ks.shell_id),
                detail: None,
            },
            Err(e) => Preview {
                summary_line: "KillShell <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, input: Value, _ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let parsed: KillShellInput = serde_json::from_value(input)
            .map_err(|e| ToolError::Validation(format!("KillShell input: {e}")))?;

        let registry = BgRegistry::global();
        registry
            .kill(&parsed.shell_id)
            .map_err(|_| ToolError::Validation(format!("unknown shell_id: {}", parsed.shell_id)))?;

        Ok(ToolOutput {
            summary: format!("killed shell_id={}", parsed.shell_id),
            detail_path: None,
            stream: None,
        })
    }
}
