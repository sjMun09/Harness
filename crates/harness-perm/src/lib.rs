//! Harness permission dispatcher (`allow` / `ask` / `deny`). PLAN §5.8.
//!
//! Rule syntax: `<Tool>(<pattern>)`.
//!   - `Bash(<shlex-prefix>)` — shlex token prefix match on the command.
//!     `Bash(git status)` matches `git status`, `git status --short`.
//!   - `Read|Write|Edit|Glob|Grep(<globset-pattern>)` — globset match on the
//!     primary path argument (`file_path` / `pattern` / `path`).
//!   - `Bash(*)` or `<Tool>(**)` = any-args wildcard.
//!
//! Precedence (first match wins within bucket, deny > allow > ask):
//!   1. `deny` match → Deny
//!   2. `allow` match → Allow
//!   3. `ask` match  → Ask
//!   4. no match     → Ask (safe default)
//!
//! Specificity: longer shlex prefix (more tokens) / longer globset literal
//! segment sorts first. Ties broken by declaration order.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// 3-valued permission outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

/// Parsed rule: tool name + tool-specific matcher.
#[derive(Debug, Clone)]
pub struct Rule {
    pub tool: String,
    pub matcher: Matcher,
    /// Original `"Tool(args)"` source — preserved for error messages + ordering.
    pub source: String,
}

#[derive(Debug, Clone)]
pub enum Matcher {
    /// `*` or `**` — match anything.
    Any,
    /// Shlex-tokenized prefix (Bash).
    ShlexPrefix(Vec<String>),
    /// Globset pattern (Read/Write/Edit/Glob/Grep).
    Glob {
        source: String,
        matcher: GlobMatcher,
    },
}

impl Matcher {
    fn specificity(&self) -> usize {
        match self {
            Self::Any => 0,
            Self::ShlexPrefix(toks) => toks.iter().map(String::len).sum::<usize>() + toks.len(),
            Self::Glob { source, .. } => source.chars().filter(|c| !matches!(c, '*' | '?')).count(),
        }
    }
}

// `Rule` serializes as its `"Tool(args)"` source string so settings.json
// round-trips cleanly.
impl Serialize for Rule {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.source)
    }
}

impl<'de> Deserialize<'de> for Rule {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Rule::parse(&s).map_err(serde::de::Error::custom)
    }
}

impl Rule {
    /// Parse `"Tool(args)"` or `"Tool"` (= any args).
    pub fn parse(s: &str) -> Result<Self, PermError> {
        let trimmed = s.trim();
        let source = trimmed.to_string();

        let (tool, args) = match trimmed.find('(') {
            None => (trimmed, None),
            Some(lp) => {
                if !trimmed.ends_with(')') {
                    return Err(PermError::InvalidRule(format!(
                        "expected trailing ')': {trimmed}"
                    )));
                }
                let tool = &trimmed[..lp];
                let args = &trimmed[lp + 1..trimmed.len() - 1];
                (tool, Some(args))
            }
        };

        if tool.is_empty() {
            return Err(PermError::InvalidRule(format!("empty tool name: {s}")));
        }

        let matcher = match args {
            None => Matcher::Any,
            Some("*" | "**") => Matcher::Any,
            Some(pat) => build_matcher(tool, pat)?,
        };

        Ok(Self {
            tool: tool.to_string(),
            matcher,
            source,
        })
    }
}

fn build_matcher(tool: &str, pat: &str) -> Result<Matcher, PermError> {
    match tool {
        "Bash" => {
            let toks = shlex::split(pat)
                .ok_or_else(|| PermError::InvalidRule(format!("shlex parse failed: {pat}")))?;
            if toks.is_empty() {
                return Ok(Matcher::Any);
            }
            Ok(Matcher::ShlexPrefix(toks))
        }
        "Read" | "Write" | "Edit" | "Glob" | "Grep" => {
            let glob = Glob::new(pat)
                .map_err(|e| PermError::InvalidRule(format!("glob parse {pat}: {e}")))?;
            Ok(Matcher::Glob {
                source: pat.to_string(),
                matcher: glob.compile_matcher(),
            })
        }
        other => Err(PermError::InvalidRule(format!("unknown tool: {other}"))),
    }
}

