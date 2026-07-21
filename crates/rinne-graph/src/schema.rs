//! SQLite DDL for the code graph tables, colocated with the blackboard state.

use rusqlite::Connection;

/// Version of the graph schema *and* the extractor's output shape.
///
/// Bump this whenever a change would make previously-indexed rows stale even
/// though the source files are unchanged — e.g. a new column, a change to how a
/// field is extracted, or a new edge kind. On open, a graph whose stored version
/// differs is wiped and rebuilt automatically (see [`migrate_graph_if_stale`]),
/// so an upgraded binary re-extracts once with no manual `rm .rinne/state.db`.
///
/// History:
///   1 — initial versioned schema; adds `container` column + container extraction
///       and scope-aware edge resolution (previously unversioned, no `container`).
pub const GRAPH_SCHEMA_VERSION: i64 = 1;

/// The graph tables. Kept as a list so migration can drop exactly these without
/// touching sibling tables (run state, budget ledger) that share the same DB.
const GRAPH_TABLES: &[&str] = &["graph_edges", "graph_symbols", "graph_files"];

/// The SQLite key under which we stash the graph schema version. We can't use the
/// DB-wide `PRAGMA user_version` because the graph shares `state.db` with the
/// blackboard, so a small dedicated table scopes the version to the graph.
const VERSION_TABLE: &str = "graph_schema_version";

/// If the stored graph schema version differs from [`GRAPH_SCHEMA_VERSION`], drop
/// the graph tables so [`init_graph_schema`] recreates them empty and the next
/// index pass re-extracts every file. No-op when the version already matches
/// (the common path) or on a brand-new DB (version 0 → treated as "needs build",
/// but the tables don't exist yet so the drops are harmless).
pub fn migrate_graph_if_stale(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {VERSION_TABLE} (version INTEGER NOT NULL);"
    ))?;

    let stored: Option<i64> = conn
        .query_row(
            &format!("SELECT version FROM {VERSION_TABLE} LIMIT 1"),
            [],
            |r| r.get(0),
        )
        .ok();

    if stored == Some(GRAPH_SCHEMA_VERSION) {
        return Ok(()); // up to date — nothing to do
    }

    // Stale or absent: drop graph tables (only these; siblings untouched) and
    // stamp the current version. Recreation happens in init_graph_schema.
    for table in GRAPH_TABLES {
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {table};"))?;
    }
    conn.execute(&format!("DELETE FROM {VERSION_TABLE}"), [])?;
    conn.execute(
        &format!("INSERT INTO {VERSION_TABLE} (version) VALUES (?1)"),
        [GRAPH_SCHEMA_VERSION],
    )?;
    Ok(())
}

pub fn init_graph_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS graph_files (
            path         TEXT PRIMARY KEY,
            lang         TEXT NOT NULL,
            content_hash TEXT NOT NULL,
            mtime        INTEGER NOT NULL,
            indexed_at   INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS graph_symbols (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            file       TEXT NOT NULL,
            name       TEXT NOT NULL,
            kind       TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line   INTEGER NOT NULL,
            signature  TEXT,
            -- Enclosing type/class/trait for a method (e.g. "ContextAssembler" for
            -- ContextAssembler::build), NULL for free functions. Gives same-file
            -- same-name defs a qualified identity so they can be disambiguated.
            container  TEXT
        );

        CREATE TABLE IF NOT EXISTS graph_edges (
            src_symbol INTEGER,
            dst_symbol INTEGER,
            dst_name   TEXT NOT NULL,
            kind       TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_symbols_name ON graph_symbols(name);
        CREATE INDEX IF NOT EXISTS idx_symbols_file ON graph_symbols(file);
        CREATE INDEX IF NOT EXISTS idx_edges_src ON graph_edges(src_symbol);
        CREATE INDEX IF NOT EXISTS idx_edges_dst ON graph_edges(dst_symbol);
        CREATE INDEX IF NOT EXISTS idx_edges_dstname ON graph_edges(dst_name);
        "#,
    )
    // Note: schema changes that must invalidate existing rows are handled by
    // `migrate_graph_if_stale` (version-gated table wipe), not per-column ALTERs.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent_and_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        init_graph_schema(&conn).unwrap();
        init_graph_schema(&conn).unwrap(); // second call must not error

        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' \
                 AND name IN ('graph_files','graph_symbols','graph_edges')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn stale_version_wipes_graph_tables_but_keeps_siblings() {
        let conn = Connection::open_in_memory().unwrap();

        // Simulate an OLD graph: a sibling (non-graph) table with data that must
        // survive, plus a graph table stamped with a stale version.
        conn.execute_batch(
            "CREATE TABLE run_meta (k TEXT, v TEXT);
             INSERT INTO run_meta VALUES ('run', 'keep-me');",
        )
        .unwrap();
        migrate_graph_if_stale(&conn).unwrap();
        init_graph_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO graph_symbols (file, name, kind, start_line, end_line) \
             VALUES ('a.rs','old','function',1,1)",
            [],
        )
        .unwrap();

        // Force a version mismatch as if the binary was upgraded.
        conn.execute(
            &format!("UPDATE {VERSION_TABLE} SET version = ?1"),
            [GRAPH_SCHEMA_VERSION - 1],
        )
        .unwrap();

        // Reopen path: migrate should wipe graph tables, keep run_meta.
        migrate_graph_if_stale(&conn).unwrap();
        init_graph_schema(&conn).unwrap();

        let graph_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM graph_symbols", [], |r| r.get(0))
            .unwrap();
        assert_eq!(graph_rows, 0, "stale graph rows must be wiped");

        let sibling: String = conn
            .query_row("SELECT v FROM run_meta WHERE k='run'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sibling, "keep-me", "non-graph tables must survive the wipe");

        let ver: i64 = conn
            .query_row(&format!("SELECT version FROM {VERSION_TABLE}"), [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(ver, GRAPH_SCHEMA_VERSION, "version stamped to current");
    }

    #[test]
    fn matching_version_is_a_noop() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_graph_if_stale(&conn).unwrap();
        init_graph_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO graph_symbols (file, name, kind, start_line, end_line) \
             VALUES ('a.rs','keep','function',1,1)",
            [],
        )
        .unwrap();

        // Second open with the SAME version must NOT wipe existing rows.
        migrate_graph_if_stale(&conn).unwrap();
        init_graph_schema(&conn).unwrap();

        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM graph_symbols", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "matching version must preserve indexed rows");
    }
}
