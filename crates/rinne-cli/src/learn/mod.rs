//! `learn` module for knowledge synthesis and code narration.

pub mod resolve;
pub mod source;
pub mod translate;
pub mod render;

/// A symbol within a cluster: represents a function, type, or other named entity.
/// Consumed by Tasks 5-9.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ClusterSymbol {
    pub name: String,
    pub file: String,
    pub line: u32,
    pub kind: String,
}

/// A cluster of related symbols around a topic.
/// Consumed by Tasks 5-9.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Cluster {
    pub topic: String,
    pub symbols: Vec<ClusterSymbol>,
    pub files: Vec<String>,
}

/// A snippet of code with associated metadata.
/// Consumed by Tasks 5-9.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Snippet {
    pub symbol: String,
    pub file: String,
    pub line: u32,
    pub code: String,
    pub doc: String,
}

/// A section of documentation extracted from sources.
/// Consumed by Tasks 5-9.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DocSection {
    pub source: String,
    pub heading: String,
    pub body: String,
}

/// A learned document combining code snippets, flow, and documentation.
/// Consumed by Tasks 5-9.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct LearnDoc {
    pub topic: String,
    pub snippets: Vec<Snippet>,
    pub flow: Vec<(String, String)>,  // (caller, callee) name pairs
    pub doc_sections: Vec<DocSection>,
}

/// A narration of architecture and design decisions.
/// Consumed by Tasks 5-9.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Narration {
    pub overview: String,
    pub components: Vec<(String, String)>,
    pub decisions: String,
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
