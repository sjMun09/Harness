//! Tiny markdown → ratatui `Text` renderer.
//!
//! Self-implemented per PLAN §3.2 ("tui-markdown 미성숙 시 자체 얇은 markdown
//! 렌더러"). Supports a deliberately minimal subset:
//!   - headings: `#`, `##`, `###`
//!   - bullet lists: `- `
//!   - bold: `**x**`
//!   - italic: `*x*`
//!   - inline code: `` `x` ``
//!   - fenced code blocks: ```` ```...``` ````
//!
//! Not supported (and renders verbatim): tables, links, images, numbered lists,
//! nested emphasis, HTML, blockquotes. Refactor candidates as iter 3 lands.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

const BOLD: Modifier = Modifier::BOLD;
const ITALIC: Modifier = Modifier::ITALIC;

/// Render a markdown string to a ratatui `Text` block.
pub fn render(input: &str) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut in_fence = false;
    let mut fence_lang: String = String::new();

    for raw in input.lines() {
        // Fenced code block boundary: `^```(lang)?$`. Strip language.
        if let Some(rest) = raw.trim_start().strip_prefix("```") {
            in_fence = !in_fence;
            if in_fence {
                fence_lang = rest.trim().to_string();
                let label = if fence_lang.is_empty() {
                    "code".to_string()
                } else {
                    fence_lang.clone()
                };
                lines.push(Line::from(Span::styled(
                    format!("┌── {label} ──"),
                    Style::default().fg(Color::DarkGray),
                )));
            } else {
                lines.push(Line::from(Span::styled(
                    "└────".to_string(),
                    Style::default().fg(Color::DarkGray),
                )));
                fence_lang.clear();
            }
            continue;
        }

        if in_fence {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(Color::Cyan),
            )));
            continue;
        }

        lines.push(render_block_line(raw));
    }

    // Unterminated fence: close it visually so the user sees something.
    if in_fence {
        lines.push(Line::from(Span::styled(
            "└────  (unterminated fence)".to_string(),
            Style::default().fg(Color::Red),
        )));
    }

    Text::from(lines)
}

fn render_block_line(raw: &str) -> Line<'static> {
    // Headings.
    if let Some(stripped) = raw.strip_prefix("### ") {
        return Line::from(Span::styled(
            stripped.to_string(),
            Style::default().fg(Color::LightYellow).add_modifier(BOLD),
        ));
    }
    if let Some(stripped) = raw.strip_prefix("## ") {
        return Line::from(Span::styled(
            stripped.to_string(),
            Style::default().fg(Color::Yellow).add_modifier(BOLD),
        ));
    }
    if let Some(stripped) = raw.strip_prefix("# ") {
        return Line::from(Span::styled(
            stripped.to_string(),
            Style::default().fg(Color::Magenta).add_modifier(BOLD),
        ));
    }

    // Bullet list.
    if let Some(stripped) = raw.strip_prefix("- ") {
        let mut spans = vec![Span::styled(
            "  • ".to_string(),
            Style::default().fg(Color::Green),
        )];
        spans.extend(render_inline(stripped));
        return Line::from(spans);
    }

    Line::from(render_inline(raw))
}

