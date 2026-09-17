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
    /// Which tracker knaben found this on (`thepiratebay`, `rutracker`, …).
    #[serde(default, rename = "trackerId")]
    tracker_id: Option<String>,
}

/// Rewrites knaben's own links into ones the user can actually open.
///
/// For The Pirate Bay, knaben returns a link to its OWN proxy page
/// (`https://knaben.xyz/thepiratebay/description.php?id=…`), which is
/// protected by a "go-away" bot challenge and fails even in a real
/// browser (confirmed: "access denied: error in challenge meta-refresh").
/// The `id` in that URL is The Pirate Bay's own torrent id, so the real
/// page is reconstructible -- and TPB resolves an id-only URL fine.
///
/// Everything else knaben returns is already the origin site's own URL
/// (1337x.to, rutracker.org, yts.gg), which is exactly what a "view on
/// the original tracker" link should be -- left untouched. Those can
/// still 403 or challenge for other reasons (rutracker's Cloudflare), but
/// that's the site's own doing, not a wrong URL.
fn origin_url(hit: &Hit) -> Option<String> {
    let details = hit.details.as_deref().filter(|d| !d.is_empty())?;

    if hit.tracker_id.as_deref() == Some("thepiratebay")
        && let Some(id) = query_arg(details, "id")
    {
        return Some(format!("https://thepiratebay.xyz/torrent/{id}"));
    }

    Some(details.to_string())
}

/// Value of `key` in a URL's query string.
fn query_arg(url: &str, key: &str) -> Option<String> {
    url.split(['?', '&'])
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
        .filter(|v| !v.is_empty())
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
            // Both reads that borrow `h` happen before any field is moved
            // out of it below.
            let source_url = origin_url(&h);

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
                source_url,
                comments: None,
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

    fn hit_with(details: &str, tracker_id: &str) -> Hit {
        serde_json::from_str(&format!(
            r#"{{"title":"T","bytes":1,"seeders":1,"peers":0,
                 "details":"{details}","trackerId":"{tracker_id}"}}"#
        ))
        .expect("valid hit")
    }

    #[test]
    fn knabens_own_broken_proxy_link_is_rewritten_to_the_real_site() {
        // knaben's description.php is behind a bot challenge that fails
        // even in a real browser; the id is The Pirate Bay's own.
        let hit = hit_with(
            "https://knaben.xyz/thepiratebay/description.php?id=84437677",
            "thepiratebay",
        );
        assert_eq!(
            origin_url(&hit).as_deref(),
            Some("https://thepiratebay.xyz/torrent/84437677")
        );
    }

    #[test]
    fn origin_site_links_pass_through_untouched() {
        // These already ARE the origin site's URL -- rewriting them would
        // be wrong.
        for (url, tid) in [
            (
                "https://1337x.to/torrent/6721046/Django-Unchained-2012/",
                "1337x",
            ),
            ("https://rutracker.org/forum/viewtopic.php?t=6878008", "rutracker"),
            ("https://yts.gg/movies/django-il-bastardo-1969", "yts"),
        ] {
            let hit = hit_with(url, tid);
            assert_eq!(origin_url(&hit).as_deref(), Some(url));
        }
    }

    #[test]
    fn a_tpb_link_without_an_id_is_left_alone() {
        // Don't invent a URL when the id can't be extracted.
        let hit = hit_with("https://knaben.xyz/thepiratebay/description.php", "thepiratebay");
        assert_eq!(
            origin_url(&hit).as_deref(),
            Some("https://knaben.xyz/thepiratebay/description.php")
        );
    }

    #[test]
    fn missing_details_yields_no_source_url() {
        let hit: Hit = serde_json::from_str(r#"{"title":"T","seeders":1}"#).expect("valid");
        assert_eq!(origin_url(&hit), None);
    }
}
