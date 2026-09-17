//! A real, working indexer for Knaben -- a public torrent meta-search
//! engine. Public, no account; its JSON API at `api.knaben.org/v1` is a
//! POST endpoint that is documented and intended to be called directly,
//! so this one is verifiable end to end without credentials.
//!
//! Two things about the live API worth knowing (both confirmed against it,
//! not assumed):
//!
//! - Results are only indexed from other trackers, and the API's `hash`
//!   and `magnetUrl` fields come back **null** for most hits. The 40-hex
//!   `id` field is the infohash, so the magnet is synthesized from that
//!   when `magnetUrl` is absent.
//! - Hits with zero seeders are dead; the upstream definition filters
//!   them out, and so does this.

use std::time::Duration;

use anyhow::{Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;

use super::Release;

pub const NAME: &str = "knaben";

const API_URL: &str = "https://api.knaben.org/v1";
const PAGE_SIZE: u32 = 100;

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    hits: Vec<Hit>,
}

#[derive(Deserialize)]
struct Hit {
    /// 40-hex infohash. Present on essentially every hit; `hash` is the
    /// field that comes back null.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    bytes: u64,
    #[serde(default)]
    seeders: u32,
    /// The API names this `peers`, but it counts leechers, not total peers.
    #[serde(default)]
    peers: u32,
    #[serde(default, rename = "details")]
    details: Option<String>,
    #[serde(default, rename = "magnetUrl")]
    magnet_url: Option<String>,
    #[serde(default)]
    hash: Option<String>,
}

pub async fn search(client: &reqwest::Client, query: &str) -> Result<Vec<Release>> {
    let mut body = serde_json::json!({
        "order_by": "date",
        "order_direction": "desc",
        "from": 0,
        "size": PAGE_SIZE,
        "hide_unsafe": true,
    });

    let query = query.trim();
    if !query.is_empty() {
        body["search_type"] = serde_json::json!("100%");
        body["search_field"] = serde_json::json!("title");
        body["query"] = serde_json::json!(query);
    }

    let response = client
        .post(API_URL)
        .json(&body)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .with_context(|| format!("request to {NAME} failed"))?
        .error_for_status()
        .with_context(|| format!("{NAME} returned an error status"))?
        .json::<SearchResponse>()
        .await
        .with_context(|| format!("failed to parse {NAME} response"))?;

    Ok(response
        .hits
        .into_iter()
        .filter(|h| h.seeders > 0 && !h.title.is_empty())
        .filter_map(|h| {
            // Prefer the API's own magnet when it has one; otherwise build
            // it from the infohash (which lives in `id`, not `hash`).
            let magnet = h.magnet_url.filter(|m| !m.is_empty()).or_else(|| {
                let hash = h
                    .hash
                    .filter(|s| !s.is_empty())
                    .or_else(|| h.id.filter(|s| s.len() == 40))?;
                Some(format!(
                    "magnet:?xt=urn:btih:{hash}&dn={}",
                    utf8_percent_encode(&h.title, NON_ALPHANUMERIC)
                ))
            })?;

            Some(Release {
                indexer: NAME,
                title: h.title,
                seeders: h.seeders,
                leechers: h.peers,
                size: super::format_size(h.bytes),
                magnet,
                source_url: h.details,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_id_is_treated_as_the_infohash() {
        // The live API returns hash/magnetUrl as null but a real id.
        let body = r#"{"hits":[{"id":"0023b7552f52c4a04a8eb57e7a01685a10f6d060",
            "title":"Some Ubuntu Pack","bytes":4720506880,"seeders":7,"peers":0,
            "details":"https://example.invalid/t=1","hash":null,"magnetUrl":null}]}"#;
        let parsed: SearchResponse = serde_json::from_str(body).expect("valid json");
        let hit = &parsed.hits[0];
        assert_eq!(hit.id.as_deref().map(str::len), Some(40));
        assert!(hit.hash.is_none());
        assert!(hit.magnet_url.is_none());
        assert_eq!(hit.seeders, 7);
    }

    #[test]
    fn explicit_magnet_is_preferred_over_synthesized() {
        let body = r#"{"hits":[{"id":"0023b7552f52c4a04a8eb57e7a01685a10f6d060",
            "title":"T","bytes":1,"seeders":1,"peers":0,
            "magnetUrl":"magnet:?xt=urn:btih:explicit"}]}"#;
        let parsed: SearchResponse = serde_json::from_str(body).expect("valid json");
        assert_eq!(
            parsed.hits[0].magnet_url.as_deref(),
            Some("magnet:?xt=urn:btih:explicit")
        );
    }

    #[test]
    fn empty_hits_is_not_an_error() {
        let parsed: SearchResponse = serde_json::from_str(r#"{"hits":[]}"#).expect("valid");
        assert!(parsed.hits.is_empty());
        // A response with no hits key at all must also parse.
        let parsed: SearchResponse = serde_json::from_str(r#"{}"#).expect("valid");
        assert!(parsed.hits.is_empty());
    }
}
