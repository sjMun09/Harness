//! `TuiApp` — top-level state and the `run_session` entry point.
//!
//! The app owns:
//!   - `scrollback` — completed user/assistant messages (plus tool-card refs)
//!   - `tool_cards` — keyed by tool_use id, mutated as `TurnEvent`s arrive
//!   - `pending_assistant` — the in-flight assistant text being streamed
//!   - `input` — the multiline composer
//!   - `modal` — `Some(_)` while a permission ask is open
//!
//! The render layer (`crate::render`) is a pure function of this state.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use harness_perm::Decision;
use tokio::sync::{mpsc, oneshot};

use crate::event::{PermissionResponse, TurnEndReason, TurnEvent};

/// State of one tool card.
#[derive(Debug, Clone)]
pub struct ToolCard {
    pub id: String,
    pub name: String,
    pub preview: String,
    pub status: ToolStatus,
    pub started_at: Instant,
}

#[derive(Debug, Clone)]
pub enum ToolStatus {
    Running,
    Ok { summary: String, elapsed: Duration },
    Err { summary: String, elapsed: Duration },
}

impl ToolStatus {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// One scrollback entry — user msg, assistant msg, or a tool card reference.
#[derive(Debug, Clone)]
pub enum Entry {
    User(String),
    Assistant(String),
    Tool(String /* tool_use_id */),
    Notice(String),
}

/// Optional permission modal state.
#[derive(Debug, Clone)]
pub struct PermissionModal {
    pub id: String,
    pub tool: String,
    pub preview: String,
}

/// Top-level immutable-ish app state. Mutated only by the event loop.
#[derive(Debug)]
pub struct TuiApp {
    pub model: String,
    pub session_label: String,
    pub scrollback: Vec<Entry>,
    pub tool_cards: HashMap<String, ToolCard>,
    pub pending_assistant: Option<String>,
    pub input: String,
    pub scroll_offset: u16,
    pub modal: Option<PermissionModal>,
    /// Channels keyed by ask-id — TUI sends `PermissionResponse` here on y/a/n.
    pub modal_replies: HashMap<String, oneshot::Sender<PermissionResponse>>,
    pub turn_active: bool,
    pub status: String,
    pub should_quit: bool,
    /// Pending submit — the event loop drains this and forwards to the engine.
    pub pending_submit: Option<String>,
    /// Cancel requested for the in-flight turn.
    pub cancel_requested: bool,
}

impl TuiApp {
    /// Build a fresh app for a given model + session label.
    ///
    /// Side-effect-free; does not touch the terminal.
    pub fn new(model: impl Into<String>, session_label: impl Into<String>) -> anyhow::Result<Self> {
        Ok(Self {
            model: model.into(),
            session_label: session_label.into(),
            scrollback: Vec::new(),
            tool_cards: HashMap::new(),
            pending_assistant: None,
            input: String::new(),
            scroll_offset: 0,
            modal: None,
            modal_replies: HashMap::new(),
            turn_active: false,
            status: String::from("ready"),
            should_quit: false,
            pending_submit: None,
            cancel_requested: false,
        })
    }

    /// Drive a session from `initial_prompt` until the engine signals
    /// `TurnEnd { EndTurn }` or the user quits. Returns the last assistant
    /// text (matches line-mode stdout contract).
    ///
    /// Wires:
    ///   - `(tx, rx)` event channel: engine pushes `TurnEvent`, TUI consumes.
    ///   - permission oneshots tracked in `modal_replies`.
    ///
    /// Iter 2 hookup: this is invoked from `harness-cli` when `--tui` is set.
    /// The CLI is responsible for spawning the actual engine task and feeding
    /// `tx`. Until the engine surfaces a real `EventSink` (sibling agent owns
    /// `harness-core::engine`), the CLI uses `crate::event_loop::demo_session`
    /// to keep the binary linked.
    pub async fn run_session(
        mut self,
        initial_prompt: String,
        engine: Box<dyn crate::event_loop::EngineDriver>,
    ) -> anyhow::Result<String> {
        crate::event_loop::run(&mut self, initial_prompt, engine).await
    }

    // ── State transitions, all unit-testable ──────────────────────────

