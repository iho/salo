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

mod academic_torrents;
mod generic_table;
mod linuxtracker;
mod public_domain_torrents;
mod toloka;

use anyhow::Result;
use serde::Serialize;

use crate::config_store::ConfigStore;

/// A single parsed release row from an indexer's search results page.
#[derive(Debug, Clone, Serialize)]
pub struct Release {
    pub indexer: &'static str,
    pub title: String,
    pub seeders: u32,
    pub leechers: u32,
    pub size: String,
    pub magnet: String,
    /// The release's own page on the indexer's site, when the indexer
    /// exposes one -- carried through to the torrent detail page as
    /// "view on `<indexer>`", separate from the magnet/swarm itself.
    pub source_url: Option<String>,
}

/// Every indexer compiled into this binary. Static dispatch (a plain enum)
/// rather than `dyn Trait` -- an `async fn` in a trait isn't object-safe
/// without boxing every call, and pulling in a helper crate for that would
/// be exactly the kind of external dependency this is meant to avoid.
enum Registered {
    GenericTable,
    LinuxTracker,
    AcademicTorrents,
    PublicDomainTorrents,
    Toloka,
}

impl Registered {
    const ALL: &'static [Registered] = &[
        Registered::GenericTable,
        Registered::LinuxTracker,
        Registered::AcademicTorrents,
        Registered::PublicDomainTorrents,
        Registered::Toloka,
    ];

    fn name(&self) -> &'static str {
        match self {
            Self::GenericTable => generic_table::NAME,
            Self::LinuxTracker => linuxtracker::NAME,
            Self::AcademicTorrents => academic_torrents::NAME,
            Self::PublicDomainTorrents => public_domain_torrents::NAME,
            Self::Toloka => toloka::NAME,
        }
    }

    async fn search(
        &self,
        client: &reqwest::Client,
        config: &ConfigStore,
        query: &str,
    ) -> Result<Vec<Release>> {
        match self {
            Self::GenericTable => generic_table::search(client, query).await,
            Self::LinuxTracker => linuxtracker::search(client, query).await,
            Self::AcademicTorrents => academic_torrents::search(client, query).await,
            Self::PublicDomainTorrents => public_domain_torrents::search(client, query).await,
            // Toloka is login-walled: it reads its credentials/session
            // cookie and its Freeleech-only / strip-Cyrillic toggles from
            // the per-indexer settings store (see `toloka::search`'s doc
            // comment for the keys it uses).
            Self::Toloka => toloka::search(client, config, query).await,
        }
    }
}

/// Names of every indexer compiled into this binary, for populating a
/// "search this tracker" selector -- there's no registry to query at
/// runtime, this is just the fixed list of what got built in.
pub fn names() -> Vec<&'static str> {
    Registered::ALL.iter().map(Registered::name).collect()
}

/// Query the embedded indexers concurrently and merge the results.
/// `only` restricts the search to a single named indexer (used when the
/// user picks one tracker instead of "all") so we don't pay for requests
/// to sites the result set will just filter back out.
///
/// One indexer being unreachable or returning unparseable HTML (sites
/// change their markup) doesn't fail the whole search -- it's logged and
/// skipped, same as a multi-indexer aggregator would treat a dead site.
pub async fn search_all(
    client: &reqwest::Client,
    config: &ConfigStore,
    query: &str,
    only: Option<&str>,
) -> Vec<Release> {
    let targets: Vec<&Registered> = Registered::ALL
        .iter()
        .filter(|i| only.is_none_or(|name| i.name() == name))
        .collect();

    let results = futures_util::future::join_all(
        targets.iter().map(|indexer| async move {
            (indexer.name(), indexer.search(client, config, query).await)
        }),
    )
    .await;

    let mut releases = Vec::new();
    for (name, result) in results {
        match result {
            Ok(mut found) => releases.append(&mut found),
            Err(err) => tracing::warn!(indexer = name, error = ?err, "indexer search failed"),
        }
    }
    releases
}

/// Best-effort size parse (`"4.7 GB"`, `"650 MiB"`, ...) for sorting by
/// size across indexers that each format it as free-text. Unparseable
/// sizes sort as zero rather than failing the request.
pub fn size_bytes(size: &str) -> f64 {
    let (num, unit) = size
        .trim()
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != ',')
        .map_or((size.trim(), ""), |i| size.trim().split_at(i));

    let num: f64 = match num.replace(',', "").parse() {
        Ok(n) => n,
        Err(_) => return 0.0,
    };

    let mult = match unit.trim().to_uppercase().as_str() {
        "B" | "" => 1.0,
        "KB" | "KIB" => 1024.0,
        "MB" | "MIB" => 1024.0f64.powi(2),
        "GB" | "GIB" => 1024.0f64.powi(3),
        "TB" | "TIB" => 1024.0f64.powi(4),
        _ => 1.0,
    };
    num * mult
}

/// Formats a byte count as a human-readable size (`"4.7 GB"`), the inverse
/// of [`size_bytes`] -- shared so every indexer/route that has a raw byte
/// count displays it the same way.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}
