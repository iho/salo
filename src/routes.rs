use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
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
use crate::search_progress;
use crate::tmdb;
use crate::templates::{
    ColumnSort, DeletedRowTemplate, EnvSetting, FileEntry, HtmlTemplate, IndexTemplate,
    IndexerSettings, OpenTemplate, ResultsTemplate, SaveStatusTemplate, SearchProgressTemplate,
    SettingEntry, SettingFieldView, SettingsTemplate, StoredFileView, StoredTemplate,
    StoredTorrentView, TorrentDetailTemplate, TorrentRow, TorrentsTemplate, TrackerProgressView,
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
        selected_trackers: Vec::new(),
        results_html: String::new(),
    })
}

/// HTMX itself, compiled into the binary at build time (`include_str!`) and
/// served from memory -- the UI never reaches out to a CDN, keeping the app
/// a single dependency-free binary that also works fully offline/air-gapped.
const HTMX_JS: &str = include_str!("../static/htmx.min.js");

/// Theme tokens (`--bg`, `--text`, `--border`, ...) and the toggle button's
/// styles, also vendored: light/dark differ only in the values under
/// `:root` and `html[data-theme="dark"]`, so no template rule knows which
/// theme is active.
const THEME_CSS: &str = include_str!("../static/theme.css");

/// Reads the saved theme (or the OS preference) and sets `data-theme`
/// before first paint, plus wires up the toggle button. Loaded
/// synchronously in every page's `<head>`, so a dark-theme user never sees
/// a white flash while the page loads.
const THEME_JS: &str = include_str!("../static/theme.js");

/// Fills the `#open-dialog` modal from a result row's hidden inputs and
/// closes it once `/open` succeeds -- see `static/open-dialog.js`.
const OPEN_DIALOG_JS: &str = include_str!("../static/open-dialog.js");

/// Starts and polls a search job for the per-tracker progress panel --
/// see `static/search-progress.js`.
const SEARCH_PROGRESS_JS: &str = include_str!("../static/search-progress.js");

/// Polls `/torrents/stats` and updates the torrents table's live figures
/// in place (see `static/torrent-stats.js`).
const TORRENT_STATS_JS: &str = include_str!("../static/torrent-stats.js");

/// Project logo/favicon, original artwork -- vendored the same way as
/// the JS/CSS above, no external image host.
const LOGO_SVG: &str = include_str!("../static/logo.svg");

pub async fn htmx_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        HTMX_JS,
    )
}

pub async fn theme_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        THEME_CSS,
    )
}

pub async fn theme_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        THEME_JS,
    )
}

pub async fn open_dialog_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        OPEN_DIALOG_JS,
    )
}

pub async fn search_progress_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        SEARCH_PROGRESS_JS,
    )
}

pub async fn logo_svg() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        LOGO_SVG,
    )
}

/// Live per-torrent transfer rates and ratio, as JSON, for the torrents
/// page's in-place refresh.
///
/// A single request covers every row, so the poll cost is constant
/// regardless of how many torrents are running -- and the page never
/// re-renders the seed-limit forms, so a field being typed into is safe.
pub async fn torrent_stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let rows: Vec<serde_json::Value> = state
        .torrents
        .list()
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "info_hash": t.info_hash,
                "progress_percent": t.progress_percent(),
                "download_speed": indexers::format_speed(t.download_speed),
                "upload_speed": indexers::format_speed(t.upload_speed),
                "ratio": t.ratio().map(|r| format!("{r:.2}")),
                "uploaded": indexers::format_size(t.uploaded_bytes),
                "finished": t.finished,
            })
        })
        .collect();
    axum::Json(rows)
}

/// The small poller that keeps the torrents page's rates current.
pub async fn torrent_stats_js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        TORRENT_STATS_JS,
    )
}