    /// Append a user message to scrollback.
    pub fn push_user(&mut self, text: String) {
        self.scrollback.push(Entry::User(text));
    }

    /// Begin streaming an assistant message. Subsequent `assistant_delta`
    /// calls accumulate into `pending_assistant`.
    pub fn begin_assistant(&mut self) {
        self.pending_assistant = Some(String::new());
    }

    pub fn assistant_delta(&mut self, text: &str) {
        if self.pending_assistant.is_none() {
            self.pending_assistant = Some(String::new());
        }
        if let Some(buf) = self.pending_assistant.as_mut() {
            buf.push_str(text);
        }
    }

    pub fn end_assistant(&mut self) {
        if let Some(text) = self.pending_assistant.take() {
            if !text.is_empty() {
                self.scrollback.push(Entry::Assistant(text));
            }
        }
    }

    pub fn tool_start(&mut self, id: String, name: String, preview: String) {
        let card = ToolCard {
            id: id.clone(),
            name,
            preview,
            status: ToolStatus::Running,
            started_at: Instant::now(),
        };
        self.tool_cards.insert(id.clone(), card);
        self.scrollback.push(Entry::Tool(id));
    }

    pub fn tool_end(&mut self, id: &str, ok: bool, summary: String, elapsed: Duration) {
        if let Some(card) = self.tool_cards.get_mut(id) {
            card.status = if ok {
                ToolStatus::Ok { summary, elapsed }
            } else {
                ToolStatus::Err { summary, elapsed }
            };
        }
    }

    pub fn open_permission_modal(
        &mut self,
        id: String,
        tool: String,
        preview: String,
        reply: oneshot::Sender<PermissionResponse>,
    ) {
        self.modal_replies.insert(id.clone(), reply);
        self.modal = Some(PermissionModal { id, tool, preview });
    }

    /// Resolve the open modal with the user's response. No-op if the id
    /// doesn't match the current modal.
    pub fn resolve_modal(&mut self, response: PermissionResponse) {
        if let Some(modal) = self.modal.take() {
            if let Some(tx) = self.modal_replies.remove(&modal.id) {
                let _ = tx.send(response);
            }
        }
    }

    /// Apply a `TurnEvent` to the app state.
    pub fn apply_event(&mut self, ev: TurnEvent) {
        match ev {
            TurnEvent::TurnStart { turn } => {
                self.turn_active = true;
                self.status = format!("turn {turn} running");
            }
            TurnEvent::AssistantTextDelta { text } => {
                self.assistant_delta(&text);
            }
            TurnEvent::AssistantMessageEnd => {
                self.end_assistant();
            }
            TurnEvent::ToolStart { id, name, preview } => {
                self.tool_start(id, name, preview);
            }
            TurnEvent::ToolEnd {
                id,
                ok,
                summary,
                elapsed,
            } => {
                self.tool_end(&id, ok, summary, elapsed);
            }
            TurnEvent::PermissionAsk { .. } | TurnEvent::PermissionResolved { .. } => {
                // Modal lifecycle is driven by the event loop, not apply_event,
                // because it needs the oneshot::Sender. No-op here.
            }
            TurnEvent::TurnEnd { reason } => {
                self.turn_active = false;
                self.cancel_requested = false;
                self.status = format!("turn ended: {}", reason.as_str());
                if matches!(reason, TurnEndReason::EndTurn) {
                    // Final assistant message already flushed by AssistantMessageEnd.
                }
            }
            TurnEvent::Error { message } => {
                self.scrollback.push(Entry::Notice(format!("error: {message}")));
                self.status = format!("error: {message}");
            }
        }
    }

