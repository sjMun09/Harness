//! Test utilities for the Harness workspace: a reusable mock `Provider`
//! implementation plus short constructors for common `StreamEvent` shapes.
//!
//! The kernel (`harness-core`) exposes `Provider`, `StreamEvent`, and
//! friends. Every crate that exercises the turn loop against a scripted
//! provider used to roll its own inline mock. This crate collects the two
//! shapes that proved useful in practice:
//!
//!  - [`MockProvider::scripted`] — a `Vec<Vec<StreamEvent>>` where each
//!    outer element is the full event sequence for one turn. Successive
//!    `stream()` calls pop the front of the queue.
//!  - [`MockProvider::channel`] — hand-fed through a tokio
//!    `UnboundedSender`. The stream terminates when the sender is dropped
//!    (closes), mirroring the `ChanProvider` pattern already in
//!    `harness_core::engine` tests.
//!
//! The helpers (`text_event`, `tool_use_event`, `message_stop`, `ping`,
//! `no_tools`) are intentionally minimal — they build the common shapes,
//! nothing more. Richer fixtures (recorder/replayer, JSON fixtures) are
//! explicitly out of scope; add them in follow-up work only when a real
//! consumer needs them.

#![forbid(unsafe_code)]

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_core::Stream;
use futures_util::stream;
use harness_core::{
    ContentBlockHeader, ContentDelta, EventStream, Provider, ProviderError, StreamEvent,
    StreamRequest, Tool,
};
use harness_proto::{Message, Usage};
use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

/// Inner state for a [`MockProvider`]. Either a FIFO queue of scripted turns
/// or a single one-shot receiver driven by the caller's channel.
enum Source {
    /// Scripted turns. Each `stream()` call pops the front element.
    Scripted(Mutex<std::collections::VecDeque<Vec<StreamEvent>>>),
    /// Channel-driven. Only the first `stream()` call consumes the receiver;
    /// subsequent calls return a `Transport` error.
    Channel(Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<StreamEvent>>>),
}

/// Reusable `harness_core::Provider` mock for the test suite.
///
/// See [`MockProvider::scripted`] and [`MockProvider::channel`] for the two
/// supported constructors. The mock intentionally does not inspect the
/// incoming `StreamRequest` — tests that need request-shape assertions should
/// wrap this with their own adapter.
pub struct MockProvider {
    source: Source,
}

impl std::fmt::Debug for MockProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match &self.source {
            Source::Scripted(_) => "scripted",
            Source::Channel(_) => "channel",
        };
        f.debug_struct("MockProvider")
            .field("source", &kind)
            .finish()
    }
}

impl MockProvider {
    /// Build a scripted mock. Each `events[i]` is the complete event sequence
    /// for the i-th `stream()` call; entries are consumed from the front so
    /// the caller writes turns in natural order.
    ///
    /// If the engine opens more streams than entries were supplied, the
    /// extra streams yield an empty sequence (stream terminates immediately).
    /// This matches the `MockProvider` pattern in `harness_core::engine`
    /// tests.
    pub fn scripted(events: Vec<Vec<StreamEvent>>) -> Arc<Self> {
        Arc::new(Self {
            source: Source::Scripted(Mutex::new(events.into_iter().collect())),
        })
    }

    /// Build a channel-driven mock. The caller pushes `StreamEvent`s through
    /// the returned sender; closing (dropping) the sender terminates the
    /// stream.
    ///
    /// Only the first `stream()` call consumes the receiver; any later call
    /// returns `ProviderError::Transport`. Multi-turn tests therefore need a
    /// fresh `MockProvider` per turn, or should use [`MockProvider::scripted`].
    pub fn channel() -> (Arc<Self>, UnboundedSender<StreamEvent>) {
        let (tx, rx) = unbounded_channel();
        let p = Arc::new(Self {
            source: Source::Channel(Mutex::new(Some(rx))),
        });
        (p, tx)
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn stream(&self, _req: StreamRequest<'_>) -> Result<EventStream, ProviderError> {
        match &self.source {
            Source::Scripted(queue) => {
                let events = queue
                    .lock()
                    .map_err(|_| ProviderError::Transport("mock queue poisoned".into()))?
                    .pop_front()
                    .unwrap_or_default();
                let s = stream::iter(events.into_iter().map(Ok::<_, ProviderError>));
                Ok(pin_event_stream(s))
            }
            Source::Channel(slot) => {
                let rx = slot
                    .lock()
                    .map_err(|_| ProviderError::Transport("mock channel poisoned".into()))?
                    .take()
                    .ok_or_else(|| {
                        ProviderError::Transport("mock channel already consumed".into())
                    })?;
                // Drive the receiver as a stream; `rx.recv().await == None`
                // (sender dropped) terminates the stream.
                let s = stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|ev| (Ok::<_, ProviderError>(ev), rx))
                });
                Ok(pin_event_stream(s))
            }
        }
    }
}

