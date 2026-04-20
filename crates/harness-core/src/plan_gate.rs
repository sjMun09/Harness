//! PreEdit plan-gate. PLAN §3.2 (필수).
//!
//! Forces the model to think before mutating risky files. The first attempt to
//! Edit/Write a path matching a `plan_gate.patterns` glob returns an
//! instructional block; the second attempt against the same path in the same
//! session passes through. This converts a single-shot edit into the
//! plan → review → approve → build cycle that justifies Harness existing
//! separately from a vanilla agent.
//!
//! Why a built-in (not just a hook): the gate ships as part of the kernel so
//! every Harness session has the safety net by default. Per-project policy
//! still overrides via `settings.harness.plan_gate`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde_json::Value;

use crate::config::PlanGate;
use crate::memory::MemoryDoc;

/// Outcome of a plan-gate check on a single tool invocation.
#[derive(Debug)]
pub enum GateOutcome {
    /// Tool call may proceed.
    Allow,
    /// Tool call is blocked. `reason` is the message returned to the model
    /// in `tool_result.content` so it can read it next turn and emit a plan.
    Block { reason: String },
}

/// Per-session state. Cheap to clone (Arc<Mutex<…>>) so it can sit in the
/// engine and be threaded through to `dispatch_one` without lifetime pain.
#[derive(Clone, Default)]
pub struct PlanGateState {
    inner: Arc<PlanGateInner>,
}

#[derive(Default)]
struct PlanGateInner {
    matcher: Option<CompiledMatcher>,
    memory: Option<MemoryDoc>,
    seen: Mutex<HashSet<PathBuf>>,
}

struct CompiledMatcher {
    set: GlobSet,
    tools: Vec<String>,
}

impl std::fmt::Debug for PlanGateState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanGateState")
            .field("active", &self.inner.matcher.is_some())
            .finish()
    }
}

impl PlanGateState {
    /// Build from settings. If disabled or all patterns invalid, returns a
    /// no-op state that allows everything.
    #[must_use]
    pub fn from_config(cfg: &PlanGate) -> Self {
        Self::from_config_with_memory(cfg, None)
    }

    /// Build from settings + an optional HARNESS.md doc whose matching
    /// sections will be appended to every block message — giving the model
    /// concrete conventions to follow when it writes its plan.
    #[must_use]
    pub fn from_config_with_memory(cfg: &PlanGate, memory: Option<MemoryDoc>) -> Self {
        if !cfg.enabled || cfg.patterns.is_empty() || cfg.tools.is_empty() {
            return Self::default();
        }
        let mut b = GlobSetBuilder::new();
        let mut any = false;
        for p in &cfg.patterns {
            match Glob::new(p) {
                Ok(g) => {
                    b.add(g);
                    any = true;
                }
                Err(e) => tracing::warn!(pattern = %p, error = %e, "plan_gate: invalid glob"),
            }
        }
        if !any {
            return Self::default();
        }
        let Ok(set) = b.build() else {
            return Self::default();
        };
        Self {
            inner: Arc::new(PlanGateInner {
                matcher: Some(CompiledMatcher {
                    set,
                    tools: cfg.tools.clone(),
                }),
                memory: memory.filter(|m| !m.is_empty()),
                seen: Mutex::new(HashSet::new()),
            }),
        }
    }

    /// Decide whether `tool_name(input)` may proceed.
    ///
    /// `Block` on first match, `Allow` on subsequent matches for the same
    /// (normalized) path. Tools/inputs outside the configured set always
    /// `Allow`.
    #[must_use]
    pub fn evaluate(&self, tool_name: &str, input: &Value) -> GateOutcome {
        let Some(matcher) = self.inner.matcher.as_ref() else {
            return GateOutcome::Allow;
        };
        if !matcher.tools.iter().any(|t| t == tool_name) {
            return GateOutcome::Allow;
        }
        let Some(path_str) = extract_path(input) else {
            return GateOutcome::Allow;
        };
        if !matcher.set.is_match(&path_str) {
            return GateOutcome::Allow;
        }

        let key = PathBuf::from(&path_str);
        let mut seen = match self.inner.seen.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if seen.insert(key) {
            let mut reason = block_message(tool_name, &path_str);
            if let Some(mem) = self.inner.memory.as_ref() {
                if let Some(extra) = mem.render_for_path(&path_str) {
                    reason.push_str("\n\n");
                    reason.push_str(&extra);
                }
            }
            GateOutcome::Block { reason }
        } else {
            GateOutcome::Allow
        }
    }
}

