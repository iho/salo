//! A real indexer for nyaa.si -- the main English-language anime torrent
//! tracker. Public, no account (verified live: a plain HTTP search returns
//! a full page of rows with magnets already in them).
//!
//! Row layout, confirmed against a live results page. Note the title cell
//! carries `colspan="2"`, so its sibling indices are offset from the
//! header's own order:
//!
//!   td[0] category icon | td[1] title (colspan 2, links to /view/<id>)
//!   td[2] links (.torrent + magnet) | td[3] size | td[4] date
//!   td[5] seeders | td[6] leechers | td[7] completed
//!
//! The site is reachable over plain HTTP here (no Cloudflare interstitial
//! on the search page), but its result count is small for non-anime terms
//! -- a query with no matches returns the table with only its header row,
//! which parses as zero releases rather than an error.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use scraper::{Html, Selector};

use super::{CommentInfo, Release};

pub const NAME: &str = "nyaa";

const BASE_URL: &str = "https://nyaa.si";

/// The results table. `table.table` is nyaa's own results class (the
/// header row is inside `<thead>`, so selecting `tbody tr` skips it).
static ROW_SEL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("table.table tbody tr").expect("valid selector")
});
/// Title + link to the release's page. `colspan=2` here is why the
/// sibling cells below are selected by position, not by header name.
///
/// `:not(.comments)` matters: the cell's FIRST `<a>` is nyaa's comments
/// link (`<a href="/view/1631458#comments" class="comments">9</a>`), so
/// plain `a` selected the comment count as the release title and every
/// result came back named `"9"`.
static TITLE_SEL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("td:nth-child(2) a:not(.comments)").expect("valid selector")
});
/// The comments link in the same cell: its text is the count, and its href
/// is the release page anchored at the comments.
static COMMENTS_SEL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("td:nth-child(2) a.comments").expect("valid selector")
});
static MAGNET_SEL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("td:nth-child(3) a[href^='magnet:']").expect("valid selector")
});
static SIZE_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(4)").expect("valid selector"));
static SEEDERS_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(6)").expect("valid selector"));
static LEECHERS_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(7)").expect("valid selector"));

pub async fn search(client: &reqwest::Client, query: &str) -> Result<Vec<Release>> {
    let term = query.trim();
    if term.is_empty() {
        return Ok(Vec::new());
    }

    // `f=0` (no filter) and `c=0_0` (all categories); sorted by seeders so
    // the most alive releases come first, matching how the site's own
    // search defaults behave.
    let url = format!(
        "{BASE_URL}/?f=0&c=0_0&s=seeders&o=desc&q={}",
        utf8_percent_encode(term, NON_ALPHANUMERIC)
    );

    let body = client
        .get(&url)
        .timeout(Duration::from_secs(20))
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
        // A row without a magnet isn't a release (nyaa renders its header
        // in thead, but a layout change or a notice row would land here) --
        // skip rather than fail the whole search.
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

        // The title's href is the release's own page (`/view/<id>`), which
        // is what "view on the original tracker" should point at.
        // nyaa serves this root-relative (`/view/<id>`); `absolute_url`
        // handles that and would also pass an absolute one through
        // unchanged.
        let source_url = link
            .value()
            .attr("href")
            .map(|href| super::absolute_url(BASE_URL, href));

        let comments = comments_of(&row);

        releases.push(Release {
            indexer: NAME,
            title,
            seeders: number_of(&row, &SEEDERS_SEL),
            leechers: number_of(&row, &LEECHERS_SEL),
            size: text_of(&row, &SIZE_SEL).unwrap_or_else(|| "unknown".to_string()),
            magnet,
            source_url,
            comments,
        });
    }

    Ok(releases)
}

/// The release's comment count and link, when the row has the comments
/// anchor. Absent for a release nobody has commented on.
fn comments_of(row: &scraper::ElementRef<'_>) -> Option<CommentInfo> {
    let link = row.select(&COMMENTS_SEL).next()?;
    let count: u32 = link
        .text()
        .collect::<String>()
        .trim()
        .parse()
        .ok()
        // The icon inside the anchor is an <i>, so the text is just the
        // number; anything unparseable means the markup changed.
        .filter(|n| *n > 0)?;
    let url = super::absolute_url(BASE_URL, link.value().attr("href")?);
    Some(CommentInfo { count, url })
}

fn text_of(row: &scraper::ElementRef, selector: &Selector) -> Option<String> {
    row.select(selector)
        .next()
        .map(|el| el.text().collect::<String>().trim().replace('\u{a0}', " "))
        .filter(|s| !s.is_empty())
}

