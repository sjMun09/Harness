//! Turn loop driver. PLAN §2.2.
//!
//! Single responsibility: drive `Provider::stream` + tool dispatch until the
//! model emits a non-ToolUse stop. Retries the whole turn on `FinalizeError`
//! (the model produced invalid JSON for a tool_use input — replay the request).

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use harness_perm::Decision;
use harness_proto::{ContentBlock, Message, Role, StopReason, Usage};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::hooks::{HookAction, HookDispatcher, HookEvent};
use crate::plan_gate::{GateOutcome, PlanGateState};
use crate::provider::{
    ContentBlockHeader, Provider, ProviderError, StreamEvent, StreamRequest, ToolSpec,
};
use crate::tool::{Tool, ToolCtx, ToolError, ToolOutput};
use crate::turn::{BlockState, FinalizeError};

/// Max number of whole-turn retries triggered by a `FinalizeError`. The retry
/// re-submits the same request; bounded so a persistently broken provider
/// response cannot spin forever.
pub const MAX_FINALIZE_RETRIES: u32 = 2;

/// Why a turn ended early. PLAN §3.2 (TaskStop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReason {
    /// User pressed Ctrl-C / Esc — CLI exit code 130.
    UserInterrupt,
    /// Wall-clock deadline expired (engine-internal).
    Timeout,
    /// Engine-internal abort (e.g. invariant violation, parent shutting down).
    Internal,
}

impl CancelReason {
    /// Name written into the `cancelled` sidecar Meta record by
    /// `harness_mem::append_cancelled_turn`.
    #[must_use]
    pub fn as_mem_reason(self) -> &'static str {
        match self {
            Self::UserInterrupt => "user_interrupt",
            Self::Timeout => "timeout",
            Self::Internal => "internal",
        }
    }
}

/// Derive a per-tool grandchild cancel token from a per-turn child token.
/// Mirrors PLAN §2.2 pseudo-code — the tool sees its own token fire if either
/// the turn cancels or the tool's parent cancels, without the tool being able
/// to cancel its siblings.
#[must_use]
pub fn child_cancel(parent: &CancellationToken) -> CancellationToken {
    parent.child_token()
}

pub struct EngineInputs {
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub system: String,
    pub ctx: ToolCtx,
    pub max_turns: u32,
    /// PreEdit plan-gate state. Defaults to a no-op gate; CLI populates from
    /// `settings.harness.plan_gate`.
    pub plan_gate: PlanGateState,
    /// Optional progress sink for line-mode rendering (PLAN §3.1).
    /// `None` = silent (subagent nested runs default to silent so their
    /// events don't bleed into the parent's stdout).
    pub event_sink: Option<EventSink>,
    /// Per-turn cancel token (PLAN §3.2 TaskStop). `None` = uncancellable
    /// (tests, sub-agent hosts that already own a parent token). The CLI
    /// wires this to a Ctrl-C watcher.
    pub cancel: Option<CancellationToken>,
}

impl std::fmt::Debug for EngineInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineInputs")
            .field("tool_count", &self.tools.len())
            .field("system_len", &self.system.len())
            .field("max_turns", &self.max_turns)
            .field("plan_gate", &self.plan_gate)
            .field("event_sink", &self.event_sink.as_ref().map(|_| "<sink>"))
            .field("cancel", &self.cancel.as_ref().map(|_| "<token>"))
            .finish()
    }
}

/// Progress events surfaced during `run_turn`. Line-mode rendering (PLAN §3.1)
/// consumes these; headless callers (subagent, tests) leave the sink `None`.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// A new turn is starting (0-indexed). Useful for blank-line separators.
    TurnStart { turn_idx: u32 },
    /// About to invoke a tool. `preview` is the tool's own `Preview.summary_line`.
    ToolCallStart {
        id: String,
        name: String,
        preview: String,
    },
    /// Tool call finished. `summary_head` is the first non-empty line of the
    /// tool's `ToolOutput.summary` (already scrubbed of `<untrusted_tool_output>`
    /// fence markers), or the error message on failure.
    ToolCallEnd {
        id: String,
        name: String,
        ok: bool,
        summary_head: String,
    },
    /// The current turn was cancelled before reaching a natural stop (PLAN §3.2).
    Cancelled { reason: CancelReason },
}

