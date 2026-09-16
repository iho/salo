use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use crate::indexers::Release;

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub trackers: Vec<&'static str>,
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

#[derive(Template)]
#[template(path = "player.html")]
pub struct PlayerTemplate {
    pub info_hash: String,
    pub file_id: usize,
    pub file_name: String,
    pub file_len: u64,
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
