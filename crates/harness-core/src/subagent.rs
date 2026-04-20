//! Subagent contracts. PLAN §5.4.
//!
//! A **subagent** is a depth-capped, tool-restricted, budget-capped sub-turn
//! the parent agent can spawn to delegate focused research without polluting
//! the main context window. The kernel owns the trait + shapes; the CLI
//! wires in the host implementation that reuses the existing `run_turn` infra.
//!
//! Hard constraints enforced here (the tool wrapper re-checks them):
//!   - `depth == 1` maximum. A spawned subagent cannot spawn further subagents.
//!   - Final assistant text capped at `SUBAGENT_OUTPUT_CAP` bytes. Overflow is
//!     truncated with a `[TRUNCATED N bytes …]` marker; full transcript lives
//!     at the sub-session JSONL so the parent can route the user there.
//!   - `Bash` / `Subagent` are stripped from any allowlist the parent passes,
//!     per PLAN §8.2 ("Subagent default Bash deny"). Re-enabling Bash is a
//!     future `subagent_bash_allowed: true` config knob, not MVP.
//!
//! Why a trait + host pattern instead of folding the sub-turn into the tool:
//! the tool sits in `harness-tools`, which is a sibling crate — it cannot pull
//! in `Provider`/`run_turn` without a dependency cycle. The host lives in
//! `harness-cli` where those are already on hand. The kernel's only job here
//! is defining the contract both sides agree on.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// Name the tool dispatcher knows the subagent by. Keep in sync with
/// `harness-tools::SubagentTool::name`.
pub const SUBAGENT_TOOL_NAME: &str = "Subagent";

/// Max bytes of final assistant text the parent sees. PLAN §5.4 = 2 KiB.
pub const SUBAGENT_OUTPUT_CAP: usize = 2 * 1024;

/// Hard cap on subagent nesting depth. A parent at `depth=0` may spawn a
/// child at `depth=1`; the child spawning further is rejected.
pub const SUBAGENT_MAX_DEPTH: u32 = 1;

/// Tools always stripped from a subagent's allowlist regardless of caller ask.
/// `Bash` per §8.2 default-deny, `Subagent` to enforce depth cap structurally.
pub const SUBAGENT_BANNED_TOOLS: &[&str] = &["Bash", "Subagent"];

/// Default allowlist when the caller doesn't specify one: read-only exploration.
pub const SUBAGENT_DEFAULT_ALLOWLIST: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "ImportTrace",
    "MyBatisDynamicParser",
];

/// The parent's ask passed verbatim to the host.
#[derive(Debug, Clone)]
pub struct SubagentSpec {
    pub prompt: String,
    /// Allowed tool names. `SUBAGENT_BANNED_TOOLS` are filtered out by the
    /// tool wrapper before the spec reaches the host, so a host impl can
    /// trust this list.
    pub tools_allowlist: Vec<String>,
    pub max_turns: u32,
    /// Opaque identifier of the parent session — host uses this to place the
    /// sub-transcript under `sessions/<parent>/subagents/<sub>` for audit.
    pub parent_session: String,
    /// Expected depth of the sub-run. Tool wrapper sets `parent_depth + 1`.
    pub depth: u32,
}

/// Returned by the host to the subagent tool.
#[derive(Debug, Clone, Default)]
pub struct SubagentResult {
    /// Final assistant text. NOT yet capped by `SUBAGENT_OUTPUT_CAP` — the
    /// tool wrapper does that in one place so the cap stays consistent.
    pub text: String,
    pub tool_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Session id of the sub-turn's JSONL file. The tool wrapper embeds this
    /// in the `[TRUNCATED …]` marker so the user can find the full run.
    pub sub_session_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SubagentError {
    #[error("subagent depth cap exceeded (max {SUBAGENT_MAX_DEPTH}) — nesting is not allowed")]
    DepthCap,
    #[error("no subagent host wired — this binary was built without Subagent support")]
    NoHost,
    #[error("subagent execution failed: {0}")]
    Execution(String),
}

#[async_trait]
pub trait SubagentHost: Send + Sync + std::fmt::Debug {
    /// Run a sub-turn to completion and return what the parent should see.
    /// Implementations MUST honour the cancel token promptly.
    async fn spawn(
        &self,
        spec: SubagentSpec,
        cancel: CancellationToken,
    ) -> Result<SubagentResult, SubagentError>;
}

/// Apply the kernel's allowlist sanity rules: strip banned tools, default to
/// read-only set when empty, deduplicate while preserving the caller's order.
#[must_use]
pub fn sanitize_allowlist(requested: Option<Vec<String>>) -> Vec<String> {
    let list = requested.unwrap_or_else(|| {
        SUBAGENT_DEFAULT_ALLOWLIST
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    });
    let mut seen = std::collections::HashSet::new();
    list.into_iter()
        .filter(|t| !SUBAGENT_BANNED_TOOLS.iter().any(|b| *b == t))
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

/// Apply the 2 KiB cap. Returns `(capped_text, truncated_bytes)` where
/// `truncated_bytes == 0` indicates the text fit under the cap unchanged.
#[must_use]
pub fn cap_output(text: String, cap: usize) -> (String, u64) {
    if text.len() <= cap {
        return (text, 0);
    }
    let keep = floor_char_boundary(&text, cap);
    let dropped = text.len() - keep;
    let mut out = text[..keep].to_string();
    out.push_str("\n...[TRUNCATED ");
    out.push_str(&dropped.to_string());
    out.push_str(" bytes]");
    (out, dropped as u64)
}

fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

// Arc-friendly newtype so the rest of the kernel doesn't write
// `Arc<dyn SubagentHost>` everywhere. Still accepts `None`.
pub type OptHost = Option<Arc<dyn SubagentHost>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_banned_tools() {
        let out = sanitize_allowlist(Some(vec![
            "Read".into(),
            "Bash".into(),
            "Subagent".into(),
            "Grep".into(),
        ]));
        assert_eq!(out, vec!["Read".to_string(), "Grep".to_string()]);
    }

    #[test]
    fn sanitize_deduplicates() {
        let out = sanitize_allowlist(Some(vec!["Read".into(), "Read".into(), "Grep".into()]));
        assert_eq!(out, vec!["Read".to_string(), "Grep".to_string()]);
    }

    #[test]
    fn sanitize_none_yields_default() {
        let out = sanitize_allowlist(None);
        assert!(out.contains(&"Read".to_string()));
        assert!(out.contains(&"Grep".to_string()));
        assert!(!out.contains(&"Bash".to_string()));
    }

    #[test]
    fn cap_output_short_passes_through() {
        let (t, n) = cap_output("hello".into(), 100);
        assert_eq!(t, "hello");
        assert_eq!(n, 0);
    }

    #[test]
    fn cap_output_truncates_long() {
        let s = "a".repeat(3000);
        let (t, n) = cap_output(s, 2048);
        assert!(t.starts_with("aaa"));
        assert!(t.contains("[TRUNCATED"));
        assert!(n > 0);
    }

    #[test]
    fn cap_output_respects_utf8_boundary() {
        // "한" is 3 bytes. Cap at an offset mid-char must not split.
        let s = "한".repeat(1000);
        let (t, _n) = cap_output(s, 2048);
        assert!(t.is_char_boundary(t.find("[TRUNCATED").unwrap_or(t.len())));
    }

    #[test]
    fn constants_match_plan_spec() {
        assert_eq!(SUBAGENT_OUTPUT_CAP, 2048);
        assert_eq!(SUBAGENT_MAX_DEPTH, 1);
    }
}
