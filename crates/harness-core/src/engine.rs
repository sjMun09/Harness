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
use harness_token::{Budget, TiktokenEstimator, TokenEstimator};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::compaction::{self, CompactionOptions};
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

/// Max number of whole-turn retries triggered by a retryable provider error
/// (transport failure, 5xx, 429 rate limit). Backoff is exponential
/// (100 ms / 400 ms / 1 600 ms) capped at 3 attempts. Respects `Retry-After`
/// when present.
pub const MAX_PROVIDER_RETRIES: u32 = 3;

/// Default model context window used when the caller does not override
/// `compaction_context_window`. 200k matches Claude Opus/Sonnet.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// Default compaction trigger — fraction of the context window at which
/// `compact()` runs before the next turn's request. 0.75 leaves comfortable
/// headroom for the model's response.
pub const DEFAULT_COMPACTION_THRESHOLD: f64 = 0.75;

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
            description: t.description().to_string(),
            input_schema: t.schema(),
        })
        .collect();
    let tool_map: HashMap<String, Arc<dyn Tool>> = tools
        .iter()
        .map(|t| (t.name().to_string(), t.clone()))
        .collect();

    let system_full = apply_session_start_hook(&ctx.hooks, &system).await;

    // Compaction knobs — overridable at runtime via env vars so the CLI (and
    // tests) can tune without adding new `EngineInputs` fields. Defaults land
    // at 75% of a 200k-token Claude context window.
    let context_window = env_u64("HARNESS_CONTEXT_WINDOW").unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let threshold = env_f64("HARNESS_COMPACTION_THRESHOLD").unwrap_or(DEFAULT_COMPACTION_THRESHOLD);
    let estimator = TiktokenEstimator;

    let mut messages = initial;
    for turn_idx in 0..max_turns {
        if let Some(tok) = cancel.as_ref() {
            if tok.is_cancelled() {
                return Ok(cancelled_outcome(messages, None));
            }
        }

        // Compaction gate: estimate pending-request tokens (system + history)
        // against a `Budget` sized to the configured threshold. If tripped,
        // drop the oldest turns and log a meta note.
        maybe_compact(
            &mut messages,
            &system_full,
            context_window,
            threshold,
            &estimator,
        );

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

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

fn env_f64(key: &str) -> Option<f64> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// Estimate the token footprint of the pending request (system prompt +
/// message history) and, if it crosses `threshold * context_window`, run
/// `compaction::compact()` and swap in the trimmed slice.
///
/// Matches PLAN §3.2 / §5.11: a no-op when under budget, otherwise replaces
/// the history with `[synthetic note, retained_tail...]` and emits a meta
/// `tracing::info!` line naming the before/after token counts. The session
/// JSONL `meta` record is written by the CLI caller (outside the engine).
fn maybe_compact(
    messages: &mut Vec<Message>,
    system: &str,
    context_window: u64,
    threshold: f64,
    estimator: &dyn TokenEstimator,
) {
    if context_window == 0 || messages.is_empty() {
        return;
    }
    // Convert the threshold into the Budget cap directly. Budget applies a
    // 0.9 safety factor on top, so we pre-undo that to land on exactly
    // `threshold * context_window` trigger.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let cap = {
        let raw = (context_window as f64) * threshold / harness_token::BUDGET_SAFETY_FACTOR;
        raw as u64
    };
    let before = estimate_request_tokens(system, messages, estimator);
    let mut budget = Budget::new(cap);
    budget.add(Usage {
        input_tokens: before,
        output_tokens: 0,
        ..Usage::default()
    });
    if !budget.exceeded() {
        return;
    }
    // Compaction target matches the trigger threshold — aim to land back
    // safely under it. `keep_recent_turns` floor of 4 matches
    // `CompactionOptions::default_for_200k`.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let target_tokens = ((context_window as f64) * threshold) as usize;
    let opts = CompactionOptions {
        target_tokens,
        keep_recent_turns: 4,
    };
    let r = compaction::compact(messages, estimator, &opts);
    if !r.changed() {
        return;
    }
    let after = estimate_request_tokens(system, &r.messages, estimator) as u64;
    info!(
        before = before,
        after = after,
        dropped_turns = r.dropped_turns,
        kept_from_turn = ?r.kept_from_turn,
        "context compacted: {before} -> {after} tokens"
    );
    *messages = r.messages;
}

