//! OpenAI Chat Completions API client.
//!
//! Wire contract: POST `/v1/chat/completions` with `stream: true`, SSE frames
//! of shape `data: {ChatCompletionChunk}\n\n` terminated by `data: [DONE]`.
//!
//! The engine (`harness_core::engine::consume_stream`) is provider-agnostic —
//! this module is pure translation from OpenAI's delta shape into the shared
//! `StreamEvent` enum defined in `harness_core::provider`. See PLAN §3.2 /
//! §5.9 for the event contract, §5.12 for `ProviderError` classification.
//!
//! Security (§8.2): API key from `OPENAI_API_KEY` env only, never from
//! `settings.json`. `SecretString` guarantees `Debug` redaction.

use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use harness_core::{
    ContentBlockHeader, ContentDelta, EventStream, Provider, ProviderError, StreamEvent,
    StreamRequest,
};
use harness_proto::{ContentBlock, Message, Role, StopReason, Usage};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;

use crate::egress_redact::maybe_redact_messages;

/// Hardcoded default when `--model` does not specify one. Overridable via
/// `--model` / `HARNESS_MODEL` / `settings.json.model`.
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-4o";

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Conservative default for local-LLM friendliness. Ollama ships with
/// `num_ctx=4096`; asking for 8192 output tokens routinely triggers a server
/// 500 once prompt tokens are added on top. 2048 keeps small local models in
/// their default context budget. Overridable via `StreamRequest.max_tokens`.
const DEFAULT_MAX_TOKENS: u32 = 2048;
/// Frame size cap — anything bigger is a DoS against the parser.
const MAX_FRAME_BYTES: usize = 1 << 20; // 1 MiB

pub struct OpenAIProvider {
    client: reqwest::Client,
    api_key: SecretString,
    model: String,
    base_url: Url,
    /// Streaming-behaviour toggles. Resolved once at construction so a running
    /// provider has stable behaviour across calls — changing the env mid-run
    /// does not take effect (matches how `OPENAI_API_KEY` is resolved).
    cfg: StreamCfg,
    /// Opt-in egress redaction. See `crate::egress_redact` + the docs at
    /// `docs/security/egress-redaction.md`. Default off.
    redact_egress: bool,
}

/// Opt-in streaming tweaks. Today only carries the local-LLM text-based
/// tool-call fallback; kept as a struct so future toggles (thinking passthrough,
/// JSON-schema coercion, ...) don't balloon the provider signature.
#[derive(Debug, Clone, Copy, Default)]
pub struct StreamCfg {
    /// When `true`, buffer assistant text until the stream terminates, then
    /// look for a `<tool:Name>{...}</tool>` envelope. If found, synthesize the
    /// equivalent `tool_use` events (matching the Anthropic-native shape) and
    /// elide the envelope from the text. Off by default — the feature only
    /// helps small local models that can't reliably emit `tool_calls` JSON.
    ///
    /// Toggle via `HARNESS_OPENAI_TEXT_TOOLCALL=1` at provider construction.
    pub text_toolcall_fallback: bool,
}

impl StreamCfg {
    /// Read from env once. `1` / `true` (case-insensitive) enable the fallback;
    /// anything else — including unset — leaves it off. We intentionally do not
    /// re-read env per stream: stable behaviour across a session is easier to
    /// reason about, and tests use the explicit struct form.
    pub fn from_env() -> Self {
        let text_toolcall_fallback = std::env::var("HARNESS_OPENAI_TEXT_TOOLCALL")
            .ok()
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                v == "1" || v == "true"
            })
            .unwrap_or(false);
        Self {
            text_toolcall_fallback,
        }
    }
}

impl std::fmt::Debug for OpenAIProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAIProvider")
            .field("model", &self.model)
            .field("base_url", &self.base_url.as_str())
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl OpenAIProvider {
    /// Read API key from env, build a shared `reqwest::Client`.
    ///
    /// Local-LLM flow: when the resolved base URL points to a loopback host
    /// (localhost / 127.0.0.1 / ::1), `OPENAI_API_KEY` is optional — local
    /// runtimes (Ollama, vLLM, LM Studio, llama.cpp, MLX) accept any or no
    /// bearer. For remote base URLs we still require a non-empty key so a
    /// forgotten key can't silently ship traffic unauthenticated.
    pub fn new(model: impl Into<String>) -> Result<Self, ProviderError> {
        Self::new_with_base_url(model, None)
    }

    /// Like [`Self::new`] but with an optional `base_url` override that takes
    /// precedence over `OPENAI_BASE_URL`. Key resolution uses the final URL
    /// so a CLI `--base-url` pointing at localhost correctly unlocks the
    /// no-key path.
    pub fn new_with_base_url(
        model: impl Into<String>,
        base_url_override: Option<Url>,
    ) -> Result<Self, ProviderError> {
        let base_url = match base_url_override {
            Some(u) => u,
            None => {
                let raw = std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
                Url::parse(&raw)
                    .map_err(|e| ProviderError::BadRequest(format!("invalid base url: {e}")))?
            }
        };

        let api_key = resolve_api_key(&base_url)?;
        let client = build_http_client()?;
        let model = model.into();
        let cfg = StreamCfg::from_env();
        log_init(&base_url, &model, cfg);

        Ok(Self {
            client,
            api_key,
            model,
            base_url,
            cfg,
            redact_egress: false,
        })
    }

    pub fn with_default_model() -> Result<Self, ProviderError> {
        Self::new(DEFAULT_OPENAI_MODEL)
    }

    /// Back-door for tests — never use in prod.
    #[doc(hidden)]
    pub fn with_config(
        model: impl Into<String>,
        api_key: SecretString,
        base_url: Url,
    ) -> Result<Self, ProviderError> {
        Self::with_config_and_cfg(model, api_key, base_url, StreamCfg::from_env())
    }

    /// Back-door for tests that need to pin `StreamCfg` explicitly (rather
    /// than rely on env). Never use in prod.
    #[doc(hidden)]
    pub fn with_config_and_cfg(
        model: impl Into<String>,
        api_key: SecretString,
        base_url: Url,
        cfg: StreamCfg,
    ) -> Result<Self, ProviderError> {
        let client = build_http_client()?;
        let model = model.into();
        log_init(&base_url, &model, cfg);
        Ok(Self {
            client,
            api_key,
            model,
            base_url,
            cfg,
            redact_egress: false,
        })
    }

    #[inline]
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Enable egress-side secret redaction. Mirrors
    /// `AnthropicProvider::with_redact_egress`. Default: off.
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

    fn chat_completions_url(&self) -> Result<Url, ProviderError> {
        self.base_url
            .join("/v1/chat/completions")
            .map_err(|e| ProviderError::BadRequest(format!("join url: {e}")))
    }
}

/// Log the resolved base URL + model once at provider construction so users
/// can verify where traffic is actually headed. API key is never logged —
/// `SecretString` is not formatted here and we intentionally do not reference
/// it. Critical for local-LLM debugging: "did harness hit Ollama or the
/// public API?" has been a recurring first-time-user confusion.
fn log_init(base_url: &Url, model: &str, cfg: StreamCfg) {
    tracing::info!(
        target: "harness_provider::openai",
        provider = "openai-compat",
        base_url = %base_url,
        model = model,
        local = is_local_url(base_url),
        text_toolcall_fallback = cfg.text_toolcall_fallback,
        "openai provider initialized"
    );
}

