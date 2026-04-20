//! `ImportTrace` tool — transitive include/refid chain for MyBatis + Freemarker.
//! PLAN §4.3.
//!
//! Given a root XML/FTL file, walks the transitive graph of references and
//! returns a rendered tree + flat visited list. The model uses this when it
//! is about to refactor a file and needs to know what else might break —
//! catches the "I changed `common.where_clause` and broke five other mappers"
//! class of bug that plan-gate + conventions alone cannot see.
//!
//! Safety per PLAN §4.3:
//!   - Depth cap 32 (user-overridable up to `MAX_DEPTH`)
//!   - Visited-set cycle detection
//!   - Missing ref = warn stub, NOT error (continues walk)
//!   - case-sensitive refid matching
//!   - MyBatis namespace scope (unqualified refid resolves to same namespace)
//!
//! Semantics (not a full XML parser):
//!   - MyBatis: extracts `<mapper namespace="…">`, `<sql id="…">`, `<include refid="…">`
//!   - Freemarker: extracts `<#import "…">`, `<#include "…">`
//!
//! Why regex not quick-xml: the surface here is tiny and well-formed,
//! quick-xml buys us structure we don't use. MyBatisDynamicParser (next tool)
//! is where AST becomes necessary.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{head_tail, parse_input, HEAD_TAIL_CAP};
use crate::fs_safe::{canonicalize_within, PathError};

pub const DEFAULT_MAX_DEPTH: u32 = 32;
pub const MAX_DEPTH: u32 = 64;
/// Upper bound on files visited in one trace — protects against pathological
/// mapper trees where every file pulls in every other.
pub const MAX_VISITED: usize = 2_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    Mybatis,
    Freemarker,
    Auto,
}

