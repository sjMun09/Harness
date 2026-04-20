//! Harness wire-protocol leaf crate.
//!
//! Per PLAN §5.1 / §5.2 / §3.1 MVP. `ContentBlock` is the reduced MVP set
//! (`Text | ToolUse | ToolResult`); `Thinking`/`Image`/`cache_control` land
//! in iter 2 as additive variants.

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
/// Iter 2 adds `Thinking`, `RedactedThinking`, `Image`, and per-variant
/// `cache_control: Option<CacheControl>`. All additions are new variants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

impl ContentBlock {
    pub fn as_tool_use(&self) -> Option<ToolUseRef<'_>> {
        match self {
            Self::ToolUse { id, name, input } => Some(ToolUseRef { id, name, input }),
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
            content: vec![ContentBlock::Text { text: text.into() }],
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
