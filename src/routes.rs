use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use anyhow::Context;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use tokio::io::AsyncSeekExt;
use tokio_util::io::ReaderStream;

use crate::config_store::ConfigStore;
use crate::error::AppError;
use crate::indexers::{self, Release};
use crate::templates::{
    ColumnSort, FileEntry, HtmlTemplate, IndexTemplate, IndexerSettings, OpenTemplate,
    ResultsTemplate, SettingEntry, SettingsTemplate, TorrentDetailTemplate, TorrentRow,
    TorrentsTemplate,
};
use crate::torrent::TorrentEngine;

const PAGE_SIZE: usize = 20;
const SORT_COLUMNS: &[(&str, &str)] = &[
    ("title", "Title"),
    ("indexer", "Indexer"),
    ("size", "Size"),
    ("seeders", "Seeders"),
    ("leechers", "Leechers"),
];

pub struct AppState {
    pub http: reqwest::Client,
    pub torrents: Arc<TorrentEngine>,
    pub config: Arc<ConfigStore>,
}

pub async fn index() -> impl IntoResponse {
    HtmlTemplate(IndexTemplate {
        trackers: indexers::names(),
        query: String::new(),
        selected_tracker: "all".to_string(),
        results_html: String::new(),
    })
}

/// HTMX itself, compiled into the binary at build time (`include_str!`) and
/// served from memory -- the UI never reaches out to a CDN, keeping the app
/// a single dependency-free binary that also works fully offline/air-gapped.
const HTMX_JS: &str = include_str!("../static/htmx.min.js");

pub async fn htmx_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        HTMX_JS,
    )
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: String,
    /// A specific indexer name, or absent/"all" to search everything.
    #[serde(default)]
    tracker: Option<String>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    page: Option<usize>,
}

/// Runs the embedded indexer(s) (see `src/indexers/`) and returns just the
/// results `<table>` fragment for HTMX to swap into the page -- no full
/// page reload, and no call to any external indexer service.
///
/// A plain GET with everything in the query string, so sorting, paging,
/// and the tracker filter are just links the results fragment renders
/// (built below), with no client-side state to keep in sync. Because the
/// search form and those links all set `hx-push-url`, the browser's
/// address bar ends up on this same `/search?...` URL -- so a reload, a
/// bookmark, or a pasted link hits this route directly, with no
/// `HX-Request` header (htmx adds that only to its own fetches). In that
/// case we render the *full* page (styles, search form, and all) with
/// this fragment already embedded, instead of the bare fragment HTMX
/// would normally swap in.
pub async fn search(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let is_htmx = headers.contains_key("hx-request");
    let tracker = q.tracker.filter(|t| !t.is_empty() && t != "all");
    let sort = q.sort.as_deref().unwrap_or("seeders");
    let dir = q
        .dir
        .as_deref()
        .unwrap_or(if matches!(sort, "title" | "indexer") {
            "asc"
        } else {
            "desc"
        });

    let mut releases = indexers::search_all(&state.http, &state.config, &q.q, tracker.as_deref()).await;
    sort_releases(&mut releases, sort, dir);

    let total_results = releases.len();
    let total_pages = total_results.div_ceil(PAGE_SIZE).max(1);
    let page = q.page.unwrap_or(1).clamp(1, total_pages);
    let releases = releases
        .into_iter()
        .skip((page - 1) * PAGE_SIZE)
        .take(PAGE_SIZE)
        .collect();

    let tracker_value = tracker.as_deref().unwrap_or("all");
    let href = |sort: &str, dir: &str, page: usize| {
        format!(
            "/search?q={}&tracker={}&sort={}&dir={}&page={page}",
            encode(&q.q),
            encode(tracker_value),
            encode(sort),
            encode(dir),
        )
    };

    let columns = SORT_COLUMNS
        .iter()
        .map(|&(key, label)| {
            let active = key == sort;
            let next_dir = match (active, dir) {
                (true, "desc") => "asc",
                (true, _) => "desc",
                (false, _) if matches!(key, "title" | "indexer") => "asc",
                (false, _) => "desc",
            };
            ColumnSort {
                label,
                href: href(key, next_dir, 1),
                arrow: if !active {
                    ""
                } else if dir == "desc" {
                    "\u{25BC}"
                } else {
                    "\u{25B2}"
                },
            }
        })
        .collect();

    let prev_href = href(sort, dir, page.saturating_sub(1));
    let next_href = href(sort, dir, page + 1);

    let results = ResultsTemplate {
        query: q.q,
        releases,
        columns,
        total_results,
        page,
        total_pages,
        has_prev: page > 1,
        prev_href,
        has_next: page < total_pages,
        next_href,
    };

    if is_htmx {
        return Ok(HtmlTemplate(results).into_response());
    }

    // Direct navigation: render the fragment first, then embed it in the
    // full page shell so styles/nav/search-form all come along with it.
    let query = results.query.clone();
    let results_html = askama::Template::render(&results)
        .map_err(|e| anyhow::anyhow!("template render error: {e}"))?;
    Ok(HtmlTemplate(IndexTemplate {
        trackers: indexers::names(),
        selected_tracker: tracker_value.to_string(),
        query,
        results_html,
    })
    .into_response())
}

