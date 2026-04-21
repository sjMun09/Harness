//! `--metrics-json` writer for `harness ask`.
//!
//! Emits a strict JSON metrics file after the turn loop completes so
//! benchmark drivers (bench/*) can read a machine-readable summary
//! instead of scraping stderr. Schema is fixed — downstream tooling
//! pins to `schema_version = 1` and the 13-field shape below.
//!
//! Write is atomic: contents go to `<path>.tmp`, then rename to `<path>`.

use std::path::{Path, PathBuf};

use harness_proto::{ContentBlock, Message, Role};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Fixed-shape metrics record. Field order mirrors the schema in the task spec.
#[derive(Debug, Serialize)]
pub struct AskMetrics {
    pub schema_version: u32,
    pub tool: &'static str,
    pub model: String,
    pub provider: &'static str,
    pub wall_ms: u128,
    pub api_ms: Option<u128>,
    pub exit_code: i32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub num_turns: u32,
    pub prompt_sha256: String,
    pub session_id: String,
}

/// Compute usage sums and turn count across all assistant messages.
///
/// Returns `(input, output, cache_read_opt, cache_creation_opt, num_turns)`.
/// Cache fields are `None` when no assistant message reported usage at all
/// (matches provider-not-supported semantics per schema). If at least one
/// assistant message carried usage, the cache fields are `Some(sum)` — this
/// may legitimately be `Some(0)` when the provider returns zero cache tokens.
pub fn summarize(messages: &[Message]) -> (u64, u64, Option<u64>, Option<u64>, u32) {
    let mut input: u64 = 0;
    let mut output: u64 = 0;
    let mut cache_read: u64 = 0;
    let mut cache_create: u64 = 0;
    let mut any_usage = false;
    let mut num_turns: u32 = 0;

    for m in messages {
        if !matches!(m.role, Role::Assistant) {
            continue;
        }
        // num_turns = assistant messages that ran a turn (tool-call or final).
        // An assistant message with zero content blocks is a stub and does
        // not count.
        let has_tool_or_text = m
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Text { .. } | ContentBlock::ToolUse { .. }));
        if has_tool_or_text {
            num_turns = num_turns.saturating_add(1);
        }
        if let Some(u) = m.usage {
            any_usage = true;
            input = input.saturating_add(u.input_tokens);
            output = output.saturating_add(u.output_tokens);
            cache_read = cache_read.saturating_add(u.cache_read_input_tokens);
            cache_create = cache_create.saturating_add(u.cache_creation_input_tokens);
        }
    }

    let (cr, cc) = if any_usage {
        (Some(cache_read), Some(cache_create))
    } else {
        (None, None)
    };
    (input, output, cr, cc, num_turns)
}

/// SHA-256 hex of the raw prompt bytes as the user passed them in.
pub fn prompt_sha256(prompt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Map a resolved model string to the provider label written in the metrics.
/// Mirrors the routing rules in `build_provider` so the two stay in sync.
pub fn provider_label(model: &str) -> &'static str {
    let m = model.trim();
    if m.starts_with("openai/")
        || m.starts_with("gpt-")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
    {
        "openai"
    } else {
        "anthropic"
    }
}

/// Atomically write the metrics JSON to `path` via `<path>.tmp` + rename.
pub fn write_atomic(path: &Path, metrics: &AskMetrics) -> anyhow::Result<()> {
    let mut tmp: PathBuf = path.to_path_buf();
    let file_name = path
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    let mut tmp_name = file_name;
    tmp_name.push(".tmp");
    tmp.set_file_name(tmp_name);

    let json = serde_json::to_string(metrics)?;
    std::fs::write(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use harness_proto::Usage;

    #[test]
    fn prompt_sha256_matches_known_vector() {
        // sha256("abc") per FIPS 180-2 test vector.
        assert_eq!(
            prompt_sha256("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn provider_label_routes_by_model_prefix() {
        assert_eq!(provider_label("claude-opus-4-7"), "anthropic");
        assert_eq!(provider_label("gpt-4o"), "openai");
        assert_eq!(provider_label("openai/gpt-4o"), "openai");
        assert_eq!(provider_label("o3-mini"), "openai");
    }

    #[test]
    fn summarize_sums_usage_and_counts_turns() {
        let mut a1 = Message::assistant_empty();
        a1.content.push(ContentBlock::Text {
            text: "hi".into(),
            cache_control: None,
        });
        a1.usage = Some(Usage {
            input_tokens: 10,
            output_tokens: 2,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 5,
        });
        let mut a2 = Message::assistant_empty();
        a2.content.push(ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Read".into(),
            input: serde_json::json!({}),
            cache_control: None,
        });
        a2.usage = Some(Usage {
            input_tokens: 3,
            output_tokens: 1,
            cache_creation_input_tokens: 7,
            cache_read_input_tokens: 0,
        });
        let msgs = vec![Message::user("q"), a1, a2];
        let (i, o, cr, cc, n) = summarize(&msgs);
        assert_eq!(i, 13);
        assert_eq!(o, 3);
        assert_eq!(cr, Some(5));
        assert_eq!(cc, Some(7));
        assert_eq!(n, 2);
    }

    #[test]
    fn summarize_no_usage_yields_null_cache_fields() {
        let mut a1 = Message::assistant_empty();
        a1.content.push(ContentBlock::Text {
            text: "hi".into(),
            cache_control: None,
        });
        // a1.usage stays None.
        let msgs = vec![a1];
        let (_, _, cr, cc, n) = summarize(&msgs);
        assert_eq!(cr, None);
        assert_eq!(cc, None);
        assert_eq!(n, 1);
    }

    #[test]
    fn write_atomic_renames_tmp_to_final() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("m.json");
        let m = AskMetrics {
            schema_version: 1,
            tool: "harness",
            model: "claude-opus-4-7".into(),
            provider: "anthropic",
            wall_ms: 42,
            api_ms: None,
            exit_code: 0,
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: None,
            cache_creation_tokens: None,
            num_turns: 1,
            prompt_sha256: "deadbeef".into(),
            session_id: "sess_1".into(),
        };
        write_atomic(&path, &m).unwrap();
        assert!(path.exists());
        // tmp sibling should have been renamed away.
        assert!(!tmp.path().join("m.json.tmp").exists());
        let s = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["tool"], "harness");
        assert_eq!(v["provider"], "anthropic");
    }
}