/// Sum estimator tokens across the system prompt plus every message's text
/// payloads (Text / ToolUse.input / ToolResult.content).
fn estimate_request_tokens(
    system: &str,
    messages: &[Message],
    estimator: &dyn TokenEstimator,
) -> u64 {
    let mut total: u64 = estimator.count(system) as u64;
    for m in messages {
        for b in &m.content {
            let c = match b {
                ContentBlock::Text { text, .. } => estimator.count(text),
                ContentBlock::ToolUse { name, input, .. } => {
                    let rendered = serde_json::to_string(input).unwrap_or_default();
                    estimator.count(name) + estimator.count(&rendered)
                }
                ContentBlock::ToolResult { content, .. } => estimator.count(content),
            };
            total = total.saturating_add(c as u64);
        }
    }
    total
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
    let mut provider_retries = 0u32;
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

        let stream_res = if let Some(tok) = cancel {
            tokio::select! {
                biased;
                () = tok.cancelled() => {
                    return Ok(DriveOutcome::Cancelled { partial: None });
                }
                s = provider.stream(req) => s,
            }
        } else {
            provider.stream(req).await
        };

        let stream = match stream_res {
            Ok(s) => s,
            Err(e) if e.is_retryable() && provider_retries < MAX_PROVIDER_RETRIES => {
                let delay = retry_delay(provider_retries, e.retry_after());
                provider_retries += 1;
                warn!(
                    error = %e,
                    attempt = provider_retries,
                    backoff_ms = delay.as_millis() as u64,
                    "provider stream open failed — backing off and retrying",
                );
                sleep_or_cancel(delay, cancel).await;
                continue;
            }
            Err(e) => return Err(anyhow::anyhow!("provider stream open: {e}")),
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
            Err(DriveErr::Provider(e))
                if e.is_retryable() && provider_retries < MAX_PROVIDER_RETRIES =>
            {
                let delay = retry_delay(provider_retries, e.retry_after());
                provider_retries += 1;
                warn!(
                    error = %e,
                    attempt = provider_retries,
                    backoff_ms = delay.as_millis() as u64,
                    "provider stream error — backing off and retrying",
                );
                sleep_or_cancel(delay, cancel).await;
                continue;
            }
            Err(DriveErr::Provider(e)) => return Err(anyhow::anyhow!("provider: {e}")),
        }
    }
}

/// Exponential backoff for provider retries (100 ms / 400 ms / 1 600 ms).
/// If the provider reported an explicit `Retry-After`, use the larger of
/// that and the backoff so we never probe sooner than the server asked.
fn retry_delay(attempt: u32, retry_after: Option<std::time::Duration>) -> std::time::Duration {
    // 100ms * 4^attempt: 100 / 400 / 1600
    let exp = 100u64.saturating_mul(4u64.saturating_pow(attempt));
    let backoff = std::time::Duration::from_millis(exp);
    match retry_after {
        Some(ra) if ra > backoff => ra,
        _ => backoff,
    }
}

