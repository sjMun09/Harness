//! Anthropic Messages API client.
//!
//! Wire contract: POST `/v1/messages` with `accept: text/event-stream`.
//! Security (§8.2): API key from env only, never from `settings.json`.
//! `SecretString` guarantees `Debug` redaction; the single `expose_secret()`
//! call is in the outbound request header construction.

use std::time::Duration;

use async_trait::async_trait;
use harness_core::{EventStream, Provider, ProviderError, StreamRequest};
use secrecy::{ExposeSecret, SecretString};
use url::Url;

use crate::sse;

/// Hardcoded default. Overridable via `--model` / `HARNESS_MODEL` / `settings.json.model`.
pub const DEFAULT_MODEL: &str = "claude-opus-4-7";

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: SecretString,
    model: String,
    base_url: Url,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("model", &self.model)
            .field("base_url", &self.base_url.as_str())
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl AnthropicProvider {
    /// Read API key from env, build a shared `reqwest::Client`.
    pub fn new(model: impl Into<String>) -> Result<Self, ProviderError> {
        let raw = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| ProviderError::Auth("ANTHROPIC_API_KEY not set".into()))?;
        if raw.trim().is_empty() {
            return Err(ProviderError::Auth("ANTHROPIC_API_KEY is empty".into()));
        }
        let api_key = SecretString::from(raw);

        let base_url_raw = std::env::var("HARNESS_ANTHROPIC_BASE_URL")
            .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let base_url = Url::parse(&base_url_raw)
            .map_err(|e| ProviderError::BadRequest(format!("invalid base url: {e}")))?;

        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("harness/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        Ok(Self {
            client,
            api_key,
            model: model.into(),
            base_url,
        })
    }

    pub fn with_default_model() -> Result<Self, ProviderError> {
        Self::new(DEFAULT_MODEL)
    }

    #[inline]
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    fn messages_url(&self) -> Result<Url, ProviderError> {
        self.base_url
            .join("/v1/messages")
            .map_err(|e| ProviderError::BadRequest(format!("join url: {e}")))
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn stream(&self, req: StreamRequest<'_>) -> Result<EventStream, ProviderError> {
        let _url = self.messages_url()?;
        let _body = build_request_body(&self.model, &req);

        // Touch fields so the skeleton does not drop them to dead_code.
        let _key_ref = self.api_key.expose_secret();
        let _client = &self.client;
        let _version = ANTHROPIC_VERSION;

        // Iter 1 body:
        //   let resp = self.client.post(url)
        //       .header("accept", "text/event-stream")
        //       .header("content-type", "application/json")
        //       .header("anthropic-version", ANTHROPIC_VERSION)
        //       .header("x-api-key", self.api_key.expose_secret())
        //       .json(&body).send().await?;
        //   classify status, then sse::parse(resp.bytes_stream())
        Err(ProviderError::Parse("stream() not yet implemented".into()))
    }
}

/// Build the wire-format JSON body. Pure, unit-testable against snapshots.
fn build_request_body(model: &str, _req: &StreamRequest<'_>) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [],
        "stream": true,
    })
}

// Suppress dead-code on the SSE parser entry point until wired above.
#[allow(dead_code)]
fn _sse_entrypoint<S>(s: S) -> EventStream
where
    S: futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    sse::parse(s)
}
