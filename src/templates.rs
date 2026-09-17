use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use crate::indexers::Release;

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub trackers: Vec<&'static str>,
    /// Pre-fills the search box and tracker checkboxes, and embeds the
    /// already-rendered results fragment -- used when `/search` is hit
    /// as a direct navigation (a reload, a bookmark, a pasted link)
    /// rather than an HTMX fragment swap, so that URL still renders a
    /// complete, styled page instead of a bare `<table>`. Empty for
    /// a plain `GET /`, and empty `selected_trackers` means "all".
    pub query: String,
    pub selected_trackers: Vec<String>,
    pub results_html: String,
}

impl IndexTemplate {
    /// Each tracker paired with whether it should render checked. Done
    /// here rather than in the template because matching a `&'static str`
    /// against the `Vec<String>` selection needs a closure, which Askama
    /// expressions don't take.
    pub fn tracker_options(&self) -> Vec<TrackerOption> {
        self.trackers
            .iter()
            .map(|name| TrackerOption {
                name,
                checked: self.selected_trackers.iter().any(|s| s == name),
            })
            .collect()
    }
}

/// One tracker checkbox: its name and whether it's currently selected.
pub struct TrackerOption {
    pub name: &'static str,
    pub checked: bool,
}

/// One sortable column header: `href` already has the toggled sort/dir
/// (and the current query/tracker/page=1) baked in, so the template just
/// links to it -- no sort-toggling logic needed in Askama itself.
pub struct ColumnSort {
    pub label: &'static str,
    pub href: String,
    pub arrow: &'static str,
}

#[derive(Template)]
#[template(path = "results.html")]
pub struct ResultsTemplate {
    pub query: String,
    pub releases: Vec<Release>,
    pub columns: Vec<ColumnSort>,
    pub total_results: usize,
    pub page: usize,
    pub total_pages: usize,
    pub has_prev: bool,
    pub prev_href: String,
    pub has_next: bool,
    pub next_href: String,
}

/// A stored key/value pair this app's code didn't declare -- legacy data
/// from the old raw key/value editor, shown so it can still be removed.
pub struct SettingEntry {
    pub key: String,
    pub value: String,
}

pub struct SettingFieldView {
    /// The human label ("Username") -- `key` stays the storage key.
    pub label: &'static str,
    /// The storage key in the per-indexer config store.
    pub key: &'static str,
    /// `text`, `password`, or `checkbox` -- the `<input>` type.
    pub input_type: &'static str,
    /// Help/hint text rendered under the label.
    pub help: &'static str,
    /// The currently stored value (empty when unset).
    pub value: String,
    /// Checkbox state: checked now (stored value or the declared default).
    pub checked: bool,
    /// Whether the input type is a checkbox (template branching).
    pub is_checkbox: bool,
    /// Whether something is stored under this key (password fields show
    /// "(set)" instead of the secret itself).
    pub has_value: bool,
}

pub struct IndexerSettings {
    pub name: &'static str,
    pub fields: Vec<SettingFieldView>,
    /// Keys with stored values that this indexer doesn't declare --
    /// legacy entries from the old raw key/value editor. Kept visible
    /// (with a Remove button) so nothing silently disappears.
    pub extra_entries: Vec<SettingEntry>,
}

/// One environment-variable-backed setting, shown read-only: these are
/// read from the process environment at startup and cannot be changed
/// from the web UI, so the page reports them rather than offering a
/// control that would lie about taking effect.
pub struct EnvSetting {
    pub name: &'static str,
    pub value: String,
    pub is_default: bool,
    pub help: &'static str,
}

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsTemplate {
    pub indexers: Vec<IndexerSettings>,
    pub env: Vec<EnvSetting>,
}

/// The tiny fragment returned to an htmx-driven field save -- just the
/// status for that one field, so saving never re-renders the page.
#[derive(Template)]
#[template(path = "settings_saved.html")]
pub struct SaveStatusTemplate {
    /// Re-show "(set)" next to a password field that now has a stored
    /// value. The value itself is never sent back.
    pub set_hint: bool,
}

/// Swapped in for a removed legacy row: htmx's `outerHTML` swap deletes
/// the row, so this is deliberately empty.
#[derive(Template)]
#[template(path = "settings_deleted.html")]
pub struct DeletedRowTemplate {}

/// One tracker's live state, for the polling progress panel.
pub struct TrackerProgressView {
    pub name: &'static str,
    /// `pending`, `running`, `done`, `failed` -- also the CSS class.
    pub state: &'static str,
    pub elapsed_ms: Option<u64>,
    pub results: usize,
    pub error: Option<String>,
}

/// The progress panel for an in-flight (or just-finished) search: a bar
/// plus each tracker's response time.
#[derive(Template)]
#[template(path = "search_progress.html")]
pub struct SearchProgressTemplate {
    /// The search term, echoed so the panel says what it's showing.
    pub query: String,
    pub trackers: Vec<TrackerProgressView>,
    pub done_count: usize,
    pub total: usize,
    pub percent: u32,
    pub elapsed_ms: u64,
    /// Slowest tracker so far: the wall-clock cost, since they run
    /// concurrently.
    pub slowest_ms: Option<u64>,
    pub finished: bool,
}

