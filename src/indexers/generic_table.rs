//! One embedded indexer definition: parses a generic HTML results table.
//!
//! Real tracker markup varies per site the way Prowlarr/Jackett's Cardigann
//! definitions vary per site -- this module is the Rust equivalent of one
//! such definition: a fixed target plus the selectors/regex needed to pull
//! releases out of its HTML, compiled directly into the binary instead of
//! interpreted from a config file at runtime.
//!
//! Swap `BASE_URL` and the selectors in `RowSelectors` for a real site's
//! shape to turn this into a working indexer.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use regex::Regex;
use scraper::{Html, Selector};

use super::Release;

pub const NAME: &str = "generic-table";

/// Placeholder target -- there is no default tracker baked into this
/// binary. Point this at the search endpoint of the site this indexer is
/// written for.
const BASE_URL: &str = "https://example-indexer.invalid/search";

// Matches a bare magnet URI anywhere in a chunk of HTML (attribute value or
// inline text), stopping at whitespace or a quote/tag delimiter. Used as a
// fallback when the magnet link isn't cleanly reachable via CSS selectors
// (some trackers stash it in a data-* attribute or JS onclick handler).
static MAGNET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"magnet:\?[^"'<>\s]+"#).expect("valid regex"));

/// Selectors for this indexer's HTML table: one `<tr>` per release inside
/// a results table.
struct RowSelectors {
    row: Selector,
    title: Selector,
    seeders: Selector,
    leechers: Selector,
    size: Selector,
    magnet_link: Selector,
    any_descendant: Selector,
}

impl RowSelectors {
    fn new() -> Result<Self> {
        Ok(Self {
            row: parse_selector("table.torrent-list tbody tr")?,
            title: parse_selector("td.title a")?,
            seeders: parse_selector("td.seeders")?,
            leechers: parse_selector("td.leechers")?,
            size: parse_selector("td.size")?,
            magnet_link: parse_selector("a[href^=\"magnet:\"]")?,
            any_descendant: parse_selector("*")?,
        })
    }
}

fn parse_selector(css: &str) -> Result<Selector> {
    Selector::parse(css).map_err(|e| anyhow::anyhow!("invalid selector {css:?}: {e}"))
}

/// Fetch and parse this indexer's search results for `query`.
///
/// No external indexing API is involved: this sends the HTTP request to
/// the tracker itself and parses the returned HTML directly.
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

    parse_results(&body, &url)
}

fn parse_results(body: &str, page_url: &reqwest::Url) -> Result<Vec<Release>> {
    let selectors = RowSelectors::new()?;
    let document = Html::parse_document(body);

    let mut releases = Vec::new();
    for row in document.select(&selectors.row) {
        let title_el = row.select(&selectors.title).next();
        let title = title_el.map(|el| el.text().collect::<String>().trim().to_string());
        let Some(title) = title.filter(|t| !t.is_empty()) else {
            // Row didn't match the expected shape (e.g. an ad or header
            // row) -- the site's DOM inevitably drifts, so skip rather
            // than fail the whole search.
            continue;
        };
        let source_url = title_el
            .and_then(|el| el.value().attr("href"))
            .and_then(|href| page_url.join(href).ok())
            .map(|u| u.to_string());

        let magnet = row
            .select(&selectors.magnet_link)
            .next()
            .and_then(|el| el.value().attr("href"))
            .map(str::to_string)
            .or_else(|| find_magnet_in_attrs(&row, &selectors.any_descendant));

        let Some(magnet) = magnet else {
            continue;
        };

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
            source_url,
        });
    }

    Ok(releases)
}

/// Fallback magnet extraction for markup where it's not a clean `<a href>`
/// (e.g. stashed in a `data-*` attribute or an inline `onclick` handler).
/// Scans already-decoded attribute values -- never the re-serialized
/// `.html()` of the row, which would double-escape `&` inside the magnet's
/// query string and corrupt it.
fn find_magnet_in_attrs(row: &scraper::ElementRef, any_descendant: &Selector) -> Option<String> {
    let attr_values = |el: scraper::ElementRef| {
        el.value()
            .attrs()
            .filter_map(|(_, v)| MAGNET_RE.find(v).map(|m| m.as_str().to_string()))
            .next()
    };
    attr_values(*row).or_else(|| row.select(any_descendant).find_map(attr_values))
}

fn extract_number(row: &scraper::ElementRef, selector: &Selector) -> u32 {
    row.select(selector)
        .next()
        .map(|el| el.text().collect::<String>())
        .and_then(|text| text.trim().replace(',', "").parse().ok())
        .unwrap_or(0)
}
