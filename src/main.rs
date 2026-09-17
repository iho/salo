mod config_store;
mod env;
mod error;
mod indexers;
mod routes;
mod search_progress;
mod templates;
mod tmdb;
mod torrent;
mod torrent_store;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

use config_store::ConfigStore;
use routes::AppState;
use torrent::TorrentEngine;

/// Tokio's default (`#[tokio::main]` with no args) spawns one worker
/// thread per CPU core -- each with its own OS thread stack and part of
/// the work-stealing scheduler. For a lightly-loaded personal server
/// (not something serving many concurrent users), that's pure overhead on
/// anything with more than a couple of cores. `WORKER_THREADS` lets you
/// tune it for the actual deployment box; the default of 2 keeps
/// `librqbit`'s `block_in_place` disk-I/O offloading working (it only
/// takes that fast path on a genuine multi-thread runtime -- see
/// `librqbit::spawn_utils::BlockingSpawner`) while capping thread count
/// on many-core machines. Set to `1` for the smallest footprint if you
/// don't mind disk I/O briefly blocking request handling.
fn main() -> anyhow::Result<()> {
    let worker_threads: usize = std::env::var("WORKER_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or_else(|| {
            env::DEFAULT_WORKER_THREADS
                .parse::<usize>()
                .expect("valid default")
        })
        .max(1);

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()?
        .block_on(run(worker_threads))
}

async fn run(worker_threads: usize) -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let download_dir = std::env::var("DOWNLOAD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env::DEFAULT_DOWNLOAD_DIR));
    let db_path = std::env::var("DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env::DEFAULT_DB_PATH));
    let bind_addr: SocketAddr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| env::DEFAULT_BIND_ADDR.to_string())
        .parse()?;

    let torrent_store = Arc::new(torrent_store::TorrentStore::open(&db_path)?);
    let torrents =
        TorrentEngine::new(download_dir, Arc::clone(&torrent_store), worker_threads).await?;
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
        .route("/search-progress.js", get(routes::search_progress_js))
        .route("/torrent-stats.js", get(routes::torrent_stats_js))
        .route("/logo.svg", get(routes::logo_svg))
        .route("/search", get(routes::search))
        .route("/search/start", get(routes::search_start))
        .route("/search/progress/{id}", get(routes::search_progress))
        .route("/open", post(routes::open))
        .route("/stream/{info_hash}/{file_id}", get(routes::stream))
        .route("/download/{info_hash}/{file_id}", get(routes::download))
        .route("/torrents", get(routes::torrents))
        .route("/torrents/stats", get(routes::torrent_stats))
        .route("/stored", get(routes::stored))
        .route("/stored/readd", post(routes::stored_readd))
        .route("/stored/forget", post(routes::stored_forget))
        .route("/torrents/delete", post(routes::delete_torrent))
        .route("/torrents/seed-limit", post(routes::set_seed_limit))
        .route("/torrents/pause", post(routes::set_paused))
        .route("/torrents/poster", post(routes::set_poster))
        .route("/torrents/{info_hash}", get(routes::torrent_detail))
        .route("/settings", get(routes::settings).post(routes::save_setting))
        .route("/settings/delete", post(routes::delete_setting))
        .with_state(state);

    tracing::info!(%bind_addr, "listening");
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
