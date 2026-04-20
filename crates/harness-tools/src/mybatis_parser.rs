//! `MyBatisDynamicParser` tool — parse MyBatis mappers into a branch-aware
//! AST + compare before/after to verify refactoring preserved control flow.
//! PLAN §4.3.
//!
//! Rationale: branch-count + normalized-condition equality is a necessary
//! (not sufficient) condition for refactored MyBatis/Freemarker to behave the
//! same as the original. Semantic equivalence is DiffExec's job; this tool
//! catches the gross mistakes where a translation silently drops an `<if>`
//! branch or collapses two conditions.
//!
//! Recognized dynamic nodes: `<if>`, `<choose>/<when>/<otherwise>`,
//! `<foreach>`, `<bind>`, `<include>`, `<trim>`, `<where>`, `<set>`.
//! Top-level statements: `<sql>`, `<select>`, `<insert>`, `<update>`,
//! `<delete>`. Conditions normalized to canonical `IS [NOT] NULL` form with
//! single-quote → double-quote unification and whitespace collapse.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use async_trait::async_trait;
use harness_core::{Preview, Tool, ToolCtx, ToolError, ToolOutput};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{head_tail, parse_input, HEAD_TAIL_CAP};
use crate::fs_safe::{canonicalize_within, PathError};

// -- Input / Tool --------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyBatisParserInput {
    pub file_path: String,
    /// If set, parse both files and report the delta.
    #[serde(default)]
    pub compare_to: Option<String>,
    /// If set, report only on the matching `<sql|select|insert|update|delete id="…">`.
    #[serde(default)]
    pub sql_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct MyBatisDynamicParserTool;

#[async_trait]
impl Tool for MyBatisDynamicParserTool {
    fn name(&self) -> &str {
        "MyBatisDynamicParser"
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path":  { "type": "string", "description": "Path to a MyBatis mapper XML" },
                "compare_to": { "type": "string", "description": "Optional second mapper for before/after delta" },
                "sql_id":     { "type": "string", "description": "Optional stmt id filter" }
            },
            "required": ["file_path"]
        })
    }

    fn preview(&self, input: &Value) -> Preview {
        match serde_json::from_value::<MyBatisParserInput>(input.clone()) {
            Ok(i) => Preview {
                summary_line: match &i.compare_to {
                    Some(b) => format!("MyBatisDynamicParser {} vs {}", i.file_path, b),
                    None => format!("MyBatisDynamicParser {}", i.file_path),
                },
                detail: None,
            },
            Err(e) => Preview {
                summary_line: "MyBatisDynamicParser <invalid input>".into(),
                detail: Some(e.to_string()),
            },
        }
    }

    async fn call(&self, input: Value, ctx: ToolCtx) -> Result<ToolOutput, ToolError> {
        let inp: MyBatisParserInput = parse_input(input, "MyBatisDynamicParser")?;
        let file_a = canonicalize_within(&ctx.cwd, Path::new(&inp.file_path))
            .map_err(path_err_to_tool_err)?;
        let file_b = match inp.compare_to.as_deref() {
            Some(p) => {
                Some(canonicalize_within(&ctx.cwd, Path::new(p)).map_err(path_err_to_tool_err)?)
            }
            None => None,
        };
        let sql_id = inp.sql_id.clone();

        let rendered = tokio::task::spawn_blocking(move || {
            let text_a = std::fs::read_to_string(&file_a).map_err(ToolError::Io)?;
            let stmts_a = filter_by_id(parse_mapper(&text_a)?, sql_id.as_deref());
            match file_b {
                Some(b) => {
                    let text_b = std::fs::read_to_string(&b).map_err(ToolError::Io)?;
                    let stmts_b = filter_by_id(parse_mapper(&text_b)?, sql_id.as_deref());
                    Ok::<String, ToolError>(render_compare(
                        &file_a.display().to_string(),
                        &b.display().to_string(),
                        &stmts_a,
                        &stmts_b,
                    ))
                }
                None => Ok(render_single(&file_a.display().to_string(), &stmts_a)),
            }
        })
        .await
        .map_err(|e| ToolError::Other(format!("MyBatisDynamicParser join: {e}")))??;

        Ok(ToolOutput {
            summary: head_tail(&rendered, HEAD_TAIL_CAP * 4),
            detail_path: None,
            stream: None,
        })
    }
}

