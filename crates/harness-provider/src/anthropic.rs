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

use crate::egress_redact::maybe_redact_messages;
use crate::sse;

/// Hardcoded default. Overridable via `--model` / `HARNESS_MODEL` / `settings.json.model`.
pub const DEFAULT_MODEL: &str = "claude-opus-4-7";

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Beta flag required when authenticating via a Claude Code OAuth token.
/// Without this header the Messages API rejects OAuth calls with
/// `authentication_error: OAuth authentication is currently not supported`.
#[cfg(feature = "claude-code-oauth")]
const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

/// Default per-request output cap. Anthropic Messages API requires `max_tokens`.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Authentication mode for `/v1/messages`.
/// - `ApiKey` → header `x-api-key: <key>` (metered / developer API).
/// - `OAuth`  → header `authorization: Bearer <token>` (Claude Code
///   subscription token reuse — §8.2).
#[derive(Debug)]
pub enum AuthMode {
    ApiKey(SecretString),
    #[cfg(feature = "claude-code-oauth")]
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
    /// Opt-in egress redaction. When true, `ContentBlock::ToolResult.content`
    /// and assistant `ContentBlock::Text.text` are passed through
    /// `harness_mem::redact::redact_str` before the outbound request body is
    /// constructed. See `docs/security/egress-redaction.md`. Default: off.
    redact_egress: bool,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mode = match self.auth {
            AuthMode::ApiKey(_) => "api-key",
            #[cfg(feature = "claude-code-oauth")]
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
    #[cfg(feature = "claude-code-oauth")]
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
            redact_egress: false,
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

    /// Enable egress-side secret redaction. When on, tool_result bodies and
    /// assistant text blocks are scrubbed through the shared
    /// `harness_mem::redact` patterns before the outbound HTTP body is
    /// constructed. See `docs/security/egress-redaction.md`. Default: off.
    #[must_use]
    pub fn with_redact_egress(mut self, enabled: bool) -> Self {
        self.redact_egress = enabled;
        self
    }

    #[inline]
    #[must_use]
    pub fn redact_egress_enabled(&self) -> bool {
        self.redact_egress
    }

    /// Override the base URL used for `/v1/messages`. Primarily for end-to-end
    /// tests that point the provider at a local fake server; callers in prod
    /// should leave this alone and let the default (`https://api.anthropic.com`)
    /// or `HARNESS_ANTHROPIC_BASE_URL` drive it.
    #[must_use]
    pub fn with_base_url(mut self, url: Url) -> Self {
        self.base_url = url;
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
        let url = self.messages_url()?;
        #[cfg(feature = "claude-code-oauth")]
        let oauth_mode = matches!(self.auth, AuthMode::OAuth(_));
        #[cfg(not(feature = "claude-code-oauth"))]
        let oauth_mode = false;
        // Apply opt-in egress redaction BEFORE wire-format construction so
        // scrubbed strings are what gets serialized.
        let scrubbed = maybe_redact_messages(req.messages, self.redact_egress);
        let req_eff = StreamRequest {
            system: req.system,
            messages: &scrubbed,
            tools: req.tools,
            max_tokens: req.max_tokens,
        };
        let body = build_request_body(&self.model, &req_eff, self.prompt_caching, oauth_mode);

        let mut builder = self
            .client
            .post(url)
            .header("accept", "text/event-stream")
            .header("content-type", "application/json")
            .header("anthropic-version", ANTHROPIC_VERSION);
        builder = match &self.auth {
            AuthMode::ApiKey(k) => builder.header("x-api-key", k.expose_secret()),
            #[cfg(feature = "claude-code-oauth")]
            AuthMode::OAuth(k) => builder
                .header("anthropic-beta", OAUTH_BETA_HEADER)
                .bearer_auth(k.expose_secret()),
        };

        let resp = builder
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let retry_after = retry_after_from_headers(resp.headers());
            let body_text = resp.text().await.unwrap_or_default();
            return Err(classify_http_error(
                status.as_u16(),
                &body_text,
                retry_after,
            ));
        }

        Ok(sse::parse(resp.bytes_stream()))
    }
}

fn retry_after_from_headers(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = h.get("retry-after")?.to_str().ok()?;
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
}

