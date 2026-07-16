//! The Rinne code graph: a local, incremental, tree-sitter structural index of
//! the repo (files, symbols, imports, call edges) persisted in the blackboard's
//! SQLite store. Retrieval returns a symbol's neighborhood — definition,
//! callers, callees, imports — so workers fetch the relevant slice of a repo
//! instead of re-reading whole files (issue #11).

pub mod branches;
pub mod extract;
pub mod indexer;
pub mod lang;
pub mod model;
pub mod resolve;
pub mod schema;
pub mod store;

use std::path::Path;
use std::sync::Mutex;

use rinne_types::graph::{CodeGraph, Neighborhood, SymbolRef};

use crate::store::Store;

/// Thread-safe handle to the code graph store.
///
/// Wraps a `Store` behind a `Mutex` so `Graph` is `Send + Sync` and can be
/// shared across async task boundaries through the `CodeGraph` trait.
pub struct Graph {
    store: Mutex<Store>,
}

impl Graph {
    /// Open (or create) the graph store at `db_path`, resolving repo-relative
    /// file keys against `root` when computing the `stale` flag.
    pub fn open(db_path: &Path, root: &Path) -> rusqlite::Result<Graph> {
        let store = Store::open(db_path, root)?;
        Ok(Graph {
            store: Mutex::new(store),
        })
    }

    /// Re-index `path` if the stored hash does not match the current `source`.
    pub fn ensure_current(&self, path: &str, source: &str, mtime: i64) -> rusqlite::Result<()> {
        let store = self.store.lock().expect("graph store lock poisoned");
        if !store.is_current(path, source) {
            store.index_file(path, source, mtime)?;
        }
        Ok(())
    }

    /// Returns (files, symbols, edges) counts for display in `rinne graph stats`.
    pub fn stats(&self) -> (usize, usize, usize) {
        self.store
            .lock()
            .expect("graph store lock poisoned")
            .stats()
    }

    /// Returns all symbols defined in a given file (for `rinne graph symbols <file>`).
    pub fn symbols_in(&self, path: &str) -> Vec<crate::model::Symbol> {
        self.store
            .lock()
            .expect("graph store lock poisoned")
            .symbols_in(path)
    }
}

impl CodeGraph for Graph {
    fn neighborhood(&self, symbol: &str) -> Option<Neighborhood> {
        self.store
            .lock()
            .expect("graph store lock poisoned")
            .neighborhood(symbol)
    }

    fn resolve_in_file(&self, file: &str, name: &str) -> Option<SymbolRef> {
        self.store
            .lock()
            .expect("graph store lock poisoned")
            .resolve_in_file(file, name)
    }

    fn symbol_names(&self) -> Vec<String> {
        self.store
            .lock()
            .expect("graph store lock poisoned")
            .symbol_names()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rinne_types::graph::CodeGraph;

    #[test]
    fn graph_exposes_codegraph_trait() {
        let dir = std::env::temp_dir().join(format!("rinne-graph-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");
        let graph = Graph::open(&db, &dir).unwrap();
        graph.ensure_current("m.rs", "fn helper() {}\nfn main() { helper(); }\n", 0).unwrap();
        let nb = CodeGraph::neighborhood(&graph, "helper").unwrap();
        assert_eq!(nb.definition.name, "helper");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn neighborhood_reports_symbol_end_line() {
        let dir = std::env::temp_dir().join(format!("rinne-graph-endline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.db");
        let graph = Graph::open(&db, &dir).unwrap();
        // `big` spans lines 1..=3; `helper` is line 4.
        graph
            .ensure_current("m.rs", "fn big() {\n    let x = 1;\n}\nfn helper() {}\n", 0)
            .unwrap();
        let nb = CodeGraph::neighborhood(&graph, "big").unwrap();
        assert_eq!(nb.definition.line, 1, "start line");
        assert_eq!(nb.definition.end_line, 3, "end line covers the whole body");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