fn path_err_to_tool_err(e: PathError) -> ToolError {
    match e {
        PathError::Io(io) => ToolError::Io(io),
        other => ToolError::Validation(other.to_string()),
    }
}

fn filter_by_id(stmts: Vec<Statement>, id: Option<&str>) -> Vec<Statement> {
    match id {
        None => stmts,
        Some(target) => stmts.into_iter().filter(|s| s.id == target).collect(),
    }
}

// -- AST -----------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DynNode {
    Text(String),
    If {
        test: String,
        body: Vec<DynNode>,
    },
    Choose {
        whens: Vec<WhenBranch>,
        otherwise: Option<Vec<DynNode>>,
    },
    Foreach {
        collection: String,
        item: Option<String>,
        index: Option<String>,
        open: Option<String>,
        close: Option<String>,
        separator: Option<String>,
        body: Vec<DynNode>,
    },
    Bind {
        name: String,
        value: String,
    },
    Include {
        refid: String,
    },
    Trim {
        prefix: Option<String>,
        suffix: Option<String>,
        prefix_overrides: Option<String>,
        suffix_overrides: Option<String>,
        body: Vec<DynNode>,
    },
    Where {
        body: Vec<DynNode>,
    },
    Set {
        body: Vec<DynNode>,
    },
    Unknown {
        tag: String,
        body: Vec<DynNode>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhenBranch {
    pub test: String,
    pub body: Vec<DynNode>,
}

#[derive(Debug, Clone)]
pub struct Statement {
    pub stmt_type: String,
    pub id: String,
    pub body: Vec<DynNode>,
}

// -- Parse ---------------------------------------------------------------------

pub fn parse_mapper(text: &str) -> Result<Vec<Statement>, ToolError> {
    let mut reader = Reader::from_str(text);
    let config = reader.config_mut();
    config.trim_text(false);
    config.expand_empty_elements = false;

    let mut stmts = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let tag = local_name(e.name().as_ref());
                if is_stmt_tag(&tag) {
                    let id = attr_of(&e, "id").unwrap_or_default();
                    let body = parse_children(&mut reader, tag.as_bytes())?;
                    stmts.push(Statement {
                        stmt_type: tag,
                        id,
                        body,
                    });
                }
                // else: wrapper like <mapper>; keep reading.
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(ToolError::Validation(format!("xml parse: {e}"))),
        }
    }
    Ok(stmts)
}

fn is_stmt_tag(tag: &str) -> bool {
    matches!(tag, "sql" | "select" | "insert" | "update" | "delete")
}

