//! Interactive `[y/n/a/d]` TTY prompt for `Decision::Ask` permission outcomes.
//!
//! # Status: partially integrated — blocked at the core boundary
//!
//! The engine (`harness-core::engine`) calls `ctx.permission.evaluate(...)`
//! directly — `PermissionSnapshot` is a concrete struct, not a trait, and
//! when `evaluate` returns `Decision::Ask` the engine immediately surfaces
//! that as a `ToolResult` error (`engine.rs` around line 606):
//!
//! ```text
//! Decision::Ask => {
//!     // Headless MVP: surface as error so the caller sees it.
//!     return error_result(id, &format!("permission requires user approval …"));
//! }
//! ```
//!
//! To inject the prompt flow below (`ask_user`) we would need one of:
//!
//! 1. Add an `Option<Box<dyn AskPrompt>>` to `ToolCtx`, consulted inside the
//!    `Decision::Ask` arm, so the CLI can register this prompt for each
//!    tool-use that trips Ask. ← Requires editing `harness-core`, which is
//!    out of scope for this workstream.
//! 2. Swap `PermissionSnapshot` for a trait + dyn — also `harness-core` and
//!    `harness-perm` surface changes.
//!
//! Neither option is doable inside this workstream's file scope
//! (crates/harness-cli only). The prompt module below is fully implemented
//! and unit-tested so the wiring work is one diff away once the core hook
//! lands; `main.rs` notes the TODO.
//!
//! TODO: need core hook — PLAN §5.8 iter-2. When it's available, wire
//! `ask_user` into the engine's `Decision::Ask` branch via the new
//! `ToolCtx` callback.

use std::io::{self, BufRead, IsTerminal, Write};

/// User answer to the Ask prompt. Mirrors Claude Code's choices verbatim so
/// muscle memory carries over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskAnswer {
    /// `[y]es` — allow this single call.
    Yes,
    /// `[n]o` — deny this single call (return a synthetic tool error).
    No,
    /// `[a]lways` — persist allow for `(tool, input)` in the session cache
    /// (`PermissionSnapshot::remember_always`).
    Always,
    /// `[d]on't ask again` — deny for the rest of the session (any Ask
    /// returns deny without prompting again).
    DontAsk,
}

/// Behavior when stdin is not a TTY. Mirrors the current non-interactive
/// failure mode: return a descriptive error string so the engine surfaces
/// it as a `ToolResult` error with `is_error=true`. The CLI reports this
/// string verbatim to the user, so it should suggest a fix.
#[must_use]
pub fn non_tty_hint(tool: &str) -> String {
    format!(
        "permission requires user approval for {tool}; stdin is not a TTY. \
         Configure settings.permissions.allow, run with --dangerously-skip-permissions, \
         or re-run interactively so you can answer [y/n/a/d]."
    )
}

/// Show the prompt and read a single char (case-insensitive). Anything
/// other than `y/n/a/d` loops up to `max_attempts` times, then defaults to
/// `No` — we err deny-side to avoid "user mashed Enter → allow".
///
/// Writes go to `out`; reads come from `input`. Injecting both makes this
/// unit-testable without fiddling with real stdin/stderr.
pub fn ask_user_inner<R: BufRead, W: Write>(
    tool: &str,
    input: &serde_json::Value,
    out: &mut W,
    input_reader: &mut R,
    max_attempts: usize,
) -> io::Result<AskAnswer> {
    let compact = summarize_input(input);
    writeln!(
        out,
        "\n\x1b[33m!\x1b[0m Tool {tool} requires approval: {compact}"
    )?;
    write!(
        out,
        "[y]es / [n]o / [a]lways (this (tool, input) combo) / [d]on't ask again: "
    )?;
    out.flush()?;

    for _ in 0..max_attempts {
        let mut line = String::new();
        let n = input_reader.read_line(&mut line)?;
        if n == 0 {
            // EOF — treat as No so we don't loop forever on a closed stdin.
            return Ok(AskAnswer::No);
        }
        match line.trim().chars().next().map(|c| c.to_ascii_lowercase()) {
            Some('y') => return Ok(AskAnswer::Yes),
            Some('n') => return Ok(AskAnswer::No),
            Some('a') => return Ok(AskAnswer::Always),
            Some('d') => return Ok(AskAnswer::DontAsk),
            _ => {
                write!(out, "Please answer [y/n/a/d]: ")?;
                out.flush()?;
            }
        }
    }
    // Exhausted retries — default deny.
    Ok(AskAnswer::No)
}