fn encode(s: &str) -> impl std::fmt::Display + '_ {
    utf8_percent_encode(s, NON_ALPHANUMERIC)
}

fn sort_releases(releases: &mut [Release], sort: &str, dir: &str) {
    match sort {
        "title" => releases.sort_by_key(|r| r.title.to_lowercase()),
        "indexer" => releases.sort_by_key(|r| r.indexer),
        "leechers" => releases.sort_by_key(|r| r.leechers),
        "size" => releases.sort_by(|a, b| {
            indexers::size_bytes(&a.size)
                .partial_cmp(&indexers::size_bytes(&b.size))
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        _ => releases.sort_by_key(|r| r.seeders),
    }
    if dir == "desc" {
        releases.reverse();
    }
}

/// Browsers submit a blank `<input type="number">` as an empty string,
/// not by omitting the field -- and `Option<u64>`/`Option<f64>` only
/// deserialize a *missing* key as `None`, so an untouched optional number
/// field made every submission fail with 422 "cannot parse integer from
/// empty string" (this is what silently broke every "Open" click before
/// this fixed it). `0` is also treated as "not set", since a `0`-value
/// limit is meaningless here and this is the default shown in the form
/// for "no limit".
fn zero_or_blank_as_none<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: std::str::FromStr + Default + PartialEq,
    T::Err: std::fmt::Display,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    match raw.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(s) => {
            let value: T = s.parse().map_err(serde::de::Error::custom)?;
            Ok((value != T::default()).then_some(value))
        }
    }
}

#[derive(Deserialize)]
pub struct OpenForm {
    magnet: String,
    /// The release's page on the indexer's site, if it had one -- kept
    /// alongside the torrent so its detail page can link back to it.
    #[serde(default)]
    source_url: Option<String>,
    /// Save this torrent under a specific directory instead of the
    /// server's default download directory. Blank means use the default.
    #[serde(default)]
    directory: Option<String>,
    /// Auto-remove this torrent (keeping the downloaded files) after it's
    /// finished and has been seeding for this many minutes. `0`/blank
    /// means seed indefinitely.
    #[serde(default, deserialize_with = "zero_or_blank_as_none")]
    seed_minutes: Option<u64>,
    /// Auto-remove once upload/download ratio reaches this (e.g. `2.0` to
    /// seed back twice what was downloaded) -- useful for keeping a good
    /// ratio on trackers that enforce one, without babysitting it
    /// yourself. `0`/blank means no ratio-based limit.
    #[serde(default, deserialize_with = "zero_or_blank_as_none")]
    seed_ratio: Option<f64>,
}

/// Adds the torrent for the chosen magnet and waits for its metadata, then
/// lists every file in it -- not just one auto-picked "video" file, since
/// plenty of torrents (datasets, disc images, archives) aren't video at
/// all. `librqbit` starts downloading every one of these files to local
/// disk in the background the moment the torrent is added (see
/// `TorrentEngine::add`); this fragment just gives each file a way to
/// read those bytes back over HTTP while that download is still going.
pub async fn open(
    State(state): State<Arc<AppState>>,
    Form(form): Form<OpenForm>,
) -> Result<impl IntoResponse, AppError> {
    let directory = form.directory.filter(|d| !d.is_empty());
    let added = state.torrents.add(&form.magnet, directory).await?;
    if form.source_url.is_some() {
        state.torrents.set_source_url(&added.info_hash, form.source_url);
    }
    if form.seed_minutes.is_some() || form.seed_ratio.is_some() {
        state
            .torrents
            .set_seed_limit(&added.info_hash, form.seed_minutes, form.seed_ratio);
    }

    let files = to_file_entries(&added.info_hash, added.files);

    Ok(HtmlTemplate(OpenTemplate {
        info_hash: added.info_hash,
        name: added.name,
        files,
    }))
}

