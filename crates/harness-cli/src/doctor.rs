//! `harness doctor` — runtime diagnostics.
//!
//! Prints a human-readable report of the things that make new installs fail
//! in the first 60 seconds:
//!
//! - auth state (API key env var, OAuth keychain if feature on, refuse-lock)
//! - cwd + whether it's in the trust store
//! - `settings.json` resolved path + whether it exists
//! - effective `OPENAI_BASE_URL` + whether it's local
//! - feature flags compiled in
//! - `.harnessignore` presence at cwd root
//!
//! All probes are read-only; no network calls, no files written.
//!
//! Output format: plain ANSI colors (green/yellow/red + bold). We do not
//! auto-detect TTY here — even when redirected to a file the report is still
//! legible, and the escape codes don't break machine reading (users who want
//! plain text can pipe through `sed`).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

// ANSI color escapes. Kept narrow — no external `colored` dep; the rest of
// the CLI hand-codes ANSI the same way (see `trust.rs`, `main.rs`).
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// Severity of a single check. Reserved for future machine-readable output
/// (e.g. `--format=json`); today it only drives the prefix glyph + color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn prefix(self) -> String {
        match self {
            Self::Ok => format!("{GREEN}✓{RESET}"),
            Self::Warn => format!("{YELLOW}!{RESET}"),
            Self::Fail => format!("{RED}✗{RESET}"),
        }
    }
}

/// Entry point. Writes to stdout so it's easy to pipe / redirect.
pub fn cmd_doctor() {
    let mut out = io::stdout().lock();
    // Ignore write errors — a broken pipe in the middle of diagnostic
    // output is not worth surfacing as a non-zero exit.
    let _ = print_report(&mut out);
}

fn print_report<W: Write>(out: &mut W) -> io::Result<()> {
    writeln!(out, "{BOLD}Harness doctor{RESET}")?;
    writeln!(out, "  harness {}", env!("CARGO_PKG_VERSION"))?;
    writeln!(out)?;

    section(out, "Auth")?;
    auth_block(out)?;
    writeln!(out)?;

    section(out, "Working directory")?;
    cwd_block(out)?;
    writeln!(out)?;

    section(out, "Settings")?;
    settings_block(out)?;
    writeln!(out)?;

    section(out, "Provider routing")?;
    base_url_block(out)?;
    writeln!(out)?;

    section(out, "Features (compiled)")?;
    features_block(out)?;
    writeln!(out)?;

    section(out, "Ignore files")?;
    ignore_block(out)?;
    Ok(())
}

fn section<W: Write>(out: &mut W, title: &str) -> io::Result<()> {
    writeln!(out, "{BOLD}{title}{RESET}")
}

fn line<W: Write>(out: &mut W, status: Status, msg: &str) -> io::Result<()> {
    writeln!(out, "  {} {msg}", status.prefix())
}

// ---- sections ----

fn auth_block<W: Write>(out: &mut W) -> io::Result<()> {
    let has_key = env_nonempty("ANTHROPIC_API_KEY");
    let refuse = env_nonempty("HARNESS_REFUSE_API_KEY");
    let has_openai = env_nonempty("OPENAI_API_KEY");

    if has_key {
        line(out, Status::Ok, "ANTHROPIC_API_KEY is set")?;
    } else {
        line(out, Status::Warn, "ANTHROPIC_API_KEY not set")?;
    }
    if has_openai {
        line(out, Status::Ok, "OPENAI_API_KEY is set")?;
    } else {
        line(
            out,
            Status::Warn,
            "OPENAI_API_KEY not set (only needed for OpenAI or non-local OpenAI-compat)",
        )?;
    }

    if refuse {
        line(
            out,
            Status::Ok,
            "HARNESS_REFUSE_API_KEY=1 (metered API paths blocked; OAuth and localhost still allowed)",
        )?;
    } else {
        line(
            out,
            Status::Warn,
            "HARNESS_REFUSE_API_KEY not set (metered API paths permitted)",
        )?;
    }

    // OAuth keychain status — only show on feature-enabled builds.
    #[cfg(feature = "claude-code-oauth")]
    {
        match harness_provider::load_from_claude_code_keychain() {
            Ok(_) => line(
                out,
                Status::Ok,
                "Claude Code OAuth token present in keychain",
            )?,
            Err(e) => line(
                out,
                Status::Warn,
                &format!("Claude Code OAuth token unavailable: {e}"),
            )?,
        }
    }
    #[cfg(not(feature = "claude-code-oauth"))]
    {
        line(
            out,
            Status::Warn,
            "OAuth feature not compiled in — rebuild with --features claude-code-oauth to enable",
        )?;
    }
    Ok(())
}

fn cwd_block<W: Write>(out: &mut W) -> io::Result<()> {
    match std::env::current_dir() {
        Ok(cwd) => {
            line(out, Status::Ok, &format!("cwd: {}", cwd.display()))?;
            match cwd_trusted(&cwd) {
                Ok(true) => line(out, Status::Ok, "cwd is in the trust store")?,
                Ok(false) => line(
                    out,
                    Status::Warn,
                    "cwd is not in the trust store — first run will prompt (or use --trust-cwd)",
                )?,
                Err(e) => line(
                    out,
                    Status::Warn,
                    &format!("could not read trust store: {e}"),
                )?,
            }
        }
        Err(e) => line(out, Status::Fail, &format!("could not read cwd: {e}"))?,
    }
    Ok(())
}

