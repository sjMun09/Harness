//! Keyboard input → app actions.
//!
//! Modes:
//!   - Normal (no modal): typing fills `app.input`.
//!     - Enter        → submit (sets `pending_submit`)
//!     - Shift+Enter  → newline in input
//!     - Backspace    → delete one char
//!     - Ctrl+C / Esc → cancel in-flight turn
//!     - Ctrl+U / PgUp → scroll up
//!     - Ctrl+D / PgDn → scroll down
//!     - q            → quit (only when not mid-turn)
//!   - Modal (permission ask):
//!     - y → AllowOnce
//!     - a → AllowAlways
//!     - n / Esc → Deny

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::TuiApp;
use crate::event::PermissionResponse;

/// Result of dispatching a single key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAction {
    /// Continue rendering and looping.
    None,
    /// User asked to submit `app.input`.
    Submitted,
    /// User asked to cancel in-flight turn.
    CancelTurn,
    /// User asked to quit the app.
    Quit,
    /// Modal was resolved.
    ModalResolved(PermissionResponse),
}

/// Apply a `KeyEvent` to `app`. Returns the high-level action so the event
/// loop can stitch in async side-effects (channel sends, engine cancel, etc.).
pub fn handle_key(app: &mut TuiApp, key: KeyEvent) -> InputAction {
    if app.modal.is_some() {
        return handle_modal_key(app, key);
    }
    handle_normal_key(app, key)
}

fn handle_modal_key(app: &mut TuiApp, key: KeyEvent) -> InputAction {
    let resp = match key.code {
        KeyCode::Char('y' | 'Y') => PermissionResponse::AllowOnce,
        KeyCode::Char('a' | 'A') => PermissionResponse::AllowAlways,
        KeyCode::Char('n' | 'N') | KeyCode::Esc => PermissionResponse::Deny,
        _ => return InputAction::None,
    };
    app.resolve_modal(resp);
    InputAction::ModalResolved(resp)
}

fn handle_normal_key(app: &mut TuiApp, key: KeyEvent) -> InputAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    match key.code {
        KeyCode::Esc => {
            if app.turn_active {
                app.cancel_requested = true;
                app.status = "cancel requested".into();
                return InputAction::CancelTurn;
            }
        }
        KeyCode::Char('c') if ctrl => {
            if app.turn_active {
                app.cancel_requested = true;
                app.status = "cancel requested".into();
                return InputAction::CancelTurn;
            }
            // Outside a turn, Ctrl+C is a quit too.
            app.should_quit = true;
            return InputAction::Quit;
        }
        KeyCode::Char('q') if !app.turn_active && app.input.is_empty() => {
            app.should_quit = true;
            return InputAction::Quit;
        }
        KeyCode::Char('u') if ctrl => {
            app.scroll_offset = app.scroll_offset.saturating_add(5);
        }
        KeyCode::Char('d') if ctrl => {
            app.scroll_offset = app.scroll_offset.saturating_sub(5);
        }
        KeyCode::PageUp => {
            app.scroll_offset = app.scroll_offset.saturating_add(10);
        }
        KeyCode::PageDown => {
            app.scroll_offset = app.scroll_offset.saturating_sub(10);
        }
        KeyCode::Enter => {
            if shift {
                app.input.push('\n');
            } else if !app.input.trim().is_empty() && !app.turn_active {
                let prompt = std::mem::take(&mut app.input);
                app.pending_submit = Some(prompt);
                return InputAction::Submitted;
            }
        }
        KeyCode::Backspace => {
            app.input.pop();
        }
        KeyCode::Char(c) => {
            app.input.push(c);
        }
        _ => {}
    }
    InputAction::None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn typing_fills_input() {
        let mut app = TuiApp::new("m", "s").unwrap();
        handle_key(&mut app, k(KeyCode::Char('h'), KeyModifiers::NONE));
        handle_key(&mut app, k(KeyCode::Char('i'), KeyModifiers::NONE));
        assert_eq!(app.input, "hi");
    }

    #[test]
    fn enter_submits_when_idle() {
        let mut app = TuiApp::new("m", "s").unwrap();
        app.input = "hello".into();
        let action = handle_key(&mut app, k(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, InputAction::Submitted);
        assert_eq!(app.pending_submit.as_deref(), Some("hello"));
        assert!(app.input.is_empty());
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let mut app = TuiApp::new("m", "s").unwrap();
        app.input = "a".into();
        handle_key(&mut app, k(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(app.input, "a\n");
    }

    #[test]
    fn ctrl_c_during_turn_cancels() {
        let mut app = TuiApp::new("m", "s").unwrap();
        app.turn_active = true;
        let action = handle_key(&mut app, k(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(action, InputAction::CancelTurn);
        assert!(app.cancel_requested);
    }

    #[test]
    fn q_quits_only_when_idle() {
        let mut app = TuiApp::new("m", "s").unwrap();
        app.turn_active = true;
        let action = handle_key(&mut app, k(KeyCode::Char('q'), KeyModifiers::NONE));
        // Mid-turn `q` is treated as text input.
        assert_eq!(action, InputAction::None);
        assert!(!app.should_quit);
        assert_eq!(app.input, "q");

        // After the turn ends and the input is clear, `q` quits.
        app.turn_active = false;
        app.input.clear();
        let action = handle_key(&mut app, k(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(action, InputAction::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn modal_y_allows_once() {
        let mut app = TuiApp::new("m", "s").unwrap();
        let (tx, _rx) = tokio::sync::oneshot::channel();
        app.open_permission_modal("p1".into(), "Edit".into(), "preview".into(), tx);
        let action = handle_key(&mut app, k(KeyCode::Char('y'), KeyModifiers::NONE));
        assert_eq!(
            action,
            InputAction::ModalResolved(PermissionResponse::AllowOnce)
        );
    }
}
