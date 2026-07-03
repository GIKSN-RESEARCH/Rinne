//! Per-language parsing support: each language pairs its tree-sitter grammar
//! with an extraction query. Adding a language is one impl + one query file.

pub mod python;
pub mod rust;
pub mod typescript;

pub trait LanguageSupport: Send + Sync {
    fn name(&self) -> &'static str;
    fn language(&self) -> tree_sitter::Language;
    fn query(&self) -> &tree_sitter::Query;
}

pub fn support_for(lang: &str) -> Option<Box<dyn LanguageSupport>> {
    match lang {
        "rust" => Some(Box::new(rust::Rust::new())),
        "typescript" => Some(Box::new(typescript::TypeScript::new())),
        "javascript" => Some(Box::new(typescript::JavaScript::new())),
        "python" => Some(Box::new(python::Python::new())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_supported_languages() {
        for l in ["rust", "typescript", "javascript", "python"] {
            assert!(support_for(l).is_some(), "missing support for {l}");
        }
        assert!(support_for("cobol").is_none());
    }
}
