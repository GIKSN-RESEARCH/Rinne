//! Tree-sitter parse + query execution → raw symbols and name-only edges.

use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, QueryCursor};

use crate::lang::support_for;
use crate::model::{EdgeKind, SymbolKind};

pub struct RawSymbol {
    pub name: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: Option<String>,
}

pub struct RawEdge {
    pub src_name: Option<String>,
    pub dst_name: String,
    pub kind: EdgeKind,
}

pub fn extract(lang: &str, source: &str) -> Option<(Vec<RawSymbol>, Vec<RawEdge>)> {
    let support = support_for(lang)?;
    let mut parser = Parser::new();
    parser.set_language(&support.language()).ok()?;
    let tree = parser.parse(source, None)?;
    let query = support.query();
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    walk(query, &tree, source.as_bytes(), &mut symbols, &mut edges);
    Some((symbols, edges))
}

fn kind_from_suffix(suffix: &str) -> Option<SymbolKind> {
    match suffix {
        "function" => Some(SymbolKind::Function),
        "method" => Some(SymbolKind::Method),
        "class" => Some(SymbolKind::Class),
        "struct" => Some(SymbolKind::Struct),
        "enum" => Some(SymbolKind::Enum),
        "trait" => Some(SymbolKind::Trait),
        "interface" => Some(SymbolKind::Interface),
        "module" => Some(SymbolKind::Module),
        _ => None,
    }
}

fn enclosing_symbol_name<'a>(
    mut node: tree_sitter::Node<'a>,
    query: &tree_sitter::Query,
    source: &[u8],
) -> Option<String> {
    // Walk up to the nearest node that a def.function or def.method capture matches.
    // We detect this by checking if any capture on the query named def.function/def.method
    // targets this node type. Instead of re-running the query, we walk parents and check
    // node kinds that represent function/method definitions.
    let def_kinds = [
        "function_item",        // rust
        "function_definition",  // python
        "function_declaration", // ts/js
        "method_definition",    // ts/js
        "arrow_function",       // ts/js (anonymous, no name — skip)
    ];

    // Get the capture names from the query so we can check if def.function / def.method exist
    let cap_names: Vec<&str> = query.capture_names().iter().map(|s| s.as_ref()).collect();
    let has_def_function = cap_names.contains(&"def.function");
    let has_def_method = cap_names.contains(&"def.method");

    if !has_def_function && !has_def_method {
        return None;
    }

    while let Some(parent) = node.parent() {
        let kind = parent.kind();
        if def_kinds.contains(&kind) {
            // Try to get child named "name"
            if let Some(name_node) = parent.child_by_field_name("name") {
                let name = name_node.utf8_text(source).ok()?.to_string();
                return Some(name);
            }
        }
        node = parent;
    }
    None
}

fn walk(
    query: &tree_sitter::Query,
    tree: &tree_sitter::Tree,
    source: &[u8],
    symbols: &mut Vec<RawSymbol>,
    edges: &mut Vec<RawEdge>,
) {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source);

    while let Some(m) = matches.next() {
        for cap in m.captures {
            let cap_name = &query.capture_names()[cap.index as usize];
            let node = cap.node;

            if let Some(suffix) = cap_name.strip_prefix("def.") {
                let Some(kind) = kind_from_suffix(suffix) else {
                    continue;
                };
                // The capture is on the name node; get the parent for line span.
                let def_node = node.parent().unwrap_or(node);
                let name = node.utf8_text(source).unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                // Signature: first line of the definition node
                let start_byte = def_node.start_byte();
                let end_byte = def_node.end_byte();
                let def_text = std::str::from_utf8(&source[start_byte..end_byte]).unwrap_or("");
                let signature = def_text.lines().next().map(|l| l.trim().to_string());

                symbols.push(RawSymbol {
                    name,
                    kind,
                    start_line: def_node.start_position().row as u32 + 1,
                    end_line: def_node.end_position().row as u32 + 1,
                    signature,
                });
            } else if cap_name.starts_with("call.") {
                let dst_name = node.utf8_text(source).unwrap_or("").to_string();
                if dst_name.is_empty() {
                    continue;
                }
                let src_name = enclosing_symbol_name(node, query, source);
                edges.push(RawEdge {
                    src_name,
                    dst_name,
                    kind: EdgeKind::Calls,
                });
            } else if cap_name.starts_with("import.") {
                let dst_name = node.utf8_text(source).unwrap_or("").to_string();
                if dst_name.is_empty() {
                    continue;
                }
                edges.push(RawEdge {
                    src_name: None,
                    dst_name,
                    kind: EdgeKind::Imports,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_functions_and_calls() {
        let src = "fn helper() {}\nfn main() { helper(); }\n";
        let (symbols, edges) = extract("rust", src).expect("rust supported");
        let names: Vec<_> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"helper"));
        assert!(names.contains(&"main"));
        assert!(edges.iter().any(|e| e.dst_name == "helper" && e.src_name.as_deref() == Some("main")));
    }

    #[test]
    fn extracts_python_defs() {
        let src = "def a():\n    b()\ndef b():\n    pass\n";
        let (symbols, edges) = extract("python", src).unwrap();
        assert!(symbols.iter().any(|s| s.name == "a"));
        assert!(edges.iter().any(|e| e.dst_name == "b"));
    }

    #[test]
    fn extracts_typescript_functions() {
        let src = "function f() { g(); }\nfunction g() {}\n";
        let (symbols, edges) = extract("typescript", src).unwrap();
        assert!(symbols.iter().any(|s| s.name == "f"));
        assert!(edges.iter().any(|e| e.dst_name == "g"));
    }
}
