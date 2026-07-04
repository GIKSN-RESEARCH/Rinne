//! Markdown → HTML rendering for learn artifacts.
//!
//! Standard markdown renders via `pulldown-cmark`. Two deviations: a fenced
//! ```mermaid``` block becomes `<pre class="mermaid">` (escaped, so
//! `</pre><script>` breakout is impossible; mermaid.js reads the decoded
//! `textContent` so `--&gt;` is transparent to it), and raw HTML in prose is
//! escaped rather than passed through.

use pulldown_cmark::{html, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// Escape the HTML-significant characters (`&`, `<`, `>`, `"`, `'`). Kept
/// local so the module is self-contained; matches the escaper in `render.rs`.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Allow only benign URL schemes; neutralize everything else to `#`.
/// Permits http(s), mailto, relative (`/…`, `./…`, `#…`), and protocol-relative-safe paths.
fn safe_url(url: &str) -> String {
    let lower = url.trim_start().to_ascii_lowercase();
    let ok = lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || url.starts_with('#')
        || url.starts_with('/')
        || url.starts_with("./")
        || url.starts_with("../")
        || !lower.contains(':'); // scheme-less relative refs (e.g. `page.html`)
    if ok { url.to_string() } else { "#".to_string() }
}

pub fn render_markdown(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);

    let mut out = String::new();
    let mut passthrough: Vec<Event> = Vec::new();
    let mut mermaid_buf: Option<String> = None;

    fn flush(events: &mut Vec<Event>, out: &mut String) {
        if !events.is_empty() {
            html::push_html(out, events.drain(..));
        }
    }

    for ev in Parser::new_ext(md, opts) {
        match ev {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang)))
                if lang.as_ref() == "mermaid" =>
            {
                flush(&mut passthrough, &mut out);
                mermaid_buf = Some(String::new());
            }
            Event::End(TagEnd::CodeBlock) if mermaid_buf.is_some() => {
                let src = mermaid_buf.take().unwrap_or_default();
                out.push_str("<pre class=\"mermaid\">");
                out.push_str(&esc(&src));
                out.push_str("</pre>\n");
            }
            Event::Text(t) if mermaid_buf.is_some() => {
                mermaid_buf.as_mut().unwrap().push_str(&t);
            }
            Event::Html(h) | Event::InlineHtml(h) => {
                flush(&mut passthrough, &mut out);
                out.push_str(&esc(&h));
            }
            Event::Start(Tag::Link { link_type, dest_url, title, id }) => {
                passthrough.push(Event::Start(Tag::Link {
                    link_type,
                    dest_url: safe_url(&dest_url).into(),
                    title,
                    id,
                }));
            }
            Event::Start(Tag::Image { link_type, dest_url, title, id }) => {
                passthrough.push(Event::Start(Tag::Image {
                    link_type,
                    dest_url: safe_url(&dest_url).into(),
                    title,
                    id,
                }));
            }
            other => passthrough.push(other),
        }
    }
    flush(&mut passthrough, &mut out);
    out
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
    fn mermaid_fence_becomes_pre_mermaid_escaped() {
        let md = "```mermaid\nflowchart LR\n  A --> B\n```";
        let html = render_markdown(md);
        assert!(html.contains("<pre class=\"mermaid\">"), "got: {html}");
        // Arrows are HTML-escaped; mermaid decodes textContent back to `-->`.
        assert!(html.contains("A --&gt; B"), "got: {html}");
    }

    #[test]
    fn mermaid_fence_cannot_break_out_of_pre() {
        let md = "```mermaid\n</pre><script>alert(1)</script>\n```";
        let html = render_markdown(md);
        assert!(!html.contains("<script>"), "live script leaked: {html}");
        assert!(html.contains("&lt;/pre&gt;&lt;script&gt;"), "not escaped: {html}");
    }

    #[test]
    fn rust_fence_stays_escaped_code() {
        let md = "```rust\nfn f() -> u8 { 1 }\n```";
        let html = render_markdown(md);
        assert!(html.contains("<pre><code"), "got: {html}");
        // Angle brackets in code are escaped by pulldown-cmark.
        assert!(html.contains("&gt;"), "code angle-bracket not escaped: {html}");
        assert!(!html.contains("class=\"mermaid\""));
    }

    #[test]
    fn raw_html_in_prose_is_escaped() {
        let html = render_markdown("hello <script>alert(1)</script> world");
        assert!(html.contains("&lt;script&gt;"), "script not escaped: {html}");
        assert!(!html.contains("<script>"), "live script tag leaked: {html}");
    }

    #[test]
    fn javascript_url_scheme_is_neutralized() {
        let html = render_markdown("[click](javascript:alert(1))");
        assert!(!html.contains("javascript:"), "js url leaked: {html}");
        assert!(html.contains("href=\"#\""), "not neutralized: {html}");
    }

    #[test]
    fn data_url_image_is_neutralized() {
        let html = render_markdown("![x](data:text/html,<script>alert(1)</script>)");
        assert!(!html.contains("data:text/html"), "data url leaked: {html}");
    }

    #[test]
    fn http_and_relative_urls_survive() {
        let html = render_markdown("[a](https://example.com) and [b](./page.md)");
        assert!(html.contains("https://example.com"), "https dropped: {html}");
        assert!(html.contains("./page.md"), "relative dropped: {html}");
    }
}