/// Result of a `run_turn` invocation. PLAN §3.2: on cancel, callers need both
/// the cancel reason and the final message history (which already includes any
/// partially-streamed assistant Text blocks) so they can persist a sidecar
/// `cancelled` marker via `harness_mem::append_cancelled_turn`.
#[derive(Debug)]
pub enum TurnOutcome {
    Completed {
        messages: Vec<Message>,
    },
    Cancelled {
        reason: CancelReason,
        messages: Vec<Message>,
        /// The partial assistant message that prompted the cancel — borrowed
        /// from `messages.last()` when non-empty, otherwise `None`. Provided
        /// separately so callers can pass it straight to `append_cancelled_turn`
        /// without cloning or scanning the history.
        partial_assistant: Option<Message>,
    },
}

/// Sink type for `TurnEvent`s. Boxed closure so callers can write to stderr,
/// a channel, a file, etc.
pub type EventSink = Arc<dyn Fn(TurnEvent) + Send + Sync>;

fn emit(sink: &Option<EventSink>, ev: TurnEvent) {
    if let Some(s) = sink.as_ref() {
        s(ev);
    }
}

/// Run the turn loop. Returns the full message history including the final
/// assistant turn. Preserved for callers that do not care about cancellation
/// (they pass `EngineInputs.cancel = None`). For cancel-aware callers, use
/// `run_turn_with_outcome`.
pub async fn run_turn(
    inputs: EngineInputs,
    initial: Vec<Message>,
) -> Result<Vec<Message>, anyhow::Error> {
    match run_turn_with_outcome(inputs, initial).await? {
        TurnOutcome::Completed { messages } | TurnOutcome::Cancelled { messages, .. } => {
            Ok(messages)
        }
    }
}

/// Run the turn loop with explicit cancel-aware outcome (PLAN §3.2). Callers
/// that wire Ctrl-C (CLI) use this so they can persist a sidecar `cancelled`
/// marker and map the exit code to 130.
pub async fn run_turn_with_outcome(
    inputs: EngineInputs,
    initial: Vec<Message>,
) -> Result<TurnOutcome, anyhow::Error> {
    let EngineInputs {
        provider,
        tools,
        system,
        ctx,
        max_turns,
        plan_gate,
        event_sink,
        cancel,
    } = inputs;

    let tool_specs: Vec<ToolSpec> = tools
        .iter()
        .map(|t| ToolSpec {
            name: t.name().to_string(),
            description: String::new(),
            input_schema: t.schema(),
        })
        .collect();
    let tool_map: HashMap<String, Arc<dyn Tool>> = tools
        .iter()
        .map(|t| (t.name().to_string(), t.clone()))
        .collect();

    let system_full = apply_session_start_hook(&ctx.hooks, &system).await;

    let mut messages = initial;
    for turn_idx in 0..max_turns {
        if let Some(tok) = cancel.as_ref() {
            if tok.is_cancelled() {
                return Ok(cancelled_outcome(messages, None));
            }
        }
        debug!(turn = turn_idx, "turn: open stream");
        emit(&event_sink, TurnEvent::TurnStart { turn_idx });

        let drive_res = drive_one_turn(
            provider.as_ref(),
            &system_full,
            &messages,
            &tool_specs,
            cancel.as_ref(),
        )
        .await?;

        match drive_res {
            DriveOutcome::Completed { msg, stop_reason } => {
                messages.push(msg);
                if !matches!(stop_reason, Some(StopReason::ToolUse)) {
                    let _ = ctx.hooks.dispatch(HookEvent::Stop, json!({})).await;
                    return Ok(TurnOutcome::Completed { messages });
                }

                let tool_uses = collect_tool_uses(messages.last());
                if tool_uses.is_empty() {
                    warn!("stop_reason=tool_use but no tool_use blocks; terminating");
                    return Ok(TurnOutcome::Completed { messages });
                }

                let results =
                    dispatch_tool_uses(&tool_map, &ctx, &plan_gate, &tool_uses, &event_sink).await;
                messages.push(Message::user_tool_results(results));
            }
            DriveOutcome::Cancelled { partial } => {
                emit(
                    &event_sink,
                    TurnEvent::Cancelled {
                        reason: CancelReason::UserInterrupt,
                    },
                );
                return Ok(cancelled_outcome(messages, partial));
            }
        }
    }
    info!(max_turns, "turn loop hit max_turns cap");
    Ok(TurnOutcome::Completed { messages })
}

fn cancelled_outcome(mut messages: Vec<Message>, partial: Option<Message>) -> TurnOutcome {
    let partial_assistant = partial.clone();
    if let Some(p) = partial {
        messages.push(p);
    }
    TurnOutcome::Cancelled {
        reason: CancelReason::UserInterrupt,
        messages,
        partial_assistant,
    }
}

