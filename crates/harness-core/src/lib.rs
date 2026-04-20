//! Harness kernel: `Tool` + `Provider` traits, turn-loop state machine,
//! settings loader. PLAN §2.2 / §5.
//!
//! Consumers:
//!   - `harness-provider` implements `Provider`.
//!   - `harness-tools` implements `Tool` per tool.
//!   - `harness-cli` wires impls into `run_turn(..)`.

#![forbid(unsafe_code)]

pub mod compaction;
pub mod config;
pub mod engine;
pub mod hooks;
pub mod memory;
pub mod plan_gate;
pub mod provider;
pub mod subagent;
pub mod tool;
pub mod turn;
pub mod tx;

pub use provider::{
    ContentBlockHeader, ContentDelta, EventStream, Provider, ProviderError, StreamEvent,
    StreamRequest, ToolSpec,
};
pub use tool::{
    HookDispatcher, OutputChunk, Preview, StreamKind, Tool, ToolCtx, ToolError, ToolOutput,
};
pub use turn::{BlockState, FinalizeError};
