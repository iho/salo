//! A tiny embedded SQLite store for per-indexer settings (API keys,
//! session cookies, whatever a given indexer definition declares it
//! needs). Bundled via `rusqlite`'s `bundled` feature, which compiles
//! SQLite's C source directly into this binary -- no system libsqlite3,
//! no separate database process, one file on disk next to the binary.
//!
//! This is deliberately generic: it's a `(indexer, key) -> value` table,
//! not anything that knows about credentials specifically. Any indexer
//! module that needs configuration (an API token, say) reads it from
//! here by its own name; nothing in `src/indexers/` currently requires
//! one, so today this is empty until a definition asks for it.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

pub struct ConfigStore {
    conn: Mutex<Connection>,
}

impl ConfigStore {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open config database at {}", path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS indexer_settings (
                indexer TEXT NOT NULL,
                key     TEXT NOT NULL,
                value   TEXT NOT NULL,
                PRIMARY KEY (indexer, key)
            );",
        )
        .context("failed to initialize config schema")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn get_all(&self, indexer: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().expect("config db mutex poisoned");
        let mut stmt = conn
            .prepare("SELECT key, value FROM indexer_settings WHERE indexer = ?1 ORDER BY key")?;
        let rows = stmt
            .query_map(params![indexer], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn set(&self, indexer: &str, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().expect("config db mutex poisoned");
        conn.execute(
            "INSERT INTO indexer_settings (indexer, key, value) VALUES (?1, ?2, ?3)
             ON CONFLICT(indexer, key) DO UPDATE SET value = excluded.value",
            params![indexer, key, value],
        )?;
        Ok(())
    }

    pub fn delete(&self, indexer: &str, key: &str) -> Result<()> {
        let conn = self.conn.lock().expect("config db mutex poisoned");
        conn.execute(
            "DELETE FROM indexer_settings WHERE indexer = ?1 AND key = ?2",
            params![indexer, key],
        )?;
        Ok(())
    }
}