/// Parse children until we see `</close_tag>`. Handles `<choose>` by
/// recursing into a dedicated parser that directly builds `DynNode::Choose`
/// with populated `whens` and `otherwise`. Everything else goes through
/// `build_node`.
fn parse_children(reader: &mut Reader<&[u8]>, close_tag: &[u8]) -> Result<Vec<DynNode>, ToolError> {
    let mut out = Vec::new();
    let mut text_buf = String::new();
    loop {
        let ev = reader
            .read_event()
            .map_err(|e| ToolError::Validation(format!("xml parse: {e}")))?;
        match ev {
            Event::Start(e) => {
                flush_text(&mut text_buf, &mut out);
                let tag = local_name(e.name().as_ref());
                if tag == "choose" {
                    out.push(parse_choose(reader)?);
                } else {
                    let attrs = collect_attrs(&e);
                    let body = parse_children(reader, tag.as_bytes())?;
                    out.push(build_node(&tag, &attrs, body));
                }
            }
            Event::Empty(e) => {
                flush_text(&mut text_buf, &mut out);
                let tag = local_name(e.name().as_ref());
                let attrs = collect_attrs(&e);
                out.push(build_node(&tag, &attrs, Vec::new()));
            }
            Event::End(e) => {
                if local_name(e.name().as_ref()).as_bytes() == close_tag {
                    flush_text(&mut text_buf, &mut out);
                    return Ok(out);
                }
                // Mismatched close — ignore and keep scanning. This keeps
                // the parser forgiving against minor mapper quirks.
            }
            Event::Text(t) => {
                let s = t.unescape().unwrap_or_default();
                text_buf.push_str(&s);
            }
            Event::CData(c) => {
                let s = String::from_utf8_lossy(c.as_ref()).into_owned();
                text_buf.push_str(&s);
            }
            Event::Eof => {
                flush_text(&mut text_buf, &mut out);
                return Ok(out);
            }
            _ => {}
        }
    }
}

fn parse_choose(reader: &mut Reader<&[u8]>) -> Result<DynNode, ToolError> {
    let mut whens = Vec::new();
    let mut otherwise: Option<Vec<DynNode>> = None;
    loop {
        let ev = reader
            .read_event()
            .map_err(|e| ToolError::Validation(format!("xml parse: {e}")))?;
        match ev {
            Event::Start(e) => {
                let tag = local_name(e.name().as_ref());
                match tag.as_str() {
                    "when" => {
                        let test = attr_of(&e, "test").unwrap_or_default();
                        let body = parse_children(reader, b"when")?;
                        whens.push(WhenBranch { test, body });
                    }
                    "otherwise" => {
                        let body = parse_children(reader, b"otherwise")?;
                        otherwise = Some(body);
                    }
                    other => {
                        // Unexpected: drain and drop.
                        let _ = parse_children(reader, other.as_bytes())?;
                    }
                }
            }
            Event::End(e) => {
                if local_name(e.name().as_ref()) == "choose" {
                    return Ok(DynNode::Choose { whens, otherwise });
                }
            }
            Event::Eof => {
                return Err(ToolError::Validation(
                    "unexpected EOF inside <choose>".into(),
                ))
            }
            _ => {}
        }
    }
}

fn flush_text(buf: &mut String, out: &mut Vec<DynNode>) {
    if buf.is_empty() {
        return;
    }
    let text = std::mem::take(buf);
    if !text.trim().is_empty() {
        out.push(DynNode::Text(collapse_ws(&text)));
    }
}

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_ws {
                out.push(' ');
                last_ws = true;
            }
        } else {
            out.push(c);
            last_ws = false;
        }
    }
    out.trim().to_string()
}

fn local_name(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    match s.rsplit_once(':') {
        Some((_, rest)) => rest.to_string(),
        None => s.into_owned(),
    }
}

fn collect_attrs(e: &quick_xml::events::BytesStart<'_>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for a in e.attributes().flatten() {
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let val = a
            .unescape_value()
            .map(|v| v.into_owned())
            .unwrap_or_default();
        out.insert(key, val);
    }
    out
}

fn attr_of(e: &quick_xml::events::BytesStart<'_>, name: &str) -> Option<String> {
    for a in e.attributes().flatten() {
        if a.key.as_ref() == name.as_bytes() {
            return a.unescape_value().ok().map(|v| v.into_owned());
        }
    }
    None
}

