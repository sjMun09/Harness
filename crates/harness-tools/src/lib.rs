//! harness-tools — MVP 6 tools (PLAN §3.1) + path safety + proc helpers.
//!
//! `unsafe` is denied by default. The syscall layer (`proc.rs`) opts in
//! per-function to invoke `Command::pre_exec` for setsid + PR_SET_PDEATHSIG
//! (PLAN §13: "unsafe는 syscall 레이어만").

#![deny(unsafe_code)]

pub mod bg_registry;
pub mod common;
pub mod fs_safe;
pub mod proc;
pub mod tx;

mod bash;
mod bash_output;
mod diff_exec;
mod edit;
mod glob;
mod grep;
mod import_trace;
mod kill_shell;
mod mybatis_parser;
mod read;
mod rollback;
mod subagent;
mod test_runner;
mod write;

pub use bash::{BashTool, DEFAULT_ENV_ALLOW};
pub use bash_output::BashOutputTool;
pub use diff_exec::DiffExecTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use import_trace::ImportTraceTool;
pub use kill_shell::KillShellTool;
pub use mybatis_parser::MyBatisDynamicParserTool;
pub use read::ReadTool;
pub use rollback::RollbackTool;
pub use subagent::SubagentTool;
pub use test_runner::TestTool;
pub use tx::Transaction;
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
        Arc::new(BashOutputTool::default()),
        Arc::new(KillShellTool::default()),
        Arc::new(GlobTool::default()),
        Arc::new(GrepTool::default()),
        Arc::new(ImportTraceTool::default()),
        Arc::new(MyBatisDynamicParserTool::default()),
        Arc::new(DiffExecTool::default()),
        Arc::new(TestTool::default()),
        Arc::new(RollbackTool::default()),
        Arc::new(SubagentTool::default()),
    ]
}
