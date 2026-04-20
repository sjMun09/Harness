//! Engine smoke tests migrated from `engine.rs`'s inline `#[cfg(test)]`
//! module onto `harness-testkit`. These two tests used to roll their own
//! `MockProvider` / `ChanProvider`; the testkit now owns that plumbing.
//!
//! They live here (integration tests) rather than in the inline `tests`
//! module because of a Rust build-graph constraint: `harness-testkit`
//! depends on `harness-core`, so adding `harness-testkit` as a dev-dep
//! inside `harness-core`'s `#[cfg(test)]` module would cause cargo to
//! compile `harness-core` twice (once as testkit's dep, once with
//! `--cfg test`), producing two incompatible `Provider` traits. Integration
//! tests avoid that — the lib is compiled once, normally, and shared.

use std::sync::Arc;

use harness_core::engine::{
    run_turn, run_turn_with_outcome, CancelReason, EngineInputs, TurnOutcome,
};
use harness_core::hooks::HookDispatcher;
use harness_core::plan_gate::PlanGateState;
use harness_core::ToolCtx;
use harness_perm::PermissionSnapshot;
use harness_proto::{ContentBlock, Message, Role, SessionId, StopReason};
use harness_testkit::{
    message_delta, message_start, message_stop, no_tools, text_event, MockProvider,
};
use tokio_util::sync::CancellationToken;

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

/// Replaces the original `engine::tests::text_only_terminates` — a one-turn
/// scripted run that emits a single Text block and ends with `EndTurn`.
#[tokio::test]
async fn text_only_terminates() {
    let mut events = vec![message_start("m")];
    events.extend(text_event("hi"));
    events.push(message_delta(StopReason::EndTurn));
    events.push(message_stop());

    let provider: Arc<MockProvider> = MockProvider::scripted(vec![events]);
    let dir = tempfile::tempdir().unwrap();
    let out = run_turn(
        EngineInputs {
            provider,
            tools: no_tools(),
            system: "sys".into(),
            ctx: mk_ctx(dir.path()),
            max_turns: 3,
            plan_gate: PlanGateState::default(),
            event_sink: None,
            cancel: None,
        },
        vec![Message::user("hello")],
    )
    .await
    .unwrap();

    assert_eq!(out.len(), 2);
    match &out[1].content[0] {
        ContentBlock::Text { text, .. } => assert_eq!(text, "hi"),
        other => panic!("expected text, got {other:?}"),
    }
}

/// Replaces the original `engine::tests::cancel_before_stream_returns_empty_partial`.
/// Uses `MockProvider::channel` to build a provider that blocks on the sender
/// until cancellation, proving `drive_one_turn` never opens the stream when
/// the cancel token was already fired.
#[tokio::test]
async fn cancel_before_stream_returns_empty_partial() {
    let (provider, _tx) = MockProvider::channel();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let dir = tempfile::tempdir().unwrap();

    let outcome = run_turn_with_outcome(
        EngineInputs {
            provider,
            tools: no_tools(),
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
            reason,
            partial_assistant,
            messages,
        } => {
            assert_eq!(reason, CancelReason::UserInterrupt);
            assert!(partial_assistant.is_none());
            assert_eq!(messages.len(), 1);
            assert!(matches!(messages[0].role, Role::User));
        }
        TurnOutcome::Completed { .. } => panic!("expected Cancelled, got Completed"),
    }
}
