//! `learn` module for knowledge synthesis and code narration.

pub mod logic;
pub mod resolve;
pub mod source;
pub mod translate;
pub mod render;
pub mod markdown;

/// A symbol within a cluster: represents a function, type, or other named entity.
#[derive(Debug, Clone)]
pub struct ClusterSymbol {
    pub name: String,
    pub file: String,
    pub line: u32,
    /// 1-based last line of the symbol's span (from the graph). Used for exact
    /// snippet extraction; falls back to `line` when unknown.
    pub end_line: u32,
    #[allow(dead_code)]
    pub kind: String,
}

/// A cluster of related symbols around a topic.
#[derive(Debug, Clone)]
pub struct Cluster {
    // Carried for serialisation and future use; the renderer uses symbols/files.
    #[allow(dead_code)]
    pub topic: String,
    /// Topic-matched / AI-picked anchors (before neighborhood expansion).
    /// Call-flow diagrams are rooted on these so the map tells a story about
    /// the query instead of a random high-degree fragment of the graph.
    pub seeds: Vec<String>,
    pub symbols: Vec<ClusterSymbol>,
    pub files: Vec<String>,
}

/// A snippet of code with associated metadata.
#[derive(Debug, Clone)]
pub struct Snippet {
    pub symbol: String,
    pub file: String,
    pub line: u32,
    pub code: String,
    pub doc: String,
}

/// A section of documentation extracted from sources.
#[derive(Debug, Clone)]
pub struct DocSection {
    pub source: String,
    pub heading: String,
    pub body: String,
}

/// A learned document combining code snippets, flow, and documentation.
#[derive(Debug, Clone)]
pub struct LearnDoc {
    pub topic: String,
    pub snippets: Vec<Snippet>,
    /// Caller → callee pairs for the call-flow diagram.
    pub flow: Vec<(String, String)>,
    /// Seed symbol names the flow should stay anchored on (topic hits).
    pub flow_seeds: Vec<String>,
    pub doc_sections: Vec<DocSection>,
}

/// A narration of architecture and design decisions.
#[derive(Debug, Clone)]
pub struct Narration {
    pub overview: String,
    // Reserved for richer rendering in future tasks.
    #[allow(dead_code)]
    pub components: Vec<(String, String)>,
    pub decisions: String,
    pub concepts: String,
}

/// Stable filesystem / URL slug for a free-form learn topic.
///
/// Raw queries like `"accounts module"` or `"Lead::qualify journey"` must not
/// become messy filenames (`accounts module.html`). This lowers, maps every
/// non-alphanumeric run to a single `-`, trims edges, and falls back to
/// `"topic"` when nothing usable remains. Display titles keep the original
/// wording; only the artifact path uses the slug.
pub fn topic_slug(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_dash = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            // Collapse spaces, underscores, `::`, path separators, etc. to one `-`.
            out.push('-');
            prev_dash = true;
        }
    }
    let slug = out.trim_matches('-').to_string();
    if slug.is_empty() {
        "topic".into()
    } else {
        // Keep paths bounded; long free-text queries shouldn't create huge names.
        const MAX: usize = 80;
        if slug.len() <= MAX {
            slug
        } else {
            let mut cut = slug.chars().take(MAX).collect::<String>();
            // Avoid ending mid-token with a trailing dash after truncation.
            while cut.ends_with('-') {
                cut.pop();
            }
            if cut.is_empty() {
                "topic".into()
            } else {
                cut
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_slug_standardises_free_form_queries() {
        assert_eq!(topic_slug("accounts module"), "accounts-module");
        assert_eq!(topic_slug("Accounts Module"), "accounts-module");
        assert_eq!(topic_slug("accounts_module"), "accounts-module");
        assert_eq!(topic_slug("Lead::qualify journey"), "lead-qualify-journey");
        assert_eq!(topic_slug("  harness  "), "harness");
        assert_eq!(topic_slug("..."), "topic");
        assert_eq!(topic_slug(""), "topic");
        // Path-like noise must not escape learn dir via the name.
        assert_eq!(topic_slug("../evil"), "evil");
        assert_eq!(topic_slug("a/b\\c"), "a-b-c");
    }

    #[test]
    fn topic_slug_truncates_very_long_queries() {
        let long = "word ".repeat(40);
        let slug = topic_slug(&long);
        assert!(slug.len() <= 80, "slug too long: {slug}");
        assert!(!slug.ends_with('-'));
        assert!(slug.starts_with("word"));
    }

    #[test]
    fn cluster_and_learndoc_construct() {
        let c = Cluster {
            topic: "harness".into(),
            seeds: vec![],
            symbols: vec![],
            files: vec![],
        };
        assert_eq!(c.topic, "harness");

        let d = LearnDoc {
            topic: "harness".into(),
            snippets: vec![],
            flow: vec![],
            flow_seeds: vec![],
            doc_sections: vec![],
        };
        assert!(d.snippets.is_empty());
    }
}
