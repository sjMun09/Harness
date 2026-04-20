//! `harness config import-claude` — imports Claude Code settings with safety downgrades.
//!
//! Security rules (PLAN §8.1 / §8.2):
//!   - Every `allow` rule is downgraded to `ask`.
//!   - `deny` rules are preserved.
//!   - `ask` rules are preserved (merged with downgraded allows, deduped by source).
//!   - `hooks` and `env` blocks are NOT imported — must be re-added manually.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use harness_core::config::{Permissions, Settings, DEFAULT_MODEL, SETTINGS_VERSION};
use harness_perm::Rule;
use serde_json::Value;

/// Parse a Claude Code settings JSON `Value`, extracting permissions.allow / deny / ask
/// as `Vec<String>`. Missing keys produce empty vecs.
fn extract_permission_lists(v: &Value) -> (Vec<String>, Vec<String>, Vec<String>) {
    let extract = |key: &str| -> Vec<String> {
        v.get("permissions")
            .and_then(|p| p.get(key))
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|e| e.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    (extract("allow"), extract("deny"), extract("ask"))
}

fn has_hooks(v: &Value) -> bool {
    v.get("hooks")
        .map(|h| !h.is_null() && h.as_object().map(|o| !o.is_empty()).unwrap_or(false))
        .unwrap_or(false)
}

fn has_env(v: &Value) -> bool {
    v.get("env")
        .map(|e| !e.is_null() && e.as_object().map(|o| !o.is_empty()).unwrap_or(false))
        .unwrap_or(false)
}

/// The pure import logic — testable without filesystem writes.
///
/// `user_claude`    — parsed JSON from `~/.claude/settings.json` (if present).
/// `project_claude` — parsed JSON from `<cwd>/.claude/settings.json` (if present).
///
/// Returns a Harness `Settings` ready to be serialized, plus counters for reporting.
pub struct ImportResult {
    pub settings: Settings,
    pub user_rule_count: usize,
    pub project_rule_count: usize,
    pub downgraded_count: usize,
    pub deny_count: usize,
    pub skipped_hooks: bool,
    pub skipped_env: bool,
}

pub fn do_import(
    user_claude: Option<&Value>,
    project_claude: Option<&Value>,
) -> anyhow::Result<ImportResult> {
    let mut all_allow: Vec<String> = Vec::new();
    let mut all_deny: Vec<String> = Vec::new();
    let mut all_ask: Vec<String> = Vec::new();

    let mut user_rule_count = 0usize;
    let mut project_rule_count = 0usize;
    let mut skipped_hooks = false;
    let mut skipped_env = false;

    // Process user settings first, then project (project wins / appends on top).
    if let Some(u) = user_claude {
        let (a, d, k) = extract_permission_lists(u);
        user_rule_count = a.len() + d.len() + k.len();
        all_allow.extend(a);
        all_deny.extend(d);
        all_ask.extend(k);
        if has_hooks(u) {
            skipped_hooks = true;
        }
        if has_env(u) {
            skipped_env = true;
        }
    }
    if let Some(p) = project_claude {
        let (a, d, k) = extract_permission_lists(p);
        project_rule_count = a.len() + d.len() + k.len();
        all_allow.extend(a);
        all_deny.extend(d);
        all_ask.extend(k);
        if has_hooks(p) {
            skipped_hooks = true;
        }
        if has_env(p) {
            skipped_env = true;
        }
    }

    // Parse rules, warn on failures, continue.
    let mut out_ask: Vec<Rule> = Vec::new();
    let mut out_deny: Vec<Rule> = Vec::new();
    let mut seen_sources: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut downgraded_count = 0usize;

    // Downgrade allow → ask.
    for s in &all_allow {
        match Rule::parse(s) {
            Ok(r) => {
                if seen_sources.insert(r.source.clone()) {
                    out_ask.push(r);
                    downgraded_count += 1;
                }
            }
            Err(e) => {
                eprintln!("[import-claude] skipped unparseable rule: \"{s}\" (reason: {e})");
            }
        }
    }

    // Preserve ask (dedup with already-seen allow-downgraded rules).
    for s in &all_ask {
        match Rule::parse(s) {
            Ok(r) => {
                if seen_sources.insert(r.source.clone()) {
                    out_ask.push(r);
                }
            }
            Err(e) => {
                eprintln!("[import-claude] skipped unparseable rule: \"{s}\" (reason: {e})");
            }
        }
    }

    // Preserve deny.
    let mut deny_count = 0usize;
    for s in &all_deny {
        match Rule::parse(s) {
            Ok(r) => {
                out_deny.push(r);
                deny_count += 1;
            }
            Err(e) => {
                eprintln!("[import-claude] skipped unparseable rule: \"{s}\" (reason: {e})");
            }
        }
    }

    let settings = Settings {
        v: SETTINGS_VERSION,
        model: DEFAULT_MODEL.to_string(),
        provider: "anthropic".to_string(),
        env_allow: harness_core::config::DEFAULT_ENV_ALLOW
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        permissions: Permissions {
            allow: Vec::new(), // all allows were downgraded to ask
            deny: out_deny,
            ask: out_ask,
        },
        hooks: BTreeMap::new(),
        harness: harness_core::config::HarnessExt::default(),
    };

    Ok(ImportResult {
        settings,
        user_rule_count,
        project_rule_count,
        downgraded_count,
        deny_count,
        skipped_hooks,
        skipped_env,
    })
}

/// Resolve the two candidate Claude Code paths, read whichever exist, run
/// `do_import`, then write the result to the Harness user settings path.
#[allow(dead_code)] // called from main.rs; not visible when included via #[path] in tests
pub async fn cmd_config_import_impl() -> anyhow::Result<()> {
    // Candidate paths.
    let home = std::env::var("HOME").context("HOME env var not set")?;
    let user_claude_path = Path::new(&home).join(".claude").join("settings.json");
    let cwd = std::env::current_dir().context("cwd")?;
    let project_claude_path = cwd.join(".claude").join("settings.json");

    let read_json = |p: &Path| -> anyhow::Result<Option<Value>> {
        if !p.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(p).with_context(|| format!("read {}", p.display()))?;
        let v: Value =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", p.display()))?;
        Ok(Some(v))
    };

    let user_json = read_json(&user_claude_path)?;
    let project_json = read_json(&project_claude_path)?;

    if user_json.is_none() && project_json.is_none() {
        anyhow::bail!(
            "no Claude Code settings found — looked at:\n  {}\n  {}",
            user_claude_path.display(),
            project_claude_path.display()
        );
    }

    // Print which files were found.
    if user_json.is_some() {
        eprintln!("[import-claude] found {}", user_claude_path.display());
    }
    if project_json.is_some() {
        eprintln!("[import-claude] found {}", project_claude_path.display());
    }

    let result = do_import(user_json.as_ref(), project_json.as_ref())?;

    // Determine output path.
    let out_path = harness_core::config::user_settings_path()
        .context("cannot determine harness user settings path (HOME missing?)")?;

    // Refuse to overwrite.
    if out_path.exists() {
        anyhow::bail!(
            "refusing to overwrite existing {}; move or remove it first",
            out_path.display()
        );
    }

    // Create parent dir with mode 0700 on unix.
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("set perms on {}", parent.display()))?;
        }
    }

    // Serialize and write.
    let json = serde_json::to_string_pretty(&result.settings).context("serialize settings")?;
    std::fs::write(&out_path, &json).with_context(|| format!("write {}", out_path.display()))?;

    // Set file mode 0600 on unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("set perms on {}", out_path.display()))?;
    }

    // Print summary.
    if user_json.is_some() {
        eprintln!(
            "[import-claude] read {} rules from {}",
            result.user_rule_count,
            user_claude_path.display()
        );
    }
    if project_json.is_some() {
        eprintln!(
            "[import-claude] read {} rules from {}",
            result.project_rule_count,
            project_claude_path.display()
        );
    }
    eprintln!(
        "[import-claude] downgraded {} allow → ask",
        result.downgraded_count
    );
    eprintln!("[import-claude] preserved {} deny", result.deny_count);

    let mut skipped_items: Vec<&str> = Vec::new();
    if result.skipped_hooks {
        skipped_items.push("hooks");
    }
    if result.skipped_env {
        skipped_items.push("env");
    }
    if !skipped_items.is_empty() {
        eprintln!(
            "[import-claude] skipped: {} (re-add manually)",
            skipped_items.join(", ")
        );
    }

    eprintln!("[import-claude] wrote {}", out_path.display());
    Ok(())
}
