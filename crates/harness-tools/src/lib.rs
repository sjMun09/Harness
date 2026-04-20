//! harness-tools — MVP 6 tools (PLAN §3.1) + path safety + proc helpers.

#![forbid(unsafe_code)]

pub mod fs_safe;
pub mod proc;

mod bash;
mod edit;
mod glob;
mod grep;
mod read;
mod write;

pub use bash::{BashTool, DEFAULT_ENV_ALLOW};
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use write::WriteTool;

use std::sync::Arc;

use harness_core::Tool;

/// Registry returning every MVP tool. Order is stable and matches the
/// display order used by line-mode rendering (`⏺ Tool(args)`).
#[must_use]
pub fn all_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadTool::default()),
        Arc::new(WriteTool::default()),
        Arc::new(EditTool::default()),
        Arc::new(BashTool::default()),
        Arc::new(GlobTool::default()),
        Arc::new(GrepTool::default()),
    ]
}
