//! Provider trait + SSE event set + error hierarchy. PLAN §5.9 / §5.12.

use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use futures_core::Stream;
use harness_proto::{Message, StopReason, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type EventStream =
    Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send + 'static>>;

/// Tool advertisement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// Request parameters. MVP shape — iter 2 adds `cache_control`.
#[derive(Debug, Clone)]
pub struct StreamRequest<'a> {
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    pub max_tokens: u32,
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Open a streamed response. SSE parsing errors surface as
    /// `ProviderError::Parse` inside the stream.
    async fn stream(&self, req: StreamRequest<'_>) -> Result<EventStream, ProviderError>;
}

/// High-level event set consumed by the turn loop.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    MessageStart {
        message_id: String,
        usage: Usage,
    },
    ContentBlockStart {
        index: usize,
        block: ContentBlockHeader,
    },
    /// Text or JSON delta. JSON is bytes only — parse at ContentBlockStop.
    ContentBlockDelta {
        index: usize,
        delta: ContentDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        stop_reason: Option<StopReason>,
        usage: Usage,
    },
    MessageStop,
    /// Keep-alive — turn loop ignores.
    Ping,
}

#[derive(Debug, Clone)]
pub enum ContentBlockHeader {
    Text,
    ToolUse { id: String, name: String },
}

#[derive(Debug, Clone)]
pub enum ContentDelta {
    Text(String),
    /// Raw JSON fragment — MUST be byte-concatenated; do NOT parse mid-stream.
    InputJson(Vec<u8>),
}

/// Provider-layer errors. PLAN §5.12.
#[derive(thiserror::Error, Debug)]
pub enum ProviderError {
    #[error("auth: {0}")]
    Auth(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("rate limit: retry in {0:?}")]
    RateLimit(Option<Duration>),
    #[error("server error: {0}")]
    Server(u16),
    #[error("stream interrupted")]
    StreamDropped,
    #[error("parse: {0}")]
    Parse(String),
    #[error("transport: {0}")]
    Transport(String),
}

impl ProviderError {
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimit(_) | Self::Server(_) | Self::StreamDropped | Self::Transport(_)
        )
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimit(d) => *d,
            _ => None,
        }
    }
}
