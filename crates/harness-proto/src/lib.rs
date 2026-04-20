//! Harness wire-protocol leaf crate.
//!
//! Per PLAN §5.1 / §5.2 / §3.1 MVP. `ContentBlock` is the reduced MVP set
//! (`Text | ToolUse | ToolResult`); `Thinking`/`Image` land in iter 2 as
//! additive variants. Iter 2 task #21 added per-variant
//! `cache_control: Option<CacheControl>` for Anthropic prompt caching;
//! the field is omitted from the wire when `None` so the byte shape is
//! byte-identical to iter-1 sessions (back-compat preserved).

#![forbid(unsafe_code)]

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Message author role. Matches Anthropic / OpenAI wire values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
}

/// MVP content-block enum. Wire shape uses `{"type": "...", ...fields}`.
///
/// Iter 2 adds `Thinking`, `RedactedThinking`, `Image` (additive variants);
/// per-variant `cache_control: Option<CacheControl>` is added on existing
/// variants — opt-in field with `skip_serializing_if = "Option::is_none"` +
/// `default` so the wire / session-JSONL byte shape is unchanged when absent
/// (back-compat preserved).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

/// Anthropic prompt-caching marker (PLAN §3.2). Wire shape:
/// `{"type": "ephemeral"}`. Attaching it to the last system block, the last
/// tool definition, or a message block opts that prefix into the cache.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CacheControl {
    Ephemeral,
}

impl ContentBlock {
    pub fn as_tool_use(&self) -> Option<ToolUseRef<'_>> {
        match self {
            Self::ToolUse {
                id, name, input, ..
            } => Some(ToolUseRef { id, name, input }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ToolUseRef<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub input: &'a Value,
}

/// One message in a conversation. `usage` populated only on assistant messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.into(),
                cache_control: None,
            }],
            usage: None,
        }
    }

    pub fn assistant_empty() -> Self {
        Self {
            role: Role::Assistant,
            content: Vec::new(),
            usage: None,
        }
    }

    /// Build the single user message carrying a batch of tool results,
    /// in original tool_use order (Anthropic API requirement, §2.2).
    pub fn user_tool_results(results: Vec<ContentBlock>) -> Self {
        debug_assert!(results
            .iter()
            .all(|b| matches!(b, ContentBlock::ToolResult { .. })));
        Self {
            role: Role::User,
            content: results,
            usage: None,
        }
    }
}

/// Token accounting, accumulated from `message_start` + `message_delta` frames.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl Usage {
    pub fn merge(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_creation_input_tokens: self
                .cache_creation_input_tokens
                .saturating_add(other.cache_creation_input_tokens),
            cache_read_input_tokens: self
                .cache_read_input_tokens
                .saturating_add(other.cache_read_input_tokens),
        }
    }
}

/// Opaque session identifier. Newtype so it cannot be confused with other string ids.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Termination reason reported on `MessageDelta`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn text_block_without_cache_control_omits_field() {
        // Wire shape MUST match iter-1 byte-for-byte when cache_control is absent.
        let block = ContentBlock::Text {
            text: "hi".into(),
            cache_control: None,
        };
        let v = serde_json::to_value(&block).expect("serialize");
        assert_eq!(v, json!({"type": "text", "text": "hi"}));
    }

    #[test]
    fn text_block_with_cache_control_emits_ephemeral() {
        let block = ContentBlock::Text {
            text: "hi".into(),
            cache_control: Some(CacheControl::Ephemeral),
        };
        let v = serde_json::to_value(&block).expect("serialize");
        assert_eq!(
            v,
            json!({
                "type": "text",
                "text": "hi",
                "cache_control": {"type": "ephemeral"}
            })
        );
    }

    #[test]
    fn text_block_round_trip_with_cache_control() {
        let block = ContentBlock::Text {
            text: "hi".into(),
            cache_control: Some(CacheControl::Ephemeral),
        };
        let s = serde_json::to_string(&block).expect("serialize");
        let back: ContentBlock = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, block);
    }

    #[test]
    fn legacy_text_block_without_cache_control_parses() {
        // Iter-1 written sessions: no cache_control field — must still load.
        let s = json!({"type": "text", "text": "legacy"}).to_string();
        let block: ContentBlock = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(
            block,
            ContentBlock::Text {
                text: "legacy".into(),
                cache_control: None,
            }
        );
    }

    #[test]
    fn legacy_tool_use_block_without_cache_control_parses() {
        let s = json!({
            "type": "tool_use",
            "id": "toolu_01",
            "name": "Read",
            "input": {"path": "/tmp/x"},
        })
        .to_string();
        let block: ContentBlock = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(
            block,
            ContentBlock::ToolUse {
                id: "toolu_01".into(),
                name: "Read".into(),
                input: json!({"path": "/tmp/x"}),
                cache_control: None,
            }
        );
    }

    #[test]
    fn legacy_tool_result_block_parses() {
        let s = json!({
            "type": "tool_result",
            "tool_use_id": "toolu_01",
            "content": "ok",
        })
        .to_string();
        let block: ContentBlock = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(
            block,
            ContentBlock::ToolResult {
                tool_use_id: "toolu_01".into(),
                content: "ok".into(),
                is_error: false,
                cache_control: None,
            }
        );
    }

    #[test]
    fn usage_merge_sums_cache_tokens() {
        let a = Usage {
            input_tokens: 10,
            output_tokens: 1,
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 200,
        };
        let b = Usage {
            input_tokens: 20,
            output_tokens: 2,
            cache_creation_input_tokens: 5,
            cache_read_input_tokens: 50,
        };
        let m = a.merge(b);
        assert_eq!(m.input_tokens, 30);
        assert_eq!(m.output_tokens, 3);
        assert_eq!(m.cache_creation_input_tokens, 105);
        assert_eq!(m.cache_read_input_tokens, 250);
    }

    #[test]
    fn cache_control_wire_shape() {
        let cc = CacheControl::Ephemeral;
        let v = serde_json::to_value(cc).expect("serialize");
        assert_eq!(v, json!({"type": "ephemeral"}));
    }
}
