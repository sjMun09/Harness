//! Redaction layer for tracing output. PLAN §8.2.
//!
//! # Approach: regex-based string-replace via a custom `MakeWriter`
//!
//! Implementing a full custom `FormatFields` requires threading the regex through
//! multiple trait-object boundaries (`FormatFields`, `FormatEvent`, `Layer`) in
//! a way that becomes quite hairy with `tracing-subscriber`'s generics.
//!
//! Instead we wrap the output `Write`r: every time tracing flushes a log line we
//! run a single regex replace over the buffer before forwarding it to stderr.
//! This keeps the implementation small (~50 LOC), avoids unsafe, and is fast
//! (one compiled `Regex` in an `OnceLock`, matched once per log line).
//!
//! Trade-off: false positives on log *messages* (not just field names) that
//! happen to contain the pattern words — we err on the side of over-redacting
//! per the spec.

use std::io::{self, Write};

use std::sync::OnceLock;

use regex::Regex;

/// Field names whose values are replaced with `***` (exact match, case-folded
/// by the regex).
#[allow(dead_code)]
pub const REDACTED_FIELD_NAMES: &[&str] = &[
    "authorization",
    "Authorization",
    "x-api-key",
    "X-Api-Key",
    "X-API-Key",
    "api_key",
    "apikey",
    "ANTHROPIC_API_KEY",
    "anthropic_api_key",
    "OPENAI_API_KEY",
    "openai_api_key",
    "GITHUB_TOKEN",
    "github_token",
];

/// Environment-variable *prefixes* whose full value gets redacted.
#[allow(dead_code)]
pub const REDACTED_ENV_PREFIXES: &[&str] = &["AWS_", "GCP_", "AZURE_"];

/// Returns `true` when the field/header name `name` should have its value
/// replaced with `***`.
///
/// Checks:
/// 1. Case-insensitive match against [`REDACTED_FIELD_NAMES`].
/// 2. Any entry in [`REDACTED_ENV_PREFIXES`] is a case-sensitive prefix of
///    `name`.
#[allow(dead_code)]
pub fn is_redacted_key(name: &str) -> bool {
    let lower = name.to_lowercase();
    for &candidate in REDACTED_FIELD_NAMES {
        if lower == candidate.to_lowercase() {
            return true;
        }
    }
    for &prefix in REDACTED_ENV_PREFIXES {
        if name.starts_with(prefix) {
            return true;
        }
    }
    false
}

/// A compiled regex that matches patterns like:
///   `Authorization: Bearer sk-abc`
///   `ANTHROPIC_API_KEY=sk-abc`
///   `x-api-key="sk-abc"`
///   `AWS_SECRET_ACCESS_KEY sk-abc`
///
/// Capture group 1 = the key name; everything after the separator is replaced
/// with `***`.
fn redact_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // r#"..."# avoids any backslash escaping issues inside the raw string.
        // Pattern: key name, then one-or-more separator chars, then the secret token.
        // Separators accepted: = : ' " space or tab.
        // Match: key_name <separators> [Bearer ]? secret_value
        // Separators: = : ' " space tab (one or more)
        // Optionally skip a single "Bearer" or "basic" scheme word before the secret.
        Regex::new(
            r#"(?i)(authorization|x-api-key|[A-Za-z_]*(api[_-]?key|token|secret)[A-Za-z_]*)[=:'" \t]+(?:(?:bearer|basic|token)\s+)?\S+"#,
        )
        .expect("hardcoded regex is valid")
    })
}

/// Apply the redaction regex to a single log line, returning the sanitised
/// version.  The replacement format is `<KEY>=***` so the key name is
/// preserved for debugging while the secret value is hidden.
pub fn redact_line(line: &str) -> std::borrow::Cow<'_, str> {
    redact_regex().replace_all(line, "$1=***")
}

// ─── MakeWriter plumbing ────────────────────────────────────────────────────

/// A `Write`r that buffers an entire log line, applies [`redact_line`], then
/// forwards the sanitised bytes to an inner writer.
pub struct RedactingWriter<W: Write> {
    inner: W,
    buf: Vec<u8>,
}

impl<W: Write> RedactingWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            buf: Vec::with_capacity(256),
        }
    }
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let raw = String::from_utf8_lossy(&self.buf);
        let sanitised = redact_line(&raw);
        self.inner.write_all(sanitised.as_bytes())?;
        self.inner.flush()?;
        self.buf.clear();
        Ok(())
    }
}

impl<W: Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            let _ = self.flush();
        }
    }
}

/// A `MakeWriter` factory that wraps every produced writer in
/// [`RedactingWriter`].
#[derive(Clone, Debug)]
pub struct RedactingMakeWriter<M> {
    inner: M,
}

impl<M> RedactingMakeWriter<M> {
    pub fn new(inner: M) -> Self {
        Self { inner }
    }
}

impl<'a, M> tracing_subscriber::fmt::MakeWriter<'a> for RedactingMakeWriter<M>
where
    M: tracing_subscriber::fmt::MakeWriter<'a>,
    M::Writer: 'a,
{
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter::new(self.inner.make_writer())
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // is_redacted_key tests
    #[test]
    fn test_authorization_lower() {
        assert!(is_redacted_key("authorization"));
    }

    #[test]
    fn test_authorization_title() {
        assert!(is_redacted_key("Authorization"));
    }

    #[test]
    fn test_x_api_key_lower() {
        assert!(is_redacted_key("x-api-key"));
    }

    #[test]
    fn test_anthropic_api_key() {
        assert!(is_redacted_key("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn test_aws_prefix() {
        assert!(is_redacted_key("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn test_user_name_not_redacted() {
        assert!(!is_redacted_key("user_name"));
    }

    #[test]
    fn test_path_not_redacted() {
        assert!(!is_redacted_key("PATH"));
    }

    // redact_line tests — the raw secret value must NOT appear in output
    #[test]
    fn test_authorization_bearer_redacted() {
        let line = "Authorization: Bearer sk-abc123";
        let out = redact_line(line);
        assert!(
            !out.contains("sk-abc123"),
            "secret survived redaction: {out}"
        );
        assert!(out.contains("***"), "expected *** placeholder in: {out}");
    }

    #[test]
    fn test_api_key_equals_redacted() {
        let line = "ANTHROPIC_API_KEY=sk-ant-secret999";
        let out = redact_line(line);
        assert!(
            !out.contains("sk-ant-secret999"),
            "secret survived redaction: {out}"
        );
    }

    #[test]
    fn test_x_api_key_colon_redacted() {
        let line = "x-api-key: supersecretvalue";
        let out = redact_line(line);
        assert!(
            !out.contains("supersecretvalue"),
            "secret survived redaction: {out}"
        );
    }

    #[test]
    fn test_unrelated_line_unchanged() {
        let line = "user logged in from 192.168.1.1";
        let out = redact_line(line);
        assert_eq!(out, line, "non-secret line should be unchanged");
    }
}