fn build_node(tag: &str, attrs: &BTreeMap<String, String>, body: Vec<DynNode>) -> DynNode {
    let get = |k: &str| attrs.get(k).cloned();
    match tag {
        "if" => DynNode::If {
            test: get("test").unwrap_or_default(),
            body,
        },
        "foreach" => DynNode::Foreach {
            collection: get("collection").unwrap_or_default(),
            item: get("item"),
            index: get("index"),
            open: get("open"),
            close: get("close"),
            separator: get("separator"),
            body,
        },
        "bind" => DynNode::Bind {
            name: get("name").unwrap_or_default(),
            value: get("value").unwrap_or_default(),
        },
        "include" => DynNode::Include {
            refid: get("refid").unwrap_or_default(),
        },
        "trim" => DynNode::Trim {
            prefix: get("prefix"),
            suffix: get("suffix"),
            prefix_overrides: get("prefixOverrides"),
            suffix_overrides: get("suffixOverrides"),
            body,
        },
        "where" => DynNode::Where { body },
        "set" => DynNode::Set { body },
        other => DynNode::Unknown {
            tag: other.to_string(),
            body,
        },
    }
}

// -- Condition normalization ---------------------------------------------------

pub fn normalize_condition(s: &str) -> String {
    let collapsed = collapse_ws(s);
    let step1 = not_null_re().replace_all(&collapsed, "$1 IS NOT NULL");
    let step2 = is_null_re().replace_all(&step1, "$1 IS NULL");
    step2.replace('\'', "\"")
}

fn not_null_re() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"(?i)([\w.$]+)\s*!=\s*null").expect("not_null re compiles"))
}

fn is_null_re() -> &'static regex::Regex {
    static R: OnceLock<regex::Regex> = OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"(?i)([\w.$]+)\s*==\s*null").expect("is_null re compiles"))
}

// -- Branch summary ------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BranchSummary {
    pub ifs: u32,
    pub choose_groups: u32,
    pub whens: u32,
    pub otherwises: u32,
    pub foreaches: u32,
    pub binds: u32,
    pub includes: u32,
    /// Sum of control-flow branches (if + choose_groups + whens + foreaches).
    /// Otherwises do not add a new branch — they're the fall-through of the
    /// choose group already counted.
    pub total_branches: u32,
}

pub fn summarize(stmt: &Statement) -> BranchSummary {
    let mut s = BranchSummary::default();
    walk(&stmt.body, &mut s);
    s.total_branches = s.ifs + s.choose_groups + s.whens + s.foreaches;
    s
}

fn walk(body: &[DynNode], s: &mut BranchSummary) {
    for n in body {
        match n {
            DynNode::If { body, .. } => {
                s.ifs += 1;
                walk(body, s);
            }
            DynNode::Choose { whens, otherwise } => {
                s.choose_groups += 1;
                for w in whens {
                    s.whens += 1;
                    walk(&w.body, s);
                }
                if let Some(o) = otherwise {
                    s.otherwises += 1;
                    walk(o, s);
                }
            }
            DynNode::Foreach { body, .. } => {
                s.foreaches += 1;
                walk(body, s);
            }
            DynNode::Bind { .. } => s.binds += 1,
            DynNode::Include { .. } => s.includes += 1,
            DynNode::Trim { body, .. }
            | DynNode::Where { body }
            | DynNode::Set { body }
            | DynNode::Unknown { body, .. } => walk(body, s),
            DynNode::Text(_) => {}
        }
    }
}

pub fn collect_conditions(stmt: &Statement) -> Vec<String> {
    let mut out = Vec::new();
    collect_conds_inner(&stmt.body, &mut out);
    out
}

fn collect_conds_inner(body: &[DynNode], out: &mut Vec<String>) {
    for n in body {
        match n {
            DynNode::If { test, body } => {
                out.push(normalize_condition(test));
                collect_conds_inner(body, out);
            }
            DynNode::Choose { whens, otherwise } => {
                for w in whens {
                    out.push(normalize_condition(&w.test));
                    collect_conds_inner(&w.body, out);
                }
                if let Some(o) = otherwise {
                    collect_conds_inner(o, out);
                }
            }
            DynNode::Foreach { body, .. }
            | DynNode::Trim { body, .. }
            | DynNode::Where { body }
            | DynNode::Set { body }
            | DynNode::Unknown { body, .. } => collect_conds_inner(body, out),
            DynNode::Text(_) | DynNode::Bind { .. } | DynNode::Include { .. } => {}
        }
    }
}