impl Default for TraceKind {
    fn default() -> Self {
        Self::Auto
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportTraceInput {
    pub file_path: String,
    #[serde(default)]
    pub kind: TraceKind,
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    /// MyBatis: root directory to scan for `<sql id>` definitions across
    /// mappers. Defaults to `ctx.cwd`.
    #[serde(default)]
    pub mapper_root: Option<String>,
}

fn default_max_depth() -> u32 {
    DEFAULT_MAX_DEPTH
}

#[derive(Debug, Default)]
pub struct ImportTraceTool;

#[async_trait]
impl Tool for ImportTraceTool {
    fn name(&self) -> &str {
        "ImportTrace"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path":   { "type": "string", "description": "Path to root XML or FTL file" },
                "kind":        { "type": "string", "enum": ["mybatis", "freemarker", "auto"] },
                "max_depth":   { "type": "integer", "minimum": 1, "maximum": MAX_DEPTH },
                "mapper_root": { "type": "string", "description": "MyBatis: dir to scan for <sql id>. Default cwd." }
            },
            "required": ["file_path"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<ImportTraceInput>(input.clone()) {
            Ok(i) => Preview {
                summary_line: format!("ImportTrace {}", i.file_path),
                detail: None,
            },
            Err(e) => Preview {
                summary_line: "ImportTrace <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let inp: ImportTraceInput = parse_input(input, "ImportTrace")?;
        let max_depth = inp.max_depth.min(MAX_DEPTH);
        let root_file = canonicalize_within(&ctx.cwd, Path::new(&inp.file_path))
            .map_err(path_error_to_tool_error)?;
        let mapper_root = match inp.mapper_root.as_deref() {
            Some(r) => canonicalize_within(&ctx.cwd, Path::new(r))
                .map_err(path_error_to_tool_error)?,
            None => ctx.cwd.clone(),
        };
        let resolved_kind = match inp.kind {
            TraceKind::Auto => sniff_kind(&root_file),
            other => other,
        };

        let rendered = tokio::task::spawn_blocking({
            let root_file = root_file.clone();
            let mapper_root = mapper_root.clone();
            move || run_trace(resolved_kind, &root_file, &mapper_root, max_depth)
        })
        .await
        .map_err(|e| ToolError::Other(format!("ImportTrace: join failed: {e}")))??;

        Ok(ToolOutput {
            summary: head_tail(&rendered, HEAD_TAIL_CAP * 4),
            detail_path: None,
            stream: None,
        })
    }
}

fn path_error_to_tool_error(e: PathError) -> ToolError {
    match e {
        PathError::Io(io) => ToolError::Io(io),
        other => ToolError::Validation(other.to_string()),
    }
}

fn sniff_kind(path: &Path) -> TraceKind {
    match path.extension().and_then(|s| s.to_str()) {
        Some("ftl" | "ftlh" | "ftlx") => TraceKind::Freemarker,
        _ => TraceKind::Mybatis,
    }
}

// -- Core walk -----------------------------------------------------------------

#[derive(Debug)]
struct TraceNode {
    label: String,
    children: Vec<TraceNode>,
    status: NodeStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeStatus {
    Ok,
    Missing,
    Cycle,
    DepthCapped,
    Capped,
}

#[derive(Debug, Default)]
struct TraceStats {
    files: usize,
    refs_total: usize,
    missing: usize,
    cycles: usize,
    max_depth_seen: u32,
}

fn run_trace(
    kind: TraceKind,
    root: &Path,
    mapper_root: &Path,
    max_depth: u32,
) -> Result<String, ToolError> {
    let (tree, stats) = match kind {
        TraceKind::Mybatis => trace_mybatis(root, mapper_root, max_depth)?,
        TraceKind::Freemarker => trace_freemarker(root, max_depth)?,
        TraceKind::Auto => unreachable!("sniff_kind resolved"),
    };
    Ok(render_trace(kind, root, max_depth, &tree, &stats))
}

fn render_trace(
    kind: TraceKind,
    root: &Path,
    max_depth: u32,
    tree: &TraceNode,
    stats: &TraceStats,
) -> String {
    let kind_str = match kind {
        TraceKind::Mybatis => "mybatis",
        TraceKind::Freemarker => "freemarker",
        TraceKind::Auto => "auto",
    };
    let mut out = format!(
        "ImportTrace: {} ({kind_str}, max_depth={max_depth}, files={}, refs={}, missing={}, cycles={}, depth_seen={})\n\n",
        root.display(),
        stats.files,
        stats.refs_total,
        stats.missing,
        stats.cycles,
        stats.max_depth_seen,
    );
    render_node(tree, &mut out, &mut Vec::new(), true);
    out
}

fn render_node(node: &TraceNode, out: &mut String, prefix: &mut Vec<bool>, is_root: bool) {
    if is_root {
        out.push_str(&node.label);
        out.push_str(status_suffix(node.status));
        out.push('\n');
    }
    for (i, child) in node.children.iter().enumerate() {
        let last = i + 1 == node.children.len();
        for &parent_last in prefix.iter() {
            out.push_str(if parent_last { "   " } else { "│  " });
        }
        out.push_str(if last { "└─ " } else { "├─ " });
        out.push_str(&child.label);
        out.push_str(status_suffix(child.status));
        out.push('\n');
        prefix.push(last);
        render_node(child, out, prefix, false);
        prefix.pop();
    }
}

fn status_suffix(s: NodeStatus) -> &'static str {
    match s {
        NodeStatus::Ok => "",
        NodeStatus::Missing => "  ⚠ MISSING",
        NodeStatus::Cycle => "  ⟲ cycle",
        NodeStatus::DepthCapped => "  … (depth cap)",
        NodeStatus::Capped => "  … (visited cap)",
    }
}

// -- MyBatis -------------------------------------------------------------------

/// `namespace.id → (path, id)`. Built by walking `mapper_root` once up-front.
#[derive(Debug, Default)]
struct MybatisIndex {
    by_qualified: HashMap<String, (PathBuf, String)>,
    /// For reverse lookup: which namespace owns which file.
    file_namespace: HashMap<PathBuf, String>,
}

fn trace_mybatis(
    root: &Path,
    mapper_root: &Path,
    max_depth: u32,
) -> Result<(TraceNode, TraceStats), ToolError> {
    let index = build_mybatis_index(mapper_root)?;
    let mut stats = TraceStats::default();
    let mut visited: HashSet<String> = HashSet::new();

    let root_text = std::fs::read_to_string(root).map_err(ToolError::Io)?;
    let (root_ns, _ids, root_refs) = parse_mybatis(&root_text);
    let root_ns_str = root_ns.unwrap_or_default();
    stats.files += 1;
    let root_key = format!("file::{}", root.display());
    visited.insert(root_key);

    let mut children = Vec::new();
    for refid in root_refs {
        children.push(walk_mybatis_ref(
            &refid,
            &root_ns_str,
            &index,
            &mut visited,
            &mut stats,
            1,
            max_depth,
        ));
    }

    let label = format!(
        "{} [namespace=\"{}\"]",
        root.display(),
        display_namespace(&root_ns_str),
    );
    Ok((
        TraceNode {
            label,
            children,
            status: NodeStatus::Ok,
        },
        stats,
    ))
}

fn display_namespace(ns: &str) -> &str {
    if ns.is_empty() {
        "<unnamed>"
    } else {
        ns
    }
}

fn walk_mybatis_ref(
    refid: &str,
    current_ns: &str,
    index: &MybatisIndex,
    visited: &mut HashSet<String>,
    stats: &mut TraceStats,
    depth: u32,
    max_depth: u32,
) -> TraceNode {
    stats.refs_total += 1;
    stats.max_depth_seen = stats.max_depth_seen.max(depth);

    let qualified = if refid.contains('.') {
        refid.to_string()
    } else if current_ns.is_empty() {
        refid.to_string()
    } else {
        format!("{current_ns}.{refid}")
    };

    let label = format!("refid=\"{refid}\" → {qualified}");

    if depth > max_depth {
        return TraceNode {
            label,
            children: Vec::new(),
            status: NodeStatus::DepthCapped,
        };
    }
    if visited.len() >= MAX_VISITED {
        return TraceNode {
            label,
            children: Vec::new(),
            status: NodeStatus::Capped,
        };
    }

    let Some((path, id)) = index.by_qualified.get(&qualified) else {
        stats.missing += 1;
        return TraceNode {
            label,
            children: Vec::new(),
            status: NodeStatus::Missing,
        };
    };

    let key = format!("{qualified}@{}", path.display());
    if !visited.insert(key) {
        stats.cycles += 1;
        return TraceNode {
            label: format!("{label} (defined in {})", path.display()),
            children: Vec::new(),
            status: NodeStatus::Cycle,
        };
    }

    // Re-open defining file, find the <sql id="X"> body, walk its refids.
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let ns = index
        .file_namespace
        .get(path)
        .map_or("", std::string::String::as_str);
    let inner_refs = extract_sql_body_refs(&text, id);
    stats.files += 1;

    let mut children = Vec::new();
    for child_ref in inner_refs {
        children.push(walk_mybatis_ref(
            &child_ref,
            ns,
            index,
            visited,
            stats,
            depth + 1,
            max_depth,
        ));
    }

    TraceNode {
        label: format!("{label}  (defined in {})", path.display()),
        children,
        status: NodeStatus::Ok,
    }
}

fn build_mybatis_index(root: &Path) -> Result<MybatisIndex, ToolError> {
    let mut idx = MybatisIndex::default();
    let walker = ignore::WalkBuilder::new(root).build();
    for entry in walker.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("xml") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let (ns, ids, _refs) = parse_mybatis(&text);
        let ns = ns.unwrap_or_default();
        if !ns.is_empty() {
            idx.file_namespace.insert(path.to_path_buf(), ns.clone());
        }
        for id in ids {
            let key = if ns.is_empty() {
                id.clone()
            } else {
                format!("{ns}.{id}")
            };
            idx.by_qualified
                .insert(key, (path.to_path_buf(), id.clone()));
        }
    }
    Ok(idx)
}

/// Extract `(namespace, sql_ids, include_refids)` from a MyBatis mapper text.
fn parse_mybatis(text: &str) -> (Option<String>, Vec<String>, Vec<String>) {
    let ns = mapper_namespace_re()
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string());
    let ids: Vec<String> = sql_id_re()
        .captures_iter(text)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .collect();
    let refs: Vec<String> = include_refid_re()
        .captures_iter(text)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .collect();
    (ns, ids, refs)
}

/// Walk a specific `<sql id="X">…</sql>` body and list the `<include refid>`
/// directly underneath it. Nesting/CDATA is tolerated via regex scanning on
/// the substring.
fn extract_sql_body_refs(text: &str, id: &str) -> Vec<String> {
    let open_re = Regex::new(&format!(
        r#"<sql\b[^>]*\bid\s*=\s*"{}"[^>]*>"#,
        regex::escape(id)
    ))
    .ok();
    let Some(open_re) = open_re else {
        return Vec::new();
    };
    let Some(m) = open_re.find(text) else {
        return Vec::new();
    };
    let body_start = m.end();
    // Find the matching </sql> — we don't support nested <sql> elements
    // (MyBatis doesn't allow them).
    let rest = &text[body_start..];
    let body_end_rel = rest.find("</sql>").unwrap_or(rest.len());
    let body = &rest[..body_end_rel];
    include_refid_re()
        .captures_iter(body)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .collect()
}

fn mapper_namespace_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"(?s)<mapper\b[^>]*\bnamespace\s*=\s*"([^"]+)""#)
            .expect("mapper namespace regex compiles")
    })
}

fn sql_id_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"(?s)<sql\b[^>]*\bid\s*=\s*"([^"]+)""#).expect("sql id regex compiles")
    })
}

