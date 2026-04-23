//! Tool trait + supporting types. PLAN §5.3 / §5.10.
//!
//! `ToolCtx: Clone + Send + 'static` is a hard requirement: the turn loop
//! moves ctx into each concurrent `dispatch()` future via `join_all`.

use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use harness_perm::PermissionSnapshot;
use harness_proto::SessionId;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub use crate::hooks::HookDispatcher;

/// User answer to an interactive permission prompt. PLAN §5.8.
/// The CLI binds `AskPrompt` to an implementation that reads stdin; headless
/// runs leave `ToolCtx::ask_prompt` as `None` and fall through to an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskAnswer {
    Yes,
    No,
    Always,
    DontAsk,
}

/// Hook consulted when permission evaluation returns `Decision::Ask`. The
/// engine calls `ask(tool, input)` and interprets the answer. Implementations
/// live in the CLI (TTY prompt) or test shims (canned answers).
pub trait AskPrompt: Send + Sync + std::fmt::Debug {
    fn ask(&self, tool: &str, input: &Value) -> AskAnswer;
}

/// A tool the model can call. Advertised to the provider via `schema()`,
/// dispatched by the turn loop per tool_use block.
#[async_trait]
pub trait Tool: Send + Sync {
    /// Stable identifier. Matches wire `tool_use.name`.
    fn name(&self) -> &str;

    /// One-sentence description shown to the model alongside the schema.
    /// Required — every Tool impl must provide something concrete so the model
    /// understands when to pick this tool over a sibling. Keep to a single
    /// short English sentence; detailed usage goes in the JSON Schema docs.
    fn description(&self) -> &'static str;

    /// JSON Schema of the `input` object. Sent verbatim to the provider.
    fn schema(&self) -> Value;

    /// Human-facing summary of a pending call. Pure + fast — called before
    /// permission check, potentially many times.
    fn preview(&self, input: &Value) -> Preview;

    /// Execute. Must honour `ctx.cancel` promptly.
    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError>;
}

/// Human-facing preview.
#[derive(Debug, Clone)]
pub struct Preview {
    pub summary_line: String,
    pub detail: Option<String>,
}

/// Output of a successful call. `summary` is wrapped into a `ToolResult`
/// and fed back to the model; `detail_path` points at a full log on disk.
pub struct ToolOutput {
    pub summary: String,
    pub detail_path: Option<PathBuf>,
    pub stream: Option<Pin<Box<dyn Stream<Item = OutputChunk> + Send + 'static>>>,
}

impl std::fmt::Debug for ToolOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolOutput")
            .field("summary", &self.summary)
            .field("detail_path", &self.detail_path)
            .field("stream", &self.stream.as_ref().map(|_| "<stream>"))
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct OutputChunk {
    pub ts: Instant,
    pub stream: StreamKind,
    pub bytes: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// Ambient context passed to every tool invocation.
///
/// INVARIANT: every field must be cheaply cloneable — the turn loop derives
/// a per-tool child context by cloning + swapping `cancel`.
#[derive(Clone, Debug)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub session_id: SessionId,
    pub cancel: CancellationToken,
    pub permission: PermissionSnapshot,
    pub hooks: HookDispatcher,
    /// Optional sub-turn host. `None` for tests and binaries that don't wire
    /// subagent support; `Some` only in top-level CLI runs.
    pub subagent: crate::subagent::OptHost,
    /// Current agent nesting depth. `0` = top-level user-facing turn.
    /// PLAN §5.4 bars depth > `SUBAGENT_MAX_DEPTH` from spawning further subagents.
    pub depth: u32,
    /// Optional multi-file rollback transaction (PLAN §3.2). `Some` when the
    /// top-level CLI has initialized staging; `None` for unit tests and
    /// binaries that opt out. Subagents inherit the parent's handle so every
    /// write lands in a single revert point.
    pub tx: crate::tx::OptTx,
    /// Optional interactive prompt for `Decision::Ask`. `None` for tests and
    /// headless runs — the engine surfaces Ask as a tool error when unset.
    pub ask_prompt: Option<std::sync::Arc<dyn AskPrompt>>,
}

impl ToolCtx {
    pub fn with_cancel(&self, cancel: CancellationToken) -> Self {
        Self {
            cancel,
            ..self.clone()
        }
    }
}

// Compile-time assertion of the Clone + Send + 'static requirement.
const _: fn() = || {
    fn assert_clone_send_static<T: Clone + Send + 'static>() {}
    assert_clone_send_static::<ToolCtx>();
};

/// Errors a tool may surface. Wrapped into `ToolResult { is_error: true }`
/// by the turn loop — never propagated past a single call.
#[derive(thiserror::Error, Debug)]
pub enum ToolError {
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("validation: {0}")]
    Validation(String),
    #[error("cancelled")]
    Cancelled,
    #[error("timeout after {0:?}")]
    Timeout(Duration),
    #[error("other: {0}")]
    Other(String),
}