/// A search request's parameters.
///
/// Parsed by hand rather than through `axum::extract::Query`: that uses
/// `serde_urlencoded`, which cannot collect a repeated key into a `Vec`
/// (it fails with "invalid type: string \"knaben\", expected a sequence"),
/// and the tracker multiselect depends on repeated `trackers=` params.
#[derive(Default)]
pub struct SearchQuery {
    pub q: String,
    /// The trackers to search. Repeated (`?trackers=a&trackers=b`), so the
    /// form's checkbox list can select any subset. Empty means every
    /// tracker.
    pub trackers: Vec<String>,
    /// The old single-tracker parameter, still accepted so existing
    /// bookmarks, links, and the `?tracker=` URLs this app used to emit
    /// keep working.
    pub tracker: Option<String>,
    pub sort: Option<String>,
    pub dir: Option<String>,
    pub page: Option<usize>,
    /// When the results came from a finished progress job, this is that
    /// job's id: its collected releases are reused instead of searching
    /// every tracker again for a mere sort or page change.
    pub job: Option<String>,
}

impl SearchQuery {
    /// Parses a raw query string (`a=1&b=2&b=3`), percent-decoding both
    /// sides. Repeated keys are collected in order.
    pub fn parse(raw: Option<&str>) -> Self {
        let mut query = Self::default();
        for (key, value) in raw.map(pairs_of).unwrap_or_default() {
            match key.as_str() {
                "q" => query.q = value,
                "trackers" => query.trackers.push(value),
                "tracker" => query.tracker = Some(value),
                "sort" => query.sort = Some(value),
                "dir" => query.dir = Some(value),
                "page" => query.page = value.parse().ok(),
                "job" => query.job = Some(value),
                _ => {}
            }
        }
        query
    }

    /// The effective selection: `trackers` if present, else the legacy
    /// single `tracker`, else nothing (meaning "all"). `all` and blanks
    /// are dropped, so `?tracker=all` and `?trackers=` both mean "all".
    pub fn selected_trackers(&self) -> Vec<String> {
        let mut chosen: Vec<String> = if self.trackers.is_empty() {
            self.tracker.iter().cloned().collect()
        } else {
            self.trackers.clone()
        };
        chosen.retain(|t| !t.trim().is_empty() && t != "all");
        chosen.sort();
        chosen.dedup();
        chosen
    }
}

/// Percent-decoded `key=value` pairs from a query string. A key with no
/// `=` is treated as empty-valued rather than dropped, matching how
/// browsers submit a checked-but-blank field.
fn pairs_of(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(&s.replace('+', " "))
        .decode_utf8_lossy()
        .into_owned()
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
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let q = SearchQuery::parse(raw.as_deref());
    let is_htmx = headers.contains_key("hx-request");
    let trackers = q.selected_trackers();
    let sort = q.sort.as_deref().unwrap_or("seeders");
    let dir = q
        .dir
        .as_deref()
        .unwrap_or(if matches!(sort, "title" | "indexer") {
            "asc"
        } else {
            "desc"
        });

    // Reuse a finished progress job's releases when the browser hands us
    // its id: re-running every tracker just to re-sort or page through
    // results already in memory would be wasteful and slow.
    let mut releases = match q.job.as_deref().and_then(search_progress::get) {
        Some(progress) if progress.finished => progress.releases,
        _ => indexers::search_all(&state.http, &state.config, &q.q, &trackers).await,
    };
    sort_releases(&mut releases, sort, dir);

    let total_results = releases.len();
    let total_pages = total_results.div_ceil(PAGE_SIZE).max(1);
    let page = q.page.unwrap_or(1).clamp(1, total_pages);
    let releases = releases
        .into_iter()
        .skip((page - 1) * PAGE_SIZE)
        .take(PAGE_SIZE)
        .collect();

    // Every selection is carried as repeated `trackers=` params, so a
    // sort/paging link preserves exactly what was chosen. The job id rides
    // along too, so paging doesn't re-search every tracker.
    let trackers_query = trackers
        .iter()
        .map(|t| format!("&trackers={}", encode(t)))
        .collect::<String>();
    let job_query = q
        .job
        .as_deref()
        .map(|job| format!("&job={}", encode(job)))
        .unwrap_or_default();
    let href = |sort: &str, dir: &str, page: usize| {
        format!(
            "/search?q={}&sort={}&dir={}&page={page}{trackers_query}{job_query}",
            encode(&q.q),
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
        selected_trackers: trackers,
        query,
        results_html,
    })
    .into_response())
}

