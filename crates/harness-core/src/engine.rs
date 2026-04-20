//! Turn loop driver. PLAN §2.2.
//!
//! Stub: concrete `run_turn` body lands with iter 1. Keeps types in place so
//! downstream crates can depend on this module without churn.

use std::sync::Arc;

use harness_proto::Message;

use crate::provider::Provider;
use crate::tool::{Tool, ToolCtx};

/// Driver inputs wired by the CLI.
pub struct EngineInputs {
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub system: String,
    pub ctx: ToolCtx,
    pub max_turns: u32,
}

impl std::fmt::Debug for EngineInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineInputs")
            .field("tool_count", &self.tools.len())
            .field("system_len", &self.system.len())
            .field("max_turns", &self.max_turns)
            .finish()
    }
}

/// Run the turn loop. §2.2 pseudo-code is the reference implementation.
pub async fn run_turn(
    _inputs: EngineInputs,
    _initial: Vec<Message>,
) -> Result<Vec<Message>, anyhow::Error> {
    anyhow::bail!("harness-core::engine::run_turn not yet implemented")
}