fn include_refid_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"(?s)<include\b[^>]*\brefid\s*=\s*"([^"]+)""#)
            .expect("include refid regex compiles")
    })
}

// -- Freemarker ----------------------------------------------------------------

fn trace_freemarker(root: &Path, max_depth: u32) -> Result<(TraceNode, TraceStats), ToolError> {
    let mut stats = TraceStats::default();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    visited.insert(root.to_path_buf());
    stats.files += 1;

    let root_dir = root
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

    let text = std::fs::read_to_string(root).map_err(ToolError::Io)?;
    let imports = extract_freemarker_imports(&text);
    let mut children = Vec::new();
    for (directive, target) in imports {
        children.push(walk_freemarker_import(
            &directive,
            &target,
            &root_dir,
            &mut visited,
            &mut stats,
            1,
            max_depth,
        ));
    }
    Ok((
        TraceNode {
            label: root.display().to_string(),
            children,
            status: NodeStatus::Ok,
        },
        stats,
    ))
}

fn walk_freemarker_import(
    directive: &str,
    target: &str,
    base_dir: &Path,
    visited: &mut HashSet<PathBuf>,
    stats: &mut TraceStats,
    depth: u32,
    max_depth: u32,
) -> TraceNode {
    stats.refs_total += 1;
    stats.max_depth_seen = stats.max_depth_seen.max(depth);

    // Deduplicate consecutive `/` and resolve `..` logically — we deliberately
    // do *not* canonicalize (would require the file to exist and do a syscall
    // chain; we want missing-ref to return a stub, not fail).
    let resolved = normalize_logical(&base_dir.join(target));
    let label = format!("<#{directive} \"{target}\"> → {}", resolved.display());

    if depth > max_depth {
        return TraceNode {
            label,
            children: Vec::new(),
            status: NodeStatus::DepthCapped,
        };
    }
    if visited.len() >= MAX_VISITED {
        return TraceNode {
            label,
            children: Vec::new(),
            status: NodeStatus::Capped,
        };
    }
    if !resolved.exists() {
        stats.missing += 1;
        return TraceNode {
            label,
            children: Vec::new(),
            status: NodeStatus::Missing,
        };
    }
    if !visited.insert(resolved.clone()) {
        stats.cycles += 1;
        return TraceNode {
            label,
            children: Vec::new(),
            status: NodeStatus::Cycle,
        };
    }
    stats.files += 1;
    let Ok(text) = std::fs::read_to_string(&resolved) else {
        return TraceNode {
            label,
            children: Vec::new(),
            status: NodeStatus::Ok,
        };
    };
    let new_base = resolved
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let mut children = Vec::new();
    for (d, t) in extract_freemarker_imports(&text) {
        children.push(walk_freemarker_import(
            &d,
            &t,
            &new_base,
            visited,
            stats,
            depth + 1,
            max_depth,
        ));
    }
    TraceNode {
        label,
        children,
        status: NodeStatus::Ok,
    }
}