    /// Last assistant text (for `run_session` return value).
    pub fn last_assistant_text(&self) -> String {
        self.scrollback
            .iter()
            .rev()
            .find_map(|e| match e {
                Entry::Assistant(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }
}

/// Convenience for the event loop: wrap a permission decision back into a
/// shape the engine wants. Currently unused inside the crate but exported
/// for downstream callers.
#[must_use]
pub fn decision_for(response: PermissionResponse) -> Decision {
    response.to_decision()
}

/// Shared in/out channel pair the event loop hands to an engine driver.
#[derive(Debug)]
pub struct EngineHandle {
    pub events_tx: mpsc::UnboundedSender<TurnEvent>,
    pub permission_tx: mpsc::UnboundedSender<PermissionRequest>,
}

/// One permission request flowing engine → TUI. The TUI replies on `respond_to`.
#[derive(Debug)]
pub struct PermissionRequest {
    pub id: String,
    pub tool: String,
    pub preview: String,
    pub respond_to: oneshot::Sender<PermissionResponse>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn smoke_construct() {
        let app = TuiApp::new("claude-opus-4-7", "session-1").unwrap();
        assert_eq!(app.model, "claude-opus-4-7");
        assert!(app.scrollback.is_empty());
        assert!(!app.turn_active);
    }

    #[test]
    fn assistant_streaming_lifecycle() {
        let mut app = TuiApp::new("m", "s").unwrap();
        app.begin_assistant();
        app.assistant_delta("hello ");
        app.assistant_delta("world");
        app.end_assistant();
        assert_eq!(app.scrollback.len(), 1);
        match &app.scrollback[0] {
            Entry::Assistant(t) => assert_eq!(t, "hello world"),
            other => panic!("expected assistant, got {other:?}"),
        }
        assert!(app.pending_assistant.is_none());
    }

    #[test]
    fn empty_assistant_message_is_dropped() {
        let mut app = TuiApp::new("m", "s").unwrap();
        app.begin_assistant();
        app.end_assistant();
        assert!(app.scrollback.is_empty());
    }

    #[test]
    fn tool_card_state_machine_ok() {
        let mut app = TuiApp::new("m", "s").unwrap();
        app.apply_event(TurnEvent::ToolStart {
            id: "t1".into(),
            name: "Read".into(),
            preview: "Read /tmp/x".into(),
        });
        assert!(matches!(
            app.tool_cards.get("t1").unwrap().status,
            ToolStatus::Running
        ));
        app.apply_event(TurnEvent::ToolEnd {
            id: "t1".into(),
            ok: true,
            summary: "20 lines".into(),
            elapsed: Duration::from_millis(12),
        });
        assert!(matches!(
            app.tool_cards.get("t1").unwrap().status,
            ToolStatus::Ok { .. }
        ));
        assert!(app.tool_cards["t1"].status.is_terminal());
    }

    #[test]
    fn tool_card_state_machine_err() {
        let mut app = TuiApp::new("m", "s").unwrap();
        app.apply_event(TurnEvent::ToolStart {
            id: "t2".into(),
            name: "Bash".into(),
            preview: "ls /no/such".into(),
        });
        app.apply_event(TurnEvent::ToolEnd {
            id: "t2".into(),
            ok: false,
            summary: "exit 1".into(),
            elapsed: Duration::from_millis(3),
        });
        match &app.tool_cards["t2"].status {
            ToolStatus::Err { summary, .. } => assert_eq!(summary, "exit 1"),
            other => panic!("expected err, got {other:?}"),
        }
    }

    #[test]
    fn modal_resolves_through_oneshot() {
        let mut app = TuiApp::new("m", "s").unwrap();
        let (tx, mut rx) = oneshot::channel();
        app.open_permission_modal("p1".into(), "Edit".into(), "Edit foo.rs".into(), tx);
        assert!(app.modal.is_some());
        app.resolve_modal(PermissionResponse::AllowAlways);
        assert!(app.modal.is_none());
        let got = rx.try_recv().unwrap();
        assert_eq!(got, PermissionResponse::AllowAlways);
    }

    #[test]
    fn turn_end_clears_active_flag() {
        let mut app = TuiApp::new("m", "s").unwrap();
        app.apply_event(TurnEvent::TurnStart { turn: 1 });
        assert!(app.turn_active);
        app.apply_event(TurnEvent::TurnEnd {
            reason: TurnEndReason::EndTurn,
        });
        assert!(!app.turn_active);
    }

    #[test]
    fn last_assistant_text_returns_most_recent() {
        let mut app = TuiApp::new("m", "s").unwrap();
        app.scrollback.push(Entry::Assistant("first".into()));
        app.scrollback.push(Entry::User("u".into()));
        app.scrollback.push(Entry::Assistant("second".into()));
        assert_eq!(app.last_assistant_text(), "second");
    }
}
