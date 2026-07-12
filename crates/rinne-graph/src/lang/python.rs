//! Python language support.

use super::LanguageSupport;
use tree_sitter::{Language, Query};

pub struct Python {
    query: Query,
}

impl Python {
    pub fn new() -> Self {
        let language: Language = tree_sitter_python::LANGUAGE.into();
        let query = Query::new(&language, include_str!("../queries/python.scm"))
            .expect("valid python query");
        Self { query }
    }
}

impl Default for Python {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageSupport for Python {
    fn name(&self) -> &'static str {
        "python"
    }
    fn language(&self) -> Language {
        tree_sitter_python::LANGUAGE.into()
    }
    fn query(&self) -> &Query {
        &self.query
    }
}
