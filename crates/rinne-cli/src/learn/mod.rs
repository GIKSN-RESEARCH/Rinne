//! `learn` module for knowledge synthesis and code narration.

pub mod resolve;
pub mod source;
pub mod translate;
pub mod render;

/// A symbol within a cluster: represents a function, type, or other named entity.
#[derive(Debug, Clone)]
pub struct ClusterSymbol {
    pub name: String,
    pub file: String,
    pub line: u32,
    // Reserved for future filtering/display; not yet consumed by the renderer.
    #[allow(dead_code)]
    pub kind: String,
}

/// A cluster of related symbols around a topic.
#[derive(Debug, Clone)]
pub struct Cluster {
    // Carried for serialisation and future use; the renderer uses symbols/files.
    #[allow(dead_code)]
    pub topic: String,
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
    pub flow: Vec<(String, String)>,  // (caller, callee) name pairs
    pub doc_sections: Vec<DocSection>,
}

/// A narration of architecture and design decisions.
#[derive(Debug, Clone)]
pub struct Narration {
    pub overview: String,
    // Reserved for richer rendering in future tasks.
    #[allow(dead_code)]
    pub components: Vec<(String, String)>,
    #[allow(dead_code)]
    pub decisions: String,
    #[allow(dead_code)]
    pub concepts: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_and_learndoc_construct() {
        let c = Cluster {
            topic: "harness".into(),
            symbols: vec![],
            files: vec![],
        };
        assert_eq!(c.topic, "harness");

        let d = LearnDoc {
            topic: "harness".into(),
            snippets: vec![],
            flow: vec![],
            doc_sections: vec![],
        };
        assert!(d.snippets.is_empty());
    }
}
