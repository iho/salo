use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use crate::indexers::Release;

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub trackers: Vec<&'static str>,
    /// Pre-fills the search box and tracker selector, and embeds the
    /// already-rendered results fragment -- used when `/search` is hit
    /// as a direct navigation (a reload, a bookmark, a pasted link)
    /// rather than an HTMX fragment swap, so that URL still renders a
    /// complete, styled page instead of a bare `<table>`. Empty/"all" for
    /// a plain `GET /`.
    pub query: String,
    pub selected_tracker: String,
    pub results_html: String,
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

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsTemplate {
    pub indexers: Vec<IndexerSettings>,
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
    pub progress_percent: u32,
    pub seeded_for_minutes: Option<u64>,
    pub files: Vec<FileEntry>,
}

pub struct TorrentRow {
    pub info_hash: String,
    pub name: String,
    pub output_folder: String,
    pub finished: bool,
    pub progress_percent: u32,
    pub total_size: String,
    pub uploaded: String,
    /// Empty string when no limit of that kind is set (keeps the template
    /// dead simple: just print the field, no `{% if %}` needed for the
    /// input's `value` attribute).
    pub seed_minutes: String,
    pub seed_ratio: String,
    pub seeded_for_minutes: Option<u64>,
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
