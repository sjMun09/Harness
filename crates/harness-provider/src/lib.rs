//! Anthropic Messages API provider + SSE parser.
//!
//! See `PLAN.md` §2.2 (turn loop contract), §5.9 (event set), §5.12
//! (ProviderError), §8.2 (API key handling).

#![forbid(unsafe_code)]

mod anthropic;
mod egress_redact;
#[cfg(feature = "claude-code-oauth")]
pub mod oauth;
mod openai;
mod sse;

pub use anthropic::{AnthropicProvider, AuthMode, DEFAULT_MODEL};
#[cfg(feature = "claude-code-oauth")]
pub use oauth::{
    load_from_claude_code_keychain, OauthError, OauthToken, CLAUDE_CODE_SYSTEM_PREFIX,
};
pub use openai::{is_local_url, OpenAIProvider, DEFAULT_OPENAI_MODEL};

pub use harness_core::ProviderError;
