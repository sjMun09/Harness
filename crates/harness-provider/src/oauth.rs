//! Claude Code OAuth token reuse.
//!
//! On macOS Claude Code stores its OAuth token in the login keychain under the
//! generic-password service name `Claude Code-credentials`. We shell out to
//! `security find-generic-password -s … -w` to read it, then parse the JSON
//! blob for `accessToken` + `expiresAt`. No credential is ever written to disk
//! by this module.
//!
//! The returned token is intended to be used with the Anthropic Messages API
//! in OAuth mode — see `AnthropicProvider::with_oauth`.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use secrecy::SecretString;
use serde::Deserialize;

const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Claude Code identifier required by the OAuth path on `/v1/messages`.
/// The API rejects OAuth calls whose `system` prompt does not start with
/// this exact string.
pub const CLAUDE_CODE_SYSTEM_PREFIX: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

#[derive(Debug, thiserror::Error)]
pub enum OauthError {
    #[error("keychain lookup failed: {0}")]
    Keychain(String),
    #[error("keychain payload is not valid JSON: {0}")]
    Parse(String),
    #[error("claude code token expired at unix_ms={0}; run `claude` once to refresh")]
    Expired(u64),
    #[error("unsupported platform — OAuth token reuse only implemented for macOS")]
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct OauthToken {
    pub access_token: SecretString,
    pub expires_at_unix_ms: u64,
    pub subscription_type: Option<String>,
}

impl OauthToken {
    #[must_use]
    pub fn is_expired(&self) -> bool {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        now_ms >= self.expires_at_unix_ms
    }
}

/// Load the Claude Code OAuth access token from the macOS keychain.
/// Returns `OauthError::Unsupported` on non-macOS targets.
pub fn load_from_claude_code_keychain() -> Result<OauthToken, OauthError> {
    if !cfg!(target_os = "macos") {
        return Err(OauthError::Unsupported);
    }

    let output = Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .output()
        .map_err(|e| OauthError::Keychain(format!("spawn security(1): {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(OauthError::Keychain(format!(
            "security(1) exited {}: {}",
            output.status, stderr.trim()
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let token = parse_keychain_payload(stdout.trim())?;
    if token.is_expired() {
        return Err(OauthError::Expired(token.expires_at_unix_ms));
    }
    Ok(token)
}

#[derive(Deserialize)]
struct KeychainPayload {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: ClaudeAiOauth,
}

#[derive(Deserialize)]
struct ClaudeAiOauth {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expiresAt")]
    expires_at: u64,
    #[serde(rename = "subscriptionType", default)]
    subscription_type: Option<String>,
}

fn parse_keychain_payload(s: &str) -> Result<OauthToken, OauthError> {
    let payload: KeychainPayload =
        serde_json::from_str(s).map_err(|e| OauthError::Parse(e.to_string()))?;
    Ok(OauthToken {
        access_token: SecretString::from(payload.claude_ai_oauth.access_token),
        expires_at_unix_ms: payload.claude_ai_oauth.expires_at,
        subscription_type: payload.claude_ai_oauth.subscription_type,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn parses_full_payload() {
        let s = r#"{"claudeAiOauth":{
          "accessToken":"sk-ant-oat01-abc",
          "refreshToken":"sk-ant-ort01-xyz",
          "expiresAt":9999999999999,
          "scopes":["user:inference"],
          "subscriptionType":"max"
        }}"#;
        let t = parse_keychain_payload(s).unwrap();
        assert_eq!(t.access_token.expose_secret(), "sk-ant-oat01-abc");
        assert_eq!(t.expires_at_unix_ms, 9999999999999);
        assert_eq!(t.subscription_type.as_deref(), Some("max"));
        assert!(!t.is_expired());
    }

    #[test]
    fn detects_expired() {
        let s = r#"{"claudeAiOauth":{
          "accessToken":"t","refreshToken":"r","expiresAt":1,"scopes":[]
        }}"#;
        let t = parse_keychain_payload(s).unwrap();
        assert!(t.is_expired());
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_keychain_payload("not json").is_err());
    }
}
