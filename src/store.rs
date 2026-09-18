//! The chunk store: one row per (tile, contributing region).
//!
//! Extracts overlap at their borders, so a tile can be written by more than one of them.
//! Rows are kept separate during the bake and merged once, at finalize, where duplicate
//! elements are dropped by OSM id - cheaper and far simpler than merging mid-bake.

use rusqlite::Connection;
use std::path::Path;

pub struct ChunkStore {
    conn: Connection,
    in_tx: bool,
}

impl ChunkStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        // Bulk-append workload: the WAL and a relaxed sync are worth hours here, and a
        // crashed bake is re-run per region anyway (see `done`).
        for p in [
            "PRAGMA journal_mode=WAL",
            "PRAGMA synchronous=NORMAL",
            "PRAGMA cache_size=-262144",
        ] {
            conn.execute_batch(p).map_err(|e| e.to_string())?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS chunk(
                 x INTEGER NOT NULL, y INTEGER NOT NULL,
                 region TEXT NOT NULL, data BLOB NOT NULL);
             CREATE TABLE IF NOT EXISTS done(
                 region TEXT PRIMARY KEY, bytes INTEGER, tiles INTEGER, finished_at TEXT);",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self { conn, in_tx: false })
    }

    pub fn begin(&mut self) -> Result<(), String> {
        if !self.in_tx {
            self.conn
                .execute_batch("BEGIN")
                .map_err(|e| e.to_string())?;
            self.in_tx = true;
        }
        Ok(())
    }

    pub fn commit(&mut self) -> Result<(), String> {
        if self.in_tx {
            self.conn
                .execute_batch("COMMIT")
                .map_err(|e| e.to_string())?;
            self.in_tx = false;
        }
        Ok(())
    }

    pub fn put(&self, x: u32, y: u32, region: &str, data: &[u8]) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT INTO chunk(x,y,region,data) VALUES (?1,?2,?3,?4)",
                rusqlite::params![x, y, region, data],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn is_done(&self, region: &str) -> Result<bool, String> {
        self.conn
            .query_row("SELECT 1 FROM done WHERE region=?1", [region], |_| Ok(()))
            .map(|_| true)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(false),
                other => Err(other.to_string()),
            })
    }

    pub fn mark_done(&self, region: &str, bytes: u64, tiles: usize) -> Result<(), String> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO done(region,bytes,tiles,finished_at)
                 VALUES (?1,?2,?3,datetime('now'))",
                rusqlite::params![region, bytes as i64, tiles as i64],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Drops a half-written region so a re-run cannot double-insert its tiles.
    pub fn clear_region(&self, region: &str) -> Result<(), String> {
        self.conn
            .execute("DELETE FROM chunk WHERE region=?1", [region])
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub fn index_for_finalize(&self) -> Result<(), String> {
        self.conn
            .execute_batch("CREATE INDEX IF NOT EXISTS chunk_xy ON chunk(x,y)")
            .map_err(|e| e.to_string())
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}