/// Inline emphasis tokenizer. Walks bytes; emits `Span`s.
///
/// Precedence: backtick > `**` > `*`. Unclosed markers render verbatim.
pub fn render_inline(s: &str) -> Vec<Span<'static>> {
    let bytes = s.as_bytes();
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    let push_buf = |buf: &mut String, out: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            out.push(Span::raw(std::mem::take(buf)));
        }
    };

    while i < bytes.len() {
        let b = bytes[i];

        // Inline code: `…`
        if b == b'`' {
            if let Some(end) = find_byte(&bytes[i + 1..], b'`') {
                let code = &s[i + 1..i + 1 + end];
                push_buf(&mut buf, &mut out);
                out.push(Span::styled(
                    code.to_string(),
                    Style::default()
                        .fg(Color::Cyan)
                        .bg(Color::Rgb(30, 30, 30)),
                ));
                i = i + 1 + end + 1;
                continue;
            }
        }

        // Bold: **…**
        if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            if let Some(end) = find_double_star(&bytes[i + 2..]) {
                let inner = &s[i + 2..i + 2 + end];
                push_buf(&mut buf, &mut out);
                // Recurse for nested italic/code inside bold.
                for span in render_inline(inner) {
                    let style = span.style.add_modifier(BOLD);
                    out.push(Span::styled(span.content.into_owned(), style));
                }
                i = i + 2 + end + 2;
                continue;
            }
        }

        // Italic: *…* — but not `**` (handled above).
        if b == b'*' && (i + 1 >= bytes.len() || bytes[i + 1] != b'*') {
            if let Some(end) = find_single_star(&bytes[i + 1..]) {
                let inner = &s[i + 1..i + 1 + end];
                push_buf(&mut buf, &mut out);
                for span in render_inline(inner) {
                    let style = span.style.add_modifier(ITALIC);
                    out.push(Span::styled(span.content.into_owned(), style));
                }
                i = i + 1 + end + 1;
                continue;
            }
        }

        // Default: append as plain text. Use char_indices to keep UTF-8 intact.
        let Some(ch) = s[i..].chars().next() else {
            break;
        };
        buf.push(ch);
        i += ch.len_utf8();
    }
    push_buf(&mut buf, &mut out);
    out
}

fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}

/// Find the next `**` not preceded by a backslash. Returns offset of the first `*`.
fn find_double_star(haystack: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 1 < haystack.len() {
        if haystack[i] == b'*' && haystack[i + 1] == b'*' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Find next single `*` that isn't part of `**`.
fn find_single_star(haystack: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i] == b'*' {
            // Skip if followed by another `*` (that's a `**` sequence, not the closer).
            if i + 1 < haystack.len() && haystack[i + 1] == b'*' {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn flat(text: &Text<'static>) -> String {
        text.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_inline_bold() {
        let t = render("hello **world** end");
        assert_eq!(flat(&t), "hello world end");
        let line = &t.lines[0];
        // Expect: "hello ", "world" (bold), " end".
        assert!(line
            .spans
            .iter()
            .any(|s| s.content == "world" && s.style.add_modifier.contains(BOLD)));
    }

    #[test]
    fn renders_inline_italic_and_code() {
        let t = render("a *italic* b `code` c");
        let line = &t.lines[0];
        assert!(line
            .spans
            .iter()
            .any(|s| s.content == "italic" && s.style.add_modifier.contains(ITALIC)));
        assert!(line.spans.iter().any(|s| s.content == "code"));
    }

    #[test]
    fn renders_fenced_code_block() {
        let md = "before\n```rust\nfn main() {}\n```\nafter";
        let t = render(md);
        // Expect: before, ┌── rust ──, fn main() {}, └────, after = 5 lines.
        assert_eq!(t.lines.len(), 5);
        assert!(t.lines[1].spans[0].content.contains("rust"));
        assert_eq!(t.lines[2].spans[0].content, "fn main() {}");
    }

    #[test]
    fn renders_bullet_list() {
        let t = render("- one\n- two");
        assert_eq!(t.lines.len(), 2);
        assert!(t.lines[0].spans[0].content.contains("•"));
        assert!(t.lines[0]
            .spans
            .iter()
            .any(|s| s.content.as_ref() == "one"));
    }

    #[test]
    fn renders_headings() {
        let t = render("# h1\n## h2\n### h3");
        assert_eq!(t.lines.len(), 3);
        assert_eq!(t.lines[0].spans[0].content, "h1");
        assert_eq!(t.lines[1].spans[0].content, "h2");
        assert_eq!(t.lines[2].spans[0].content, "h3");
    }

    #[test]
    fn unterminated_fence_does_not_panic() {
        let t = render("```\nincomplete");
        assert!(t.lines.len() >= 2);
    }

    #[test]
    fn unmatched_emphasis_renders_verbatim() {
        let t = render("a *b c");
        assert_eq!(flat(&t), "a *b c");
    }

    #[test]
    fn handles_utf8_in_emphasis() {
        let t = render("**한글** ok");
        assert!(t.lines[0]
            .spans
            .iter()
            .any(|s| s.content == "한글" && s.style.add_modifier.contains(BOLD)));
    }
}
