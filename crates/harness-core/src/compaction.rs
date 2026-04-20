//! Session turn auto-compaction. PLAN §3.2 / §5.11.
//!
//! When a session's accumulated message history would overflow the model's
//! context window, drop the oldest turns and replace them with a single
//! synthetic user note. The helper is a pure function over `&[Message]` — it
//! does not touch the provider, the session file, or the engine directly. The
//! engine calls it before each turn's outgoing request when
//! `Budget::exceeded()` trips, swaps the returned slice in as the request
//! body, and appends a `{"type":"meta","event":"compaction","kept_from_turn":N}`
//! record to the session JSONL (PLAN §5.11).
//!
//! ## Invariants
//!
//! 1. **Pair preservation.** A `tool_use` assistant block and its matching
//!    `tool_result` user block must stay together. Boundaries never split a
//!    pair — if the cutoff would fall between `tool_use` and `tool_result`,
//!    we drop both (extending the drop region backward one turn).
//!
//! 2. **Fresh-user boundary.** Every retained prefix must start with a
//!    fresh user message (role=User, no `ToolResult` blocks). A tool-results
//!    message is never a turn boundary, because it semantically belongs to
//!    the preceding assistant turn.
//!
//! 3. **`keep_recent_turns` floor.** At least this many trailing turns are
//!    always retained, even when the budget would suggest a deeper cut —
//!    the model needs immediate context to answer coherently. Dropping below
//!    the floor is a no-op (we return the full history unchanged).
//!
//! 4. **Synthetic placeholder.** When at least one turn is dropped, the
//!    returned slice is prefixed with a synthetic `User+Text` message:
//!    `"[N earlier turns elided to fit context budget]"`. This keeps the
//!    wire sequence valid (first message is user) even when a long tool
//!    chain existed at the start.
//!
//! ## Not in scope (iter 3)
//!
//! - LLM-backed summarization. This helper only **truncates**; an iter 3
//!   variant may call back into the provider to condense dropped turns into
//!   a semantic summary. The placeholder's shape is stable so that future
//!   variant can swap the text without breaking the call site.

use harness_proto::{ContentBlock, Message, Role};
use harness_token::TokenEstimator;

/// Knobs supplied by the engine. `target_tokens` is the **soft ceiling** —
/// compaction retains as many recent turns as fit within it, plus the
/// `keep_recent_turns` floor regardless of budget.
#[derive(Debug, Clone, Copy)]
pub struct CompactionOptions {
    /// Soft ceiling on the post-compaction history token count.
    pub target_tokens: usize,
    /// Minimum trailing turns to preserve even if the budget would drop
    /// more. The caller should set this to at least 2 (last user + assistant).
    pub keep_recent_turns: usize,
}

impl CompactionOptions {
    /// Sensible default for 200k-context Claude models: aim for 120k tokens
    /// of history, keep the last 4 turns always. Leaves ~80k for new output.
    #[must_use]
    pub fn default_for_200k() -> Self {
        Self {
            target_tokens: 120_000,
            keep_recent_turns: 4,
        }
    }
}

/// Result of a compaction pass. `kept_from_turn` is `Some(idx)` when the
/// input was modified — `idx` is the 0-based turn index (in the original
/// message slice's turn enumeration) that the retained prefix starts at,
/// which the engine writes into the session JSONL meta record. When nothing
/// changed, all fields indicate the no-op.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub messages: Vec<Message>,
    pub kept_from_turn: Option<usize>,
    pub dropped_turns: usize,
    pub synthetic_note: Option<String>,
}

impl CompactionResult {
    /// Was any history dropped?
    #[must_use]
    pub fn changed(&self) -> bool {
        self.dropped_turns > 0
    }
}

