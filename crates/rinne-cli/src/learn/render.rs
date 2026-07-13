//! Rendering of learned documents and narrations.

use crate::learn::{LearnDoc, Narration};
use crate::learn::markdown::render_markdown;

fn esc(s: &str) -> String {
    // Escape & FIRST to avoid double-escaping subsequent substitutions.
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Build a Mermaid `flowchart LR` from caller→callee pairs.
///
/// Mermaid node IDs can't contain `::`, spaces, or angle brackets, so each
/// unique symbol name is mapped to a safe `n{index}` id with the original name
/// kept as the bracketed label. Returns an empty string for empty input.
fn flow_mermaid(flow: &[(String, String)]) -> String {
    if flow.is_empty() {
        return String::new();
    }

    let mut ids: Vec<(String, String)> = Vec::new(); // (name, id)
    let id_of = |name: &str, ids: &mut Vec<(String, String)>| -> String {
        if let Some((_, id)) = ids.iter().find(|(n, _)| n == name) {
            return id.clone();
        }
        let id = format!("n{}", ids.len());
        ids.push((name.to_string(), id.clone()));
        id
    };

    let mut edges = String::new();
    for (caller, callee) in flow {
        let a = id_of(caller, &mut ids);
        let b = id_of(callee, &mut ids);
        edges.push_str(&format!("  {a} --> {b}\n"));
    }

    // Mermaid labels: quote to survive `.`, `<`, etc. Escape quotes in names.
    let mut nodes = String::new();
    for (name, id) in &ids {
        let label = esc(name);
        nodes.push_str(&format!("  {id}[\"{label}\"]\n"));
    }

    format!("<pre class=\"mermaid\">flowchart LR\n{nodes}{edges}</pre>\n")
}

pub fn render_html(doc: &LearnDoc, narration: Option<&Narration>) -> String {
    let overview = narration
        .map(|n| render_markdown(&n.overview))
        .unwrap_or_else(|| {
            "<p>Structural overview (run with a configured worker for narration).</p>".into()
        });

    let mut html = String::new();

    html.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"UTF-8\">\n");
    html.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n");
    html.push_str(&format!("<title>{}</title>\n", esc(&doc.topic)));
    html.push_str("<style>\n");
    html.push_str(include_style());
    html.push_str("</style>\n</head>\n<body>\n<main>\n");

    // Header
    html.push_str("<header class=\"masthead\">\n");
    html.push_str("<p class=\"eyebrow\">rinne learn</p>\n");
    html.push_str(&format!("<h1>{}</h1>\n", esc(&doc.topic)));
    html.push_str(
        "<p class=\"lede\">Product context, the business journey this code serves, the rules \
         and exceptions that encode how the company works, and the domain concepts you need — \
         grounded in the real symbols.</p>\n",
    );
    html.push_str("</header>\n");

    // 1. Overview — product problem + journey placement.
    html.push_str("<section id=\"overview\">\n");
    html.push_str("<p class=\"section-label\">Overview</p>\n");
    html.push_str(&overview);
    html.push('\n');
    html.push_str("</section>\n");

    // 2. Business rules / conditions (AI-only; hidden when absent).
    if let Some(dec) = narration.map(|n| n.decisions.trim()).filter(|d| !d.is_empty()) {
        html.push_str("<section id=\"decisions\">\n");
        html.push_str("<p class=\"section-label\">Business Rules</p>\n");
        html.push_str(&render_markdown(dec));
        html.push('\n');
        html.push_str("</section>\n");
    }

    // 3. Domain + design concepts (AI-only; hidden when absent).
    if let Some(con) = narration.map(|n| n.concepts.trim()).filter(|c| !c.is_empty()) {
        html.push_str("<section id=\"concepts\">\n");
        html.push_str("<p class=\"section-label\">Domain Concepts</p>\n");
        html.push_str(&render_markdown(con));
        html.push('\n');
        html.push_str("</section>\n");
    }

    // 4. Rationale from the repo's own design docs (deterministic, offline).
    if !doc.doc_sections.is_empty() {
        html.push_str("<section id=\"design\">\n");
        html.push_str("<p class=\"section-label\">Rationale</p>\n");
        for ds in &doc.doc_sections {
            html.push_str("<article class=\"doc-section\">\n");
            html.push_str(&format!(
                "<h3>{} <span class=\"source\">({})</span></h3>\n",
                esc(&ds.heading),
                esc(&ds.source),
            ));
            html.push_str(&render_markdown(&ds.body));
            html.push('\n');
            html.push_str("</article>\n");
        }
        html.push_str("</section>\n");
    }

    // 5. Call flow.
    html.push_str("<section id=\"flow\">\n");
    html.push_str("<p class=\"section-label\">Call Flow</p>\n");
    if doc.flow.is_empty() {
        html.push_str("<p class=\"empty\">No call flow recorded.</p>\n");
    } else {
        html.push_str(&flow_mermaid(&doc.flow));
    }
    html.push_str("</section>\n");

    // 6. Source reference — evidence, collapsed by default so understanding leads.
    if !doc.snippets.is_empty() {
        html.push_str("<section id=\"source\">\n");
        html.push_str("<details class=\"source-ref\">\n");
        html.push_str(&format!(
            "<summary><span class=\"section-label\">Source reference</span>\
             <span class=\"count\">{} symbols</span></summary>\n",
            doc.snippets.len(),
        ));

        // Symbol index inside the collapsed block.
        html.push_str("<ul class=\"symbol-index\">\n");
        for s in &doc.snippets {
            html.push_str(&format!(
                "<li><code>{}</code> <span class=\"file\">{} line {}</span></li>\n",
                esc(&s.symbol),
                esc(&s.file),
                s.line,
            ));
        }
        html.push_str("</ul>\n");

        for s in &doc.snippets {
            html.push_str("<article class=\"snippet\">\n");
            html.push_str(&format!("<h3>{}</h3>\n", esc(&s.symbol)));
            if !s.doc.is_empty() {
                html.push_str(&format!("<p class=\"doc\">{}</p>\n", esc(&s.doc)));
            }
            html.push_str(&format!("<pre><code>{}</code></pre>\n", esc(&s.code)));
            html.push_str("</article>\n");
        }
        html.push_str("</details>\n");
        html.push_str("</section>\n");
    }

    html.push_str("</main>\n");

    if html.contains("class=\"mermaid\"") {
        // Theme the diagram dark so it sits on the slate ground instead of
        // glaring as a white box.
        html.push_str(
            "<script type=\"module\">import mermaid from \
             \"https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs\";\
             mermaid.initialize({startOnLoad:true,theme:\"dark\"});</script>\n",
        );
    }

    html.push_str("</body>\n</html>");
    html
}