fn to_file_entries(info_hash: &str, files: Vec<crate::torrent::TorrentFile>) -> Vec<FileEntry> {
    files
        .into_iter()
        .map(|f| FileEntry {
            is_video: crate::torrent::is_video_file(&f.name),
            is_audio: crate::torrent::is_audio_file(&f.name),
            size: indexers::format_size(f.len),
            stream_href: format!("/stream/{info_hash}/{}", f.file_id),
            download_href: format!("/download/{info_hash}/{}", f.file_id),
            name: f.name,
        })
        .collect()
}

/// The per-torrent detail page: name, a link back to the release's page
/// on whichever indexer it came from (if known), how long it's been
/// seeding once finished, and the same Watch/Listen/Save file list shown
/// right after adding it -- reachable again later without re-adding
/// anything, e.g. from a file's name on the results/open fragment or from
/// the torrent-management list.
pub async fn torrent_detail(
    State(state): State<Arc<AppState>>,
    Path(info_hash): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let summary = state
        .torrents
        .get(&info_hash)
        .context("torrent not found (it may have been removed)")?;
    let files = to_file_entries(&info_hash, state.torrents.files(&info_hash)?);

    let progress_percent = summary
        .progress_bytes
        .checked_mul(100)
        .and_then(|v| v.checked_div(summary.total_bytes))
        .unwrap_or(0) as u32;

    Ok(HtmlTemplate(TorrentDetailTemplate {
        info_hash,
        name: summary.name,
        source_url: summary.source_url,
        finished: summary.finished,
        progress_percent,
        seeded_for_minutes: summary.seeded_for_minutes,
        files,
    }))
}

/// Pipes decoded torrent pieces straight into the HTTP response body as
/// they arrive from the swarm, in sequential order from wherever the
/// request starts reading -- shared by `/stream` (inline `<video>`
/// playback) and `/download` (save-to-disk in the browser); the only
/// difference is whether the response asks the browser to display or save
/// it.
///
/// Honors a single-range `Range` request (what browsers send for seeking,
/// and what a download manager sends to resume a partial download):
/// `FileStream` implements `AsyncSeek`, which internally reprioritizes
/// pieces around the new position, so a seek is still "sequential
/// download" from that offset -- this keeps working concurrently with
/// `librqbit` downloading the rest of the torrent to disk in the
/// background.
async fn serve_file(
    state: &AppState,
    info_hash: &str,
    file_id: usize,
    headers: &HeaderMap,
    attachment: bool,
) -> Result<Response, AppError> {
    let (mut file_stream, file_name, total_len) = state.torrents.stream(info_hash, file_id).await?;
    let content_type = mime_guess::from_path(&file_name)
        .first_or_octet_stream()
        .to_string();
    let bare_name = file_name.rsplit('/').next().unwrap_or(&file_name);
    let disposition = if attachment {
        format!(
            "attachment; filename=\"{}\"",
            bare_name.replace('"', "'")
        )
    } else {
        "inline".to_string()
    };

    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| parse_range(v, total_len));

    let (status, start, len) = match range {
        Some((start, end)) => (StatusCode::PARTIAL_CONTENT, start, end - start + 1),
        None => (StatusCode::OK, 0, total_len),
    };

    if start > 0 {
        file_stream
            .seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(anyhow::Error::from)?;
    }

    let limited = tokio::io::AsyncReadExt::take(file_stream, len);
    let body = axum::body::Body::from_stream(ReaderStream::new(limited));

    let mut response = (
        status,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_LENGTH, len.to_string()),
            (header::ACCEPT_RANGES, "bytes".to_string()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        body,
    )
        .into_response();

    if status == StatusCode::PARTIAL_CONTENT {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{total_len}", start + len - 1)
                .parse()
                .expect("valid header value"),
        );
    }

    Ok(response)
}

/// Inline playback: no `Content-Disposition`, so the browser renders it
/// (used by the `<video>` tag).
pub async fn stream(
    State(state): State<Arc<AppState>>,
    Path((info_hash, file_id)): Path<(String, usize)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    serve_file(&state, &info_hash, file_id, &headers, false).await
}

/// Save-to-disk: `Content-Disposition: attachment` tells the browser to
/// download it through its normal download manager (which also means
/// browser-side resume via `Range` works) rather than try to render it.
pub async fn download(
    State(state): State<Arc<AppState>>,
    Path((info_hash, file_id)): Path<(String, usize)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    serve_file(&state, &info_hash, file_id, &headers, true).await
}

