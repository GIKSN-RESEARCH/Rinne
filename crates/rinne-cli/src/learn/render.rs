//! Rendering of learned documents and narrations.

use crate::learn::{LearnDoc, Narration};

#[allow(dead_code)]
fn esc(s: &str) -> String {
    // Escape & FIRST to avoid double-escaping subsequent substitutions.
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[allow(dead_code)]
pub fn render_html(doc: &LearnDoc, narration: Option<&Narration>) -> String {
    let overview = narration
        .map(|n| esc(&n.overview))
        .unwrap_or_else(|| {
            "Structural overview (run with a configured worker for narration).".into()
        });

    let mut html = String::new();

    html.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"UTF-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n");
    html.push_str(&format!("<title>{}</title>\n", esc(&doc.topic)));
    html.push_str("<style>\n");
    html.push_str(include_style());
    html.push_str("</style>\n</head>\n<body>\n");

    // Header
    html.push_str(&format!("<h1>{}</h1>\n", esc(&doc.topic)));

    // Overview section
    html.push_str("<section id=\"overview\">\n");
    html.push_str("<h2>Overview</h2>\n");
    html.push_str(&format!("<p>{overview}</p>\n"));
    html.push_str("</section>\n");

    // Components / architecture section
    html.push_str("<section id=\"components\">\n");
    html.push_str("<h2>Components</h2>\n");
    if doc.snippets.is_empty() {
        html.push_str("<p>No components found.</p>\n");
    } else {
        html.push_str("<ul>\n");
        for s in &doc.snippets {
            html.push_str(&format!(
                "<li><code>{}</code> — <span class=\"file\">{}</span> line {}</li>\n",
                esc(&s.symbol),
                esc(&s.file),
                s.line,
            ));
        }
        html.push_str("</ul>\n");
    }
    html.push_str("</section>\n");

    // Call-flow section
    html.push_str("<section id=\"flow\">\n");
    html.push_str("<h2>Call Flow</h2>\n");
    if doc.flow.is_empty() {
        html.push_str("<p>No call flow recorded.</p>\n");
    } else {
        html.push_str("<ul>\n");
        for (caller, callee) in &doc.flow {
            html.push_str(&format!(
                "<li><code>{}</code> &rarr; <code>{}</code></li>\n",
                esc(caller),
                esc(callee),
            ));
        }
        html.push_str("</ul>\n");
    }
    html.push_str("</section>\n");

    // Code snippets section
    html.push_str("<section id=\"snippets\">\n");
    html.push_str("<h2>Code Snippets</h2>\n");
    for s in &doc.snippets {
        html.push_str("<article class=\"snippet\">\n");
        html.push_str(&format!("<h3>{}</h3>\n", esc(&s.symbol)));
        if !s.doc.is_empty() {
            html.push_str(&format!("<p class=\"doc\">{}</p>\n", esc(&s.doc)));
        }
        html.push_str(&format!("<pre><code>{}</code></pre>\n", esc(&s.code)));
        html.push_str("</article>\n");
    }
    html.push_str("</section>\n");

    // Design doc sections
    if !doc.doc_sections.is_empty() {
        html.push_str("<section id=\"design\">\n");
        html.push_str("<h2>Design References</h2>\n");
        for ds in &doc.doc_sections {
            html.push_str("<article class=\"doc-section\">\n");
            html.push_str(&format!(
                "<h3>{} <span class=\"source\">({})</span></h3>\n",
                esc(&ds.heading),
                esc(&ds.source),
            ));
            html.push_str(&format!("<p>{}</p>\n", esc(&ds.body)));
            html.push_str("</article>\n");
        }
        html.push_str("</section>\n");
    }

    html.push_str("</body>\n</html>");
    html
}

#[allow(dead_code)]
fn include_style() -> &'static str {
    r#"
body { font-family: system-ui, sans-serif; max-width: 900px; margin: 2rem auto; padding: 0 1rem; color: #1a1a1a; line-height: 1.6; }
h1 { border-bottom: 2px solid #333; padding-bottom: .4rem; }
h2 { color: #444; margin-top: 2rem; }
h3 { color: #555; }
pre { background: #f5f5f5; border: 1px solid #ddd; border-radius: 4px; padding: 1rem; overflow-x: auto; }
code { font-family: ui-monospace, monospace; font-size: .9em; }
.file { color: #666; font-style: italic; }
.doc { color: #555; border-left: 3px solid #ccc; padding-left: .75rem; margin-bottom: .5rem; }
.source { font-size: .85em; color: #888; }
section { margin-bottom: 2.5rem; }
article.snippet, article.doc-section { margin-bottom: 1.5rem; border-bottom: 1px solid #eee; padding-bottom: 1rem; }
ul { padding-left: 1.5rem; }
li { margin: .3rem 0; }
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::{DocSection, LearnDoc, Snippet};

    #[test]
    fn renders_self_contained_html_with_real_symbols() {
        let doc = LearnDoc {
            topic: "harness".into(),
            snippets: vec![Snippet {
                symbol: "HarnessAdapter".into(),
                file: "common.rs".into(),
                line: 10,
                code: "fn run() { /* <b> */ }".into(),
                doc: "/// runs a node".into(),
            }],
            flow: vec![("run_node".into(), "HarnessAdapter".into())],
            doc_sections: vec![DocSection {
                source: "CONTEXT.md".into(),
                heading: "§8".into(),
                body: "the why".into(),
            }],
        };
        let html = render_html(&doc, None);
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("HarnessAdapter"));
        assert!(html.contains("run_node"));
        assert!(html.contains("the why"));
        // Self-contained: no network refs.
        assert!(!html.contains("src=\"http"));
        assert!(!html.contains("href=\"http"));
        // Escaped: raw <b> from code must not appear as a live tag.
        assert!(html.contains("&lt;b&gt;"));
    }
}