impl PlanGateState {
    /// PLAN §4.1 verify-stage signal. After a tool call succeeds, return an
    /// optional advisory string to append to the `tool_result.content` so the
    /// model sees: "edits applied — verify with these commands".
    ///
    /// Activates only when the same matcher that runs the gate (tool ∈ tools,
    /// path ∈ patterns) hits, *and* the loaded HARNESS.md has at least one
    /// applicable section whose heading mentions Test/Verify and whose body
    /// contains shell-prompt lines (`$ ...`) or fenced commands. Returns
    /// `None` when there's nothing useful to say — never nags.
    #[must_use]
    pub fn advise_after(&self, tool_name: &str, input: &Value) -> Option<String> {
        let matcher = self.inner.matcher.as_ref()?;
        if !matcher.tools.iter().any(|t| t == tool_name) {
            return None;
        }
        let path_str = extract_path(input)?;
        if !matcher.set.is_match(&path_str) {
            return None;
        }
        let mem = self.inner.memory.as_ref()?;

        let mut cmds: Vec<String> = Vec::new();
        for section in mem.lookup(&path_str) {
            let heading_low = section.heading.to_ascii_lowercase();
            if !heading_low.contains("test") && !heading_low.contains("verify") {
                continue;
            }
            cmds.extend(extract_shell_commands(&section.body));
        }
        if cmds.is_empty() {
            return None;
        }
        let mut out = String::from(
            "\n\n---\nVERIFY: edits applied to a gated path. Before reporting done, run the \
project's verification commands (from HARNESS.md ## Test Commands):",
        );
        for c in &cmds {
            out.push_str("\n  $ ");
            out.push_str(c);
        }
        Some(out)
    }
}

