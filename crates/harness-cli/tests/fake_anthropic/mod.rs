//! Minimal fake Anthropic Messages API server used by `e2e_ask.rs`.
//!
//! Just enough to satisfy `AnthropicProvider::stream`:
//!   * accepts `POST /v1/messages`,
//!   * reads (and discards) the request body,
//!   * writes a scripted SSE response for each POST and closes the stream.
//!
//! Implementation detail: we speak HTTP/1.1 by hand over `tokio::net::TcpListener`
//! so there is no extra HTTP-framework dependency to pull in. This is tiny —
//! ~60 lines of protocol glue. `reqwest` is content with any `200 OK` response
//! that advertises `content-type: text/event-stream` and sends the body with
//! `Transfer-Encoding: chunked` or `Connection: close`. We use `Connection:
//! close` + one body write for simplicity.

#![allow(dead_code)] // individual tests only use a subset of helpers.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// Script for one scripted turn. Each `Script` maps to a single
/// `POST /v1/messages` response body.
#[derive(Debug, Clone)]
pub struct Script {
    /// Raw SSE body bytes (already framed with `event:` / `data:` lines).
    pub body: String,
}

impl Script {
    /// Emit a single text block `"<text>"` followed by `end_turn`.
    pub fn text_only(text: &str) -> Self {
        // IMPORTANT: keep `text` as a literal JSON value. Callers pass short
        // fixed strings so naive escaping is fine.
        let text_json = serde_json::Value::String(text.to_string()).to_string();
        let body = format!(
            "event: message_start\n\
             data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_01\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n\
             event: content_block_start\n\
             data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
             event: content_block_delta\n\
             data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{text_json}}}}}\n\n\
             event: content_block_stop\n\
             data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
             event: message_delta\n\
             data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\",\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":0,\"output_tokens\":5}}}}\n\n\
             event: message_stop\n\
             data: {{\"type\":\"message_stop\"}}\n\n"
        );
        Self { body }
    }

    /// Emit a single `tool_use` block with the given `id`/`name`/`input` JSON
    /// payload, followed by `stop_reason: tool_use`.
    ///
    /// `input_json` must be a complete JSON object literal (e.g.
    /// `r#"{"file_path":"/tmp/a"}"#`). It is streamed back as a single
    /// `input_json_delta` fragment.
    pub fn tool_use(id: &str, name: &str, input_json: &str) -> Self {
        let id_json = serde_json::Value::String(id.to_string()).to_string();
        let name_json = serde_json::Value::String(name.to_string()).to_string();
        // Wrap the raw JSON literal as a JSON string for `partial_json`.
        let partial_json = serde_json::Value::String(input_json.to_string()).to_string();
        let body = format!(
            "event: message_start\n\
             data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_02\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-opus-4-7\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n\
             event: content_block_start\n\
             data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":{id_json},\"name\":{name_json},\"input\":{{}}}}}}\n\n\
             event: content_block_delta\n\
             data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{partial_json}}}}}\n\n\
             event: content_block_stop\n\
             data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
             event: message_delta\n\
             data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\",\"stop_sequence\":null}},\"usage\":{{\"input_tokens\":0,\"output_tokens\":8}}}}\n\n\
             event: message_stop\n\
             data: {{\"type\":\"message_stop\"}}\n\n"
        );
        Self { body }
    }
}

/// Handle to a running fake server.
pub struct FakeServer {
    shutdown: CancellationToken,
    joined: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl FakeServer {
    /// Spin up a server bound to 127.0.0.1:0 and return `(handle, addr)`.
    /// `scripts` are consumed one per POST, in order; further POSTs after
    /// the scripts run out get an empty 200 (which the provider will report
    /// as a dropped stream — but tests are designed so this never happens).
    pub async fn start(scripts: Vec<Script>) -> (Self, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let shutdown = CancellationToken::new();
        let shutdown_child = shutdown.clone();
        let scripts = Arc::new(Mutex::new(scripts.into_iter().collect::<Vec<_>>()));

        let joined = tokio::spawn(async move {
            loop {
                let (stream, _peer) = tokio::select! {
                    biased;
                    () = shutdown_child.cancelled() => return,
                    accept = listener.accept() => match accept {
                        Ok(ok) => ok,
                        Err(_) => continue,
                    },
                };
                let scripts = scripts.clone();
                let token = shutdown_child.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(
                        Duration::from_secs(60),
                        handle_conn(stream, scripts, token),
                    )
                    .await;
                });
            }
        });

        (
            Self {
                shutdown,
                joined: Arc::new(Mutex::new(Some(joined))),
            },
            addr,
        )
    }

    /// Signal the accept loop to exit and wait for it.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let mut guard = self.joined.lock().await;
        if let Some(h) = guard.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), h).await;
        }
    }
}

async fn handle_conn(
    mut stream: tokio::net::TcpStream,
    scripts: Arc<Mutex<Vec<Script>>>,
    _cancel: CancellationToken,
) {
    // Read request headers (up to \r\n\r\n), then best-effort drain the body.
    // The request parser here is intentionally permissive — we only care that
    // it's a POST; the body is discarded.
    let mut buf = vec![0u8; 8192];
    let mut acc = Vec::new();
    let mut headers_end = None;
    loop {
        match tokio::time::timeout(Duration::from_secs(10), stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                acc.extend_from_slice(&buf[..n]);
                if let Some(pos) = find_headers_end(&acc) {
                    headers_end = Some(pos);
                    break;
                }
                if acc.len() > 64 * 1024 {
                    break; // oversized
                }
            }
            _ => break,
        }
    }

    let Some(hend) = headers_end else {
        return;
    };
    let headers = &acc[..hend];
    let content_length = parse_content_length(headers);

    // Drain body up to content_length so reqwest sees a clean read.
    let already = acc.len().saturating_sub(hend + 4);
    let mut remaining = content_length.saturating_sub(already);
    while remaining > 0 {
        match tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => remaining = remaining.saturating_sub(n),
            _ => break,
        }
    }

    // Pick next script. If the list is empty, respond with 500 — the turn
    // loop will error out loudly rather than hang.
    let script = {
        let mut s = scripts.lock().await;
        if s.is_empty() {
            drop(s);
            let body = b"no more scripted responses";
            let resp = format!(
                "HTTP/1.1 500 Internal Server Error\r\n\
                 content-type: text/plain\r\n\
                 content-length: {}\r\n\
                 connection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes()).await;
            let _ = stream.write_all(body).await;
            let _ = stream.shutdown().await;
            return;
        }
        s.remove(0)
    };

    let body_bytes = script.body.as_bytes();
    let resp_head = format!(
        "HTTP/1.1 200 OK\r\n\
         content-type: text/event-stream\r\n\
         cache-control: no-cache\r\n\
         content-length: {}\r\n\
         connection: close\r\n\r\n",
        body_bytes.len()
    );
    let _ = stream.write_all(resp_head.as_bytes()).await;
    let _ = stream.write_all(body_bytes).await;
    let _ = stream.shutdown().await;
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_content_length(headers: &[u8]) -> usize {
    let text = match std::str::from_utf8(headers) {
        Ok(t) => t,
        Err(_) => return 0,
    };
    for line in text.split("\r\n") {
        if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            if let Ok(n) = rest.trim().parse::<usize>() {
                return n;
            }
        }
    }
    0
}