async fn apply_session_start_hook(hooks: &HookDispatcher, system: &str) -> String {
    if !hooks.has(HookEvent::SessionStart) {
        return system.to_string();
    }
    let out = hooks.dispatch(HookEvent::SessionStart, json!({})).await;
    if let Some(extra) = out.additional_context {
        let fenced = crate::hooks::fence_untrusted(&extra);
        if system.is_empty() {
            return fenced;
        }
        return format!("{system}\n\n{fenced}");
    }
    system.to_string()
}

/// Internal outcome of `drive_one_turn`. A clean completion carries the
/// assistant Message + stop reason; a cancellation carries any partially-
/// finalized assistant Message so `run_turn` can persist it before exiting.
enum DriveOutcome {
    Completed {
        msg: Message,
        stop_reason: Option<StopReason>,
    },
    Cancelled {
        partial: Option<Message>,
    },
}

/// One stream → assistant message. Retries on FinalizeError up to
/// MAX_FINALIZE_RETRIES. PLAN §3.2: if `cancel` fires mid-stream, we drop the
/// stream (tokio handles the underlying reqwest body close), preserve any
/// fully-finalized Text blocks as a partial Message, and discard incomplete
/// ToolUse blocks — we never replay a half-parsed tool call.
async fn drive_one_turn(
    provider: &dyn Provider,
    system: &str,
    messages: &[Message],
    tools: &[ToolSpec],
    cancel: Option<&CancellationToken>,
) -> Result<DriveOutcome, anyhow::Error> {
    let mut finalize_retries = 0u32;
    loop {
        if let Some(tok) = cancel {
            if tok.is_cancelled() {
                return Ok(DriveOutcome::Cancelled { partial: None });
            }
        }
        let req = StreamRequest {
            model: "",
            system,
            messages,
            tools,
            max_tokens: 0,
        };

        let stream = if let Some(tok) = cancel {
            tokio::select! {
                biased;
                () = tok.cancelled() => {
                    return Ok(DriveOutcome::Cancelled { partial: None });
                }
                s = provider.stream(req) => s
                    .map_err(|e| anyhow::anyhow!("provider stream open: {e}"))?,
            }
        } else {
            provider
                .stream(req)
                .await
                .map_err(|e| anyhow::anyhow!("provider stream open: {e}"))?
        };

        match consume_stream(stream, cancel).await {
            Ok(ConsumeOutcome::Completed(acc)) => {
                let stop_reason = acc.stop_reason;
                let usage = acc.usage;
                let msg = Message {
                    role: Role::Assistant,
                    content: acc.blocks_in_order(),
                    usage: Some(usage),
                };
                return Ok(DriveOutcome::Completed { msg, stop_reason });
            }
            Ok(ConsumeOutcome::Cancelled(acc)) => {
                let usage = acc.usage;
                let blocks = acc.finalized_only();
                let partial = if blocks.is_empty() {
                    None
                } else {
                    Some(Message {
                        role: Role::Assistant,
                        content: blocks,
                        usage: Some(usage),
                    })
                };
                return Ok(DriveOutcome::Cancelled { partial });
            }
            Err(DriveErr::Finalize(e)) if finalize_retries < MAX_FINALIZE_RETRIES => {
                finalize_retries += 1;
                warn!(error = %e, attempt = finalize_retries, "finalize error — retrying whole turn");
                continue;
            }
            Err(DriveErr::Finalize(e)) => return Err(anyhow::anyhow!("finalize: {e}")),
            Err(DriveErr::Provider(e)) => return Err(anyhow::anyhow!("provider: {e}")),
        }
    }
}

#[derive(Debug)]
enum DriveErr {
    Finalize(FinalizeError),
    Provider(ProviderError),
}

enum ConsumeOutcome {
    Completed(Accumulated),
    Cancelled(Accumulated),
}

#[derive(Default)]
struct Accumulated {
    blocks: HashMap<usize, BlockState>,
    finalized: HashMap<usize, ContentBlock>,
    order: Vec<usize>,
    usage: Usage,
    stop_reason: Option<StopReason>,
}

impl Accumulated {
    fn blocks_in_order(mut self) -> Vec<ContentBlock> {
        let mut out = Vec::with_capacity(self.order.len());
        for idx in &self.order {
            if let Some(b) = self.finalized.remove(idx) {
                out.push(b);
            }
        }
        out
    }

    /// On cancel, return only the blocks that fully finalized. Incomplete
    /// blocks (those still sitting in `self.blocks`) are discarded — we
    /// never replay a half-parsed tool call.
    fn finalized_only(mut self) -> Vec<ContentBlock> {
        let mut out = Vec::with_capacity(self.finalized.len());
        for idx in &self.order {
            if let Some(b) = self.finalized.remove(idx) {
                out.push(b);
            }
        }
        out
    }
}

