//! SQLite DDL for the code graph tables, colocated with the blackboard state.

use rusqlite::Connection;

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
            signature  TEXT
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
}
