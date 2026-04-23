//! `harness models` — print the naming conventions harness recognizes.
//!
//! Static text only; no network calls. The point is to answer "what string
//! do I put after `--model`?" without the user having to grep `main.rs`.
//!
//! Keep in sync with:
//! - `main.rs::is_openai_model` (the OpenAI-prefix routing rule).
//! - the Anthropic ID list from the current system prompt's known-models
//!   section.

/// Implementation of `harness models`.
///
/// Prints a short menu of the model names + aliases harness currently
/// understands, along with a copy-pasteable invocation for each. Returns
/// `Ok(())` unconditionally — this is a help-style command.
pub fn cmd_models() {
    println!("Harness — supported model naming");
    println!();
    println!("Anthropic (native):");
    println!("  claude-opus-4-7             # default; strongest reasoning");
    println!("  claude-sonnet-4-6           # balanced");
    println!("  claude-haiku-4-5-20251001   # fast / cheap");
    println!();
    println!("OpenAI-compatible (any of these prefixes routes to the OpenAI provider):");
    println!("  gpt-*          e.g. gpt-4o, gpt-4o-mini");
    println!("  o1, o1-*       e.g. o1, o1-preview, o1-mini");
    println!("  o3, o3-*");
    println!("  o4, o4-*");
    println!("  openai/<id>    explicit prefix; the `openai/` is stripped before");
    println!("                 sending, so the server sees just `<id>`.");
    println!();
    println!("Examples:");
    println!();
    println!("  # Anthropic API key (default auto-detect):");
    println!("  harness ask \"explain this repo\"");
    println!();
    println!("  # Claude Code OAuth (if built with --features claude-code-oauth):");
    println!("  harness --auth oauth ask \"..\"");
    println!();
    println!("  # OpenAI:");
    println!("  harness --model gpt-4o ask \"..\"");
    println!();
    println!("  # Local LLM via Ollama (three-flag combo):");
    println!("  harness \\");
    println!("    --model openai/qwen2.5-coder:14b \\");
    println!("    --base-url http://localhost:11434/v1 \\");
    println!("    ask \"..\"");
    println!();
    println!(
        "See `harness doctor` for a runtime check of your auth / settings, and `docs/local-llm/` for local runtime setup."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_models_does_not_panic() {
        // Bare minimum: the command prints static text without panicking.
        // We don't capture stdout here; just confirm the call returns.
        cmd_models();
    }
}
