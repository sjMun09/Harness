//! Session-JSONL secret redaction. PLAN §8.2.
//!
//! Applied to every `Record` before append: prompts, assistant replies,
//! tool arguments, and `Meta` detail values. Session files live under
//! `$XDG_STATE_HOME/harness/sessions/*.jsonl` indefinitely — a pasted API
//! key in a single prompt becomes a permanent leak on disk if we don't
//! scrub here.
//!
//! Design:
//!   - Regex list compiled once via `OnceLock`.
//!   - Pattern match replaced with `[REDACTED:<kind>]` — the kind tag lets
//!     a session reader see that redaction happened and what triggered it.
//!   - Idempotent: redacted markers contain no character that matches any
//!     pattern, so a second pass is a no-op. Roundtrip tests rely on this.
//!   - Conservative: false positives are OK (operator can re-enter the
//!     secret if it really wasn't one); false negatives are NOT — once a
//!     real secret lands on disk it's already compromised.
//!
//! Patterns (PLAN §8.2 checklist):
//!   - `sk-…` — Anthropic / OpenAI / generic API keys
//!   - `ghp_…` — GitHub personal access token
//!   - `AKIA…` — AWS access key id (partial — pair to scan value too)
//!   - `AIza…` — Google API key
//!   - `xox[bp]-…` — Slack bot / user token
//!   - `-----BEGIN … PRIVATE KEY-----` — SSH / PGP / RSA private key header

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

/// One redaction rule — a compiled pattern + the label to substitute.
#[derive(Debug)]
pub struct Pattern {
    pub kind: &'static str,
    pub regex: Regex,
}

/// Build-once list of patterns. Ordered highest-confidence first so the
/// label attached to a match reflects the most specific format.
fn patterns() -> &'static [Pattern] {
    static P: OnceLock<Vec<Pattern>> = OnceLock::new();
    P.get_or_init(|| {
        let specs: &[(&str, &str)] = &[
            // Anthropic / OpenAI / generic sk-prefixed keys (min 20 char body
            // to avoid `sk-123` user-chosen placeholders, though false positives
            // on short `sk-…` strings are fine — conservative by design).
            ("sk", r"sk-[A-Za-z0-9_-]{20,}"),
            // GitHub PAT — fixed 36-char body.
            ("github_pat", r"ghp_[A-Za-z0-9]{36}"),
            // GitHub fine-grained PAT + other ghX_ variants.
            ("github_token", r"gh[oursp]_[A-Za-z0-9]{20,}"),
            // AWS access key id — 16-char body after AKIA.
            ("aws_akid", r"AKIA[0-9A-Z]{16}"),
            // Google API key — 35-char body.
            ("google_api_key", r"AIza[0-9A-Za-z_-]{35}"),
            // Slack bot / user / app-level token.
            ("slack_token", r"xox[bpars]-[A-Za-z0-9-]{10,}"),
            // Private-key PEM headers — strips just the header line, enough
            // to make the block unparseable downstream. Matches RSA /
            // OPENSSH / DSA / EC / plain variants.
            (
                "private_key",
                r"-----BEGIN (?:RSA |OPENSSH |DSA |EC |ENCRYPTED |PGP )?PRIVATE KEY-----",
            ),
        ];
        specs
            .iter()
            .map(|(kind, pat)| Pattern {
                kind,
                regex: Regex::new(pat).expect("static redaction regex"),
            })
            .collect()
    })
}

/// Returns the redacted form of `s`. Allocates only when a pattern fires,
/// so the common no-secret path is cheap (single pass per pattern).
#[must_use]
pub fn redact_str(s: &str) -> String {
    let mut out = s.to_string();
    for p in patterns() {
        // Using `replace_all` — one allocation per pattern that matches.
        let rep = format!("[REDACTED:{}]", p.kind);
        out = p.regex.replace_all(&out, rep.as_str()).into_owned();
    }
    out
}

