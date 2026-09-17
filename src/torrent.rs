use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use librqbit::api::TorrentIdOrHash;
use librqbit::{AddTorrent, AddTorrentOptions, Session, SessionOptions};

const METADATA_TIMEOUT: Duration = Duration::from_secs(60);
/// How often the background task checks finished torrents against their
/// configured seed-time limit. Coarser than the limits themselves will
/// ever need to be precise to.
const REAP_INTERVAL: Duration = Duration::from_secs(30);

/// Extensions worth offering an inline `<video>`/`<audio>` preview for.
/// Every file, regardless of extension, still gets a direct download
/// link -- this only decides whether we *also* show a player.
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "avi", "webm", "mov", "m4v"];
const AUDIO_EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "ogg", "m4a", "opus", "aac"];

pub fn is_video_file(name: &str) -> bool {
    let name = name.to_lowercase();
    VIDEO_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

pub fn is_audio_file(name: &str) -> bool {
    let name = name.to_lowercase();
    AUDIO_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

/// The display name carried by an add request, when it has one.
///
/// Only a magnet URL carries this (`dn=`); a `.torrent` document's name
/// lives inside its metadata, which isn't available yet at this point.
fn add_torrent_name(source: &AddTorrent<'_>) -> Option<String> {
    let AddTorrent::Url(url) = source else {
        return None;
    };
    let url = url.as_ref();
    // `dn` is the magnet's display-name parameter. It is percent-encoded
    // in the wild, so decode rather than using the raw slice -- a
    // `%20`-laden folder name is worse than useless.
    let query = url.split_once('?')?.1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("dn="))
        .and_then(percent_decode)
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// Minimal percent-decoding for a magnet name (`%20` -> space).
///
/// Deliberately not a full URL decoder: this project's `reqwest` has no
/// helper exposed for it and a magnet's `dn` only ever needs `%XX` and
/// `+` handling. Invalid escapes are left as-is rather than dropped.
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Makes a torrent name safe to use as a single path component.
///
/// librqbit validates the subfolder IT derives from metadata
/// (`check_valid` rejects path separators), but it does **not** validate a
/// `sub_folder` passed in by the caller -- so a hostile magnet `dn` of
/// `../../etc` would escape the download root through this path. Every
/// separator and control character is folded to `_`, and the result can
/// never be `.` or `..`, so the folder is always exactly one component.
fn sanitize_subfolder(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // Trim dots/spaces from both ends: `..`, `.`, and names Windows
    // refuses to create.
    let trimmed = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if trimmed.is_empty() {
        return "_".to_string();
    }
    // Windows caps a path component at 255 chars; leave room for a
    // deduplicating suffix librqbit may add.
    trimmed.chars().take(150).collect()
}

/// An auto-remove-after-seeding rule for one torrent: once it's finished
/// downloading, remove it (but keep the downloaded files) once *either*
/// configured condition is met -- it's seeded for `seed_for`, or its
/// upload/download ratio has reached `seed_ratio` (the standard way
/// private trackers require you to keep giving back what you took; this
/// just automates stopping once you've hit whatever target you set,
/// rather than tracking it yourself). `finished_at` is filled in the
/// first time the reaper observes the torrent as finished -- `librqbit`'s
/// stats don't track that timestamp themselves.
struct SeedPolicy {
    seed_for: Option<Duration>,
    seed_ratio: Option<f64>,
    finished_at: Option<Instant>,
}

/// Thin wrapper around a single embedded `librqbit` session: one process,
/// one default download directory, no external torrent daemon.
pub struct TorrentEngine {
    session: Arc<Session>,
    seed_policies: Mutex<HashMap<String, SeedPolicy>>,
    /// The indexer-side page each torrent came from, if any -- `librqbit`
    /// has no notion of this, so it's tracked here purely for the detail
    /// page's "view on `<indexer>`" link.
    source_urls: Mutex<HashMap<String, String>>,
    /// Durable record of every torrent and its files, so a restart -- or a
    /// lost download directory -- doesn't lose what was added.
    store: Arc<crate::torrent_store::TorrentStore>,
}

/// One file of an already-added torrent. `downloaded_bytes` is `None`
/// right after `add` (no stats snapshot exists yet) and `Some` when read
/// back through `files()`, so the detail page can show per-file progress.
/// The value is capped at the file's length -- librqbit's chunk
/// accounting can overshoot slightly at piece boundaries.
pub struct TorrentFile {
    pub file_id: usize,
    pub name: String,
    pub len: u64,
    pub downloaded_bytes: Option<u64>,
}

pub struct AddedTorrent {
    pub info_hash: String,
    pub name: String,
    pub files: Vec<TorrentFile>,
}

/// A row for the torrent-management page: current progress plus whatever
/// seed-limit policy (if any) is configured for it.
pub struct TorrentSummary {
    pub info_hash: String,
    pub name: String,
    pub output_folder: String,
    pub finished: bool,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub uploaded_bytes: u64,
    /// Current rates, in bytes per second. 0 when the torrent isn't
    /// running (`stats.live` is `None` once a torrent is stopped/errored).
    pub download_speed: u64,
    pub upload_speed: u64,
    /// True when the torrent is deliberately paused (not errored).
    pub paused: bool,
    pub seed_minutes: Option<u64>,
    pub seed_ratio: Option<f64>,
    pub seeded_for_minutes: Option<u64>,
    pub source_url: Option<String>,
}

impl TorrentSummary {
    /// Upload/download ratio reached so far -- "seed coefficient", the
    /// number a private tracker judges you on. Uploaded over total size
    /// (not over bytes *fetched*): for a torrent that started from
    /// nothing on disk the two are the same, and using total size keeps
    /// the figure stable while the download is still in progress.
    /// `None` until the size is known, since a ratio against 0 is
    /// meaningless rather than infinite.
    pub fn ratio(&self) -> Option<f64> {
        (self.total_bytes > 0).then(|| self.uploaded_bytes as f64 / self.total_bytes as f64)
    }

    /// How far along the *download* is, as a percentage.
    pub fn progress_percent(&self) -> u32 {
        if self.total_bytes == 0 {
            return 0;
        }
        let pct = self.progress_bytes.saturating_mul(100) / self.total_bytes;
        u32::try_from(pct.min(100)).unwrap_or(100)
    }
}

impl TorrentEngine {
    pub async fn new(
        download_dir: PathBuf,
        store: Arc<crate::torrent_store::TorrentStore>,
        worker_threads: usize,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&download_dir).context("failed to create download directory")?;
        // Matches librqbit's own blocking-work semaphore to the tokio
        // runtime's actual worker count (see `main.rs`'s `WORKER_THREADS`)
        // instead of its hardcoded default of 8, so a deliberately small
        // runtime doesn't let disk-I/O work oversubscribe it.
        let session = Session::new_with_opts(
            download_dir,
            SessionOptions {
                runtime_worker_threads: Some(worker_threads),
                ..Default::default()
            },
        )
        .await
        .context("failed to start torrent session")?;
        let engine = Arc::new(Self {
            session,
            seed_policies: Mutex::new(HashMap::new()),
            source_urls: Mutex::new(HashMap::new()),
            store,
        });

        // Reload what was persisted: seed limits (with the moment each
        // torrent finished, which the reaper times from) and source URLs.
        // Without this, a restart silently dropped every seed limit.
        engine.restore_persisted();

        let reaper = Arc::clone(&engine);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(REAP_INTERVAL);
            loop {
                interval.tick().await;
                reaper.reap_once().await;
            }
        });

        Ok(engine)
    }

    /// Rebuilds the in-memory seed policies and source URLs from SQLite so
    /// they survive a restart.
    fn restore_persisted(&self) {
        let stored = match self.store.all() {
            Ok(stored) => stored,
            Err(err) => {
                tracing::warn!(error = ?err, "could not read stored torrents");
                return;
            }
        };

        let mut policies = self.seed_policies.lock().unwrap();
        let mut urls = self.source_urls.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        for torrent in stored {
            if let Some(url) = torrent.source_url {
                urls.insert(torrent.info_hash.clone(), url);
            }
            if torrent.seed_minutes.is_some() || torrent.seed_ratio.is_some() {
                // The stored `finished_at` is wall-clock; translate it back
                // into "how long ago" for the reaper's `Instant`-based math.
                let finished_at = torrent.finished_at.map(|at| {
                    let elapsed = now.saturating_sub(at).max(0) as u64;
                    Instant::now()
                        .checked_sub(Duration::from_secs(elapsed))
                        .unwrap_or_else(Instant::now)
                });
                policies.insert(
                    torrent.info_hash.clone(),
                    SeedPolicy {
                        seed_for: torrent.seed_minutes.map(|m| Duration::from_secs(m * 60)),
                        seed_ratio: torrent.seed_ratio,
                        finished_at,
                    },
                );
            }
        }
    }

    /// Add a torrent from a magnet URI and wait for its metadata to
    /// resolve (peer/DHT exchange -- there's nothing to list before this).
    /// `output_folder`, if given, saves this torrent under that directory
    /// instead of the session's default download directory.
    ///
    /// Every file in the torrent is selected for download by default (we
    /// never set `only_files`), and `librqbit` starts pulling pieces for
    /// all of them the moment this returns -- the whole torrent downloads
    /// to `output_folder` on this machine's disk in the background,
    /// independent of whether anything ever reads it back over HTTP.
    /// `stream()`/`download()` below just let you read an individual
    /// file's bytes concurrently with that download, sequentially from
    /// wherever you start reading.
    pub async fn add(&self, magnet: &str, output_folder: Option<String>) -> Result<AddedTorrent> {
        self.add_torrent(AddTorrent::from_url(magnet), output_folder, None, false, false)
            .await
    }

    /// [`add`] with the torrent's files placed in a subfolder named after
    /// the torrent (inside the session's download root). `name` is the
    /// release title as the user saw it, which is preferred over the
    /// magnet's own `dn` -- that is often a truncated or decorated form.
    pub async fn add_in_subfolder(
        &self,
        magnet: &str,
        name: Option<String>,
    ) -> Result<AddedTorrent> {
        self.add_torrent(AddTorrent::from_url(magnet), None, name, true, false)
            .await
    }

    /// Re-adds a torrent whose files are expected to already be on disk --
    /// the restore path, from a record in the store.
    ///
    /// `librqbit` refuses to add a torrent when a target file exists
    /// (`overwrite = false`), which is exactly the situation here: the
    /// files didn't go anywhere, the *client* forgot the torrent. So this
    /// allows overwrite and lets the session validate the existing data --
    /// it re-checks the pieces already present rather than re-downloading
    /// them.
    ///
    /// Reattaching with the torrent's *stored* output folder also lands on
    /// the same directory the download used, whether or not that was a
    /// per-torrent subfolder -- the stored path is already resolved, so no
    /// subfolder flag is needed here.
    pub async fn reattach(&self, magnet: &str, output_folder: Option<String>) -> Result<AddedTorrent> {
        self.add_torrent(
            AddTorrent::from_url(magnet),
            output_folder,
            None,
            false,
            true,
        )
        .await
    }

    /// Same as [`add`] for a .torrent document already fetched into
    /// memory -- for login-walled indexers whose download URLs need an
    /// authenticated session that librqbit's own URL fetch can't carry.
    ///
    /// `name` is the release name, used to name the per-torrent subfolder
    /// when `subfolder` is set: a .torrent's own name lives in its
    /// metadata, which isn't parsed yet at this point.
    pub async fn add_bytes(
        &self,
        bytes: bytes::Bytes,
        output_folder: Option<String>,
        name: Option<String>,
        subfolder: bool,
    ) -> Result<AddedTorrent> {
        self.add_torrent(
            AddTorrent::from_bytes(bytes),
            output_folder,
            name,
            subfolder,
            false,
        )
        .await
    }

    async fn add_torrent(
        &self,
        source: AddTorrent<'_>,
        output_folder: Option<String>,
        // Explicit release name, for a source whose name isn't in the
        // source itself (a .torrent document's lives in its metadata).
        name: Option<String>,
        // Put the torrent's files in their own subfolder named after the
        // torrent, instead of directly in `output_folder`.
        subfolder_by_name: bool,
        allow_overwrite: bool,
    ) -> Result<AddedTorrent> {
        // `sub_folder` and `output_folder` are mutually exclusive in
        // librqbit -- it bails with "you can't provide both".
        //
        // When a name is knowable up front (a magnet's `dn`, or the
        // caller's `name`), pass it as `sub_folder` so even a single-file
        // torrent gets its own folder. With `sub_folder: None` librqbit
        // still derives a name-based subfolder from the metadata itself --
        // but only for multi-file torrents, so a single-file one would
        // land flat.
        let sub_folder = if subfolder_by_name {
            name.or_else(|| add_torrent_name(&source))
                .map(|name| sanitize_subfolder(&name))
        } else {
            None
        };

        let opts = AddTorrentOptions {
            output_folder: if subfolder_by_name {
                None
            } else {
                output_folder
            },
            sub_folder,
            overwrite: allow_overwrite,
            ..Default::default()
        };

        // The timeout has to cover `add_torrent` itself, not just the wait
        // below: for a magnet, librqbit resolves the metadata from peers
        // *inside* this call (`resolve_magnet`) and blocks until some peer
        // answers or a tracker/DHT lookup gives up -- with no timeout of its
        // own. A magnet no one is seeding therefore hung the HTTP request
        // forever, so the browser sat on a spinner with no error, and the
        // `wait_until_initialized` timeout below was never even reached.
        let handle = tokio::time::timeout(METADATA_TIMEOUT, async {
            let response = self
                .session
                .add_torrent(source, Some(opts))
                .await
                .context("failed to add torrent")?;
            response
                .into_handle()
                .context("magnet was list-only; nothing to download")
        })
        .await
        .context("timed out resolving torrent metadata (no peers responded)")??;

        tokio::time::timeout(METADATA_TIMEOUT, handle.wait_until_initialized())
            .await
            .context("timed out resolving torrent metadata (no peers responded)")?
            .context("failed to resolve torrent metadata")?;

        let info_hash = handle.info_hash().as_string();
        let name = handle.name().unwrap_or_else(|| info_hash.clone());

        let files = handle
            .with_metadata(|m| {
                m.file_infos
                    .iter()
                    .enumerate()
                    .map(|(file_id, f)| TorrentFile {
                        file_id,
                        name: f.relative_filename.to_string_lossy().into_owned(),
                        len: f.len,
                        downloaded_bytes: None,
                    })
                    .collect::<Vec<_>>()
            })
            .context("torrent metadata unavailable after initialization")?;

        Ok(AddedTorrent {
            info_hash,
            name,
            files,
        })
    }

    /// Records a freshly added torrent (and its files) in SQLite. Called
    /// by the `/open` handler once the caller knows the magnet/URL it came
    /// from, which the engine itself doesn't.
    pub fn persist_added(
        &self,
        added: &AddedTorrent,
        source: Option<String>,
        source_url: Option<String>,
        output_folder: Option<String>,
    ) {
        let folder = output_folder
            .filter(|f| !f.is_empty())
            .or_else(|| {
                TorrentIdOrHash::parse(&added.info_hash)
                    .ok()
                    .and_then(|idx| self.session.get(idx))
                    .map(|handle| handle.output_folder().to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let stored = crate::torrent_store::StoredTorrent {
            info_hash: added.info_hash.clone(),
            name: added.name.clone(),
            source,
            source_url,
            output_folder: folder,
            total_bytes: added.files.iter().map(|f| f.len).sum(),
            added_at: crate::torrent_store::unix_now(),
            finished_at: None,
            seed_minutes: None,
            seed_ratio: None,
            active: true,
            files: added
                .files
                .iter()
                .map(|f| crate::torrent_store::StoredFile {
                    file_id: f.file_id as u64,
                    name: f.name.clone(),
                    len: f.len,
                })
                .collect(),
        };
        if let Err(err) = self.store.upsert(&stored) {
            // Persistence is a convenience, not a correctness requirement
            // for the running torrent -- log and carry on.
            tracing::warn!(error = ?err, "could not persist torrent");
        }
    }

    /// List the files of an already-added torrent, for the detail page --
    /// same shape `add` returns, plus each file's downloaded bytes so the
    /// page can show a per-file percentage.
    pub fn files(&self, info_hash: &str) -> Result<Vec<TorrentFile>> {
        let idx = TorrentIdOrHash::parse(info_hash).context("invalid info hash")?;
        let handle = self
            .session
            .get(idx)
            .with_context(|| format!("torrent {info_hash} is not active"))?;

        // `file_progress` is indexed the same as `file_infos`, so the two
        // are zipped into one row per file.
        handle
            .with_metadata(|m| {
                let progress = handle.stats().file_progress;
                m.file_infos
                    .iter()
                    .enumerate()
                    .map(|(file_id, f)| TorrentFile {
                        file_id,
                        name: f.relative_filename.to_string_lossy().into_owned(),
                        len: f.len,
                        downloaded_bytes: Some(progress.get(file_id).copied().unwrap_or(0).min(f.len)),
                    })
                    .collect::<Vec<_>>()
            })
            .context("torrent metadata unavailable")
    }

    /// Open a sequential, seekable byte stream for `file_id` inside an
    /// already-added torrent, identified by its info hash. Also returns the
    /// file's relative name and length, so callers can guess a
    /// `Content-Type` and set `Content-Length`/`Content-Range` without
    /// needing to name `librqbit`'s internal `FileStream` type.
    pub async fn stream(
        &self,
        info_hash: &str,
        file_id: usize,
    ) -> Result<(
        impl tokio::io::AsyncRead + tokio::io::AsyncSeek + Send + Unpin + use<>,
        String,
        u64,
    )> {
        let idx =
            librqbit::api::TorrentIdOrHash::parse(info_hash).context("invalid info hash")?;
        let handle = self
            .session
            .get(idx)
            .with_context(|| format!("torrent {info_hash} is not active"))?;

        let (file_name, file_len) = handle
            .with_metadata(|m| {
                m.file_infos
                    .get(file_id)
                    .map(|f| (f.relative_filename.to_string_lossy().into_owned(), f.len))
            })
            .context("torrent metadata unavailable")?
            .with_context(|| format!("no such file id {file_id}"))?;

        let stream = handle
            .stream(file_id)
            .await
            .context("failed to open file stream")?;

        Ok((stream, file_name, file_len))
    }

    /// Remove a torrent manually. `delete_files` controls whether the
    /// downloaded data on disk is deleted along with it -- the auto-reaper
    /// below always keeps the files, but a manual delete is often "I
    /// don't want this at all" and should be able to wipe it too.
    pub async fn delete(&self, info_hash: &str, delete_files: bool) -> Result<()> {
        let idx = TorrentIdOrHash::parse(info_hash).context("invalid info hash")?;
        self.session
            .delete(idx, delete_files)
            .await
            .with_context(|| format!("failed to remove torrent {info_hash}"))?;
        self.seed_policies.lock().unwrap().remove(info_hash);
        self.source_urls.lock().unwrap().remove(info_hash);

        // A manual delete that keeps the files keeps the record: the point
        // of storing this is being able to find what was downloaded. Only
        // a delete that wipes the files drops the record entirely.
        let result = if delete_files {
            self.store.delete(info_hash)
        } else {
            self.store.mark_inactive(info_hash)
        };
        if let Err(err) = result {
            tracing::warn!(error = ?err, "could not update stored torrent");
        }
        Ok(())
    }

    /// Record the indexer-side page this torrent came from, for the
    /// detail page's "view on `<indexer>`" link.
    pub fn set_source_url(&self, info_hash: &str, url: Option<String>) {
        {
            let mut urls = self.source_urls.lock().unwrap();
            match &url {
                Some(url) => {
                    urls.insert(info_hash.to_string(), url.clone());
                }
                None => {
                    urls.remove(info_hash);
                }
            }
        }
        // Keep the durable copy in step, so the "view on <indexer>" link
        // survives a restart too.
        if let Err(err) = self.store.set_source_url_stored(info_hash, url.as_deref()) {
            tracing::warn!(error = ?err, "could not persist source url");
        }
    }

    /// A single torrent's summary, for the per-torrent detail page.
    pub fn get(&self, info_hash: &str) -> Option<TorrentSummary> {
        self.list().into_iter().find(|t| t.info_hash == info_hash)
    }

    /// Stops a torrent transferring, keeping everything: the files, the
    /// seed policy, and its place in the list. It can be resumed from the
    /// same page.
    ///
    /// Both directions go through `librqbit`'s session-level `pause`/
    /// `unpause` (not the bare handle method) so the dependency also
    /// updates its own persistence metadata -- otherwise a paused torrent
    /// would silently restart transferring after a salo restart.
    pub async fn set_paused(&self, info_hash: &str, paused: bool) -> Result<()> {
        let idx = TorrentIdOrHash::parse(info_hash).context("invalid info hash")?;
        let handle = self
            .session
            .get(idx)
            .with_context(|| format!("torrent {info_hash} is not in the session"))?;
        if paused {
            self.session
                .pause(&handle)
                .await
                .context("failed to pause torrent")?;
        } else {
            self.session
                .unpause(&handle)
                .await
                .context("failed to resume torrent")?;
        }
        if let Err(err) = self.store.set_paused(info_hash, paused) {
            tracing::warn!(error = ?err, "could not persist paused state");
        }
        Ok(())
    }

    /// Set (or, with both `None`, clear) how this torrent should be
    /// auto-removed once it's done seeding -- after `minutes` of seeding,
    /// once its upload/download ratio reaches `ratio`, or whichever of
    /// the two (if both are set) happens first. Downloaded files are kept
    /// either way -- this only stops the torrent from continuing to
    /// upload forever.
    pub fn set_seed_limit(&self, info_hash: &str, minutes: Option<u64>, ratio: Option<f64>) {
        {
            let mut policies = self.seed_policies.lock().unwrap();
            if minutes.is_none() && ratio.is_none() {
                policies.remove(info_hash);
            } else {
                policies.insert(
                    info_hash.to_string(),
                    SeedPolicy {
                        seed_for: minutes.map(|m| Duration::from_secs(m * 60)),
                        seed_ratio: ratio,
                        finished_at: None,
                    },
                );
            }
        }
        if let Err(err) = self.store.set_seed_limit(info_hash, minutes, ratio) {
            tracing::warn!(error = ?err, "could not persist seed limit");
        }
    }

    /// The durable store, for pages that report or restore what was
    /// downloaded rather than what's currently in the client.
    pub fn store(&self) -> Arc<crate::torrent_store::TorrentStore> {
        Arc::clone(&self.store)
    }

    /// A snapshot of every currently managed torrent, for the
    /// torrent-management page.
    pub fn list(&self) -> Vec<TorrentSummary> {
        let policies = self.seed_policies.lock().unwrap();
        let source_urls = self.source_urls.lock().unwrap();
        self.session.with_torrents(|iter| {
            iter.map(|(_, handle)| {
                let info_hash = handle.info_hash().as_string();
                let stats = handle.stats();
                let policy = policies.get(&info_hash);
                TorrentSummary {
                    name: handle.name().unwrap_or_else(|| info_hash.clone()),
                    output_folder: handle.output_folder().to_string_lossy().into_owned(),
                    finished: stats.finished,
                    progress_bytes: stats.progress_bytes,
                    total_bytes: stats.total_bytes,
                    uploaded_bytes: stats.uploaded_bytes,
                    // `live` is absent for a stopped/errored torrent, so
                    // speeds read as 0 rather than the last known rate.
                    download_speed: stats
                        .live
                        .as_ref()
                        .map_or(0, |l| l.download_speed.as_bytes()),
                    upload_speed: stats.live.as_ref().map_or(0, |l| l.upload_speed.as_bytes()),
                    paused: matches!(stats.state, librqbit::TorrentStatsState::Paused),
                    seed_minutes: policy.and_then(|p| p.seed_for).map(|d| d.as_secs() / 60),
                    seed_ratio: policy.and_then(|p| p.seed_ratio),
                    seeded_for_minutes: policy
                        .and_then(|p| p.finished_at)
                        .map(|at| at.elapsed().as_secs() / 60),
                    source_url: source_urls.get(&info_hash).cloned(),
                    info_hash,
                }
            })
            .collect()
        })
    }

    /// Checks every torrent with a seed-time or seed-ratio limit against
    /// its current stats, and removes any that have hit either one. Runs
    /// on a fixed interval from a background task started in `new`.
    async fn reap_once(&self) {
        let now = Instant::now();

        // Torrents seen finished for the first time this pass. Held behind
        // a mutex because `with_torrents` takes an `Fn` closure, which
        // can't capture a local mutably; the durable writes happen below,
        // outside the scan.
        let newly_finished: Mutex<Vec<String>> = Mutex::new(Vec::new());

        let to_remove: Vec<(usize, String)> = self.session.with_torrents(|iter| {
            let mut policies = self.seed_policies.lock().unwrap();
            let mut to_remove = Vec::new();
            for (id, handle) in iter {
                let info_hash = handle.info_hash().as_string();
                let Some(policy) = policies.get_mut(&info_hash) else {
                    continue;
                };
                let stats = handle.stats();
                if !stats.finished {
                    continue;
                }
                let first_observation = policy.finished_at.is_none();
                let finished_at = *policy.finished_at.get_or_insert(now);
                if first_observation {
                    newly_finished
                        .lock()
                        .expect("reaper scratch poisoned")
                        .push(info_hash.clone());
                }

                let time_hit = policy
                    .seed_for
                    .is_some_and(|limit| now.duration_since(finished_at) >= limit);
                let ratio_hit = policy.seed_ratio.is_some_and(|limit| {
                    stats.total_bytes > 0
                        && (stats.uploaded_bytes as f64 / stats.total_bytes as f64) >= limit
                });

                if time_hit || ratio_hit {
                    to_remove.push((id, info_hash));
                }
            }
            to_remove
        });

        let now_unix = crate::torrent_store::unix_now();
        let newly_finished = newly_finished.into_inner().expect("reaper scratch poisoned");
        for info_hash in newly_finished {
            if let Err(err) = self.store.mark_finished(&info_hash, now_unix) {
                tracing::warn!(error = ?err, "could not persist finish time");
            }
        }

        for (id, info_hash) in to_remove {
            self.seed_policies.lock().unwrap().remove(&info_hash);
            match self.session.delete(TorrentIdOrHash::from(id), false).await {
                Ok(()) => {
                    // The files are kept, so the record stays -- just no
                    // longer active in the client.
                    if let Err(err) = self.store.mark_inactive(&info_hash) {
                        tracing::warn!(error = ?err, "could not update stored torrent");
                    }
                    tracing::info!(%info_hash, "removed torrent after its seed-time limit elapsed")
                }
                Err(err) => {
                    tracing::warn!(%info_hash, error = ?err, "failed to auto-remove torrent")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_magnet_name_is_decoded_from_dn() {
        let src = AddTorrent::from_url(
            "magnet:?xt=urn:btih:abc&dn=Django%20Unchained%202012&tr=udp%3A%2F%2Fx",
        );
        assert_eq!(
            add_torrent_name(&src).as_deref(),
            Some("Django Unchained 2012")
        );
    }

    #[test]
    fn a_magnet_without_dn_has_no_name() {
        let src = AddTorrent::from_url("magnet:?xt=urn:btih:abc");
        assert_eq!(add_torrent_name(&src), None);
        // And a .torrent document never carries one.
        let src = AddTorrent::from_bytes(bytes::Bytes::from_static(b"d4:infod"));
        assert_eq!(add_torrent_name(&src), None);
    }

    #[test]
    fn percent_decoding_handles_escapes_and_leaves_invalid_ones_alone() {
        assert_eq!(percent_decode("%5BAnoZu%5D%20One%20Piece").as_deref(), Some("[AnoZu] One Piece"));
        assert_eq!(percent_decode("plain+name").as_deref(), Some("plain name"));
        // A stray % that isn't an escape must survive rather than be eaten.
        assert_eq!(percent_decode("100%%").as_deref(), Some("100%%"));
        assert_eq!(percent_decode("t%C3%A9l%C3%A9charger").as_deref(), Some("télécharger"));
    }

    #[test]
    fn a_subfolder_name_can_never_escape_the_download_root() {
        // The whole point of the sanitizer: librqbit does NOT validate a
        // caller-supplied sub_folder, so traversal must be neutralised here.
        for hostile in [
            "../../etc/passwd",
            "..\\..\\windows",
            "/absolute/path",
            "a/../../../b",
            "..",
            ".",
            "  ..  ",
        ] {
            let safe = sanitize_subfolder(hostile);
            assert!(
                !safe.contains('/') && !safe.contains('\\'),
                "{hostile:?} -> {safe:?} still has a separator"
            );
            assert_ne!(safe, "..", "{hostile:?} stayed as a parent ref");
            assert_ne!(safe, ".", "{hostile:?} stayed as a current-dir ref");
            assert!(!safe.is_empty(), "{hostile:?} became empty");
        }
    }

    #[test]
    fn an_ordinary_release_name_survives_intact() {
        // Sanitizing must not mangle the common case.
        assert_eq!(
            sanitize_subfolder("Django Unchained 2012 1080p BluRay"),
            "Django Unchained 2012 1080p BluRay"
        );
        assert_eq!(sanitize_subfolder("[AnoZu] One Piece"), "[AnoZu] One Piece");
    }

    #[test]
    fn a_very_long_name_is_truncated_for_the_filesystem() {
        let long = "x".repeat(400);
        assert_eq!(sanitize_subfolder(&long).chars().count(), 150);
    }
}
