//! SQLite-backed store: persist files, freshness checks, neighborhood queries.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;

use rusqlite::Connection;

use crate::extract::extract;
use crate::model::{Neighborhood, Symbol, SymbolKind, SymbolRef};
use crate::resolve::resolve_file;
use crate::schema::init_graph_schema;
use rinne_types::skip::source_lang;

pub struct Store {
    pub(crate) conn: Connection,
}

impl Store {
    pub fn open(db_path: &Path) -> rusqlite::Result<Store> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;",
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        init_graph_schema(&conn)?;
        Ok(Store { conn })
    }

    /// Stable content hash for freshness comparison.
    /// Uses DefaultHasher — deterministic within a build, sufficient for this use.
    pub fn hash_of(source: &str) -> String {
        let mut h = DefaultHasher::new();
        source.hash(&mut h);
        format!("{:016x}", h.finish())
    }

    /// Returns true if the stored hash for `path` matches `hash_of(source)`.
    pub fn is_current(&self, path: &str, source: &str) -> bool {
        let hash = Self::hash_of(source);
        self.conn
            .query_row(
                "SELECT content_hash FROM graph_files WHERE path = ?1",
                [path],
                |row| row.get::<_, String>(0),
            )
            .map(|stored| stored == hash)
            .unwrap_or(false)
    }

    /// Transactionally re-index a file: delete prior rows, extract+resolve, insert new rows.
    /// No-ops for files with unsupported languages (upserts only the file row with lang='').
    pub fn index_file(&self, path: &str, source: &str, mtime: i64) -> rusqlite::Result<()> {
        let hash = Self::hash_of(source);
        let lang = source_lang(path);

        let tx = self.conn.unchecked_transaction()?;

        // Delete prior edges for this file's symbols first (FK-style manual cascade).
        tx.execute(
            "DELETE FROM graph_edges WHERE src_symbol IN (SELECT id FROM graph_symbols WHERE file = ?1) \
             OR dst_symbol IN (SELECT id FROM graph_symbols WHERE file = ?1)",
            rusqlite::params![path],
        )?;
        tx.execute(
            "DELETE FROM graph_symbols WHERE file = ?1",
            rusqlite::params![path],
        )?;

        if let Some(lang_str) = lang {
            if let Some((raw_symbols, raw_edges)) = extract(lang_str, source) {
                // Insert each symbol, capture its rowid immediately.
                let mut name_to_id: std::collections::HashMap<String, i64> =
                    std::collections::HashMap::new();

                for rs in &raw_symbols {
                    tx.execute(
                        "INSERT INTO graph_symbols (file, name, kind, start_line, end_line, signature) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![
                            path,
                            rs.name,
                            rs.kind.as_str(),
                            rs.start_line,
                            rs.end_line,
                            rs.signature,
                        ],
                    )?;
                    let id = tx.last_insert_rowid();
                    // Last-write-wins for duplicate names within one file (acceptable for v1).
                    name_to_id.insert(rs.name.clone(), id);
                }

                let (_, edges) =
                    resolve_file(path, raw_symbols, raw_edges, |name| {
                        name_to_id.get(name).copied().unwrap_or(0)
                    });

                for edge in &edges {
                    tx.execute(
                        "INSERT INTO graph_edges (src_symbol, dst_symbol, dst_name, kind) \
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![
                            edge.src,
                            edge.dst,
                            edge.dst_name,
                            edge.kind.as_str(),
                        ],
                    )?;
                }
            }
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        tx.execute(
            "INSERT INTO graph_files (path, lang, content_hash, mtime, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
               lang         = excluded.lang,
               content_hash = excluded.content_hash,
               mtime        = excluded.mtime,
               indexed_at   = excluded.indexed_at",
            rusqlite::params![path, lang.unwrap_or(""), hash, mtime, now],
        )?;

        tx.commit()
    }

    /// Returns the definition + callers + callees + imports for a symbol by name.
    pub fn neighborhood(&self, symbol_name: &str) -> Option<Neighborhood> {
        // Look up the definition symbol.
        let (sym_id, file, start_line): (i64, String, u32) = self
            .conn
            .query_row(
                "SELECT id, file, start_line FROM graph_symbols WHERE name = ?1 LIMIT 1",
                [symbol_name],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .ok()?;

        let definition = SymbolRef {
            name: symbol_name.to_string(),
            file: file.clone(),
            line: start_line,
        };

        // Callers: symbols whose id is src_symbol of an edge with dst_symbol = sym_id.
        let callers = self
            .conn
            .prepare(
                "SELECT s.name, s.file, s.start_line
                 FROM graph_edges e
                 JOIN graph_symbols s ON s.id = e.src_symbol
                 WHERE e.dst_symbol = ?1 AND e.kind = 'calls'",
            )
            .and_then(|mut stmt| {
                stmt.query_map([sym_id], |row| {
                    Ok(SymbolRef {
                        name: row.get(0)?,
                        file: row.get(1)?,
                        line: row.get(2)?,
                    })
                })
                .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            })
            .unwrap_or_default();

        // Callees: symbols whose id is dst_symbol of an edge with src_symbol = sym_id.
        let callees = self
            .conn
            .prepare(
                "SELECT s.name, s.file, s.start_line
                 FROM graph_edges e
                 JOIN graph_symbols s ON s.id = e.dst_symbol
                 WHERE e.src_symbol = ?1 AND e.kind = 'calls'",
            )
            .and_then(|mut stmt| {
                stmt.query_map([sym_id], |row| {
                    Ok(SymbolRef {
                        name: row.get(0)?,
                        file: row.get(1)?,
                        line: row.get(2)?,
                    })
                })
                .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            })
            .unwrap_or_default();

        // Imports: edges of kind 'imports' where src_symbol = sym_id; dst may be unresolved.
        // When dst_symbol is set, join on symbols; otherwise use dst_name best-effort.
        let imports = self
            .conn
            .prepare(
                "SELECT COALESCE(s.name, e.dst_name), COALESCE(s.file, ''), COALESCE(s.start_line, 0)
                 FROM graph_edges e
                 LEFT JOIN graph_symbols s ON s.id = e.dst_symbol
                 WHERE e.src_symbol = ?1 AND e.kind = 'imports'",
            )
            .and_then(|mut stmt| {
                stmt.query_map([sym_id], |row| {
                    Ok(SymbolRef {
                        name: row.get(0)?,
                        file: row.get(1)?,
                        line: row.get(2)?,
                    })
                })
                .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            })
            .unwrap_or_default();

        Some(Neighborhood {
            definition,
            callers,
            callees,
            imports,
            stale: false,
        })
    }

    /// Returns the names of every symbol currently in the index.
    pub fn symbol_names(&self) -> Vec<String> {
        self.conn
            .prepare("SELECT DISTINCT name FROM graph_symbols")
            .and_then(|mut stmt| {
                stmt.query_map([], |row| row.get(0))
                    .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            })
            .unwrap_or_default()
    }

    /// Returns the first symbol named `name` in `file`.
    pub fn resolve_in_file(&self, file: &str, name: &str) -> Option<SymbolRef> {
        self.conn
            .query_row(
                "SELECT name, file, start_line FROM graph_symbols WHERE file = ?1 AND name = ?2 LIMIT 1",
                rusqlite::params![file, name],
                |row| {
                    Ok(SymbolRef {
                        name: row.get(0)?,
                        file: row.get(1)?,
                        line: row.get(2)?,
                    })
                },
            )
            .ok()
    }

    /// Returns (files, symbols, edges) counts for `rinne graph stats`.
    pub fn stats(&self) -> (usize, usize, usize) {
        let count = |sql: &str| -> usize {
            self.conn
                .query_row(sql, [], |row| row.get::<_, i64>(0))
                .unwrap_or(0) as usize
        };
        (
            count("SELECT COUNT(*) FROM graph_files"),
            count("SELECT COUNT(*) FROM graph_symbols"),
            count("SELECT COUNT(*) FROM graph_edges"),
        )
    }

    /// Returns all symbols defined in a given file.
    pub fn symbols_in(&self, path: &str) -> Vec<Symbol> {
        self.conn
            .prepare(
                "SELECT id, file, name, kind, start_line, end_line, signature
                 FROM graph_symbols WHERE file = ?1",
            )
            .and_then(|mut stmt| {
                stmt.query_map([path], |row| {
                    let kind_str: String = row.get(3)?;
                    let kind =
                        SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);
                    Ok(Symbol {
                        id: row.get(0)?,
                        file: row.get(1)?,
                        name: row.get(2)?,
                        kind,
                        start_line: row.get(4)?,
                        end_line: row.get(5)?,
                        signature: row.get(6)?,
                    })
                })
                .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_store() -> Store {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::init_graph_schema(&conn).unwrap();
        Store { conn }
    }

    #[test]
    fn index_then_query_neighborhood() {
        let store = mem_store();
        let src = "fn helper() {}\nfn main() { helper(); }\n";
        store.index_file("src/main.rs", src, 0).unwrap();

        let nb = store.neighborhood("helper").expect("helper indexed");
        assert_eq!(nb.definition.name, "helper");
        assert!(nb.callers.iter().any(|c| c.name == "main"));
        assert!(!nb.stale);
    }

    #[test]
    fn is_current_tracks_content_hash() {
        let store = mem_store();
        let src = "fn a() {}\n";
        store.index_file("a.rs", src, 0).unwrap();
        assert!(store.is_current("a.rs", src));
        assert!(!store.is_current("a.rs", "fn a() {}\nfn b() {}\n"));
    }

    #[test]
    fn reindex_replaces_prior_symbols() {
        let store = mem_store();
        store.index_file("a.rs", "fn a() {}\n", 0).unwrap();
        store.index_file("a.rs", "fn c() {}\n", 1).unwrap();
        assert!(store.neighborhood("a").is_none());
        assert!(store.neighborhood("c").is_some());
    }

    #[test]
    fn reindex_handles_path_with_quote() {
        let store = mem_store();
        let path = "weird'name.rs";
        store.index_file(path, "fn first_sym() {}\n", 0).unwrap();
        store.index_file(path, "fn second_sym() {}\n", 1).unwrap();
        assert!(store.neighborhood("first_sym").is_none(), "old symbol must be gone after reindex");
        assert!(store.neighborhood("second_sym").is_some(), "new symbol must exist after reindex");
    }
}
