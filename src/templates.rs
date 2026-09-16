use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

use crate::indexers::Release;

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate;

#[derive(Template)]
#[template(path = "results.html")]
pub struct ResultsTemplate {
    pub query: String,
    pub releases: Vec<Release>,
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
