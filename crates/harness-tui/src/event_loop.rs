//! Async event loop wiring crossterm input + engine `TurnEvent` stream into
//! the `TuiApp` state machine, then re-rendering each tick.
//!
//! Engine integration: callers provide an `EngineDriver`. The TUI hands it
//! an `EngineHandle` (event_tx + permission_tx) and a prompt; the driver
//! pushes `TurnEvent`s as the turn progresses and surfaces permission asks
//! through `PermissionRequest`.
//!
//! Until `harness-core::engine` exposes a real `EventSink` (sibling agent
//! owns that file), `DemoEngine` provides a self-contained driver that
//! exercises every code path. The CLI uses it when `--tui` is passed.

use std::io::Stdout;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self as cterm_event, Event};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::app::{EngineHandle, PermissionRequest, TuiApp};
use crate::event::TurnEvent;
use crate::input::handle_key;
use crate::render::draw;

/// Driver the TUI uses to start a session. Implementors push events into the
/// channel they receive on `start`. The TUI awaits `TurnEvent::TurnEnd` on
/// the receiver side.
///
/// The trait is intentionally object-safe-friendly: `start` returns a boxed
/// future so a `Box<dyn EngineDriver>` works.
pub trait EngineDriver: Send {
    fn start<'a>(
        self: Box<Self>,
        prompt: String,
        handle: EngineHandle,
        cancel: tokio_util_lite::CancellationFlag,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
}

/// A no-op cancellation flag — keeps the TUI crate from depending on
/// `tokio_util` (already pulled in by harness-core, but declaring it here
/// would duplicate). Cheap shared bool.
pub mod tokio_util_lite {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[derive(Clone, Default, Debug)]
    pub struct CancellationFlag {
        flag: Arc<AtomicBool>,
    }

    impl CancellationFlag {
        pub fn new() -> Self {
            Self::default()
        }
        pub fn cancel(&self) {
            self.flag.store(true, Ordering::SeqCst);
        }
        pub fn is_cancelled(&self) -> bool {
            self.flag.load(Ordering::SeqCst)
        }
    }
}

/// Public entry from `TuiApp::run_session`. Manages terminal init/teardown,
/// pumps events, and returns the final assistant text.
pub async fn run(
    app: &mut TuiApp,
    initial_prompt: String,
    engine: Box<dyn EngineDriver>,
) -> Result<String> {
    let mut term = init_terminal()?;
    let result = run_inner(&mut term, app, initial_prompt, engine).await;
    teardown_terminal(&mut term).ok(); // best-effort
    result
}

async fn run_inner(
    term: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut TuiApp,
    initial_prompt: String,
    engine: Box<dyn EngineDriver>,
) -> Result<String> {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TurnEvent>();
    let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<PermissionRequest>();
    let cancel = tokio_util_lite::CancellationFlag::new();

    // Push initial prompt into scrollback so the user sees it.
    app.push_user(initial_prompt.clone());
    app.turn_active = true;

    let handle = EngineHandle {
        events_tx: event_tx,
        permission_tx: perm_tx,
    };
    let cancel_for_engine = cancel.clone();
    let engine_task = tokio::spawn(async move {
        engine
            .start(initial_prompt, handle, cancel_for_engine)
            .await
    });

    // Spawn an OS-thread bridge for crossterm events so we can `select!` them.
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<Event>();
    let key_thread = std::thread::spawn(move || loop {
        match cterm_event::poll(Duration::from_millis(100)) {
            Ok(true) => match cterm_event::read() {
                Ok(ev) => {
                    if key_tx.send(ev).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    });

    let mut redraw_interval = tokio::time::interval(Duration::from_millis(80));

    loop {
        // Render.
        term.draw(|f| draw(f, app))?;

        if app.should_quit {
            break;
        }

        tokio::select! {
            // Engine event.
            Some(ev) = event_rx.recv() => {
                let is_end = matches!(ev, TurnEvent::TurnEnd { .. });
                app.apply_event(ev);
                if is_end {
                    // Conversation continues only if user submits again.
                    // For a one-shot `harness ask --tui`, exit here.
                    break;
                }
            }
            // Permission request from engine → open modal.
            Some(req) = perm_rx.recv() => {
                app.open_permission_modal(req.id, req.tool, req.preview, req.respond_to);
            }
            // Keyboard event.
            Some(Event::Key(key)) = key_rx.recv() => {
                let action = handle_key(app, key);
                use crate::input::InputAction;
                match action {
                    InputAction::Submitted => {
                        // For iter 2 first cut: a submit after a session is
                        // running just appends to scrollback. Multi-turn
                        // continuation lands when the engine driver handles it.
                        if let Some(prompt) = app.pending_submit.take() {
                            app.push_user(prompt);
                        }
                    }
                    InputAction::CancelTurn => {
                        cancel.cancel();
                    }
                    InputAction::Quit => break,
                    InputAction::None | InputAction::ModalResolved(_) => {}
                }
            }
            _ = redraw_interval.tick() => { /* fall through to redraw */ }
        }
    }

    // Drop receivers so the engine task's sender errors out and it returns.
    drop(event_rx);
    drop(perm_rx);
    let _ = engine_task.await;
    drop(key_thread); // detach — the thread exits when its tx is dropped (already dropped above)

    Ok(app.last_assistant_text())
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn teardown_terminal(term: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

// ── Demo engine ──────────────────────────────────────────────────────
//
// Until `harness-core::engine` exposes a real EventSink (sibling agent owns
// that file), this driver exercises the full TUI surface so `harness --tui
// ask "..."` produces a usable demo.

/// A self-contained driver that streams a canned response. Useful as the
/// `harness --tui ask "..."` driver until the real engine is wired.
#[derive(Debug)]
pub struct DemoEngine;

impl EngineDriver for DemoEngine {
    fn start<'a>(
        self: Box<Self>,
        prompt: String,
        handle: EngineHandle,
        cancel: tokio_util_lite::CancellationFlag,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            let _ = handle.events_tx.send(TurnEvent::TurnStart { turn: 1 });
            let response = format!(
                "**Demo response** to: `{prompt}`\n\nThe TUI is wired and rendering.\n\
                 - input box at bottom (Enter submits)\n\
                 - tool cards stream into scrollback\n\
                 - permission modal on `[y/a/n]`"
            );
            for chunk in response.split_inclusive(' ') {
                if cancel.is_cancelled() {
                    break;
                }
                let _ = handle.events_tx.send(TurnEvent::AssistantTextDelta {
                    text: chunk.to_string(),
                });
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = handle.events_tx.send(TurnEvent::AssistantMessageEnd);

            // Demo a tool card.
            let _ = handle.events_tx.send(TurnEvent::ToolStart {
                id: "demo-1".into(),
                name: "Read".into(),
                preview: "Read README.md".into(),
            });
            tokio::time::sleep(Duration::from_millis(150)).await;
            let _ = handle.events_tx.send(TurnEvent::ToolEnd {
                id: "demo-1".into(),
                ok: true,
                summary: "42 lines".into(),
                elapsed: Duration::from_millis(150),
            });

            let _ = handle.events_tx.send(TurnEvent::TurnEnd {
                reason: crate::event::TurnEndReason::EndTurn,
            });
        })
    }
}
