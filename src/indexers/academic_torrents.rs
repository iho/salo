//! A real indexer for academictorrents.com -- a tracker dedicated to
//! research datasets, course material, and other academic/scientific
//! works distributed with the rightsholder's consent.
//!
//! The site's own `/checkb.htm` anti-scraper page asks bots not to hit
//! `/browse.php` and instead points at `database.xml`, a full dump of
//! every torrent that refreshes about once a day:
//!
//!   "This generates a lot of traffic for us so we instead ask you to
//!    search an XML file (in RSS format). These XML files are cached
//!    so you can request them as many times as you want."
//!
//! So that's what this does: fetch the dump, cache it in memory for an
//! hour (far more often than the site itself refreshes it), and filter
//! it locally per search instead of hitting their server every time.
//! It's plain RSS/XML with no attributes or namespaced tags, so
//! `scraper`'s HTML parser reads it just fine -- no separate XML crate
//! needed for one feed shaped this simply.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use scraper::{Html, Selector};
use tokio::sync::Mutex;

use super::Release;

pub const NAME: &str = "academictorrents";

const DATABASE_URL: &str = "https://academictorrents.com/database.xml";
// The feed itself only updates ~daily; re-fetching hourly is already far
// more often than that and keeps this from ever hammering their server.
const CACHE_TTL: Duration = Duration::from_secs(3600);

#[derive(Clone)]
struct Entry {
    title: String,
    info_hash: String,
    size_bytes: u64,
}

static CACHE: Mutex<Option<(Instant, Vec<Entry>)>> = Mutex::const_new(None);

static ITEM_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("item").unwrap());
static TITLE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("title").unwrap());
static HASH_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("infohash").unwrap());
static SIZE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("size").unwrap());

pub async fn search(client: &reqwest::Client, query: &str) -> Result<Vec<Release>> {
    let entries = cached_entries(client).await?;
    let needle = query.to_lowercase();

    Ok(entries
        .iter()
        .filter(|e| e.title.to_lowercase().contains(&needle))
        .map(|e| Release {
            indexer: NAME,
            title: e.title.clone(),
            // The dump has no live swarm stats, only catalog metadata.
            seeders: 0,
            leechers: 0,
            size: super::format_size(e.size_bytes),
            magnet: format!(
                "magnet:?xt=urn:btih:{}&dn={}",
                e.info_hash,
                utf8_percent_encode(&e.title, NON_ALPHANUMERIC)
            ),
            // Deterministic: the feed's own <link>/<guid> for each item is
            // exactly this shape.
            source_url: Some(format!("https://academictorrents.com/details/{}", e.info_hash)),
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
        .get(DATABASE_URL)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("request to {NAME} failed"))?
        .error_for_status()
        .with_context(|| format!("{NAME} returned an error status"))?
        .text()
        .await
        .with_context(|| format!("failed to read {NAME} response body"))?;

    let entries = parse_database(&body)?;
    *cache = Some((Instant::now(), entries.clone()));
    Ok(entries)
}

fn parse_database(body: &str) -> Result<Vec<Entry>> {
    let document = Html::parse_document(body);

    let entries = document
        .select(&ITEM_SEL)
        .filter_map(|item| {
            let title = item
                .select(&TITLE_SEL)
                .next()
                .map(|el| el.text().collect::<String>().trim().to_string())
                .filter(|t| !t.is_empty())?;
            let info_hash = item
                .select(&HASH_SEL)
                .next()
                .map(|el| el.text().collect::<String>().trim().to_lowercase())
                .filter(|h| h.len() == 40)?;
            let size_bytes = item
                .select(&SIZE_SEL)
                .next()
                .and_then(|el| el.text().collect::<String>().trim().parse().ok())
                .unwrap_or(0);
            Some(Entry {
                title,
                info_hash,
                size_bytes,
            })
        })
        .collect();

    Ok(entries)
}