/// Starts a background search job for the browser's progress panel and
/// returns the little script that kicks off polling -- see
/// `static/search-progress.js`. The job runs the same per-tracker search
/// `search_all` does, but records each tracker's state and timing as it
/// goes, so the page can show a bar and response times instead of sitting
/// blank until the slowest tracker answers.
pub async fn search_start(
    State(state): State<Arc<AppState>>,
    RawQuery(raw): RawQuery,
) -> impl IntoResponse {
    let q = SearchQuery::parse(raw.as_deref());
    let query = q.q.trim().to_string();
    let selected = q.selected_trackers();

    let names: Vec<&'static str> = indexers::all_names()
        .into_iter()
        .filter(|name| selected.is_empty() || selected.iter().any(|s| s == name))
        .collect();

    let (id, progress) = search_progress::create(query.clone(), names.clone());
    let targets = names;
    let client = state.http.clone();
    let config = Arc::clone(&state.config);
    tokio::spawn(async move {
        search_progress::run(progress, client, config, query, targets).await;
    });

    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        format!("window.__saloStartSearch({id:?});"),
    )
}

/// Polls one search job: the progress panel while it runs, and a redirect
/// to the real results (from the job's own collected releases, so nothing
/// is searched twice) once it finishes.
pub async fn search_progress(Path(id): Path<String>) -> Result<Response, AppError> {
    let Some(progress) = search_progress::get(&id) else {
        // The job aged out; stop polling rather than spinning forever.
        return Ok((
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "",
        )
            .into_response());
    };

    if progress.finished {
        // Results are already collected -- hand the browser the URL for
        // them and let it navigate (hx-push-url keeps the address bar in
        // sync). `id` lets /search reuse this job's releases.
        return Ok((
            [(
                header::CONTENT_TYPE,
                "text/javascript; charset=utf-8",
            )],
            format!("window.__saloFinishSearch({id:?});"),
        )
            .into_response());
    }

    let view = SearchProgressTemplate {
        query: progress.query.clone(),
        trackers: progress
            .trackers
            .iter()
            .map(|t| TrackerProgressView {
                name: t.name,
                state: t.state.as_str(),
                elapsed_ms: t.elapsed_ms,
                results: t.results,
                error: t.error.clone(),
            })
            .collect(),
        done_count: progress.done_count(),
        total: progress.total(),
        percent: progress.percent(),
        elapsed_ms: progress.elapsed_ms(),
        slowest_ms: progress.slowest_ms(),
        finished: false,
    };
    Ok(HtmlTemplate(view).into_response())
}

/// Everything recorded in SQLite about torrents added here -- including
/// ones no longer active in the client, which is what makes the database
/// useful for finding downloaded files again (or re-adding a torrent).
pub async fn stored(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let stored = state.torrents.store().all()?;
    let now = crate::torrent_store::unix_now();

    // The stored `active` flag says a torrent was in the client when it was
    // written, but the client's own state doesn't survive a restart -- so
    // trust what's actually running right now instead. Otherwise a
    // restarted instance would claim torrents are active when nothing is
    // attached, and hide the Re-add button that fixes it.
    let live: std::collections::HashSet<String> = state
        .torrents
        .list()
        .into_iter()
        .map(|t| t.info_hash)
        .collect();

    Ok(HtmlTemplate(StoredTemplate {
        torrents: stored
            .into_iter()
            .map(|t| StoredTorrentView {
                active: live.contains(&t.info_hash),
                total_size: indexers::format_size(t.total_bytes),
                added_ago: humanize_ago(now.saturating_sub(t.added_at).max(0) as u64),
                finished_ago: t
                    .finished_at
                    .map(|at| humanize_ago(now.saturating_sub(at).max(0) as u64)),
                files: t
                    .files
                    .into_iter()
                    .map(|f| {
                        // Check the real path: the point of this page is
                        // telling you what you can still get back.
                        let on_disk = state.torrents.store().saved_path(&t.output_folder, &f.name);
                        StoredFileView {
                            present: on_disk.is_file(),
                            size: indexers::format_size(f.len),
                            name: f.name,
                        }
                    })
                    .collect(),
                seed_minutes: t.seed_minutes,
                info_hash: t.info_hash,
                name: t.name,
                source_url: t.source_url,
                output_folder: t.output_folder,
            })
            .collect(),
    }))
}

