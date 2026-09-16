use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Form;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use tokio::io::AsyncSeekExt;
use tokio_util::io::ReaderStream;

use crate::error::AppError;
use crate::indexers::{self, Release};
use crate::templates::{ColumnSort, HtmlTemplate, IndexTemplate, PlayerTemplate, ResultsTemplate};
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
}

pub async fn index() -> impl IntoResponse {
    HtmlTemplate(IndexTemplate {
        trackers: indexers::names(),
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
/// (built below), with no client-side state to keep in sync.
pub async fn search(
    State(state): State<Arc<AppState>>,
    Query(q): Query<SearchQuery>,
) -> Result<impl IntoResponse, AppError> {
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

    let mut releases = indexers::search_all(&state.http, &q.q, tracker.as_deref()).await;
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

    Ok(HtmlTemplate(ResultsTemplate {
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
    }))
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

#[derive(Deserialize)]
pub struct WatchForm {
    magnet: String,
}

/// Adds the torrent for the chosen magnet, waits for its metadata, picks a
/// file to play, and returns the `<video>` fragment pointed at the stream
/// route. The actual bytes only start flowing once the browser requests
/// `/stream/...`.
pub async fn watch(
    State(state): State<Arc<AppState>>,
    Form(form): Form<WatchForm>,
) -> Result<impl IntoResponse, AppError> {
    let selected = state.torrents.add_magnet(&form.magnet).await?;
    Ok(HtmlTemplate(PlayerTemplate {
        info_hash: selected.info_hash,
        file_id: selected.file_id,
        file_name: selected.file_name,
        file_len: selected.file_len,
    }))
}

/// Pipes decoded torrent pieces straight into the HTTP response body as
/// they arrive from the swarm, in sequential (playback) order, so the
/// `<video>` tag can start playing before the download finishes.
///
/// Honors a single-range `Range` request (what browsers send for seeking
/// and for the initial probe some players make): `FileStream` implements
/// `AsyncSeek`, which internally reprioritizes pieces around the new
/// position, so a seek is still "sequential download" from that offset.
pub async fn stream(
    State(state): State<Arc<AppState>>,
    Path((info_hash, file_id)): Path<(String, usize)>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let (mut file_stream, file_name, total_len) =
        state.torrents.stream(&info_hash, file_id).await?;
    let content_type = mime_guess::from_path(&file_name)
        .first_or_octet_stream()
        .to_string();

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
