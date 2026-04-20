//! Turn-loop state-machine types. PLAN §2.2.
//!
//! `BlockState` accumulates streamed bytes per `index`; `finalize()` runs
//! the single `serde_json::from_slice` required for `ToolUse.input`.

use harness_proto::ContentBlock;
use serde_json::Value;

use crate::provider::{ContentBlockHeader, ContentDelta};

/// Per-block accumulator driven by `ContentBlockStart` / `ContentBlockDelta` /
/// `ContentBlockStop` events.
///
/// `input_buf` is `Vec<u8>`, NOT `String`: SSE deltas can split UTF-8 scalars
/// mid-byte and JSON tokens mid-literal. Concatenate raw bytes, parse exactly
/// once at stop-time.
#[derive(Debug, Clone)]
pub enum BlockState {
    Text {
        text_buf: String,
    },
    ToolUse {
        id: String,
        name: String,
        input_buf: Vec<u8>,
    },
}

impl BlockState {
    pub fn from_start(header: ContentBlockHeader) -> Self {
        match header {
            ContentBlockHeader::Text => Self::Text {
                text_buf: String::new(),
            },
            ContentBlockHeader::ToolUse { id, name } => Self::ToolUse {
                id,
                name,
                input_buf: Vec::new(),
            },
        }
    }

    pub fn push_text(&mut self, s: &str) {
        match self {
            Self::Text { text_buf } => text_buf.push_str(s),
            Self::ToolUse { .. } => {
                debug_assert!(false, "text delta for tool_use block");
            }
        }
    }

    pub fn push_json_bytes(&mut self, bytes: &[u8]) {
        match self {
            Self::ToolUse { input_buf, .. } => input_buf.extend_from_slice(bytes),
            Self::Text { .. } => {
                debug_assert!(false, "input_json delta for text block");
            }
        }
    }

    pub fn push_delta(&mut self, delta: &ContentDelta) {
        match delta {
            ContentDelta::Text(s) => self.push_text(s),
            ContentDelta::InputJson(b) => self.push_json_bytes(b),
        }
    }

    /// Collapse accumulated partials into a wire-ready `ContentBlock`.
    ///
    /// ONE place `serde_json::from_slice` runs on tool input. Empty tool input
    /// is permitted and parsed as `{}` (some providers omit the body for
    /// zero-arg tools).
    pub fn finalize(self) -> Result<ContentBlock, FinalizeError> {
        match self {
            Self::Text { text_buf } => Ok(ContentBlock::Text {
                text: text_buf,
                cache_control: None,
            }),
            Self::ToolUse {
                id,
                name,
                input_buf,
            } => {
                let input: Value = if input_buf.is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_slice(&input_buf).map_err(|e| FinalizeError::JsonParse {
                        tool_use_id: id.clone(),
                        tool_name: name.clone(),
                        source: e,
                    })?
                };
                if !input.is_object() {
                    return Err(FinalizeError::NotAnObject {
                        tool_use_id: id,
                        tool_name: name,
                    });
                }
                Ok(ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    cache_control: None,
                })
            }
        }
    }
}

/// Errors surfaced when collapsing accumulated partials.
///
/// Per PLAN §2.2, a `FinalizeError` triggers *whole-turn retry* — the turn
/// loop discards all in-flight partials and replays the request. Prior
/// completed assistant messages are preserved.
#[derive(thiserror::Error, Debug)]
pub enum FinalizeError {
    #[error("tool_use {tool_use_id} ({tool_name}): invalid JSON input: {source}")]
    JsonParse {
        tool_use_id: String,
        tool_name: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("tool_use {tool_use_id} ({tool_name}): input must be a JSON object")]
    NotAnObject {
        tool_use_id: String,
        tool_name: String,
    },
}