fn number_of(row: &scraper::ElementRef, selector: &Selector) -> u32 {
    text_of(row, selector)
        .map(|t| t.replace([',', '\u{a0}'], ""))
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shaped after the live markup, including the title cell's
    // `colspan="2"` (which offsets the seeders/leechers positions) and an
    // `&amp;`-escaped magnet.
    const SAMPLE: &str = r#"<html><body>
      <table class="table table-bordered table-hover table-striped">
        <thead><tr><th>Category</th><th>Name</th><th>Link</th><th>Size</th>
          <th>Date</th><th>S</th><th>L</th><th>C</th></tr></thead>
        <tbody>
        <tr>
          <td><a href="/?c=1_2" title="Anime - English-translated"><img src="/static/img/icons/nyaa/1_2.png"></a></td>
          <td colspan="2">
            <a href="/view/2157520#comments" class="comments" title="9 comments"><i class="fa fa-comments-o"></i>9</a>
            <a href="/view/2157520" title="[AnoZu] One Piece S23E22 1080p">[AnoZu] One Piece S23E22 1080p CR WEB-DL AAC 2.0 H.264</a>
          </td>
          <td class="text-center">
            <a href="/download/2157520.torrent"><i class="fa fa-fw fa-download"></i></a>
            <a href="magnet:?xt=urn:btih:04f5241b36d09965357e6dc85bd8514a2ff41c76&amp;dn=%5BAnoZu%5D%20One%20Piece"><i class="fa fa-fw fa-magnet"></i></a>
          </td>
          <td class="text-center">1.3 GiB</td>
          <td class="text-center">2026-09-06 16:03</td>
          <td class="text-center">1084</td>
          <td class="text-center">10</td>
          <td class="text-center">7820</td>
        </tr>
        <tr>
          <td><a href="/?c=1_2" title="Anime - English-translated"><img src="/static/img/icons/nyaa/1_2.png"></a></td>
          <td colspan="2">
            <a href="/view/1631458" title="No comments yet">Armitage III Poly Matrix (1996)</a>
          </td>
          <td class="text-center">
            <a href="/download/1631458.torrent"><i class="fa fa-fw fa-download"></i></a>
            <a href="magnet:?xt=urn:btih:997863b5b1f4c1e51652cd1018d49a8f9c0f4ea1&amp;dn=Armitage"><i class="fa fa-fw fa-magnet"></i></a>
          </td>
          <td class="text-center">5.0 GiB</td>
          <td class="text-center">2023-01-29 05:37</td>
          <td class="text-center">58</td>
          <td class="text-center">0</td>
          <td class="text-center">6628</td>
        </tr>
        </tbody>
      </table>
    </body></html>"#;

    #[test]
    fn parses_a_result_row() {
        let releases = parse_results(SAMPLE).expect("parses");
        assert_eq!(releases.len(), 2);
        let r = &releases[0];
        // The cell's FIRST <a> is the comments link, so a plain `a`
        // selector returned "9" as the title for every result. The title
        // must come from the link that isn't the comments one.
        assert_eq!(r.title, "[AnoZu] One Piece S23E22 1080p CR WEB-DL AAC 2.0 H.264");
        // Column positions, offset by the title cell's colspan.
        assert_eq!(r.size, "1.3 GiB");
        assert_eq!(r.seeders, 1084);
        assert_eq!(r.leechers, 10);
        // The magnet's &amp; must decode or its trackers are mangled.
        assert!(!r.magnet.contains("amp;"), "magnet: {}", r.magnet);
        assert!(r.magnet.starts_with("magnet:?xt=urn:btih:04f5241b"));
        assert_eq!(
            r.source_url.as_deref(),
            Some("https://nyaa.si/view/2157520")
        );
        // The comment count and its anchored link.
        let c = r.comments.as_ref().expect("row has a comments link");
        assert_eq!(c.count, 9);
        assert_eq!(c.url, "https://nyaa.si/view/2157520#comments");
    }

    #[test]
    fn a_release_with_no_comments_link_reports_none() {
        // `None`, not 0: "no comments yet" and "the site didn't say" are
        // different, and a fabricated 0 would claim the former.
        let releases = parse_results(SAMPLE).expect("parses");
        let r = &releases[1];
        assert_eq!(r.title, "Armitage III Poly Matrix (1996)");
        assert!(r.comments.is_none());
    }

    #[test]
    fn the_title_is_never_the_comment_count() {
        // The exact shape of the bug that shipped: every nyaa result named
        // with a bare number. Guard the whole fixture against it.
        for r in parse_results(SAMPLE).expect("parses") {
            assert!(
                r.title.parse::<u32>().is_err(),
                "title {:?} parsed as a bare number",
                r.title
            );
        }
    }

    #[test]
    fn header_only_page_yields_nothing() {
        // What a zero-result search actually returns: the table, with the
        // header in <thead> and no body rows.
        let body = r#"<table class="table"><thead><tr><th>Name</th></tr></thead><tbody></tbody></table>"#;
        assert!(parse_results(body).expect("parses").is_empty());
    }

    #[test]
    fn rows_without_a_magnet_are_skipped() {
        let body = r#"<table class="table"><tbody><tr>
            <td><a href="/?c=1_2">cat</a></td>
            <td colspan="2"><a href="/view/1">notice, no magnet</a></td>
        </tr></tbody></table>"#;
        assert!(parse_results(body).expect("parses").is_empty());
    }
}