/// Non-fatal read of the trust store to check if `cwd` (canonicalized)
/// is already accepted. Kept self-contained here so `trust.rs` doesn't have
/// to grow a public read API for the doctor path — the store format is
/// intentionally simple.
fn cwd_trusted(cwd: &Path) -> io::Result<bool> {
    let path = harness_mem::state_dir().join("trust.json");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if bytes.is_empty() {
        return Ok(false);
    }
    let store: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let hash = hash_path(&canonical);
    Ok(store
        .get("trusted")
        .and_then(|t| t.as_object())
        .is_some_and(|obj| obj.contains_key(&hash)))
}

/// Mirror of `trust::hash_cwd` (hex sha256 of canonicalized path bytes).
/// Inlined here to avoid exposing internals of `trust.rs`.
fn hash_path(canonical: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        h.update(canonical.as_os_str().as_bytes());
    }
    #[cfg(not(unix))]
    {
        h.update(canonical.to_string_lossy().as_bytes());
    }
    let digest = h.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

fn settings_block<W: Write>(out: &mut W) -> io::Result<()> {
    match harness_core::config::user_settings_path() {
        Some(p) => {
            let exists = p.exists();
            if exists {
                line(
                    out,
                    Status::Ok,
                    &format!("settings.json: {} (exists)", p.display()),
                )?;
            } else {
                line(
                    out,
                    Status::Warn,
                    &format!(
                        "settings.json: {} (not yet created — defaults in use)",
                        p.display()
                    ),
                )?;
            }
        }
        None => line(
            out,
            Status::Warn,
            "settings.json path could not be resolved (no XDG base dir available)",
        )?,
    }
    let project = PathBuf::from(".harness").join("settings.json");
    if project.exists() {
        line(
            out,
            Status::Ok,
            &format!("project override present: {}", project.display()),
        )?;
    }
    Ok(())
}

fn base_url_block<W: Write>(out: &mut W) -> io::Result<()> {
    match std::env::var("OPENAI_BASE_URL") {
        Ok(raw) if !raw.trim().is_empty() => match url::Url::parse(&raw) {
            Ok(u) => {
                let local = harness_provider::is_local_url(&u);
                let label = if local {
                    "local (loopback) — HARNESS_REFUSE_API_KEY lock still permits this"
                } else {
                    "remote — treated as metered; HARNESS_REFUSE_API_KEY will block"
                };
                line(
                    out,
                    if local { Status::Ok } else { Status::Warn },
                    &format!("OPENAI_BASE_URL={u} ({label})"),
                )?;
            }
            Err(e) => line(
                out,
                Status::Fail,
                &format!("OPENAI_BASE_URL is set but does not parse as a URL: {e}"),
            )?,
        },
        _ => line(
            out,
            Status::Ok,
            "OPENAI_BASE_URL not set — OpenAI default endpoint in use for OpenAI models",
        )?,
    }
    Ok(())
}

fn features_block<W: Write>(out: &mut W) -> io::Result<()> {
    let tui = cfg!(feature = "tui");
    let oauth = cfg!(feature = "claude-code-oauth");
    line(
        out,
        if tui { Status::Ok } else { Status::Warn },
        &format!("tui: {}", if tui { "on" } else { "off" }),
    )?;
    line(
        out,
        if oauth { Status::Ok } else { Status::Warn },
        &format!("claude-code-oauth: {}", if oauth { "on" } else { "off" }),
    )?;
    Ok(())
}

fn ignore_block<W: Write>(out: &mut W) -> io::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let p = cwd.join(".harnessignore");
    if p.exists() {
        line(
            out,
            Status::Ok,
            &format!(".harnessignore: {} (exists)", p.display()),
        )?;
    } else {
        line(
            out,
            Status::Warn,
            &format!(
                ".harnessignore: {} (not present — add one to tune Glob/Grep)",
                p.display()
            ),
        )?;
    }
    Ok(())
}

fn env_nonempty(var: &str) -> bool {
    std::env::var(var)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_nonempty_semantics() {
        // Negative path — empty var is treated as not set.
        std::env::set_var("HARNESS_DOCTOR_TEST_EMPTY", "");
        assert!(!env_nonempty("HARNESS_DOCTOR_TEST_EMPTY"));
        std::env::set_var("HARNESS_DOCTOR_TEST_SET", "1");
        assert!(env_nonempty("HARNESS_DOCTOR_TEST_SET"));
        std::env::remove_var("HARNESS_DOCTOR_TEST_EMPTY");
        std::env::remove_var("HARNESS_DOCTOR_TEST_SET");
    }

    #[test]
    fn print_report_runs_without_panic() {
        // Sanity: the whole report can be written to a buffer.
        let mut buf = Vec::<u8>::new();
        print_report(&mut buf).expect("report writes");
        let s = String::from_utf8(buf).unwrap();
        // Must contain every section header.
        assert!(s.contains("Auth"));
        assert!(s.contains("Working directory"));
        assert!(s.contains("Settings"));
        assert!(s.contains("Provider routing"));
        assert!(s.contains("Features"));
        assert!(s.contains("Ignore files"));
    }

    #[test]
    fn hash_path_is_stable_and_hex64() {
        let p = std::env::temp_dir();
        let h1 = hash_path(&p);
        let h2 = hash_path(&p);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
