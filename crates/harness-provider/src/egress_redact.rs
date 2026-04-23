//! Opt-in egress redaction.
//!
//! Problem (see `docs/security/egress-redaction.md`): `harness-mem` redacts
//! session JSONL at write time, so the on-disk transcript is clean — but the
//! same tool outputs that get scrubbed before disk are sent **raw** to the
//! LLM provider API on the next turn's request body. If a shell command
//! prints an API key, the provider sees it.
//!
//! This module provides a thin egress hook that each provider applies to the
//! message slice right before wire-format construction. It reuses
//! `harness_mem::redact::redact_str` so patterns stay in lockstep with the
//! on-disk path — we never reimplement regexes here.
//!
//! Scope (deliberately narrow):
//!   - `ContentBlock::ToolResult.content` — primary leak vector.
//!   - `ContentBlock::Text` on assistant messages — the model may echo a
//!     secret it read from a tool output, so we scrub those too.
//!   - User `Text` blocks are **untouched**: the user typed them, so they
//!     already know what's in them. Redacting user prompts would break
//!     legitimate flows ("use this token to call X").
//!   - `ContentBlock::ToolUse.input` is untouched: the model synthesized it
//!     from earlier (already-redacted) context; redacting it here would risk
//!     double-mangling and does not protect a new leak vector.
//!
//! Enabled only when the caller passes `enabled = true` (set by the provider
//! from its `redact_egress` config flag). Default off.

use harness_proto::{ContentBlock, Message, Role};

/// Return a fresh Vec with redaction applied to the opted-in block kinds.
/// A no-op when `enabled == false` — messages are cloned unchanged so the
/// caller can pass `&Vec<Message>` without special-casing.
#[must_use]
pub(crate) fn maybe_redact_messages(messages: &[Message], enabled: bool) -> Vec<Message> {
    if !enabled {
        return messages.to_vec();
    }
    messages.iter().map(redact_message).collect()
}

fn redact_message(m: &Message) -> Message {
    let content = m.content.iter().map(|b| redact_block(m.role, b)).collect();
    Message {
        role: m.role,
        content,
        usage: m.usage,
    }
}

fn redact_block(role: Role, block: &ContentBlock) -> ContentBlock {
    match block {
        ContentBlock::Text {
            text,
            cache_control,
        } => {
            // User text is intentionally untouched — the user typed it.
            if matches!(role, Role::Assistant) {
                let red = harness_mem::redact::redact_str(text);
                if red.as_str() != text.as_str() {
                    return ContentBlock::Text {
                        text: red,
                        cache_control: *cache_control,
                    };
                }
            }
            block.clone()
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            cache_control,
        } => {
            let red = harness_mem::redact::redact_str(content);
            if red.as_str() == content.as_str() {
                block.clone()
            } else {
                ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: red,
                    is_error: *is_error,
                    cache_control: *cache_control,
                }
            }
        }
        // ToolUse.input is out of scope — see module docstring.
        ContentBlock::ToolUse { .. } => block.clone(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    const FAKE_KEY: &str = "sk-ant-api03-abcdefghij1234567890XYZ";
    const FAKE_AWS: &str = "AKIAIOSFODNN7EXAMPLE";

    fn tool_result_msg(id: &str, body: &str) -> Message {
        Message::user_tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: body.into(),
            is_error: false,
            cache_control: None,
        }])
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

    #[test]
    fn disabled_is_identity() {
        let msgs = vec![tool_result_msg("t1", &format!("key {FAKE_KEY}"))];
        let out = maybe_redact_messages(&msgs, false);
        match &out[0].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert!(content.contains(FAKE_KEY), "must leak when disabled");
            }
            _ => panic!("unexpected block"),
        }
    }

    #[test]
    fn enabled_scrubs_tool_result() {
        let msgs = vec![tool_result_msg("t1", &format!("key {FAKE_KEY}"))];
        let out = maybe_redact_messages(&msgs, true);
        match &out[0].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert!(!content.contains(FAKE_KEY), "leaked: {content}");
                assert!(content.contains("[REDACTED:sk]"));
            }
            _ => panic!("unexpected block"),
        }
    }

    #[test]
    fn enabled_scrubs_assistant_text() {
        let msgs = vec![assistant_text(&format!(
            "the key is {FAKE_KEY} as returned"
        ))];
        let out = maybe_redact_messages(&msgs, true);
        match &out[0].content[0] {
            ContentBlock::Text { text, .. } => {
                assert!(!text.contains(FAKE_KEY), "leaked: {text}");
                assert!(text.contains("[REDACTED:sk]"));
            }
            _ => panic!("unexpected block"),
        }
    }

    #[test]
    fn enabled_leaves_user_text_alone() {
        // User typed the key — we trust the user to know what's in their own
        // prompt. Only tool_result + assistant text get scrubbed.
        let user_msg = Message::user(format!("please use my key {FAKE_KEY}"));
        let out = maybe_redact_messages(&[user_msg], true);
        match &out[0].content[0] {
            ContentBlock::Text { text, .. } => {
                assert!(
                    text.contains(FAKE_KEY),
                    "user text must NOT be redacted (policy): {text}"
                );
            }
            _ => panic!("unexpected block"),
        }
    }

    #[test]
    fn enabled_leaves_tool_use_input_alone() {
        // ToolUse.input is out of scope — the model synthesized it from
        // (already-redacted) prior context. Redacting here would risk
        // double-mangling without adding protection.
        let assistant = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: json!({"command": format!("echo {FAKE_AWS}")}),
                cache_control: None,
            }],
            usage: None,
        };
        let out = maybe_redact_messages(&[assistant], true);
        match &out[0].content[0] {
            ContentBlock::ToolUse { input, .. } => {
                assert!(
                    input.to_string().contains(FAKE_AWS),
                    "ToolUse.input must stay untouched at egress: {input}"
                );
            }
            _ => panic!("unexpected block"),
        }
    }

    #[test]
    fn clean_messages_byte_stable() {
        // No secrets → output clones are semantically identical.
        let msgs = vec![
            Message::user("hi there"),
            assistant_text("hello, how can I help?"),
            tool_result_msg("t1", "no secrets in here"),
        ];
        let out = maybe_redact_messages(&msgs, true);
        for (a, b) in msgs.iter().zip(out.iter()) {
            let ja = serde_json::to_value(a).unwrap();
            let jb = serde_json::to_value(b).unwrap();
            assert_eq!(ja, jb, "clean messages must round-trip unchanged");
        }
    }
}
