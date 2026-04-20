//! Anthropic Messages API client.
//!
//! Wire contract: POST `/v1/messages` with `accept: text/event-stream`.
//! Security (§8.2): API key from env only, never from `settings.json`.
//! `SecretString` guarantees `Debug` redaction; the single `expose_secret()`
//! call is in the outbound request header construction.
//!
//! Iter 2 task #21 — prompt caching: when `prompt_caching` is true (default)
//! the request builder attaches `cache_control: { "type": "ephemeral" }` to
//! the last system block and the last tool definition. The wire shape is
//! the literal JSON object Anthropic expects — we do NOT depend on the
//! `harness_proto::CacheControl` Rust enum here because `system` and `tools`
//! are not `ContentBlock` values. This keeps the engine layer untouched
//! (avoids racing the cancel-flow agent).

use std::time::Duration;

use async_trait::async_trait;
use harness_core::{EventStream, Provider, ProviderError, StreamRequest, ToolSpec};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};
use url::Url;

use crate::sse;

/// Hardcoded default. Overridable via `--model` / `HARNESS_MODEL` / `settings.json.model`.
pub const DEFAULT_MODEL: &str = "claude-opus-4-7";

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default per-request output cap. Anthropic Messages API requires `max_tokens`.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Authentication mode for `/v1/messages`.
/// - `ApiKey` → header `x-api-key: <key>` (metered / developer API).
/// - `OAuth`  → header `authorization: Bearer <token>` (Claude Code
///   subscription token reuse — §8.2).
#[derive(Debug)]
pub enum AuthMode {
    ApiKey(SecretString),
    OAuth(SecretString),
}

pub struct AnthropicProvider {
    client: reqwest::Client,
    auth: AuthMode,
    model: String,
    base_url: Url,
    /// When true (default), attach `cache_control: ephemeral` to the last
    /// system block and the last tool. Tests can disable for exact wire diffs.
    prompt_caching: bool,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mode = match self.auth {
            AuthMode::ApiKey(_) => "api-key",
            AuthMode::OAuth(_) => "oauth",
        };
        f.debug_struct("AnthropicProvider")
            .field("model", &self.model)
            .field("base_url", &self.base_url.as_str())
            .field("auth", &mode)
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
        Self::build(model, AuthMode::ApiKey(SecretString::from(raw)))
    }

    /// Build a provider that authenticates using a Claude Code OAuth token.
    /// See `oauth::load_from_claude_code_keychain`.
    pub fn with_oauth(
        model: impl Into<String>,
        token: SecretString,
    ) -> Result<Self, ProviderError> {
        Self::build(model, AuthMode::OAuth(token))
    }

    pub fn with_default_model() -> Result<Self, ProviderError> {
        Self::new(DEFAULT_MODEL)
    }

    fn build(model: impl Into<String>, auth: AuthMode) -> Result<Self, ProviderError> {
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
            auth,
            model: model.into(),
            base_url,
            prompt_caching: true,
        })
    }

    /// Toggle prompt-caching markers on outgoing requests. Default: on.
    /// Useful in tests that snapshot request bodies, or when targeting an
    /// API surface that doesn't support `cache_control`.
    #[must_use]
    pub fn with_prompt_caching(mut self, enabled: bool) -> Self {
        self.prompt_caching = enabled;
        self
    }

    #[inline]
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[inline]
    #[must_use]
    pub fn prompt_caching_enabled(&self) -> bool {
        self.prompt_caching
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
        let _body = build_request_body(&self.model, &req, self.prompt_caching);

        // Touch fields so the skeleton does not drop them to dead_code.
        let _key_ref = match &self.auth {
            AuthMode::ApiKey(k) | AuthMode::OAuth(k) => k.expose_secret(),
        };
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
        //
        // Note: `anthropic-beta: prompt-caching-2024-07-31` was required during
        // the beta. Prompt caching is now GA on `anthropic-version: 2023-06-01`,
        // so no extra beta header is sent.
        Err(ProviderError::Parse("stream() not yet implemented".into()))
    }
}

/// Build the wire-format JSON body. Pure, unit-testable against snapshots.
///
/// When `prompt_caching` is true, attaches `cache_control: { "type": "ephemeral" }`
/// to:
///   * the last block of the `system` array (single concatenated text block here),
///   * the last entry of the `tools` array.
///
/// The conversation `messages` are passed through verbatim — per-message caching
/// is opt-in at the call site by setting `ContentBlock::*::cache_control` before
/// the message reaches the provider.
fn build_request_body(
    model: &str,
    req: &StreamRequest<'_>,
    prompt_caching: bool,
) -> serde_json::Value {
    let max_tokens = if req.max_tokens == 0 {
        DEFAULT_MAX_TOKENS
    } else {
        req.max_tokens
    };

    let system = build_system_blocks(req.system, prompt_caching);
    let tools = build_tools_array(req.tools, prompt_caching);
    let messages =
        serde_json::to_value(req.messages).unwrap_or_else(|_| Value::Array(Vec::new()));

    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
    });

    let map = body.as_object_mut().expect("json! object");
    if !system.is_empty() {
        map.insert("system".into(), Value::Array(system));
    }
    if !tools.is_empty() {
        map.insert("tools".into(), Value::Array(tools));
    }
    body
}

