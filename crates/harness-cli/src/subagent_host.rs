//! `CliSubagentHost` — implements `SubagentHost` by reusing `run_turn`.
//!
//! Lives in `harness-cli` rather than `harness-tools` because spawning a
//! sub-turn needs `Provider` + `run_turn`, and pulling those into the tool
//! crate would create a dependency cycle (kernel depends on tool crate for
//! `Tool` impls in testing? no — but the host needs the concrete provider +
//! registry that only the CLI assembles). PLAN §5.4.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use harness_core::engine::{run_turn, EngineInputs};
use harness_core::hooks::HookDispatcher;
use harness_core::plan_gate::PlanGateState;
use harness_core::subagent::{SubagentError, SubagentHost, SubagentResult, SubagentSpec};
use harness_core::tx::OptTx;
use harness_core::{Provider, Tool, ToolCtx};
use harness_mem::{Record, SessionHeader};
use harness_perm::{PermissionSnapshot, Rule};
use harness_proto::{ContentBlock, Message, Role, SessionId};
use tokio_util::sync::CancellationToken;

/// Host that spawns a child sub-turn using the parent's provider/tool registry.
///
/// Cloneable via `Arc` — `ToolCtx::subagent` is `Option<Arc<dyn SubagentHost>>`
/// so one host is shared across every `Subagent` tool call in a session.
pub struct CliSubagentHost {
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
    system: String,
    hooks: HookDispatcher,
    plan_gate: PlanGateState,
    cwd: PathBuf,
    model: String,
    tx: OptTx,
}

impl std::fmt::Debug for CliSubagentHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliSubagentHost")
            .field("tool_count", &self.tools.len())
            .field("model", &self.model)
            .finish()
    }
}

impl CliSubagentHost {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        system: String,
        hooks: HookDispatcher,
        plan_gate: PlanGateState,
        cwd: PathBuf,
        model: String,
        tx: OptTx,
    ) -> Self {
        Self {
            provider,
            tools,
            system,
            hooks,
            plan_gate,
            cwd,
            model,
            tx,
        }
    }
}

#[async_trait]
impl SubagentHost for CliSubagentHost {
    async fn spawn(
        &self,
        spec: SubagentSpec,
        cancel: CancellationToken,
    ) -> Result<SubagentResult, SubagentError> {
        let allow: HashSet<&str> = spec.tools_allowlist.iter().map(String::as_str).collect();
        let child_tools: Vec<Arc<dyn Tool>> = self
            .tools
            .iter()
            .filter(|t| allow.contains(t.name()))
            .cloned()
            .collect();

        let allow_rules: Vec<Rule> = spec
            .tools_allowlist
            .iter()
            .filter_map(|name| Rule::parse(name).ok())
            .collect();
        let permission = PermissionSnapshot::new(Vec::new(), allow_rules, Vec::new());

        let sub_session_id = harness_mem::new_session_id();
        let sub_session_path = harness_mem::session_path(&sub_session_id);
        let header = SessionHeader::new(sub_session_id.clone(), &self.model);
        harness_mem::init(&sub_session_path, &header)
            .await
            .map_err(|e| SubagentError::Execution(format!("init sub-session: {e}")))?;
        let parent_meta = harness_mem::Meta {
            event: "subagent_spawn".into(),
            detail: serde_json::json!({
                "parent_session": spec.parent_session,
                "depth": spec.depth,
                "max_turns": spec.max_turns,
                "tools": spec.tools_allowlist,
            }),
        };
        let _ = harness_mem::append(&sub_session_path, &Record::Meta(parent_meta)).await;

        let ctx = ToolCtx {
            cwd: self.cwd.clone(),
            session_id: sub_session_id.clone(),
            cancel,
            permission,
            hooks: self.hooks.clone(),
            subagent: None, // depth-cap is also enforced structurally: no host → no spawn
            depth: spec.depth,
            tx: self.tx.clone(),
            ask_prompt: None,
        };

        let initial = vec![Message::user(spec.prompt.clone())];
        for m in &initial {
            let _ = harness_mem::append(&sub_session_path, &Record::Message(m.clone())).await;
        }

        let inputs = EngineInputs {
            provider: self.provider.clone(),
            tools: child_tools,
            system: self.system.clone(),
            ctx,
            max_turns: spec.max_turns,
            plan_gate: self.plan_gate.clone(),
            // Silent: sub-agent work is summarised via `SubagentResult`.
            event_sink: None,
            // Sub-agents inherit cancellation through `ctx.cancel` (child
            // token from the parent turn) so no separate engine-level cancel
            // is plumbed here.
            cancel: None,
        };

        let msgs = run_turn(inputs, initial)
            .await
            .map_err(|e| SubagentError::Execution(format!("run_turn: {e}")))?;

        for m in msgs.iter().skip(1) {
            let _ = harness_mem::append(&sub_session_path, &Record::Message(m.clone())).await;
        }

        let (text, usage) = extract_final(&msgs);
        let tool_calls = count_tool_calls(&msgs);

        Ok(SubagentResult {
            text,
            tool_calls,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            sub_session_id: sub_session_id.to_string(),
        })
    }
}

/// Walk messages in reverse; pull the last assistant text + its usage.
fn extract_final(msgs: &[Message]) -> (String, harness_proto::Usage) {
    for m in msgs.iter().rev() {
        if matches!(m.role, Role::Assistant) {
            let mut out = String::new();
            for b in &m.content {
                if let ContentBlock::Text { text, .. } = b {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                }
            }
            if !out.is_empty() {
                return (out, m.usage.unwrap_or_default());
            }
        }
    }
    (String::new(), harness_proto::Usage::default())
}

fn count_tool_calls(msgs: &[Message]) -> u32 {
    let mut n = 0u32;
    for m in msgs {
        if !matches!(m.role, Role::Assistant) {
            continue;
        }
        for b in &m.content {
            if matches!(b, ContentBlock::ToolUse { .. }) {
                n = n.saturating_add(1);
            }
        }
    }
    n
}

// Keep `SessionId` usable at the call site without an extra import.
#[allow(dead_code)]
fn _link_check() {
    let _ = SessionId::new("x");
}
