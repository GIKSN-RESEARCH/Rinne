//! Plain data types for the code graph: symbols, edges, files, neighborhoods.

use serde::{Deserialize, Serialize};

// SymbolRef and Neighborhood now live in rinne-types so downstream crates can
// use the CodeGraph trait without depending on rinne-graph.
pub use rinne_types::graph::{Neighborhood, SymbolRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Trait,
    Interface,
    Module,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Method => "method",
            SymbolKind::Class => "class",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Trait => "trait",
            SymbolKind::Interface => "interface",
            SymbolKind::Module => "module",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "function" => SymbolKind::Function,
            "method" => SymbolKind::Method,
            "class" => SymbolKind::Class,
            "struct" => SymbolKind::Struct,
            "enum" => SymbolKind::Enum,
            "trait" => SymbolKind::Trait,
            "interface" => SymbolKind::Interface,
            "module" => SymbolKind::Module,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeKind {
    Calls,
    Imports,
    Defines,
    References,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Calls => "calls",
            EdgeKind::Imports => "imports",
            EdgeKind::Defines => "defines",
            EdgeKind::References => "references",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "calls" => EdgeKind::Calls,
            "imports" => EdgeKind::Imports,
            "defines" => EdgeKind::Defines,
            "references" => EdgeKind::References,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub id: i64,
    pub file: String,
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: Option<String>,
    /// Enclosing type/class/trait for a method; `None` for free functions.
    pub container: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub src: Option<i64>,
    pub dst: Option<i64>,
    pub dst_name: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRecord {
    pub path: String,
    pub lang: String,
    pub content_hash: String,
    pub mtime: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_kind_roundtrips_through_str() {
        for k in [
            SymbolKind::Function,
            SymbolKind::Method,
            SymbolKind::Class,
            SymbolKind::Struct,
            SymbolKind::Enum,
            SymbolKind::Trait,
            SymbolKind::Interface,
            SymbolKind::Module,
        ] {
            assert_eq!(SymbolKind::from_str(k.as_str()), Some(k));
        }
    }

    #[test]
    fn edge_kind_roundtrips_through_str() {
        for k in [
            EdgeKind::Calls,
            EdgeKind::Imports,
            EdgeKind::Defines,
            EdgeKind::References,
        ] {
            assert_eq!(EdgeKind::from_str(k.as_str()), Some(k));
        }
    }
}
