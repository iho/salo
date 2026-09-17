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
mod annas_archive;
mod archive_org;
mod generic_table;
mod knaben;
mod linuxtracker;
mod piratebay;
mod pornolab;
mod public_domain_torrents;
mod rutracker;
mod subsplease;
mod toloka;
mod torrents_csv;

use anyhow::Result;
use serde::Serialize;
use std::time::Duration;

use crate::config_store::ConfigStore;

/// Per-indexer ceiling for one search. Well above any healthy site's
/// response time (the slowest live one measured ~1s cold) but well under
/// a user's patience, so a stalled or challenge-walled site degrades to
/// "that tracker contributed nothing" instead of stalling the page.
const INDEXER_TIMEOUT: Duration = Duration::from_secs(20);

/// A single declared settings field for one indexer: how the settings
/// page renders an input for it and where its value is stored.
#[derive(Debug, Clone)]
pub struct SettingField {
    /// Human label shown next to the input ("Username", "API key").
    pub label: &'static str,
    /// The storage key in the per-indexer config store -- what the
    /// indexer's own code reads back (`toloka::KEY_USERNAME`, ...).
    pub key: &'static str,
    /// Input type on the settings page: text, password (masked),
    /// or checkbox.
    pub kind: SettingFieldKind,
    /// One-line help/hint shown under the label.
    pub help: &'static str,
    /// Checked/`on` value for checkboxes when the setting is absent.
    pub default_on: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingFieldKind {
    Text,
    Password,
    Checkbox,
}

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
    ArchiveOrg,
    AnnaArchive,
    Knaben,
    PirateBay,
    PornoLab,
    RuTracker,
    SubsPlease,
    TorrentsCsv,
    Toloka,
}

impl Registered {
    const ALL: &'static [Registered] = &[
        Registered::GenericTable,
        Registered::LinuxTracker,
        Registered::AcademicTorrents,
        Registered::PublicDomainTorrents,
        Registered::ArchiveOrg,
        Registered::AnnaArchive,
        Registered::Knaben,
        Registered::PirateBay,
        Registered::PornoLab,
        Registered::RuTracker,
        Registered::SubsPlease,
        Registered::TorrentsCsv,
        Registered::Toloka,
    ];

    fn name(&self) -> &'static str {
        match self {
            Self::GenericTable => generic_table::NAME,
            Self::LinuxTracker => linuxtracker::NAME,
            Self::AcademicTorrents => academic_torrents::NAME,
            Self::PublicDomainTorrents => public_domain_torrents::NAME,
            Self::ArchiveOrg => archive_org::NAME,
            Self::AnnaArchive => annas_archive::NAME,
            Self::Knaben => knaben::NAME,
            Self::PirateBay => piratebay::NAME,
            Self::PornoLab => pornolab::NAME,
            Self::RuTracker => rutracker::NAME,
            Self::SubsPlease => subsplease::NAME,
            Self::TorrentsCsv => torrents_csv::NAME,
            Self::Toloka => toloka::NAME,
        }
    }

    /// The settings fields this indexer declares, rendered on the
    /// settings page. Public indexers declare none -- an empty list
    /// renders as "no configuration needed" rather than a raw key/value
    /// editor inviting people to invent keys nothing reads.
    fn settings_fields(&self) -> Vec<SettingField> {
        match self {
            Self::PornoLab => pornolab::settings_fields(),
            Self::RuTracker => rutracker::settings_fields(),
            Self::Toloka => toloka::settings_fields(),
            _ => Vec::new(),
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
            Self::ArchiveOrg => archive_org::search(client, query).await,
            Self::AnnaArchive => annas_archive::search(client, query).await,
            Self::Knaben => knaben::search(client, query).await,
            Self::PirateBay => piratebay::search(client, query).await,
            Self::SubsPlease => subsplease::search(client, query).await,
            Self::TorrentsCsv => torrents_csv::search(client, query).await,
            // Login-walled: these read their credentials/session cookie
            // from the per-indexer settings store.
            Self::PornoLab => pornolab::search(client, config, query).await,
            Self::RuTracker => rutracker::search(client, config, query).await,
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

/// The settings fields a named indexer declares (empty for public
/// indexers that need no configuration) -- what the settings page
/// renders labeled inputs from.
pub fn settings_fields(name: &str) -> Vec<SettingField> {
    Registered::ALL
        .iter()
        .find(|i| i.name() == name)
        .map(Registered::settings_fields)
        .unwrap_or_default()
}

/// Fetches the .torrent bytes behind one of `indexer`'s download URLs
/// when that URL needs an authenticated session (login-walled sites --
/// a plain HTTP fetch of it returns a login page, so librqbit's own
/// URL add can't work). Returns `Ok(None)` when the URL is fetchable
/// without credentials (magnet links, public trackers) or the indexer
/// is unknown.
pub async fn download_torrent(
    client: &reqwest::Client,
    config: &ConfigStore,
    indexer: &str,
    download_url: &str,
) -> Result<Option<bytes::Bytes>> {
    match indexer {
        "toloka" => Ok(Some(toloka::download_torrent(client, config, download_url).await?)),
        "pornolab" => Ok(Some(
            pornolab::download_torrent(client, config, download_url).await?,
        )),
        "rutracker" => Ok(Some(
            rutracker::download_torrent(client, config, download_url).await?,
        )),
        _ => Ok(None),
    }
}

/// Runs exactly one named indexer. Used by the search-progress job, which
/// needs each tracker's own timing and outcome rather than a merged list.
pub async fn search_one(
    client: &reqwest::Client,
    config: &ConfigStore,
    name: &str,
    query: &str,
) -> Result<Vec<Release>> {
    let indexer = Registered::ALL
        .iter()
        .find(|i| i.name() == name)
        .ok_or_else(|| anyhow::anyhow!("unknown indexer {name:?}"))?;
    indexer.search(client, config, query).await
}

/// Every tracker's name, for a progress display that lists them all.
pub fn all_names() -> Vec<&'static str> {
    Registered::ALL.iter().map(Registered::name).collect()
}

/// Query the embedded indexers concurrently and merge the results.
/// `only` restricts the search to the named indexers (used when the user
/// picks a subset instead of "all") so we don't pay for requests to sites
/// the result set will just filter back out. An empty `only` means every
/// tracker.
///
/// One indexer being unreachable or returning unparseable HTML (sites
/// change their markup) doesn't fail the whole search -- it's logged and
/// skipped, same as a multi-indexer aggregator would treat a dead site.
///
/// Because the results are merged with `join_all`, the whole search takes
/// as long as its SLOWEST indexer -- a site that stalls (Cloudflare
/// challenges, a hung connection) holds up the page even though every
/// other tracker already answered. Each indexer is therefore capped
/// individually; whatever hasn't answered by then is dropped with a
/// warning, so one bad site can't stall a search for everyone.
pub async fn search_all(
    client: &reqwest::Client,
    config: &ConfigStore,
    query: &str,
    only: &[String],
) -> Vec<Release> {
    let targets: Vec<&Registered> = Registered::ALL
        .iter()
        .filter(|i| only.is_empty() || only.iter().any(|name| name == i.name()))
        .collect();

    let results = futures_util::future::join_all(targets.iter().map(|indexer| async move {
        let name = indexer.name();
        match tokio::time::timeout(INDEXER_TIMEOUT, indexer.search(client, config, query)).await {
            Ok(result) => (name, result),
            Err(_) => (
                name,
                Err(anyhow::anyhow!(
                    "timed out after {}s",
                    INDEXER_TIMEOUT.as_secs()
                )),
            ),
        }
    }))
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
