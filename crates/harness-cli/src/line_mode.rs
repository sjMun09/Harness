//! Line-mode rendering (PLAN §3.1). Writes a one-line tool-call marker to
//! stderr at each `ToolCallStart`, and an indented outcome line at each
//! `ToolCallEnd`. Kept on stderr so the final assistant text (which `print_final`
//! writes to stdout) is cleanly separable — `harness ask ... > out.txt` still
//! captures the model's answer without the progress chatter.
//!
//! Format:
//!   ⏺ ToolName(preview)
//!     ↳ ok: <first line of summary>
//!   or
//!     ↳ err: <error message>
//!
//! `TurnEvent::TurnStart` is ignored for MVP — the existing tracing covers it.

use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};

use harness_core::engine::{EventSink, TurnEvent};

/// Build a sink that writes to stderr. Uses ANSI styling only when the
/// terminal supports it.
pub fn stderr_sink() -> EventSink {
    let styled = io::stderr().is_terminal();
    let writer = Arc::new(Mutex::new(io::stderr()));
    Arc::new(move |ev: TurnEvent| {
        let mut w = writer.lock().unwrap();
        let _ = render(&mut *w, &ev, styled);
    })
}

fn render<W: Write>(w: &mut W, ev: &TurnEvent, styled: bool) -> io::Result<()> {
    match ev {
        TurnEvent::TurnStart { .. } => Ok(()),
        TurnEvent::ToolCallStart { name, preview, .. } => {
            let dot = if styled { "\x1b[36m⏺\x1b[0m" } else { "⏺" };
            let shown = truncate_line(preview, 160);
            writeln!(w, "{dot} {name}({shown})")
        }
        TurnEvent::ToolCallEnd {
            ok, summary_head, ..
        } => {
            let (arrow, tag) = if *ok {
                (if styled { "\x1b[2m↳\x1b[0m" } else { "↳" }, "ok")
            } else {
                (if styled { "\x1b[31m↳\x1b[0m" } else { "↳" }, "err")
            };
            let shown = truncate_line(summary_head, 160);
            writeln!(w, "  {arrow} {tag}: {shown}")
        }
        TurnEvent::Cancelled { .. } => {
            let mark = if styled { "\x1b[33m⏹\x1b[0m" } else { "⏹" };
            writeln!(w, "{mark} cancelled (user interrupt)")
        }
    }
}

/// Replace control chars, cap at `max` chars (character-count, not bytes, to
/// avoid splitting a multi-byte codepoint). Appends `…` when truncated.
fn truncate_line(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max * 4));
    for (written, c) in s.chars().enumerate() {
        if written >= max {
            out.push('…');
            return out;
        }
        match c {
            '\n' | '\r' => out.push(' '),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_tool_start_and_end() {
        let mut buf = Vec::new();
        render(
            &mut buf,
            &TurnEvent::ToolCallStart {
                id: "a".into(),
                name: "Read".into(),
                preview: "Read src/lib.rs".into(),
            },
            false,
        )
        .unwrap();
        render(
            &mut buf,
            &TurnEvent::ToolCallEnd {
                id: "a".into(),
                name: "Read".into(),
                ok: true,
                summary_head: "read 120 lines".into(),
            },
            false,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("⏺ Read(Read src/lib.rs)"));
        assert!(s.contains("↳ ok: read 120 lines"));
    }

    #[test]
    fn renders_error_tail() {
        let mut buf = Vec::new();
        render(
            &mut buf,
            &TurnEvent::ToolCallEnd {
                id: "b".into(),
                name: "Edit".into(),
                ok: false,
                summary_head: "validation: old_string not found".into(),
            },
            false,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("↳ err: validation: old_string not found"));
    }

    #[test]
    fn truncate_replaces_newlines_and_caps() {
        let got = truncate_line("line1\nline2", 160);
        assert_eq!(got, "line1 line2");
        let long: String = "x".repeat(200);
        let got = truncate_line(&long, 10);
        assert_eq!(got, "xxxxxxxxxx…");
    }

    #[test]
    fn turn_start_is_silent() {
        let mut buf = Vec::new();
        render(&mut buf, &TurnEvent::TurnStart { turn_idx: 0 }, false).unwrap();
        assert!(buf.is_empty());
    }
}
