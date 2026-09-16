//! A real, working indexer definition for linuxtracker.org -- a public
//! tracker dedicated to Linux/BSD distributions and other freely
//! redistributable software, chosen specifically so this indexer only
//! ever surfaces legally distributable releases.
//!
//! Its search results page doesn't print a magnet link directly; each
//! release's title links to a detail page at `/torrents/<info_hash>/`,
//! where the path segment *is* the BitTorrent v1 info hash. We regex the
//! hash straight out of that href and synthesize the magnet ourselves,
//! saving a second HTTP request per row.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use regex::Regex;
use scraper::{Html, Selector};

use super::Release;

pub const NAME: &str = "linuxtracker";

const BASE_URL: &str = "https://linuxtracker.org/torrents/";

// A torrent detail link's path is exactly `/torrents/<40 hex chars>/`.
static INFO_HASH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/torrents/([0-9a-fA-F]{40})/").expect("valid regex"));

struct RowSelectors {
    row: Selector,
    title_link: Selector,
    size: Selector,
    seeders: Selector,
    leechers: Selector,
}

impl RowSelectors {
    fn new() -> Result<Self> {
        Ok(Self {
            row: parse_selector("table.torrent-table tbody tr")?,
            title_link: parse_selector("td.torrent-name-cell a.torrent-name")?,
            size: parse_selector("td.numeric.muted-cell")?,
            seeders: parse_selector("td.seeds strong")?,
            leechers: parse_selector("td.leeches strong")?,
        })
    }
}

fn parse_selector(css: &str) -> Result<Selector> {
    Selector::parse(css).map_err(|e| anyhow::anyhow!("invalid selector {css:?}: {e}"))
}

pub async fn search(client: &reqwest::Client, query: &str) -> Result<Vec<Release>> {
    let url = reqwest::Url::parse_with_params(BASE_URL, &[("q", query)])
        .context("failed to build search URL")?;

    let body = client
        .get(url.clone())
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .with_context(|| format!("request to {NAME} failed: {url}"))?
        .error_for_status()
        .with_context(|| format!("{NAME} returned an error status: {url}"))?
        .text()
        .await
        .with_context(|| format!("failed to read {NAME} response body"))?;

    parse_results(&body)
}

fn parse_results(body: &str) -> Result<Vec<Release>> {
    let selectors = RowSelectors::new()?;
    let document = Html::parse_document(body);

    let mut releases = Vec::new();
    for row in document.select(&selectors.row) {
        let Some(link) = row.select(&selectors.title_link).next() else {
            continue;
        };
        let title = link.text().collect::<String>().trim().to_string();
        if title.is_empty() {
            continue;
        }

        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let Some(hash) = INFO_HASH_RE.captures(href).map(|c| c[1].to_lowercase()) else {
            // The site's DOM inevitably drifts -- skip rows that don't
            // match the expected link shape rather than failing the
            // whole search.
            continue;
        };

        let dn = utf8_percent_encode(&title, NON_ALPHANUMERIC);
        let magnet = format!("magnet:?xt=urn:btih:{hash}&dn={dn}");

        let seeders = extract_number(&row, &selectors.seeders);
        let leechers = extract_number(&row, &selectors.leechers);
        let size = row
            .select(&selectors.size)
            .next()
            .map(|el| el.text().collect::<String>().trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        releases.push(Release {
            indexer: NAME,
            title,
            seeders,
            leechers,
            size,
            magnet,
        });
    }

    Ok(releases)
}

fn extract_number(row: &scraper::ElementRef, selector: &Selector) -> u32 {
    row.select(selector)
        .next()
        .map(|el| el.text().collect::<String>())
        .and_then(|text| text.trim().replace(',', "").parse().ok())
        .unwrap_or(0)
}
