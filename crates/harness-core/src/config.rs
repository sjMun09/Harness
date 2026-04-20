//! Settings loader. PLAN §5.7.
//!
//! Precedence (later wins on scalar keys, vectors concat):
//!     defaults
//!   < `~/.config/harness/settings.json`   (user)
//!   < `<cwd>/.harness/settings.json`      (project)
//!   < env `HARNESS_*`                     (env overlay)
//!   < CLI flags                           (applied post-load by caller)
//!
//! Security (§8.2): plaintext `api_key` in any settings file is rejected
//! before merge — there is no "approved" location for credentials.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::hooks::HookConfig;

pub const SETTINGS_VERSION: u32 = 1;
pub const DEFAULT_MODEL: &str = "claude-opus-4-7";
pub const DEFAULT_ENV_ALLOW: &[&str] = &["PATH", "HOME", "LANG", "TERM", "USER"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_version")]
    pub v: u32,
    #[serde(default = "default_model_string")]
    pub model: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_env_allow")]
    pub env_allow: Vec<String>,
    #[serde(default)]
    pub permissions: Permissions,
    /// Map keyed by `HookEvent` discriminator (`"pre_tool_use"`, etc).
    #[serde(default)]
    pub hooks: BTreeMap<String, Vec<HookConfig>>,
    #[serde(default)]
    pub harness: HarnessExt,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            v: SETTINGS_VERSION,
            model: DEFAULT_MODEL.to_string(),
            provider: default_provider(),
            env_allow: default_env_allow(),
            permissions: Permissions::default(),
            hooks: BTreeMap::new(),
            harness: HarnessExt::default(),
        }
    }
}

fn default_version() -> u32 {
    SETTINGS_VERSION
}
fn default_model_string() -> String {
    DEFAULT_MODEL.to_string()
}
fn default_provider() -> String {
    "anthropic".into()
}
fn default_env_allow() -> Vec<String> {
    DEFAULT_ENV_ALLOW.iter().map(|s| (*s).to_string()).collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Permissions {
    #[serde(default)]
    pub deny: Vec<harness_perm::Rule>,
    #[serde(default)]
    pub allow: Vec<harness_perm::Rule>,
    #[serde(default)]
    pub ask: Vec<harness_perm::Rule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessExt {
    #[serde(default = "default_memory_paths")]
    pub memory_paths: Vec<String>,
    #[serde(default)]
    pub subagent_bash_allowed: bool,
    #[serde(default)]
    pub plan_gate: PlanGate,
}

impl Default for HarnessExt {
    fn default() -> Self {
        Self {
            memory_paths: default_memory_paths(),
            subagent_bash_allowed: false,
            plan_gate: PlanGate::default(),
        }
    }
}

fn default_memory_paths() -> Vec<String> {
    vec!["HARNESS.md".into(), ".harness/HARNESS.md".into()]
}

/// PreEdit plan-gate. PLAN §3.2.
///
/// Forces the model to emit a written plan before its first Edit/Write to a
/// risky path (XML/Freemarker/SQL/migrations by default). The first attempt is
/// blocked with an instructional message; the second attempt to the same path
/// in the same session passes through.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanGate {
    #[serde(default = "default_plan_gate_enabled")]
    pub enabled: bool,
    #[serde(default = "default_plan_gate_patterns")]
    pub patterns: Vec<String>,
    #[serde(default = "default_plan_gate_tools")]
    pub tools: Vec<String>,
}

impl Default for PlanGate {
    fn default() -> Self {
        Self {
            enabled: default_plan_gate_enabled(),
            patterns: default_plan_gate_patterns(),
            tools: default_plan_gate_tools(),
        }
    }
}

fn default_plan_gate_enabled() -> bool {
    true
}

fn default_plan_gate_patterns() -> Vec<String> {
    vec![
        "**/*.xml".into(),
        "**/*.ftl".into(),
        "**/*.sql".into(),
        "**/migrations/**".into(),
    ]
}

fn default_plan_gate_tools() -> Vec<String> {
    vec!["Edit".into(), "Write".into()]
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("settings.json at {0} contains plaintext api_key; put secrets in env only (§8.2)")]
    PlaintextSecret(PathBuf),
    #[error("unknown version at {path}: {found}")]
    UnknownVersion { path: PathBuf, found: u32 },
}

/// Candidate settings paths — user first, then project. Precedence applies
/// later items on top.
#[must_use]
pub fn candidate_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(user) = user_settings_path() {
        out.push(user);
    }
    out.push(PathBuf::from(".harness").join("settings.json"));
    out
}

#[must_use]
pub fn user_settings_path() -> Option<PathBuf> {
    use etcetera::BaseStrategy;
    let base = etcetera::choose_base_strategy().ok()?;
    Some(base.config_dir().join("harness").join("settings.json"))
}