impl Rule {
    /// True when `tool` + `input` JSON satisfy this rule.
    #[must_use]
    pub fn matches(&self, tool: &str, input: &serde_json::Value) -> bool {
        if self.tool != tool {
            return false;
        }
        match &self.matcher {
            Matcher::Any => true,
            Matcher::ShlexPrefix(prefix) => bash_command(input).is_some_and(|cmd| {
                shlex::split(&cmd).is_some_and(|cmd_toks| starts_with_tokens(&cmd_toks, prefix))
            }),
            Matcher::Glob { matcher, .. } => {
                glob_target(tool, input).is_some_and(|s| matcher.is_match(std::path::Path::new(&s)))
            }
        }
    }
}

fn starts_with_tokens(haystack: &[String], needle: &[String]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.iter().zip(needle).all(|(a, b)| a == b)
}

fn bash_command(input: &serde_json::Value) -> Option<String> {
    input
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

/// Extract the primary path argument by tool convention.
fn glob_target(tool: &str, input: &serde_json::Value) -> Option<String> {
    let key = match tool {
        "Read" | "Write" | "Edit" => "file_path",
        "Glob" | "Grep" => "path",
        _ => return None,
    };
    input.get(key).and_then(|v| v.as_str()).map(str::to_owned)
}

/// Cheap-clone, immutable-for-lifetime-of-turn permission view.
#[derive(Clone, Default)]
pub struct PermissionSnapshot {
    inner: Arc<PermissionInner>,
}

#[derive(Default)]
struct PermissionInner {
    deny: Vec<Rule>,
    allow: Vec<Rule>,
    ask: Vec<Rule>,
    /// `(tool, canonical_input_hash)` → Decision. Only `Decision::Allow` is
    /// seeded from user `[a]lways` answers.
    ask_cache: Mutex<HashSet<(String, u64)>>,
}

impl std::fmt::Debug for PermissionSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionSnapshot")
            .field("deny", &self.inner.deny.len())
            .field("allow", &self.inner.allow.len())
            .field("ask", &self.inner.ask.len())
            .finish()
    }
}

impl PermissionSnapshot {
    pub fn new(mut deny: Vec<Rule>, mut allow: Vec<Rule>, mut ask: Vec<Rule>) -> Self {
        // Sort each bucket most-specific-first so `matches` short-circuits on the
        // strongest rule. Stable sort keeps config-file order on ties.
        deny.sort_by(|a, b| b.matcher.specificity().cmp(&a.matcher.specificity()));
        allow.sort_by(|a, b| b.matcher.specificity().cmp(&a.matcher.specificity()));
        ask.sort_by(|a, b| b.matcher.specificity().cmp(&a.matcher.specificity()));
        Self {
            inner: Arc::new(PermissionInner {
                deny,
                allow,
                ask,
                ask_cache: Mutex::new(HashSet::new()),
            }),
        }
    }

    /// Evaluate — pure, side-effect-free. Session `[a]lways` cache checked first.
    pub fn evaluate(&self, tool: &str, input: &serde_json::Value) -> Decision {
        let key = (tool.to_string(), hash_input(input));
        if self
            .inner
            .ask_cache
            .lock()
            .map(|g| g.contains(&key))
            .unwrap_or(false)
        {
            return Decision::Allow;
        }

        if self.inner.deny.iter().any(|r| r.matches(tool, input)) {
            return Decision::Deny;
        }
        if self.inner.allow.iter().any(|r| r.matches(tool, input)) {
            return Decision::Allow;
        }
        if self.inner.ask.iter().any(|r| r.matches(tool, input)) {
            return Decision::Ask;
        }
        // Safe default per §5.8: unmatched = Ask.
        Decision::Ask
    }

