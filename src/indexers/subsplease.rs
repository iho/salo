//! A real, working indexer for SubsPlease -- an anime release group whose
//! torrents are also on public trackers. Public, no account, and its
//! `/api/` JSON endpoint is intended to be queried directly, so this one
//! is verifiable end to end without credentials.
//!
//! The API is shaped as an object keyed by `"<Show> - <Episode>"`, each
//! with a list of downloads (one per resolution), and each download
//! carries a full magnet. Two quirks from the upstream definition, both
//! confirmed against the live API:
//!
//! - A search with no matches returns an empty body or `[]` rather than an
//!   object, so both are treated as "no results" instead of a parse error.
//! - The API doesn't report file size. The magnet's `xl=` parameter holds
//!   it when present; otherwise the size is estimated from resolution.
//!
//! Seeders/leechers aren't published by this API at all -- upstream
//! reports a flat 1/2 for every release, and this does the same rather
//! than inventing numbers.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use regex::Regex;
use serde::Deserialize;

use super::Release;

pub const NAME: &str = "subsplease";

const API_URL: &str = "https://subsplease.org/api/";

static SIZE_IN_MAGNET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)&xl=(?P<size>\d+)").expect("valid regex"));

/// `[SubsPlease]` and a stray `?` before it are stripped from the term
/// before it's sent to the API (upstream does the same).
static BRAND_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\[?SubsPlease\]?\s*").expect("valid regex"));

/// A resolution in the search term (`1080p`) is dropped before querying:
/// the API matches on show/episode, and leaving it in finds nothing.
static RESOLUTION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\d{3,4}p").expect("valid regex"));

#[derive(Deserialize)]
struct ReleaseInfo {
    show: String,
    episode: String,
    #[serde(default)]
    downloads: Vec<Download>,
}

#[derive(Deserialize)]
struct Download {
    /// `"480"`, `"720"`, `"1080"`.
    #[serde(default)]
    res: String,
    #[serde(default)]
    magnet: String,
}

pub async fn search(client: &reqwest::Client, query: &str) -> Result<Vec<Release>> {
    let without_brand = BRAND_RE.replace_all(query, "");
    let term = RESOLUTION_RE.replace_all(&without_brand, "");
    let term = term.trim();
    if term.is_empty() {
        return Ok(Vec::new());
    }

    let body = client
        .get(API_URL)
        .query(&[("f", "search"), ("tz", "UTC"), ("s", term)])
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .with_context(|| format!("request to {NAME} failed"))?
        .error_for_status()
        .with_context(|| format!("{NAME} returned an error status"))?
        .text()
        .await
        .with_context(|| format!("failed to read {NAME} response body"))?;

    parse_results(&body)
}

fn parse_results(body: &str) -> Result<Vec<Release>> {
    let body = body.trim();
    // "no matches" comes back as an empty body or an empty array, not an
    // object -- neither is an error.
    if body.is_empty() || body == "[]" {
        return Ok(Vec::new());
    }

    let parsed: HashMap<String, ReleaseInfo> = serde_json::from_str(body)
        .with_context(|| format!("failed to parse {NAME} response"))?;

    let mut releases = Vec::new();
    for info in parsed.values() {
        for download in &info.downloads {
            if download.magnet.is_empty() {
                continue;
            }
            releases.push(Release {
                indexer: NAME,
                // Ex: [SubsPlease] Shingeki no Kyojin (The Final Season) - 64 (1080p)
                title: format!(
                    "[SubsPlease] {} - {} ({}p)",
                    info.show, info.episode, download.res
                ),
                // This API publishes no swarm stats for its releases.
                seeders: 1,
                leechers: 1,
                size: release_size(&download.magnet, &download.res),
                magnet: download.magnet.clone(),
                source_url: Some(format!("https://subsplease.org/shows/{}/", info.show)),
                comments: None,
            });
        }
    }
    Ok(releases)
}

/// The magnet's `xl=` when the API included it, else an estimate by
/// resolution (mirroring the upstream definition's fallbacks).
fn release_size(magnet: &str, resolution: &str) -> String {
    if let Some(caps) = SIZE_IN_MAGNET_RE.captures(magnet)
        && let Ok(bytes) = caps["size"].parse::<u64>()
        && bytes > 0
    {
        return super::format_size(bytes);
    }

    let megabytes: u64 = match resolution {
        "1080" => 1_300,
        "720" => 700,
        "480" => 350,
        _ => 1_024,
    };
    super::format_size(megabytes * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_object_keyed_by_show_and_episode() {
        let body = r#"{"One Piece - 1178":{
            "show":"One Piece","episode":"1178",
            "downloads":[{"res":"480","magnet":"magnet:?xt=urn:btih:AAA&xl=382467922"}]}}"#;
        let releases = parse_results(body).expect("parses");
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].title, "[SubsPlease] One Piece - 1178 (480p)");
        // Size comes from the magnet's xl= parameter (382467922 bytes).
        assert_eq!(releases[0].size, "364.7 MB");
    }

    #[test]
    fn empty_body_and_empty_array_are_not_errors() {
        assert!(parse_results("").expect("ok").is_empty());
        assert!(parse_results("[]").expect("ok").is_empty());
        assert!(parse_results("  []  ").expect("ok").is_empty());
    }

    #[test]
    fn size_falls_back_to_resolution_estimate() {
        // No xl= in the magnet -> estimate.
        assert_eq!(release_size("magnet:?xt=urn:btih:AAA", "1080"), "1.3 GB");
        assert_eq!(release_size("magnet:?xt=urn:btih:AAA", "720"), "700.0 MB");
        assert_eq!(release_size("magnet:?xt=urn:btih:AAA", "480"), "350.0 MB");
    }

    #[test]
    fn brand_and_resolution_are_stripped_from_the_term() {
        // What the request generator sends: neither the brand nor a
        // resolution should survive into the query string.
        let without_brand = BRAND_RE.replace_all("[SubsPlease] One Piece 1080p", "");
        let cleaned = RESOLUTION_RE.replace_all(&without_brand, "");
        assert_eq!(cleaned.trim(), "One Piece");
    }

    #[test]
    fn multiple_resolutions_produce_multiple_releases() {
        let body = r#"{"Show - 01":{"show":"Show","episode":"01","downloads":[
            {"res":"1080","magnet":"magnet:?xt=urn:btih:AAA"},
            {"res":"720","magnet":"magnet:?xt=urn:btih:BBB"}]}}"#;
        let releases = parse_results(body).expect("parses");
        assert_eq!(releases.len(), 2);
        assert!(releases.iter().any(|r| r.title.ends_with("(1080p)")));
        assert!(releases.iter().any(|r| r.title.ends_with("(720p)")));
    }
}