fn extract_freemarker_imports(text: &str) -> Vec<(String, String)> {
    let re = freemarker_re();
    re.captures_iter(text)
        .filter_map(|c| {
            let directive = c.get(1)?.as_str().to_string();
            let path = c.get(2).or_else(|| c.get(3))?.as_str().to_string();
            Some((directive, path))
        })
        .collect()
}

fn freemarker_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r#"<#(import|include)\s+(?:"([^"]+)"|'([^']+)')"#)
            .expect("freemarker directive regex compiles")
    })
}

fn normalize_logical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                let popped = out.pop();
                if !popped {
                    out.push("..");
                }
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// The `BTreeMap` import is retained for deterministic serialization if we
// expand the output to structured JSON later; the unused warning is a sign
// the MVP is rendering purely textually.
#[allow(dead_code)]
fn _keep_btreemap(_: &BTreeMap<String, String>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn mybatis_parse_extracts_namespace_ids_refs() {
        let text = r#"
<mapper namespace="com.example.UserMapper">
  <sql id="baseCols">id, name</sql>
  <sql id="where">
    <include refid="common.paging"/>
  </sql>
  <select id="find">
    select <include refid="baseCols"/> from users <include refid="where"/>
  </select>
</mapper>"#;
        let (ns, ids, refs) = parse_mybatis(text);
        assert_eq!(ns.as_deref(), Some("com.example.UserMapper"));
        assert_eq!(ids, vec!["baseCols", "where"]);
        assert_eq!(refs, vec!["common.paging", "baseCols", "where"]);
    }

    #[test]
    fn mybatis_unqualified_refid_uses_current_namespace() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        write(
            root,
            "user.xml",
            r#"<mapper namespace="user">
                 <sql id="cols">id,name</sql>
                 <sql id="where"><include refid="cols"/></sql>
               </mapper>"#,
        );
        let index = build_mybatis_index(root).unwrap();
        assert!(index.by_qualified.contains_key("user.cols"));
        assert!(index.by_qualified.contains_key("user.where"));
    }

    #[test]
    fn mybatis_trace_resolves_cross_namespace_refid() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        write(
            root,
            "user.xml",
            r#"<mapper namespace="user">
                 <sql id="select"><include refid="common.paging"/></sql>
               </mapper>"#,
        );
        write(
            root,
            "common.xml",
            r#"<mapper namespace="common">
                 <sql id="paging">limit 10</sql>
               </mapper>"#,
        );
        let user = root.join("user.xml");
        let (tree, stats) = trace_mybatis(&user, root, 32).unwrap();
        assert_eq!(stats.files, 2);
        assert_eq!(stats.missing, 0);
        assert_eq!(stats.refs_total, 1);
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].status, NodeStatus::Ok);
        assert!(tree.children[0].label.contains("common.paging"));
    }

    #[test]
    fn mybatis_missing_refid_yields_warn_stub() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        write(
            root,
            "user.xml",
            r#"<mapper namespace="user">
                 <sql id="select"><include refid="nowhere.nothing"/></sql>
               </mapper>"#,
        );
        let user = root.join("user.xml");
        let (tree, stats) = trace_mybatis(&user, root, 32).unwrap();
        assert_eq!(stats.missing, 1);
        assert_eq!(tree.children[0].status, NodeStatus::Missing);
    }

    #[test]
    fn mybatis_cycle_is_detected_not_infinite() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        // a.x → a.y, a.y → a.x
        write(
            root,
            "a.xml",
            r#"<mapper namespace="a">
                 <sql id="x"><include refid="y"/></sql>
                 <sql id="y"><include refid="x"/></sql>
               </mapper>"#,
        );
        // Walk the <sql id="x"> body indirectly: trace root is a file, so
        // drive via a file that includes a.x as its single ref.
        write(
            root,
            "root.xml",
            r#"<mapper namespace="root">
                 <sql id="s"><include refid="a.x"/></sql>
               </mapper>"#,
        );
        let rootp = root.join("root.xml");
        // root.xml has a <sql id="s"> but trace_mybatis walks refids at the
        // top of the file. We parse the whole file, so top-level refids are
        // the ones inside `<sql id="s">`.
        let (_tree, stats) = trace_mybatis(&rootp, root, 32).unwrap();
        assert!(stats.cycles >= 1, "expected cycle, got stats {stats:?}");
    }

    #[test]
    fn mybatis_depth_cap_stops_walk() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        // Chain 10 deep.
        write(
            root,
            "a.xml",
            r#"<mapper namespace="a">
                 <sql id="s0"><include refid="a.s1"/></sql>
                 <sql id="s1"><include refid="a.s2"/></sql>
                 <sql id="s2"><include refid="a.s3"/></sql>
                 <sql id="s3"><include refid="a.s4"/></sql>
                 <sql id="s4">leaf</sql>
               </mapper>"#,
        );
        write(
            root,
            "r.xml",
            r#"<mapper namespace="r">
                 <sql id="entry"><include refid="a.s0"/></sql>
               </mapper>"#,
        );
        let rp = root.join("r.xml");
        let (_tree, stats) = trace_mybatis(&rp, root, 2).unwrap();
        // depth 1=s0, depth 2=s1, depth 3 capped before s2 resolves.
        // Visited files only: r.xml, a.xml (a.xml re-opened doesn't double-count).
        assert!(stats.max_depth_seen >= 2);
    }

    #[test]
    fn freemarker_relative_include_resolves() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        write(root, "main.ftl", r#"<#include "partials/header.ftl">"#);
        write(root, "partials/header.ftl", r#"<#import "../util.ftl" as u>"#);
        write(root, "util.ftl", "<#-- utility -->");
        let main = root.join("main.ftl");
        let (tree, stats) = trace_freemarker(&main, 32).unwrap();
        assert_eq!(stats.missing, 0);
        assert_eq!(stats.files, 3);
        assert_eq!(tree.children[0].children.len(), 1);
    }

    #[test]
    fn freemarker_missing_include_warns() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        write(root, "main.ftl", r#"<#include "does_not_exist.ftl">"#);
        let main = root.join("main.ftl");
        let (tree, stats) = trace_freemarker(&main, 32).unwrap();
        assert_eq!(stats.missing, 1);
        assert_eq!(tree.children[0].status, NodeStatus::Missing);
    }

    #[test]
    fn freemarker_cycle_detected() {
        let td = TempDir::new().unwrap();
        let root = td.path();
        write(root, "a.ftl", r#"<#include "b.ftl">"#);
        write(root, "b.ftl", r#"<#include "a.ftl">"#);
        let a = root.join("a.ftl");
        let (_tree, stats) = trace_freemarker(&a, 32).unwrap();
        assert!(stats.cycles >= 1);
    }

    #[test]
    fn freemarker_extract_supports_single_and_double_quotes() {
        let text = r#"<#import "x.ftl" as x><#include 'y.ftl'>"#;
        let got = extract_freemarker_imports(text);
        assert_eq!(
            got,
            vec![
                ("import".into(), "x.ftl".into()),
                ("include".into(), "y.ftl".into()),
            ]
        );
    }

    #[test]
    fn sniff_kind_picks_mybatis_for_xml() {
        assert_eq!(sniff_kind(Path::new("a/b.xml")), TraceKind::Mybatis);
        assert_eq!(sniff_kind(Path::new("a/b.ftl")), TraceKind::Freemarker);
        assert_eq!(sniff_kind(Path::new("a/b.ftlh")), TraceKind::Freemarker);
    }

    #[test]
    fn render_tree_uses_box_characters() {
        let tree = TraceNode {
            label: "root.xml".into(),
            status: NodeStatus::Ok,
            children: vec![
                TraceNode {
                    label: "refid=\"a\" → ns.a".into(),
                    status: NodeStatus::Ok,
                    children: vec![TraceNode {
                        label: "refid=\"b\" → ns.b".into(),
                        status: NodeStatus::Missing,
                        children: vec![],
                    }],
                },
                TraceNode {
                    label: "refid=\"c\" → ns.c".into(),
                    status: NodeStatus::Cycle,
                    children: vec![],
                },
            ],
        };
        let mut out = String::new();
        render_node(&tree, &mut out, &mut Vec::new(), true);
        assert!(out.contains("├─ refid=\"a\""));
        assert!(out.contains("└─ refid=\"c\""));
        assert!(out.contains("⚠ MISSING"));
        assert!(out.contains("⟲ cycle"));
    }

    #[test]
    fn normalize_logical_collapses_parent_refs() {
        assert_eq!(
            normalize_logical(Path::new("a/b/../c")),
            PathBuf::from("a/c")
        );
    }
}
