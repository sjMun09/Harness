//! Integration tests for `harness config import-claude` logic.
//!
//! These tests call the pure `do_import` function directly to avoid any I/O
//! to actual Claude Code settings paths.
//!
//! Since harness-cli is a [[bin]] crate (no lib target), we reach the module
//! directly via `#[path]`.

use serde_json::json;

#[path = "../src/config_import.rs"]
mod config_import;

use config_import::do_import as run_import;
use harness_core::config::{DEFAULT_MODEL, SETTINGS_VERSION};

// ── helpers ─────────────────────────────────────────────────────────────────

fn make_claude_json(allow: &[&str], deny: &[&str], ask: &[&str]) -> serde_json::Value {
    json!({
        "permissions": {
            "allow": allow,
            "deny":  deny,
            "ask":   ask,
        }
    })
}

fn rule_sources(rules: &[harness_perm::Rule]) -> Vec<&str> {
    rules.iter().map(|r| r.source.as_str()).collect()
}

// ── test 1: only user settings ───────────────────────────────────────────────

#[test]
fn only_user_settings() {
    let user = make_claude_json(&["Read(**)", "Bash(git status)"], &[], &[]);
    let result = run_import(Some(&user), None).unwrap();

    // allow must be empty — all were downgraded
    assert!(
        result.settings.permissions.allow.is_empty(),
        "allow should be empty after downgrade"
    );
    // both original allow rules appear in ask
    let ask_sources = rule_sources(&result.settings.permissions.ask);
    assert!(ask_sources.contains(&"Read(**)"), "Read(**) should be in ask");
    assert!(
        ask_sources.contains(&"Bash(git status)"),
        "Bash(git status) should be in ask"
    );

    assert_eq!(result.downgraded_count, 2);
    assert_eq!(result.deny_count, 0);
    assert_eq!(result.user_rule_count, 2);
    assert_eq!(result.project_rule_count, 0);
}

// ── test 2: only project settings ────────────────────────────────────────────

#[test]
fn only_project_settings() {
    let project = make_claude_json(&["Write(**/*.rs)"], &["Bash(rm -rf *)"], &[]);
    let result = run_import(None, Some(&project)).unwrap();

    assert!(result.settings.permissions.allow.is_empty());

    let ask_sources = rule_sources(&result.settings.permissions.ask);
    assert!(ask_sources.contains(&"Write(**/*.rs)"));

    let deny_sources = rule_sources(&result.settings.permissions.deny);
    assert!(deny_sources.contains(&"Bash(rm -rf *)"));

    assert_eq!(result.downgraded_count, 1);
    assert_eq!(result.deny_count, 1);
    assert_eq!(result.user_rule_count, 0);
    assert_eq!(result.project_rule_count, 2);
}

// ── test 3: both exist, rules additive ───────────────────────────────────────

#[test]
fn both_settings_additive() {
    let user = make_claude_json(&["Read(**)"], &[], &["Bash(*)"]);
    let project = make_claude_json(&["Write(**/*.json)"], &["Bash(rm -rf *)"], &[]);
    let result = run_import(Some(&user), Some(&project)).unwrap();

    assert!(result.settings.permissions.allow.is_empty());

    let ask_sources = rule_sources(&result.settings.permissions.ask);
    // user allow downgraded → ask
    assert!(ask_sources.contains(&"Read(**)"), "Read(**) missing");
    // user ask preserved
    assert!(ask_sources.contains(&"Bash(*)"), "Bash(*) missing");
    // project allow downgraded → ask
    assert!(ask_sources.contains(&"Write(**/*.json)"), "Write missing");

    let deny_sources = rule_sources(&result.settings.permissions.deny);
    assert!(deny_sources.contains(&"Bash(rm -rf *)"), "deny missing");

    assert_eq!(result.user_rule_count, 2);   // allow + ask from user
    assert_eq!(result.project_rule_count, 2); // allow + deny from project
}

// ── test 4: unparseable rule → warning, other rules succeed ──────────────────