fn include_style() -> &'static str {
    // Design language: a terminal readout, because that's where Rinne lives.
    // A cool slate ground (NOT the near-black+acid-green AI default) carries
    // warm paper-serif prose for the "why"; every machine fact is mono. The
    // signature is a left SPINE rail — each section label hangs off it as an
    // amber tick, so the sections read as nodes on one graph (lenses on a
    // single subsystem), not independent chapters. Amber (phosphor-terminal
    // lineage) is the deliberate accent instead of the cliché acid green.
    // Self-contained; system font stacks only.
    r#"
:root {
  --bg: #0f1620;
  --panel: #161f2b;
  --panel-2: #1b2634;
  --prose: #e6e4de;
  --muted: #9aa5b1;
  --faint: #5f6b78;
  --hairline: #26313f;
  --amber: #d8a657;
  --amber-dim: #7a6438;
  --serif: Charter, "Bitstream Charter", "Sitka Text", Cambria, Georgia, serif;
  --mono: ui-monospace, "SF Mono", SFMono-Regular, Menlo, "Cascadia Mono", Consolas, monospace;
  --measure: 68ch;
  --rail: 2.25rem; /* gutter reserved for the spine + ticks */
}

* { box-sizing: border-box; }

body {
  font-family: var(--serif);
  background: var(--bg);
  color: var(--prose);
  max-width: 900px;
  margin: 0 auto;
  padding: 0 1.75rem 6rem;
  line-height: 1.72;
  font-size: 1.03rem;
  -webkit-font-smoothing: antialiased;
  text-rendering: optimizeLegibility;
}

