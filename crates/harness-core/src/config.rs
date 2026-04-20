//! Settings loader. PLAN §5.7.
//!
//! Precedence (later wins on key): defaults < `~/.config/harness/settings.json` <
//! `<cwd>/.harness/settings.json` < env (`HARNESS_*`) < CLI flags.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const SETTINGS_VERSION: u32 = 1;

pub const DEFAULT_MODEL: &str = "claude-opus-4-7";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_version")]
    pub v: u32,
    #[serde(default = "default_model_string")]
    pub model: String,
    #[serde(default)]
    pub env_allow: Vec<String>,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub hooks: Vec<crate::hooks::HookConfig>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            v: SETTINGS_VERSION,
            model: DEFAULT_MODEL.to_string(),
            env_allow: vec![],
            permissions: Permissions::default(),
            hooks: vec![],
        }
    }
}

fn default_version() -> u32 {
    SETTINGS_VERSION
}

fn default_model_string() -> String {
    DEFAULT_MODEL.to_string()
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

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("settings.json has plaintext secret key; put in env only")]
    PlaintextSecret,
    #[error("unknown version: {0}")]
    UnknownVersion(u32),
}

/// Resolve settings path candidates in precedence order.
pub fn candidate_paths() -> Vec<PathBuf> {
    // Iter 1 body: etcetera::BaseStrategy::config_dir() + cwd/.harness/.
    Vec::new()
}

/// Load + merge settings. Stub.
pub fn load() -> Result<Settings, ConfigError> {
    Ok(Settings::default())
}
