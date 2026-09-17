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
    pub seed_minutes: Option<u64>,
    pub seed_ratio: Option<f64>,
    pub seeded_for_minutes: Option<u64>,
    pub source_url: Option<String>,
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
        self.add_torrent(AddTorrent::from_url(magnet), output_folder, false)
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
    pub async fn reattach(&self, magnet: &str, output_folder: Option<String>) -> Result<AddedTorrent> {
        self.add_torrent(AddTorrent::from_url(magnet), output_folder, true)
            .await
    }

    /// Same as [`add`] for a .torrent document already fetched into
    /// memory -- for login-walled indexers whose download URLs need an
    /// authenticated session that librqbit's own URL fetch can't carry.
    pub async fn add_bytes(
        &self,
        bytes: bytes::Bytes,
        output_folder: Option<String>,
    ) -> Result<AddedTorrent> {
        self.add_torrent(AddTorrent::from_bytes(bytes), output_folder, false)
            .await
    }

    async fn add_torrent(
        &self,
        source: AddTorrent<'_>,
        output_folder: Option<String>,
        allow_overwrite: bool,
    ) -> Result<AddedTorrent> {
        let opts = AddTorrentOptions {
            output_folder,
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