fn pin_event_stream<S>(s: S) -> EventStream
where
    S: Stream<Item = Result<StreamEvent, ProviderError>> + Send + 'static,
{
    Box::pin(s) as Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send + 'static>>
}

/// Snapshot of what the engine sent on one `stream()` call: the system
/// prompt and the full message history at that point in time. Cloned from
/// the `StreamRequest` so callers can inspect it after the turn completes
/// without borrowing.
#[derive(Debug, Clone)]
pub struct CallSnapshot {
    pub system: String,
    pub messages: Vec<Message>,
}

/// A `Provider` that delivers a scripted sequence of `StreamEvent`s (same
/// shape as [`MockProvider::scripted`]) **and** records each inbound request
/// so tests can assert what the engine sent.
///
/// Tests use [`RecordingProvider::new`] to build the provider + its shared
/// log, then pass the `Arc<Self>` as the `Provider` and keep the log around
/// to inspect after the turn loop completes.
pub struct RecordingProvider {
    script: Mutex<std::collections::VecDeque<Vec<StreamEvent>>>,
    calls: Arc<Mutex<Vec<CallSnapshot>>>,
}

impl std::fmt::Debug for RecordingProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let calls_len = self.calls.lock().map(|v| v.len()).unwrap_or(0);
        f.debug_struct("RecordingProvider")
            .field("calls_recorded", &calls_len)
            .finish()
    }
}

impl RecordingProvider {
    /// Build a recording provider. Returns the `Arc<Self>` (pluggable as
    /// `Arc<dyn Provider>`) plus a shared log the caller reads after
    /// `run_turn` returns.
    pub fn new(events: Vec<Vec<StreamEvent>>) -> (Arc<Self>, Arc<Mutex<Vec<CallSnapshot>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let me = Arc::new(Self {
            script: Mutex::new(events.into_iter().collect()),
            calls: calls.clone(),
        });
        (me, calls)
    }
}

#[async_trait]
impl Provider for RecordingProvider {
    async fn stream(&self, req: StreamRequest<'_>) -> Result<EventStream, ProviderError> {
        {
            let mut log = self
                .calls
                .lock()
                .map_err(|_| ProviderError::Transport("recorder poisoned".into()))?;
            log.push(CallSnapshot {
                system: req.system.to_string(),
                messages: req.messages.to_vec(),
            });
        }
        let events = self
            .script
            .lock()
            .map_err(|_| ProviderError::Transport("recorder script poisoned".into()))?
            .pop_front()
            .unwrap_or_default();
        let s = stream::iter(events.into_iter().map(Ok::<_, ProviderError>));
        Ok(pin_event_stream(s))
    }
}

// ────────────────────────────────────────────────────────────────────
// Short constructors for common StreamEvent shapes.
//
// These cover >90% of what the existing inline mocks build by hand. They
// deliberately do not cover every event variant — ping/MessageStart/etc.
// exist as enum literals already.
// ────────────────────────────────────────────────────────────────────

/// Build a complete Text block at `index = 0` carrying `text`:
/// `ContentBlockStart(Text) → ContentBlockDelta(Text) → ContentBlockStop`.
#[must_use]
pub fn text_event(text: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            block: ContentBlockHeader::Text,
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::Text(text.to_string()),
        },
        StreamEvent::ContentBlockStop { index: 0 },
    ]
}

/// Build a complete ToolUse block at `index = 0` with the given id/name and
/// serialized JSON input: `ContentBlockStart(ToolUse) → ContentBlockDelta
/// (InputJson bytes) → ContentBlockStop`.
#[must_use]
pub fn tool_use_event(id: &str, name: &str, input: &Value) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ContentBlockStart {
            index: 0,
            block: ContentBlockHeader::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
            },
        },
        StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::InputJson(input.to_string().into_bytes()),
        },
        StreamEvent::ContentBlockStop { index: 0 },
    ]
}

/// `StreamEvent::MessageStop` — the terminal event for a well-formed turn.
#[must_use]
pub fn message_stop() -> StreamEvent {
    StreamEvent::MessageStop
}

/// `StreamEvent::Ping` — keep-alive event, ignored by the turn loop.
#[must_use]
pub fn ping() -> StreamEvent {
    StreamEvent::Ping
}

/// `StreamEvent::MessageStart` with a default `Usage`. `message_id` is the
/// opaque id the provider echoes back; tests rarely care about its value.
#[must_use]
pub fn message_start(message_id: &str) -> StreamEvent {
    StreamEvent::MessageStart {
        message_id: message_id.to_string(),
        usage: Usage::default(),
    }
}

