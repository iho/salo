//! A real, working indexer for The Pirate Bay (thepiratebay.xyz proxy).
//! Public, no account, and its search results page is served as plain
//! HTML with the magnet already in the row -- verified live before this
//! was wired up (30 magnet-bearing rows for a normal query, and the
//! literal string "No hits" for one that matches nothing).
//!
//! The search URL is path-segmented rather than query-string: 
//! `/search/<term>/<page>/<category>/<filter>`, with `0` meaning "all
//! categories". Category filtering is left to salo's own result filter
//! rather than mapped here.
//!
//! Row layout (confirmed against the live page, one `<td>` each):
//!   0 category | 1 title (links to the detail page) | 2 date
//!   3 magnet   | 4 size  | 5 seeders | 6 leechers | 7 uploader
//!
//! The site has no per-release swarm data beyond those counts, and no
//! JSON API, so this is a straight table scrape.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use scraper::{Html, Selector};

use super::Release;

pub const NAME: &str = "piratebay";

const BASE_URL: &str = "https://thepiratebay.xyz";
/// `/search/<term>/<page>/<order>/<category>`. Order is required to be a
/// valid sort value (`99` = by seeders); sending the wrong segment there
/// -- e.g. `0` -- makes the site answer `404`, which is how this was
/// pinned down against the live site.
const SEARCH_PATH: &str = "search";
const ORDER_BY_SEEDERS: u32 = 99;
/// `0` = every category.
const ALL_CATEGORIES: u32 = 0;

/// The results table is the only `<table id="searchResult">` on the page.
static ROW_SEL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("table#searchResult tbody tr").expect("valid selector")
});
static TITLE_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(2) a").expect("valid selector"));
static MAGNET_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(4) a[href^='magnet:']").expect("valid selector"));
static SIZE_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(5)").expect("valid selector"));
static SEEDERS_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(6)").expect("valid selector"));
static LEECHERS_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(7)").expect("valid selector"));

pub async fn search(client: &reqwest::Client, query: &str) -> Result<Vec<Release>> {
    let term = query.trim();
    if term.is_empty() {
        return Ok(Vec::new());
    }

    let url = format!(
        "{BASE_URL}/{SEARCH_PATH}/{}/1/{ORDER_BY_SEEDERS}/{ALL_CATEGORIES}",
        utf8_percent_encode(term, NON_ALPHANUMERIC),
    );

    let body = client
        .get(&url)
        .timeout(Duration::from_secs(15))
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
    let document = Html::parse_document(body);
    let mut releases = Vec::new();

    for row in document.select(&ROW_SEL) {
        // A row without a magnet is the table's own header/spacer, or a
        // layout the site has since changed -- skip it rather than
        // failing the whole search on somebody else's markup change.
        let Some(magnet) = row
            .select(&MAGNET_SEL)
            .next()
            .and_then(|a| a.value().attr("href"))
            .map(str::to_string)
        else {
            continue;
        };

        let Some(link) = row.select(&TITLE_SEL).next() else {
            continue;
        };
        let title = link.text().collect::<String>().trim().to_string();
        if title.is_empty() {
            continue;
        }

        let source_url = link
            .value()
            .attr("href")
            .map(|href| format!("{BASE_URL}{href}"));

        // The live page pads sizes with non-breaking spaces ("6.07 GiB"),
        // so normalize them or every size sorts/reads oddly.
        let size = text_of(&row, &SIZE_SEL)
            .map(|s| s.replace('\u{a0}', " "))
            .unwrap_or_else(|| "unknown".to_string());

        releases.push(Release {
            indexer: NAME,
            title,
            seeders: number_of(&row, &SEEDERS_SEL),
            leechers: number_of(&row, &LEECHERS_SEL),
            size,
            magnet,
            source_url,
        });
    }

    Ok(releases)
}

fn text_of(row: &scraper::ElementRef, selector: &Selector) -> Option<String> {
    row.select(selector)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
}

