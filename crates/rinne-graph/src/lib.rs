//! The Rinne code graph: a local, incremental, tree-sitter structural index of
//! the repo (files, symbols, imports, call edges) persisted in the blackboard's
//! SQLite store. Retrieval returns a symbol's neighborhood — definition,
//! callers, callees, imports — so workers fetch the relevant slice of a repo
//! instead of re-reading whole files (issue #11).

pub mod extract;
pub mod lang;
pub mod model;
pub mod schema;