#[test]
fn unparseable_rule_skipped_others_succeed() {
    // Rules that should fail parsing:
    //   - missing closing ')' → parse error
    //   - "mcp__tool(some arg)" → unknown tool (reaches build_matcher with non-* args)
    let user = make_claude_json(
        &[
            "Read(**)",
            "Bash(git log",                       // missing closing ')'
            "mcp__server__tool(some/path/**)",    // unknown tool with a non-* glob arg
        ],
        &[],
        &[],
    );
    let result = run_import(Some(&user), None).unwrap();

    // The parseable rule should appear in ask
    let ask_sources = rule_sources(&result.settings.permissions.ask);
    assert!(ask_sources.contains(&"Read(**)"), "Read(**) should be in ask");

    // The bad rules should NOT appear anywhere
    let all_sources: Vec<&str> = result
        .settings
        .permissions
        .ask
        .iter()
        .chain(result.settings.permissions.allow.iter())
        .chain(result.settings.permissions.deny.iter())
        .map(|r| r.source.as_str())
        .collect();
    assert!(
        !all_sources.iter().any(|s| s.contains("mcp__")),
        "mcp__ rule should have been skipped"
    );
    assert!(
        !all_sources.iter().any(|s| s.contains("git log")),
        "malformed rule should have been skipped"
    );

    // only the 1 parseable allow was downgraded
    assert_eq!(result.downgraded_count, 1);
}

// ── test 5: allow fully downgraded (none remain in allow) ────────────────────

#[test]
fn allow_fully_downgraded_none_remain() {
    let user = make_claude_json(
        &["Read(**)", "Bash(git log)", "Write(**/*.md)"],
        &[],
        &[],
    );
    let result = run_import(Some(&user), None).unwrap();

    assert!(
        result.settings.permissions.allow.is_empty(),
        "allow must be empty after full downgrade"
    );
    assert_eq!(result.settings.permissions.ask.len(), 3);
    assert_eq!(result.downgraded_count, 3);
}

// ── test 6: deny preserved verbatim ──────────────────────────────────────────

#[test]
fn deny_preserved_verbatim() {
    let user = make_claude_json(
        &[],
        &["Bash(rm -rf *)", "Write(/etc/**)"],
        &[],
    );
    let result = run_import(Some(&user), None).unwrap();

    assert!(result.settings.permissions.allow.is_empty());
    assert!(result.settings.permissions.ask.is_empty());

    let deny_sources = rule_sources(&result.settings.permissions.deny);
    assert_eq!(deny_sources.len(), 2);
    assert!(deny_sources.contains(&"Bash(rm -rf *)"));
    assert!(deny_sources.contains(&"Write(/etc/**)"));

    assert_eq!(result.deny_count, 2);
    assert_eq!(result.downgraded_count, 0);
}

// ── test 7: hooks and env not imported ───────────────────────────────────────

#[test]
fn hooks_and_env_not_imported() {
    let user = json!({
        "permissions": {
            "allow": ["Read(**)"],
            "deny":  [],
            "ask":   []
        },
        "hooks": {
            "pre_tool_use": [{"command": "echo hi"}]
        },
        "env": {
            "ANTHROPIC_API_KEY": "sk-secret"
        }
    });
    let result = run_import(Some(&user), None).unwrap();

    // Hooks/env skipped flags set
    assert!(result.skipped_hooks, "hooks should be flagged as skipped");
    assert!(result.skipped_env, "env should be flagged as skipped");

    // Output settings must have no hooks, no env keys that look like secrets
    assert!(result.settings.hooks.is_empty(), "hooks must not be imported");
    // env_allow is the default safe list, not the Claude env block
    let default_env: Vec<String> = harness_core::config::DEFAULT_ENV_ALLOW
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(result.settings.env_allow, default_env);
}

// ── test 8: output settings has correct defaults ─────────────────────────────

#[test]
fn output_settings_has_correct_defaults() {
    let user = make_claude_json(&[], &[], &[]);
    let result = run_import(Some(&user), None).unwrap();

    assert_eq!(result.settings.v, SETTINGS_VERSION);
    assert_eq!(result.settings.model, DEFAULT_MODEL);
    assert_eq!(result.settings.provider, "anthropic");
}

// ── test 9: dedup — same rule in both user and project ask ───────────────────

#[test]
fn dedup_same_rule_across_files() {
    let user = make_claude_json(&["Read(**)"], &[], &[]);
    let project = make_claude_json(&["Read(**)"], &[], &[]); // duplicate
    let result = run_import(Some(&user), Some(&project)).unwrap();

    // Should appear only once in ask
    let ask_sources = rule_sources(&result.settings.permissions.ask);
    assert_eq!(
        ask_sources.iter().filter(|s| **s == "Read(**)").count(),
        1,
        "duplicate rule should appear only once"
    );
}
