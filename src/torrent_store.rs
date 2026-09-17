//! Durable record of every torrent added, and of the files inside each --
//! so a restart doesn't lose what you downloaded, and so a `.db` file is
//! enough to find (and re-add) that content later.
//!
//! Why this exists: `librqbit`'s own session persistence covers the
//! torrent *swarm* state, but everything salo tracks around it was
//! in-memory only -- the source URL each release came from, and the
//! seed-limit policy (with the moment a torrent finished, which the
//! reaper needs to time a seed limit). All of that vanished on restart.
//!
//! The file list is stored too, because it's the useful part for
//! recovery: it says which files a torrent contained, how big they were,
//! and where they were saved, which is what's needed to find them again
//! on disk and re-add the torrent (by magnet, or from the site again) if
//! the download directory is ever lost.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

/// One file inside a stored torrent. `file_id` is librqbit's own index
/// within the torrent, so a re-added torrent maps back to the same file.
#[derive(Debug, Clone)]
pub struct StoredFile {
    pub file_id: u64,
    pub name: String,
    pub len: u64,
}

/// A torrent as recorded in SQLite -- everything needed to find its files
/// on disk, or to re-add it.
#[derive(Debug, Clone)]
pub struct StoredTorrent {
    pub info_hash: String,
    pub name: String,
    /// The magnet (or site download URL) it was added from -- enough to
    /// re-add it.
    pub source: Option<String>,
    pub source_url: Option<String>,
    /// Where its files were saved.
    pub output_folder: String,
    pub total_bytes: u64,
    pub added_at: i64,
    pub finished_at: Option<i64>,
    pub seed_minutes: Option<u64>,
    pub seed_ratio: Option<f64>,
    /// False once its torrent was removed from the client while keeping the
    /// downloaded files -- the record stays so those files can still be
    /// found (that's the point of storing this).
    pub active: bool,
    pub files: Vec<StoredFile>,
}

pub struct TorrentStore {
    conn: Mutex<Connection>,
}