/// Re-adds a stored torrent from the magnet/URL recorded with it -- the
/// restore path when a torrent was removed (or the client restarted) but
/// its files are still on disk.
pub async fn stored_readd(
    State(state): State<Arc<AppState>>,
    Form(form): Form<StoredForm>,
) -> Result<impl IntoResponse, AppError> {
    let Some(stored) = state.torrents.store().get(&form.info_hash)? else {
        return Err(anyhow::anyhow!("no stored record for {}", form.info_hash).into());
    };
    let Some(source) = stored.source.clone() else {
        return Err(anyhow::anyhow!(
            "the stored record for this torrent has no magnet/URL to re-add from \
             (it was added from a site download that wasn't recorded)"
        )
        .into());
    };

    let folder = (!stored.output_folder.is_empty()).then(|| stored.output_folder.clone());
    // `reattach` rather than `add`: the files are expected to still be on
    // disk, and librqbit refuses to add a torrent over existing files.
    state.torrents.reattach(&source, folder).await?;
    if let Err(err) = state.torrents.store().mark_active(&form.info_hash) {
        tracing::warn!(error = ?err, "could not mark stored torrent active");
    }
    Ok(Redirect::to("/stored"))
}

/// Drops a stored record and its file list. Files on disk are untouched --
/// this only forgets what the database knew.
pub async fn stored_forget(
    State(state): State<Arc<AppState>>,
    Form(form): Form<StoredForm>,
) -> Result<impl IntoResponse, AppError> {
    state.torrents.store().delete(&form.info_hash)?;
    Ok(Redirect::to("/stored"))
}

#[derive(Deserialize)]
pub struct StoredForm {
    info_hash: String,
}

