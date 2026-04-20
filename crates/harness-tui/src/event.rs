//! `TurnEvent` — high-level events emitted by the turn loop and consumed by
//! the TUI (or any other front-end). PLAN §5.9 (SSE event set) sits below
//! this layer; this enum is the *post-aggregation* view a renderer wants.
//!
//! The engine in `harness-core` will eventually expose this via an
//! `EventSink` trait. Defining the enum here lets the TUI ship without
//! blocking on the engine refactor; the engine can re-export it later.

use std::time::Duration;

use harness_perm::Decision;

/// A logical event from the turn loop. Renderers should treat it as
/// append-only: state is reconstructed from the event sequence.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// Turn loop opened a new turn (turn index, 1-based).
    TurnStart { turn: u32 },
    /// Streaming assistant text — append to current assistant message.
    AssistantTextDelta { text: String },
    /// Assistant message completed (current text frozen as a scrollback entry).
    AssistantMessageEnd,
    /// A tool call started executing.
    ToolStart {
        id: String,
        name: String,
        preview: String,
    },
    /// Tool finished. `ok=false` means `is_error` was set on the result.
    ToolEnd {
        id: String,
        ok: bool,
        summary: String,
        elapsed: Duration,
    },
    /// Permission required for a pending tool call. The TUI surfaces a modal.
    PermissionAsk {
        id: String,
        tool: String,
        preview: String,
    },
    /// Resolution of a previously-asked permission (by id).
    PermissionResolved { id: String, decision: Decision },
    /// Turn ended (loop hit `end_turn` or a stop condition).
    TurnEnd { reason: TurnEndReason },
    /// Surface a non-fatal error to the user.
    Error { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEndReason {
    EndTurn,
    MaxTurns,
    BudgetExceeded,
    Cancelled,
    ProviderError,
}

impl TurnEndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::MaxTurns => "max_turns",
            Self::BudgetExceeded => "budget_exceeded",
            Self::Cancelled => "cancelled",
            Self::ProviderError => "provider_error",
        }
    }
}

/// User-originated permission decision delivered back to the engine through
/// whichever `oneshot` channel the TUI was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionResponse {
    /// Allow this single call.
    AllowOnce,
    /// Allow + cache for the rest of the session (`[a]lways`).
    AllowAlways,
    /// Deny this call. Engine reports `permission_denied` ToolError.
    Deny,
}

impl PermissionResponse {
    pub fn to_decision(self) -> Decision {
        match self {
            Self::AllowOnce | Self::AllowAlways => Decision::Allow,
            Self::Deny => Decision::Deny,
        }
    }

    pub fn is_always(self) -> bool {
        matches!(self, Self::AllowAlways)
    }
}