fn build_http_client() -> Result<reqwest::Client, ProviderError> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(concat!("harness/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| ProviderError::Transport(e.to_string()))
}

/// Hostname is local-only if a lookup-free match shows it's loopback. We
/// intentionally do NOT resolve DNS — a hostname like `host.docker.internal`
/// could point anywhere. Users with those setups can set a placeholder key.
pub fn is_local_url(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => {
            let d = d.to_ascii_lowercase();
            d == "localhost" || d == "localhost."
        }
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

fn resolve_api_key(base_url: &Url) -> Result<SecretString, ProviderError> {
    match std::env::var("OPENAI_API_KEY") {
        Ok(raw) if !raw.trim().is_empty() => Ok(SecretString::from(raw)),
        Ok(_) | Err(_) if is_local_url(base_url) => {
            // Local runtimes accept any bearer; send a placeholder so the
            // Authorization header is well-formed.
            Ok(SecretString::from("local".to_string()))
        }
        Ok(_) => Err(ProviderError::Auth("OPENAI_API_KEY is empty".into())),
        Err(_) => Err(ProviderError::Auth("OPENAI_API_KEY not set".into())),
    }
}

#[async_trait]
impl Provider for OpenAIProvider {
    async fn stream(&self, req: StreamRequest<'_>) -> Result<EventStream, ProviderError> {
        let url = self.chat_completions_url()?;
        // Apply opt-in egress redaction BEFORE wire-format construction.
        let scrubbed = maybe_redact_messages(req.messages, self.redact_egress);
        let req_eff = StreamRequest {
            system: req.system,
            messages: &scrubbed,
            tools: req.tools,
            max_tokens: req.max_tokens,
        };
        let body = build_request_body(&self.model, &req_eff);

        let resp = self
            .client
            .post(url)
            .header("accept", "text/event-stream")
            .header("content-type", "application/json")
            .bearer_auth(self.api_key.expose_secret())
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

        Ok(parse_stream_with_cfg(resp.bytes_stream(), self.cfg))
    }
}

fn retry_after_from_headers(h: &reqwest::header::HeaderMap) -> Option<Duration> {
    let v = h.get("retry-after")?.to_str().ok()?;
    // Either integer seconds or HTTP-date; accept only the integer form.
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

// ---------------------------------------------------------------------------
// Request body construction
// ---------------------------------------------------------------------------

/// Build the wire-format JSON body. Pure, unit-testable.
///
/// - `system` → prepended as `{role:"system"}`
/// - `messages` translated per [`translate_message`]
/// - `tools` translated to OpenAI function-calling schema
/// - `stream_options.include_usage: true` so the final chunk carries usage
///   (OpenAI only emits `usage` if we opt in).
pub(crate) fn build_request_body(model: &str, req: &StreamRequest<'_>) -> Value {
    let max_tokens = if req.max_tokens > 0 {
        req.max_tokens
    } else {
        tracing::debug!(
            default = DEFAULT_MAX_TOKENS,
            "openai: StreamRequest.max_tokens not set; falling back to default"
        );
        DEFAULT_MAX_TOKENS
    };

    let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len() + 1);
    if !req.system.is_empty() {
        messages.push(json!({ "role": "system", "content": req.system }));
    }
    for m in req.messages {
        messages.extend(translate_message(m));
    }

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }

    body
}

/// Translate a single harness `Message` to one-or-more OpenAI messages.
///
/// OpenAI's shape differs from Anthropic in two important ways:
///   1. Tool results are a separate `role: "tool"` message (not nested in user).
///   2. Assistant `tool_use` blocks are serialized as `tool_calls: [...]` with
///      `content: null`.
///
/// A single Anthropic-style user message carrying N `ToolResult` blocks
/// becomes N `role:"tool"` messages. An assistant message mixing text +
/// tool_use blocks becomes a single assistant message with `content` (text)
/// and `tool_calls` (tool_use).
fn translate_message(m: &Message) -> Vec<Value> {
    match m.role {
        Role::System => {
            // Flatten text blocks; system here is unusual but supported.
            let text = collect_text(&m.content);
            vec![json!({ "role": "system", "content": text })]
        }
        Role::User => {
            // If this message carries tool_results, emit one role:"tool"
            // message per ToolResult. Otherwise, single user message.
            let has_tool_results = m
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }));
            if has_tool_results {
                let mut out = Vec::new();
                let user_text = collect_text(&m.content);
                if !user_text.is_empty() {
                    out.push(json!({ "role": "user", "content": user_text }));
                }
                for b in &m.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } = b
                    {
                        // OpenAI has no `is_error` field — tag inline so the
                        // model can distinguish errors from normal results.
                        let body = if *is_error {
                            format!("[error] {content}")
                        } else {
                            content.clone()
                        };
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": body,
                        }));
                    }
                }
                out
            } else {
                vec![json!({
                    "role": "user",
                    "content": collect_text(&m.content),
                })]
            }
        }
        Role::Assistant => {
            let text = collect_text(&m.content);
            let tool_calls: Vec<Value> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse {
                        id, name, input, ..
                    } => Some(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": input.to_string(),
                        },
                    })),
                    _ => None,
                })
                .collect();

            let mut msg = serde_json::Map::new();
            msg.insert("role".into(), Value::String("assistant".into()));
            if text.is_empty() && !tool_calls.is_empty() {
                // OpenAI allows content: null when only tool_calls present.
                msg.insert("content".into(), Value::Null);
            } else {
                msg.insert("content".into(), Value::String(text));
            }
            if !tool_calls.is_empty() {
                msg.insert("tool_calls".into(), Value::Array(tool_calls));
            }
            vec![Value::Object(msg)]
        }
    }
}

fn collect_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        if let ContentBlock::Text { text, .. } = b {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// SSE parsing + OpenAI chunk → StreamEvent translation
// ---------------------------------------------------------------------------

/// Entry point. Frames `\n\n`-delimited SSE, parses each `data: {...}` body
/// as a `ChatCompletionChunk`, and fans out into `StreamEvent`s.
///
/// OpenAI quirks handled:
/// - `data: [DONE]` is a sentinel (no JSON) — terminate cleanly.
/// - The final chunk typically has `choices: []` and carries `usage`. We still
///   need to flush any open blocks on prior `finish_reason`.
/// - `tool_calls[i].function.arguments` is a stream of fragments; they may
///   split mid-JSON-token (even mid-UTF-8 byte). We byte-concat at the engine,
///   so emit raw bytes as `ContentDelta::InputJson`.
#[cfg(test)]
pub(crate) fn parse_stream<S>(inner: S) -> EventStream
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    parse_stream_with_cfg(inner, StreamCfg::default())
}

pub(crate) fn parse_stream_with_cfg<S>(inner: S, cfg: StreamCfg) -> EventStream
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    Box::pin(SseStream {
        inner: Box::pin(inner),
        buf: BytesMut::with_capacity(16 * 1024),
        queue: VecDeque::new(),
        state: StreamState::with_cfg(cfg),
        done: false,
    })
}

#[derive(Default)]
struct StreamState {
    /// First chunk emits `MessageStart`. Tracks that.
    started: bool,
    /// Accumulated usage (from final chunk).
    usage: Usage,
    /// Accumulated stop reason. OpenAI may emit `finish_reason` on a chunk that
    /// still has deltas; we defer `MessageDelta` + `MessageStop` until the SSE
    /// stream ends (usually signalled by `[DONE]` or EOF).
    stop_reason: Option<StopReason>,
    /// Whether we have emitted `MessageDelta` already.
    delta_emitted: bool,
    /// text block state
    text: TextBlockState,
    /// tool_call blocks keyed by the OpenAI delta's `tool_calls[i].index`.
    /// Ordered by index → stable iteration for `ContentBlockStop` on finalize.
    tool_calls: BTreeMap<usize, ToolCallBlockState>,
    /// harness-side block indices: `0` for text, `1..` for tool_calls in
    /// emission order. We assign these the first time a block opens so engine
    /// indexing is stable.
    next_block_index: usize,
    /// Per-stream config snapshot. Copied in once at construction.
    cfg: StreamCfg,
    /// Buffered text awaiting `flush_terminal` inspection when
    /// `cfg.text_toolcall_fallback` is on. While this is active, `text` above
    /// stays un-opened and no text `ContentBlockStart`/`Delta` events go out;
    /// `flush_terminal` decides what to do with the whole buffer.
    buffered_text: String,
}

impl StreamState {
    fn with_cfg(cfg: StreamCfg) -> Self {
        Self {
            cfg,
            ..Self::default()
        }
    }
}

#[derive(Default)]
struct TextBlockState {
    opened: bool,
    /// harness block index assigned once opened.
    index: Option<usize>,
}

struct ToolCallBlockState {
    /// harness block index (stable once opened).
    index: usize,
    /// Emitted `ContentBlockStart` yet? (requires `id` + `name`.)
    opened: bool,
    /// Latest tool_call id seen (OpenAI usually emits it once on the first
    /// delta, but be defensive).
    id: Option<String>,
    /// Latest function.name seen.
    name: Option<String>,
    /// Buffered `arguments` fragments seen *before* `ContentBlockStart` was
    /// emitted. We stash them until `id`+`name` show up, then flush.
    pending_args: Vec<Vec<u8>>,
}

struct SseStream<S> {
    inner: Pin<Box<S>>,
    buf: BytesMut,
    queue: VecDeque<StreamEvent>,
    state: StreamState,
    done: bool,
}