async fn consume_stream(
    mut stream: crate::provider::EventStream,
    cancel: Option<&CancellationToken>,
) -> Result<ConsumeOutcome, DriveErr> {
    let mut acc = Accumulated::default();
    loop {
        let item_opt = if let Some(tok) = cancel {
            tokio::select! {
                biased;
                () = tok.cancelled() => {
                    return Ok(ConsumeOutcome::Cancelled(acc));
                }
                x = stream.next() => x,
            }
        } else {
            stream.next().await
        };
        let Some(item) = item_opt else { break };
        let ev = item.map_err(DriveErr::Provider)?;
        match ev {
            StreamEvent::MessageStart { usage, .. } => {
                acc.usage = acc.usage.merge(usage);
            }
            StreamEvent::ContentBlockStart { index, block } => {
                acc.blocks.insert(index, BlockState::from_start(block));
                if !acc.order.contains(&index) {
                    acc.order.push(index);
                }
            }
            StreamEvent::ContentBlockDelta { index, delta } => {
                if let Some(b) = acc.blocks.get_mut(&index) {
                    b.push_delta(&delta);
                } else {
                    // Delta for unknown index — treat as new block starter.
                    let stub = BlockState::from_start(match &delta {
                        crate::provider::ContentDelta::Text(_) => ContentBlockHeader::Text,
                        crate::provider::ContentDelta::InputJson(_) => {
                            ContentBlockHeader::ToolUse {
                                id: String::new(),
                                name: String::new(),
                            }
                        }
                    });
                    acc.blocks.insert(index, stub);
                    if !acc.order.contains(&index) {
                        acc.order.push(index);
                    }
                    if let Some(b) = acc.blocks.get_mut(&index) {
                        b.push_delta(&delta);
                    }
                }
            }
            StreamEvent::ContentBlockStop { index } => {
                if let Some(state) = acc.blocks.remove(&index) {
                    let block = state.finalize().map_err(DriveErr::Finalize)?;
                    acc.finalized.insert(index, block);
                }
            }
            StreamEvent::MessageDelta { stop_reason, usage } => {
                acc.usage = acc.usage.merge(usage);
                if let Some(r) = stop_reason {
                    acc.stop_reason = Some(r);
                }
            }
            StreamEvent::MessageStop => break,
            StreamEvent::Ping => {}
        }
    }
    Ok(ConsumeOutcome::Completed(acc))
}

fn collect_tool_uses(last: Option<&Message>) -> Vec<(String, String, Value)> {
    let Some(msg) = last else { return Vec::new() };
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse {
                id, name, input, ..
            } => Some((id.clone(), name.clone(), input.clone())),
            _ => None,
        })
        .collect()
}

async fn dispatch_tool_uses(
    tool_map: &HashMap<String, Arc<dyn Tool>>,
    ctx: &ToolCtx,
    plan_gate: &PlanGateState,
    tool_uses: &[(String, String, Value)],
    event_sink: &Option<EventSink>,
) -> Vec<ContentBlock> {
    let mut out = Vec::with_capacity(tool_uses.len());
    for (id, name, input) in tool_uses {
        let preview = tool_map
            .get(name)
            .map(|t| t.preview(input).summary_line)
            .unwrap_or_else(|| format!("{name}(<unknown>)"));
        emit(
            event_sink,
            TurnEvent::ToolCallStart {
                id: id.clone(),
                name: name.clone(),
                preview,
            },
        );
        let result = dispatch_one(tool_map, ctx, plan_gate, id, name, input.clone()).await;
        let (ok, head) = tool_result_to_head(&result);
        emit(
            event_sink,
            TurnEvent::ToolCallEnd {
                id: id.clone(),
                name: name.clone(),
                ok,
                summary_head: head,
            },
        );
        out.push(result);
    }
    out
}

/// Extract `(ok, first_non_empty_line)` from a `ContentBlock::ToolResult`.
/// Strips `<untrusted_tool_output ...>` opener lines so the line mode shows
/// the actual content, not the fence wrapper.
fn tool_result_to_head(block: &ContentBlock) -> (bool, String) {
    let (ok, text) = match block {
        ContentBlock::ToolResult {
            content, is_error, ..
        } => (!is_error, content.as_str()),
        _ => (true, ""),
    };
    let head = text
        .lines()
        .map(str::trim)
        .find(|l| {
            !l.is_empty()
                && !l.starts_with("<untrusted_tool_output")
                && !l.starts_with("</untrusted_tool_output")
        })
        .unwrap_or("")
        .to_string();
    (ok, head)
}