/// `StreamEvent::MessageDelta { stop_reason, usage: default }`. Use this to
/// terminate a turn with `StopReason::EndTurn` / `StopReason::ToolUse` etc.
#[must_use]
pub fn message_delta(stop_reason: harness_proto::StopReason) -> StreamEvent {
    StreamEvent::MessageDelta {
        stop_reason: Some(stop_reason),
        usage: Usage::default(),
    }
}

/// Convenience alias for an empty tool registry. The turn-loop harness
/// demands `Vec<Arc<dyn Tool>>`; spelling that out inline is noisy.
#[must_use]
pub fn no_tools() -> Vec<Arc<dyn Tool>> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use harness_core::StreamRequest;
    use harness_proto::StopReason;

    fn empty_req() -> StreamRequest<'static> {
        StreamRequest {
            system: "",
            messages: &[],
            tools: &[],
            max_tokens: 0,
        }
    }

    #[tokio::test]
    async fn scripted_delivers_turns_in_order() {
        let t0 = vec![message_start("m0"), message_stop()];
        let t1 = vec![message_start("m1"), ping(), message_stop()];
        let p = MockProvider::scripted(vec![t0.clone(), t1.clone()]);

        let s0: Vec<_> = p
            .stream(empty_req())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let s1: Vec<_> = p
            .stream(empty_req())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;

        assert_eq!(s0.len(), t0.len());
        assert_eq!(s1.len(), t1.len());
        assert!(matches!(
            s0[0].as_ref().unwrap(),
            StreamEvent::MessageStart { message_id, .. } if message_id == "m0"
        ));
        assert!(matches!(
            s1[0].as_ref().unwrap(),
            StreamEvent::MessageStart { message_id, .. } if message_id == "m1"
        ));
        assert!(matches!(s1[1].as_ref().unwrap(), StreamEvent::Ping));

        // Exhausted script — subsequent streams produce empty sequences, not
        // errors. This mirrors the original inline `MockProvider` semantics.
        let s2: Vec<_> = p
            .stream(empty_req())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert!(s2.is_empty());
    }

    #[tokio::test]
    async fn channel_terminates_when_sender_drops() {
        let (p, tx) = MockProvider::channel();
        tx.send(message_start("m")).unwrap();
        tx.send(message_stop()).unwrap();
        drop(tx); // close the channel → stream ends after draining.

        let items: Vec<_> = p
            .stream(empty_req())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        assert_eq!(items.len(), 2);
        assert!(matches!(
            items[0].as_ref().unwrap(),
            StreamEvent::MessageStart { .. }
        ));
        assert!(matches!(
            items[1].as_ref().unwrap(),
            StreamEvent::MessageStop
        ));
    }

    #[tokio::test]
    async fn channel_second_stream_errors() {
        let (p, _tx) = MockProvider::channel();
        let _ = p.stream(empty_req()).await.unwrap();
        let err = p.stream(empty_req()).await.err().expect("second call errs");
        assert!(matches!(err, ProviderError::Transport(_)));
    }

    #[test]
    fn text_event_shape() {
        let ev = text_event("hello");
        assert_eq!(ev.len(), 3);
        assert!(matches!(
            &ev[0],
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockHeader::Text
            }
        ));
        match &ev[1] {
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::Text(t),
            } => assert_eq!(t, "hello"),
            other => panic!("expected Text delta, got {other:?}"),
        }
        assert!(matches!(&ev[2], StreamEvent::ContentBlockStop { index: 0 }));
    }

    #[test]
    fn tool_use_event_shape() {
        let input = serde_json::json!({"path": "/tmp/x"});
        let ev = tool_use_event("tu_1", "Read", &input);
        assert_eq!(ev.len(), 3);
        match &ev[0] {
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockHeader::ToolUse { id, name },
            } => {
                assert_eq!(id, "tu_1");
                assert_eq!(name, "Read");
            }
            other => panic!("expected ToolUse start, got {other:?}"),
        }
        match &ev[1] {
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::InputJson(bytes),
            } => {
                let s = std::str::from_utf8(bytes).unwrap();
                let parsed: Value = serde_json::from_str(s).unwrap();
                assert_eq!(parsed, input);
            }
            other => panic!("expected InputJson delta, got {other:?}"),
        }
        assert!(matches!(&ev[2], StreamEvent::ContentBlockStop { index: 0 }));
    }

    #[test]
    fn scalar_helpers_shape() {
        assert!(matches!(message_stop(), StreamEvent::MessageStop));
        assert!(matches!(ping(), StreamEvent::Ping));
        assert!(matches!(
            message_start("m"),
            StreamEvent::MessageStart { message_id, .. } if message_id == "m"
        ));
        assert!(matches!(
            message_delta(StopReason::EndTurn),
            StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                ..
            }
        ));
        assert!(no_tools().is_empty());
    }
}