/// Compact `messages` according to `opts`. Pure — no I/O, no mutation of
/// the input slice. The returned `messages` Vec is either a clone of the
/// input (no-op) or a `[synthetic_note, retained_tail...]` sequence.
///
/// # Semantics
///
/// - Turns are delimited at "fresh" user messages (`Role::User` with at
///   least one non-`ToolResult` block — text or an empty content vec). A
///   user message carrying only `ToolResult` blocks is **not** a boundary;
///   it belongs to the preceding assistant turn's pair.
/// - Token counts per turn are summed from every message's `Text` and
///   `ToolResult` block contents plus `ToolUse.input` as rendered JSON.
///   `ToolUse.name` adds a tiny constant (name length / 4). Role overhead
///   is ignored — the estimator is already coarse and role markers shrink
///   in the actual wire format.
/// - The retained suffix is chosen greedily from the end: keep adding
///   turns until either the cumulative token count would exceed
///   `target_tokens` **or** there are no more turns to consider. The
///   `keep_recent_turns` floor overrides the budget — the last N turns
///   are always retained, even if they alone exceed `target_tokens` (the
///   alternative is an invalid truncation that strands a tool_use).
/// - When the full history fits, `messages` is returned unchanged
///   (cloned) and `dropped_turns == 0`.
pub fn compact(
    messages: &[Message],
    estimator: &dyn TokenEstimator,
    opts: &CompactionOptions,
) -> CompactionResult {
    let turn_starts = turn_start_indices(messages);
    let total_turns = turn_starts.len();

    // Edge cases: no fresh-user boundary found, or fewer turns than the
    // floor — nothing to drop.
    if total_turns == 0 || total_turns <= opts.keep_recent_turns {
        return no_op(messages);
    }

    // Token count per turn — turn `i` spans `turn_starts[i] ..
    // turn_starts.get(i+1).unwrap_or(messages.len())`.
    let mut per_turn_tokens: Vec<usize> = Vec::with_capacity(total_turns);
    for (i, start) in turn_starts.iter().enumerate() {
        let end = turn_starts.get(i + 1).copied().unwrap_or(messages.len());
        per_turn_tokens.push(count_turn_tokens(&messages[*start..end], estimator));
    }

    // Greedy from the tail: include turns while budget allows, but never
    // go below the `keep_recent_turns` floor.
    let mut retained_from = total_turns; // exclusive-left cursor into turn_starts
    let mut running = 0usize;
    for (i, tokens) in per_turn_tokens.iter().enumerate().rev() {
        // Turns currently retained (excluding the one we're about to decide
        // on). If this is still below the floor, we owe the floor and must
        // include `i` even when it overflows the token budget.
        let currently_retained = total_turns - retained_from;
        let under_floor = currently_retained < opts.keep_recent_turns;
        let fits = running.saturating_add(*tokens) <= opts.target_tokens;
        if under_floor || fits {
            retained_from = i;
            running = running.saturating_add(*tokens);
        } else {
            break;
        }
    }

    if retained_from == 0 {
        // Nothing dropped.
        return no_op(messages);
    }

    let retained_msg_start = turn_starts[retained_from];
    let retained = &messages[retained_msg_start..];
    let dropped = retained_from;
    let note = format!(
        "[{dropped} earlier turn{plural} elided to fit context budget]",
        plural = if dropped == 1 { "" } else { "s" },
    );

    let mut out = Vec::with_capacity(retained.len() + 1);
    out.push(Message::user(note.clone()));
    out.extend_from_slice(retained);

    CompactionResult {
        messages: out,
        kept_from_turn: Some(retained_from),
        dropped_turns: dropped,
        synthetic_note: Some(note),
    }
}

fn no_op(messages: &[Message]) -> CompactionResult {
    CompactionResult {
        messages: messages.to_vec(),
        kept_from_turn: None,
        dropped_turns: 0,
        synthetic_note: None,
    }
}

/// Indices of "fresh" user messages in `messages` — the turn boundaries.
/// A user message carrying only `ToolResult` blocks belongs to the prior
/// assistant turn and is NOT a boundary (invariant 2).
fn turn_start_indices(messages: &[Message]) -> Vec<usize> {
    let mut starts = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        if m.role == Role::User && !is_tool_results_only(m) {
            starts.push(i);
        }
    }
    starts
}

