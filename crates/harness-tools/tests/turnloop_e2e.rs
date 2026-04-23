//! End-to-end turn-loop integration tests.
//!
//! Unit tests for each stage of the turn loop (stream parser, finalizer,
//! tool dispatch) already live next to their sources. What was missing:
//! a test that drives the full user → provider → tool → tool_result →
//! next provider turn → final message loop *for real*, against real
//! tools, without a live API key or Ollama.
//!
//! These tests pair `harness_testkit::RecordingProvider` (a scripted
//! `Provider` impl that also records each inbound request) with real
//! `ReadTool`/`WriteTool` from this crate and the real `run_turn`
//! engine. Every filesystem side effect happens inside a `TempDir`; no
//! network calls; no user-home touching.

use std::path::Path;
use std::sync::{Arc, Mutex};

use harness_core::engine::{run_turn, EngineInputs};
use harness_core::hooks::HookDispatcher;
use harness_core::plan_gate::PlanGateState;
use harness_core::{AskAnswer, AskPrompt, Tool, ToolCtx};
use harness_perm::{PermissionSnapshot, Rule};
use harness_proto::{ContentBlock, Message, Role, SessionId, StopReason};
use harness_testkit::{
    message_delta, message_start, message_stop, text_event, tool_use_event, CallSnapshot,
    RecordingProvider,
};
use harness_tools::{ReadTool, WriteTool};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

// ────────────────────────────────────────────────────────────────────
// Shared test plumbing
// ────────────────────────────────────────────────────────────────────

fn mk_ctx(dir: &Path) -> ToolCtx {
    ToolCtx {
        cwd: dir.to_path_buf(),
        session_id: SessionId::new("e2e"),
        cancel: CancellationToken::new(),
        permission: PermissionSnapshot::default(),
        hooks: HookDispatcher::default(),
        subagent: None,
        depth: 0,
        tx: None,
        ask_prompt: None,
    }
}

fn allow_all(tools: &[&str]) -> PermissionSnapshot {
    let rules: Vec<Rule> = tools
        .iter()
        .map(|t| Rule::parse(&format!("{t}(**)")).expect("rule parses"))
        .collect();
    PermissionSnapshot::new(vec![], rules, vec![])
}

fn assistant_tool_use_turn(id: &str, name: &str, input: &Value) -> Vec<harness_core::StreamEvent> {
    let mut ev = vec![message_start("m-tu")];
    ev.extend(tool_use_event(id, name, input));
    ev.push(message_delta(StopReason::ToolUse));
    ev.push(message_stop());
    ev
}

fn assistant_final_text_turn(text: &str) -> Vec<harness_core::StreamEvent> {
    let mut ev = vec![message_start("m-final")];
    ev.extend(text_event(text));
    ev.push(message_delta(StopReason::EndTurn));
    ev.push(message_stop());
    ev
}

fn find_last_tool_result(msgs: &[Message]) -> Option<&ContentBlock> {
    msgs.iter()
        .rev()
        .flat_map(|m| m.content.iter())
        .find(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

// ────────────────────────────────────────────────────────────────────
// 1) Golden path: one Read → final text
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn golden_path_read_then_final_text() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("foo.txt");
    std::fs::write(&file, "hello from foo\n").unwrap();

    let read_input = json!({"file_path": file.to_string_lossy()});
    let (provider, calls) = RecordingProvider::new(vec![
        assistant_tool_use_turn("tu_read_1", "Read", &read_input),
        assistant_final_text_turn("the file says: hello from foo"),
    ]);

    let mut ctx = mk_ctx(dir.path());
    ctx.permission = allow_all(&["Read"]);

    let msgs = run_turn(
        EngineInputs {
            provider,
            tools: vec![Arc::new(ReadTool::default()) as Arc<dyn Tool>],
            system: "be helpful".into(),
            ctx,
            max_turns: 3,
            plan_gate: PlanGateState::default(),
            event_sink: None,
            cancel: None,
        },
        vec![Message::user("read foo.txt and tell me what it says")],
    )
    .await
    .expect("run_turn completes");

    // Final assistant text.
    let last = msgs.last().expect("at least one message");
    assert!(matches!(last.role, Role::Assistant));
    match &last.content[0] {
        ContentBlock::Text { text, .. } => {
            assert_eq!(text, "the file says: hello from foo");
        }
        other => panic!("expected final Text, got {other:?}"),
    }

    // Read tool was called exactly once — the provider was asked to stream
    // twice (call turn + final turn), the first call carries only the user
    // message, the second call's history ends with a tool_result carrying
    // the file contents.
    let calls: Vec<CallSnapshot> = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "provider opened the stream twice");

    // Second call — assert the tool_result made it into the next turn's
    // history and that it references our tool_use_id + file text.
    let second_last = calls[1].messages.last().expect("non-empty second call");
    let result_block = second_last
        .content
        .iter()
        .find_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } if tool_use_id == "tu_read_1" => Some((content.clone(), *is_error)),
            _ => None,
        })
        .expect("tool_result for tu_read_1 in second inbound history");
    assert!(!result_block.1, "read should succeed: {}", result_block.0);
    assert!(
        result_block.0.contains("hello from foo"),
        "tool_result should echo file contents, got: {}",
        result_block.0
    );

    // Sanity: exactly one ToolResult across the whole history (Read called once).
    let tool_results_total = msgs
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
        .count();
    assert_eq!(tool_results_total, 1, "Read should be called exactly once");
}

