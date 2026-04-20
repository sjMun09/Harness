//! Anthropic Messages API provider + SSE parser.
//!
//! See `PLAN.md` §2.2 (turn loop contract), §5.9 (event set), §5.12
//! (ProviderError), §8.2 (API key handling).

#![forbid(unsafe_code)]

mod anthropic;
mod sse;

pub use anthropic::{AnthropicProvider, DEFAULT_MODEL};

pub use harness_core::ProviderError;