    /// Cache an `[a]lways` response for future evaluation of the same
    /// `(tool, input)` pair. No-op on lock poisoning.
    pub fn remember_always(&self, tool: &str, input: &serde_json::Value) {
        if let Ok(mut g) = self.inner.ask_cache.lock() {
            g.insert((tool.to_string(), hash_input(input)));
        }
    }
}

fn hash_input(input: &serde_json::Value) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let canonical = serde_json::to_string(input).unwrap_or_default();
    let mut h = DefaultHasher::new();
    canonical.hash(&mut h);
    h.finish()
}

#[derive(Debug, Error)]
pub enum PermError {
    #[error("invalid rule: {0}")]
    InvalidRule(String),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snap(deny: &[&str], allow: &[&str], ask: &[&str]) -> PermissionSnapshot {
        PermissionSnapshot::new(
            deny.iter().map(|s| Rule::parse(s).unwrap()).collect(),
            allow.iter().map(|s| Rule::parse(s).unwrap()).collect(),
            ask.iter().map(|s| Rule::parse(s).unwrap()).collect(),
        )
    }

    #[test]
    fn parse_rule_forms() {
        assert!(matches!(
            Rule::parse("Bash(git status)").unwrap().matcher,
            Matcher::ShlexPrefix(ref t) if t == &["git".to_string(), "status".to_string()]
        ));
        assert!(matches!(
            Rule::parse("Read(**)").unwrap().matcher,
            Matcher::Any
        ));
        assert!(matches!(
            Rule::parse("Write(/etc/**)").unwrap().matcher,
            Matcher::Glob { .. }
        ));
        assert!(matches!(Rule::parse("Glob").unwrap().matcher, Matcher::Any));
    }

    #[test]
    fn deny_beats_allow() {
        let p = snap(&["Write(/etc/**)"], &["Write(**)"], &[]);
        assert_eq!(
            p.evaluate("Write", &json!({"file_path": "/etc/passwd"})),
            Decision::Deny
        );
        assert_eq!(
            p.evaluate("Write", &json!({"file_path": "/tmp/x"})),
            Decision::Allow
        );
    }

    #[test]
    fn bash_shlex_prefix() {
        let p = snap(&[], &["Bash(git status)"], &["Bash(*)"]);
        assert_eq!(
            p.evaluate("Bash", &json!({"command": "git status"})),
            Decision::Allow
        );
        assert_eq!(
            p.evaluate("Bash", &json!({"command": "git status --short"})),
            Decision::Allow
        );
        assert_eq!(
            p.evaluate("Bash", &json!({"command": "git push"})),
            Decision::Ask
        );
    }

    #[test]
    fn unmatched_defaults_to_ask() {
        let p = snap(&[], &[], &[]);
        assert_eq!(
            p.evaluate("Read", &json!({"file_path": "/tmp/foo"})),
            Decision::Ask
        );
    }

    #[test]
    fn always_cache_promotes_ask_to_allow() {
        let p = snap(&[], &[], &["Edit(**)"]);
        let inp = json!({"file_path": "/tmp/foo", "old_string": "x", "new_string": "y"});
        assert_eq!(p.evaluate("Edit", &inp), Decision::Ask);
        p.remember_always("Edit", &inp);
        assert_eq!(p.evaluate("Edit", &inp), Decision::Allow);
    }

    #[test]
    fn specificity_sorts_longer_prefix_first() {
        // `git status --short` should match the longer prefix even when listed
        // second in the file.
        let p = snap(&[], &["Bash(git)", "Bash(git status)"], &[]);
        // Both allow-rules match, but sort order should put the more-specific
        // one ahead — matters if we ever report which rule matched.
        let inner = &p.inner.allow;
        assert!(inner[0].matcher.specificity() >= inner[1].matcher.specificity());
    }

    #[test]
    fn rule_source_roundtrips_via_serde() {
        let r = Rule::parse("Write(**/*.rs)").unwrap();
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, "\"Write(**/*.rs)\"");
        let back: Rule = serde_json::from_str(&s).unwrap();
        assert_eq!(back.source, r.source);
    }
}
