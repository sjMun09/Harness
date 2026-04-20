//! Bridge between the real `harness-core` engine and the `harness-tui`
//! front-end. PLAN §3.2.
//!
//! This module is the glue that lets `harness --tui ask "..."` drive the real
//! turn loop instead of the `DemoEngine`. It implements `harness_tui::EngineDriver`
//! by spawning a tokio task that calls `run_turn_with_outcome` with a custom
//! `EventSink`; the sink translates each engine `TurnEvent` into the TUI's
//! higher-level `TurnEvent` and pushes it onto the TUI's events channel.
//!
//! Event translation (engine → TUI):
//!   - `TurnStart { turn_idx }`                      → `TurnStart { turn: turn_idx + 1 }` (1-based)
//!   - `ToolCallStart { id, name, preview }`         → `ToolStart { id, name, preview }` (+ record start time)
//!   - `ToolCallEnd { id, ok, summary_head, .. }`    → `ToolEnd { id, ok, summary, elapsed }`
//!   - `Cancelled { .. }`                            → `TurnEnd { reason: Cancelled }`
//!
//! The engine does not stream assistant text deltas through its current
//! `TurnEvent` surface — Text blocks are only visible on the final `Message`
//! returned from `run_turn_with_outcome`. To preserve the TUI contract
//! (AssistantTextDelta{…} + AssistantMessageEnd before TurnEnd), this bridge
//! extracts the final assistant Text blocks from the returned `Vec<Message>`
//! and emits them as a single `AssistantTextDelta` followed by
//! `AssistantMessageEnd`. This is lossy w.r.t. streaming — the user sees the
//! whole response land at once rather than char-by-char — but it's a correct
//! event sequence and an improvement over DemoEngine. A future iter can lift
//! streaming by adding a streaming `EventSink` method or by making the sink
//! aware of per-block text deltas.
//!
//! Permission modal: out of scope for this pass. The engine's built-in
//! permission flow already rejects `Ask` decisions with an informative error
//! (see `harness_core::engine::dispatch_one` → `Decision::Ask`), which surfaces
//! as a ToolEnd{ok:false}. Users who want interactive approval can drop an
//! `ask: [...]` hook in `settings.json` — the hook dispatcher produces the
//! same contract. A proper TUI modal requires threading a oneshot channel
//! from `dispatch_one` back through `EventSink`, which is a non-trivial
//! engine surface change; deferring is the right call for this iter.
//!
//! Cancellation: the TUI hands the driver a `CancellationFlag` (a shared
//! `AtomicBool`, not a tokio-util token). We bridge this to a real
//! `tokio_util::sync::CancellationToken` (the engine's native type) by
//! spawning a tiny polling task.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use harness_core::engine::{
    run_turn_with_outcome, CancelReason, EngineInputs, EventSink, TurnEvent as EngineEvent,
    TurnOutcome,
};
use harness_proto::{ContentBlock, Message, Role};
use harness_tui::event_loop::tokio_util_lite::CancellationFlag;
use harness_tui::{
    app::EngineHandle,
    event::{TurnEndReason, TurnEvent as TuiEvent},
    event_loop::EngineDriver,
};
use tokio_util::sync::CancellationToken;

/// Adapter bundling everything the engine needs for one turn plus the message
/// history to feed it. Constructed by the CLI's `--tui ask` path.
///
/// The driver is a one-shot: `EngineDriver::start` consumes `self` in a boxed
/// context, so callers that want access to the final `TurnOutcome` (e.g. to
/// persist the session transcript after the TUI tears down) install a
/// [`OutcomeSlot`] via [`TuiEngineDriver::with_outcome_slot`]. The slot is
/// populated before the terminal TurnEnd event fires, so after
/// `TuiApp::run_session` returns the CLI can lock the slot and read the
/// outcome synchronously.
pub struct TuiEngineDriver {
    inputs: EngineInputs,
    initial: Vec<Message>,
    /// Cancel token wired into `EngineInputs.cancel`. We retain a clone so the
    /// TUI-side `CancellationFlag` can fire it via a bridge task.
    engine_cancel: CancellationToken,
    /// Optional sink for the final outcome. `None` drops it — fine for tests
    /// that only care about event translation.
    outcome_slot: Option<OutcomeSlot>,
}

/// Shared slot the CLI reads after the TUI returns, to persist the transcript
/// and compute the correct `SessionExit`. `Arc<Mutex<_>>` because the driver
/// future runs on a spawned task and populates it before emitting TurnEnd.
pub type OutcomeSlot = Arc<Mutex<Option<Result<TurnOutcome, anyhow::Error>>>>;