// ────────────────────────────────────────────────────────────────────
// 2) Multi-tool turn: two ToolUse blocks in parallel → two results
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn multi_tool_turn_dispatches_both_and_threads_results() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, "AAA\n").unwrap();
    std::fs::write(&b, "BBB\n").unwrap();

    // Two tool_use blocks (index 0 and 1) in the SAME assistant turn.
    use harness_core::{ContentBlockHeader, ContentDelta, StreamEvent};
    use harness_proto::Usage;

    let in_a = json!({"file_path": a.to_string_lossy()});
    let in_b = json!({"file_path": b.to_string_lossy()});
    let call_turn = vec![
        StreamEvent::MessageStart {
            message_id: "m-multi".into(),
            usage: Usage::default(),
        },
        // index 0 — Read(a)
        StreamEvent::ContentBlockStart {
            index: 0,
            block: ContentBlockHeader::ToolUse {
                id: "tu_a".into(),
                name: "Read".into(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::InputJson(in_a.to_string().into_bytes()),
        },
        StreamEvent::ContentBlockStop { index: 0 },
        // index 1 — Read(b)
        StreamEvent::ContentBlockStart {
            index: 1,
            block: ContentBlockHeader::ToolUse {
                id: "tu_b".into(),
                name: "Read".into(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 1,
            delta: ContentDelta::InputJson(in_b.to_string().into_bytes()),
        },
        StreamEvent::ContentBlockStop { index: 1 },
        StreamEvent::MessageDelta {
            stop_reason: Some(StopReason::ToolUse),
            usage: Usage::default(),
        },
        StreamEvent::MessageStop,
    ];

    let (provider, calls) =
        RecordingProvider::new(vec![call_turn, assistant_final_text_turn("both read")]);

    let mut ctx = mk_ctx(dir.path());
    ctx.permission = allow_all(&["Read"]);

    let msgs = run_turn(
        EngineInputs {
            provider,
            tools: vec![Arc::new(ReadTool::default()) as Arc<dyn Tool>],
            system: "sys".into(),
            ctx,
            max_turns: 3,
            plan_gate: PlanGateState::default(),
            event_sink: None,
            cancel: None,
        },
        vec![Message::user("read both a and b")],
    )
    .await
    .expect("run_turn completes");

    // Exactly one user+tool_results message with two ToolResults in input order.
    let tr_msg = msgs
        .iter()
        .find(|m| {
            m.role == Role::User
                && m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        })
        .expect("tool_results message present");
    let ids: Vec<&str> = tr_msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["tu_a", "tu_b"], "order must match input");

    // Both results threaded into the next provider call (provider call #2).
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    let last_msg = calls[1].messages.last().unwrap();
    let result_contents: Vec<String> = last_msg
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(result_contents.len(), 2);
    assert!(result_contents[0].contains("AAA"));
    assert!(result_contents[1].contains("BBB"));
}

// ────────────────────────────────────────────────────────────────────
// 3) Tool error propagation
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tool_error_surfaces_to_next_turn_then_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    // Path that doesn't exist → ReadTool returns ToolError::Io.
    let missing = dir.path().join("does-not-exist.txt");
    let read_input = json!({"file_path": missing.to_string_lossy()});

    let (provider, calls) = RecordingProvider::new(vec![
        assistant_tool_use_turn("tu_bad", "Read", &read_input),
        assistant_final_text_turn("gracefully giving up"),
    ]);

    let mut ctx = mk_ctx(dir.path());
    ctx.permission = allow_all(&["Read"]);

    let msgs = run_turn(
        EngineInputs {
            provider,
            tools: vec![Arc::new(ReadTool::default()) as Arc<dyn Tool>],
            system: "sys".into(),
            ctx,
            max_turns: 3,
            plan_gate: PlanGateState::default(),
            event_sink: None,
            cancel: None,
        },
        vec![Message::user("read a missing file")],
    )
    .await
    .expect("run_turn should complete cleanly even after tool error");

    // The tool_result in the final history carries is_error=true.
    let result = find_last_tool_result(&msgs).expect("tool_result present");
    match result {
        ContentBlock::ToolResult {
            is_error, content, ..
        } => {
            assert!(*is_error, "Read on missing file should error");
            assert!(!content.is_empty(), "error message should be non-empty");
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }

    // Provider's second call saw the error block in its inbound history.
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    let last = calls[1].messages.last().unwrap();
    let saw_error = last.content.iter().any(|b| {
        matches!(
            b,
            ContentBlock::ToolResult {
                tool_use_id, is_error: true, ..
            } if tool_use_id == "tu_bad"
        )
    });
    assert!(saw_error, "provider should see is_error=true in history");

    // Final assistant message is the scripted text — loop exited cleanly.
    let last_msg = msgs.last().unwrap();
    assert!(matches!(last_msg.role, Role::Assistant));
    match &last_msg.content[0] {
        ContentBlock::Text { text, .. } => assert_eq!(text, "gracefully giving up"),
        other => panic!("expected final Text, got {other:?}"),
    }
}

// ────────────────────────────────────────────────────────────────────
// 4) MaxTokens mid-tool
//
// BEHAVIOR DOCUMENTED (not necessarily "correct"): the engine treats any
// non-ToolUse stop as "this turn is done — return to caller" (engine.rs:256
// `if !matches!(stop_reason, Some(StopReason::ToolUse)) { return Completed }`).
// An incomplete tool_use block (start+delta but no ContentBlockStop) sits in
// `acc.blocks` and never moves to `acc.finalized`; `blocks_in_order()` only
// drains the finalized map, so the half-open call is *silently dropped*.
//
// Observable result: the assistant message carries ZERO content blocks, the
// loop exits (no second provider call), and the user sees an empty reply.
// See engine.rs:580-603 (`Accumulated::blocks_in_order` / `finalized_only`).
//
// If this ever changes to "surface a tool_result with is_error=true" or
// "request continuation", update this test. For now it locks in the
// *actual* behavior so we notice any regression.
// ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn max_tokens_mid_tool_drops_incomplete_call_and_exits() {
    use harness_core::{ContentBlockHeader, ContentDelta, StreamEvent};
    use harness_proto::Usage;

    let dir = tempfile::tempdir().unwrap();
    // Incomplete tool_use: Start + partial Delta, no Stop, then MaxTokens.
    let truncated_turn = vec![
        StreamEvent::MessageStart {
            message_id: "m-trunc".into(),
            usage: Usage::default(),
        },
        StreamEvent::ContentBlockStart {
            index: 0,
            block: ContentBlockHeader::ToolUse {
                id: "tu_half".into(),
                name: "Read".into(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::InputJson("{\"file_path\":\"/tmp".into()),
        },
        // No ContentBlockStop — the model was cut off mid-tool.
        StreamEvent::MessageDelta {
            stop_reason: Some(StopReason::MaxTokens),
            usage: Usage::default(),
        },
        StreamEvent::MessageStop,
    ];

    let (provider, calls) = RecordingProvider::new(vec![truncated_turn]);
    let mut ctx = mk_ctx(dir.path());
    ctx.permission = allow_all(&["Read"]);

    let msgs = run_turn(
        EngineInputs {
            provider,
            tools: vec![Arc::new(ReadTool::default()) as Arc<dyn Tool>],
            system: "sys".into(),
            ctx,
            max_turns: 3,
            plan_gate: PlanGateState::default(),
            event_sink: None,
            cancel: None,
        },
        vec![Message::user("read something")],
    )
    .await
    .expect("run_turn should still complete under MaxTokens");

    // Provider was called exactly once — no follow-up turn happens because
    // stop_reason != ToolUse.
    let calls_snap = calls.lock().unwrap().clone();
    assert_eq!(
        calls_snap.len(),
        1,
        "no retry / second turn on MaxTokens mid-tool"
    );

    // The loop pushes the assistant message even when its content is empty.
    // Shape: [user, assistant(empty content)].
    assert_eq!(msgs.len(), 2);
    let last = msgs.last().unwrap();
    assert!(matches!(last.role, Role::Assistant));
    assert!(
        last.content.is_empty(),
        "incomplete tool_use must be dropped — got {:?}",
        last.content
    );

    // No ToolResults anywhere — the tool was never dispatched.
    let any_tool_result = msgs
        .iter()
        .flat_map(|m| m.content.iter())
        .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
    assert!(
        !any_tool_result,
        "no tool should have been dispatched for a half-open tool_use"
    );
}

// ────────────────────────────────────────────────────────────────────
// 5) Ask-path: tool triggers Ask, AskPrompt says Yes, loop continues
// ────────────────────────────────────────────────────────────────────

/// Canned answerer: returns a scripted sequence of [`AskAnswer`]s. Used by
/// the ask-path test to simulate a user confirming at the interactive
/// prompt.
#[derive(Debug)]
struct CannedAskPrompt {
    answers: Mutex<std::collections::VecDeque<AskAnswer>>,
    calls: Mutex<Vec<(String, Value)>>,
}

impl CannedAskPrompt {
    fn new(answers: Vec<AskAnswer>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers.into_iter().collect()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

impl AskPrompt for CannedAskPrompt {
    fn ask(&self, tool: &str, input: &Value) -> AskAnswer {
        self.calls
            .lock()
            .unwrap()
            .push((tool.to_string(), input.clone()));
        self.answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(AskAnswer::No)
    }
}

#[tokio::test]
async fn ask_path_user_says_yes_loop_continues() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("new.txt");

    let write_input = json!({
        "file_path": target.to_string_lossy(),
        "content": "written via ask-path",
    });

    let (provider, _calls) = RecordingProvider::new(vec![
        assistant_tool_use_turn("tu_write", "Write", &write_input),
        assistant_final_text_turn("wrote the file"),
    ]);

    // Permission setup: empty snapshot → no matching allow, no matching deny,
    // no explicit ask → default is Decision::Ask. Paired with a CannedAskPrompt
    // saying Yes, the write should go through.
    let ask = CannedAskPrompt::new(vec![AskAnswer::Yes]);
    let mut ctx = mk_ctx(dir.path());
    ctx.permission = PermissionSnapshot::default();
    ctx.ask_prompt = Some(ask.clone() as Arc<dyn AskPrompt>);

    let msgs = run_turn(
        EngineInputs {
            provider,
            tools: vec![Arc::new(WriteTool::default()) as Arc<dyn Tool>],
            system: "sys".into(),
            ctx,
            max_turns: 3,
            plan_gate: PlanGateState::default(),
            event_sink: None,
            cancel: None,
        },
        vec![Message::user("please write a new file")],
    )
    .await
    .expect("run_turn completes");

    // AskPrompt was invoked exactly once, for "Write".
    let asked = ask.calls();
    assert_eq!(asked.len(), 1, "AskPrompt should be consulted once");
    assert_eq!(asked[0].0, "Write");
    assert_eq!(asked[0].1, write_input);

    // File actually landed on disk — the tool ran after Ask→Yes.
    assert!(
        target.exists(),
        "Write tool should have created {}",
        target.display()
    );

    // Loop reached the final scripted Text.
    let last = msgs.last().unwrap();
    assert!(matches!(last.role, Role::Assistant));
    match &last.content[0] {
        ContentBlock::Text { text, .. } => assert_eq!(text, "wrote the file"),
        other => panic!("expected final Text, got {other:?}"),
    }

    // The tool_result for the Write call should NOT be an error.
    let tr = find_last_tool_result(&msgs).unwrap();
    match tr {
        ContentBlock::ToolResult {
            is_error, content, ..
        } => {
            assert!(!*is_error, "Write after Ask→Yes should succeed: {content}");
        }
        _ => unreachable!(),
    }
}
