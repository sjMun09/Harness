//! Cross-tool helpers: output truncation, input parsing, cancel-aware waits.

use harness_core::ToolError;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// Per-side head/tail cap — matches PLAN §3.1 Bash spec (4 KiB head + 4 KiB tail).
pub const HEAD_TAIL_CAP: usize = 4 * 1024;

/// Collapse a long string into `head + <elision marker> + tail` if oversize.
#[must_use]
pub fn head_tail(input: &str, cap: usize) -> String {
    if input.len() <= cap * 2 {
        return input.to_string();
    }
    let head_end = floor_char_boundary(input, cap);
    let tail_start = ceil_char_boundary(input, input.len() - cap);
    let elided = input.len() - head_end - (input.len() - tail_start);
    format!(
        "{}\n... [truncated {} bytes] ...\n{}",
        &input[..head_end],
        elided,
        &input[tail_start..]
    )
}

fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let idx = idx.min(s.len());
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Parse input JSON with a uniform `Validation` error on failure.
pub fn parse_input<T: DeserializeOwned>(input: Value, tool: &str) -> Result<T, ToolError> {
    serde_json::from_value::<T>(input)
        .map_err(|e| ToolError::Validation(format!("{tool}: {e}")))
}

/// Wrap raw tool output in `<untrusted_tool_output tool="..." path="...">…</untrusted_tool_output>`.
/// Path is optional — omit the attr when absent.
#[must_use]
pub fn fence_tool_output(tool: &str, path: Option<&str>, body: &str) -> String {
    let tool_escaped = tool.replace('"', "&quot;");
    let open_tag = match path {
        Some(p) => {
            let path_escaped = p.replace('"', "&quot;");
            format!(r#"<untrusted_tool_output tool="{tool_escaped}" path="{path_escaped}">"#)
        }
        None => format!(r#"<untrusted_tool_output tool="{tool_escaped}">"#),
    };
    format!("{open_tag}\n{body}\n</untrusted_tool_output>\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_tail_short_input_passthrough() {
        assert_eq!(head_tail("hi", 1024), "hi");
    }

    #[test]
    fn head_tail_elides_middle() {
        let s = "a".repeat(10_000);
        let trimmed = head_tail(&s, 100);
        assert!(trimmed.contains("truncated"));
        assert!(trimmed.starts_with("aaa"));
    }

    #[test]
    fn head_tail_honours_utf8_boundary() {
        // 3-byte char "한" (U+D55C). Cap inside the char must not split it.
        let s = "한".repeat(10);
        let trimmed = head_tail(&s, 4);
        assert!(trimmed.is_char_boundary(trimmed.len()));
    }

    #[test]
    fn fence_tool_output_with_path() {
        let result = fence_tool_output("Read", Some("src/main.rs"), "line content");
        assert!(result.starts_with(r#"<untrusted_tool_output tool="Read" path="src/main.rs">"#));
        assert!(result.contains("line content"));
        assert!(result.contains("</untrusted_tool_output>"));
        assert!(result.ends_with('\n'));
    }

    #[test]
    fn fence_tool_output_without_path() {
        let result = fence_tool_output("Grep", None, "match result");
        assert!(result.starts_with(r#"<untrusted_tool_output tool="Grep">"#));
        assert!(!result.contains("path="));
        assert!(result.contains("match result"));
        assert!(result.contains("</untrusted_tool_output>"));
        assert!(result.ends_with('\n'));
    }

    #[test]
    fn fence_tool_output_escapes_quotes_in_attrs() {
        let result = fence_tool_output(r#"bad"tool"#, Some(r#"bad"path"#), "body");
        assert!(result.contains(r#"tool="bad&quot;tool""#));
        assert!(result.contains(r#"path="bad&quot;path""#));
    }

    #[test]
    fn fence_tool_output_wraps_empty_body() {
        let result = fence_tool_output("Bash", None, "");
        assert!(result.contains("<untrusted_tool_output"));
        assert!(result.contains("</untrusted_tool_output>"));
    }
}
