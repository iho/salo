//! A real indexer for archive.org (the Internet Archive) -- scoped to two
//! collections chosen specifically for their unambiguous legal status,
//! since the Internet Archive as a whole hosts a huge range of material
//! including user uploads of uncertain rights:
//!
//! - `prelinger`: the Prelinger Archives, ephemeral/educational/industrial
//!   films donated to the public domain by their creators.
//! - `etree`: the Live Music Archive, concert recordings artists have
//!   explicitly authorized fans to record and freely trade.
//!
//! Uses the site's own `advancedsearch.php` JSON API rather than scraping
//! HTML, and each result's `.torrent` is a predictable per-item URL
//! (`archive.org/download/<id>/<id>_archive.torrent`) rather than a
//! magnet -- confirmed live before wiring this up.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::Release;

pub const NAME: &str = "archive";

const SEARCH_URL: &str = "https://archive.org/advancedsearch.php";
// Only collections verified to be rights-cleared for free redistribution;
// archive.org overall hosts plenty of user uploads of uncertain rights,
// so this is deliberately not an open-ended search of the whole site.
const COLLECTIONS: &str = "prelinger OR etree";

#[derive(Deserialize)]
struct SearchResponse {
    response: ResponseBody,
}

#[derive(Deserialize)]
struct ResponseBody {
    docs: Vec<Doc>,
}

#[derive(Deserialize)]
struct Doc {
    identifier: String,
    title: Option<String>,
    item_size: Option<u64>,
}

pub async fn search(client: &reqwest::Client, query: &str) -> Result<Vec<Release>> {
    let q = format!("({query}) AND collection:({COLLECTIONS})");
    let body = client
        .get(SEARCH_URL)
        .query(&[
            ("q", q.as_str()),
            ("fl[]", "identifier"),
            ("fl[]", "title"),
            ("fl[]", "item_size"),
            ("rows", "25"),
            ("output", "json"),
        ])
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .with_context(|| format!("request to {NAME} failed"))?
        .error_for_status()
        .with_context(|| format!("{NAME} returned an error status"))?
        .json::<SearchResponse>()
        .await
        .with_context(|| format!("failed to parse {NAME} response"))?;

    Ok(body
        .response
        .docs
        .into_iter()
        .map(|doc| {
            let id = doc.identifier;
            Release {
                indexer: NAME,
                title: doc.title.unwrap_or_else(|| id.clone()),
                // The search API doesn't carry live swarm stats.
                seeders: 0,
                leechers: 0,
                size: match doc.item_size {
                    Some(bytes) if bytes > 0 => super::format_size(bytes),
                    _ => "unknown".to_string(),
                },
                magnet: format!("https://archive.org/download/{id}/{id}_archive.torrent"),
                source_url: Some(format!("https://archive.org/details/{id}")),
            }
        })
        .collect())
}
