mod error;
mod indexers;
mod routes;
mod templates;
mod torrent;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

use routes::AppState;
use torrent::TorrentEngine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let download_dir = std::env::var("DOWNLOAD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./downloads"));
    let bind_addr: SocketAddr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:3000".to_string())
        .parse()?;

    let torrents = Arc::new(TorrentEngine::new(download_dir).await?);
    let state = Arc::new(AppState {
        http: reqwest::Client::builder()
            .user_agent("salo/0.1")
            .build()?,
        torrents,
    });

    let app = Router::new()
        .route("/", get(routes::index))
        .route("/htmx.min.js", get(routes::htmx_js))
        .route("/search", get(routes::search))
        .route("/watch", post(routes::watch))
        .route("/stream/{info_hash}/{file_id}", get(routes::stream))
        .with_state(state);

    tracing::info!(%bind_addr, "listening");
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