impl<S> Stream for SseStream<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send,
{
    type Item = Result<StreamEvent, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);
        loop {
            if let Some(ev) = this.queue.pop_front() {
                return Poll::Ready(Some(Ok(ev)));
            }

            if let Some(frame) = extract_frame(&mut this.buf) {
                if frame.len() > MAX_FRAME_BYTES {
                    this.done = true;
                    return Poll::Ready(Some(Err(ProviderError::Parse(format!(
                        "sse frame exceeds {MAX_FRAME_BYTES} bytes"
                    )))));
                }
                match process_frame(&frame, &mut this.state) {
                    Ok(FrameOutcome::Events(events)) => {
                        this.queue.extend(events);
                        continue;
                    }
                    Ok(FrameOutcome::Done) => {
                        // Flush remaining open blocks + MessageDelta + MessageStop.
                        flush_terminal(&mut this.state, &mut this.queue);
                        this.done = true;
                        continue;
                    }
                    Err(e) => {
                        this.done = true;
                        return Poll::Ready(Some(Err(e)));
                    }
                }
            }

            if this.done {
                return Poll::Ready(None);
            }

            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.done = true;
                    if !this.buf.is_empty() {
                        return Poll::Ready(Some(Err(ProviderError::StreamDropped)));
                    }
                    // Clean EOF without explicit [DONE] — still flush so the
                    // engine sees a tidy MessageStop.
                    if this.state.started {
                        flush_terminal(&mut this.state, &mut this.queue);
                        continue;
                    }
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Err(e))) => {
                    this.done = true;
                    return Poll::Ready(Some(Err(ProviderError::Transport(e.to_string()))));
                }
                Poll::Ready(Some(Ok(bytes))) => {
                    this.buf.extend_from_slice(&bytes);
                }
            }
        }
    }
}

enum FrameOutcome {
    Events(Vec<StreamEvent>),
    Done,
}

fn extract_frame(buf: &mut BytesMut) -> Option<String> {
    let (end, sep_len) = find_frame_end(buf)?;
    let frame_bytes = buf.split_to(end + sep_len);
    let body_len = frame_bytes.len() - sep_len;
    Some(String::from_utf8_lossy(&frame_bytes[..body_len]).into_owned())
}

fn find_frame_end(haystack: &[u8]) -> Option<(usize, usize)> {
    // Prefer `\r\n\r\n`; fall back to `\n\n`.
    if let Some(i) = haystack.windows(4).position(|w| w == b"\r\n\r\n") {
        return Some((i, 4));
    }
    haystack
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|i| (i, 2))
}

fn process_frame(frame: &str, state: &mut StreamState) -> Result<FrameOutcome, ProviderError> {
    let mut data_parts: Vec<&str> = Vec::new();
    for line in frame.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data_parts.push(rest.trim_start_matches(' '));
        }
        // OpenAI never emits `event:` / `id:` / `retry:` — ignore if seen.
    }

    if data_parts.is_empty() {
        return Ok(FrameOutcome::Events(Vec::new()));
    }
    let data = data_parts.join("\n");
    if data.trim() == "[DONE]" {
        return Ok(FrameOutcome::Done);
    }

    let chunk: ChatCompletionChunk = serde_json::from_str(&data)
        .map_err(|e| ProviderError::Parse(format!("openai chunk json: {e}")))?;

    Ok(FrameOutcome::Events(translate_chunk(chunk, state)?))
}

fn translate_chunk(
    chunk: ChatCompletionChunk,
    state: &mut StreamState,
) -> Result<Vec<StreamEvent>, ProviderError> {
    let mut out = Vec::new();

    if !state.started {
        state.started = true;
        out.push(StreamEvent::MessageStart {
            message_id: chunk.id.clone().unwrap_or_default(),
            usage: Usage::default(),
        });
    }

    // Carry usage if present (typically final chunk).
    if let Some(u) = chunk.usage {
        state.usage = state.usage.merge(u.into_usage());
    }

    for choice in chunk.choices {
        // Text delta.
        if let Some(text) = choice.delta.content {
            if !text.is_empty() {
                if state.cfg.text_toolcall_fallback {
                    // Defer: flush_terminal decides whether this text held a
                    // `<tool:Name>{...}</tool>` envelope and splits accordingly.
                    state.buffered_text.push_str(&text);
                } else {
                    if !state.text.opened {
                        let idx = state.next_block_index;
                        state.next_block_index += 1;
                        state.text.index = Some(idx);
                        state.text.opened = true;
                        out.push(StreamEvent::ContentBlockStart {
                            index: idx,
                            block: ContentBlockHeader::Text,
                        });
                    }
                    if let Some(idx) = state.text.index {
                        out.push(StreamEvent::ContentBlockDelta {
                            index: idx,
                            delta: ContentDelta::Text(text),
                        });
                    }
                }
            }
        }

        // Tool-call deltas. When the runtime omits `tool_calls[i].index` (seen
        // on Ollama and some llama.cpp `--jinja` templates) fall back to the
        // element's position within this delta's array. This preserves
        // single-call correctness; multi-call interleaving without `index` is
        // unrecoverable by nature and we document that in docs/local-llm.
        for (array_pos, tc) in choice.delta.tool_calls.into_iter().enumerate() {
            let tc_index = tc.index.unwrap_or(array_pos);
            let entry = state.tool_calls.entry(tc_index).or_insert_with(|| {
                // Reserve a harness index now so ordering is stable.
                let idx = state.next_block_index;
                state.next_block_index += 1;
                ToolCallBlockState {
                    index: idx,
                    opened: false,
                    id: None,
                    name: None,
                    pending_args: Vec::new(),
                }
            });

            if let Some(id) = tc.id {
                entry.id = Some(id);
            }
            if let Some(f) = tc.function.as_ref() {
                if let Some(name) = f.name.as_ref() {
                    entry.name = Some(name.clone());
                }
            }

            // Once id + name known, emit ContentBlockStart (once) and flush any
            // queued argument fragments.
            if !entry.opened {
                if let (Some(id), Some(name)) = (entry.id.clone(), entry.name.clone()) {
                    entry.opened = true;
                    out.push(StreamEvent::ContentBlockStart {
                        index: entry.index,
                        block: ContentBlockHeader::ToolUse { id, name },
                    });
                    for buffered in entry.pending_args.drain(..) {
                        out.push(StreamEvent::ContentBlockDelta {
                            index: entry.index,
                            delta: ContentDelta::InputJson(buffered),
                        });
                    }
                }
            }

            if let Some(f) = tc.function {
                if let Some(args) = f.arguments {
                    if !args.is_empty() {
                        let bytes = args.into_bytes();
                        if entry.opened {
                            out.push(StreamEvent::ContentBlockDelta {
                                index: entry.index,
                                delta: ContentDelta::InputJson(bytes),
                            });
                        } else {
                            entry.pending_args.push(bytes);
                        }
                    }
                }
            }
        }

        // Record finish_reason; don't emit MessageDelta yet — we wait for
        // [DONE] / EOF so usage (which arrives in a later chunk with
        // choices:[]) has a chance to accumulate first.
        if let Some(reason) = choice.finish_reason {
            state.stop_reason = Some(map_finish_reason(&reason)?);
        }
    }

    Ok(out)
}

/// Pattern for the local-LLM text-based tool-call envelope. Matches:
///
/// ```text
/// <tool:ToolName>{"some":"json"}</tool>
/// ```
///
/// - Capture 1: tool name (word chars — letters, digits, `_`).
/// - Capture 2: a `{...}` JSON object, greedy over everything between a leading
///   `{` and the last `}` before `</tool>`. Using `[\s\S]*?` keeps it
///   non-greedy so two envelopes on the same line don't merge.
///
/// Kept lazily initialised via `once_cell`-less `LazyLock` equivalent pattern:
/// small cost on first use, no runtime re-compile. We allocate per-call in
/// `synthesize_text_toolcall` instead — compilation is microseconds and this
/// path only fires for local-LLM streams. Avoids dragging `once_cell` in.
const LOCAL_TOOL_ENVELOPE_RE: &str = r"<tool:(\w+)>\s*(\{[\s\S]*?\})\s*</tool>";