/// Helper to build a fresh outcome slot.
pub fn new_outcome_slot() -> OutcomeSlot {
    Arc::new(Mutex::new(None))
}

impl std::fmt::Debug for TuiEngineDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiEngineDriver")
            .field("initial_messages", &self.initial.len())
            .field("engine_cancel", &"<token>")
            .field("outcome_slot", &self.outcome_slot.as_ref().map(|_| "<slot>"))
            .finish()
    }
}

impl TuiEngineDriver {
    /// Build a driver from the same `EngineInputs` the line-mode path uses.
    ///
    /// The caller is expected to have *already* placed the engine cancel token
    /// in `inputs.cancel` (it's needed so this bridge can wire the TUI's
    /// cancel flag to the same token). If `inputs.cancel` is `None`, a fresh
    /// token is installed — cancellation from the TUI then works but an
    /// external Ctrl-C watcher has no hook.
    ///
    /// `inputs.event_sink` is overwritten by this bridge: it's the channel we
    /// own for translating engine events into TUI events. Any sink the caller
    /// set is dropped — the TUI is assumed to be the sole consumer.
    pub fn new(mut inputs: EngineInputs, initial: Vec<Message>) -> Self {
        let engine_cancel = inputs.cancel.clone().unwrap_or_else(|| {
            let tok = CancellationToken::new();
            inputs.cancel = Some(tok.clone());
            tok
        });
        Self {
            inputs,
            initial,
            engine_cancel,
            outcome_slot: None,
        }
    }

    /// Install an outcome slot. After `EngineDriver::start` completes, the
    /// slot contains `Some(Ok(TurnOutcome))` on a normal run or
    /// `Some(Err(_))` if `run_turn_with_outcome` itself failed.
    pub fn with_outcome_slot(mut self, slot: OutcomeSlot) -> Self {
        self.outcome_slot = Some(slot);
        self
    }
}

impl EngineDriver for TuiEngineDriver {
    fn start<'a>(
        self: Box<Self>,
        _prompt: String,
        handle: EngineHandle,
        cancel: CancellationFlag,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let TuiEngineDriver {
                mut inputs,
                initial,
                engine_cancel,
                outcome_slot,
            } = *self;

            // ── 1. Bridge the TUI's CancellationFlag → engine's CancellationToken.
            //
            // The TUI fires `CancellationFlag::cancel()` on user Ctrl-C / Esc.
            // The engine's select! arms watch `CancellationToken::cancelled()`,
            // so we translate by spawning a polling task. `select!`-style wait
            // isn't possible because CancellationFlag exposes only a sync
            // `is_cancelled()` call. 20ms is fast enough to feel instant while
            // staying cheap (one atomic load per poll).
            let bridge_cancel = engine_cancel.clone();
            let flag = cancel.clone();
            let bridge_done = bridge_cancel.clone();
            let bridge = tokio::spawn(async move {
                loop {
                    if flag.is_cancelled() {
                        bridge_cancel.cancel();
                        return;
                    }
                    if bridge_done.is_cancelled() {
                        // Engine already cancelled for some other reason (or
                        // completed + we cancelled ourselves below); stop polling.
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });

            // ── 2. Install the translating EventSink.
            let sink = build_sink(handle.events_tx.clone());
            inputs.event_sink = Some(sink);

            // ── 3. Run the turn.
            let outcome = run_turn_with_outcome(inputs, initial).await;

            // ── 4. Translate outcome → final TUI events. We must drive the
            // TUI sends *before* moving the outcome into the slot, so borrow
            // the message list and cancel reason out first.
            match &outcome {
                Ok(TurnOutcome::Completed { messages }) => {
                    emit_final_assistant(&handle, messages);
                    let _ = handle.events_tx.send(TuiEvent::TurnEnd {
                        reason: TurnEndReason::EndTurn,
                    });
                }
                Ok(TurnOutcome::Cancelled {
                    reason, messages, ..
                }) => {
                    // Any finalized text the partial assistant accumulated is
                    // the last message in `messages` (run_turn_with_outcome
                    // appends the partial before returning). Show it.
                    emit_final_assistant(&handle, messages);
                    let tui_reason = match reason {
                        CancelReason::UserInterrupt
                        | CancelReason::Timeout
                        | CancelReason::Internal => TurnEndReason::Cancelled,
                    };
                    let _ = handle.events_tx.send(TuiEvent::TurnEnd { reason: tui_reason });
                }
                Err(e) => {
                    let _ = handle.events_tx.send(TuiEvent::Error {
                        message: e.to_string(),
                    });
                    let _ = handle.events_tx.send(TuiEvent::TurnEnd {
                        reason: TurnEndReason::ProviderError,
                    });
                }
            }

            // ── 5. Hand the outcome to the CLI for persistence + exit code.
            if let Some(slot) = outcome_slot {
                if let Ok(mut g) = slot.lock() {
                    *g = Some(outcome);
                }
            }

            // Tell the polling bridge to exit (engine is done; no more cancels
            // are relevant). The bridge sees `bridge_done.is_cancelled()` →
            // returns. We cancel *our own* handle on `engine_cancel`, which the
            // engine has already released.
            engine_cancel.cancel();
            let _ = bridge.await;
        })
    }
}