/// A user message with at least one content block and where every block is
/// a `ToolResult`. Empty content → false (treat as fresh boundary).
fn is_tool_results_only(m: &Message) -> bool {
    !m.content.is_empty()
        && m.content
            .iter()
            .all(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

fn count_turn_tokens(turn: &[Message], estimator: &dyn TokenEstimator) -> usize {
    turn.iter()
        .map(|m| count_message_tokens(m, estimator))
        .sum()
}

fn count_message_tokens(m: &Message, estimator: &dyn TokenEstimator) -> usize {
    m.content
        .iter()
        .map(|b| count_block_tokens(b, estimator))
        .sum()
}

fn count_block_tokens(b: &ContentBlock, estimator: &dyn TokenEstimator) -> usize {
    match b {
        ContentBlock::Text { text, .. } => estimator.count(text),
        ContentBlock::ToolUse { name, input, .. } => {
            let rendered = serde_json::to_string(input).unwrap_or_default();
            estimator.count(name) + estimator.count(&rendered)
        }
        ContentBlock::ToolResult { content, .. } => estimator.count(content),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_proto::{ContentBlock, Message, Role};
    use harness_token::NullEstimator;
    use serde_json::json;

    /// Word-count estimator — 1 token per word. Keeps per-turn math
    /// trivial to reason about, without depending on `cl100k_base` disk
    /// caches in CI.
    #[derive(Default)]
    struct WordEstimator;
    impl TokenEstimator for WordEstimator {
        fn count(&self, text: &str) -> usize {
            text.split_whitespace().count()
        }
    }

    fn user(text: &str) -> Message {
        Message::user(text)
    }

    fn assistant_text(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: text.into(),
                cache_control: None,
            }],
            usage: None,
        }
    }

    fn assistant_tool_use(id: &str, name: &str, input: serde_json::Value) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input,
                cache_control: None,
            }],
            usage: None,
        }
    }

    fn user_tool_result(id: &str, body: &str) -> Message {
        Message::user_tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: body.into(),
            is_error: false,
            cache_control: None,
        }])
    }

    #[test]
    fn no_op_when_under_budget() {
        let msgs = vec![
            user("hi"),
            assistant_text("hello"),
            user("again"),
            assistant_text("hi again"),
        ];
        let opts = CompactionOptions {
            target_tokens: 1_000,
            keep_recent_turns: 1,
        };
        let r = compact(&msgs, &WordEstimator, &opts);
        assert!(!r.changed());
        assert_eq!(r.messages.len(), 4);
        assert_eq!(r.kept_from_turn, None);
    }

    #[test]
    fn no_op_when_fewer_turns_than_floor() {
        // Single turn, floor of 2 — nothing to drop even if over budget.
        let long = "a very long prompt ".repeat(100);
        let msgs = vec![user(&long), assistant_text("ok")];
        let opts = CompactionOptions {
            target_tokens: 1, // absurdly small
            keep_recent_turns: 2,
        };
        let r = compact(&msgs, &WordEstimator, &opts);
        assert!(!r.changed());
    }

    #[test]
    fn drops_oldest_turn_when_over_budget() {
        // Three turns of 10 words each = 30 total. Budget 15 + floor 1 →
        // keep only the last turn (10 words <= 15).
        let pad = "word ".repeat(10);
        let msgs = vec![
            user(&pad),
            assistant_text("ok"),
            user(&pad),
            assistant_text("ok"),
            user(&pad),
            assistant_text("ok"),
        ];
        let opts = CompactionOptions {
            target_tokens: 15,
            keep_recent_turns: 1,
        };
        let r = compact(&msgs, &WordEstimator, &opts);
        assert!(r.changed());
        assert_eq!(r.dropped_turns, 2);
        assert_eq!(r.kept_from_turn, Some(2));
        // First message is the synthetic placeholder.
        assert_eq!(r.messages[0].role, Role::User);
        match &r.messages[0].content[0] {
            ContentBlock::Text { text, .. } => assert!(text.contains("2 earlier turns elided")),
            _ => panic!("expected Text placeholder"),
        }
        // Then the retained tail (last turn = 2 messages).
        assert_eq!(r.messages.len(), 3);
    }

    #[test]
    fn preserves_tool_use_pair_as_single_turn() {
        // Turn 1: user + assistant(ToolUse) + user(ToolResult) + assistant(text)
        // Turn 2: user + assistant
        //
        // The tool_result user message must NOT be treated as a boundary —
        // otherwise a cut could strand the ToolUse without its result.
        let msgs = vec![
            user("refactor the mapper"),
            assistant_tool_use("tu1", "Read", json!({"file_path": "a.xml"})),
            user_tool_result("tu1", "<mapper>...</mapper>"),
            assistant_text("done"),
            user("now the other file"),
            assistant_text("also done"),
        ];
        let starts = turn_start_indices(&msgs);
        // Boundaries at indices 0 and 4 only — not at 2 (tool_result user).
        assert_eq!(starts, vec![0, 4]);
    }

    #[test]
    fn cut_at_tool_pair_boundary_drops_whole_turn() {
        // Two turns; turn 1 has a ToolUse + ToolResult pair. Budget forces
        // a single-turn retain. The result must include the synthetic note
        // + only the 2 messages of turn 2. The pair from turn 1 is gone
        // entirely — no stranded ToolUse, no orphan ToolResult.
        let msgs = vec![
            user("do a bunch of reads"),
            assistant_tool_use("tu1", "Read", json!({"file_path": "a.xml"})),
            user_tool_result("tu1", "payload A ".repeat(20).trim()),
            assistant_text("read it"),
            user("short"),
            assistant_text("ok"),
        ];
        let opts = CompactionOptions {
            target_tokens: 5,
            keep_recent_turns: 1,
        };
        let r = compact(&msgs, &WordEstimator, &opts);
        assert!(r.changed());
        assert_eq!(r.dropped_turns, 1);
        // Retained = placeholder + (user short, assistant ok).
        assert_eq!(r.messages.len(), 3);
        // No ToolUse or ToolResult survives.
        for m in &r.messages {
            for b in &m.content {
                assert!(!matches!(b, ContentBlock::ToolUse { .. }));
                assert!(!matches!(b, ContentBlock::ToolResult { .. }));
            }
        }
    }

    #[test]
    fn floor_forces_retention_even_over_budget() {
        // 3 turns of 100 words each. Budget 10, floor 3 → retain all 3
        // turns because the floor overrides the budget.
        let pad = "word ".repeat(100);
        let msgs = vec![
            user(&pad),
            assistant_text("ok"),
            user(&pad),
            assistant_text("ok"),
            user(&pad),
            assistant_text("ok"),
        ];
        let opts = CompactionOptions {
            target_tokens: 10,
            keep_recent_turns: 3,
        };
        let r = compact(&msgs, &WordEstimator, &opts);
        assert!(!r.changed());
        assert_eq!(r.messages.len(), 6);
    }

    #[test]
    fn placeholder_grammar_singular_vs_plural() {
        let pad = "w ".repeat(10);
        // 4 turns, drop 1 → "1 earlier turn"
        let msgs = vec![
            user(&pad),
            assistant_text("a"),
            user(&pad),
            assistant_text("b"),
            user(&pad),
            assistant_text("c"),
            user(&pad),
            assistant_text("d"),
        ];
        let opts = CompactionOptions {
            target_tokens: 30,
            keep_recent_turns: 3,
        };
        let r = compact(&msgs, &WordEstimator, &opts);
        assert!(r.changed());
        assert_eq!(r.dropped_turns, 1);
        match &r.messages[0].content[0] {
            ContentBlock::Text { text, .. } => {
                assert!(text.contains("1 earlier turn elided"), "got: {text}");
                assert!(!text.contains("turns"), "singular form expected");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn tiktoken_estimator_does_not_panic() {
        // Sanity: the helper must accept any TokenEstimator impl,
        // including the null fallback. No tokenizer disk I/O here.
        let msgs = vec![user("hi"), assistant_text("there")];
        let opts = CompactionOptions {
            target_tokens: 1,
            keep_recent_turns: 1,
        };
        let r = compact(&msgs, &NullEstimator, &opts);
        // Floor of 1 + only 1 turn → no-op.
        assert!(!r.changed());
    }

    #[test]
    fn kept_from_turn_is_zero_based() {
        // 5 turns, drop 2 → kept_from_turn == 2 (0-indexed).
        let pad = "w ".repeat(5);
        let mut msgs = Vec::new();
        for _ in 0..5 {
            msgs.push(user(&pad));
            msgs.push(assistant_text("a"));
        }
        let opts = CompactionOptions {
            target_tokens: 18,
            keep_recent_turns: 1,
        };
        let r = compact(&msgs, &WordEstimator, &opts);
        assert!(r.changed());
        assert_eq!(r.kept_from_turn, Some(2));
    }

    #[test]
    fn empty_input_is_noop() {
        let r = compact(
            &[],
            &WordEstimator,
            &CompactionOptions {
                target_tokens: 1,
                keep_recent_turns: 1,
            },
        );
        assert!(!r.changed());
        assert!(r.messages.is_empty());
    }
}
