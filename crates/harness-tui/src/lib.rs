//! Harness TUI — ratatui + crossterm front-end. PLAN §3.2.
//!
//! Surface:
//!   - [`TuiApp`] holds all renderable state.
//!   - [`TuiApp::run_session`] is the async entry point invoked by the CLI
//!     when `--tui` is passed.
//!   - [`TurnEvent`] is the wire from the engine into the TUI; defined here
//!     because `harness-core::engine` is owned by a sibling agent during
//!     iter 2 and the trait isn't yet exposed there.
//!
//! Module layout:
//!   - `app`         — state machine + `TuiApp`
//!   - `event`       — `TurnEvent` + permission types
//!   - `event_loop`  — async loop that wires crossterm + engine + render
//!   - `input`       — keyboard → `InputAction`
//!   - `markdown`    — minimal self-implemented md → `ratatui::Text`
//!   - `render`      — pure render function

#![forbid(unsafe_code)]

pub mod app;
pub mod event;
pub mod event_loop;
pub mod input;
pub mod markdown;
pub mod render;

pub use app::{EngineHandle, Entry, PermissionModal, PermissionRequest, ToolCard, ToolStatus, TuiApp};
pub use event::{PermissionResponse, TurnEndReason, TurnEvent};
pub use event_loop::{DemoEngine, EngineDriver};

/// Convenience for the CLI binary: launch a one-shot ask via the demo engine.
///
/// This is the integration point for `harness --tui ask "..."`. When the real
/// engine surface lands, the CLI will swap `DemoEngine` for an
/// `EngineSinkDriver` that wraps `harness_core::engine::run_turn`.
pub async fn run_one_shot(model: String, prompt: String) -> anyhow::Result<String> {
    let app = TuiApp::new(model, "ask")?;
    app.run_session(prompt, Box::new(DemoEngine)).await
}