/// Build the engine-side `EventSink` that translates each engine `TurnEvent`
/// to a TUI `TurnEvent` and pushes it onto `events_tx`. The sink tracks tool
/// start times so `ToolCallEnd` can emit an accurate elapsed duration — the
/// engine's `ToolCallEnd` carries only `(id, name, ok, summary_head)`.
fn build_sink(events_tx: tokio::sync::mpsc::UnboundedSender<TuiEvent>) -> EventSink {
    let started: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));
    Arc::new(move |ev: EngineEvent| {
        match ev {
            EngineEvent::TurnStart { turn_idx } => {
                let _ = events_tx.send(TuiEvent::TurnStart {
                    turn: turn_idx.saturating_add(1),
                });
            }
            EngineEvent::ToolCallStart { id, name, preview } => {
                if let Ok(mut m) = started.lock() {
                    m.insert(id.clone(), Instant::now());
                }
                let _ = events_tx.send(TuiEvent::ToolStart { id, name, preview });
            }
            EngineEvent::ToolCallEnd {
                id,
                name: _,
                ok,
                summary_head,
            } => {
                let elapsed = started
                    .lock()
                    .ok()
                    .and_then(|mut m| m.remove(&id))
                    .map(|t| t.elapsed())
                    .unwrap_or_default();
                let _ = events_tx.send(TuiEvent::ToolEnd {
                    id,
                    ok,
                    summary: summary_head,
                    elapsed,
                });
            }
            EngineEvent::Cancelled { .. } => {
                // Authoritative TurnEnd is emitted by the outer translator
                // once `run_turn_with_outcome` returns. Suppress here so the
                // TUI doesn't see two TurnEnds.
            }
        }
    })
}

