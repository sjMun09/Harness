//! `Subagent` tool — delegate a scoped sub-turn. PLAN §5.4.
//!
//! The tool enforces the kernel contract and delegates actual execution to
//! `ToolCtx::subagent` (an `Arc<dyn SubagentHost>`). The kernel lives in
//! `harness-core`; the host impl lives in `harness-cli` (it needs `Provider`
//! + `run_turn`, which would be a dep cycle here).
//!
//! Enforcement:
//!   - `ctx.depth >= SUBAGENT_MAX_DEPTH` → Validation error, refuse to spawn.
//!   - `ctx.subagent == None` → Validation error ("no host wired").
//!   - `tools_allowlist` is sanitized via `sanitize_allowlist()` — Bash +
//!     Subagent are always stripped.
//!   - Final `text` is capped at `SUBAGENT_OUTPUT_CAP` bytes with a truncation
//!     marker that includes the sub-session id so the user can find the full
//!     transcript.

use async_trait::async_trait;
use harness_core::subagent::{
    cap_output, sanitize_allowlist, SubagentError, SubagentSpec, SUBAGENT_MAX_DEPTH,
    SUBAGENT_OUTPUT_CAP, SUBAGENT_TOOL_NAME,
};
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::parse_input;

/// Safety ceiling on turn loops the spawned sub-agent may execute. Keeps a
/// malformed prompt from running unbounded even if the caller omits the field.
pub const DEFAULT_SUBAGENT_MAX_TURNS: u32 = 10;
pub const MAX_SUBAGENT_MAX_TURNS: u32 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentInput {
    /// The research / exploration prompt the sub-agent will be given as its
    /// first user message.
    pub prompt: String,
    /// Tool names the sub-agent may call. Bash + Subagent are always stripped
    /// regardless of what the caller requests. Omit to use the default
    /// read-only set (Read / Glob / Grep / ImportTrace / MyBatisDynamicParser).
    #[serde(default)]
    pub tools_allowlist: Option<Vec<String>>,
    /// Turn-loop cap. Defaults to `DEFAULT_SUBAGENT_MAX_TURNS`, hard-capped at
    /// `MAX_SUBAGENT_MAX_TURNS`.
    #[serde(default)]
    pub max_turns: Option<u32>,
}

#[derive(Debug, Default)]
pub struct SubagentTool;