/// Try to rewrite the buffered text into a synthesized tool_use + residual
/// text. Returns events to push (in order) and the stop_reason override to
/// apply if a tool was synthesized.
///
/// Decision matrix:
/// - No envelope match → emit a single plain text block; no stop_reason change.
/// - Envelope found but JSON is invalid → same as "no match" (safer to show
///   the raw text than crash). We log a warning so operators can spot it.
/// - Envelope found, JSON valid → emit `[optional Text block for any residual
///   prose] + ContentBlockStart(ToolUse) + InputJson + ContentBlockStop`, and
///   ask the caller to force `stop_reason = ToolUse` if the model claimed
///   `EndTurn` / `None` (the engine's turn loop keys on it to run the tool).
///
/// Only the *first* envelope in the buffer is extracted — models don't
/// typically emit more than one per turn, and chaining is better handled by
/// the next turn. If users report chained-calls in the wild we can revisit.
fn synthesize_text_toolcall(
    buffered_text: &str,
    state: &mut StreamState,
) -> (Vec<StreamEvent>, bool) {
    let mut out = Vec::new();
    let re = match regex::Regex::new(LOCAL_TOOL_ENVELOPE_RE) {
        Ok(re) => re,
        Err(e) => {
            // Programmer error; surface and fall through to plain text.
            tracing::error!(
                target: "harness_provider::openai",
                pattern = LOCAL_TOOL_ENVELOPE_RE,
                error = %e,
                "local-LLM tool-call regex failed to compile; emitting raw text"
            );
            emit_plain_text(buffered_text, state, &mut out);
            return (out, false);
        }
    };

    let Some(caps) = re.captures(buffered_text) else {
        emit_plain_text(buffered_text, state, &mut out);
        return (out, false);
    };

    // Extract pieces.
    let whole = caps.get(0).expect("capture 0 always present");
    let name = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
    let json_raw = caps.get(2).map(|m| m.as_str()).unwrap_or_default();

    // Validate JSON. Invalid → leave text alone. Silent downgrade with a warn
    // so operators can diagnose a misbehaving prompt without a stream crash.
    if serde_json::from_str::<serde_json::Value>(json_raw).is_err() {
        tracing::warn!(
            target: "harness_provider::openai",
            tool_name = name,
            "local-LLM tool-call envelope matched but JSON is malformed; falling through as text"
        );
        emit_plain_text(buffered_text, state, &mut out);
        return (out, false);
    }

    // Residual text = buffer minus the envelope (with incidental whitespace
    // around the envelope compacted — a trailing newline after `</tool>` is
    // almost always boilerplate, not signal).
    let mut residual = String::with_capacity(buffered_text.len());
    residual.push_str(&buffered_text[..whole.start()]);
    residual.push_str(&buffered_text[whole.end()..]);
    let trimmed = residual.trim();
    if !trimmed.is_empty() {
        emit_plain_text(trimmed, state, &mut out);
    }

    // Synthesize the tool_use block. Id must be unique within the message —
    // tool_call blocks earlier in this stream already consumed indexes, so we
    // derive an id from the next block index to avoid collision.
    let tool_idx = state.next_block_index;
    state.next_block_index += 1;
    let synthetic_id = format!("call_local_{tool_idx}");

    out.push(StreamEvent::ContentBlockStart {
        index: tool_idx,
        block: ContentBlockHeader::ToolUse {
            id: synthetic_id,
            name: name.to_string(),
        },
    });
    out.push(StreamEvent::ContentBlockDelta {
        index: tool_idx,
        delta: ContentDelta::InputJson(json_raw.as_bytes().to_vec()),
    });
    out.push(StreamEvent::ContentBlockStop { index: tool_idx });

    (out, true)
}

/// Open/close a text block carrying `text` verbatim. Used by the fallback
/// path to flush either the full buffer (no match) or the envelope-stripped
/// residual (match). The `text.opened` flag is left `false` afterwards so
/// `flush_terminal` won't double-close.
fn emit_plain_text(text: &str, state: &mut StreamState, out: &mut Vec<StreamEvent>) {
    if text.is_empty() {
        return;
    }
    let idx = state.next_block_index;
    state.next_block_index += 1;
    out.push(StreamEvent::ContentBlockStart {
        index: idx,
        block: ContentBlockHeader::Text,
    });
    out.push(StreamEvent::ContentBlockDelta {
        index: idx,
        delta: ContentDelta::Text(text.to_string()),
    });
    out.push(StreamEvent::ContentBlockStop { index: idx });
}

/// Called on `[DONE]` or on clean EOF after at least one chunk: emit
/// `ContentBlockStop` for each open block, then `MessageDelta` + `MessageStop`.
///
/// When the local-LLM text-toolcall fallback is on, text content was buffered
/// in `state.buffered_text` rather than streamed out. Here we inspect that
/// buffer, synthesize a `tool_use` block if an envelope is found, and emit
/// the stripped residual (if any) as a normal text block — preserving
/// prose-around-envelope output shape.
fn flush_terminal(state: &mut StreamState, queue: &mut VecDeque<StreamEvent>) {
    // Local-LLM fallback: replay buffered text (with optional tool-use
    // synthesis) before closing native blocks. Happens before the `text`
    // block close because in fallback mode the native text block is never
    // opened — `emit_plain_text` / `synthesize_text_toolcall` open-and-close
    // their own blocks with stable indexes.
    let mut synthesized_tool = false;
    if state.cfg.text_toolcall_fallback && !state.buffered_text.is_empty() {
        let buf = std::mem::take(&mut state.buffered_text);
        let (events, synth) = synthesize_text_toolcall(&buf, state);
        synthesized_tool = synth;
        queue.extend(events);
    }

    if state.text.opened {
        if let Some(idx) = state.text.index {
            queue.push_back(StreamEvent::ContentBlockStop { index: idx });
        }
        state.text.opened = false;
    }
    // Iterate in BTreeMap order → stable by `tool_calls[i].index`.
    let tool_calls: Vec<_> = std::mem::take(&mut state.tool_calls).into_iter().collect();
    for (_, tc) in tool_calls {
        if tc.opened {
            queue.push_back(StreamEvent::ContentBlockStop { index: tc.index });
        }
    }

    // If we synthesized a tool_use but the model claimed `EndTurn` (or never
    // reported a finish_reason), upgrade to `ToolUse` so the turn loop knows
    // to actually execute the tool instead of terminating the conversation.
    // `MaxTokens` / `StopSequence` are preserved — they're stronger signals
    // that the model didn't finish its thought and forcing ToolUse would
    // paper over truncation bugs.
    if synthesized_tool {
        match state.stop_reason {
            None | Some(StopReason::EndTurn) => {
                state.stop_reason = Some(StopReason::ToolUse);
            }
            _ => {}
        }
    }

    if !state.delta_emitted {
        queue.push_back(StreamEvent::MessageDelta {
            stop_reason: state.stop_reason,
            usage: state.usage,
        });
        state.delta_emitted = true;
    }
    queue.push_back(StreamEvent::MessageStop);
}

/// Map an OpenAI / OpenAI-compatible `finish_reason` to the provider-neutral
/// [`StopReason`]. Local runtimes (Ollama, llama.cpp, MLX servers) emit a
/// wider zoo of values than the OpenAI reference — we handle the common
/// variants explicitly and `warn!` on anything unknown so truncation or a new
/// upstream value is visible in logs rather than silently mapped to EndTurn.
///
/// Returns `Err(ProviderError::BadRequest)` for `"content_filter"` — callers
/// expect the loop to stop loudly so the user sees their prompt was refused
/// rather than blaming a quiet "model had nothing to say".
fn map_finish_reason(s: &str) -> Result<StopReason, ProviderError> {
    match s {
        // Normal turn completion. "end_turn" is Anthropic's name that some
        // shim layers pass through untouched.
        "stop" | "end_turn" | "eos" => Ok(StopReason::EndTurn),
        // Tool call completion. "function_call" is the deprecated OpenAI name.
        "tool_calls" | "function_call" => Ok(StopReason::ToolUse),
        // Budget exhaustion. Visible so the caller can retry with more tokens.
        "length" | "max_tokens" => Ok(StopReason::MaxTokens),
        // Policy refusal. Surface as a hard error rather than silently
        // converting to EndTurn — the user needs to see that their request
        // was blocked upstream, not stall on an empty response.
        "content_filter" => Err(ProviderError::BadRequest(
            "provider refused: content_filter".into(),
        )),
        other => {
            tracing::warn!(
                target: "harness_provider::openai",
                finish_reason = other,
                "unknown finish_reason; defaulting to EndTurn so the loop can exit"
            );
            Ok(StopReason::EndTurn)
        }
    }
}

// ---------------------------------------------------------------------------
// Wire-format shapes — match OpenAI Chat Completions streaming verbatim.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ChatCompletionChunk {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: ChunkDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChunkDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChunkToolCall>,
}

#[derive(Debug, Deserialize)]
struct ChunkToolCall {
    /// Position within the assistant's tool_calls array. OpenAI reference
    /// servers always emit this; Ollama and llama.cpp `--jinja` templates
    /// sometimes omit it. When missing, `translate_chunk` falls back to the
    /// tool_call's position in the enclosing delta array — lossy for
    /// multi-call interleaving, but keeps the single-call case (the common
    /// one on small local models) working.
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ChunkFunction>,
}

#[derive(Debug, Deserialize)]
struct ChunkFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