/// Emit the final assistant text from the last Assistant message in
/// `messages`. If there is no assistant message (e.g. cancelled before any
/// text finalized), emit nothing — the TUI leaves scrollback as-is.
fn emit_final_assistant(handle: &EngineHandle, messages: &[Message]) {
    let Some(msg) = messages.iter().rev().find(|m| matches!(m.role, Role::Assistant)) else {
        return;
    };
    let mut text = String::new();
    for b in &msg.content {
        if let ContentBlock::Text { text: t, .. } = b {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(t);
        }
    }
    if !text.is_empty() {
        let _ = handle
            .events_tx
            .send(TuiEvent::AssistantTextDelta { text });
    }
    let _ = handle.events_tx.send(TuiEvent::AssistantMessageEnd);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use harness_proto::Usage;
    use tokio::sync::mpsc;

    /// The build_sink closure must translate a TurnStart and emit a 1-based turn.
    #[tokio::test]
    async fn sink_turn_start_is_one_based() {
        let (tx, mut rx) = mpsc::unbounded_channel::<TuiEvent>();
        let sink = build_sink(tx);
        sink(EngineEvent::TurnStart { turn_idx: 0 });
        let ev = rx.recv().await.expect("event");
        match ev {
            TuiEvent::TurnStart { turn } => assert_eq!(turn, 1),
            other => panic!("expected TurnStart, got {other:?}"),
        }
    }

    /// ToolCallStart is forwarded with matching fields and start time is
    /// recorded; ToolCallEnd then carries a non-zero elapsed duration and
    /// the summary_head field becomes `summary`.
    #[tokio::test]
    async fn sink_tool_lifecycle_elapsed_populated() {
        let (tx, mut rx) = mpsc::unbounded_channel::<TuiEvent>();
        let sink = build_sink(tx);
        sink(EngineEvent::ToolCallStart {
            id: "tu_1".into(),
            name: "Read".into(),
            preview: "Read foo.rs".into(),
        });
        // Introduce measurable delay so `elapsed > 0` is reliable on CI.
        std::thread::sleep(Duration::from_millis(5));
        sink(EngineEvent::ToolCallEnd {
            id: "tu_1".into(),
            name: "Read".into(),
            ok: true,
            summary_head: "42 lines".into(),
        });

        let start = rx.recv().await.unwrap();
        match start {
            TuiEvent::ToolStart { id, name, preview } => {
                assert_eq!(id, "tu_1");
                assert_eq!(name, "Read");
                assert_eq!(preview, "Read foo.rs");
            }
            other => panic!("expected ToolStart, got {other:?}"),
        }

        let end = rx.recv().await.unwrap();
        match end {
            TuiEvent::ToolEnd {
                id,
                ok,
                summary,
                elapsed,
            } => {
                assert_eq!(id, "tu_1");
                assert!(ok);
                assert_eq!(summary, "42 lines");
                assert!(
                    elapsed >= Duration::from_millis(5),
                    "expected elapsed >= 5ms, got {elapsed:?}"
                );
            }
            other => panic!("expected ToolEnd, got {other:?}"),
        }
    }

    /// ToolCallEnd with a tool_use id we never saw a Start for emits
    /// `elapsed = 0` rather than panicking — graceful degradation matters
    /// because a provider could in principle send end-without-start.
    #[tokio::test]
    async fn sink_tool_end_without_start_zero_elapsed() {
        let (tx, mut rx) = mpsc::unbounded_channel::<TuiEvent>();
        let sink = build_sink(tx);
        sink(EngineEvent::ToolCallEnd {
            id: "unknown".into(),
            name: "Bash".into(),
            ok: false,
            summary_head: "nope".into(),
        });
        match rx.recv().await.unwrap() {
            TuiEvent::ToolEnd { elapsed, ok, .. } => {
                assert_eq!(elapsed, Duration::from_secs(0));
                assert!(!ok);
            }
            other => panic!("expected ToolEnd, got {other:?}"),
        }
    }

    /// The engine's `Cancelled` event is intentionally *not* forwarded — the
    /// TUI's authoritative TurnEnd comes from `TuiEngineDriver::start`
    /// translating the outer `TurnOutcome`. Forwarding here would cause a
    /// double TurnEnd in the TUI channel.
    #[tokio::test]
    async fn sink_suppresses_cancelled() {
        let (tx, mut rx) = mpsc::unbounded_channel::<TuiEvent>();
        let sink = build_sink(tx);
        sink(EngineEvent::Cancelled {
            reason: CancelReason::UserInterrupt,
        });
        // Drop the sink so the receiver wakes on the last sender going away.
        drop(sink);
        assert!(rx.recv().await.is_none(), "Cancelled should produce no TUI event");
    }

    /// `emit_final_assistant` collects Text blocks from the newest Assistant
    /// message into one `AssistantTextDelta` followed by `AssistantMessageEnd`.
    #[tokio::test]
    async fn emit_final_assistant_single_delta_plus_end() {
        let (tx, mut rx) = mpsc::unbounded_channel::<TuiEvent>();
        let (perm_tx, _perm_rx) = mpsc::unbounded_channel();
        let handle = EngineHandle {
            events_tx: tx,
            permission_tx: perm_tx,
        };
        let messages = vec![
            Message::user("hi"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "first para".into(),
                        cache_control: None,
                    },
                    ContentBlock::Text {
                        text: "second para".into(),
                        cache_control: None,
                    },
                ],
                usage: Some(Usage::default()),
            },
        ];
        emit_final_assistant(&handle, &messages);

        match rx.recv().await.unwrap() {
            TuiEvent::AssistantTextDelta { text } => {
                assert!(text.contains("first para"));
                assert!(text.contains("second para"));
            }
            other => panic!("expected AssistantTextDelta, got {other:?}"),
        }
        match rx.recv().await.unwrap() {
            TuiEvent::AssistantMessageEnd => {}
            other => panic!("expected AssistantMessageEnd, got {other:?}"),
        }
    }

    /// If the message history has no assistant message (e.g. cancelled before
    /// any text finalized), the helper emits nothing — the TUI simply leaves
    /// `pending_assistant` as-is and the outer TurnEnd closes out the turn.
    #[tokio::test]
    async fn emit_final_assistant_no_assistant_message_noop() {
        let (tx, mut rx) = mpsc::unbounded_channel::<TuiEvent>();
        let (perm_tx, _perm_rx) = mpsc::unbounded_channel();
        let handle = EngineHandle {
            events_tx: tx,
            permission_tx: perm_tx,
        };
        let messages = vec![Message::user("hi")];
        emit_final_assistant(&handle, &messages);
        drop(handle);
        // Without an assistant message, we emit nothing — rx should close
        // cleanly with no pending events.
        assert!(rx.recv().await.is_none(), "expected no events when no assistant");
    }
}
