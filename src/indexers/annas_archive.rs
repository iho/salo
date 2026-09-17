//! A real, working indexer for Anna's Archive -- the "Torrents" section,
//! which publishes the project's own preservation torrents.
//!
//! IMPORTANT, verified live: Anna's Archive's free-text `/search` is
//! behind a DDoS-Guard JavaScript challenge, so it cannot be driven over
//! plain HTTP at all. The `/torrents` listing pages, however, are served
//! as ordinary HTML with the magnet already in each row -- and those are
//! what this reads. The listing is small (one page per collection, every
//! row shipped in the initial HTML), so rather than pretending to search,
//! this fetches the collections' listing pages, caches them, and filters
//! the rows locally by title. That is a "browse their archive and match
//! titles", not a full-text index search.
//!
//! Category filtering is deliberately not exposed: the whole point of
//! `/torrents` is the archive's own curated collections.
//!
//! Row layout (confirmed against the live page, one `<td>` each):
//!   0 flags | 1 name (with .torrent and magnet links) | 2 date
//!   3 size/count | 4 type | 5 swarm ("0 seed / 6 leech")

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use scraper::{Html, Selector};
use tokio::sync::Mutex;

use super::Release;

pub const NAME: &str = "annasarchive";

const BASE_URL: &str = "https://annas-archive.pk";
/// The archive's own preservation collections, each a single listing page.
const COLLECTIONS: &[&str] = &[
    "aa_magazines",
    "aa_misc_data",
    "aa_derived_mirror_metadata",
    "libgen_li_fic",
    "libgen_li_non_fic",
    "libgen_li_comics",
    "libgen_li_magazines",
    "libgen_li_standarts",
    "scihub",
    "zlib",
    "magzdb",
    "wikilib",
    "upload",
];

/// Listings change slowly (they're static archive dumps), and each page is
/// a few hundred KB, so a shared cache keeps a search from re-downloading
/// every collection on every query.
const CACHE_TTL: Duration = Duration::from_secs(1800);

#[derive(Clone)]
struct Entry {
    title: String,
    magnet: String,
    size: String,
    seeders: u32,
    leechers: u32,
    source_url: String,
}

static CACHE: Mutex<Option<(Instant, Vec<Entry>)>> = Mutex::const_new(None);

static ROW_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("tr").expect("valid selector"));
static LINK_SEL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("td:nth-child(2) a").expect("valid selector")
});
static MAGNET_SEL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("td:nth-child(2) a[href^='magnet:']").expect("valid selector")
});
/// td 3 is the date, 4 is "size / file count", 5 is the type, 6 is swarm.
static SIZE_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(4)").expect("valid selector"));
static SWARM_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(6)").expect("valid selector"));
/// The swarm cell reads like `🔴 0 seed / 6 leech —` (or `🟢 12 seed / 3 leech`).
static SEED_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?P<seeds>\d+)\s*seed\s*/\s*(?P<leech>\d+)\s*leech").expect("valid regex")
});

pub async fn search(client: &reqwest::Client, query: &str) -> Result<Vec<Release>> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Ok(Vec::new());
    }

    let entries = cached_entries(client).await?;

    Ok(entries
        .iter()
        .filter(|e| e.title.to_lowercase().contains(&needle))
        .map(|e| Release {
            indexer: NAME,
            title: e.title.clone(),
            seeders: e.seeders,
            leechers: e.leechers,
            size: e.size.clone(),
            magnet: e.magnet.clone(),
            source_url: Some(e.source_url.clone()),
        })
        .collect())
}

async fn cached_entries(client: &reqwest::Client) -> Result<Vec<Entry>> {
    let mut cache = CACHE.lock().await;
    if let Some((fetched_at, entries)) = cache.as_ref()
        && fetched_at.elapsed() < CACHE_TTL
    {
        return Ok(entries.clone());
    }

    let mut entries = Vec::new();
    let mut failures = Vec::new();
    for collection in COLLECTIONS {
        let url = format!("{BASE_URL}/torrents/{collection}");
        match fetch_collection(client, &url).await {
            Ok(mut found) => entries.append(&mut found),
            // One collection being down or renamed shouldn't sink the
            // whole indexer -- keep what worked, note what didn't.
            Err(err) => failures.push(format!("{collection}: {err:#}")),
        }
    }

    if entries.is_empty() {
        anyhow::bail!(
            "no Anna's Archive collections could be read ({})",
            failures.join("; ")
        );
    }
    if !failures.is_empty() {
        tracing::warn!(indexer = NAME, failures = %failures.join("; "), "some collections unavailable");
    }

    *cache = Some((Instant::now(), entries.clone()));
    Ok(entries)
}