/* The spine: a single amber-dim rail down the whole reading column, with a
   soft glow. Sections sit to its right; each label puts an amber node on it. */
main { position: relative; padding-left: var(--rail); }
main::before {
  content: "";
  position: absolute;
  left: .55rem; top: .4rem; bottom: .4rem;
  width: 1px;
  background: linear-gradient(var(--amber-dim), var(--hairline) 60%, transparent);
}

/* Masthead ------------------------------------------------------------- */
.masthead { padding: 3.5rem 0 2.5rem; margin-bottom: 2.5rem; position: relative; }
.eyebrow {
  font-family: var(--mono);
  font-size: .72rem;
  letter-spacing: .24em;
  text-transform: uppercase;
  color: var(--amber);
  margin: 0 0 1rem;
}
h1 {
  font-family: var(--mono);
  font-weight: 600;
  font-size: clamp(2.2rem, 6.5vw, 3.4rem);
  line-height: 1.02;
  letter-spacing: -.015em;
  margin: 0;
  color: #f4f2ec;
}
.lede { color: var(--muted); font-size: 1.14rem; max-width: var(--measure); margin: 1.1rem 0 0; }

/* Sections: the label is the tick on the spine ------------------------- */
section { margin: 0 0 3.75rem; position: relative; }
.section-label {
  position: relative;
  font-family: var(--mono);
  font-size: .74rem;
  letter-spacing: .22em;
  text-transform: uppercase;
  color: var(--amber);
  margin: 0 0 1.5rem;
}
/* the node: a filled amber dot centered on the spine, aligned to the label */
.section-label::before {
  content: "";
  position: absolute;
  left: calc(-1 * var(--rail) + .3rem);
  top: .34em;
  width: .5rem; height: .5rem;
  background: var(--amber);
  border-radius: 50%;
  box-shadow: 0 0 0 4px var(--bg), 0 0 8px 1px rgba(216,166,87,.5);
}

