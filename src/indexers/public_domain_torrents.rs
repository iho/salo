//! A real indexer for publicdomaintorrents.info -- a tracker dedicated
//! specifically to films confirmed to be in the US public domain,
//! distributed "for free using BitTorrent technology" per the site's own
//! description. Chosen (like `linuxtracker` and `academic_torrents`) so
//! this indexer only ever surfaces legally distributable content -- and,
//! unlike the other two, one with actual video/audio to exercise the
//! Watch/Listen player against real media instead of ISOs and datasets.
//!
//! The site publishes an RSS feed (`bt/rss.php`) rather than requiring a
//! scrape of its browse pages, same courteous shape as academictorrents'
//! `database.xml`. Each item names a specific file (usually the original
//! `.avi` transfer) but titles this old don't have modern codecs, so
//! browsers won't play them inline -- every title also has a `.mp4`
//! ("IPOD MP4") torrent at a predictable sibling URL, which is what gets
//! used here so the in-browser player actually works.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use regex::Regex;
use scraper::{Html, Selector};
use tokio::sync::Mutex;

use super::Release;

pub const NAME: &str = "publicdomaintorrents";

const RSS_URL: &str = "https://www.publicdomaintorrents.info/bt/rss.php";
// The feed is a small, hand-maintained catalog, not a live swarm-stats
// feed -- an hour between refetches is already more than it needs.
const CACHE_TTL: Duration = Duration::from_secs(3600);

static ITEM_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("item").unwrap());
static TITLE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("title").unwrap());
static LINK_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("link").unwrap());
static DESC_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("description").unwrap());

// The <description> text looks like "Filename: Foo.avi <br />\nUploaders:
// 0<br />\nDownloaders: 0<br />\nSize: 12.34MB" (a literal "<br />", not a
// nested tag -- RSS escapes it so feed readers render it as one).
static FILENAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Filename:\s*(\S+)\.(\w+)").expect("valid regex"));
static SIZE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"Size:\s*([\d.]+)\s*MB").expect("valid regex"));

#[derive(Clone)]
struct Entry {
    display_title: String,
    base_name: String,
    detail_url: Option<String>,
    size_bytes: u64,
}

static CACHE: Mutex<Option<(Instant, Vec<Entry>)>> = Mutex::const_new(None);

pub async fn search(client: &reqwest::Client, query: &str) -> Result<Vec<Release>> {
    let entries = cached_entries(client).await?;
    let needle = query.to_lowercase();

    Ok(entries
        .iter()
        .filter(|e| e.display_title.to_lowercase().contains(&needle))
        .map(|e| Release {
            indexer: NAME,
            title: e.display_title.clone(),
            // The feed carries no live swarm stats.
            seeders: 0,
            leechers: 0,
            size: if e.size_bytes > 0 {
                super::format_size(e.size_bytes)
            } else {
                "unknown".to_string()
            },
            // A direct .torrent file URL, not a magnet -- librqbit's
            // `AddTorrent::from_url` accepts either.
            magnet: format!(
                "https://www.publicdomaintorrents.com/bt/btdownload.php?type=torrent&file={}.mp4.torrent",
                e.base_name
            ),
            source_url: e.detail_url.clone(),
            comments: None,
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

    let body = client
        .get(RSS_URL)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .with_context(|| format!("request to {NAME} failed"))?
        .error_for_status()
        .with_context(|| format!("{NAME} returned an error status"))?
        .text()
        .await
        .with_context(|| format!("failed to read {NAME} response body"))?;

    let entries = parse_feed(&body)?;
    *cache = Some((Instant::now(), entries.clone()));
    Ok(entries)
}

fn parse_feed(body: &str) -> Result<Vec<Entry>> {
    let document = Html::parse_document(body);

    let entries = document
        .select(&ITEM_SEL)
        .filter_map(|item| {
            let title = item
                .select(&TITLE_SEL)
                .next()
                .map(|el| el.text().collect::<String>().trim().to_string())
                .filter(|t| !t.is_empty())?;

            let description = item
                .select(&DESC_SEL)
                .next()
                .map(|el| el.text().collect::<String>())
                .unwrap_or_default();

            // The filename in the description is the source of truth for
            // what to ask the tracker for -- the <title> is just a label
            // and may not match a real file's base name.
            let base_name = FILENAME_RE
                .captures(&description)
                .map(|c| c[1].to_string())
                .unwrap_or_else(|| title.clone());

            let size_bytes = SIZE_RE
                .captures(&description)
                .and_then(|c| c[1].parse::<f64>().ok())
                .map(|mb| (mb * 1024.0 * 1024.0) as u64)
                .unwrap_or(0);

            let detail_url = item
                .select(&LINK_SEL)
                .next()
                .map(|el| el.text().collect::<String>().trim().to_string())
                .filter(|u| !u.is_empty());

            Some(Entry {
                display_title: title.replace('_', " "),
                base_name,
                detail_url,
                size_bytes,
            })
        })
        .collect();

    Ok(entries)
}
