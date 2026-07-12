//! TypeScript and JavaScript language support.

use super::LanguageSupport;
use tree_sitter::{Language, Query};

pub struct TypeScript {
    query: Query,
}

impl TypeScript {
    pub fn new() -> Self {
        let language: Language = tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into();
        let query = Query::new(&language, include_str!("../queries/typescript.scm"))
            .expect("valid typescript query");
        Self { query }
    }
}

impl Default for TypeScript {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageSupport for TypeScript {
    fn name(&self) -> &'static str {
        "typescript"
    }
    fn language(&self) -> Language {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    }
    fn query(&self) -> &Query {
        &self.query
    }
}

pub struct JavaScript {
    query: Query,
}

impl JavaScript {
    pub fn new() -> Self {
        let language: Language = tree_sitter_javascript::LANGUAGE.into();
        let query = Query::new(&language, include_str!("../queries/javascript.scm"))
            .expect("valid javascript query");
        Self { query }
    }
}

impl Default for JavaScript {
    fn default() -> Self {
        Self::new()
    }
}

impl LanguageSupport for JavaScript {
    fn name(&self) -> &'static str {
        "javascript"
    }
    fn language(&self) -> Language {
        tree_sitter_javascript::LANGUAGE.into()
    }
    fn query(&self) -> &Query {
        &self.query
    }
}
