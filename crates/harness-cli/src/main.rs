//! `harness` — CLI entry. clap v4 derive with the `cargo` feature OFF so
//! `--help` stays under the 20ms target (PLAN §5.7, §3.1 exit criteria).

#![forbid(unsafe_code)]

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "harness",
    bin_name = "harness",
    version,
    about = "Harness — Rust coding-agent harness",
    disable_version_flag = false
)]
struct Cli {
    /// Override model. Also read from `HARNESS_MODEL` env, `settings.json.model`.
    #[arg(long, global = true)]
    model: Option<String>,

    /// Verbose logging. Enables DEBUG tracing; shows warning banner (§8.2).
    #[arg(long, short = 'v', global = true)]
    verbose: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run an agent turn loop on the given prompt.
    Ask {
        /// Prompt text. Quote to include spaces.
        prompt: String,
    },
    /// Session management.
    #[command(subcommand)]
    Session(SessionCmd),
    /// Config management (settings.json).
    #[command(subcommand)]
    Config(ConfigCmd),
}

#[derive(Subcommand, Debug)]
enum SessionCmd {
    /// List known sessions under `$XDG_STATE_HOME/harness/sessions/`.
    List,
    /// Resume a session by id.
    Resume { id: String },
    /// Show session metadata + transcript head.
    Show { id: String },
}

#[derive(Subcommand, Debug)]
enum ConfigCmd {
    /// Import Claude Code settings (downgrades all `allow` → `ask`, §8.2).
    Import,
    /// Print resolved settings (after precedence merge, §5.7).
    Show,
    /// Print the settings.json path that would be written.
    Path,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.cmd {
        Cmd::Ask { prompt } => cmd_ask(prompt, cli.model).await,
        Cmd::Session(s) => match s {
            SessionCmd::List => cmd_session_list().await,
            SessionCmd::Resume { id } => cmd_session_resume(id).await,
            SessionCmd::Show { id } => cmd_session_show(id).await,
        },
        Cmd::Config(c) => match c {
            ConfigCmd::Import => cmd_config_import().await,
            ConfigCmd::Show => cmd_config_show().await,
            ConfigCmd::Path => cmd_config_path().await,
        },
    }
}

fn init_tracing(_verbose: bool) {
    // Iter 1 body: tracing_subscriber with EnvFilter + redaction layer (§8.2).
}

async fn cmd_ask(_prompt: String, _model_override: Option<String>) -> anyhow::Result<()> {
    anyhow::bail!("harness ask not yet implemented")
}
async fn cmd_session_list() -> anyhow::Result<()> {
    anyhow::bail!("harness session list not yet implemented")
}
async fn cmd_session_resume(_id: String) -> anyhow::Result<()> {
    anyhow::bail!("harness session resume not yet implemented")
}
async fn cmd_session_show(_id: String) -> anyhow::Result<()> {
    anyhow::bail!("harness session show not yet implemented")
}
async fn cmd_config_import() -> anyhow::Result<()> {
    anyhow::bail!("harness config import not yet implemented")
}
async fn cmd_config_show() -> anyhow::Result<()> {
    anyhow::bail!("harness config show not yet implemented")
}
async fn cmd_config_path() -> anyhow::Result<()> {
    anyhow::bail!("harness config path not yet implemented")
}

// Keep unused internal crates linked so the workspace graph is exercised by
// `cargo check` on the binary. These will be consumed in iter 1.
#[allow(dead_code)]
fn _link_check() {
    let _ = harness_core::config::load;
    let _ = harness_tools::all_tools;
    let _ = harness_provider::AnthropicProvider::with_default_model;
    let _ = harness_mem::sessions_dir;
    let _: harness_token::NullEstimator = harness_token::NullEstimator;
    let _: harness_perm::PermissionSnapshot = harness_perm::PermissionSnapshot::default();
    let _: Option<&harness_proto::Role> = None;
}