/// Parses a `Range: bytes=start-end` / `bytes=start-` header. Only the
/// single-range form browsers actually send for video is supported.
fn parse_range(header: &str, total_len: u64) -> Option<(u64, u64)> {
    let spec = header.strip_prefix("bytes=")?;
    let (start, end) = spec.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    let end: u64 = if end.is_empty() {
        total_len.checked_sub(1)?
    } else {
        end.parse().ok()?
    };
    if start > end || end >= total_len {
        return None;
    }
    Some((start, end))
}

/// Per-indexer settings (API keys, tokens, whatever a given indexer module
/// declares it needs -- see `src/config_store.rs`). Nothing in
/// `src/indexers/` currently reads any of this; it's here so a future
/// indexer that needs configuration has somewhere to keep it without
/// wiring up a new mechanism.
pub async fn settings(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let mut indexers = Vec::new();
    for name in indexers::names() {
        let entries = state
            .config
            .get_all(name)?
            .into_iter()
            .map(|(key, value)| SettingEntry { key, value })
            .collect();
        indexers.push(IndexerSettings { name, entries });
    }
    Ok(HtmlTemplate(SettingsTemplate { indexers }))
}

#[derive(Deserialize)]
pub struct SaveSettingForm {
    indexer: String,
    key: String,
    value: String,
}

pub async fn save_setting(
    State(state): State<Arc<AppState>>,
    Form(form): Form<SaveSettingForm>,
) -> Result<impl IntoResponse, AppError> {
    state.config.set(&form.indexer, &form.key, &form.value)?;
    Ok(Redirect::to("/settings"))
}

#[derive(Deserialize)]
pub struct DeleteSettingForm {
    indexer: String,
    key: String,
}

pub async fn delete_setting(
    State(state): State<Arc<AppState>>,
    Form(form): Form<DeleteSettingForm>,
) -> Result<impl IntoResponse, AppError> {
    state.config.delete(&form.indexer, &form.key)?;
    Ok(Redirect::to("/settings"))
}

/// The torrent-management page: every currently added torrent, its
/// progress, where it's saving to, and its seed-time limit (if any) --
/// with actions to change that limit or remove the torrent outright.
pub async fn torrents(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let torrents = state
        .torrents
        .list()
        .into_iter()
        .map(|t| TorrentRow {
            progress_percent: t
                .progress_bytes
                .checked_mul(100)
                .and_then(|v| v.checked_div(t.total_bytes))
                .unwrap_or(0) as u32,
            total_size: indexers::format_size(t.total_bytes),
            uploaded: indexers::format_size(t.uploaded_bytes),
            // `0` reads back as "indefinitely", matching the form's own
            // convention (see `zero_or_blank_as_none`) instead of a blank
            // field that looks unset by accident.
            seed_minutes: t.seed_minutes.unwrap_or(0).to_string(),
            seed_ratio: t.seed_ratio.unwrap_or(0.0).to_string(),
            seeded_for_minutes: t.seeded_for_minutes,
            info_hash: t.info_hash,
            name: t.name,
            output_folder: t.output_folder,
            finished: t.finished,
        })
        .collect();

    HtmlTemplate(TorrentsTemplate { torrents })
}

#[derive(Deserialize)]
pub struct DeleteTorrentForm {
    info_hash: String,
    #[serde(default)]
    delete_files: Option<String>,
}

pub async fn delete_torrent(
    State(state): State<Arc<AppState>>,
    Form(form): Form<DeleteTorrentForm>,
) -> Result<impl IntoResponse, AppError> {
    state
        .torrents
        .delete(&form.info_hash, form.delete_files.is_some())
        .await?;
    Ok(Redirect::to("/torrents"))
}

#[derive(Deserialize)]
pub struct SeedLimitForm {
    info_hash: String,
    /// Both `0`/blank clears the limit (seed indefinitely again).
    #[serde(default, deserialize_with = "zero_or_blank_as_none")]
    seed_minutes: Option<u64>,
    #[serde(default, deserialize_with = "zero_or_blank_as_none")]
    seed_ratio: Option<f64>,
}

pub async fn set_seed_limit(
    State(state): State<Arc<AppState>>,
    Form(form): Form<SeedLimitForm>,
) -> impl IntoResponse {
    state
        .torrents
        .set_seed_limit(&form.info_hash, form.seed_minutes, form.seed_ratio);
    Redirect::to("/torrents")
}
