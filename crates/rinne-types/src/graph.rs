//! The `CodeGraph` trait seam and shared graph types.
//!
//! `SymbolRef` and `Neighborhood` live here so that `rinne-loop`/`rinne-conductor`
//! can depend on the trait without depending on `rinne-graph`. The concrete
//! implementation lives in `rinne-graph` behind a `Mutex<Store>`.

use serde::{Deserialize, Serialize};

/// A lightweight reference to a symbol: name, file path, and 1-based line span.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRef {
    pub name: String,
    pub file: String,
    /// 1-based first line of the definition.
    pub line: u32,
    /// 1-based last line of the definition span. Equals `line` for one-liners
    /// and for rows indexed before this field existed (COALESCE fallback).
    pub end_line: u32,
}

/// The structural neighborhood of a symbol: its definition site plus all
/// callers, callees, and imports known to the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Neighborhood {
    pub definition: SymbolRef,
    pub callers: Vec<SymbolRef>,
    pub callees: Vec<SymbolRef>,
    pub imports: Vec<SymbolRef>,
    /// `true` when the underlying file has been modified since the last index run.
    pub stale: bool,
}

/// Read-only view over the code graph. Implementations must be `Send + Sync`
/// so they can be shared across async task boundaries.
pub trait CodeGraph: Send + Sync {
    /// Returns the neighborhood of the first symbol matching `symbol`, or `None`
    /// if the symbol is not in the index.
    fn neighborhood(&self, symbol: &str) -> Option<Neighborhood>;

    /// Returns the first symbol named `name` defined in `file`, or `None`.
    fn resolve_in_file(&self, file: &str, name: &str) -> Option<SymbolRef>;

    /// Returns the names of every symbol currently in the index.
    fn symbol_names(&self) -> Vec<String>;
}
