//! SQLite-backed store: persist files, freshness checks, neighborhood queries.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::extract::extract;
use crate::model::{Neighborhood, Symbol, SymbolKind, SymbolRef};
use crate::resolve::resolve_file;
use crate::schema::init_graph_schema;
use rinne_types::skip::source_lang;

pub struct Store {
    pub(crate) conn: Connection,
    root: PathBuf,
}

impl Store {
    pub fn open(db_path: &Path, root: &Path) -> rusqlite::Result<Store> {
        let conn = Connection::open(db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;",
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Wipe stale graph tables on a schema/extractor version bump so an upgraded
        // binary re-extracts every file automatically (no manual re-index needed).
        crate::schema::migrate_graph_if_stale(&conn)?;
        init_graph_schema(&conn)?;
        Ok(Store {
            conn,
            root: root.to_path_buf(),
        })
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
                // Insert each symbol, capturing its rowid by position. Distinct rows
                // get distinct ids, so same-name defs in one file are not collapsed.
                let mut ids: Vec<i64> = Vec::with_capacity(raw_symbols.len());
                for rs in &raw_symbols {
                    tx.execute(
                        "INSERT INTO graph_symbols (file, name, kind, start_line, end_line, signature, container) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        rusqlite::params![
                            path,
                            rs.name,
                            rs.kind.as_str(),
                            rs.start_line,
                            rs.end_line,
                            rs.signature,
                            rs.container,
                        ],
                    )?;
                    ids.push(tx.last_insert_rowid());
                }

                let (_, edges) = resolve_file(path, raw_symbols, raw_edges, |i| ids[i]);

                for edge in &edges {
                    tx.execute(
                        "INSERT INTO graph_edges (src_symbol, dst_symbol, dst_name, kind) \
                         VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![edge.src, edge.dst, edge.dst_name, edge.kind.as_str(),],
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

    /// Returns the definition + callers + callees + imports for the first symbol
    /// matching `symbol_name`.
    ///
    /// When the same name is defined in more than one place this collapses to one
    /// arbitrary match; prefer [`Store::neighborhood_all`] in that case.
    pub fn neighborhood(&self, symbol_name: &str) -> Option<Neighborhood> {
        self.neighborhood_all(symbol_name).into_iter().next()
    }

    /// Returns one [`Neighborhood`] per definition site matching `symbol_name`,
    /// ordered by file then start line (deterministic). Empty when the name is
    /// not indexed. This is the multi-result form that avoids the single-winner
    /// collapse of [`Store::neighborhood`].
    pub fn neighborhood_all(&self, symbol_name: &str) -> Vec<Neighborhood> {
        // Every definition row for this name, in a stable order.
        let defs: Vec<(i64, String, u32, u32)> = self
            .conn
            .prepare(
                "SELECT id, file, start_line, COALESCE(end_line, start_line) \
                 FROM graph_symbols WHERE name = ?1 ORDER BY file, start_line",
            )
            .and_then(|mut stmt| {
                stmt.query_map([symbol_name], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            })
            .unwrap_or_default();

        defs.into_iter()
            .map(|(sym_id, file, start_line, end_line)| {
                self.neighborhood_for(symbol_name, sym_id, file, start_line, end_line)
            })
            .collect()
    }

    /// Build the neighborhood for one specific definition row.
    fn neighborhood_for(
        &self,
        symbol_name: &str,
        sym_id: i64,
        file: String,
        start_line: u32,
        end_line: u32,
    ) -> Neighborhood {
        let definition = SymbolRef {
            name: symbol_name.to_string(),
            file: file.clone(),
            line: start_line,
            end_line,
        };

        // Callers: symbols whose id is src_symbol of an edge with dst_symbol = sym_id.
        let callers = self
            .conn
            .prepare(
                "SELECT s.name, s.file, s.start_line, COALESCE(s.end_line, s.start_line)
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
                        end_line: row.get(3)?,
                    })
                })
                .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            })
            .unwrap_or_default();