pub(crate) fn classify_http_error(
    status: u16,
    body: &str,
    retry_after: Option<Duration>,
) -> ProviderError {
    let excerpt: String = body.chars().take(512).collect();
    match status {
        401 | 403 => ProviderError::Auth(excerpt),
        429 => ProviderError::RateLimit(retry_after),
        400..=499 => ProviderError::BadRequest(excerpt),
        500..=599 => ProviderError::Server(status),
        _ => ProviderError::Transport(format!("unexpected status {status}: {excerpt}")),
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
    oauth_mode: bool,
) -> serde_json::Value {
    let max_tokens = if req.max_tokens == 0 {
        DEFAULT_MAX_TOKENS
    } else {
        req.max_tokens
    };

    let system = build_system_blocks(req.system, prompt_caching, oauth_mode);
    let tools = build_tools_array(req.tools, prompt_caching);
    let messages = serde_json::to_value(req.messages).unwrap_or_else(|_| Value::Array(Vec::new()));

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
/// empty Vec when the prompt is empty AND we're not in OAuth mode so
/// the caller can omit the field.
///
/// In OAuth mode the API requires the system text to start with
/// `CLAUDE_CODE_SYSTEM_PREFIX`; this function prepends it (or emits
/// the prefix alone if the caller's system is empty). Requests that
/// already start with the prefix are passed through unchanged.
fn build_system_blocks(system: &str, prompt_caching: bool, oauth_mode: bool) -> Vec<Value> {
    #[cfg(feature = "claude-code-oauth")]
    let effective = if oauth_mode {
        if system.is_empty() {
            crate::oauth::CLAUDE_CODE_SYSTEM_PREFIX.to_string()
        } else if system.starts_with(crate::oauth::CLAUDE_CODE_SYSTEM_PREFIX) {
            system.to_string()
        } else {
            format!("{}\n\n{}", crate::oauth::CLAUDE_CODE_SYSTEM_PREFIX, system)
        }
    } else if system.is_empty() {
        return Vec::new();
    } else {
        system.to_string()
    };
    #[cfg(not(feature = "claude-code-oauth"))]
    let effective = {
        // OAuth path gated out — `oauth_mode` is always false under the
        // default build, so no Claude Code prefix injection happens here.
        let _ = oauth_mode;
        if system.is_empty() {
            return Vec::new();
        }
        system.to_string()
    };

    let mut block = json!({
        "type": "text",
        "text": effective,
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

        let body = build_request_body("claude-opus-4-7", &req, true, false);

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

        let body = build_request_body("claude-opus-4-7", &req, true, false);

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

        let body = build_request_body("claude-opus-4-7", &req, false, false);

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

        let body = build_request_body("claude-opus-4-7", &req, true, false);
        assert!(body.get("system").is_none());
    }

    #[test]
    fn body_includes_max_tokens_and_stream() {
        let msgs = vec![Message::user("hi")];
        let tools: Vec<ToolSpec> = Vec::new();
        let req = make_req("sys", &msgs, &tools);

        let body = build_request_body("claude-opus-4-7", &req, true, false);
        assert_eq!(body.get("model"), Some(&Value::from("claude-opus-4-7")));
        assert_eq!(body.get("stream"), Some(&Value::Bool(true)));
        assert_eq!(body.get("max_tokens"), Some(&Value::from(1024)));
    }

    #[cfg(feature = "claude-code-oauth")]
    #[test]
    fn body_prepends_claude_code_prefix_in_oauth_mode() {
        let msgs = vec![Message::user("hi")];
        let tools: Vec<ToolSpec> = Vec::new();
        let req = make_req("you are harness", &msgs, &tools);

        let body = build_request_body("claude-opus-4-7", &req, false, true);

        let sys = body
            .get("system")
            .and_then(Value::as_array)
            .expect("system array");
        assert_eq!(sys.len(), 1);
        let text = sys[0].get("text").and_then(Value::as_str).unwrap();
        assert!(
            text.starts_with(crate::oauth::CLAUDE_CODE_SYSTEM_PREFIX),
            "oauth mode must start system with Claude Code prefix, got: {text:?}"
        );
        assert!(
            text.contains("you are harness"),
            "user system content must still be present after prefix"
        );
    }

    #[cfg(feature = "claude-code-oauth")]
    #[test]
    fn body_emits_prefix_alone_when_system_empty_in_oauth_mode() {
        let msgs = vec![Message::user("hi")];
        let tools: Vec<ToolSpec> = Vec::new();
        let req = make_req("", &msgs, &tools);

        let body = build_request_body("claude-opus-4-7", &req, false, true);

        let sys = body
            .get("system")
            .and_then(Value::as_array)
            .expect("system array");
        assert_eq!(sys.len(), 1);
        assert_eq!(
            sys[0].get("text").and_then(Value::as_str),
            Some(crate::oauth::CLAUDE_CODE_SYSTEM_PREFIX)
        );
    }

    #[cfg(feature = "claude-code-oauth")]
    #[test]
    fn body_does_not_duplicate_prefix_when_already_present() {
        let msgs = vec![Message::user("hi")];
        let tools: Vec<ToolSpec> = Vec::new();
        let combined = format!(
            "{}\n\nextra context",
            crate::oauth::CLAUDE_CODE_SYSTEM_PREFIX
        );
        let req = make_req(&combined, &msgs, &tools);

        let body = build_request_body("claude-opus-4-7", &req, false, true);

        let text = body
            .get("system")
            .and_then(Value::as_array)
            .and_then(|a| a[0].get("text"))
            .and_then(Value::as_str)
            .unwrap();
        // The prefix should appear exactly once.
        let occurrences = text
            .matches(crate::oauth::CLAUDE_CODE_SYSTEM_PREFIX)
            .count();
        assert_eq!(occurrences, 1, "prefix duplicated: {text:?}");
    }

    #[cfg(feature = "claude-code-oauth")]
    #[test]
    fn body_omits_prefix_in_api_key_mode() {
        let msgs = vec![Message::user("hi")];
        let tools: Vec<ToolSpec> = Vec::new();
        let req = make_req("you are harness", &msgs, &tools);

        let body = build_request_body("claude-opus-4-7", &req, false, false);

        let text = body
            .get("system")
            .and_then(Value::as_array)
            .and_then(|a| a[0].get("text"))
            .and_then(Value::as_str)
            .unwrap();
        assert!(
            !text.contains(crate::oauth::CLAUDE_CODE_SYSTEM_PREFIX),
            "api-key mode must NOT inject the Claude Code prefix, got: {text:?}"
        );
    }

    // Egress redaction mock-server tests — see
    // `docs/security/egress-redaction.md`. The body capture pattern mirrors
    // the openai module's `mock_server_e2e`.
    mod egress_redaction_e2e {
        use super::*;
        use futures_util::StreamExt;
        use harness_proto::{ContentBlock, Message};
        use secrecy::SecretString;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        const FAKE_SECRET: &str = "sk-ant-api03-abcdefghij1234567890XYZ";

        async fn serve_once(captured: std::sync::Arc<tokio::sync::Mutex<Vec<u8>>>) -> u16 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 16 * 1024];
                let mut total = Vec::new();
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    total.extend_from_slice(&buf[..n]);
                    if let Some(hdr_end) = total.windows(4).position(|w| w == b"\r\n\r\n") {
                        let header_str = std::str::from_utf8(&total[..hdr_end]).unwrap_or("");
                        let cl = header_str
                            .lines()
                            .find_map(|l| {
                                let l = l.to_ascii_lowercase();
                                l.strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if total.len() - (hdr_end + 4) >= cl {
                            break;
                        }
                    }
                }
                *captured.lock().await = total;
                // Anthropic SSE minimal: message_stop immediately. We don't
                // need a well-formed event stream because our assertions are
                // on the inbound request body, not outbound events.
                let body = b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                sock.write_all(hdr.as_bytes()).await.unwrap();
                sock.write_all(body).await.unwrap();
                sock.flush().await.unwrap();
            });
            port
        }

        fn build_provider(port: u16, redact_egress: bool) -> AnthropicProvider {
            AnthropicProvider {
                client: reqwest::Client::new(),
                auth: AuthMode::ApiKey(SecretString::from("test".to_string())),
                model: "claude-opus-4-7".into(),
                base_url: Url::parse(&format!("http://127.0.0.1:{port}")).unwrap(),
                prompt_caching: false,
                redact_egress,
            }
        }

        fn tool_result_message() -> Message {
            Message::user_tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: format!("got {FAKE_SECRET} back"),
                is_error: false,
                cache_control: None,
            }])
        }

        async fn capture(redact_egress: bool) -> String {
            let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
            let port = serve_once(captured.clone()).await;
            let provider = build_provider(port, redact_egress);

            let msgs = vec![tool_result_message()];
            let req = StreamRequest {
                system: "",
                messages: &msgs,
                tools: &[],
                max_tokens: 64,
            };
            if let Ok(mut s) = provider.stream(req).await {
                while s.next().await.is_some() {}
            }
            let bytes = captured.lock().await.clone();
            String::from_utf8_lossy(&bytes).into_owned()
        }

        #[tokio::test]
        async fn egress_off_by_default_sends_raw_secret_to_provider() {
            let body = capture(false).await;
            assert!(
                body.contains(FAKE_SECRET),
                "default-off Anthropic path must forward raw tool_result content"
            );
        }

        #[tokio::test]
        async fn egress_on_redacts_secret_in_outbound_body() {
            let body = capture(true).await;
            assert!(
                !body.contains(FAKE_SECRET),
                "secret leaked into outbound Anthropic body despite redact_egress=true: {body}"
            );
            assert!(
                body.contains("[REDACTED:sk]"),
                "redaction marker missing in Anthropic outbound body: {body}"
            );
        }
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
            redact_egress: false,
        };
        assert!(p.prompt_caching_enabled());
        let p = p.with_prompt_caching(false);
        assert!(!p.prompt_caching_enabled());
    }
}
