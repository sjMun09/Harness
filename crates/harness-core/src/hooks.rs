//! Hook dispatcher. PLAN §5.5 / §5.10 / §8.2.
//!
//! MVP hooks: `SessionStart`, `PreToolUse`, `PostToolUse`, `Stop`. Per-hook
//! `timeout_ms` + `on_timeout: allow|deny`. `additionalContext` is fenced with
//! `<untrusted_hook>` before joining the system prompt.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    PreToolUse,
    PostToolUse,
    Stop,
}

impl HookEvent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::PreToolUse => "pre_tool_use",
            Self::PostToolUse => "post_tool_use",
            Self::Stop => "stop",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "session_start" => Some(Self::SessionStart),
            "pre_tool_use" => Some(Self::PreToolUse),
            "post_tool_use" => Some(Self::PostToolUse),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookAction {
    Allow,
    Block,
    Rewrite,
}

impl Default for HookAction {
    fn default() -> Self {
        Self::Allow
    }
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookOutput {
    #[serde(default)]
    pub action: HookAction,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub additional_context: Option<String>,
    #[serde(default)]
    pub rewrite: Option<Value>,
}

impl HookOutput {
    pub fn allow() -> Self {
        Self {
            action: HookAction::Allow,
            ..Default::default()
        }
    }
    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            action: HookAction::Block,
            reason: Some(reason.into()),
            ..Default::default()
        }
    }
}

/// Cheap-clone facade over the hook registry.
#[derive(Clone, Default)]
pub struct HookDispatcher {
    inner: Arc<HookDispatcherInner>,
}

#[derive(Default)]
struct HookDispatcherInner {
    hooks: BTreeMap<HookEvent, Vec<HookConfig>>,
}

impl std::fmt::Debug for HookDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let counts: BTreeMap<&str, usize> = self
            .inner
            .hooks
            .iter()
            .map(|(k, v)| (k.as_str(), v.len()))
            .collect();
        f.debug_struct("HookDispatcher")
            .field("hooks", &counts)
            .finish()
    }
}

impl HookDispatcher {
    pub fn new(hooks: BTreeMap<HookEvent, Vec<HookConfig>>) -> Self {
        Self {
            inner: Arc::new(HookDispatcherInner { hooks }),
        }
    }

    /// Construct from the string-keyed map stored in `Settings.hooks`.
    pub fn from_settings_map(map: &BTreeMap<String, Vec<HookConfig>>) -> Self {
        let mut out: BTreeMap<HookEvent, Vec<HookConfig>> = BTreeMap::new();
        for (k, v) in map {
            if let Some(ev) = HookEvent::parse(k) {
                out.entry(ev).or_default().extend(v.iter().cloned());
            }
        }
        Self::new(out)
    }

    pub fn has(&self, event: HookEvent) -> bool {
        self.inner.hooks.get(&event).is_some_and(|v| !v.is_empty())
    }

    /// Run every configured hook for `event`; return the first non-Allow result,
    /// else the merged Allow with concatenated `additional_context`.
    pub async fn dispatch(&self, event: HookEvent, payload: Value) -> HookOutput {
        let Some(configs) = self.inner.hooks.get(&event) else {
            return HookOutput::allow();
        };
        let mut merged = HookOutput::allow();
        for cfg in configs {
            let out = run_one(cfg, &payload).await;
            match out.action {
                HookAction::Block => return out,
                HookAction::Rewrite => return out,
                HookAction::Allow => {
                    if let Some(ctx) = out.additional_context {
                        let slot = merged.additional_context.get_or_insert_with(String::new);
                        if !slot.is_empty() {
                            slot.push('\n');
                        }
                        slot.push_str(&ctx);
                    }
                }
            }
        }
        merged
    }
}

async fn run_one(cfg: &HookConfig, payload: &Value) -> HookOutput {
    let dur = Duration::from_millis(cfg.timeout_ms.max(50));
    let fut = async {
        let mut child = match Command::new("/bin/sh")
            .arg("-c")
            .arg(&cfg.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => return HookOutput::allow(),
        };
        if let Some(mut stdin) = child.stdin.take() {
            let payload_bytes = serde_json::to_vec(payload).unwrap_or_default();
            let _ = stdin.write_all(&payload_bytes).await;
            drop(stdin);
        }
        let mut stdout = Vec::new();
        if let Some(mut so) = child.stdout.take() {
            let _ = so.read_to_end(&mut stdout).await;
        }
        let _ = child.wait().await;
        parse_hook_output(&stdout)
    };
    match timeout(dur, fut).await {
        Ok(out) => out,
        Err(_) => match cfg.on_timeout {
            OnTimeout::Allow => HookOutput::allow(),
            OnTimeout::Deny => HookOutput::block("hook timeout"),
        },
    }
}

fn parse_hook_output(bytes: &[u8]) -> HookOutput {
    if bytes.is_empty() {
        return HookOutput::allow();
    }
    serde_json::from_slice::<HookOutput>(bytes).unwrap_or_else(|_| HookOutput::allow())
}

/// Wrap untrusted hook-supplied text per §8.2.
#[must_use]
pub fn fence_untrusted(s: &str) -> String {
    format!("<untrusted_hook>\n{s}\n</untrusted_hook>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_roundtrip() {
        for ev in [
            HookEvent::SessionStart,
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::Stop,
        ] {
            assert_eq!(HookEvent::parse(ev.as_str()), Some(ev));
        }
    }

    #[test]
    fn empty_dispatcher_allows() {
        let d = HookDispatcher::default();
        assert!(!d.has(HookEvent::PreToolUse));
    }

    #[tokio::test]
    async fn missing_hook_returns_allow() {
        let d = HookDispatcher::default();
        let out = d.dispatch(HookEvent::PreToolUse, json!({})).await;
        assert_eq!(out.action, HookAction::Allow);
    }

    #[tokio::test]
    async fn block_hook_short_circuits() {
        let mut map = BTreeMap::new();
        map.insert(
            HookEvent::PreToolUse,
            vec![HookConfig {
                event: HookEvent::PreToolUse,
                command: r#"printf '{"action":"block","reason":"nope"}'"#.into(),
                timeout_ms: 3_000,
                on_timeout: OnTimeout::Deny,
            }],
        );
        let d = HookDispatcher::new(map);
        let out = d.dispatch(HookEvent::PreToolUse, json!({})).await;
        assert_eq!(out.action, HookAction::Block);
        assert_eq!(out.reason.as_deref(), Some("nope"));
    }

    #[tokio::test]
    async fn timeout_honours_on_timeout_deny() {
        let mut map = BTreeMap::new();
        map.insert(
            HookEvent::PreToolUse,
            vec![HookConfig {
                event: HookEvent::PreToolUse,
                command: "sleep 2".into(),
                timeout_ms: 100,
                on_timeout: OnTimeout::Deny,
            }],
        );
        let d = HookDispatcher::new(map);
        let out = d.dispatch(HookEvent::PreToolUse, json!({})).await;
        assert_eq!(out.action, HookAction::Block);
    }
}