/// Load + merge all candidates → apply env overlay → return merged settings.
pub fn load() -> Result<Settings, ConfigError> {
    let mut out = Settings::default();
    for path in candidate_paths() {
        let layered = match load_file(&path) {
            Ok(s) => s,
            Err(ConfigError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                continue;
            }
            Err(e) => return Err(e),
        };
        out = merge(out, layered);
    }
    apply_env_overlay(&mut out);
    Ok(out)
}

/// Load one file with plaintext-secret + version guards.
pub fn load_file(path: &Path) -> Result<Settings, ConfigError> {
    let bytes = std::fs::read(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if contains_plaintext_secret(&bytes) {
        return Err(ConfigError::PlaintextSecret(path.to_path_buf()));
    }
    let parsed: Settings = serde_json::from_slice(&bytes).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    if parsed.v != SETTINGS_VERSION {
        return Err(ConfigError::UnknownVersion {
            path: path.to_path_buf(),
            found: parsed.v,
        });
    }
    Ok(parsed)
}

/// Rudimentary but conservative plaintext-secret detector — catches the common
/// mistake of embedding `"api_key": "..."` or `"anthropic_api_key": "..."` in
/// the settings file.
fn contains_plaintext_secret(bytes: &[u8]) -> bool {
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s,
        Err(_) => return false,
    };
    // Normalize casing for the substring probe.
    let low = s.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "\"api_key\"",
        "\"apikey\"",
        "\"anthropic_api_key\"",
        "\"openai_api_key\"",
        "\"bearer_token\"",
    ];
    NEEDLES.iter().any(|n| low.contains(n))
}

/// Merge `overlay` on top of `base`: scalars overwrite, vectors concatenate,
/// hooks BTree merges per-key (overlay appends to base entries).
#[must_use]
pub fn merge(mut base: Settings, overlay: Settings) -> Settings {
    base.model = overlay.model;
    base.provider = overlay.provider;
    if !overlay.env_allow.is_empty() {
        base.env_allow = overlay.env_allow;
    }
    base.permissions.deny.extend(overlay.permissions.deny);
    base.permissions.allow.extend(overlay.permissions.allow);
    base.permissions.ask.extend(overlay.permissions.ask);
    for (k, v) in overlay.hooks {
        base.hooks.entry(k).or_default().extend(v);
    }
    if !overlay.harness.memory_paths.is_empty() {
        base.harness.memory_paths = overlay.harness.memory_paths;
    }
    base.harness.subagent_bash_allowed = overlay.harness.subagent_bash_allowed;
    base.harness.plan_gate = overlay.harness.plan_gate;
    base
}

/// Apply `HARNESS_MODEL` overlay. Extend with more env keys as needed.
fn apply_env_overlay(s: &mut Settings) {
    if let Ok(m) = std::env::var("HARNESS_MODEL") {
        if !m.trim().is_empty() {
            s.model = m;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn defaults_have_canonical_model() {
        let s = Settings::default();
        assert_eq!(s.model, DEFAULT_MODEL);
        assert_eq!(s.provider, "anthropic");
        assert_eq!(s.env_allow, DEFAULT_ENV_ALLOW);
    }

    #[test]
    fn merge_concats_permission_rules() {
        let base = Settings {
            permissions: Permissions {
                allow: vec![harness_perm::Rule::parse("Read(**)").unwrap()],
                ..Permissions::default()
            },
            ..Settings::default()
        };
        let overlay = Settings {
            permissions: Permissions {
                deny: vec![harness_perm::Rule::parse("Write(/etc/**)").unwrap()],
                ..Permissions::default()
            },
            ..Settings::default()
        };
        let m = merge(base, overlay);
        assert_eq!(m.permissions.allow.len(), 1);
        assert_eq!(m.permissions.deny.len(), 1);
    }

    #[test]
    fn plaintext_api_key_rejected() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, br#"{"v":1,"api_key":"sk-abc","model":"x"}"#).unwrap();
        let err = load_file(&p).unwrap_err();
        assert!(matches!(err, ConfigError::PlaintextSecret(_)));
    }

    #[test]
    fn version_mismatch_rejected() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("settings.json");
        std::fs::write(&p, br#"{"v":9}"#).unwrap();
        let err = load_file(&p).unwrap_err();
        assert!(matches!(err, ConfigError::UnknownVersion { found: 9, .. }));
    }

    #[test]
    fn env_overlay_overrides_model() {
        std::env::set_var("HARNESS_MODEL", "claude-test-stub");
        let mut s = Settings::default();
        apply_env_overlay(&mut s);
        assert_eq!(s.model, "claude-test-stub");
        std::env::remove_var("HARNESS_MODEL");
    }
}
