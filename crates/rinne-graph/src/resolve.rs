//! Intra-file edge resolution: turns raw symbols/edges into resolved Symbol/Edge rows.
//!
//! Edges whose `dst_name` matches a symbol in this file get `dst = Some(id)`.
//! Edges to names not found in this file get `dst = None` (unresolved, kept for
//! cross-file resolution later) — they are never dropped or guessed.

use std::collections::HashMap;

use crate::extract::{RawEdge, RawSymbol};
use crate::model::{Edge, Symbol};

/// Resolve raw symbols and edges within a single file.
///
/// `symbol_id` maps a symbol name (within this file) to its assigned row id.
/// Task 8 supplies real ids from SQLite; tests supply a fake closure.
pub fn resolve_file(
    file: &str,
    raw_symbols: Vec<RawSymbol>,
    raw_edges: Vec<RawEdge>,
    symbol_id: impl Fn(&str) -> i64,
) -> (Vec<Symbol>, Vec<Edge>) {
    // Build name → id map for all symbols in this file.
    let name_to_id: HashMap<String, i64> = raw_symbols
        .iter()
        .map(|s| (s.name.clone(), symbol_id(&s.name)))
        .collect();

    let symbols: Vec<Symbol> = raw_symbols
        .into_iter()
        .map(|s| {
            let id = *name_to_id.get(&s.name).expect("name must be in map");
            Symbol {
                id,
                file: file.to_string(),
                name: s.name,
                kind: s.kind,
                start_line: s.start_line,
                end_line: s.end_line,
                signature: s.signature,
            }
        })
        .collect();

    let edges: Vec<Edge> = raw_edges
        .into_iter()
        .map(|e| {
            let src = e.src_name.as_deref().and_then(|n| name_to_id.get(n).copied());
            let dst = name_to_id.get(&e.dst_name).copied();
            Edge {
                src,
                dst,
                dst_name: e.dst_name,
                kind: e.kind,
            }
        })
        .collect();

    (symbols, edges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::{RawEdge, RawSymbol};
    use crate::model::{EdgeKind, SymbolKind};

    #[test]
    fn resolves_intra_file_calls_and_keeps_unresolved() {
        let raw_symbols = vec![
            RawSymbol { name: "a".into(), kind: SymbolKind::Function, start_line: 1, end_line: 1, signature: None },
            RawSymbol { name: "b".into(), kind: SymbolKind::Function, start_line: 2, end_line: 2, signature: None },
        ];
        let raw_edges = vec![
            RawEdge { src_name: Some("a".into()), dst_name: "b".into(), kind: EdgeKind::Calls },
            RawEdge { src_name: Some("a".into()), dst_name: "external".into(), kind: EdgeKind::Calls },
        ];
        // Fake id assignment: a=10, b=11.
        let (symbols, edges) = resolve_file("f.rs", raw_symbols, raw_edges, |n| match n {
            "a" => 10, "b" => 11, _ => 0,
        });
        assert_eq!(symbols.len(), 2);
        let to_b = edges.iter().find(|e| e.dst_name == "b").unwrap();
        assert_eq!(to_b.dst, Some(11));
        assert_eq!(to_b.src, Some(10));
        let to_ext = edges.iter().find(|e| e.dst_name == "external").unwrap();
        assert_eq!(to_ext.dst, None); // unresolved, not dropped
    }
}
