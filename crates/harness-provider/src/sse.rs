//! SSE parser for Anthropic Messages API streams.
//!
//! Frame grammar (PLAN §5.9):
//!   - Frame boundary: `\n\n`
//!   - `:` prefix → comment line → ignored (covers `:ping` keep-alives)
//!   - `event: <name>\n` + `data: <json>\n` pair per frame
//!   - `data: [DONE]` → terminate stream cleanly (OpenAI-style tolerance)
//!
//! **CRITICAL (§2.2):** `input_json_delta.partial_json` is passed up as raw
//! bytes — NOT parsed here. Turn loop runs `serde_json::from_slice` exactly
//! once at `content_block_stop`.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use harness_core::{ContentBlockHeader, ContentDelta, EventStream, ProviderError, StreamEvent};
use harness_proto::{StopReason, Usage};
use serde::Deserialize;

/// SSE frame size cap — protects against pathological/DoS streams.
const MAX_FRAME_BYTES: usize = 1 << 20; // 1 MiB

pub(crate) fn parse<S>(inner: S) -> EventStream
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    Box::pin(SseStream {
        inner: Box::pin(inner),
        buf: BytesMut::with_capacity(8 * 1024),
        queue: VecDeque::new(),
        done: false,
    })
}

struct SseStream<S> {
    inner: Pin<Box<S>>,
    buf: BytesMut,
    /// Pending events produced by the last frame, drained one per poll.
    queue: VecDeque<Result<StreamEvent, ProviderError>>,
    done: bool,
}

impl<S> Stream for SseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send,
{
    type Item = Result<StreamEvent, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);
        loop {
            if let Some(ev) = this.queue.pop_front() {
                return Poll::Ready(Some(ev));
            }

            if let Some(frame) = extract_frame(&mut this.buf) {
                if frame.len() > MAX_FRAME_BYTES {
                    this.done = true;
                    return Poll::Ready(Some(Err(ProviderError::Parse(format!(
                        "sse frame exceeds {MAX_FRAME_BYTES} bytes"
                    )))));
                }
                match process_frame(&frame) {
                    Ok(None) => continue,
                    Ok(Some(ev)) => {
                        return Poll::Ready(Some(Ok(ev)));
                    }
                    Err(e) => {
                        this.done = true;
                        return Poll::Ready(Some(Err(e)));
                    }
                }
            }

            if this.done {
                return Poll::Ready(None);
            }

            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.done = true;
                    if !this.buf.is_empty() {
                        return Poll::Ready(Some(Err(ProviderError::StreamDropped)));
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Err(e))) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(ProviderError::Transport(e.to_string()))));
                }
                Poll::Ready(Some(Ok(bytes))) => {
                    this.buf.extend_from_slice(&bytes);
                }
            }
        }
    }
}

fn extract_frame(buf: &mut BytesMut) -> Option<String> {
    let (end, sep_len) = find_frame_end(buf)?;
    let frame_bytes = buf.split_to(end + sep_len);
    let body_len = frame_bytes.len() - sep_len;
    Some(String::from_utf8_lossy(&frame_bytes[..body_len]).into_owned())
}

fn find_frame_end(haystack: &[u8]) -> Option<(usize, usize)> {
    if let Some(i) = haystack.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some((i, 4));
    }
    haystack
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| (i, 2))
}

/// Parse a complete SSE frame (without the terminator). Returns:
///   * `Ok(None)`     — comment/keep-alive frame; caller should continue.
///   * `Ok(Some(ev))` — one StreamEvent to emit.
///   * `Err(_)`       — parse error; terminate stream.
///
/// Anthropic's SSE frames carry exactly one `event:` + `data:` pair per frame.
fn process_frame(frame: &str) -> Result<Option<StreamEvent>, ProviderError> {
    let mut event_name: Option<&str> = None;
    let mut data: Option<String> = None;
    for line in frame.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event_name = Some(rest.trim_start_matches(' '));
        } else if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.trim_start_matches(' ');
            match &mut data {
                Some(d) => {
                    d.push('\n');
                    d.push_str(rest);
                }
                None => data = Some(rest.to_string()),
            }
        }
    }

    let Some(data) = data else {
        // Comment-only or empty frame.
        return Ok(None);
    };
    // Anthropic SSE uses an `event:` header naming the variant; but the `type`
    // field inside `data` is canonical (and OpenAI-style `[DONE]` is tolerated).
    if data.trim() == "[DONE]" {
        return Ok(None);
    }
    let raw: RawEvent = serde_json::from_str(&data)
        .map_err(|e| ProviderError::Parse(format!("anthropic sse json: {e}")))?;
    let _ = event_name; // name is advisory; body's `type` drives dispatch.
    Ok(Some(map_raw(raw)))
}