async fn dispatch_one(
    tool_map: &HashMap<String, Arc<dyn Tool>>,
    ctx: &ToolCtx,
    plan_gate: &PlanGateState,
    id: &str,
    name: &str,
    mut input: Value,
) -> ContentBlock {
    let Some(tool) = tool_map.get(name) else {
        return error_result(id, &format!("unknown tool: {name}"));
    };

    // Built-in plan-gate. Runs before external hooks because it's part of the
    // kernel safety net and we want the model to see its message even when the
    // user has no hooks configured.
    if let GateOutcome::Block { reason } = plan_gate.evaluate(name, &input) {
        return error_result(id, &reason);
    }

    // PreToolUse hook
    let pre_payload = json!({
        "event": "pre_tool_use",
        "tool": name,
        "tool_use_id": id,
        "input": input,
    });
    let pre = ctx.hooks.dispatch(HookEvent::PreToolUse, pre_payload).await;
    match pre.action {
        HookAction::Block => {
            let msg = pre
                .reason
                .unwrap_or_else(|| "blocked by pre_tool_use hook".into());
            return error_result(id, &msg);
        }
        HookAction::Rewrite => {
            if let Some(new_input) = pre.rewrite {
                input = new_input;
            }
        }
        HookAction::Allow => {}
    }

    // Permission check
    match ctx.permission.evaluate(name, &input) {
        Decision::Allow => {}
        Decision::Deny => {
            return error_result(id, &format!("permission denied for {name}"));
        }
        Decision::Ask => {
            // Headless MVP: surface as error so the caller sees it.
            return error_result(
                id,
                &format!(
                    "permission requires user approval for {name}; configure settings.permissions.allow or run with --dangerously-skip-permissions"
                ),
            );
        }
    }

    // Dispatch
    let call_res = tool.call(input.clone(), ctx.clone()).await;

    // PostToolUse hook (best-effort; result is advisory)
    let post_payload = json!({
        "event": "post_tool_use",
        "tool": name,
        "tool_use_id": id,
        "input": input,
        "ok": call_res.is_ok(),
    });
    let _ = ctx
        .hooks
        .dispatch(HookEvent::PostToolUse, post_payload)
        .await;

    match call_res {
        Ok(ToolOutput { mut summary, .. }) => {
            if let Some(advice) = plan_gate.advise_after(name, &input) {
                summary.push_str(&advice);
            }
            ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: summary,
                is_error: false,
                cache_control: None,
            }
        }
        Err(e) => error_result(id, &format_tool_error(&e)),
    }
}

fn error_result(id: &str, msg: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: msg.to_string(),
        is_error: true,
        cache_control: None,
    }
}