impl OpenAIUsage {
    fn into_usage(self) -> Usage {
        Usage {
            input_tokens: self.prompt_tokens,
            output_tokens: self.completion_tokens,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures_util::StreamExt;
    use harness_core::ToolSpec;
    use harness_proto::{ContentBlock, Message, Role};

    fn req<'a>(
        _model: &'a str,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
    ) -> StreamRequest<'a> {
        // `model` is intentionally unused: `StreamRequest` no longer carries
        // a model field — the concrete provider holds it. We keep the arg to
        // minimise churn in the test call sites.
        StreamRequest {
            system,
            messages,
            tools,
            max_tokens: 1024,
        }
    }

    fn stream_of(chunks: Vec<&'static [u8]>) -> EventStream {
        use futures_util::stream;
        let items: Vec<Result<Bytes, reqwest::Error>> = chunks
            .into_iter()
            .map(|b| Ok(Bytes::from_static(b)))
            .collect();
        parse_stream(stream::iter(items))
    }

    fn stream_of_with_cfg(chunks: Vec<&'static [u8]>, cfg: StreamCfg) -> EventStream {
        use futures_util::stream;
        let items: Vec<Result<Bytes, reqwest::Error>> = chunks
            .into_iter()
            .map(|b| Ok(Bytes::from_static(b)))
            .collect();
        parse_stream_with_cfg(stream::iter(items), cfg)
    }

    async fn collect(mut s: EventStream) -> Vec<Result<StreamEvent, ProviderError>> {
        let mut out = Vec::new();
        while let Some(ev) = s.next().await {
            out.push(ev);
        }
        out
    }

