//! Indexers: native Rust code that knows how to search and parse a
//! particular tracker's results page, compiled directly into this binary.
//!
//! This is deliberately not a runtime-configured "point me at an indexer
//! service" layer -- there is no external proxy, no YAML definition loaded
//! at startup, no companion process. Each indexer is a plain Rust module
//! that owns its own target and its own parsing logic (selectors, regex),
//! the same knowledge Prowlarr/Jackett express as an interpreted per-site
//! definition, just reimplemented here as compiled code. Adding a new site
//! means adding a new module and a new `Registered` variant below.

mod generic_table;
mod linuxtracker;

use anyhow::Result;
use serde::Serialize;

/// A single parsed release row from an indexer's search results page.
#[derive(Debug, Clone, Serialize)]
pub struct Release {
    pub indexer: &'static str,
    pub title: String,
    pub seeders: u32,
    pub leechers: u32,
    pub size: String,
    pub magnet: String,
}

/// Every indexer compiled into this binary. Static dispatch (a plain enum)
/// rather than `dyn Trait` -- an `async fn` in a trait isn't object-safe
/// without boxing every call, and pulling in a helper crate for that would
/// be exactly the kind of external dependency this is meant to avoid.
enum Registered {
    GenericTable,
    LinuxTracker,
}

impl Registered {
    const ALL: &'static [Registered] = &[Registered::GenericTable, Registered::LinuxTracker];

    fn name(&self) -> &'static str {
        match self {
            Self::GenericTable => generic_table::NAME,
            Self::LinuxTracker => linuxtracker::NAME,
        }
    }

    async fn search(&self, client: &reqwest::Client, query: &str) -> Result<Vec<Release>> {
        match self {
            Self::GenericTable => generic_table::search(client, query).await,
            Self::LinuxTracker => linuxtracker::search(client, query).await,
        }
    }
}

/// Query every embedded indexer concurrently and merge the results.
///
/// One indexer being unreachable or returning unparseable HTML (sites
/// change their markup) doesn't fail the whole search -- it's logged and
/// skipped, same as a multi-indexer aggregator would treat a dead site.
pub async fn search_all(client: &reqwest::Client, query: &str) -> Vec<Release> {
    let results = futures_util::future::join_all(
        Registered::ALL
            .iter()
            .map(|indexer| async move { (indexer.name(), indexer.search(client, query).await) }),
    )
    .await;

    let mut releases = Vec::new();
    for (name, result) in results {
        match result {
            Ok(mut found) => releases.append(&mut found),
            Err(err) => tracing::warn!(indexer = name, error = ?err, "indexer search failed"),
        }
    }

    // Best releases first: more seeders means faster, more reliable
    // sequential streaming.
    releases.sort_by_key(|r| std::cmp::Reverse(r.seeders));
    releases
}
