//! Machine state in SQLite (`CONTEXT.md` §12, §14).
//!
//! `state.db` holds everything needed to reconstruct a run: node statuses,
//! iteration counts, the budget ledger, and quota buckets. Workers are amnesiac,
//! so a killed run resumes entirely from here (`CONTEXT.md` §12 persistence).

use std::path::Path;

use rusqlite::Connection;

use crate::worker::Usage;
use crate::{Result, RinneError};

// The node lifecycle status now lives in `rinne-types` (the Blackboard seam);
// re-export it so existing `rinne_core::state::NodeStatus` paths keep working.
pub use rinne_types::NodeStatus;

/// One recorded worker invocation, for `rinne logs`.
#[derive(Debug, Clone)]
pub struct UsageRow {
    pub node_id: String,
    pub worker: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub wall_ms: u64,
    pub ts: u64,
}

/// A handle to the run's SQLite state.
pub struct State {
    conn: Connection,
}

impl State {
    /// Open (creating if needed) the state database at `path`, ensuring schema.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(sql_err)?;
        // WAL gives durable, concurrent-friendly writes and clean recovery.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(sql_err)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql_err)?;
        let state = State { conn };
        state.init_schema()?;
        Ok(state)
    }

    /// Open an in-memory database (for tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(sql_err)?;
        let state = State { conn };
        state.init_schema()?;
        Ok(state)
    }

    /// Clear all run state in place (node statuses, iterations, ledger, meta),
    /// keeping the connection and schema. Used by `Blackboard::reset_run` so a
    /// fresh goal starts clean without deleting the db file out from under an
    /// open connection.
    pub fn reset(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "DELETE FROM nodes; DELETE FROM usage_ledger; \
                 DELETE FROM quota_buckets; DELETE FROM run_meta;",
            )
            .map_err(sql_err)?;
        Ok(())
    }

    fn init_schema(&self) -> Result<()> {
        self.conn
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS nodes (
                    node_id    TEXT PRIMARY KEY,
                    status     TEXT NOT NULL DEFAULT 'pending',
                    iterations INTEGER NOT NULL DEFAULT 0,
                    worker     TEXT,
                    updated_at INTEGER NOT NULL DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS usage_ledger (
                    id                INTEGER PRIMARY KEY AUTOINCREMENT,
                    node_id           TEXT NOT NULL,
                    worker            TEXT,
                    prompt_tokens     INTEGER NOT NULL,
                    completion_tokens INTEGER NOT NULL,
                    wall_ms           INTEGER NOT NULL,
                    ts                INTEGER NOT NULL DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS quota_buckets (
                    worker     TEXT PRIMARY KEY,
                    tokens     REAL NOT NULL,
                    updated_at INTEGER NOT NULL DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS run_meta (
                    key   TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                "#,
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// Ensure a node row exists, without disturbing an existing one (so resume
    /// preserves prior status). Call once per node when a plan is loaded.
    pub fn ensure_node(&self, node_id: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO nodes (node_id, status, iterations, updated_at)
                 VALUES (?1, 'pending', 0, ?2)",
                rusqlite::params![node_id, now()],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    pub fn set_status(&self, node_id: &str, status: NodeStatus) -> Result<()> {
        self.conn
            .execute(
                "UPDATE nodes SET status = ?2, updated_at = ?3 WHERE node_id = ?1",
                rusqlite::params![node_id, status.as_str(), now()],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    pub fn set_worker(&self, node_id: &str, worker: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE nodes SET worker = ?2, updated_at = ?3 WHERE node_id = ?1",
                rusqlite::params![node_id, worker, now()],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    pub fn status(&self, node_id: &str) -> Result<NodeStatus> {
        let s: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM nodes WHERE node_id = ?1",
                [node_id],
                |r| r.get(0),
            )
            .ok();
        Ok(s.map(|s| NodeStatus::from_str(&s)).unwrap_or(NodeStatus::Pending))
    }

    /// The worker last assigned to a node, if any.
    pub fn worker(&self, node_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT worker FROM nodes WHERE node_id = ?1",
                [node_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten())
    }

    /// Increment a node's iteration counter and return the new value.
    pub fn incr_iteration(&self, node_id: &str) -> Result<u32> {
        self.conn
            .execute(
                "UPDATE nodes SET iterations = iterations + 1, updated_at = ?2 WHERE node_id = ?1",
                rusqlite::params![node_id, now()],
            )
            .map_err(sql_err)?;
        self.iterations(node_id)
    }

    pub fn iterations(&self, node_id: &str) -> Result<u32> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT iterations FROM nodes WHERE node_id = ?1",
                [node_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(n as u32)
    }

    /// Record token/time usage for a node invocation.
    pub fn record_usage(&self, node_id: &str, worker: &str, usage: &Usage) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO usage_ledger
                   (node_id, worker, prompt_tokens, completion_tokens, wall_ms, ts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    node_id,
                    worker,
                    usage.prompt_tokens as i64,
                    usage.completion_tokens as i64,
                    usage.wall_ms as i64,
                    now()
                ],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    /// Total iterations recorded across all nodes (the run-level loop count).
    pub fn total_iterations(&self) -> Result<u32> {
        let n: i64 = self
            .conn
            .query_row("SELECT COALESCE(SUM(iterations), 0) FROM nodes", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        Ok(n as u32)
    }

    /// Aggregate token usage across the run.
    pub fn total_usage(&self) -> Result<Usage> {
        let (p, c, w): (i64, i64, i64) = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0), \
                        COALESCE(SUM(wall_ms),0) FROM usage_ledger",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap_or((0, 0, 0));
        Ok(Usage {
            prompt_tokens: p as u64,
            completion_tokens: c as u64,
            wall_ms: w as u64,
        })
    }

    /// Per-invocation usage rows for trajectory inspection (`rinne logs`).
    pub fn usage_rows(&self) -> Result<Vec<UsageRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT node_id, worker, prompt_tokens, completion_tokens, wall_ms, ts \
                 FROM usage_ledger ORDER BY id ASC",
            )
            .map_err(sql_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(UsageRow {
                    node_id: r.get(0)?,
                    worker: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    prompt_tokens: r.get::<_, i64>(2)? as u64,
                    completion_tokens: r.get::<_, i64>(3)? as u64,
                    wall_ms: r.get::<_, i64>(4)? as u64,
                    ts: r.get::<_, i64>(5)? as u64,
                })
            })
            .map_err(sql_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(sql_err)?);
        }
        Ok(out)
    }

    /// Store/read a run-level scalar (e.g. the start timestamp).
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO run_meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = ?2",
                rusqlite::params![key, value],
            )
            .map_err(sql_err)?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM run_meta WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .ok())
    }
}

fn sql_err(e: rusqlite::Error) -> RinneError {
    RinneError::Blackboard(format!("sqlite: {e}"))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
