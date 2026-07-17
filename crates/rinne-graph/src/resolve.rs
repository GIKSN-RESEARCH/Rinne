//! Intra-file edge resolution: turns raw symbols/edges into resolved Symbol/Edge rows.
//!
//! Edges whose `dst_name` matches a symbol in this file get `dst = Some(id)`.
//! Edges to names not found in this file get `dst = None` (unresolved, kept for
//! cross-file resolution later) — they are never dropped or guessed.

use crate::extract::{RawEdge, RawSymbol};
use crate::model::{Edge, Symbol};

/// Resolve raw symbols and edges within a single file.
///
/// `symbol_id` returns the assigned row id for the i-th raw symbol (in the same
/// order as `raw_symbols`). Each symbol gets a distinct id — same-name symbols in
/// one file are NOT collapsed (that was the Stage 2 bug). The store supplies real
/// per-row SQLite ids; tests supply a fake closure.
///
/// Edge resolution is container-aware: a call resolves to the definition of the
/// same name in the *same container* (scope) as the call site when one exists,
/// else falls back to a unique same-name definition, else stays unresolved
/// (`None`). This routes calls to the right definition when a name is defined more
/// than once in the file.
pub fn resolve_file(
    file: &str,
    raw_symbols: Vec<RawSymbol>,
    raw_edges: Vec<RawEdge>,
    symbol_id: impl Fn(usize) -> i64,
) -> (Vec<Symbol>, Vec<Edge>) {
    // Assign each raw symbol its own id, preserving order. No name-keyed map, so
    // same-name defs stay distinct.
    let symbols: Vec<Symbol> = raw_symbols
        .iter()
        .enumerate()
        .map(|(i, s)| Symbol {
            id: symbol_id(i),
            file: file.to_string(),
            name: s.name.clone(),
            kind: s.kind,
            start_line: s.start_line,
            end_line: s.end_line,
            signature: s.signature.clone(),
            container: s.container.clone(),
        })
        .collect();

    // Resolve a name (optionally scoped to a container) to a single symbol id.
    // Preference: exact (name, container) match → unique name match → None.
    let resolve = |name: &str, container: Option<&str>| -> Option<i64> {
        // 1. Same-container definition (the call's own scope).
        if let Some(c) = container {
            if let Some(sym) = symbols
                .iter()
                .find(|s| s.name == name && s.container.as_deref() == Some(c))
            {
                return Some(sym.id);
            }
        }
        // 2. A definition at file scope (no container) with this name.
        if let Some(sym) = symbols
            .iter()
            .find(|s| s.name == name && s.container.is_none())
        {
            return Some(sym.id);
        }
        // 3. A unique same-name definition anywhere in the file.
        let mut matches = symbols.iter().filter(|s| s.name == name);
        match (matches.next(), matches.next()) {
            (Some(sym), None) => Some(sym.id),
            _ => None, // ambiguous or absent → unresolved, never guessed
        }
    };

    let edges: Vec<Edge> = raw_edges
        .into_iter()
        .map(|e| {
            let src = e
                .src_name
                .as_deref()
                .and_then(|n| resolve(n, e.src_container.as_deref()));
            let dst = resolve(&e.dst_name, e.src_container.as_deref());
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
            RawSymbol { name: "a".into(), kind: SymbolKind::Function, start_line: 1, end_line: 1, signature: None, container: None },
            RawSymbol { name: "b".into(), kind: SymbolKind::Function, start_line: 2, end_line: 2, signature: None, container: None },
        ];
        let raw_edges = vec![
            RawEdge { src_name: Some("a".into()), dst_name: "b".into(), kind: EdgeKind::Calls, src_container: None },
            RawEdge { src_name: Some("a".into()), dst_name: "external".into(), kind: EdgeKind::Calls, src_container: None },
        ];
        // Fake id assignment by position: symbol 0 (a) = 10, symbol 1 (b) = 11.
        let (symbols, edges) = resolve_file("f.rs", raw_symbols, raw_edges, |i| [10, 11][i]);
        assert_eq!(symbols.len(), 2);
        let to_b = edges.iter().find(|e| e.dst_name == "b").unwrap();
        assert_eq!(to_b.dst, Some(11));
        assert_eq!(to_b.src, Some(10));
        let to_ext = edges.iter().find(|e| e.dst_name == "external").unwrap();
        assert_eq!(to_ext.dst, None); // unresolved, not dropped
    }
}
