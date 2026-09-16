use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// Single error type for all HTTP handlers. Every fallible step in the
/// scrape/torrent/stream pipeline collapses into this so handlers can just
/// use `?` and still return a sane HTTP response instead of panicking.
pub struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!(error = ?self.0, "request failed");
        (StatusCode::INTERNAL_SERVER_ERROR, format!("error: {:#}", self.0)).into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}
