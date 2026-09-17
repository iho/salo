mod config_store;
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

use config_store::ConfigStore;
use routes::AppState;
use torrent::TorrentEngine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let download_dir = std::env::var("DOWNLOAD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./downloads"));
    let db_path = std::env::var("DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./salo.db"));
    let bind_addr: SocketAddr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:3000".to_string())
        .parse()?;

    let torrents = TorrentEngine::new(download_dir).await?;
    let config = Arc::new(ConfigStore::open(&db_path)?);
    let state = Arc::new(AppState {
        http: reqwest::Client::builder()
            .user_agent("salo/0.1")
            .build()?,
        torrents,
        config,
    });

    let app = Router::new()
        .route("/", get(routes::index))
        .route("/htmx.min.js", get(routes::htmx_js))
        .route("/theme.css", get(routes::theme_css))
        .route("/theme.js", get(routes::theme_js))
        .route("/open-dialog.js", get(routes::open_dialog_js))
        .route("/search", get(routes::search))
        .route("/open", post(routes::open))
        .route("/stream/{info_hash}/{file_id}", get(routes::stream))
        .route("/download/{info_hash}/{file_id}", get(routes::download))
        .route("/torrents", get(routes::torrents))
        .route("/torrents/delete", post(routes::delete_torrent))
        .route("/torrents/seed-limit", post(routes::set_seed_limit))
        .route("/torrents/{info_hash}", get(routes::torrent_detail))
        .route("/settings", get(routes::settings).post(routes::save_setting))
        .route("/settings/delete", post(routes::delete_setting))
        .with_state(state);

    tracing::info!(%bind_addr, "listening");
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
