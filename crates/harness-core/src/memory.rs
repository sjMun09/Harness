//! HARNESS.md memory: section-tagged convention lookup. PLAN §3.2 / §5.6.
//!
//! HARNESS.md is the project-local convention document. The MVP loads the
//! whole file, but every section can carry an optional `[pattern: "..."]`
//! tag (a glob). At lookup time, given the path the model is about to edit,
//! we return only the sections whose pattern matches — that's the "lazy"
//! part: the model never sees conventions for files it isn't touching.
//!
//! Section format:
//!
//! ```text
//! ## Conventions [pattern: "**/*.xml"]
//!
//! Use lowercase tag names. Never mix attribute and content in the same element.
//!
//! ← ! Canonical: <user id="1">name</user>
//! ← ✗ Anti:      <user id="1"><name>name</name></user>
//! ```
//!
//! Untagged sections are treated as global (always returned). The canonical
//! marker `← !` and anti marker `← ✗` are PLAN §3.2; ASCII fallbacks `✅`/`❌`
//! and the literal words `canonical`/`anti` are also recognized.

use std::path::Path;

use globset::{Glob, GlobMatcher};

#[derive(Debug, Clone)]
pub struct MemoryDoc {
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone)]
pub struct Section {
    pub heading: String,
    pub pattern: Option<String>,
    pub body: String,
    matcher: Option<GlobMatcher>,
}

impl Section {
    /// Body lines that the doc author tagged as canonical examples.
    #[must_use]
    pub fn canonical_lines(&self) -> Vec<&str> {
        self.body.lines().filter(|l| is_canonical_line(l)).collect()
    }

    /// Body lines that the doc author tagged as anti-examples.
    #[must_use]
    pub fn anti_lines(&self) -> Vec<&str> {
        self.body.lines().filter(|l| is_anti_line(l)).collect()
    }

    /// Does this section apply to `target`? Untagged sections always apply.
    #[must_use]
    pub fn matches(&self, target: &str) -> bool {
        match self.matcher.as_ref() {
            None => true,
            Some(m) => m.is_match(target),
        }
    }
}

impl MemoryDoc {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            sections: Vec::new(),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    /// Parse a single HARNESS.md document. Anything before the first `## `
    /// heading is dropped (intro prose belongs in CLAUDE.md or the README).
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut sections = Vec::new();
        let mut cur_heading: Option<String> = None;
        let mut cur_pattern: Option<String> = None;
        let mut cur_body = String::new();

        for raw in text.lines() {
            if let Some(rest) = raw.strip_prefix("## ") {
                // Flush previous.
                if let Some(h) = cur_heading.take() {
                    sections.push(make_section(
                        h,
                        cur_pattern.take(),
                        std::mem::take(&mut cur_body),
                    ));
                }
                let (heading, pattern) = split_heading_pattern(rest);
                cur_heading = Some(heading.to_string());
                cur_pattern = pattern.map(str::to_owned);
                cur_body.clear();
            } else if cur_heading.is_some() {
                cur_body.push_str(raw);
                cur_body.push('\n');
            }
        }
        if let Some(h) = cur_heading {
            sections.push(make_section(h, cur_pattern, cur_body));
        }
        Self { sections }
    }

    /// Load every existing path; non-existent paths are silently skipped (the
    /// `SessionStart` hook in PLAN §3.2 verifies + strips, but for the MVP
    /// loader missing files are not an error). All docs are concatenated;
    /// later files extend the section list.
    #[must_use]
    pub fn load_from_paths<P: AsRef<Path>>(paths: &[P]) -> Self {
        let mut all = Vec::new();
        for p in paths {
            if let Ok(s) = std::fs::read_to_string(p.as_ref()) {
                all.extend(Self::parse(&s).sections);
            }
        }
        Self { sections: all }
    }

    /// Sections that apply to `target_path`. Untagged sections always match;
    /// tagged sections match iff their glob hits the path.
    #[must_use]
    pub fn lookup(&self, target_path: &str) -> Vec<&Section> {
        self.sections
            .iter()
            .filter(|s| s.matches(target_path))
            .collect()
    }

    /// Render a compact bullet summary for the matching sections — suitable
    /// for stitching into a `tool_result` block (e.g. the plan-gate message).
    /// Returns `None` if no sections match.
    #[must_use]
    pub fn render_for_path(&self, target_path: &str) -> Option<String> {
        let hits = self.lookup(target_path);
        if hits.is_empty() {
            return None;
        }
        let mut out = String::new();
        out.push_str("Relevant HARNESS.md sections:\n");
        for s in hits {
            out.push_str(&format!("\n### {}\n", s.heading));
            for c in s.canonical_lines() {
                out.push_str(&format!("  CANONICAL: {}\n", c.trim()));
            }
            for a in s.anti_lines() {
                out.push_str(&format!("  ANTI:      {}\n", a.trim()));
            }
            // Include any leftover body that isn't a marker line, trimmed.
            let plain: Vec<&str> = s
                .body
                .lines()
                .filter(|l| !is_canonical_line(l) && !is_anti_line(l) && !l.trim().is_empty())
                .collect();
            if !plain.is_empty() {
                out.push_str("  ");
                out.push_str(&plain.join(" ").trim().chars().take(400).collect::<String>());
                out.push('\n');
            }
        }
        Some(out)
    }
}