/// In-place recurse through a `serde_json::Value`, redacting every string
/// node and recursing into arrays / object values.
pub fn redact_value(v: &mut Value) {
    match v {
        Value::String(s) => {
            let red = redact_str(s);
            if red.as_str() != s.as_str() {
                *s = red;
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item);
            }
        }
        Value::Object(map) => {
            for (_k, item) in map.iter_mut() {
                redact_value(item);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn sk_key_redacted() {
        let raw = "here is my key sk-ant-api03-abcdefghij1234567890XYZ end";
        let out = redact_str(raw);
        assert!(!out.contains("sk-ant-api03"), "leaked: {out}");
        assert!(out.contains("[REDACTED:sk]"));
    }

    #[test]
    fn github_pat_redacted() {
        // 36 chars after the prefix — must match the rule exactly.
        let tok = "ghp_0123456789ABCDEFGHIJ0123456789abcdef";
        assert_eq!(tok.len() - 4, 36);
        let out = redact_str(&format!("token {tok} here"));
        assert!(!out.contains(tok), "leaked: {out}");
        assert!(out.contains("[REDACTED:github_pat]"));
    }

    #[test]
    fn aws_akid_redacted() {
        let raw = "AKIAIOSFODNN7EXAMPLE and more";
        let out = redact_str(raw);
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"), "leaked: {out}");
        assert!(out.contains("[REDACTED:aws_akid]"));
    }

    #[test]
    fn google_api_key_redacted() {
        let k = "AIzaSyA-abcdefghijklmnopqrstuvwxyz1234567";
        let out = redact_str(&format!("GOOGLE_API_KEY={k}"));
        assert!(!out.contains(k), "leaked: {out}");
        assert!(out.contains("[REDACTED:google_api_key]"));
    }

    #[test]
    fn slack_token_redacted() {
        let raw = "xoxb-1234567890-0987654321-abcdefghij";
        let out = redact_str(raw);
        assert!(!out.contains(raw), "leaked: {out}");
        assert!(out.contains("[REDACTED:slack_token]"));
    }

    #[test]
    fn private_key_header_redacted() {
        let raw = "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAAB3...\n";
        let out = redact_str(raw);
        assert!(!out.contains("BEGIN OPENSSH PRIVATE KEY"), "leaked: {out}");
        assert!(out.contains("[REDACTED:private_key]"));

        let raw2 = "-----BEGIN PRIVATE KEY-----\nMIIE...\n";
        let out2 = redact_str(raw2);
        assert!(!out2.contains("BEGIN PRIVATE KEY"), "leaked: {out2}");
    }

    #[test]
    fn clean_string_unchanged() {
        let raw = "Hello world, no secrets here. user@example.com /tmp/foo";
        assert_eq!(redact_str(raw), raw);
    }

    /// Re-running redaction on an already-redacted string must not further
    /// mangle the marker. This is the property `append` relies on for
    /// idempotency when a session is rewritten or replayed.
    #[test]
    fn redaction_is_idempotent() {
        let raw = "sk-ant-api03-abcdefghij1234567890XYZ";
        let once = redact_str(raw);
        let twice = redact_str(&once);
        assert_eq!(once, twice, "redaction not idempotent");
    }

    #[test]
    fn value_recurses_arrays_and_objects() {
        let mut v = serde_json::json!({
            "prompt": "use sk-ant-api03-abcdefghij1234567890XYZ please",
            "meta": {
                "nested": ["ok", "also AKIAIOSFODNN7EXAMPLE bad"],
                "count": 3,
                "flag": true,
            }
        });
        redact_value(&mut v);
        let s = v.to_string();
        assert!(!s.contains("sk-ant-api03"), "top-level leak: {s}");
        assert!(!s.contains("AKIAIOSFODNN7EXAMPLE"), "nested leak: {s}");
        // numbers / bools untouched
        assert!(s.contains("\"count\":3"));
        assert!(s.contains("\"flag\":true"));
    }

    #[test]
    fn unrelated_tokens_not_touched() {
        // sk- too short to trip the 20-char body requirement.
        let raw = "sk-abc";
        assert_eq!(redact_str(raw), raw);
    }
}