// ---------------------------------------------------------------------------
// Wire-format shapes — match Anthropic verbatim. pub(crate) until tests want them.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum RawEvent {
    MessageStart {
        message: RawMessage,
    },
    ContentBlockStart {
        index: usize,
        content_block: RawContentBlockHeader,
    },
    ContentBlockDelta {
        index: usize,
        delta: RawDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: RawMessageDelta,
        usage: Usage,
    },
    MessageStop,
    Ping,
    Error {
        error: ApiError,
    },
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct RawMessage {
    pub id: String,
    #[serde(default)]
    pub usage: Usage,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct RawMessageDelta {
    #[serde(default)]
    pub stop_reason: Option<StopReason>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum RawContentBlockHeader {
    Text { text: String },
    ToolUse { id: String, name: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum RawDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct ApiError {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

/// Map wire-format `RawEvent` to the flattened `StreamEvent` the turn loop
/// consumes. **Never parses `partial_json`** — passes raw bytes through.
///
/// Cache metrics: `Usage::{cache_creation,cache_read}_input_tokens` are
/// extracted automatically because `Usage`'s serde derive accepts them with
/// `#[serde(default)]` (PLAN §5.2 / iter-2 task #21).
fn map_raw(raw: RawEvent) -> StreamEvent {
    match raw {
        RawEvent::MessageStart { message } => StreamEvent::MessageStart {
            message_id: message.id,
            usage: message.usage,
        },
        RawEvent::ContentBlockStart {
            index,
            content_block,
        } => {
            let block = match content_block {
                RawContentBlockHeader::Text { .. } => ContentBlockHeader::Text,
                RawContentBlockHeader::ToolUse { id, name } => {
                    ContentBlockHeader::ToolUse { id, name }
                }
            };
            StreamEvent::ContentBlockStart { index, block }
        }
        RawEvent::ContentBlockDelta { index, delta } => {
            let d = match delta {
                RawDelta::TextDelta { text } => ContentDelta::Text(text),
                RawDelta::InputJsonDelta { partial_json } => {
                    ContentDelta::InputJson(partial_json.into_bytes())
                }
            };
            StreamEvent::ContentBlockDelta { index, delta: d }
        }
        RawEvent::ContentBlockStop { index } => StreamEvent::ContentBlockStop { index },
        RawEvent::MessageDelta { delta, usage } => StreamEvent::MessageDelta {
            stop_reason: delta.stop_reason,
            usage,
        },
        RawEvent::MessageStop => StreamEvent::MessageStop,
        RawEvent::Ping => StreamEvent::Ping,
        RawEvent::Error { error } => {
            StreamEvent::Error(format!("anthropic {}: {}", error.kind, error.message))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use harness_proto::Usage;

    #[test]
    fn message_start_extracts_cache_usage() {
        // Real Anthropic message_start payload shape (subset).
        let raw = r#"{
            "type": "message_start",
            "message": {
                "id": "msg_01",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-4-7",
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": 25,
                    "output_tokens": 1,
                    "cache_creation_input_tokens": 512,
                    "cache_read_input_tokens": 1024
                }
            }
        }"#;
        let evt: RawEvent = serde_json::from_str(raw).expect("parse message_start");
        match map_raw(evt) {
            StreamEvent::MessageStart { usage, .. } => {
                assert_eq!(usage.input_tokens, 25);
                assert_eq!(usage.output_tokens, 1);
                assert_eq!(usage.cache_creation_input_tokens, 512);
                assert_eq!(usage.cache_read_input_tokens, 1024);
            }
            other => panic!("expected MessageStart, got {other:?}"),
        }
    }

    #[test]
    fn message_delta_extracts_cache_usage() {
        let raw = r#"{
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {
                "input_tokens": 0,
                "output_tokens": 99,
                "cache_creation_input_tokens": 64,
                "cache_read_input_tokens": 128
            }
        }"#;
        let evt: RawEvent = serde_json::from_str(raw).expect("parse message_delta");
        match map_raw(evt) {
            StreamEvent::MessageDelta { usage, .. } => {
                assert_eq!(usage.output_tokens, 99);
                assert_eq!(usage.cache_creation_input_tokens, 64);
                assert_eq!(usage.cache_read_input_tokens, 128);
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
    }

    #[test]
    fn legacy_usage_without_cache_fields_defaults_to_zero() {
        // Iter-1 Usage payloads that lack the cache_* fields must keep parsing.
        let raw = r#"{"input_tokens": 7, "output_tokens": 3}"#;
        let u: Usage = serde_json::from_str(raw).expect("parse legacy Usage");
        assert_eq!(u.input_tokens, 7);
        assert_eq!(u.output_tokens, 3);
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.cache_read_input_tokens, 0);
    }

    /// An Anthropic SSE `event: error` frame must surface as
    /// `StreamEvent::Error(msg)` carrying the error kind + message. Previously
    /// this was mapped to `Ping`, causing the engine to hang waiting for a
    /// `MessageStop` that would never arrive.
    #[test]
    fn error_event_propagates_as_stream_error() {
        let raw = r#"{
            "type": "error",
            "error": {
                "type": "overloaded_error",
                "message": "Overloaded"
            }
        }"#;
        let evt: RawEvent = serde_json::from_str(raw).expect("parse error event");
        match map_raw(evt) {
            StreamEvent::Error(msg) => {
                assert!(msg.contains("overloaded_error"), "got: {msg}");
                assert!(msg.contains("Overloaded"), "got: {msg}");
            }
            other => panic!("expected StreamEvent::Error, got {other:?}"),
        }
    }
}