/// One stored torrent, formatted for the "stored torrents" page.
pub struct StoredTorrentView {
    pub info_hash: String,
    pub name: String,
    pub source_url: Option<String>,
    pub output_folder: String,
    pub total_size: String,
    pub added_ago: String,
    pub finished_ago: Option<String>,
    pub seed_minutes: Option<u64>,
    pub active: bool,
    pub files: Vec<StoredFileView>,
}

impl StoredTorrentView {
    /// How many of this torrent's files are still on disk -- computed here
    /// rather than in the template for clarity.
    pub fn present_count(&self) -> usize {
        self.files.iter().filter(|f| f.present).count()
    }
}

pub struct StoredFileView {
    pub name: String,
    pub size: String,
    /// Whether the file is actually present on disk right now -- the thing
    /// that makes this list useful for recovery rather than just a record
    /// of intent.
    pub present: bool,
}

/// Everything recorded in SQLite, including torrents no longer in the
/// client -- the page that makes the database useful for restoring.
#[derive(Template)]
#[template(path = "stored.html")]
pub struct StoredTemplate {
    pub torrents: Vec<StoredTorrentView>,
}

pub struct FileEntry {
    pub name: String,
    pub size: String,
    /// Per-file download progress, 0-100 -- `None` when unknown (the
    /// freshly-added `open` page has no stats snapshot yet).
    pub percent: Option<u32>,
    pub is_video: bool,
    pub is_audio: bool,
    pub stream_href: String,
    pub download_href: String,
}

#[derive(Template)]
#[template(path = "open.html")]
pub struct OpenTemplate {
    pub info_hash: String,
    pub name: String,
    pub files: Vec<FileEntry>,
}

#[derive(Template)]
#[template(path = "torrent_detail.html")]
pub struct TorrentDetailTemplate {
    pub info_hash: String,
    pub name: String,
    pub source_url: Option<String>,
    pub finished: bool,
    /// Deliberately stopped (paused), as opposed to errored or running.
    pub paused: bool,
    pub progress_percent: u32,
    pub seeded_for_minutes: Option<u64>,
    pub download_speed: String,
    pub upload_speed: String,
    /// Achieved upload/download ratio, pre-formatted, or `None` when the
    /// size isn't known yet.
    pub ratio: Option<String>,
    pub total_size: String,
    pub uploaded: String,
    /// Human-readable description of the configured seed limit, or
    /// "none" -- a sentence, since time and ratio limits can both apply.
    pub seed_limit: String,
    /// Current limit values for the form inputs (`"0"` = unset).
    pub seed_minutes: String,
    pub seed_ratio: String,
    /// Poster and blurb from TMDB, when a key is configured and there's a
    /// match. `None` hides the block entirely.
    pub movie: Option<crate::tmdb::MovieInfo>,
    pub files: Vec<FileEntry>,
}

impl TorrentDetailTemplate {
    pub fn ratio_at_least_one(&self) -> bool {
        self.ratio
            .as_deref()
            .and_then(|r| r.parse::<f64>().ok())
            .is_some_and(|r| r >= 1.0)
    }
}

pub struct TorrentRow {
    pub info_hash: String,
    pub name: String,
    pub output_folder: String,
    pub finished: bool,
    pub progress_percent: u32,
    pub total_size: String,
    pub uploaded: String,
    /// Pre-formatted rates (`"1.2 MB/s"`, or an em dash when idle).
    pub download_speed: String,
    pub upload_speed: String,
    /// Upload/download ratio actually reached so far, pre-formatted
    /// (`"1.42"`), or `None` while the size is still unknown -- distinct
    /// from `seed_ratio`, which is the *limit* that stops seeding.
    pub ratio: Option<String>,
    /// Empty string when no limit of that kind is set (keeps the template
    /// dead simple: just print the field, no `{% if %}` needed for the
    /// input's `value` attribute).
    pub seed_minutes: String,
    pub seed_ratio: String,
    pub seeded_for_minutes: Option<u64>,
}

impl TorrentRow {
    /// Whether the achieved ratio has reached 1.0, i.e. as much uploaded
    /// as downloaded -- precomputed because Askama can't compare an
    /// `Option<String>` against a number inline.
    pub fn ratio_at_least_one(&self) -> bool {
        self.ratio
            .as_deref()
            .and_then(|r| r.parse::<f64>().ok())
            .is_some_and(|r| r >= 1.0)
    }
}

#[derive(Template)]
#[template(path = "torrents.html")]
pub struct TorrentsTemplate {
    pub torrents: Vec<TorrentRow>,
}

/// Adapts any Askama `Template` into an axum response, rendering to HTML
/// (or a 500 with the render error, which only fires on a broken template).
pub struct HtmlTemplate<T>(pub T);

impl<T: Template> IntoResponse for HtmlTemplate<T> {
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(html) => Html(html).into_response(),
            Err(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template render error: {err}"),
            )
                .into_response(),
        }
    }
}