async fn fetch_collection(client: &reqwest::Client, url: &str) -> Result<Vec<Entry>> {
    let body = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("request failed: {url}"))?
        .error_for_status()
        .with_context(|| format!("error status: {url}"))?
        .text()
        .await
        .with_context(|| format!("failed to read response body: {url}"))?;

    Ok(parse_collection(&body, url))
}

fn parse_collection(body: &str, page_url: &str) -> Vec<Entry> {
    let document = Html::parse_document(body);
    let mut entries = Vec::new();

    for row in document.select(&ROW_SEL) {
        let Some(magnet) = row
            .select(&MAGNET_SEL)
            .next()
            .and_then(|a| a.value().attr("href"))
            .map(str::to_string)
        else {
            continue;
        };

        // The name cell holds two links (the .torrent file and the
        // magnet); the title is the one that isn't the magnet.
        let (title, source_url) = row
            .select(&LINK_SEL)
            .filter_map(|a| {
                let href = a.value().attr("href")?;
                if href.starts_with("magnet:") {
                    return None;
                }
                let text = a.text().collect::<String>().trim().to_string();
                (!text.is_empty()).then_some((text, format!("{BASE_URL}{href}")))
            })
            .next()
            .unwrap_or_else(|| (page_url.to_string(), page_url.to_string()));

        let size = cell_text(&row, &SIZE_SEL).unwrap_or_else(|| "unknown".to_string());

        let swarm = cell_text(&row, &SWARM_SEL).unwrap_or_default();
        let (seeders, leechers) = SEED_RE
            .captures(&swarm)
            .map(|c| {
                (
                    c["seeds"].parse().unwrap_or(0),
                    c["leech"].parse().unwrap_or(0),
                )
            })
            .unwrap_or((0, 0));

        entries.push(Entry {
            title,
            magnet,
            size,
            seeders,
            leechers,
            source_url,
        });
    }

    entries
}

/// Text of a single cell, with the non-breaking spaces the live page uses
/// in its size column normalized.
fn cell_text(row: &scraper::ElementRef, selector: &Selector) -> Option<String> {
    row.select(selector)
        .next()
        .map(|el| {
            el.text()
                .collect::<String>()
                .replace('\u{a0}', " ")
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shaped after the live /torrents markup (magnet &amp;-escaped, size
    // cell with a non-breaking space, swarm cell with emoji).
    const SAMPLE: &str = r#"<html><body><table><tbody>
      <tr>
        <td><span title="Torrent under embargo.">🔒</span></td>
        <td><a href="/dyn/small_file/torrents/annas_archive_data__aacid__upload_files_magazines__20260412.torrent">annas_archive_data__aacid__upload_files_magazines__20260412.torrent</a><a href="magnet:?xt=urn:btih:884d33234a46f96e9a12dfcc998f2ec9aa4c33b8&amp;dn=annas_archive_data__aacid__upload_files_magazines__20260412.torrent&amp;tr=udp://tracker.opentrackr.org:1337/announce">magnet</a></td>
        <td>2026-04-06</td>
        <td>300.0&nbsp;GB / 12,681</td>
        <td>data</td>
        <td>🔴 0 seed / 6 leech —</td>
      </tr>
    </tbody></table></body></html>"#;

    #[test]
    fn parses_listing_row() {
        let entries = parse_collection(SAMPLE, "https://annas-archive.pk/torrents");
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert!(e.title.starts_with("annas_archive_data__aacid"));
        assert!(e.magnet.contains("urn:btih:884d33234a46f96e9a12dfcc998f2ec9aa4c33b8"));
        // &amp; must decode, or every tracker in the magnet is malformed.
        assert!(!e.magnet.contains("amp;"), "magnet: {}", e.magnet);
        assert_eq!(e.size, "300.0 GB / 12,681");
        assert_eq!((e.seeders, e.leechers), (0, 6));
    }

    #[test]
    fn rows_without_a_magnet_are_ignored() {
        let body = "<table><tbody><tr><td>a</td><td><a href=\"/x\">title</a></td></tr></tbody></table>";
        assert!(parse_collection(body, "u").is_empty());
    }

    #[test]
    fn swarm_cell_variants_parse() {
        let c = SEED_RE.captures("🟢 12 seed / 3 leech").expect("matches");
        assert_eq!(&c["seeds"], "12");
        assert_eq!(&c["leech"], "3");
        // An unrecognized swarm cell must not panic, just yield zeros.
        assert!(SEED_RE.captures("unknown").is_none());
    }

    #[test]
    fn title_comes_from_the_non_magnet_link() {
        let entries = parse_collection(SAMPLE, "https://annas-archive.pk/torrents");
        // Must be the .torrent filename, not the literal anchor text "magnet".
        assert_ne!(entries[0].title, "magnet");
        assert!(entries[0].source_url.contains(".torrent"));
    }
}