/// Sleep for `delay` but wake early on cancel. Never fails — cancel during
/// the sleep simply shortens the wait; the outer loop's cancel check picks
/// it up on the next iteration.
async fn sleep_or_cancel(delay: std::time::Duration, cancel: Option<&CancellationToken>) {
    if let Some(tok) = cancel {
        tokio::select! {
            biased;
            () = tok.cancelled() => {}
            () = tokio::time::sleep(delay) => {}
        }
    } else {
        tokio::time::sleep(delay).await;
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
            StreamEvent::Error(msg) => {
                // Anthropic mid-stream `event: error` frames are terminal.
                // Previously mapped to Ping, causing the engine to hang
                // waiting for a MessageStop that would never arrive.
                return Err(DriveErr::Provider(ProviderError::Transport(format!(
                    "provider stream error: {msg}"
                ))));
            }
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
    // Phase 1: emit ToolCallStart for every call in input order so the line
    // renderer shows consistent "entering" banners before any call resolves.
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
    }

    // Phase 2: dispatch every tool_use concurrently via `join_all`. Each child
    // sees its own grandchild cancel token (PLAN §2.2) so cancel propagates
    // from the turn-level token without a tool being able to cancel a sibling.
    let futures = tool_uses.iter().map(|(id, name, input)| {
        let child_ctx = ctx.with_cancel(child_cancel(&ctx.cancel));
        let id = id.clone();
        let name = name.clone();
        let input = input.clone();
        async move {
            dispatch_one(tool_map, &child_ctx, plan_gate, &id, &name, input).await
        }
    });
    let results: Vec<ContentBlock> = futures_util::future::join_all(futures).await;

    // Phase 3: emit ToolCallEnd in the original input order (results are
    // index-aligned with `tool_uses`).
    for ((id, name, _), result) in tool_uses.iter().zip(results.iter()) {
        let (ok, head) = tool_result_to_head(result);
        emit(
            event_sink,
            TurnEvent::ToolCallEnd {
                id: id.clone(),
                name: name.clone(),
                ok,
                summary_head: head,
            },
        );
    }

    results
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
            fn description(&self) -> &'static str {
                "test echo edit"
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
            fn description(&self) -> &'static str {
                "test echo tool"
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

    /// Parallel dispatch contract: several tool_use blocks in a single
    /// assistant turn are issued concurrently (wall-clock faster than the
    /// sequential sum of their sleeps) but the returned ToolResults keep the
    /// original input order (1:1 tool_use_id correspondence).
    #[tokio::test]
    async fn parallel_tool_dispatch_preserves_order_and_runs_concurrently() {
        use std::time::Duration;
        use tokio::time::Instant;

        /// Sleeps for `delay_ms` then echoes its `tag`. Each call writes a
        /// `start` timestamp into a shared vec so the test can verify the
        /// calls overlap in time.
        struct SleepEcho {
            tag: &'static str,
            delay_ms: u64,
            starts: Arc<Mutex<Vec<(String, Instant)>>>,
        }
        #[async_trait]
        impl Tool for SleepEcho {
            fn name(&self) -> &str {
                self.tag
            }
            fn description(&self) -> &'static str {
                "sleep-echo test tool"
            }
            fn schema(&self) -> Value {
                json!({"type":"object"})
            }
            fn preview(&self, _input: &Value) -> crate::tool::Preview {
                crate::tool::Preview {
                    summary_line: self.tag.into(),
                    detail: None,
                }
            }
            async fn call(&self, _input: Value, _ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
                self.starts
                    .lock()
                    .unwrap()
                    .push((self.tag.to_string(), Instant::now()));
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
                Ok(ToolOutput {
                    summary: format!("done:{}", self.tag),
                    detail_path: None,
                    stream: None,
                })
            }
        }

        let starts: Arc<Mutex<Vec<(String, Instant)>>> = Arc::new(Mutex::new(Vec::new()));
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(SleepEcho {
                tag: "A",
                delay_ms: 120,
                starts: starts.clone(),
            }) as Arc<dyn Tool>,
            Arc::new(SleepEcho {
                tag: "B",
                delay_ms: 120,
                starts: starts.clone(),
            }),
            Arc::new(SleepEcho {
                tag: "C",
                delay_ms: 120,
                starts: starts.clone(),
            }),
        ];

        // Assistant turn producing three tool_use blocks A, B, C in that order.
        let mk_use = |index: usize, name: &str, id: &str| {
            vec![
                StreamEvent::ContentBlockStart {
                    index,
                    block: ContentBlockHeader::ToolUse {
                        id: id.into(),
                        name: name.into(),
                    },
                },
                StreamEvent::ContentBlockDelta {
                    index,
                    delta: crate::provider::ContentDelta::InputJson("{}".into()),
                },
                StreamEvent::ContentBlockStop { index },
            ]
        };
        let mut call_turn = vec![StreamEvent::MessageStart {
            message_id: "m".into(),
            usage: Usage::default(),
        }];
        call_turn.extend(mk_use(0, "A", "tu_a"));
        call_turn.extend(mk_use(1, "B", "tu_b"));
        call_turn.extend(mk_use(2, "C", "tu_c"));
        call_turn.push(StreamEvent::MessageDelta {
            stop_reason: Some(StopReason::ToolUse),
            usage: Usage::default(),
        });
        call_turn.push(StreamEvent::MessageStop);

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
        let mut ctx = mk_ctx(dir.path());
        ctx.permission = PermissionSnapshot::new(
            vec![],
            vec![
                harness_perm::Rule::parse("A").unwrap(),
                harness_perm::Rule::parse("B").unwrap(),
                harness_perm::Rule::parse("C").unwrap(),
            ],
            vec![],
        );

        let wall_start = Instant::now();
        let msgs = run_turn(
            EngineInputs {
                provider,
                tools,
                system: "sys".into(),
                ctx,
                max_turns: 3,
                plan_gate: PlanGateState::default(),
                event_sink: None,
                cancel: None,
            },
            vec![Message::user("go")],
        )
        .await
        .unwrap();
        let elapsed = wall_start.elapsed();

        // Concurrency: with three 120 ms sleeps, a sequential loop would
        // take >= 360 ms. Parallel dispatch should finish well under that —
        // we accept anything under 280 ms (leaves headroom for slow CI).
        assert!(
            elapsed < Duration::from_millis(280),
            "expected parallel dispatch (<280ms), took {elapsed:?}"
        );

        // Order: the single user+tool_results message must carry A,B,C in
        // input order (1:1 with the tool_use blocks).
        let tool_results_msg = msgs
            .iter()
            .find(|m| {
                m.role == Role::User
                    && m.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            })
            .expect("tool_results message");
        let ids: Vec<&str> = tool_results_msg
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["tu_a", "tu_b", "tu_c"]);
    }

    /// `maybe_compact` must be a no-op when the history is well under budget.
    #[test]
    fn maybe_compact_noop_under_budget() {
        let mut msgs = vec![Message::user("hi"), Message::user("there")];
        let before_len = msgs.len();
        maybe_compact(
            &mut msgs,
            "sys",
            DEFAULT_CONTEXT_WINDOW,
            DEFAULT_COMPACTION_THRESHOLD,
            &harness_token::TiktokenEstimator,
        );
        assert_eq!(msgs.len(), before_len, "must not mutate when under budget");
    }

    /// When the computed request footprint crosses the threshold,
    /// `maybe_compact` trims oldest turns and the caller ends up with a
    /// shorter history whose first message is the synthetic placeholder.
    #[test]
    fn maybe_compact_trims_when_over_threshold() {
        // Build 8 tiny turns. Shrink the context window to 40 tokens (word
        // estimator semantics) so the history trips the 0.75 trigger.
        let pad = "word ".repeat(8);
        let mut msgs = Vec::new();
        for _ in 0..8 {
            msgs.push(Message::user(&pad));
            msgs.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "ok".into(),
                    cache_control: None,
                }],
                usage: None,
            });
        }
        let before_len = msgs.len();

        struct WordEstimator;
        impl TokenEstimator for WordEstimator {
            fn count(&self, text: &str) -> usize {
                text.split_whitespace().count()
            }
        }

        // 8 turns × ~8 words ≈ 64 tokens. Window=40 × 0.75 trigger → compact.
        maybe_compact(&mut msgs, "", 40, 0.75, &WordEstimator);
        assert!(
            msgs.len() < before_len,
            "expected compaction to drop turns (before {before_len}, after {})",
            msgs.len()
        );
        // First message is the synthetic placeholder.
        match &msgs[0].content[0] {
            ContentBlock::Text { text, .. } => {
                assert!(text.contains("elided"), "got: {text}");
            }
            other => panic!("expected synthetic Text, got {other:?}"),
        }
    }

    /// Exponential backoff schedule: 100 ms / 400 ms / 1600 ms, and
    /// `retry_after` overrides when larger.
    #[test]
    fn retry_delay_follows_exponential_schedule() {
        use std::time::Duration;
        assert_eq!(retry_delay(0, None), Duration::from_millis(100));
        assert_eq!(retry_delay(1, None), Duration::from_millis(400));
        assert_eq!(retry_delay(2, None), Duration::from_millis(1_600));
        // retry_after longer than backoff wins.
        assert_eq!(
            retry_delay(0, Some(Duration::from_secs(5))),
            Duration::from_secs(5)
        );
        // retry_after shorter than backoff is ignored.
        assert_eq!(
            retry_delay(2, Some(Duration::from_millis(10))),
            Duration::from_millis(1_600)
        );
    }

    /// Transport errors on stream open trigger up to `MAX_PROVIDER_RETRIES`
    /// retries before surfacing the final error. Uses a provider that refuses
    /// every call; we verify the call count grows past the first attempt.
    /// The backoff schedule is 100 + 400 + 1600 ≈ 2.1 s total — fine in real
    /// time, and the test exits as soon as the final attempt resolves.
    #[tokio::test]
    async fn transport_error_retries_before_giving_up() {
        use std::sync::atomic::{AtomicU32, Ordering};

        struct FlakyProvider {
            attempts: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Provider for FlakyProvider {
            async fn stream(
                &self,
                _req: StreamRequest<'_>,
            ) -> Result<crate::provider::EventStream, ProviderError> {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                Err(ProviderError::Transport("simulated connreset".into()))
            }
        }

        let attempts = Arc::new(AtomicU32::new(0));
        let provider = Arc::new(FlakyProvider {
            attempts: attempts.clone(),
        });
        let dir = tempfile::tempdir().unwrap();

        let res = run_turn(
            EngineInputs {
                provider,
                tools: Vec::new(),
                system: String::new(),
                ctx: mk_ctx(dir.path()),
                max_turns: 1,
                plan_gate: PlanGateState::default(),
                event_sink: None,
                cancel: None,
            },
            vec![Message::user("hi")],
        )
        .await;
        assert!(res.is_err(), "expected final error after retries");
        // 1 initial attempt + MAX_PROVIDER_RETRIES retries.
        let n = attempts.load(Ordering::SeqCst);
        assert_eq!(n, 1 + MAX_PROVIDER_RETRIES, "attempts: {n}");
    }

    /// A retryable error that resolves on attempt 2 returns a successful
    /// turn outcome — verifies the happy-path retry loop doesn't leak errors.
    #[tokio::test]
    async fn transport_error_recovers_after_retry() {
        use futures_util::stream;
        use std::pin::Pin;
        use std::sync::atomic::{AtomicU32, Ordering};

        struct RecoveringProvider {
            attempts: Arc<AtomicU32>,
        }
        #[async_trait]
        impl Provider for RecoveringProvider {
            async fn stream(
                &self,
                _req: StreamRequest<'_>,
            ) -> Result<crate::provider::EventStream, ProviderError> {
                let n = self.attempts.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    return Err(ProviderError::Server(503));
                }
                let events = vec![
                    Ok(StreamEvent::MessageStart {
                        message_id: "m".into(),
                        usage: Usage::default(),
                    }),
                    Ok(StreamEvent::ContentBlockStart {
                        index: 0,
                        block: ContentBlockHeader::Text,
                    }),
                    Ok(StreamEvent::ContentBlockDelta {
                        index: 0,
                        delta: crate::provider::ContentDelta::Text("ok".into()),
                    }),
                    Ok(StreamEvent::ContentBlockStop { index: 0 }),
                    Ok(StreamEvent::MessageDelta {
                        stop_reason: Some(StopReason::EndTurn),
                        usage: Usage::default(),
                    }),
                    Ok(StreamEvent::MessageStop),
                ];
                Ok(Box::pin(stream::iter(events))
                    as Pin<Box<dyn futures_core::Stream<Item = _> + Send + 'static>>)
            }
        }

        let attempts = Arc::new(AtomicU32::new(0));
        let provider = Arc::new(RecoveringProvider {
            attempts: attempts.clone(),
        });
        let dir = tempfile::tempdir().unwrap();
        let msgs = run_turn(
            EngineInputs {
                provider,
                tools: Vec::new(),
                system: String::new(),
                ctx: mk_ctx(dir.path()),
                max_turns: 1,
                plan_gate: PlanGateState::default(),
                event_sink: None,
                cancel: None,
            },
            vec![Message::user("hi")],
        )
        .await
        .expect("second attempt should succeed");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(msgs.len() >= 2, "missing assistant reply: {msgs:?}");
    }
}
