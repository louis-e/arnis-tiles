//! The chunk store: rows of (tile, contributing region, a slice of that tile's elements).
//!
//! Extracts overlap at their borders and a big region is flushed in pieces to bound memory, so
//! a tile accumulates several rows. They are merged once, at finalize, where duplicate elements
//! are dropped by OSM id - cheaper and far simpler than merging mid-bake.
//!
//! One store per continent, not one for the planet: finalize can then publish a continent and
//! reclaim its store immediately, which is what keeps peak disk near 70 GB instead of 135.
//! Blobs are zstd'd on the way in for the same reason - the store is the biggest thing on disk
//! during a run, and it is written once and read once.

use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Fast level: this data is read once, by finalize, which re-compresses at 19 for publication.
const CHUNK_ZSTD_LEVEL: i32 = 3;

/// One store per continent, named so `finalize` can find them by globbing.
pub fn store_path(work: &Path, continent: &str) -> PathBuf {
    work.join(format!("chunks-{continent}.db"))
}

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
        let packed = zstd::encode_all(data, CHUNK_ZSTD_LEVEL).map_err(|e| e.to_string())?;
        self.conn
            .execute(
                "INSERT INTO chunk(x,y,region,data) VALUES (?1,?2,?3,?4)",
                rusqlite::params![x, y, region, packed],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Undoes [`put`]'s compression. Kept next to it so the two cannot drift.
    pub fn unpack(blob: &[u8]) -> Result<Vec<u8>, String> {
        zstd::decode_all(blob).map_err(|e| e.to_string())
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