        // Callees: symbols whose id is dst_symbol of an edge with src_symbol = sym_id.
        let callees = self
            .conn
            .prepare(
                "SELECT s.name, s.file, s.start_line, COALESCE(s.end_line, s.start_line)
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
                        end_line: row.get(3)?,
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
                "SELECT COALESCE(s.name, e.dst_name), COALESCE(s.file, ''), \
                        COALESCE(s.start_line, 0), COALESCE(s.end_line, s.start_line, 0)
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
                        end_line: row.get(3)?,
                    })
                })
                .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            })
            .unwrap_or_default();

        let stale = {
            let abs = self.root.join(&definition.file);
            match std::fs::read_to_string(&abs) {
                Ok(contents) => {
                    let cur = Self::hash_of(&contents);
                    let stored: Option<String> = self
                        .conn
                        .query_row(
                            "SELECT content_hash FROM graph_files WHERE path = ?1",
                            [definition.file.as_str()],
                            |r| r.get(0),
                        )
                        .ok();
                    stored.map(|s| s != cur).unwrap_or(false)
                }
                Err(_) => false,
            }
        };

        Neighborhood {
            definition,
            callers,
            callees,
            imports,
            stale,
        }
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
                "SELECT name, file, start_line, COALESCE(end_line, start_line) \
                 FROM graph_symbols WHERE file = ?1 AND name = ?2 LIMIT 1",
                rusqlite::params![file, name],
                |row| {
                    Ok(SymbolRef {
                        name: row.get(0)?,
                        file: row.get(1)?,
                        line: row.get(2)?,
                        end_line: row.get(3)?,
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
                "SELECT id, file, name, kind, start_line, end_line, signature, container
                 FROM graph_symbols WHERE file = ?1",
            )
            .and_then(|mut stmt| {
                stmt.query_map([path], |row| {
                    let kind_str: String = row.get(3)?;
                    let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);
                    Ok(Symbol {
                        id: row.get(0)?,
                        file: row.get(1)?,
                        name: row.get(2)?,
                        kind,
                        start_line: row.get(4)?,
                        end_line: row.get(5)?,
                        signature: row.get(6)?,
                        container: row.get(7)?,
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
        Store {
            conn,
            root: std::env::temp_dir(),
        }
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
        assert!(
            store.neighborhood("first_sym").is_none(),
            "old symbol must be gone after reindex"
        );
        assert!(
            store.neighborhood("second_sym").is_some(),
            "new symbol must exist after reindex"
        );
    }

    // REPRODUCTION (Stage 1 bug): two symbols named `build` in different files.
    // `neighborhood("build")` today returns exactly one arbitrary definition
    // (WHERE name = ?1 LIMIT 1, no ORDER BY) and silently drops the other — so
    // the assembler can anchor a worker's context on the WRONG `build`.
    //
    // This test asserts the *fix's* contract: given the caller's file hint, the
    // store can resolve to the RIGHT `build`. It exercises `resolve_in_file`,
    // which already filters by file and is the primitive Stage 1 wires into the
    // assembler. It should PASS today (proving the primitive exists) — the gap is
    // that `neighborhood` ignores it. See the companion `neighborhood`-level
    // assertion below for the part that FAILS today.
    #[test]
    fn resolve_in_file_disambiguates_same_name_across_files() {
        let store = mem_store();
        store
            .index_file(
                "assembler.rs",
                "fn build() {}\nfn assemble() { build(); }\n",
                0,
            )
            .unwrap();
        store
            .index_file(
                "blackboard.rs",
                "fn build() {}\nfn open() { build(); }\n",
                0,
            )
            .unwrap();

        let in_assembler = store
            .resolve_in_file("assembler.rs", "build")
            .expect("build exists in assembler.rs");
        assert_eq!(in_assembler.file, "assembler.rs");

        let in_blackboard = store
            .resolve_in_file("blackboard.rs", "build")
            .expect("build exists in blackboard.rs");
        assert_eq!(in_blackboard.file, "blackboard.rs");
    }

    // STAGE 1 FIX VERIFIED: with two `build` defs indexed, `neighborhood_all`
    // surfaces BOTH definitions' files, ordered deterministically. The singular
    // `neighborhood` still returns one (the first in order) for callers that want
    // a single best-effort result.
    #[test]
    fn neighborhood_surfaces_all_same_name_definitions() {
        let store = mem_store();
        store
            .index_file("assembler.rs", "fn build() {}\n", 0)
            .unwrap();
        store
            .index_file("blackboard.rs", "fn build() {}\n", 0)
            .unwrap();

        let all = store.neighborhood_all("build");
        let mut files: Vec<&str> = all.iter().map(|n| n.definition.file.as_str()).collect();
        files.sort_unstable();
        assert_eq!(
            files,
            vec!["assembler.rs", "blackboard.rs"],
            "neighborhood_all must surface BOTH build definitions, not collapse to one"
        );

        // Deterministic order: ORDER BY file, start_line → assembler.rs first.
        assert_eq!(all.first().unwrap().definition.file, "assembler.rs");

        // Singular form still works, returning the first in order.
        assert_eq!(
            store.neighborhood("build").unwrap().definition.file,
            "assembler.rs"
        );
    }

    // ===== STAGE 2 (within-file duplicate-name collapse) =====
    //
    // `index_file` builds a `name_to_id` HashMap with last-write-wins
    // (store.rs, "Last-write-wins for duplicate names within one file") and the
    // resolver closure uses `unwrap_or(0)`. Two symbols with the SAME name in the
    // SAME file therefore collapse: the first symbol's id is overwritten, so
    // edges that should point at the first are mis-attributed to the second (or
    // aliased to a bogus id). This is distinct from the Stage 1 cross-file case.
    //
    // Ignored until Stage 2: the fix is to key symbols by (name, start_line) or a
    // real per-symbol id so same-name-same-file defs stay distinct.
    // CORRECTED SCOPE: symbols never collapse — graph_symbols.id is AUTOINCREMENT,
    // so distinct rows get distinct ids regardless of the name_to_id HashMap. The
    // real Stage 2 defect is EDGE ATTRIBUTION: `name_to_id` is last-write-wins per
    // file (store.rs), so a call to a name with two same-file definitions resolves
    // to whichever won the map — both callers attach to ONE build, not each to its
    // own. This test targets that edge property directly.
    //
    // `#[ignore]` with the real assertion below: it FAILS today (edges mis-attribute)
    // and is the true Stage 2 red spec. The symbol-distinctness part is kept as a
    // separate always-run assertion since it already holds.
    #[test]
    fn same_name_symbols_in_one_file_get_distinct_ids() {
        let store = mem_store();
        let src = "\
fn build() {}
mod second { pub fn build() {} }
";
        store.index_file("dup.rs", src, 0).unwrap();
        let builds: Vec<_> = store
            .symbols_in("dup.rs")
            .into_iter()
            .filter(|s| s.name == "build")
            .collect();
        // This ALREADY holds (AUTOINCREMENT) — proves symbols themselves don't collapse.
        assert_eq!(builds.len(), 2, "both `build` symbols indexed");
        assert_ne!(builds[0].id, builds[1].id, "distinct rows get distinct ids");
    }

    #[test]
    fn same_name_defs_in_one_file_get_correct_edge_attribution() {
        let store = mem_store();
        // Two `build` in one file, each with its own exclusive caller.
        let src = "\
fn build() {}
fn caller_one() { build(); }
mod second { pub fn build() {} pub fn caller_two() { build(); } }
";
        store.index_file("dup.rs", src, 0).unwrap();

        let builds: Vec<_> = store
            .symbols_in("dup.rs")
            .into_iter()
            .filter(|s| s.name == "build")
            .collect();
        assert_eq!(builds.len(), 2);

        // Each `build` should have exactly its own caller. Today the HashMap
        // collapse routes both callers to one build (so one has 2 callers, the
        // other 0) — this assertion catches that mis-attribution.
        let nbs = store.neighborhood_all("build");
        let caller_counts: Vec<usize> = nbs.iter().map(|n| n.callers.len()).collect();
        assert!(
            caller_counts.iter().all(|&c| c == 1),
            "each same-file `build` must own exactly its caller, got caller counts {caller_counts:?} \
             (last-write-wins mis-attribution)"
        );
    }

    // ===== STAGE 3 (qualified identity via `container`) =====
    //
    // The STRUCTURAL fix: a symbol today is (file, name, kind) with NO container,
    // so `ContextAssembler::build` and `Blackboard::build` are indistinguishable
    // except by file — and within one file, not at all. Stage 3 adds a `container`
    // column (enclosing impl/class/trait) so a symbol has a qualified identity.
    //
    // This test asserts the fix's contract: `symbols_in` surfaces a `container`
    // for a method inside an `impl`. It FAILS/does-not-compile until the schema +
    // extractor carry `container`, hence ignored and written against the future
    // shape. Left as the executable spec for Stage 3.
    #[test]
    fn method_carries_its_impl_container() {
        let store = mem_store();
        // `build` is a method on `ContextAssembler`; its container must be recorded.
        let src = "\
struct ContextAssembler;
impl ContextAssembler {
    fn build(&self) {}
}
";
        store.index_file("assembler.rs", src, 0).unwrap();

        let build = store
            .symbols_in("assembler.rs")
            .into_iter()
            .find(|s| s.name == "build")
            .expect("build method indexed");

        // Stage 3 contract: the method carries its enclosing impl type.
        assert_eq!(
            build.container.as_deref(),
            Some("ContextAssembler"),
            "method `build` must record its container `ContextAssembler`"
        );
        assert!(
            build.signature.as_deref().unwrap_or("").contains("build"),
            "placeholder: strengthen to assert container == \"ContextAssembler\" once \
             the container column is added"
        );
    }

    #[test]
    fn neighborhood_reports_stale_when_file_changed_on_disk() {
        let dir = std::env::temp_dir().join(format!("rinne-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("m.rs");
        std::fs::write(&file, "fn helper() {}\nfn main() { helper(); }\n").unwrap();

        let store = Store::open(&dir.join("state.db"), &dir).unwrap();
        let src = std::fs::read_to_string(&file).unwrap();
        store.index_file("m.rs", &src, 0).unwrap();

        // Fresh: not stale.
        assert!(!store.neighborhood("helper").unwrap().stale);

        // Mutate the file on disk without reindexing.
        std::fs::write(&file, "fn helper() {}\nfn main() { helper(); helper(); }\n").unwrap();
        assert!(
            store.neighborhood("helper").unwrap().stale,
            "must report stale after on-disk edit"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
