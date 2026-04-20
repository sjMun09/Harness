//! Harness permission dispatcher (`allow` / `ask` / `deny`).
//!
//! Rule precedence: `deny` > `allow` > `ask`. Grammar per PLAN §5.8.
//! MVP is a skeleton; real matching lands with iter 1 `Tool::call` wiring.

#![forbid(unsafe_code)]

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 3-valued permission outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

/// One permission rule, parsed from `settings.json` permissions arrays.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// Tool name (`Read`, `Bash`, ...).
    pub tool: String,
    /// Tool-specific argument matcher (glob or exact). `None` = any args.
    #[serde(default)]
    pub args: Option<String>,
}

/// Opaque snapshot of the session permission set — cloned into every `ToolCtx`.
///
/// Wraps an Arc so `Clone` is O(1) and the snapshot remains immutable for the
/// life of a turn (settings hot-reload is iter 2).
#[derive(Clone, Default)]
pub struct PermissionSnapshot {
    inner: Arc<PermissionInner>,
}

#[derive(Default)]
struct PermissionInner {
    deny: Vec<Rule>,
    allow: Vec<Rule>,
    ask: Vec<Rule>,
}

impl std::fmt::Debug for PermissionSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionSnapshot")
            .field("deny", &self.inner.deny.len())
            .field("allow", &self.inner.allow.len())
            .field("ask", &self.inner.ask.len())
            .finish()
    }
}

impl PermissionSnapshot {
    pub fn new(deny: Vec<Rule>, allow: Vec<Rule>, ask: Vec<Rule>) -> Self {
        Self {
            inner: Arc::new(PermissionInner { deny, allow, ask }),
        }
    }

    /// Evaluate a tool call against the rule set. deny > allow > ask.
    pub fn evaluate(&self, _tool: &str, _args: &serde_json::Value) -> Decision {
        // Iter 1 body: glob-match against each bucket in precedence order.
        let _ = &self.inner;
        Decision::Ask
    }
}

#[derive(Debug, Error)]
pub enum PermError {
    #[error("invalid rule: {0}")]
    InvalidRule(String),
}
