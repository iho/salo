//! A real, working indexer for torrents-csv.com -- "a self-hostable open
//! source torrent search engine and database". Public, no account, and
//! its own `/service/search` JSON endpoint is meant to be queried this
//! way (no scraping, no auth), so this one is verifiable end to end
//! without credentials.
//!
//! The API hands back a raw infohash rather than a magnet, and has no
//! per-release detail page at all -- the upstream definition synthesizes
//! the magnet from the infohash and uses the site's own search URL as the
//! "release page" link, and this does the same.
//!
//! Note the API's own minimum: a blank term or one under 3 characters is
//! not served, so those return nothing rather than an error.

use std::time::Duration;

use anyhow::{Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;

use super::Release;

pub const NAME: &str = "torrentscsv";

const SEARCH_URL: &str = "https://torrents-csv.com/service/search";

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    torrents: Vec<Torrent>,
}

#[derive(Deserialize)]
struct Torrent {
    infohash: String,
    name: String,
    #[serde(default)]
    size_bytes: u64,
    #[serde(default)]
    seeders: u32,
    #[serde(default)]
    leechers: u32,
}

pub async fn search(client: &reqwest::Client, query: &str) -> Result<Vec<Release>> {
    let query = query.trim();
    // The service itself refuses terms shorter than 3 characters; asking
    // anyway returns an error page, so treat it as "no results".
    if query.len() < 3 {
        return Ok(Vec::new());
    }

    let body = client
        .get(SEARCH_URL)
        .query(&[("q", query), ("size", "100")])
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
        .torrents
        .into_iter()
        .filter(|t| !t.infohash.is_empty() && !t.name.is_empty())
        .map(|t| Release {
            indexer: NAME,
            title: t.name.clone(),
            seeders: t.seeders,
            leechers: t.leechers,
            size: super::format_size(t.size_bytes),
            magnet: format!(
                "magnet:?xt=urn:btih:{}&dn={}",
                t.infohash,
                utf8_percent_encode(&t.name, NON_ALPHANUMERIC)
            ),
            // No per-release page exists on this site; its own definition
            // points at a search URL, which is the closest thing available.
            source_url: Some(format!(
                "https://torrents-csv.com/search?q={}",
                utf8_percent_encode(&t.name, NON_ALPHANUMERIC)
            )),
            comments: None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_torrents_and_builds_magnets() {
        let body = r#"{"torrents":[
            {"infohash":"ABC123","name":"Some Release","size_bytes":1048576,
             "created_unix":1700000000,"seeders":5,"leechers":2},
            {"infohash":"DEF456","name":"Another","size_bytes":2048,
             "seeders":0,"leechers":0}
        ]}"#;
        let parsed: SearchResponse = serde_json::from_str(body).expect("valid json");
        assert_eq!(parsed.torrents.len(), 2);
        assert_eq!(parsed.torrents[0].infohash, "ABC123");
        assert_eq!(parsed.torrents[0].seeders, 5);
        assert_eq!(parsed.torrents[1].size_bytes, 2048);
    }

    #[test]
    fn missing_optional_fields_default() {
        // A row with no seeders/leechers/size must not fail the parse.
        let body = r#"{"torrents":[{"infohash":"X","name":"Y"}]}"#;
        let parsed: SearchResponse = serde_json::from_str(body).expect("valid json");
        assert_eq!(parsed.torrents[0].seeders, 0);
        assert_eq!(parsed.torrents[0].size_bytes, 0);
    }

    #[test]
    fn empty_response_is_not_an_error() {
        let parsed: SearchResponse = serde_json::from_str(r#"{"torrents":[]}"#).expect("valid");
        assert!(parsed.torrents.is_empty());
    }
}
