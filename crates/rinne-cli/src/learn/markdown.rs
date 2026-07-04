//! Markdown → HTML rendering for learn artifacts.
//!
//! Standard markdown renders via `pulldown-cmark`. Two deviations: a fenced
//! ```mermaid``` block becomes `<pre class="mermaid">` (raw, so mermaid.js can
//! draw it), and raw HTML in prose is escaped rather than passed through.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Escape the five HTML-significant characters. Kept local so the module is
/// self-contained; matches the escaper in `render.rs`.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn render_markdown(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);

    let mut out = String::new();
    let mut mermaid_buf: Option<String> = None;

    for ev in Parser::new_ext(md, opts) {
        match ev {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang)))
                if lang.as_ref() == "mermaid" =>
            {
                mermaid_buf = Some(String::new());
            }
            Event::End(TagEnd::CodeBlock) if mermaid_buf.is_some() => {
                let src = mermaid_buf.take().unwrap_or_default();
                out.push_str("<pre class=\"mermaid\">");
                out.push_str(&src);
                out.push_str("</pre>\n");
            }
            Event::Text(t) if mermaid_buf.is_some() => {
                mermaid_buf.as_mut().unwrap().push_str(&t);
            }
            // Raw HTML must not be trusted; escape and emit as text.
            Event::Html(h) | Event::InlineHtml(h) => {
                out.push_str(&esc(&h));
            }
            Event::Start(tag) => push_open_tag(&mut out, tag),
            Event::End(tag) => push_close_tag(&mut out, tag),
            Event::Text(t) => out.push_str(&esc(&t)),
            Event::Code(t) => {
                out.push_str("<code>");
                out.push_str(&esc(&t));
                out.push_str("</code>");
            }
            Event::SoftBreak => out.push('\n'),
            Event::HardBreak => out.push_str("<br />\n"),
            Event::Rule => out.push_str("<hr />\n"),
            _ => {}
        }
    }
    out
}

fn heading_tag(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "h1",
        HeadingLevel::H2 => "h2",
        HeadingLevel::H3 => "h3",
        HeadingLevel::H4 => "h4",
        HeadingLevel::H5 => "h5",
        HeadingLevel::H6 => "h6",
    }
}

fn push_open_tag(out: &mut String, tag: Tag<'_>) {
    match tag {
        Tag::Paragraph => out.push_str("<p>"),
        Tag::Heading { level, .. } => {
            out.push('<');
            out.push_str(heading_tag(level));
            out.push('>');
        }
        Tag::BlockQuote(_) => out.push_str("<blockquote>\n"),
        Tag::CodeBlock(CodeBlockKind::Fenced(lang)) => {
            out.push_str("<pre><code");
            if !lang.is_empty() {
                out.push_str(" class=\"language-");
                out.push_str(&esc(&lang));
                out.push_str("\"");
            }
            out.push('>');
        }
        Tag::CodeBlock(CodeBlockKind::Indented) => out.push_str("<pre><code>"),
        Tag::List(Some(start)) => {
            out.push_str("<ol");
            if start != 1 {
                out.push_str(&format!(" start=\"{start}\""));
            }
            out.push_str(">\n");
        }
        Tag::List(None) => out.push_str("<ul>\n"),
        Tag::Item => out.push_str("<li>"),
        Tag::Emphasis => out.push_str("<em>"),
        Tag::Strong => out.push_str("<strong>"),
        Tag::Strikethrough => out.push_str("<del>"),
        Tag::Link { dest_url, title, .. } => {
            out.push_str("<a href=\"");
            out.push_str(&esc(&dest_url));
            out.push('"');
            if !title.is_empty() {
                out.push_str(" title=\"");
                out.push_str(&esc(&title));
                out.push('"');
            }
            out.push('>');
        }
        Tag::Image { dest_url, title, .. } => {
            out.push_str("<img src=\"");
            out.push_str(&esc(&dest_url));
            out.push('"');
            if !title.is_empty() {
                out.push_str(" alt=\"");
                out.push_str(&esc(&title));
                out.push('"');
            }
            out.push_str(" />");
        }
        Tag::Table(_) => out.push_str("<table>\n"),
        Tag::TableHead => out.push_str("<thead><tr>"),
        Tag::TableRow => out.push_str("<tr>"),
        Tag::TableCell => out.push_str("<td>"),
        _ => {}
    }
}

fn push_close_tag(out: &mut String, tag: TagEnd) {
    match tag {
        TagEnd::Paragraph => out.push_str("</p>\n"),
        TagEnd::Heading(level) => {
            out.push_str("</");
            out.push_str(heading_tag(level));
            out.push_str(">\n");
        }
        TagEnd::BlockQuote(_) => out.push_str("</blockquote>\n"),
        TagEnd::CodeBlock => out.push_str("</code></pre>\n"),
        TagEnd::List(true) => out.push_str("</ol>\n"),
        TagEnd::List(false) => out.push_str("</ul>\n"),
        TagEnd::Item => out.push_str("</li>\n"),
        TagEnd::Emphasis => out.push_str("</em>"),
        TagEnd::Strong => out.push_str("</strong>"),
        TagEnd::Strikethrough => out.push_str("</del>"),
        TagEnd::Link => out.push_str("</a>"),
        TagEnd::Image => {}
        TagEnd::Table => out.push_str("</table>\n"),
        TagEnd::TableHead => out.push_str("</tr></thead>\n"),
        TagEnd::TableRow => out.push_str("</tr>\n"),
        TagEnd::TableCell => out.push_str("</td>"),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_renders_as_h1() {
        let html = render_markdown("# Title");
        assert!(html.contains("<h1>"), "got: {html}");
        assert!(html.contains("Title"));
    }

    #[test]
    fn mermaid_fence_becomes_pre_mermaid_unescaped() {
        let md = "```mermaid\nflowchart LR\n  A --> B\n```";
        let html = render_markdown(md);
        assert!(html.contains("<pre class=\"mermaid\">"), "got: {html}");
        // Diagram source stays raw so mermaid can parse arrows.
        assert!(html.contains("A --> B"), "got: {html}");
        // Must NOT be wrapped as an escaped code block.
        assert!(!html.contains("A --&gt; B"), "mermaid was escaped: {html}");
    }

    #[test]
    fn rust_fence_stays_escaped_code() {
        let md = "```rust\nfn f() -> u8 { 1 }\n```";
        let html = render_markdown(md);
        assert!(html.contains("<pre><code"), "got: {html}");
        // Angle brackets in code are escaped by pulldown-cmark.
        assert!(html.contains("&gt;") || html.contains("fn f()"), "got: {html}");
        assert!(!html.contains("class=\"mermaid\""));
    }

    #[test]
    fn raw_html_in_prose_is_escaped() {
        let html = render_markdown("hello <script>alert(1)</script> world");
        assert!(html.contains("&lt;script&gt;"), "script not escaped: {html}");
        assert!(!html.contains("<script>"), "live script tag leaked: {html}");
    }
}