#[async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        SUBAGENT_TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Spawn a depth-capped sub-agent to research a focused question and return a short summary without polluting the parent context."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "description": "Spawn a depth-capped sub-agent to research a focused question without polluting the parent context. Output is capped at 2 KiB; the full transcript is written to the sub-session JSONL.",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The research question or task for the sub-agent."
                },
                "tools_allowlist": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Tool names the sub-agent may call. Bash + Subagent are always stripped. Defaults to Read/Glob/Grep/ImportTrace/MyBatisDynamicParser."
                },
                "max_turns": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_SUBAGENT_MAX_TURNS,
                    "description": "Turn-loop cap. Default 10, max 30."
                }
            },
            "required": ["prompt"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<SubagentInput>(input.clone()) {
            Ok(si) => {
                let head: String = si.prompt.chars().take(80).collect();
                Preview {
                    summary_line: format!("Subagent({head})"),
                    detail: None,
                }
            }
            Err(e) => Preview {
                summary_line: "Subagent <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let si: SubagentInput = parse_input(input, SUBAGENT_TOOL_NAME)?;

        if ctx.depth >= SUBAGENT_MAX_DEPTH {
            return Err(ToolError::Validation(format!(
                "subagent depth cap reached (max {SUBAGENT_MAX_DEPTH}) — a sub-agent cannot spawn further sub-agents"
            )));
        }

        let host = ctx
            .subagent
            .as_ref()
            .ok_or_else(|| {
                ToolError::Validation(
                    "no subagent host wired — this binary was built without Subagent support"
                        .into(),
                )
            })?
            .clone();

        let tools_allowlist = sanitize_allowlist(si.tools_allowlist);
        let max_turns = si
            .max_turns
            .unwrap_or(DEFAULT_SUBAGENT_MAX_TURNS)
            .min(MAX_SUBAGENT_MAX_TURNS);

        let spec = SubagentSpec {
            prompt: si.prompt,
            tools_allowlist,
            max_turns,
            parent_session: ctx.session_id.to_string(),
            depth: ctx.depth + 1,
        };

        let result = host
            .spawn(spec, ctx.cancel.clone())
            .await
            .map_err(|e| match e {
                SubagentError::DepthCap => ToolError::Validation(e.to_string()),
                SubagentError::NoHost => ToolError::Validation(e.to_string()),
                SubagentError::Execution(s) => ToolError::Other(s),
            })?;

        let (capped, dropped) = cap_output(result.text, SUBAGENT_OUTPUT_CAP);
        let mut summary = capped;
        if dropped > 0 {
            summary.push_str("\n[sub-session: ");
            summary.push_str(&result.sub_session_id);
            summary.push(']');
        } else {
            // Always emit the sub-session id on success so the user can locate
            // the full transcript even when the cap didn't trigger.
            summary.push_str("\n[sub-session: ");
            summary.push_str(&result.sub_session_id);
            summary.push(']');
        }

        Ok(ToolOutput {
            summary,
            detail_path: None,
            stream: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::subagent::{SubagentHost, SubagentResult};
    use harness_core::HookDispatcher;
    use harness_perm::PermissionSnapshot;
    use harness_proto::SessionId;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[derive(Debug)]
    struct StubHost {
        reply: String,
        sub_session_id: String,
    }

    #[async_trait]
    impl SubagentHost for StubHost {
        async fn spawn(
            &self,
            _spec: SubagentSpec,
            _cancel: CancellationToken,
        ) -> Result<SubagentResult, SubagentError> {
            Ok(SubagentResult {
                text: self.reply.clone(),
                tool_calls: 0,
                input_tokens: 0,
                output_tokens: 0,
                sub_session_id: self.sub_session_id.clone(),
            })
        }
    }

    fn ctx_with(host: Option<Arc<dyn SubagentHost>>, depth: u32) -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            session_id: SessionId::new("parent"),
            cancel: CancellationToken::new(),
            permission: PermissionSnapshot::default(),
            hooks: HookDispatcher::default(),
            subagent: host,
            depth,
            tx: None,
        }
    }

    #[tokio::test]
    async fn rejects_when_depth_at_cap() {
        let host: Arc<dyn SubagentHost> = Arc::new(StubHost {
            reply: "ignored".into(),
            sub_session_id: "s".into(),
        });
        let err = SubagentTool
            .call(
                serde_json::json!({"prompt": "hi"}),
                ctx_with(Some(host), SUBAGENT_MAX_DEPTH),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }

    #[tokio::test]
    async fn rejects_when_no_host() {
        let err = SubagentTool
            .call(serde_json::json!({"prompt": "hi"}), ctx_with(None, 0))
            .await
            .unwrap_err();
        match err {
            ToolError::Validation(m) => assert!(m.contains("no subagent host")),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn caps_long_output_and_emits_sub_session_id() {
        let big = "x".repeat(SUBAGENT_OUTPUT_CAP * 2);
        let host: Arc<dyn SubagentHost> = Arc::new(StubHost {
            reply: big,
            sub_session_id: "sub-xyz".into(),
        });
        let out = SubagentTool
            .call(
                serde_json::json!({"prompt": "scan mappers"}),
                ctx_with(Some(host), 0),
            )
            .await
            .unwrap();
        assert!(out.summary.contains("[TRUNCATED"));
        assert!(out.summary.contains("sub-xyz"));
    }

    #[tokio::test]
    async fn short_output_passes_through_with_session_marker() {
        let host: Arc<dyn SubagentHost> = Arc::new(StubHost {
            reply: "found 3 pivots".into(),
            sub_session_id: "sub-abc".into(),
        });
        let out = SubagentTool
            .call(
                serde_json::json!({"prompt": "scan mappers"}),
                ctx_with(Some(host), 0),
            )
            .await
            .unwrap();
        assert!(out.summary.starts_with("found 3 pivots"));
        assert!(out.summary.contains("sub-abc"));
        assert!(!out.summary.contains("[TRUNCATED"));
    }

    #[tokio::test]
    async fn rejects_missing_prompt() {
        let host: Arc<dyn SubagentHost> = Arc::new(StubHost {
            reply: "x".into(),
            sub_session_id: "s".into(),
        });
        let err = SubagentTool
            .call(serde_json::json!({}), ctx_with(Some(host), 0))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Validation(_)));
    }
}