impl TorrentStore {
    /// Opens (creating if needed) the store at `path`, alongside the
    /// config store in the same database file.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open torrent database at {}", path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS torrents (
                info_hash      TEXT PRIMARY KEY,
                name           TEXT NOT NULL,
                source         TEXT,
                source_url     TEXT,
                output_folder  TEXT NOT NULL,
                total_bytes    INTEGER NOT NULL DEFAULT 0,
                added_at       INTEGER NOT NULL,
                finished_at    INTEGER,
                seed_minutes   INTEGER,
                seed_ratio     REAL,
                active         INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS torrent_files (
                info_hash  TEXT NOT NULL,
                file_id    INTEGER NOT NULL,
                name       TEXT NOT NULL,
                len        INTEGER NOT NULL,
                PRIMARY KEY (info_hash, file_id),
                FOREIGN KEY (info_hash) REFERENCES torrents(info_hash) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS torrent_files_hash ON torrent_files(info_hash);",
        )
        .context("failed to initialize torrent schema")?;
        // Older databases predate `active`; add it rather than making the
        // user start over (an ALTER on an existing column is an error, so
        // the failure is expected and ignored).
        let _ = conn.execute(
            "ALTER TABLE torrents ADD COLUMN active INTEGER NOT NULL DEFAULT 1",
            [],
        );
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("torrent db mutex poisoned")
    }

    /// Records a torrent and its files. Re-adding an existing info hash
    /// updates the row and replaces the file list (a re-add can discover
    /// a different set of files).
    pub fn upsert(&self, torrent: &StoredTorrent) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction().context("failed to begin transaction")?;
        tx.execute(
            "INSERT INTO torrents
                (info_hash, name, source, source_url, output_folder, total_bytes,
                 added_at, finished_at, seed_minutes, seed_ratio, active)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(info_hash) DO UPDATE SET
                name = excluded.name,
                source = COALESCE(excluded.source, torrents.source),
                source_url = COALESCE(excluded.source_url, torrents.source_url),
                output_folder = excluded.output_folder,
                total_bytes = excluded.total_bytes,
                finished_at = COALESCE(torrents.finished_at, excluded.finished_at),
                seed_minutes = excluded.seed_minutes,
                seed_ratio = excluded.seed_ratio,
                active = excluded.active",
            params![
                torrent.info_hash,
                torrent.name,
                torrent.source,
                torrent.source_url,
                torrent.output_folder,
                torrent.total_bytes as i64,
                torrent.added_at,
                torrent.finished_at,
                torrent.seed_minutes.map(|m| m as i64),
                torrent.seed_ratio,
                torrent.active as i64,
            ],
        )
        .context("failed to upsert torrent")?;

        // Replace the file list wholesale: simpler than diffing, and a
        // re-add genuinely can see a different set.
        tx.execute(
            "DELETE FROM torrent_files WHERE info_hash = ?1",
            params![torrent.info_hash],
        )?;
        {
            let mut stmt = tx
                .prepare("INSERT INTO torrent_files (info_hash, file_id, name, len) VALUES (?1, ?2, ?3, ?4)")
                .context("failed to prepare file insert")?;
            for file in &torrent.files {
                stmt.execute(params![
                    torrent.info_hash,
                    file.file_id as i64,
                    file.name,
                    file.len as i64,
                ])
                .context("failed to insert torrent file")?;
            }
        }
        tx.commit().context("failed to commit torrent")?;
        Ok(())
    }

    /// The moment a torrent is first seen finished, so a seed limit can be
    /// measured from it. Only writes the first time.
    pub fn mark_finished(&self, info_hash: &str, finished_at: i64) -> Result<()> {
        self.lock().execute(
            "UPDATE torrents SET finished_at = ?2
             WHERE info_hash = ?1 AND finished_at IS NULL",
            params![info_hash, finished_at],
        )?;
        Ok(())
    }

    pub fn set_seed_limit(
        &self,
        info_hash: &str,
        seed_minutes: Option<u64>,
        seed_ratio: Option<f64>,
    ) -> Result<()> {
        self.lock().execute(
            "UPDATE torrents SET seed_minutes = ?2, seed_ratio = ?3 WHERE info_hash = ?1",
            params![
                info_hash,
                seed_minutes.map(|m| m as i64),
                seed_ratio
            ],
        )?;
        Ok(())
    }

    /// Marks a torrent as no longer active (removed from the client but its
    /// files kept) -- the record stays so the files can still be found.
    pub fn mark_inactive(&self, info_hash: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE torrents SET active = 0 WHERE info_hash = ?1",
            params![info_hash],
        )?;
        Ok(())
    }

    /// Marks a torrent active again, for a re-add of something previously
    /// removed.
    pub fn mark_active(&self, info_hash: &str) -> Result<()> {
        self.lock().execute(
            "UPDATE torrents SET active = 1 WHERE info_hash = ?1",
            params![info_hash],
        )?;
        Ok(())
    }

    /// Persists the source URL for an already-stored torrent.
    pub fn set_source_url_stored(&self, info_hash: &str, url: Option<&str>) -> Result<()> {
        self.lock().execute(
            "UPDATE torrents SET source_url = ?2 WHERE info_hash = ?1",
            params![info_hash, url],
        )?;
        Ok(())
    }

    /// Removes a stored record (and its file list). Used by the "forget"
    /// action; the files on disk are untouched.
    pub fn delete(&self, info_hash: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "DELETE FROM torrent_files WHERE info_hash = ?1",
            params![info_hash],
        )?;
        conn.execute("DELETE FROM torrents WHERE info_hash = ?1", params![info_hash])?;
        Ok(())
    }

    /// Every stored torrent, newest first, with its files.
    pub fn all(&self) -> Result<Vec<StoredTorrent>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT info_hash, name, source, source_url, output_folder, total_bytes,
                        added_at, finished_at, seed_minutes, seed_ratio, active
                 FROM torrents ORDER BY added_at DESC",
            )
            .context("failed to prepare torrent query")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(StoredTorrent {
                    info_hash: row.get(0)?,
                    name: row.get(1)?,
                    source: row.get(2)?,
                    source_url: row.get(3)?,
                    output_folder: row.get(4)?,
                    total_bytes: row.get::<_, i64>(5)?.max(0) as u64,
                    added_at: row.get(6)?,
                    finished_at: row.get(7)?,
                    seed_minutes: row.get::<_, Option<i64>>(8)?.map(|m| m.max(0) as u64),
                    seed_ratio: row.get(9)?,
                    active: row.get::<_, i64>(10)? != 0,
                    files: Vec::new(),
                })
            })
            .context("failed to read torrents")?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut files_stmt = conn
            .prepare("SELECT file_id, name, len FROM torrent_files WHERE info_hash = ?1 ORDER BY file_id")
            .context("failed to prepare file query")?;
        let mut torrents = rows;
        for torrent in &mut torrents {
            torrent.files = files_stmt
                .query_map(params![torrent.info_hash], |row| {
                    Ok(StoredFile {
                        file_id: row.get::<_, i64>(0)?.max(0) as u64,
                        name: row.get(1)?,
                        len: row.get::<_, i64>(2)?.max(0) as u64,
                    })
                })
                .context("failed to read torrent files")?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        Ok(torrents)
    }

    pub fn get(&self, info_hash: &str) -> Result<Option<StoredTorrent>> {
        Ok(self
            .all()?
            .into_iter()
            .find(|torrent| torrent.info_hash == info_hash))
    }

    /// The saved path of a file stored for a torrent: where it was meant
    /// to land on disk. Useful for finding content again after a restart,
    /// or after the torrent was removed from the client.
    pub fn saved_path(&self, output_folder: &str, file_name: &str) -> std::path::PathBuf {
        std::path::Path::new(output_folder).join(file_name)
    }
}