fn format_tool_error(e: &ToolError) -> String {
    match e {
        ToolError::PermissionDenied(s) => format!("permission denied: {s}"),
        ToolError::Io(err) => format!("io: {err}"),
        ToolError::Validation(s) => format!("validation: {s}"),
        ToolError::Cancelled => "cancelled".into(),
        ToolError::Timeout(d) => format!("timeout after {d:?}"),
        ToolError::Other(s) => format!("error: {s}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ContentDelta, EventStream};
    use async_trait::async_trait;
    use futures_util::stream;
    use harness_perm::PermissionSnapshot;
    use harness_proto::SessionId;
    use std::pin::Pin;
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    struct MockProvider {
        scripts: Mutex<Vec<Vec<StreamEvent>>>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn stream(&self, _req: StreamRequest<'_>) -> Result<EventStream, ProviderError> {
            let events = self
                .scripts
                .lock()
                .map_err(|_| ProviderError::Transport("poisoned".into()))?
                .pop()
                .unwrap_or_default();
            let s = stream::iter(events.into_iter().map(Ok::<_, ProviderError>));
            Ok(Box::pin(s)
                as Pin<
                    Box<dyn futures_core::Stream<Item = _> + Send + 'static>,
                >)
        }
    }

    fn mk_provider(scripts: Vec<Vec<StreamEvent>>) -> Arc<MockProvider> {
        // Pop from the tail; reverse so caller can read them in order.
        let mut s = scripts;
        s.reverse();
        Arc::new(MockProvider {
            scripts: Mutex::new(s),
        })
    }

    fn mk_ctx(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            session_id: SessionId::new("t"),
            cancel: CancellationToken::new(),
            permission: PermissionSnapshot::default(),
            hooks: HookDispatcher::default(),
            subagent: None,
            depth: 0,
            tx: None,
        }
    }

    // `text_only_terminates` — migrated to `tests/engine_testkit.rs`
    // (harness-testkit integration test); it exercises `MockProvider::scripted`.

    /// Plan-gate integration: a tool call to a risky path is blocked the first
    /// time, then the model retries and the second call succeeds. Walks both
    /// turns through the engine to prove the gate state is shared across the
    /// full session.
    #[tokio::test]
    async fn plan_gate_blocks_then_allows() {
        use crate::config::PlanGate;
        use harness_perm::Rule;

        // Minimal Edit-like tool that just records the input.
        struct EchoEdit;
        #[async_trait]
        impl Tool for EchoEdit {
            fn name(&self) -> &str {
                "Edit"
            }
            fn schema(&self) -> Value {
                json!({"type": "object"})
            }
            fn preview(&self, _input: &Value) -> crate::tool::Preview {
                crate::tool::Preview {
                    summary_line: "edit".into(),
                    detail: None,
                }
            }
            async fn call(&self, input: Value, _ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput {
                    summary: format!("ok: {}", input["file_path"].as_str().unwrap_or("?")),
                    detail_path: None,
                    stream: None,
                })
            }
        }

        let edit_input = json!({"file_path": "src/foo.xml", "old_string": "a", "new_string": "b"});
        let tool_use_block = ContentBlockHeader::ToolUse {
            id: "tu_1".into(),
            name: "Edit".into(),
        };
        // Same tool call, twice — once per turn.
        let mk_turn = || {
            vec![
                StreamEvent::MessageStart {
                    message_id: "m".into(),
                    usage: Usage::default(),
                },
                StreamEvent::ContentBlockStart {
                    index: 0,
                    block: tool_use_block.clone(),
                },
                StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: crate::provider::ContentDelta::InputJson(
                        edit_input.to_string().into_bytes(),
                    ),
                },
                StreamEvent::ContentBlockStop { index: 0 },
                StreamEvent::MessageDelta {
                    stop_reason: Some(StopReason::ToolUse),
                    usage: Usage::default(),
                },
                StreamEvent::MessageStop,
            ]
        };
        // Third turn finally stops cleanly.
        let stop_turn = vec![
            StreamEvent::MessageStart {
                message_id: "m".into(),
                usage: Usage::default(),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockHeader::Text,
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: crate::provider::ContentDelta::Text("done".into()),
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            },
            StreamEvent::MessageStop,
        ];
        let provider = mk_provider(vec![mk_turn(), mk_turn(), stop_turn]);

        let dir = tempfile::tempdir().unwrap();
        let mut ctx = mk_ctx(dir.path());
        ctx.permission =
            PermissionSnapshot::new(vec![], vec![Rule::parse("Edit").unwrap()], vec![]);

        let out = run_turn(
            EngineInputs {
                provider,
                tools: vec![Arc::new(EchoEdit) as Arc<dyn Tool>],
                system: "sys".into(),
                ctx,
                max_turns: 5,
                plan_gate: PlanGateState::from_config(&PlanGate {
                    enabled: true,
                    patterns: vec!["**/*.xml".into()],
                    tools: vec!["Edit".into()],
                }),
                event_sink: None,
                cancel: None,
            },
            vec![Message::user("edit src/foo.xml please")],
        )
        .await
        .unwrap();

        // Expected message sequence:
        //   [0] user prompt
        //   [1] assistant: tool_use #1
        //   [2] user: tool_result is_error=true (PLAN-GATE)
        //   [3] assistant: tool_use #2
        //   [4] user: tool_result is_error=false ("ok: src/foo.xml")
        //   [5] assistant: text "done"
        assert!(out.len() >= 5, "unexpected message count: {}", out.len());

        match &out[2].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(*is_error, "first attempt should error");
                assert!(content.contains("PLAN-GATE"), "content: {content}");
            }
            other => panic!("expected ToolResult at [2], got {other:?}"),
        }
        match &out[4].content[0] {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(!*is_error, "second attempt should succeed: {content}");
                assert!(content.starts_with("ok:"), "content: {content}");
            }
            other => panic!("expected ToolResult at [4], got {other:?}"),
        }
    }

    /// Line-mode contract: the sink sees a `TurnStart`, then a matched
    /// `ToolCallStart` / `ToolCallEnd` pair per tool_use block, in order.
    /// `ok=true` on success, `summary_head` = tool's first summary line.
    #[tokio::test]
    async fn event_sink_receives_tool_lifecycle() {
        struct Echo;
        #[async_trait]
        impl Tool for Echo {
            fn name(&self) -> &str {
                "Echo"
            }
            fn schema(&self) -> Value {
                json!({"type":"object"})
            }
            fn preview(&self, _input: &Value) -> crate::tool::Preview {
                crate::tool::Preview {
                    summary_line: "echo preview".into(),
                    detail: None,
                }
            }
            async fn call(&self, _input: Value, _ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
                Ok(ToolOutput {
                    summary: "echoed the thing\nsecond line".into(),
                    detail_path: None,
                    stream: None,
                })
            }
        }

        let tool_use_block = ContentBlockHeader::ToolUse {
            id: "tu_7".into(),
            name: "Echo".into(),
        };
        let call_turn = vec![
            StreamEvent::MessageStart {
                message_id: "m".into(),
                usage: Usage::default(),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                block: tool_use_block,
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: crate::provider::ContentDelta::InputJson("{}".into()),
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::ToolUse),
                usage: Usage::default(),
            },
            StreamEvent::MessageStop,
        ];
        let stop_turn = vec![
            StreamEvent::MessageStart {
                message_id: "m".into(),
                usage: Usage::default(),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockHeader::Text,
            },
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: crate::provider::ContentDelta::Text("done".into()),
            },
            StreamEvent::ContentBlockStop { index: 0 },
            StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            },
            StreamEvent::MessageStop,
        ];
        let provider = mk_provider(vec![call_turn, stop_turn]);

        let dir = tempfile::tempdir().unwrap();
        let events: Arc<Mutex<Vec<TurnEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let sink: EventSink = Arc::new(move |ev: TurnEvent| {
            events_clone.lock().unwrap().push(ev);
        });

        let mut ctx = mk_ctx(dir.path());
        ctx.permission = PermissionSnapshot::new(
            vec![],
            vec![harness_perm::Rule::parse("Echo").unwrap()],
            vec![],
        );

        let _ = run_turn(
            EngineInputs {
                provider,
                tools: vec![Arc::new(Echo) as Arc<dyn Tool>],
                system: "sys".into(),
                ctx,
                max_turns: 3,
                plan_gate: PlanGateState::default(),
                event_sink: Some(sink),
                cancel: None,
            },
            vec![Message::user("go")],
        )
        .await
        .unwrap();

        let events = events.lock().unwrap();
        // Expect: TurnStart(0), ToolCallStart, ToolCallEnd, TurnStart(1).
        let starts = events
            .iter()
            .filter(|e| matches!(e, TurnEvent::TurnStart { .. }))
            .count();
        assert!(starts >= 1, "missing TurnStart in {events:?}");

        let start = events
            .iter()
            .find_map(|e| match e {
                TurnEvent::ToolCallStart { name, preview, .. } => {
                    Some((name.clone(), preview.clone()))
                }
                _ => None,
            })
            .expect("ToolCallStart not emitted");
        assert_eq!(start.0, "Echo");
        assert_eq!(start.1, "echo preview");

        let end = events
            .iter()
            .find_map(|e| match e {
                TurnEvent::ToolCallEnd {
                    name,
                    ok,
                    summary_head,
                    ..
                } => Some((name.clone(), *ok, summary_head.clone())),
                _ => None,
            })
            .expect("ToolCallEnd not emitted");
        assert_eq!(end.0, "Echo");
        assert!(end.1, "tool should report ok");
        assert_eq!(end.2, "echoed the thing");
    }

    // ────────────────────────────────────────────────────────────────────
    // TaskStop (#22) — cancel-flow tests. PLAN §3.2.
    // ────────────────────────────────────────────────────────────────────

    /// Channel-driven provider for cancel tests — the test hand-feeds
    /// `StreamEvent`s through a tokio `UnboundedSender` so it can keep the
    /// stream suspended mid-flight while flipping the cancel token.
    struct ChanProvider {
        rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<StreamEvent>>>,
    }

    #[async_trait]
    impl Provider for ChanProvider {
        async fn stream(&self, _req: StreamRequest<'_>) -> Result<EventStream, ProviderError> {
            let rx = self
                .rx
                .lock()
                .map_err(|_| ProviderError::Transport("poisoned".into()))?
                .take()
                .ok_or_else(|| ProviderError::Transport("stream consumed".into()))?;
            let s = stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|ev| (Ok::<_, ProviderError>(ev), rx))
            });
            Ok(Box::pin(s)
                as Pin<
                    Box<dyn futures_core::Stream<Item = _> + Send + 'static>,
                >)
        }
    }

    fn mk_chan_provider() -> (
        Arc<ChanProvider>,
        tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let p = Arc::new(ChanProvider {
            rx: Mutex::new(Some(rx)),
        });
        (p, tx)
    }

    // `cancel_before_stream_returns_empty_partial` — migrated to
    // `tests/engine_testkit.rs`; it exercises `MockProvider::channel`.

    #[tokio::test]
    async fn cancel_mid_stream_preserves_finalized_text() {
        // Send one complete Text block, cancel, then verify the partial
        // assistant carries that Text and the TurnOutcome is Cancelled.
        let (provider, tx) = mk_chan_provider();
        let cancel = CancellationToken::new();
        let dir = tempfile::tempdir().unwrap();

        let fire_cancel = cancel.clone();
        tokio::spawn(async move {
            tx.send(StreamEvent::MessageStart {
                message_id: "m".into(),
                usage: Usage::default(),
            })
            .unwrap();
            tx.send(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockHeader::Text,
            })
            .unwrap();
            tx.send(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::Text("partial reply".into()),
            })
            .unwrap();
            tx.send(StreamEvent::ContentBlockStop { index: 0 }).unwrap();
            // Give the engine a beat to drain and finalize the block, then
            // cancel. We never send MessageStop so the stream stays open —
            // without cancel this would hang forever, so the test also
            // verifies that the cancel path wakes the stream future.
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            fire_cancel.cancel();
            // Keep tx alive until the receiver drops; dropping early would
            // close the channel and end the stream normally.
            std::mem::forget(tx);
        });

        let outcome = run_turn_with_outcome(
            EngineInputs {
                provider,
                tools: Vec::new(),
                system: String::new(),
                ctx: mk_ctx(dir.path()),
                max_turns: 3,
                plan_gate: PlanGateState::default(),
                event_sink: None,
                cancel: Some(cancel),
            },
            vec![Message::user("hi")],
        )
        .await
        .unwrap();

        match outcome {
            TurnOutcome::Cancelled {
                partial_assistant,
                messages,
                ..
            } => {
                let partial = partial_assistant.expect("expected partial assistant");
                assert!(matches!(partial.role, Role::Assistant));
                assert_eq!(partial.content.len(), 1);
                assert!(
                    matches!(&partial.content[0], ContentBlock::Text { text, .. } if text == "partial reply")
                );
                // messages = [user, partial_assistant]
                assert_eq!(messages.len(), 2);
            }
            TurnOutcome::Completed { .. } => panic!("expected Cancelled"),
        }
    }

    #[tokio::test]
    async fn cancel_drops_incomplete_tool_use() {
        // Tool use starts but never gets ContentBlockStop — cancel fires and
        // the partial should be None (we never replay a half-parsed tool_use).
        let (provider, tx) = mk_chan_provider();
        let cancel = CancellationToken::new();
        let dir = tempfile::tempdir().unwrap();

        let fire_cancel = cancel.clone();
        tokio::spawn(async move {
            tx.send(StreamEvent::MessageStart {
                message_id: "m".into(),
                usage: Usage::default(),
            })
            .unwrap();
            tx.send(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockHeader::ToolUse {
                    id: "c1".into(),
                    name: "Read".into(),
                },
            })
            .unwrap();
            tx.send(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::InputJson("{\"path\":\"/tmp".into()),
            })
            .unwrap();
            // No ContentBlockStop — incomplete tool_use. Cancel.
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            fire_cancel.cancel();
            std::mem::forget(tx);
        });

        let outcome = run_turn_with_outcome(
            EngineInputs {
                provider,
                tools: Vec::new(),
                system: String::new(),
                ctx: mk_ctx(dir.path()),
                max_turns: 3,
                plan_gate: PlanGateState::default(),
                event_sink: None,
                cancel: Some(cancel),
            },
            vec![Message::user("hi")],
        )
        .await
        .unwrap();

        match outcome {
            TurnOutcome::Cancelled {
                partial_assistant, ..
            } => {
                assert!(
                    partial_assistant.is_none(),
                    "incomplete tool_use must be dropped — got {partial_assistant:?}"
                );
            }
            TurnOutcome::Completed { .. } => panic!("expected Cancelled"),
        }
    }

    #[tokio::test]
    async fn child_cancel_propagates_from_parent() {
        // Grandchild fires when the parent is cancelled — PLAN §2.2 pseudo.
        let parent = CancellationToken::new();
        let grand = child_cancel(&parent);
        assert!(!grand.is_cancelled());
        parent.cancel();
        assert!(grand.is_cancelled());
    }
}