// -- Render --------------------------------------------------------------------

fn render_single(path: &str, stmts: &[Statement]) -> String {
    let mut out = format!(
        "MyBatisDynamicParser: {path}\nStatements: {}\n\n",
        stmts.len()
    );
    for s in stmts {
        let sum = summarize(s);
        out.push_str(&format!(
            "  [{stype} {id}]  branches={b} (if={i}, choose={c}, when={w}, otherwise={o}, foreach={f}, bind={bi}, include={inc})\n",
            stype = s.stmt_type,
            id = s.id,
            b = sum.total_branches,
            i = sum.ifs,
            c = sum.choose_groups,
            w = sum.whens,
            o = sum.otherwises,
            f = sum.foreaches,
            bi = sum.binds,
            inc = sum.includes,
        ));
        let conds = collect_conditions(s);
        if !conds.is_empty() {
            out.push_str("    conditions:\n");
            for c in &conds {
                out.push_str(&format!("      - {c}\n"));
            }
        }
    }
    out
}

fn render_compare(a_path: &str, b_path: &str, a: &[Statement], b: &[Statement]) -> String {
    use std::collections::HashSet;
    let mut out = format!("MyBatisDynamicParser: before={a_path}, after={b_path}\n\n");
    let a_keys: HashSet<(String, String)> = a
        .iter()
        .map(|s| (s.stmt_type.clone(), s.id.clone()))
        .collect();
    let b_keys: HashSet<(String, String)> = b
        .iter()
        .map(|s| (s.stmt_type.clone(), s.id.clone()))
        .collect();

    let matching = a_keys.intersection(&b_keys).count();
    let added: Vec<_> = b_keys.difference(&a_keys).collect();
    let removed: Vec<_> = a_keys.difference(&b_keys).collect();
    out.push_str(&format!(
        "Statements: {matching} matching, {} added, {} removed\n\n",
        added.len(),
        removed.len()
    ));

    for r in &removed {
        out.push_str(&format!("  − [{} {}]  (removed)\n", r.0, r.1));
    }
    for a_key in &added {
        out.push_str(&format!("  + [{} {}]  (added)\n", a_key.0, a_key.1));
    }

    for s in a {
        let Some(s_after) = b
            .iter()
            .find(|o| o.stmt_type == s.stmt_type && o.id == s.id)
        else {
            continue;
        };
        let before_sum = summarize(s);
        let after_sum = summarize(s_after);
        let before_conds: HashSet<String> = collect_conditions(s).into_iter().collect();
        let after_conds: HashSet<String> = collect_conditions(s_after).into_iter().collect();

        let cond_added: Vec<_> = after_conds.difference(&before_conds).collect();
        let cond_removed: Vec<_> = before_conds.difference(&after_conds).collect();
        let same_struct =
            before_sum == after_sum && cond_added.is_empty() && cond_removed.is_empty();
        if same_struct {
            out.push_str(&format!("  [{} {}]  identical\n", s.stmt_type, s.id));
            continue;
        }
        out.push_str(&format!(
            "  [{} {}]  branches: {} → {}  (Δif={:+}, Δchoose={:+}, Δwhen={:+}, Δforeach={:+})\n",
            s.stmt_type,
            s.id,
            before_sum.total_branches,
            after_sum.total_branches,
            i64::from(after_sum.ifs) - i64::from(before_sum.ifs),
            i64::from(after_sum.choose_groups) - i64::from(before_sum.choose_groups),
            i64::from(after_sum.whens) - i64::from(before_sum.whens),
            i64::from(after_sum.foreaches) - i64::from(before_sum.foreaches),
        ));
        if !cond_added.is_empty() {
            out.push_str("    + conditions added:\n");
            for c in &cond_added {
                out.push_str(&format!("        + {c}\n"));
            }
        }
        if !cond_removed.is_empty() {
            out.push_str("    − conditions removed:\n");
            for c in &cond_removed {
                out.push_str(&format!("        − {c}\n"));
            }
        }
    }
    out
}