fn number_of(row: &scraper::ElementRef, selector: &Selector) -> u32 {
    text_of(row, selector)
        // Strip both thousands separators and non-breaking spaces; the
        // live page pads cells with nbsp, which would otherwise fail the
        // parse and read as zero seeders.
        .map(|t| t.replace([',', '\u{a0}'], ""))
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shaped after the live page's markup: the magnet lives in the 4th
    // cell, wrapped in a <nobr>, with an &amp;-escaped href.
    const SAMPLE: &str = r#"<html><body>
      <table id="searchResult"><tbody>
        <tr>
          <td class="vertTh"><a href="/browse/303">Applications &gt; UNIX</a></td>
          <td><a href="/torrent/82917197/ubuntu-26.04-desktop-amd64_iso">ubuntu-26.04-desktop-amd64.iso</a></td>
          <td>04-25&nbsp;16:35</td>
          <td><nobr><a href="magnet:?xt=urn:btih:DAFC8C076CA2F3ED376EEAE7C76A0D6BE2415C45&amp;dn=ubuntu-26.04-desktop-amd64.iso" title="Download this torrent using magnet"><img src="/static/img/icon-magnet.gif"></a></nobr></td>
          <td align="right">6.07&nbsp;GiB</td>
          <td align="right">146</td>
          <td align="right">11</td>
          <td><a href="/user/P_O_O_P/">P_O_O_P</a></td>
        </tr>
      </tbody></table>
    </body></html>"#;

    #[test]
    fn parses_row_columns_and_unescapes_the_magnet() {
        let releases = parse_results(SAMPLE).expect("parses");
        assert_eq!(releases.len(), 1);
        let r = &releases[0];
        assert_eq!(r.title, "ubuntu-26.04-desktop-amd64.iso");
        assert_eq!(r.seeders, 146);
        assert_eq!(r.leechers, 11);
        assert_eq!(r.size, "6.07 GiB");
        // &amp; in the href must decode, or the magnet's trackers are
        // mangled into "amp;tr=..." and the link is dead.
        assert!(r.magnet.contains("&dn=ubuntu"), "magnet: {}", r.magnet);
        assert!(!r.magnet.contains("amp;"), "magnet: {}", r.magnet);
        assert_eq!(
            r.source_url.as_deref(),
            Some("https://thepiratebay.xyz/torrent/82917197/ubuntu-26.04-desktop-amd64_iso")
        );
    }

    #[test]
    fn search_url_segment_order_matches_the_live_site() {
        // `/search/<term>/<page>/<order>/<category>` -- the order segment
        // must be a real sort value, not 0, or the site 404s. Pinned
        // because getting this wrong looks exactly like "no results".
        let url = format!(
            "{BASE_URL}/{SEARCH_PATH}/{}/1/{ORDER_BY_SEEDERS}/{ALL_CATEGORIES}",
            "ubuntu"
        );
        assert_eq!(url, "https://thepiratebay.xyz/search/ubuntu/1/99/0");
        assert_ne!(ORDER_BY_SEEDERS, 0);
    }

    #[test]
    fn numbers_tolerate_non_breaking_spaces() {
        // The live page pads cells with a non-breaking space ("6.07 GiB"),
        // which must still parse as a number. Built with a real nbsp
        // rather than an escape, since this fixture is a raw string.
        let nbsp = '\u{a0}';
        let body = format!(
            r#"<table id="searchResult"><tbody><tr>
            <td>c</td>
            <td><a href="/torrent/1/a">t</a></td>
            <td>d</td>
            <td><a href="magnet:?xt=urn:btih:AAA">m</a></td>
            <td align="right">1{nbsp}234 MiB</td>
            <td align="right">1{nbsp}234</td>
            <td align="right">7</td>
            <td>u</td>
        </tr></tbody></table>"#
        );
        let releases = parse_results(&body).expect("parses");
        assert_eq!(releases[0].seeders, 1234);
        assert_eq!(releases[0].size, "1 234 MiB");
    }

    #[test]
    fn no_hits_page_yields_no_releases() {
        // The live "no matches" page says exactly this, with no table rows.
        let body = "<html><body><div>No hits</div><table id=\"searchResult\"><tbody></tbody></table></body></html>";
        assert!(parse_results(body).expect("parses").is_empty());
    }

    #[test]
    fn rows_without_a_magnet_are_skipped() {
        // A header-ish row (no magnet) must not produce a bogus release.
        let body = r#"<table id="searchResult"><tbody>
            <tr><td>x</td><td><a href="/torrent/1/a">title only</a></td><td>d</td>
                <td></td><td>1 GiB</td><td>1</td><td>2</td><td>u</td></tr>
        </tbody></table>"#;
        assert!(parse_results(body).expect("parses").is_empty());
    }
}
