//! Rust language support.

use super::LanguageSupport;
use tree_sitter::{Language, Query};

pub struct Rust {
    query: Query,
}

impl Rust {
    pub fn new() -> Self {
        let language: Language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, include_str!("../queries/rust.scm"))
            .expect("valid rust query");
        Self { query }
    }
}

impl Default for Rust {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageSupport for Rust {
    fn name(&self) -> &'static str {
        "rust"
    }
    fn language(&self) -> Language {
        tree_sitter_rust::LANGUAGE.into()
    }
    fn query(&self) -> &Query {
        &self.query
    }
}