/// Render the `system` field as Anthropic's structured form
/// (`[{"type":"text","text":"...","cache_control":{...}}]`). Returns an
/// empty Vec when the prompt is empty so the caller can omit the field.
fn build_system_blocks(system: &str, prompt_caching: bool) -> Vec<Value> {
    if system.is_empty() {
        return Vec::new();
    }
    let mut block = json!({
        "type": "text",
        "text": system,
    });
    if prompt_caching {
        block
            .as_object_mut()
            .expect("json! object")
            .insert("cache_control".into(), ephemeral_marker());
    }
    vec![block]
}

/// Render the `tools` field. Cache marker is attached to the last tool only,
/// per Anthropic's prefix-caching contract.
fn build_tools_array(tools: &[ToolSpec], prompt_caching: bool) -> Vec<Value> {
    if tools.is_empty() {
        return Vec::new();
    }
    let last = tools.len() - 1;
    tools
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut v = json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            });
            if prompt_caching && i == last {
                v.as_object_mut()
                    .expect("json! object")
                    .insert("cache_control".into(), ephemeral_marker());
            }
            v
        })
        .collect()
}

#[inline]
fn ephemeral_marker() -> Value {
    json!({ "type": "ephemeral" })
}

// Suppress dead-code on the SSE parser entry point until wired above.
#[allow(dead_code)]
fn _sse_entrypoint<S>(s: S) -> EventStream
where
    S: futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    sse::parse(s)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use harness_core::{StreamRequest, ToolSpec};
    use harness_proto::Message;

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: format!("desc {name}"),
            input_schema: json!({"type":"object","properties":{}}),
        }
    }

    fn make_req<'a>(
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
    ) -> StreamRequest<'a> {
        StreamRequest {
            model: "claude-opus-4-7",
            system,
            messages,
            tools,
            max_tokens: 1024,
        }
    }

    #[test]
    fn body_marks_last_system_block_with_ephemeral() {
        let msgs = vec![Message::user("hi")];
        let tools: Vec<ToolSpec> = Vec::new();
        let req = make_req("you are helpful", &msgs, &tools);

        let body = build_request_body("claude-opus-4-7", &req, true);

        let sys = body
            .get("system")
            .and_then(Value::as_array)
            .expect("system array");
        assert_eq!(sys.len(), 1);
        assert_eq!(
            sys.last().unwrap().get("cache_control"),
            Some(&json!({"type":"ephemeral"}))
        );
    }

    #[test]
    fn body_marks_last_tool_with_ephemeral() {
        let msgs = vec![Message::user("hi")];
        let tools = vec![tool("Read"), tool("Write"), tool("Edit")];
        let req = make_req("sys", &msgs, &tools);

        let body = build_request_body("claude-opus-4-7", &req, true);

        let arr = body
            .get("tools")
            .and_then(Value::as_array)
            .expect("tools array");
        assert_eq!(arr.len(), 3);
        // First two have NO cache_control
        assert!(arr[0].get("cache_control").is_none());
        assert!(arr[1].get("cache_control").is_none());
        // Last has it
        assert_eq!(
            arr[2].get("cache_control"),
            Some(&json!({"type":"ephemeral"}))
        );
    }

    #[test]
    fn body_omits_cache_control_when_caching_disabled() {
        let msgs = vec![Message::user("hi")];
        let tools = vec![tool("Read"), tool("Write")];
        let req = make_req("sys", &msgs, &tools);

        let body = build_request_body("claude-opus-4-7", &req, false);

        let sys = body.get("system").and_then(Value::as_array).unwrap();
        assert!(sys.last().unwrap().get("cache_control").is_none());
        let arr = body.get("tools").and_then(Value::as_array).unwrap();
        assert!(arr.iter().all(|t| t.get("cache_control").is_none()));
    }

    #[test]
    fn body_omits_system_field_when_prompt_empty() {
        let msgs = vec![Message::user("hi")];
        let tools: Vec<ToolSpec> = Vec::new();
        let req = make_req("", &msgs, &tools);

        let body = build_request_body("claude-opus-4-7", &req, true);
        assert!(body.get("system").is_none());
    }

    #[test]
    fn body_includes_max_tokens_and_stream() {
        let msgs = vec![Message::user("hi")];
        let tools: Vec<ToolSpec> = Vec::new();
        let req = make_req("sys", &msgs, &tools);

        let body = build_request_body("claude-opus-4-7", &req, true);
        assert_eq!(body.get("model"), Some(&Value::from("claude-opus-4-7")));
        assert_eq!(body.get("stream"), Some(&Value::Bool(true)));
        assert_eq!(body.get("max_tokens"), Some(&Value::from(1024)));
    }

    #[test]
    fn provider_default_has_caching_enabled() {
        // Build directly to avoid env-var dependency in this test.
        let p = AnthropicProvider {
            client: reqwest::Client::new(),
            auth: AuthMode::ApiKey(secrecy::SecretString::from(String::from("placeholder"))),
            model: "claude-opus-4-7".into(),
            base_url: Url::parse(DEFAULT_BASE_URL).unwrap(),
            prompt_caching: true,
        };
        assert!(p.prompt_caching_enabled());
        let p = p.with_prompt_caching(false);
        assert!(!p.prompt_caching_enabled());
    }
}