// -- Tests ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Vec<Statement> {
        parse_mapper(text).expect("parse ok")
    }

    #[test]
    fn parses_simple_select_with_if() {
        let xml = r#"<mapper namespace="u">
          <select id="find">
            select * from t
            <if test="name != null">and name = #{name}</if>
          </select>
        </mapper>"#;
        let stmts = parse(xml);
        assert_eq!(stmts.len(), 1);
        let s = summarize(&stmts[0]);
        assert_eq!(s.ifs, 1);
        assert_eq!(s.total_branches, 1);
        let conds = collect_conditions(&stmts[0]);
        assert_eq!(conds, vec!["name IS NOT NULL"]);
    }

    #[test]
    fn parses_choose_when_otherwise() {
        let xml = r#"<mapper namespace="u">
          <select id="find">
            <choose>
              <when test="a == 1">A</when>
              <when test="b == 2">B</when>
              <otherwise>C</otherwise>
            </choose>
          </select>
        </mapper>"#;
        let stmts = parse(xml);
        let s = summarize(&stmts[0]);
        assert_eq!(s.choose_groups, 1);
        assert_eq!(s.whens, 2);
        assert_eq!(s.otherwises, 1);
        // 0 if + 1 choose + 2 when + 0 foreach = 3.
        assert_eq!(s.total_branches, 3);
    }

    #[test]
    fn parses_foreach_with_all_attrs() {
        let xml = r#"<mapper namespace="u">
          <insert id="batch">
            insert into t values
            <foreach collection="items" item="i" index="idx" open="(" close=")" separator=",">
              #{i}
            </foreach>
          </insert>
        </mapper>"#;
        let stmts = parse(xml);
        let s = summarize(&stmts[0]);
        assert_eq!(s.foreaches, 1);
        // Find the foreach node (after any leading text).
        let fe = stmts[0]
            .body
            .iter()
            .find(|n| matches!(n, DynNode::Foreach { .. }))
            .expect("foreach present");
        match fe {
            DynNode::Foreach {
                collection,
                item,
                index,
                open,
                close,
                separator,
                ..
            } => {
                assert_eq!(collection, "items");
                assert_eq!(item.as_deref(), Some("i"));
                assert_eq!(index.as_deref(), Some("idx"));
                assert_eq!(open.as_deref(), Some("("));
                assert_eq!(close.as_deref(), Some(")"));
                assert_eq!(separator.as_deref(), Some(","));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn parses_nested_if_inside_foreach() {
        let xml = r#"<mapper namespace="u">
          <select id="find">
            <foreach collection="xs" item="x">
              <if test="x != null">#{x}</if>
            </foreach>
          </select>
        </mapper>"#;
        let s = summarize(&parse(xml)[0]);
        assert_eq!(s.ifs, 1);
        assert_eq!(s.foreaches, 1);
    }

    #[test]
    fn include_and_bind_counted() {
        let xml = r#"<mapper namespace="u">
          <select id="find">
            <bind name="p" value="'%' + q + '%'"/>
            <include refid="common.paging"/>
          </select>
        </mapper>"#;
        let s = summarize(&parse(xml)[0]);
        assert_eq!(s.binds, 1);
        assert_eq!(s.includes, 1);
    }

    #[test]
    fn normalize_condition_canonicalizes_null_checks() {
        assert_eq!(normalize_condition("name  !=   null"), "name IS NOT NULL");
        assert_eq!(normalize_condition("name == null"), "name IS NULL");
        assert_eq!(
            normalize_condition("a != null and b == null"),
            "a IS NOT NULL and b IS NULL"
        );
    }

    #[test]
    fn normalize_condition_unifies_quote_style() {
        assert_eq!(
            normalize_condition("status == 'active'"),
            "status == \"active\""
        );
    }

    #[test]
    fn collect_conditions_includes_when_tests() {
        let xml = r#"<mapper namespace="u">
          <select id="find">
            <choose>
              <when test="a != null">x</when>
              <when test="b == null">y</when>
            </choose>
          </select>
        </mapper>"#;
        let stmts = parse(xml);
        let conds = collect_conditions(&stmts[0]);
        assert_eq!(conds, vec!["a IS NOT NULL", "b IS NULL"]);
    }

    #[test]
    fn render_single_shows_statements_and_branches() {
        let xml = r#"<mapper>
          <select id="f"><if test="x != null">a</if></select>
          <sql id="cols">id, name</sql>
        </mapper>"#;
        let s = render_single("m.xml", &parse(xml));
        assert!(s.contains("Statements: 2"));
        assert!(s.contains("[select f]"));
        assert!(s.contains("[sql cols]"));
        assert!(s.contains("if=1"));
    }

    #[test]
    fn compare_detects_identical() {
        let xml = r#"<mapper><select id="f"><if test="x != null">a</if></select></mapper>"#;
        let a = parse(xml);
        let b = parse(xml);
        let out = render_compare("a.xml", "b.xml", &a, &b);
        assert!(out.contains("identical"));
    }

    #[test]
    fn compare_detects_added_if() {
        let xml_a = r#"<mapper><select id="f"><if test="x != null">a</if></select></mapper>"#;
        let xml_b = r#"<mapper><select id="f"><if test="x != null">a</if><if test="y != null">b</if></select></mapper>"#;
        let a = parse(xml_a);
        let b = parse(xml_b);
        let out = render_compare("a.xml", "b.xml", &a, &b);
        assert!(out.contains("branches: 1 → 2"));
        assert!(out.contains("Δif=+1"));
        assert!(out.contains("y IS NOT NULL"));
    }

    #[test]
    fn compare_detects_removed_stmt() {
        let xml_a = r#"<mapper><select id="f">a</select><select id="g">b</select></mapper>"#;
        let xml_b = r#"<mapper><select id="f">a</select></mapper>"#;
        let out = render_compare("a.xml", "b.xml", &parse(xml_a), &parse(xml_b));
        assert!(out.contains("1 removed"));
        assert!(out.contains("− [select g]"));
    }

    #[test]
    fn handles_cdata_as_text() {
        let xml =
            r#"<mapper><select id="f">select <![CDATA[ * from t where a < 1 ]]></select></mapper>"#;
        let stmts = parse(xml);
        assert_eq!(stmts.len(), 1);
        let has_text = stmts[0]
            .body
            .iter()
            .any(|n| matches!(n, DynNode::Text(t) if t.contains("from t where a < 1")));
        assert!(has_text, "CDATA text lost: {:?}", stmts[0].body);
    }

    #[test]
    fn namespace_prefix_stripped() {
        let xml = r#"<ns:mapper xmlns:ns="x"><ns:select id="f"><ns:if test="a == null">q</ns:if></ns:select></ns:mapper>"#;
        let stmts = parse(xml);
        assert_eq!(stmts.len(), 1);
        let s = summarize(&stmts[0]);
        assert_eq!(s.ifs, 1);
    }

    #[test]
    fn sql_id_filter_narrows_output() {
        let xml = r#"<mapper><select id="a">x</select><select id="b"><if test="x != null">y</if></select></mapper>"#;
        let stmts = filter_by_id(parse(xml), Some("b"));
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0].id, "b");
    }

    #[test]
    fn trim_where_set_recurse_for_branch_counting() {
        let xml = r#"<mapper>
          <select id="f">
            <where>
              <if test="a != null">and a = 1</if>
              <trim prefix="AND">
                <if test="b != null">b = 2</if>
              </trim>
            </where>
          </select>
        </mapper>"#;
        let s = summarize(&parse(xml)[0]);
        assert_eq!(s.ifs, 2);
    }
}