    #[test]
    fn request_body_has_model_stream_and_system_message() {
        let msgs = vec![Message::user("hi")];
        let body = build_request_body("gpt-4o", &req("gpt-4o", "you are helpful", &msgs, &[]));
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["max_tokens"], 1024);
        // First message is the system prompt.
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "you are helpful");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "hi");
    }

    #[test]
    fn request_body_translates_tools_to_function_schema() {
        let tools = vec![ToolSpec {
            name: "Read".into(),
            description: "read a file".into(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        }];
        let body = build_request_body("m", &req("m", "", &[], &tools));
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "Read");
        assert_eq!(body["tools"][0]["function"]["description"], "read a file");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn request_body_translates_tool_use_and_tool_result_messages() {
        let assistant = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "Read".into(),
                input: json!({"file_path": "/tmp/a"}),
                cache_control: None,
            }],
            usage: None,
        };
        let user_result = Message::user_tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: "contents of a".into(),
            is_error: false,
            cache_control: None,
        }]);
        let err_result = Message::user_tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "call_2".into(),
            content: "boom".into(),
            is_error: true,
            cache_control: None,
        }]);
        let msgs = vec![assistant, user_result, err_result];
        let body = build_request_body("m", &req("m", "", &msgs, &[]));
        // assistant → tool_calls[0]
        assert_eq!(body["messages"][0]["role"], "assistant");
        assert!(body["messages"][0]["content"].is_null());
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["function"]["name"],
            "Read"
        );
        // arguments is stringified JSON
        let args = body["messages"][0]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments must be a string");
        assert!(args.contains("file_path"));
        // user_tool_results → role:"tool"
        assert_eq!(body["messages"][1]["role"], "tool");
        assert_eq!(body["messages"][1]["tool_call_id"], "call_1");
        assert_eq!(body["messages"][1]["content"], "contents of a");
        // is_error prefixed
        assert_eq!(body["messages"][2]["role"], "tool");
        assert_eq!(body["messages"][2]["content"], "[error] boom");
    }

    #[tokio::test]
    async fn text_deltas_produce_message_start_block_and_stop() {
        let body = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hel\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let evs: Vec<_> = collect(stream_of(vec![body.as_bytes()]))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        // Sequence: MessageStart, ContentBlockStart(0,text), Text("hel"), Text("lo"),
        // ContentBlockStop(0), MessageDelta(EndTurn, usage{5,2}), MessageStop.
        assert!(matches!(evs[0], StreamEvent::MessageStart { .. }));
        assert!(matches!(
            evs[1],
            StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockHeader::Text
            }
        ));
        match &evs[2] {
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::Text(s),
            } => assert_eq!(s, "hel"),
            other => panic!("expected text delta, got {other:?}"),
        }
        match &evs[3] {
            StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentDelta::Text(s),
            } => assert_eq!(s, "lo"),
            other => panic!("expected text delta, got {other:?}"),
        }
        assert!(matches!(evs[4], StreamEvent::ContentBlockStop { index: 0 }));
        match &evs[5] {
            StreamEvent::MessageDelta { stop_reason, usage } => {
                assert_eq!(*stop_reason, Some(StopReason::EndTurn));
                assert_eq!(usage.input_tokens, 5);
                assert_eq!(usage.output_tokens, 2);
            }
            other => panic!("expected MessageDelta, got {other:?}"),
        }
        assert!(matches!(evs[6], StreamEvent::MessageStop));
    }

    #[tokio::test]
    async fn tool_call_with_fragmented_arguments() {
        // id + name appear on first tool_call delta, then arguments stream in
        // 3 fragments, then finish_reason=tool_calls, then [DONE].
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"type\":\"function\",\"function\":{\"name\":\"Read\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"fi\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"le_path\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"/tmp/a\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let evs: Vec<_> = collect(stream_of(vec![body.as_bytes()]))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();

        assert!(matches!(evs[0], StreamEvent::MessageStart { .. }));
        match &evs[1] {
            StreamEvent::ContentBlockStart {
                index,
                block: ContentBlockHeader::ToolUse { id, name },
            } => {
                assert_eq!(*index, 0);
                assert_eq!(id, "call_abc");
                assert_eq!(name, "Read");
            }
            other => panic!("expected ToolUse start, got {other:?}"),
        }

        // Collect all InputJson fragments; byte-concat must equal the full
        // arguments JSON. Also verify no text-delta events slipped in.
        let mut collected = Vec::<u8>::new();
        let mut saw_stop = false;
        let mut saw_delta = false;
        let mut saw_stop_msg = false;
        for ev in &evs[2..] {
            match ev {
                StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentDelta::InputJson(bytes),
                } => collected.extend_from_slice(bytes),
                StreamEvent::ContentBlockStop { index: 0 } => saw_stop = true,
                StreamEvent::MessageDelta { stop_reason, .. } => {
                    saw_delta = true;
                    assert_eq!(*stop_reason, Some(StopReason::ToolUse));
                }
                StreamEvent::MessageStop => saw_stop_msg = true,
                StreamEvent::ContentBlockDelta {
                    delta: ContentDelta::Text(_),
                    ..
                } => panic!("unexpected text delta during tool_call stream"),
                _ => {}
            }
        }
        assert_eq!(
            std::str::from_utf8(&collected).unwrap(),
            r#"{"file_path":"/tmp/a"}"#
        );
        assert!(saw_stop, "expected ContentBlockStop");
        assert!(saw_delta, "expected MessageDelta(ToolUse)");
        assert!(saw_stop_msg, "expected MessageStop");
    }

    #[tokio::test]
    async fn finish_reason_stop_maps_to_end_turn() {
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let evs: Vec<_> = collect(stream_of(vec![body.as_bytes()]))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        let (delta, stop) = evs
            .iter()
            .find_map(|ev| match ev {
                StreamEvent::MessageDelta { stop_reason, .. } => Some((stop_reason, true)),
                _ => None,
            })
            .expect("MessageDelta not emitted");
        assert_eq!(*delta, Some(StopReason::EndTurn));
        assert!(stop);
        assert!(matches!(evs.last(), Some(StreamEvent::MessageStop)));
    }

    #[tokio::test]
    async fn finish_reason_tool_calls_maps_to_tool_use() {
        // A terminal chunk with finish_reason=tool_calls must surface as
        // StopReason::ToolUse on MessageDelta. This covers the case where
        // tool_calls complete and no text was emitted.
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t1\",\"type\":\"function\",\"function\":{\"name\":\"X\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let evs: Vec<_> = collect(stream_of(vec![body.as_bytes()]))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        let stop_reason = evs.iter().find_map(|ev| match ev {
            StreamEvent::MessageDelta { stop_reason, .. } => Some(*stop_reason),
            _ => None,
        });
        assert_eq!(stop_reason, Some(Some(StopReason::ToolUse)));
    }

    #[tokio::test]
    async fn finish_reason_length_maps_to_max_tokens() {
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"truncat\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let evs: Vec<_> = collect(stream_of(vec![body.as_bytes()]))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        let stop_reason = evs.iter().find_map(|ev| match ev {
            StreamEvent::MessageDelta { stop_reason, .. } => Some(*stop_reason),
            _ => None,
        });
        assert_eq!(stop_reason, Some(Some(StopReason::MaxTokens)));
    }

    #[tokio::test]
    async fn finish_reason_length_warns() {
        // `length` is a truncation signal. It must still reach the engine as
        // MaxTokens (not silently dropped to EndTurn) so the loop can decide
        // whether to retry with a larger budget.
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let evs: Vec<_> = collect(stream_of(vec![body.as_bytes()]))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        // MaxTokens survives end-to-end — asserted here because a regression
        // that silently remapped it to EndTurn would be invisible at runtime.
        let sr = evs
            .iter()
            .find_map(|ev| match ev {
                StreamEvent::MessageDelta { stop_reason, .. } => *stop_reason,
                _ => None,
            })
            .expect("MessageDelta with stop_reason expected");
        assert_eq!(sr, StopReason::MaxTokens);
    }

    #[tokio::test]
    async fn finish_reason_content_filter_errors() {
        // content_filter must surface as a hard error. Silently mapping to
        // EndTurn would leave the user staring at an empty-looking response
        // without knowing their prompt was refused.
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":\"content_filter\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let errs: Vec<_> = collect(stream_of(vec![body.as_bytes()]))
            .await
            .into_iter()
            .filter_map(Result::err)
            .collect();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ProviderError::BadRequest(m) if m.contains("content_filter"))),
            "expected BadRequest carrying content_filter, got {errs:?}"
        );
    }

    #[tokio::test]
    async fn finish_reason_eos_maps_to_end_turn() {
        // llama.cpp and a few MLX shims emit `"eos"` instead of `"stop"`.
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"eos\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let evs: Vec<_> = collect(stream_of(vec![body.as_bytes()]))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        let sr = evs.iter().find_map(|ev| match ev {
            StreamEvent::MessageDelta { stop_reason, .. } => *stop_reason,
            _ => None,
        });
        assert_eq!(sr, Some(StopReason::EndTurn));
    }

    #[tokio::test]
    async fn finish_reason_unknown_warns_but_ends() {
        // Unknown values fall through to EndTurn — keep the loop unstuck.
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"weird_unknown_value\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let evs: Vec<_> = collect(stream_of(vec![body.as_bytes()]))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        let sr = evs.iter().find_map(|ev| match ev {
            StreamEvent::MessageDelta { stop_reason, .. } => *stop_reason,
            _ => None,
        });
        assert_eq!(sr, Some(StopReason::EndTurn));
    }

    // ---------------------------------------------------------------------
    // Local-LLM text-based tool-call fallback
    //
    // Gated behind `StreamCfg.text_toolcall_fallback` (env:
    // `HARNESS_OPENAI_TEXT_TOOLCALL=1`). Small local models (qwen2.5-coder:7b,
    // gemma:7b at low quant) regularly emit tool calls as prose with a
    // `<tool:Name>{...}</tool>` envelope instead of an OpenAI `tool_calls`
    // JSON field. The fallback buffers text until stream end, extracts the
    // first envelope, and synthesizes the Anthropic-shaped event trio
    // (ContentBlockStart ToolUse → InputJson → ContentBlockStop) so the
    // turn loop runs the tool just like on a native tool_calls response.
    // ---------------------------------------------------------------------

    #[tokio::test]
    async fn text_toolcall_fallback_synthesizes_tool_use_events() {
        // Model streams plain text with a tool envelope. With fallback on we
        // expect: MessageStart → ContentBlockStart(ToolUse) → InputJson →
        // ContentBlockStop → MessageDelta(ToolUse) → MessageStop. No raw text
        // block because after stripping the envelope the residual is empty.
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"<tool:Bash>\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"{\\\"cmd\\\":\\\"ls\\\",\\\"mode\\\":\\\"Argv\\\"}\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"</tool>\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let cfg = StreamCfg {
            text_toolcall_fallback: true,
        };
        let evs: Vec<_> = collect(stream_of_with_cfg(vec![body.as_bytes()], cfg))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();

        // MessageStart first.
        assert!(matches!(evs[0], StreamEvent::MessageStart { .. }));

        // Find the synthesized ToolUse ContentBlockStart.
        let tool_start = evs
            .iter()
            .find_map(|e| match e {
                StreamEvent::ContentBlockStart {
                    index,
                    block: ContentBlockHeader::ToolUse { id, name },
                } => Some((*index, id.clone(), name.clone())),
                _ => None,
            })
            .expect("synthesized ToolUse ContentBlockStart missing");
        assert_eq!(tool_start.2, "Bash", "tool name captured from <tool:Bash>");
        assert!(
            tool_start.1.starts_with("call_local_"),
            "synthetic id should be recognizable, got {}",
            tool_start.1
        );
        let tool_idx = tool_start.0;

        // InputJson delta must carry the full JSON object byte-for-byte.
        let json_bytes: Vec<u8> = evs
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockDelta {
                    index,
                    delta: ContentDelta::InputJson(b),
                } if *index == tool_idx => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(
            std::str::from_utf8(&json_bytes).unwrap(),
            r#"{"cmd":"ls","mode":"Argv"}"#
        );

        // ContentBlockStop for the tool block.
        assert!(
            evs.iter().any(
                |e| matches!(e, StreamEvent::ContentBlockStop { index } if *index == tool_idx)
            ),
            "synthesized ToolUse ContentBlockStop missing"
        );

        // stop_reason must be upgraded to ToolUse even though the model said "stop".
        let sr = evs
            .iter()
            .find_map(|ev| match ev {
                StreamEvent::MessageDelta { stop_reason, .. } => *stop_reason,
                _ => None,
            })
            .expect("MessageDelta missing");
        assert_eq!(
            sr,
            StopReason::ToolUse,
            "stop_reason must be upgraded to ToolUse so the turn loop runs the tool"
        );

        // No text block was emitted (residual was empty after envelope strip).
        assert!(
            !evs.iter().any(|e| matches!(
                e,
                StreamEvent::ContentBlockStart {
                    block: ContentBlockHeader::Text,
                    ..
                }
            )),
            "unexpected text block for envelope-only stream"
        );

        assert!(matches!(evs.last(), Some(StreamEvent::MessageStop)));
    }

    #[tokio::test]
    async fn text_toolcall_fallback_passes_through_plain_text() {
        // No tool envelope → plain text must come out as one Text block.
        // stop_reason stays EndTurn; no ToolUse synthesis.
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"hello \"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"world\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let cfg = StreamCfg {
            text_toolcall_fallback: true,
        };
        let evs: Vec<_> = collect(stream_of_with_cfg(vec![body.as_bytes()], cfg))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();

        // A single text block with the concatenated content.
        let text: String = evs
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockDelta {
                    delta: ContentDelta::Text(s),
                    ..
                } => Some(s.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(text, "hello world");

        // No tool-use block.
        assert!(
            !evs.iter().any(|e| matches!(
                e,
                StreamEvent::ContentBlockStart {
                    block: ContentBlockHeader::ToolUse { .. },
                    ..
                }
            )),
            "false positive: synthesized a tool_use from plain prose"
        );

        // stop_reason preserved as EndTurn.
        let sr = evs.iter().find_map(|ev| match ev {
            StreamEvent::MessageDelta { stop_reason, .. } => *stop_reason,
            _ => None,
        });
        assert_eq!(sr, Some(StopReason::EndTurn));
    }

    #[tokio::test]
    async fn text_toolcall_fallback_preserves_surrounding_prose() {
        // Model emits prose around the tool envelope:
        //   "I'll check the files. <tool:Bash>{"cmd":"ls"}</tool> done."
        // Expected: the prose survives (minus the envelope), the tool gets
        // synthesized, stop_reason upgraded to ToolUse.
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"I'll check the files. <tool:Bash>{\\\"cmd\\\":\\\"ls\\\"}</tool> done.\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let cfg = StreamCfg {
            text_toolcall_fallback: true,
        };
        let evs: Vec<_> = collect(stream_of_with_cfg(vec![body.as_bytes()], cfg))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();

        // Residual prose is present and carries *both* the leading and
        // trailing snippets, envelope-free.
        let text: String = evs
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockDelta {
                    delta: ContentDelta::Text(s),
                    ..
                } => Some(s.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(
            text.contains("I'll check the files."),
            "leading prose lost: {text:?}"
        );
        assert!(text.contains("done."), "trailing prose lost: {text:?}");
        assert!(
            !text.contains("<tool:") && !text.contains("</tool>"),
            "envelope leaked into text block: {text:?}"
        );

        // Synthesized tool_use is present with captured args.
        let saw_tool = evs.iter().any(|e| {
            matches!(
                e,
                StreamEvent::ContentBlockStart {
                    block: ContentBlockHeader::ToolUse { name, .. },
                    ..
                } if name == "Bash"
            )
        });
        assert!(saw_tool, "synthesized ToolUse missing");

        let sr = evs.iter().find_map(|ev| match ev {
            StreamEvent::MessageDelta { stop_reason, .. } => *stop_reason,
            _ => None,
        });
        assert_eq!(sr, Some(StopReason::ToolUse));
    }

    #[tokio::test]
    async fn text_toolcall_fallback_malformed_json_falls_through_as_text() {
        // Envelope tags match but the inner payload isn't valid JSON.
        // We must NOT crash, and the entire raw text (envelope included)
        // must be passed through as text. The engine will then surface
        // the model's mistake so the user can retry.
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"<tool:Bash>{not json}</tool>\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let cfg = StreamCfg {
            text_toolcall_fallback: true,
        };
        let evs: Vec<_> = collect(stream_of_with_cfg(vec![body.as_bytes()], cfg))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();

        // No tool_use synthesized.
        assert!(
            !evs.iter().any(|e| matches!(
                e,
                StreamEvent::ContentBlockStart {
                    block: ContentBlockHeader::ToolUse { .. },
                    ..
                }
            )),
            "synthesized a tool_use despite malformed inner JSON"
        );

        // Raw text is emitted (envelope included — the point of the
        // fallthrough is visibility).
        let text: String = evs
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ContentBlockDelta {
                    delta: ContentDelta::Text(s),
                    ..
                } => Some(s.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("<tool:Bash>"));
        assert!(text.contains("</tool>"));

        // stop_reason stays EndTurn — no tool to run.
        let sr = evs.iter().find_map(|ev| match ev {
            StreamEvent::MessageDelta { stop_reason, .. } => *stop_reason,
            _ => None,
        });
        assert_eq!(sr, Some(StopReason::EndTurn));
    }

    #[test]
    fn stream_cfg_from_env_defaults_off_when_unset() {
        // Smoke test that we don't read env in a weird way. Mutating env from
        // a test is a race with other parallel tests in the crate (both
        // `new_requires_env_var` and `new_with_base_url_*` key off
        // `OPENAI_API_KEY`), so we keep this path constant-expression only.
        // Truthy-value parsing is exercised indirectly through the stream
        // tests above, which use the explicit `StreamCfg` struct.
        let cfg = StreamCfg::default();
        assert!(!cfg.text_toolcall_fallback);
    }

    #[test]
    fn max_tokens_respects_request() {
        // Non-zero request value passes through verbatim — the engine's
        // caller-supplied budget is authoritative.
        let msgs = vec![Message::user("hi")];
        let body = build_request_body(
            "gpt-4o",
            &StreamRequest {
                system: "",
                messages: &msgs,
                tools: &[],
                max_tokens: 4096,
            },
        );
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn max_tokens_defaults_to_conservative_value_when_zero() {
        // Zero (or missing) falls back to DEFAULT_MAX_TOKENS. The literal
        // value matters — it must be small enough to fit Ollama's default
        // 4096-token context window even with prompt tokens stacked on top.
        let msgs = vec![Message::user("hi")];
        let body = build_request_body(
            "gpt-4o",
            &StreamRequest {
                system: "",
                messages: &msgs,
                tools: &[],
                max_tokens: 0,
            },
        );
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        // Compile-time invariant — DEFAULT_MAX_TOKENS must fit inside
        // Ollama's stock num_ctx (4096) even with some prompt budget on top.
        // A const_assert keeps this cheap; runtime check would be optimized
        // out by the compiler (clippy flags it) since both sides are consts.
        const _: () = assert!(DEFAULT_MAX_TOKENS <= 4096);
    }

    #[tokio::test]
    async fn tool_call_index_optional_deserializes() {
        // Ollama and some llama.cpp `--jinja` templates omit
        // `tool_calls[i].index`. Our deserializer must tolerate that and the
        // stream must still translate into a well-formed ToolUse block. Using
        // two separate chunks without `index` — the fallback uses each
        // chunk's array-position (0), so both fragments attach to the same
        // tool_call which is the correct behavior for a single call.
        let body = concat!(
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"id\":\"call_no_idx\",\"type\":\"function\",\"function\":{\"name\":\"Read\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"arguments\":\"{\\\"f\\\":1}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let evs: Vec<_> = collect(stream_of(vec![body.as_bytes()]))
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect();
        // ContentBlockStart with id+name must have been emitted.
        let saw_start = evs.iter().any(|e| {
            matches!(
                e,
                StreamEvent::ContentBlockStart {
                    block: ContentBlockHeader::ToolUse { id, name },
                    ..
                } if id == "call_no_idx" && name == "Read"
            )
        });
        assert!(saw_start, "missing ToolUse ContentBlockStart");
        // And the argument JSON must have been forwarded.
        let mut args = Vec::<u8>::new();
        for ev in &evs {
            if let StreamEvent::ContentBlockDelta {
                delta: ContentDelta::InputJson(b),
                ..
            } = ev
            {
                args.extend_from_slice(b);
            }
        }
        assert_eq!(std::str::from_utf8(&args).unwrap(), "{\"f\":1}");
        // ...and the stop_reason maps correctly.
        let sr = evs.iter().find_map(|ev| match ev {
            StreamEvent::MessageDelta { stop_reason, .. } => *stop_reason,
            _ => None,
        });
        assert_eq!(sr, Some(StopReason::ToolUse));
    }

    #[test]
    fn chunk_tool_call_deserializes_without_index_field() {
        // Belt-and-suspenders: the raw serde path itself must accept
        // tool_calls entries missing `index` without erroring. Guards against
        // a future change that reverts `index` back to a required field.
        let json = r#"{"id":"c1","choices":[{"delta":{"tool_calls":[
            {"id":"t1","function":{"name":"X","arguments":"{}"}}
        ]},"finish_reason":null}]}"#;
        let chunk: ChatCompletionChunk = serde_json::from_str(json).expect("must deserialize");
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert!(tc.index.is_none());
        assert_eq!(tc.id.as_deref(), Some("t1"));
    }

    #[test]
    fn http_errors_classified() {
        assert!(matches!(
            classify_http_error(401, "bad key", None),
            ProviderError::Auth(_)
        ));
        assert!(matches!(
            classify_http_error(429, "slow", Some(Duration::from_secs(5))),
            ProviderError::RateLimit(Some(d)) if d.as_secs() == 5
        ));
        assert!(matches!(
            classify_http_error(500, "", None),
            ProviderError::Server(500)
        ));
        assert!(matches!(
            classify_http_error(400, "bad input", None),
            ProviderError::BadRequest(_)
        ));
        // 5xx returned as Server(status).
        assert!(matches!(
            classify_http_error(503, "maintenance", None),
            ProviderError::Server(503)
        ));
    }

    #[tokio::test]
    async fn mid_stream_eof_is_stream_dropped() {
        // Half a frame with no terminator.
        let body = "data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hi";
        let evs = collect(stream_of(vec![body.as_bytes()])).await;
        let err = evs
            .into_iter()
            .find_map(|r| r.err())
            .expect("expected a ProviderError");
        assert!(matches!(err, ProviderError::StreamDropped));
    }

    #[tokio::test]
    async fn invalid_chunk_json_is_parse_error() {
        let body = "data: {not-json\n\n";
        let evs = collect(stream_of(vec![body.as_bytes()])).await;
        let err = evs
            .into_iter()
            .find_map(|r| r.err())
            .expect("expected a ProviderError");
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn new_requires_env_var() {
        // Tests run with env not guaranteed; ensure the "missing key" path
        // surfaces Auth cleanly. We can't mutate env safely in parallel tests,
        // so only check when unset. When set, skip. Also: `OPENAI_BASE_URL`
        // must be unset (or point to a non-local URL) for this path, since a
        // localhost URL would short-circuit the Auth check.
        if std::env::var("OPENAI_API_KEY").is_err() && std::env::var("OPENAI_BASE_URL").is_err() {
            let err = OpenAIProvider::new("m").unwrap_err();
            assert!(matches!(err, ProviderError::Auth(_)));
        }
    }

    #[test]
    fn is_local_url_recognizes_loopback() {
        for url in [
            "http://localhost:11434/v1",
            "http://localhost/",
            "http://127.0.0.1:8000/v1",
            "http://[::1]:1234/v1",
            "https://localhost:8443",
        ] {
            assert!(
                is_local_url(&Url::parse(url).unwrap()),
                "expected local: {url}"
            );
        }
        for url in [
            "https://api.openai.com",
            "https://example.com:11434/v1",
            "http://10.0.0.1:8000",
            "http://host.docker.internal:11434/v1",
        ] {
            assert!(
                !is_local_url(&Url::parse(url).unwrap()),
                "expected non-local: {url}"
            );
        }
    }

    #[test]
    fn new_with_base_url_localhost_succeeds_without_api_key() {
        // Covers the main local-LLM entry point: `new_with_base_url(model,
        // Some(localhost_url))` must succeed even with OPENAI_API_KEY unset,
        // because `resolve_api_key` substitutes a placeholder bearer for
        // loopback URLs. Skipped when env has OPENAI_API_KEY already set
        // (parallel test safety — we can't mutate process env here).
        if std::env::var("OPENAI_API_KEY").is_err() {
            let localhost = Url::parse("http://localhost:11434/v1").unwrap();
            let p = OpenAIProvider::new_with_base_url("qwen2.5", Some(localhost))
                .expect("localhost URL must allow missing OPENAI_API_KEY");
            assert_eq!(
                p.chat_completions_url().unwrap().as_str(),
                "http://localhost:11434/v1/chat/completions"
            );
        }
    }

    #[test]
    fn new_with_base_url_remote_requires_api_key() {
        // Mirror of the above: for a non-loopback URL, missing key must
        // still surface as ProviderError::Auth. Skip when env supplies a
        // key (same parallel-safety caveat).
        if std::env::var("OPENAI_API_KEY").is_err() {
            let remote = Url::parse("https://api.openai.com").unwrap();
            let err = OpenAIProvider::new_with_base_url("gpt-4o", Some(remote)).unwrap_err();
            assert!(matches!(err, ProviderError::Auth(_)));
        }
    }

    // ---- End-to-end against an in-process mock OpenAI-compatible server ----
    //
    // Spins up a raw tokio::net::TcpListener, reads one HTTP request, writes
    // an SSE-framed ChatCompletion stream, and drives OpenAIProvider::stream
    // through it. Verifies URL joining, Authorization header, tool-call
    // translation, and final stop_reason — the whole wire path that
    // `cargo test -p harness-provider --test openai_stream` would cover.
    mod mock_server_e2e {
        use super::*;
        use harness_core::StreamEvent as SE;
        use harness_proto::Message;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        async fn serve_once(
            body: &'static [u8],
            captured_req: std::sync::Arc<tokio::sync::Mutex<Vec<u8>>>,
        ) -> u16 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                // Read until we see the end of the request (CRLF-CRLF then
                // consume declared Content-Length). Keep it cheap — we just
                // need enough to assert on.
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
                        let have_body = total.len() - (hdr_end + 4);
                        if have_body >= cl {
                            break;
                        }
                    }
                }
                *captured_req.lock().await = total;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
                sock.write_all(body).await.unwrap();
                sock.flush().await.unwrap();
            });
            port
        }

        #[tokio::test]
        async fn stream_end_to_end_against_mock() {
            let sse = b"data: {\"id\":\"c1\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"c1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
            let port = serve_once(sse, captured.clone()).await;

            let url = Url::parse(&format!("http://127.0.0.1:{port}")).unwrap();
            let provider = OpenAIProvider::with_config(
                "qwen2.5",
                SecretString::from("local".to_string()),
                url,
            )
            .unwrap();

            let msgs = vec![Message::user("hi")];
            let req = StreamRequest {
                system: "",
                messages: &msgs,
                tools: &[],
                max_tokens: 64,
            };
            let mut s = provider.stream(req).await.expect("stream open");
            let mut evs = Vec::new();
            while let Some(e) = s.next().await {
                evs.push(e.unwrap());
            }

            // Validate event shape.
            assert!(matches!(evs.first(), Some(SE::MessageStart { .. })));
            let saw_text = evs.iter().any(|e| {
                matches!(e,
                SE::ContentBlockDelta { delta: ContentDelta::Text(s), .. } if s == "hi")
            });
            assert!(saw_text, "expected text delta 'hi'");
            assert!(matches!(evs.last(), Some(SE::MessageStop)));

            // Validate the wire request: POST to /v1/chat/completions with
            // Authorization: Bearer local and a JSON body referencing the model.
            let req_bytes = captured.lock().await.clone();
            let req_str = String::from_utf8_lossy(&req_bytes);
            assert!(
                req_str.starts_with("POST /v1/chat/completions"),
                "bad request line: {req_str}"
            );
            let lower = req_str.to_ascii_lowercase();
            assert!(lower.contains("authorization: bearer local"));
            assert!(req_str.contains("\"model\":\"qwen2.5\""));
            assert!(req_str.contains("\"stream\":true"));
        }

        #[tokio::test]
        async fn auth_error_classifies_401() {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let body = b"{\"error\":\"bad key\"}";
                let hdr = format!(
                    "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                sock.write_all(hdr.as_bytes()).await.unwrap();
                sock.write_all(body).await.unwrap();
                sock.flush().await.unwrap();
            });
            let p = OpenAIProvider::with_config(
                "m",
                SecretString::from("bad".to_string()),
                Url::parse(&format!("http://127.0.0.1:{port}")).unwrap(),
            )
            .unwrap();
            let req = StreamRequest {
                system: "",
                messages: &[],
                tools: &[],
                max_tokens: 8,
            };
            let err = match p.stream(req).await {
                Ok(_) => panic!("expected 401 to surface as ProviderError::Auth"),
                Err(e) => e,
            };
            assert!(matches!(err, ProviderError::Auth(_)));
        }

        // --- Egress redaction ----------------------------------------------
        //
        // Confirms the security property from `docs/security/egress-redaction.md`:
        // when `with_redact_egress(true)`, a fake secret embedded in a
        // `ToolResult.content` must NOT appear in the outbound HTTP body that
        // the provider sends upstream. With the flag off (the default), the
        // raw secret goes through verbatim — this is the expected behavior
        // so the model can thread tokens through tools. Both halves are
        // asserted to make the contrast explicit and guard against drift.
        const EGRESS_FAKE_SECRET: &str = "sk-ant-api03-abcdefghij1234567890XYZ";

        fn tool_result_message() -> Message {
            Message::user_tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "tid".into(),
                content: format!("printed {EGRESS_FAKE_SECRET} from env"),
                is_error: false,
                cache_control: None,
            }])
        }

        async fn capture_request_body(provider: OpenAIProvider) -> String {
            // Minimal well-formed SSE response so the client doesn't error.
            let sse = b"data: [DONE]\n\n";
            let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<u8>::new()));
            let port = serve_once(sse, captured.clone()).await;
            // Rebind base URL to the mock port (with_config path).
            let url = Url::parse(&format!("http://127.0.0.1:{port}")).unwrap();
            let provider = OpenAIProvider::with_config(
                provider.model().to_string(),
                SecretString::from("local".to_string()),
                url,
            )
            .unwrap()
            .with_redact_egress(provider.redact_egress_enabled());

            let msgs = vec![tool_result_message()];
            let req = StreamRequest {
                system: "",
                messages: &msgs,
                tools: &[],
                max_tokens: 64,
            };
            let mut s = provider.stream(req).await.expect("stream open");
            while s.next().await.is_some() {}
            let bytes = captured.lock().await.clone();
            String::from_utf8_lossy(&bytes).into_owned()
        }

        #[tokio::test]
        async fn egress_redaction_off_by_default_leaks_secret_to_provider() {
            // A built-via-with_config provider has `redact_egress: false`.
            let p = OpenAIProvider::with_config(
                "qwen2.5",
                SecretString::from("local".to_string()),
                Url::parse("http://127.0.0.1:1").unwrap(),
            )
            .unwrap();
            assert!(!p.redact_egress_enabled(), "default must be off");

            let req_str = capture_request_body(p).await;
            assert!(
                req_str.contains(EGRESS_FAKE_SECRET),
                "default-off must pass raw tool_result content through to the provider"
            );
        }

        #[tokio::test]
        async fn egress_redaction_on_scrubs_secret_in_outbound_body() {
            let p = OpenAIProvider::with_config(
                "qwen2.5",
                SecretString::from("local".to_string()),
                Url::parse("http://127.0.0.1:1").unwrap(),
            )
            .unwrap()
            .with_redact_egress(true);
            assert!(p.redact_egress_enabled());

            let req_str = capture_request_body(p).await;
            assert!(
                !req_str.contains(EGRESS_FAKE_SECRET),
                "secret leaked into outbound HTTP body despite redact_egress=true: {req_str}"
            );
            assert!(
                req_str.contains("[REDACTED:sk]"),
                "missing redaction marker in outbound body: {req_str}"
            );
        }
    }
}