/// Pull the target file path from a tool input. Both Edit and Write use
/// `file_path`; falls back to `path` for forward-compat with future tools.
fn extract_path(input: &Value) -> Option<String> {
    input
        .get("file_path")
        .or_else(|| input.get("path"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// Extract shell commands from a HARNESS.md section body.
///
/// Recognized: lines starting with `$ ` (shell prompt convention) and lines
/// inside ```` ``` ```` fenced blocks. Everything else is treated as prose
/// and ignored, so the doc author can write surrounding explanation freely.
fn extract_shell_commands(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for raw in body.lines() {
        let trimmed = raw.trim();
        if let Some(rest) = trimmed.strip_prefix("```") {
            in_fence = !in_fence;
            // Allow ```bash on the fence line; the rest is just a language tag.
            let _ = rest;
            continue;
        }
        if in_fence {
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
            continue;
        }
        if let Some(cmd) = trimmed.strip_prefix("$ ") {
            if !cmd.is_empty() {
                out.push(cmd.to_string());
            }
        }
    }
    out
}

fn block_message(tool: &str, path: &str) -> String {
    format!(
        "PLAN-GATE: `{tool}` on `{path}` is gated. This file matches a risky pattern \
         (XML/Freemarker/SQL/migration). Before retrying:\n\
         1. Write a short plan as plain text in your next message:\n   \
            - **Files** — what you'll touch\n   \
            - **Changes** — what edits you'll make and the conventions you're following\n   \
            - **Why** — the user need or constraint driving the change\n   \
            - **Risks** — what could regress and how you'd verify\n\
         2. Then call `{tool}` again on the same path. The second attempt is \
         allowed; the gate's job was to make you think once before mutating."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(patterns: &[&str], tools: &[&str]) -> PlanGate {
        PlanGate {
            enabled: true,
            patterns: patterns.iter().map(|s| s.to_string()).collect(),
            tools: tools.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn disabled_allows_everything() {
        let state = PlanGateState::from_config(&PlanGate {
            enabled: false,
            ..Default::default()
        });
        let out = state.evaluate("Edit", &json!({"file_path": "/a/b.xml"}));
        assert!(matches!(out, GateOutcome::Allow));
    }

    #[test]
    fn first_attempt_blocked_second_allowed() {
        let state = PlanGateState::from_config(&cfg(&["**/*.xml"], &["Edit"]));
        let input = json!({"file_path": "src/foo.xml"});
        match state.evaluate("Edit", &input) {
            GateOutcome::Block { reason } => assert!(reason.contains("PLAN-GATE")),
            _ => panic!("expected first call blocked"),
        }
        assert!(matches!(
            state.evaluate("Edit", &input),
            GateOutcome::Allow
        ));
    }

    #[test]
    fn untracked_tool_passes() {
        let state = PlanGateState::from_config(&cfg(&["**/*.xml"], &["Edit"]));
        assert!(matches!(
            state.evaluate("Read", &json!({"file_path": "src/foo.xml"})),
            GateOutcome::Allow
        ));
    }

    #[test]
    fn nonmatching_path_passes() {
        let state = PlanGateState::from_config(&cfg(&["**/*.xml"], &["Edit"]));
        assert!(matches!(
            state.evaluate("Edit", &json!({"file_path": "src/foo.rs"})),
            GateOutcome::Allow
        ));
    }

    #[test]
    fn migrations_glob_matches_subdirs() {
        let state = PlanGateState::from_config(&cfg(&["**/migrations/**"], &["Write"]));
        match state.evaluate("Write", &json!({"file_path": "db/migrations/0001.sql"})) {
            GateOutcome::Block { .. } => {}
            _ => panic!("expected block on migrations path"),
        }
    }

    #[test]
    fn missing_path_input_passes() {
        let state = PlanGateState::from_config(&cfg(&["**/*.xml"], &["Edit"]));
        assert!(matches!(
            state.evaluate("Edit", &json!({"other": "field"})),
            GateOutcome::Allow
        ));
    }

    #[test]
    fn absolute_path_matches_double_star_glob() {
        let state = PlanGateState::from_config(&cfg(&["**/*.xml"], &["Edit"]));
        match state.evaluate("Edit", &json!({"file_path": "/tmp/plan-gate-test/sample.xml"})) {
            GateOutcome::Block { .. } => {}
            other => panic!("expected block on absolute xml path, got {other:?}"),
        }
    }

    #[test]
    fn block_message_includes_matching_memory_sections() {
        let mem = MemoryDoc::parse(
            "## Conventions [pattern: \"**/*.xml\"]\n\
             ← ! Canonical: lowercase tags\n\
             ← ✗ Anti:      mixed-case tags\n",
        );
        let state =
            PlanGateState::from_config_with_memory(&cfg(&["**/*.xml"], &["Edit"]), Some(mem));
        let GateOutcome::Block { reason } =
            state.evaluate("Edit", &json!({"file_path": "src/foo.xml"}))
        else {
            panic!("expected block");
        };
        assert!(reason.contains("Relevant HARNESS.md sections"));
        assert!(reason.contains("CANONICAL: ← ! Canonical: lowercase tags"));
        assert!(reason.contains("ANTI:      ← ✗ Anti:      mixed-case tags"));
    }

    #[test]
    fn block_message_skips_memory_section_when_empty_doc() {
        let state = PlanGateState::from_config_with_memory(
            &cfg(&["**/*.xml"], &["Edit"]),
            Some(MemoryDoc::empty()),
        );
        let GateOutcome::Block { reason } =
            state.evaluate("Edit", &json!({"file_path": "src/foo.xml"}))
        else {
            panic!("expected block");
        };
        assert!(!reason.contains("HARNESS.md"));
    }

    #[test]
    fn advise_after_emits_test_commands_for_gated_path() {
        let mem = MemoryDoc::parse(
            "## Test Commands [pattern: \"**/*.xml\"]\n\
             Run the XML schema check.\n\
             $ cargo test --workspace\n\
             $ ./scripts/lint-xml.sh\n",
        );
        let state =
            PlanGateState::from_config_with_memory(&cfg(&["**/*.xml"], &["Edit"]), Some(mem));
        let advice = state
            .advise_after("Edit", &json!({"file_path": "src/foo.xml"}))
            .expect("expected advice");
        assert!(advice.contains("$ cargo test --workspace"));
        assert!(advice.contains("$ ./scripts/lint-xml.sh"));
        assert!(advice.contains("VERIFY"));
    }

    #[test]
    fn advise_after_handles_fenced_code_block() {
        let mem = MemoryDoc::parse(
            "## Verify [pattern: \"**/*.sql\"]\n\
             ```bash\n\
             psql -f schema.sql\n\
             pytest tests/sql\n\
             ```\n",
        );
        let state =
            PlanGateState::from_config_with_memory(&cfg(&["**/*.sql"], &["Write"]), Some(mem));
        let advice = state
            .advise_after("Write", &json!({"file_path": "db/schema.sql"}))
            .unwrap();
        assert!(advice.contains("psql -f schema.sql"));
        assert!(advice.contains("pytest tests/sql"));
    }

    #[test]
    fn advise_after_returns_none_for_non_gated_path() {
        let mem = MemoryDoc::parse(
            "## Test Commands\n$ cargo test\n",
        );
        let state =
            PlanGateState::from_config_with_memory(&cfg(&["**/*.xml"], &["Edit"]), Some(mem));
        // Path doesn't match XML pattern.
        assert!(state
            .advise_after("Edit", &json!({"file_path": "src/foo.rs"}))
            .is_none());
    }

    #[test]
    fn advise_after_returns_none_when_no_test_section_matches() {
        let mem = MemoryDoc::parse(
            "## Conventions\nplain prose only\n",
        );
        let state =
            PlanGateState::from_config_with_memory(&cfg(&["**/*.xml"], &["Edit"]), Some(mem));
        assert!(state
            .advise_after("Edit", &json!({"file_path": "src/foo.xml"}))
            .is_none());
    }

    #[test]
    fn advise_after_returns_none_without_loaded_memory() {
        let state = PlanGateState::from_config(&cfg(&["**/*.xml"], &["Edit"]));
        assert!(state
            .advise_after("Edit", &json!({"file_path": "src/foo.xml"}))
            .is_none());
    }

    #[test]
    fn separate_paths_each_blocked_once() {
        let state = PlanGateState::from_config(&cfg(&["**/*.xml"], &["Edit"]));
        assert!(matches!(
            state.evaluate("Edit", &json!({"file_path": "a.xml"})),
            GateOutcome::Block { .. }
        ));
        assert!(matches!(
            state.evaluate("Edit", &json!({"file_path": "b.xml"})),
            GateOutcome::Block { .. }
        ));
        assert!(matches!(
            state.evaluate("Edit", &json!({"file_path": "a.xml"})),
            GateOutcome::Allow
        ));
        assert!(matches!(
            state.evaluate("Edit", &json!({"file_path": "b.xml"})),
            GateOutcome::Allow
        ));
    }
}