/// Coarse "3d", "5h", "12m" rendering of an age in seconds, for the stored
/// list -- exact timestamps aren't useful there.
fn humanize_ago(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
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
    /// Which indexer the magnet came from -- a login-walled indexer's
    /// download URL needs the stored session to fetch its .torrent file,
    /// which librqbit's plain URL fetch cannot do.
    #[serde(default)]
    indexer: Option<String>,
    /// The release's page on the indexer's site, if it had one -- kept
    /// alongside the torrent so its detail page can link back to it.
    #[serde(default)]
    source_url: Option<String>,
    /// Save this torrent under a specific directory instead of the
    /// server's default download directory. Blank means use the default.
    #[serde(default)]
    directory: Option<String>,
    /// The release's title, as shown on the results page. Only used to
    /// name the per-torrent subfolder for a login-walled indexer's
    /// .torrent file (whose own name isn't parsed until later).
    #[serde(default)]
    title: Option<String>,
    /// Put this torrent's files in their own folder, named after the
    /// release, rather than scattering them into the download directory.
    /// Absent (unchecked) means flat, which is the historical behaviour.
    #[serde(default)]
    subfolder: Option<String>,
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
    let subfolder = form.subfolder.is_some();

    // Login-walled indexers (toloka) serve their .torrent files only to
    // an authenticated session, so fetch the bytes here and add from
    // those; everything else (magnet links, public .torrent URLs) goes
    // through librqbit's own URL fetch as before.
    let added = if let Some(indexer) = form.indexer.as_deref() {
        match indexers::download_torrent(&state.http, &state.config, indexer, &form.magnet).await? {
            Some(bytes) => {
                let name = form.title.clone();
                state
                    .torrents
                    .add_bytes(bytes, directory.clone(), name, subfolder)
                    .await?
            }
            None if subfolder => {
                state
                    .torrents
                    .add_in_subfolder(&form.magnet, form.title.clone())
                    .await?
            }
            None => state.torrents.add(&form.magnet, directory.clone()).await?,
        }
    } else if subfolder {
        state
            .torrents
            .add_in_subfolder(&form.magnet, form.title.clone())
            .await?
    } else {
        state.torrents.add(&form.magnet, directory.clone()).await?
    };

    // Record it durably FIRST, with the magnet/URL it came from. Order
    // matters: `set_seed_limit` and `set_source_url` are UPDATEs, so they
    // would affect no rows (and be silently lost) if the record didn't
    // exist yet -- `persist_added` carries the source URL itself, and the
    // seed limit is applied right after.
    state.torrents.persist_added(
        &added,
        Some(form.magnet.clone()),
        form.source_url.clone(),
        directory,
    );

    if form.seed_minutes.is_some() || form.seed_ratio.is_some() {
        state
            .torrents
            .set_seed_limit(&added.info_hash, form.seed_minutes, form.seed_ratio);
    }

    // The in-memory source URL is what the detail page reads; keep it in
    // step with the row just written.
    if form.source_url.is_some() {
        state
            .torrents
            .set_source_url(&added.info_hash, form.source_url.clone());
    }

    // Freshly added: librqbit has no stats snapshot yet, so there's no
    // per-file progress to show.
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
        .map(|f| {
            // Per-file percentage when the engine reported downloaded
            // bytes (the detail page); `None` right after `add`, where
            // no stats snapshot exists yet.
            let percent = f.downloaded_bytes.map(|done| {
                if f.len == 0 {
                    0
                } else {
                    (done * 100 / f.len) as u32
                }
            });
            FileEntry {
                is_video: crate::torrent::is_video_file(&f.name),
                is_audio: crate::torrent::is_audio_file(&f.name),
                size: indexers::format_size(f.len),
                percent,
                stream_href: format!("/stream/{info_hash}/{}", f.file_id),
                download_href: format!("/download/{info_hash}/{}", f.file_id),
                name: f.name,
            }
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

    // Computed before the fields below move out of `summary`.
    let download_speed = indexers::format_speed(summary.download_speed);
    let upload_speed = indexers::format_speed(summary.upload_speed);
    let ratio = summary.ratio().map(|r| format!("{r:.2}"));
    let total_size = indexers::format_size(summary.total_bytes);
    let uploaded = indexers::format_size(summary.uploaded_bytes);
    let seed_limit = seed_limit_label(summary.seed_minutes, summary.seed_ratio);

    // Poster/blurb, if a TMDB key is configured and it has a match. A
    // failure here must not break the page: the torrent's own controls
    // are the point, the artwork is a bonus.
    let movie = match tmdb::lookup(&state.http, &state.config, &summary.name).await {
        Ok(found) => found,
        Err(err) => {
            tracing::warn!(error = ?err, "TMDB lookup failed");
            None
        }
    };

    Ok(HtmlTemplate(TorrentDetailTemplate {
        info_hash,
        name: summary.name,
        source_url: summary.source_url,
        finished: summary.finished,
        paused: summary.paused,
        progress_percent,
        seeded_for_minutes: summary.seeded_for_minutes,
        download_speed,
        upload_speed,
        ratio,
        total_size,
        uploaded,
        seed_limit,
        seed_minutes: summary.seed_minutes.unwrap_or(0).to_string(),
        seed_ratio: summary.seed_ratio.unwrap_or(0.0).to_string(),
        movie,
        files,
    }))
}

/// Describes the configured seed limit in words, for the detail page.
/// Both limits can be set at once (whichever is reached first wins), so
/// this has to read sensibly for any combination.
fn seed_limit_label(minutes: Option<u64>, ratio: Option<f64>) -> String {
    match (minutes, ratio) {
        (None, None) => "none (seeds indefinitely)".to_string(),
        (Some(m), None) => format!("{m} minutes"),
        (None, Some(r)) => format!("ratio {r}"),
        (Some(m), Some(r)) => format!("{m} minutes or ratio {r}, whichever comes first"),
    }
}

/// The read-only environment-variable section of the settings page.
fn env_settings() -> Vec<EnvSetting> {
    crate::env::current()
        .into_iter()
        .map(|(name, value, is_default, help)| EnvSetting {
            name,
            value,
            is_default,
            help,
        })
        .collect()
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

/// Per-indexer settings: labeled inputs for everything the indexer's
/// module declares it needs (see `indexers::settings_fields`), plus any
/// legacy raw key/value rows kept from the old editor. Values live in
/// the per-indexer config store (`src/config_store.rs`).
pub async fn settings(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, AppError> {
    let mut indexers = Vec::new();
    // The metadata section first: it is not a tracker, and rendering it
    // through the same loop keeps its field handling (masking, the
    // "(set)" hint) identical to every indexer's.
    let sections = std::iter::once(crate::tmdb::SECTION).chain(indexers::names());

    for name in sections {
        let stored: Vec<(String, String)> = state.config.get_all(name)?;
        let get = |key: &str| {
            stored
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
        };

        let declared = indexers::settings_fields(name);
        let declared_keys: Vec<&str> = declared.iter().map(|f| f.key).collect();

        let fields = declared
            .into_iter()
            .map(|f| {
                let stored_value = get(f.key);
                let has_value = stored_value
                    .as_deref()
                    .is_some_and(|v| !v.trim().is_empty());
                let (checked, value) = match f.kind {
                    indexers::SettingFieldKind::Checkbox => {
                        // Match `toloka::Settings::load`'s flag parsing.
                        let checked = stored_value
                            .as_deref()
                            .map(|v| matches!(v.trim(), "1" | "true" | "on" | "yes"))
                            .unwrap_or(f.default_on);
                        (checked, String::new())
                    }
                    _ => (false, stored_value.unwrap_or_default()),
                };
                SettingFieldView {
                    label: f.label,
                    key: f.key,
                    input_type: match f.kind {
                        indexers::SettingFieldKind::Password => "password",
                        indexers::SettingFieldKind::Checkbox => "checkbox",
                        _ => "text",
                    },
                    help: f.help,
                    value,
                    checked,
                    is_checkbox: f.kind == indexers::SettingFieldKind::Checkbox,
                    has_value,
                }
            })
            .collect();

        let extra_entries = stored
            .into_iter()
            .filter(|(k, _)| !declared_keys.contains(&k.as_str()))
            .map(|(key, value)| SettingEntry { key, value })
            .collect();

        indexers.push(IndexerSettings {
            name,
            fields,
            extra_entries,
        });
    }
    Ok(HtmlTemplate(SettingsTemplate {
        indexers,
        env: env_settings(),
    }))
}

#[derive(Deserialize)]
pub struct SaveSettingForm {
    indexer: String,
    key: String,
    /// Missing entirely when a checkbox form is submitted unchecked
    /// (HTML omits unchecked boxes from the POST body).
    #[serde(default)]
    value: String,
}

pub async fn save_setting(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<SaveSettingForm>,
) -> Result<Response, AppError> {
    let kind = indexers::settings_fields(&form.indexer)
        .into_iter()
        .find(|f| f.key == form.key)
        .map(|f| f.kind);

    match kind {
        // Password inputs never pre-fill their stored value, so a blank
        // submission means "leave it as-is", not "clear it" -- clearing
        // is the Remove button next to the field.
        Some(indexers::SettingFieldKind::Password) if form.value.trim().is_empty() => {}
        // Checkboxes submit no `value` when unchecked: absent = "0".
        Some(indexers::SettingFieldKind::Checkbox) => {
            let value = if form.value.trim().is_empty() { "0" } else { "1" };
            state.config.set(&form.indexer, &form.key, value)?;
        }
        // Text fields (and undeclared legacy keys) save verbatim.
        _ => {
            state.config.set(&form.indexer, &form.key, &form.value)?;
        }
    }

    // htmx-driven saves get just this field's status back, so saving one
    // field never re-renders the whole page (and never disturbs whatever
    // is half-typed in another). A plain form POST (no JS) still redirects.
    if headers.contains_key("hx-request") {
        // Read the stored state back rather than inferring it from the
        // submission, so a blank-password save that kept an existing value
        // reports "(set)" correctly.
        let set_hint = state
            .config
            .get_all(&form.indexer)?
            .iter()
            .any(|(k, v)| k == &form.key && !v.trim().is_empty());
        return Ok(HtmlTemplate(SaveStatusTemplate { set_hint }).into_response());
    }
    Ok(Redirect::to("/settings").into_response())
}

#[derive(Deserialize)]
pub struct DeleteSettingForm {
    indexer: String,
    key: String,
}

pub async fn delete_setting(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<DeleteSettingForm>,
) -> Result<Response, AppError> {
    state.config.delete(&form.indexer, &form.key)?;

    // htmx replaces the row with this (empty) response, so removing one
    // legacy key doesn't reload the page. Plain POST redirects as before.
    if headers.contains_key("hx-request") {
        return Ok(HtmlTemplate(DeletedRowTemplate {}).into_response());
    }
    Ok(Redirect::to("/settings").into_response())
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
            progress_percent: t.progress_percent(),
            total_size: indexers::format_size(t.total_bytes),
            uploaded: indexers::format_size(t.uploaded_bytes),
            download_speed: indexers::format_speed(t.download_speed),
            upload_speed: indexers::format_speed(t.upload_speed),
            ratio: t.ratio().map(|r| format!("{r:.2}")),
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
    /// `"detail"` when submitted from a torrent's own page, so the
    /// redirect returns there (see `return_to`).
    #[serde(default)]
    return_to: Option<String>,
}

pub async fn delete_torrent(
    State(state): State<Arc<AppState>>,
    Form(form): Form<DeleteTorrentForm>,
) -> Result<impl IntoResponse, AppError> {
    state
        .torrents
        .delete(&form.info_hash, form.delete_files.is_some())
        .await?;
    // Actions started from a torrent's own page return there, so the
    // page reflects the result instead of bouncing to the list.
    Ok(Redirect::to(&return_to(&form.return_to, &form.info_hash)))
}

/// Where a torrent action should send the browser afterwards: back to the
/// torrent's detail page when the request came from there, otherwise the
/// management list.
///
/// The path is built from the info hash rather than taken from the form's
/// value, so a crafted `return_to` cannot redirect a user off-site -- the
/// field only selects *which* of our two pages to land on.
fn return_to(return_to: &Option<String>, info_hash: &str) -> String {
    match return_to.as_deref() {
        Some("detail") => format!("/torrents/{info_hash}"),
        _ => "/torrents".to_string(),
    }
}

#[derive(Deserialize)]
pub struct SeedLimitForm {
    info_hash: String,
    /// Both `0`/blank clears the limit (seed indefinitely again).
    #[serde(default, deserialize_with = "zero_or_blank_as_none")]
    seed_minutes: Option<u64>,
    #[serde(default, deserialize_with = "zero_or_blank_as_none")]
    seed_ratio: Option<f64>,
    #[serde(default)]
    return_to: Option<String>,
}

pub async fn set_seed_limit(
    State(state): State<Arc<AppState>>,
    Form(form): Form<SeedLimitForm>,
) -> impl IntoResponse {
    state
        .torrents
        .set_seed_limit(&form.info_hash, form.seed_minutes, form.seed_ratio);
    Redirect::to(&return_to(&form.return_to, &form.info_hash))
}

#[derive(Deserialize)]
pub struct PauseForm {
    info_hash: String,
    /// `"1"` pauses, anything else resumes.
    #[serde(default)]
    paused: Option<String>,
    #[serde(default)]
    return_to: Option<String>,
}

/// Pauses a torrent (stopping it seeding or downloading) or resumes it.
/// Keeping the torrent in the session is the point: this is the "stop
/// seeding" action, not a removal.
pub async fn set_paused(
    State(state): State<Arc<AppState>>,
    Form(form): Form<PauseForm>,
) -> Result<impl IntoResponse, AppError> {
    let paused = form.paused.as_deref() == Some("1");
    state.torrents.set_paused(&form.info_hash, paused).await?;
    Ok(Redirect::to(&return_to(&form.return_to, &form.info_hash)))
}

#[derive(Deserialize)]
pub struct PosterForm {
    info_hash: String,
    /// The poster URL to use from now on. Empty clears the override and
    /// goes back to TMDB's own first match.
    #[serde(default)]
    poster_url: Option<String>,
}

/// Remembers which poster the user chose for a torrent's detail page.
///
/// The choice is stored globally rather than per torrent: it is a
/// preference for how this release name should be presented, and the same
/// release re-added (or restored from `/stored`) should keep it.
pub async fn set_poster(
    State(state): State<Arc<AppState>>,
    Form(form): Form<PosterForm>,
) -> Result<impl IntoResponse, AppError> {
    let url = form
        .poster_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());

    // Only a URL from TMDB's own image host is accepted. The value is
    // rendered into an `img src`, so accepting an arbitrary URL would let
    // a crafted link point the browser at a third party.
    if let Some(url) = url
        && !url.starts_with("https://image.tmdb.org/t/p/")
    {
        return Err(anyhow::anyhow!("poster must be a TMDB image URL").into());
    }

    match url {
        Some(url) => state
            .config
            .set(tmdb::SECTION, tmdb::KEY_POSTER, url)
            .map_err(AppError::from)?,
        None => state
            .config
            .delete(tmdb::SECTION, tmdb::KEY_POSTER)
            .map_err(AppError::from)?,
    }
    Ok(Redirect::to(&format!("/torrents/{}", form.info_hash)))
}
