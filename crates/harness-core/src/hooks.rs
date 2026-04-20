//! Hook dispatcher. PLAN §5.5 / §5.10 / §8.2.
//!
//! MVP hooks: `SessionStart`, `PreToolUse`, `PostToolUse`, `Stop`. Per-hook
//! `timeout_ms` + `on_timeout: allow|deny`. `additionalContext` is fenced with
//! `<untrusted_hook>` before joining the system prompt.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    PreToolUse,
    PostToolUse,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookAction {
    Allow,
    Block,
    Rewrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnTimeout {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    pub event: HookEvent,
    pub command: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_on_timeout")]
    pub on_timeout: OnTimeout,
}

fn default_timeout_ms() -> u64 {
    5_000
}

fn default_on_timeout() -> OnTimeout {
    OnTimeout::Deny
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookOutput {
    pub action: HookAction,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub additional_context: Option<String>,
    #[serde(default)]
    pub rewrite: Option<serde_json::Value>,
}
