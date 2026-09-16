use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use librqbit::{AddTorrent, Session};

const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "avi", "webm", "mov", "m4v"];
const METADATA_TIMEOUT: Duration = Duration::from_secs(60);

/// Thin wrapper around a single embedded `librqbit` session: one process,
/// one download directory, no external torrent daemon.
pub struct TorrentEngine {
    session: Arc<Session>,
}

/// The file inside a torrent that we picked to stream, plus enough
/// metadata for the player fragment and the `Content-*` response headers.
pub struct SelectedFile {
    pub info_hash: String,
    pub file_id: usize,
    pub file_name: String,
    pub file_len: u64,
}

impl TorrentEngine {
    pub async fn new(download_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&download_dir).context("failed to create download directory")?;
        let session = Session::new(download_dir)
            .await
            .context("failed to start torrent session")?;
        Ok(Self { session })
    }

    /// Add a torrent from a magnet URI, wait for its metadata to resolve
    /// (peer/DHT exchange -- there are no files to pick from before this),
    /// and select the largest video file to stream.
    pub async fn add_magnet(&self, magnet: &str) -> Result<SelectedFile> {
        let response = self
            .session
            .add_torrent(AddTorrent::from_url(magnet), None)
            .await
            .context("failed to add magnet link")?;

        let handle = response
            .into_handle()
            .context("magnet was list-only; nothing to download")?;

        tokio::time::timeout(METADATA_TIMEOUT, handle.wait_until_initialized())
            .await
            .context("timed out resolving torrent metadata (no peers responded)")?
            .context("failed to resolve torrent metadata")?;

        let info_hash = handle.info_hash().as_string();
        let (file_id, file_name, file_len) = handle
            .with_metadata(|m| pick_file(&m.file_infos))
            .context("torrent metadata unavailable after initialization")??;

        Ok(SelectedFile {
            info_hash,
            file_id,
            file_name,
            file_len,
        })
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
}

fn pick_file(files: &librqbit::FileInfos) -> Result<(usize, String, u64)> {
    if files.is_empty() {
        anyhow::bail!("torrent has no files");
    }

    let is_video = |name: &str| {
        VIDEO_EXTENSIONS
            .iter()
            .any(|ext| name.to_lowercase().ends_with(ext))
    };

    let pick = files
        .iter()
        .enumerate()
        .filter(|(_, f)| is_video(&f.relative_filename.to_string_lossy()))
        .max_by_key(|(_, f)| f.len)
        .or_else(|| files.iter().enumerate().max_by_key(|(_, f)| f.len))
        .context("could not select a file from torrent metadata")?;

    let (id, info) = pick;
    Ok((id, info.relative_filename.to_string_lossy().into_owned(), info.len))
}