/// Seconds since the Unix epoch, for the wall-clock timestamps stored here.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> TorrentStore {
        TorrentStore::open(Path::new(":memory:")).expect("in-memory store")
    }

    fn sample(info_hash: &str) -> StoredTorrent {
        StoredTorrent {
            info_hash: info_hash.into(),
            name: "Some Release".into(),
            source: Some("magnet:?xt=urn:btih:abc".into()),
            source_url: Some("https://example.invalid/t/1".into()),
            output_folder: "downloads".into(),
            total_bytes: 2048,
            added_at: 1_700_000_000,
            finished_at: None,
            seed_minutes: None,
            seed_ratio: None,
            active: true,
            files: vec![
                StoredFile {
                    file_id: 0,
                    name: "a.mkv".into(),
                    len: 1024,
                },
                StoredFile {
                    file_id: 1,
                    name: "b.nfo".into(),
                    len: 1024,
                },
            ],
        }
    }

    #[test]
    fn round_trips_a_torrent_and_its_files() {
        let store = store();
        store.upsert(&sample("hash1")).expect("upsert");
        let all = store.all().expect("all");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].info_hash, "hash1");
        assert_eq!(all[0].name, "Some Release");
        assert_eq!(all[0].files.len(), 2);
        assert_eq!(all[0].files[1].name, "b.nfo");
        assert_eq!(all[0].total_bytes, 2048);
    }

    #[test]
    fn re_upsert_replaces_files_and_keeps_finished_at() {
        let store = store();
        store.upsert(&sample("hash1")).expect("upsert");
        store.mark_finished("hash1", 1_700_000_999).expect("finish");

        // A re-add with a different file set.
        let mut updated = sample("hash1");
        updated.files = vec![StoredFile {
            file_id: 0,
            name: "only.mkv".into(),
            len: 4096,
        }];
        store.upsert(&updated).expect("re-upsert");

        let stored = store.get("hash1").expect("get").expect("exists");
        assert_eq!(stored.files.len(), 1, "old file rows must not linger");
        assert_eq!(stored.files[0].name, "only.mkv");
        // finished_at is the original observation, not clobbered by the
        // re-add (the reaper times seed limits from it).
        assert_eq!(stored.finished_at, Some(1_700_000_999));
    }

    #[test]
    fn mark_finished_only_records_the_first_time() {
        let store = store();
        store.upsert(&sample("h")).expect("upsert");
        store.mark_finished("h", 111).expect("first");
        store.mark_finished("h", 222).expect("second");
        assert_eq!(store.get("h").unwrap().unwrap().finished_at, Some(111));
    }

    #[test]
    fn delete_removes_files_too() {
        // The record and its file rows both go.
        let store = store();
        store.upsert(&sample("h")).expect("upsert");
        assert_eq!(store.get("h").unwrap().unwrap().files.len(), 2);
        store.delete("h").expect("delete");
        assert!(store.all().expect("all").is_empty());
    }

    #[test]
    fn seed_limit_round_trips() {
        let store = store();
        store.upsert(&sample("h")).expect("upsert");
        store.set_seed_limit("h", Some(45), Some(1.5)).expect("set");
        let stored = store.get("h").unwrap().unwrap();
        assert_eq!(stored.seed_minutes, Some(45));
        assert_eq!(stored.seed_ratio, Some(1.5));
    }

    #[test]
    fn saved_path_reconstructs_a_files_location() {
        // What makes the stored file list useful: given the record, this is
        // where the file lives on disk.
        let store = store();
        let path = store.saved_path("downloads/Some Release", "a.mkv");
        assert_eq!(path, std::path::Path::new("downloads/Some Release/a.mkv"));
    }

    #[test]
    fn removing_a_torrent_keeps_its_record_until_files_go() {
        // The restore use case: removing a torrent from the client while
        // keeping the files must leave the record (and file list) behind.
        let store = store();
        store.upsert(&sample("h")).expect("upsert");

        store.mark_inactive("h").expect("inactive");
        let stored = store.get("h").expect("get").expect("record stays");
        assert!(!stored.active);
        assert_eq!(stored.files.len(), 2, "file list must survive");

        // A delete that also removes the files drops the record entirely.
        store.delete("h").expect("delete");
        assert!(store.get("h").expect("get").is_none());
    }

    #[test]
    fn re_adding_makes_a_record_active_again() {
        let store = store();
        store.upsert(&sample("h")).expect("upsert");
        store.mark_inactive("h").expect("inactive");
        store.mark_active("h").expect("active");
        assert!(store.get("h").unwrap().unwrap().active);
    }

    #[test]
    fn survives_reopening_the_same_file() {
        // The point of the whole module: data outlives the process.
        let dir = std::env::temp_dir().join(format!("salo-store-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("torrents.db");

        {
            let store = TorrentStore::open(&path).expect("open");
            store.upsert(&sample("persisted")).expect("upsert");
        }
        {
            let store = TorrentStore::open(&path).expect("reopen");
            let stored = store.get("persisted").expect("get").expect("exists");
            assert_eq!(stored.files.len(), 2);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
