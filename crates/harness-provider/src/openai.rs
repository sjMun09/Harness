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
        log_init(&base_url, &model);

        Ok(Self {
            client,
            api_key,
            model,
            base_url,
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
        let client = build_http_client()?;
        let model = model.into();
        log_init(&base_url, &model);
        Ok(Self {
            client,
            api_key,
            model,
            base_url,
        })
    }

    #[inline]
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
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
fn log_init(base_url: &Url, model: &str) {
    tracing::info!(
        target: "harness_provider::openai",
        provider = "openai-compat",
        base_url = %base_url,
        model = model,
        local = is_local_url(base_url),
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
        let body = build_request_body(&self.model, &req);

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

        Ok(parse_stream(resp.bytes_stream()))
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
pub(crate) fn parse_stream<S>(inner: S) -> EventStream
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    Box::pin(SseStream {
        inner: Box::pin(inner),
        buf: BytesMut::with_capacity(16 * 1024),
        queue: VecDeque::new(),
        state: StreamState::default(),
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

// TODO(local-llm): text-based tool-call fallback.
// Many small local models (qwen2.5-coder:7b, gemma:7b) can't reliably emit
// `tool_calls` JSON — they revert to natural language like
// `I would run: Bash('ls')` or a `<tool>...</tool>` XML-ish envelope.
// A minimal fallback would: at `flush_terminal`, if no tool_call blocks were
// opened AND the text block matches a pattern like
//   `<tool:(\w+)>\s*(\{[\s\S]*?\})\s*</tool>`
// then synthesize a ContentBlockStart(ToolUse) + InputJson delta + Stop, and
// elide the matching substring from the text block. Non-trivial because the
// text block has already been streamed out as deltas; we'd need a buffering
// mode for local providers only. Deferred until tasks 1-5 are in production
// and we can validate the pattern against real model outputs rather than
// guessing. See docs/local-llm/README.md "툴콜링" section.

/// Called on `[DONE]` or on clean EOF after at least one chunk: emit
/// `ContentBlockStop` for each open block, then `MessageDelta` + `MessageStop`.
fn flush_terminal(state: &mut StreamState, queue: &mut VecDeque<StreamEvent>) {
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
        model: &'a str,
        system: &'a str,
        messages: &'a [Message],
        tools: &'a [ToolSpec],
    ) -> StreamRequest<'a> {
        StreamRequest {
            model,
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

    #[test]
    fn max_tokens_respects_request() {
        // Non-zero request value passes through verbatim — the engine's
        // caller-supplied budget is authoritative.
        let msgs = vec![Message::user("hi")];
        let body = build_request_body(
            "gpt-4o",
            &StreamRequest {
                model: "gpt-4o",
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
                model: "gpt-4o",
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
                model: "qwen2.5",
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
                model: "m",
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
    }
}