/// Real-stdin wrapper. Returns the non-TTY hint as `Err` when stdin isn't
/// a terminal so callers fall back to the old non-interactive error text.
pub fn ask_user(tool: &str, input: &serde_json::Value) -> Result<AskAnswer, String> {
    if !io::stdin().is_terminal() {
        return Err(non_tty_hint(tool));
    }
    let stdin = io::stdin();
    let mut locked = stdin.lock();
    let stderr = io::stderr();
    let mut stderr_locked = stderr.lock();
    ask_user_inner(tool, input, &mut stderr_locked, &mut locked, 3)
        .map_err(|e| format!("prompt I/O error: {e}"))
}

/// Render a short summary of the JSON input for the prompt. Keeps the
/// approval line on one screen row even for chunky `Edit` inputs.
fn summarize_input(input: &serde_json::Value) -> String {
    // Common high-signal keys across builtin tools — pick the first one
    // that's present. Falls back to a clipped JSON dump.
    for key in ["command", "file_path", "path", "pattern", "url"] {
        if let Some(v) = input.get(key).and_then(|v| v.as_str()) {
            let clipped = clip(v, 80);
            return format!("{key}={clipped}");
        }
    }
    let dump = serde_json::to_string(input).unwrap_or_else(|_| "<unserializable>".into());
    clip(&dump, 100)
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n - 1).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn drive(input_text: &str) -> (Vec<u8>, AskAnswer) {
        let mut out = Vec::<u8>::new();
        let mut reader = std::io::Cursor::new(input_text.as_bytes().to_vec());
        let ans = ask_user_inner("Bash", &json!({"command": "ls -la"}), &mut out, &mut reader, 3)
            .expect("ask_user_inner");
        (out, ans)
    }

    #[test]
    fn y_accepts() {
        let (_, a) = drive("y\n");
        assert_eq!(a, AskAnswer::Yes);
    }

    #[test]
    fn n_denies() {
        let (_, a) = drive("n\n");
        assert_eq!(a, AskAnswer::No);
    }

    #[test]
    fn a_always() {
        let (_, a) = drive("a\n");
        assert_eq!(a, AskAnswer::Always);
    }

    #[test]
    fn d_dont_ask() {
        let (_, a) = drive("d\n");
        assert_eq!(a, AskAnswer::DontAsk);
    }

    #[test]
    fn case_insensitive() {
        let (_, a) = drive("Y\n");
        assert_eq!(a, AskAnswer::Yes);
    }

    #[test]
    fn garbage_then_y() {
        let (_, a) = drive("??\n \nyes\n");
        assert_eq!(a, AskAnswer::Yes);
    }

    #[test]
    fn eof_is_no() {
        // Cursor that returns 0-read immediately — simulate closed stdin.
        let mut out = Vec::<u8>::new();
        let mut reader = std::io::Cursor::new(Vec::<u8>::new());
        let a = ask_user_inner("Bash", &json!({}), &mut out, &mut reader, 3).unwrap();
        assert_eq!(a, AskAnswer::No);
    }

    #[test]
    fn summarize_picks_command() {
        let s = summarize_input(&json!({"command": "git status --short"}));
        assert!(s.starts_with("command="));
        assert!(s.contains("git status"));
    }

    #[test]
    fn summarize_picks_file_path() {
        let s = summarize_input(&json!({"file_path": "/etc/passwd"}));
        assert_eq!(s, "file_path=/etc/passwd");
    }

    #[test]
    fn summarize_falls_back_to_dump() {
        let s = summarize_input(&json!({"unknown_field": "v"}));
        assert!(s.contains("unknown_field"));
    }

    #[test]
    fn non_tty_hint_mentions_tool() {
        let s = non_tty_hint("Bash");
        assert!(s.contains("Bash"));
        assert!(s.contains("--dangerously-skip-permissions"));
    }
}