h2 { font-family: var(--mono); font-weight: 600; font-size: 1.12rem; color: #f4f2ec; margin: 2.2rem 0 .7rem; }
h3 { font-family: var(--mono); font-weight: 600; font-size: 1rem; color: var(--amber); margin: 0 0 .5rem; }
p { max-width: var(--measure); }
strong { color: #f4f2ec; }
a { color: var(--amber); text-underline-offset: 2px; text-decoration-color: var(--amber-dim); }
a:hover { text-decoration-color: var(--amber); }

/* Inline code + fenced blocks ------------------------------------------ */
code { font-family: var(--mono); font-size: .87em; }
:not(pre) > code { color: var(--amber); }
pre {
  background: var(--panel);
  border: 1px solid var(--hairline);
  border-left: 2px solid var(--amber-dim);
  border-radius: 5px;
  padding: 1.1rem 1.25rem;
  overflow-x: auto;
  font-size: .85rem;
  line-height: 1.6;
  color: #d3d8de;
}
pre code { color: inherit; }

.file { color: var(--faint); font-style: normal; font-family: var(--mono); }
.empty { color: var(--faint); font-style: italic; }

/* Source reference: collapsed by default so understanding leads -------- */
.source-ref { margin: 0; }
.source-ref > summary {
  cursor: pointer;
  list-style: none;
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 1rem;
}
.source-ref > summary::-webkit-details-marker { display: none; }
.source-ref > summary .section-label { margin: 0; }
.source-ref > summary::after {
  content: "expand";
  font-family: var(--mono);
  font-size: .7rem;
  letter-spacing: .1em;
  color: var(--faint);
}
.source-ref[open] > summary::after { content: "collapse"; }
.source-ref[open] > summary { margin-bottom: 1.75rem; }
.source-ref > summary .count { font-family: var(--mono); font-size: .74rem; color: var(--faint); }

.symbol-index { list-style: none; padding: 0; margin: 0 0 2rem; border-top: 1px solid var(--hairline); }
.symbol-index li {
  font-family: var(--mono);
  font-size: .83rem;
  padding: .5rem .25rem;
  border-bottom: 1px solid var(--hairline);
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: .6rem;
}
.symbol-index li code { color: var(--amber); }

/* Snippets + rationale cards ------------------------------------------- */
article.snippet, article.doc-section { margin: 0 0 2rem; }
.doc {
  color: var(--muted);
  border-left: 2px solid var(--amber-dim);
  padding-left: .9rem;
  margin: 0 0 .7rem;
  font-style: italic;
  max-width: var(--measure);
}
.source { font-family: var(--mono); font-size: .74rem; letter-spacing: .04em; color: var(--faint); font-weight: 400; }

/* Prose lists in narration / rationale --------------------------------- */
section ul, section ol { padding-left: 1.3rem; max-width: var(--measure); }
section li { margin: .4rem 0; }
section li::marker { color: var(--amber-dim); }

/* Tables from markdown narration --------------------------------------- */
table { border-collapse: collapse; width: 100%; font-size: .9rem; margin: 1.2rem 0; }
th, td { text-align: left; padding: .55rem .7rem; border-bottom: 1px solid var(--hairline); }
th { font-family: var(--mono); font-size: .76rem; letter-spacing: .06em; text-transform: uppercase; color: var(--amber); }
td { color: var(--muted); }

/* Blockquotes ---------------------------------------------------------- */
blockquote { margin: 1.2rem 0; padding: .2rem 0 .2rem 1.1rem; border-left: 2px solid var(--hairline); color: var(--muted); }

/* Call-flow diagram (mermaid is themed dark from the loader) ------------ */
pre.mermaid { background: none; border: none; padding: 0; text-align: center; }

@media (max-width: 640px) {
  :root { --rail: 1.5rem; }
  body { padding: 0 1.1rem 4rem; }
  main::before { left: .3rem; }
  .masthead { padding: 2.5rem 0 1.75rem; }
}

@media (prefers-reduced-motion: reduce) {
  * { animation: none !important; transition: none !important; }
}
"#
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::{DocSection, LearnDoc, Snippet};

    #[test]
    fn flow_becomes_sanitized_mermaid_flowchart() {
        let flow = vec![
            ("compose_prompt".to_string(), "render_symbol_map".to_string()),
            ("Foo::bar".to_string(), "baz".to_string()),
        ];
        let out = flow_mermaid(&flow);
        assert!(out.contains("<pre class=\"mermaid\">"), "got: {out}");
        assert!(out.contains("flowchart LR"), "got: {out}");
        // Node IDs must be sanitized: no raw `::` in an id position.
        assert!(!out.contains("Foo::bar["), "unsanitized id: {out}");
        // Human label is preserved in brackets.
        assert!(out.contains("Foo::bar"), "label lost: {out}");
        assert!(out.contains("-->"), "no edge: {out}");
    }

    #[test]
    fn empty_flow_yields_empty_mermaid() {
        assert_eq!(flow_mermaid(&[]), "");
    }

    #[test]
    fn flow_mermaid_escapes_html_in_labels() {
        let flow = vec![("Vec<T>".to_string(), "a & b".to_string())];
        let out = flow_mermaid(&flow);
        assert!(out.contains("Vec&lt;T&gt;"), "angle brackets not escaped: {out}");
        assert!(out.contains("a &amp; b"), "ampersand not escaped: {out}");
        assert!(!out.contains("Vec<T>"), "raw < leaked into label: {out}");
    }

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
        // The doc's own assets are self-contained: no external stylesheet,
        // no <img>, no <script src>/<link href> http refs.
        assert!(!html.contains("<link"), "no external stylesheet");
        assert!(!html.contains("<img"), "no external images");
        assert!(!html.contains("src=\"http"), "no <script src>/<img src> http refs");
        assert!(!html.contains("href=\"http"), "no <link href> http refs");
        // The sole permitted remote dependency is the mermaid runtime, loaded
        // as an ES-module import (not an src=/href= attribute).
        assert!(
            html.contains("cdn.jsdelivr.net/npm/mermaid"),
            "mermaid CDN import present for the flow diagram"
        );
        // Escaped: raw <b> from code must not appear as a live tag.
        assert!(html.contains("&lt;b&gt;"));
    }

    #[test]
    fn understanding_leads_and_source_is_collapsed() {
        use crate::learn::Narration;
        let doc = LearnDoc {
            topic: "harness".into(),
            snippets: vec![Snippet {
                symbol: "HarnessAdapter".into(),
                file: "common.rs".into(),
                line: 10,
                code: "fn run() {}".into(),
                doc: String::new(),
            }],
            flow: vec![],
            doc_sections: vec![],
        };
        let narration = Narration {
            overview: "It adapts workers.".into(),
            components: vec![],
            decisions: "- handles a missing worker by degrading".into(),
            concepts: "- Graceful degradation: keep working when a dep is absent".into(),
        };
        let html = render_html(&doc, Some(&narration));

        // Understanding sections render from the narration parts.
        assert!(html.contains("Business Rules"), "rules section missing");
        assert!(html.contains("degrading"), "decisions body missing");
        assert!(html.contains("Domain Concepts"), "concepts section missing");
        assert!(html.contains("Graceful degradation"), "concepts body missing");

        // Source is demoted into a collapsed <details>, after the understanding.
        assert!(html.contains("<details class=\"source-ref\">"), "source not collapsed");
        let rules_at = html.find(">Business Rules<").unwrap();
        // Match the body markup, not the CSS comment in the <style> block.
        let source_at = html.find("<details class=\"source-ref\">").unwrap();
        assert!(rules_at < source_at, "source must come after understanding");
        // The snippet still exists — as evidence, inside the collapsed block.
        assert!(html.contains("HarnessAdapter"));
    }

    #[test]
    fn no_narration_hides_understanding_sections() {
        // --no-ai path: only structural facts, no invented understanding sections.
        let doc = LearnDoc {
            topic: "t".into(),
            snippets: vec![Snippet {
                symbol: "Foo".into(), file: "f.rs".into(), line: 1,
                code: "fn foo() {}".into(), doc: String::new(),
            }],
            flow: vec![],
            doc_sections: vec![],
        };
        let html = render_html(&doc, None);
        assert!(!html.contains("Business Rules"), "rules leaked without AI");
        assert!(!html.contains("Domain Concepts"), "concepts leaked without AI");
        // Source reference is still present (structural), just collapsed.
        assert!(html.contains("source-ref"), "source ref missing");
    }

    #[test]
    fn mermaid_script_only_when_diagram_present() {
        // Doc WITH flow → mermaid block → CDN loader present.
        let with_flow = LearnDoc {
            topic: "t".into(),
            snippets: vec![],
            flow: vec![("a".into(), "b".into())],
            doc_sections: vec![],
        };
        let html = render_html(&with_flow, None);
        assert!(html.contains("class=\"mermaid\""), "no diagram: {html}");
        assert!(html.contains("mermaid.initialize"), "init missing: {html}");
        assert!(
            html.contains("cdn.jsdelivr.net/npm/mermaid"),
            "cdn loader missing: {html}"
        );

        // Doc with NO flow and NO diagram → no mermaid loader emitted.
        let no_flow = LearnDoc {
            topic: "t".into(),
            snippets: vec![],
            flow: vec![],
            doc_sections: vec![],
        };
        let html2 = render_html(&no_flow, None);
        assert!(!html2.contains("mermaid.initialize"), "init leaked: {html2}");
        assert!(
            !html2.contains("cdn.jsdelivr.net"),
            "cdn leaked into diagram-free doc: {html2}"
        );
    }
}