fn make_section(heading: String, pattern: Option<String>, body: String) -> Section {
    let matcher = pattern
        .as_deref()
        .and_then(|p| Glob::new(p).ok())
        .map(|g| g.compile_matcher());
    Section {
        heading,
        pattern,
        body,
        matcher,
    }
}

/// `Conventions [pattern: "**/*.xml"]` → `("Conventions", Some("**/*.xml"))`.
fn split_heading_pattern(rest: &str) -> (&str, Option<&str>) {
    let trimmed = rest.trim_end();
    let Some(open) = trimmed.find("[pattern:") else {
        return (trimmed, None);
    };
    let Some(close_rel) = trimmed[open..].find(']') else {
        return (trimmed, None);
    };
    let close = open + close_rel;
    let head = trimmed[..open].trim_end();
    // inside is `pattern: "..."` or `pattern: ...`
    let inside = trimmed[open + "[pattern:".len()..close].trim();
    let pat = inside
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(inside);
    if pat.is_empty() {
        (head, None)
    } else {
        (head, Some(pat))
    }
}

fn is_canonical_line(l: &str) -> bool {
    let t = l.trim_start();
    t.starts_with("← !")
        || t.starts_with("✅")
        || t.to_ascii_lowercase().starts_with("- canonical")
        || t.to_ascii_lowercase().starts_with("canonical:")
}

fn is_anti_line(l: &str) -> bool {
    let t = l.trim_start();
    t.starts_with("← ✗")
        || t.starts_with("❌")
        || t.to_ascii_lowercase().starts_with("- anti")
        || t.to_ascii_lowercase().starts_with("anti:")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
intro prose dropped

## Conventions [pattern: \"**/*.xml\"]

XML files use lowercase tags.

← ! Canonical: <user id=\"1\">name</user>
← ✗ Anti:      <user><id>1</id><name>name</name></user>

## Test Commands

cargo test --workspace

## Globals

Always prefer file_path:line_number references.
";

    #[test]
    fn parse_drops_intro_and_keeps_three_sections() {
        let d = MemoryDoc::parse(SAMPLE);
        assert_eq!(d.sections.len(), 3);
        assert_eq!(d.sections[0].heading, "Conventions");
        assert_eq!(d.sections[0].pattern.as_deref(), Some("**/*.xml"));
        assert_eq!(d.sections[1].heading, "Test Commands");
        assert!(d.sections[1].pattern.is_none());
        assert_eq!(d.sections[2].heading, "Globals");
    }

    #[test]
    fn lookup_xml_returns_xml_section_plus_untagged() {
        let d = MemoryDoc::parse(SAMPLE);
        let hits = d.lookup("src/foo.xml");
        // Conventions (xml-tagged) + Test Commands + Globals (both untagged).
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn lookup_rust_skips_xml_section() {
        let d = MemoryDoc::parse(SAMPLE);
        let hits = d.lookup("src/foo.rs");
        let headings: Vec<&str> = hits.iter().map(|s| s.heading.as_str()).collect();
        assert!(!headings.contains(&"Conventions"));
        assert!(headings.contains(&"Test Commands"));
        assert!(headings.contains(&"Globals"));
    }

    #[test]
    fn canonical_and_anti_lines_extracted() {
        let d = MemoryDoc::parse(SAMPLE);
        let s = &d.sections[0];
        assert_eq!(s.canonical_lines().len(), 1);
        assert_eq!(s.anti_lines().len(), 1);
    }

    #[test]
    fn render_includes_canonical_and_anti() {
        let d = MemoryDoc::parse(SAMPLE);
        let r = d.render_for_path("src/foo.xml").unwrap();
        assert!(r.contains("CANONICAL:"));
        assert!(r.contains("ANTI:"));
        assert!(r.contains("### Conventions"));
    }

    #[test]
    fn render_returns_none_when_no_match_and_no_globals() {
        let d = MemoryDoc::parse("## Conventions [pattern: \"**/*.xml\"]\nbody\n");
        assert!(d.render_for_path("src/foo.rs").is_none());
    }

    #[test]
    fn empty_doc_is_empty() {
        assert!(MemoryDoc::empty().is_empty());
        assert!(MemoryDoc::parse("").is_empty());
    }

    #[test]
    fn untagged_section_matches_anything() {
        let d = MemoryDoc::parse("## Globals\nrule\n");
        assert_eq!(d.lookup("/anything.rs").len(), 1);
    }

    #[test]
    fn ascii_marker_fallbacks_recognized() {
        let d = MemoryDoc::parse(
            "## S\n\
             ✅ canonical: a\n\
             ❌ anti: b\n\
             - canonical: c\n\
             - anti: d\n",
        );
        let s = &d.sections[0];
        assert_eq!(s.canonical_lines().len(), 2);
        assert_eq!(s.anti_lines().len(), 2);
    }
}
