//! TMDB lookups: the poster, blurb and metadata shown on a torrent's
//! detail page.
//!
//! TMDB is the only metadata provider used here, which is not an
//! arbitrary choice -- it is the one TorrServer uses (see the README's
//! submodule notes), so the behaviour follows what the user already knows
//! from that app, including its two non-obvious decisions:
//!
//! 1. **The release name is parsed, not pattern-matched.** TorrServer
//!    leans on `parse-torrent-title`, a real title parser, to turn
//!    `Django.Unchained.2012.1080p.BluRay.DDP5.1.x265.10bit-GalaxyRG265[TGx]`
//!    into `Django Unchained`. A hand-rolled "strip known junk words"
//!    heuristic is NOT equivalent: it left `DDP5` in the query, and TMDB
//!    returns *zero* results for `Django Unchained 2012 DDP5` -- a
//!    multi-word query is matched essentially literally. So this uses
//!    `torrent-name-parser`, the Rust equivalent.
//! 2. **All poster candidates are kept, not just the first.** TorrServer
//!    shows the other results as clickable suggestions and stores the one
//!    the user picks. A wrong first match is common enough that being able
//!    to switch matters.
//!
//! The API key is the user's own (`api_key` in the settings store under
//! the `tmdb` section) -- there is no bundled key. Without one, [`lookup`]
//! returns `Ok(None)` **without making a request**: no key means no
//! metadata, not an error, and nothing leaves the machine.

use std::str::FromStr;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config_store::ConfigStore;

/// Storage key for the user's TMDB API key, in the `tmdb` settings
/// section. A `v3` auth key (`api_key` query parameter), not a v4 bearer
/// token.
pub const KEY_API: &str = "api_key";

/// Storage key for the poster the user picked, when they overrode the
/// automatic first match. Read back by the detail page.
pub const KEY_POSTER: &str = "poster_url";

/// The settings section these values live under. Not an indexer: it is
/// deliberately absent from `indexers::names()` so it never appears in
/// the tracker picker or gets searched.
pub const SECTION: &str = "tmdb";

const API_BASE: &str = "https://api.themoviedb.org/3";
/// Poster host. `w300` matches what TorrServer requests -- enough to look
/// sharp at the detail page's width without pulling a multi-MB original.
const IMAGE_BASE: &str = "https://image.tmdb.org/t/p/w300";
const SITE_BASE: &str = "https://www.themoviedb.org";
/// How many alternative posters to offer. TorrServer shows 12; the same
/// here, since more than that is a wall rather than a choice.
const MAX_CANDIDATES: usize = 12;

/// The settings page's single field for this section.
pub fn settings_field() -> crate::indexers::SettingField {
    crate::indexers::SettingField {
        label: "TMDB API key",
        key: KEY_API,
        kind: crate::indexers::SettingFieldKind::Password,
        help: "Optional. Your own TMDB v3 API key (themoviedb.org → Settings → API). \
               With a key, torrent detail pages show a poster and plot summary; \
               without one, no TMDB request is made at all.",
        default_on: false,
    }
}

/// One poster candidate: the image, plus which release it belongs to, so
/// a suggestion is identifiable without opening it.
pub struct Poster {
    pub url: String,
    pub title: String,
    pub year: String,
}

/// What the detail page renders.
pub struct MovieInfo {
    pub title: String,
    /// `"2012"`, empty when TMDB has no date.
    pub year: String,
    pub overview: String,
    /// The poster being shown -- TMDB's first match, or the user's saved
    /// choice when they picked a different one.
    pub poster_url: String,
    /// `"8.1"`, or empty when unrated.
    pub rating: String,
    /// `"Movie"` or `"TV"`, for the badge next to the title.
    pub kind: String,
    /// The TMDB page, for "read more".
    pub page_url: String,
    /// Other candidates (excluding the one above), for the picker.
    pub alternatives: Vec<Poster>,
}

/// Fields shared by movie and TV results in a `/search/multi` response.
#[derive(Deserialize)]
struct SearchResult {
    id: u64,
    #[serde(default)]
    media_type: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default)]
    first_air_date: Option<String>,
    #[serde(default)]
    overview: Option<String>,
    #[serde(default)]
    poster_path: Option<String>,
    #[serde(default)]
    vote_average: Option<f64>,
}

impl SearchResult {
    fn display_title(&self) -> String {
        self.title
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_else(|| "Untitled".to_string())
    }

    /// The year, from whichever date field this media type populates:
    /// movies carry `release_date`, TV carries `first_air_date`.
    fn year(&self) -> String {
        let date = self
            .release_date
            .as_deref()
            .or(self.first_air_date.as_deref())
            .unwrap_or_default();
        date.split('-').next().unwrap_or_default().trim().to_string()
    }

    fn poster_url(&self) -> Option<String> {
        self.poster_path
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(|p| format!("{IMAGE_BASE}{p}"))
    }

    fn page_url(&self) -> String {
        let kind = if self.media_type == "tv" { "tv" } else { "movie" };
        format!("{SITE_BASE}/{kind}/{}", self.id)
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchResult>,
}

/// Looks up the movie/series a torrent name refers to.
///
/// Returns `Ok(None)` when there is no API key configured, when TMDB has
/// no match, or when no match has a poster -- all of which mean "show no
/// metadata block", not an error. A transient failure (network,
/// unparseable response) is likewise swallowed to `None` by the caller:
/// the detail page must render whether or not TMDB is reachable, and a
/// torrent's own controls matter more than artwork.
pub async fn lookup(
    client: &reqwest::Client,
    config: &ConfigStore,
    torrent_name: &str,
) -> Result<Option<MovieInfo>> {
    let Some(api_key) = api_key(config)? else {
        return Ok(None);
    };
    let query = search_query(torrent_name);
    if query.is_empty() {
        return Ok(None);
    }

    let response = client
        .get(format!("{API_BASE}/search/multi"))
        .query(&[
            ("api_key", api_key.as_str()),
            ("query", query.as_str()),
            ("include_adult", "false"),
        ])
        .send()
        .await
        .context("TMDB search request failed")?;

    if !response.status().is_success() {
        anyhow::bail!("TMDB search returned {}", response.status());
    }

    let parsed: SearchResponse = response.json().await.context("TMDB response was not JSON")?;
    Ok(build(parsed.results, saved_poster(config)?))
}

/// The stored API key, if any. Trimmed, and an empty value counts as
/// absent -- an empty string in the settings store must not produce a
/// request that comes back 401.
fn api_key(config: &ConfigStore) -> Result<Option<String>> {
    Ok(config
        .get(SECTION, KEY_API)?
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty()))
}

/// The poster the user previously chose, if any.
fn saved_poster(config: &ConfigStore) -> Result<Option<String>> {
    Ok(config
        .get(SECTION, KEY_POSTER)?
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty()))
}

/// Turns TMDB's results into what the page renders.
///
/// The default poster is the first result that *has* one -- TMDB's
/// relevance order is good, but it routinely returns poster-less entries
/// (obscure documentaries, person credits) ahead of the obvious match, and
/// a card with no artwork is worse than none. A saved choice wins over
/// that, so the user's pick survives a reload.
fn build(results: Vec<SearchResult>, saved: Option<String>) -> Option<MovieInfo> {
    let with_art: Vec<SearchResult> = results
        .into_iter()
        .filter(|r| r.poster_path.as_deref().is_some_and(|p| !p.is_empty()))
        .collect();
    let first = with_art.first()?;

    // Honour a saved poster only if it's still among the candidates:
    // otherwise a stale value from a previous match would show a poster
    // belonging to a different film.
    let poster_url = saved
        .filter(|s| with_art.iter().any(|r| r.poster_url().as_deref() == Some(s.as_str())))
        .or_else(|| first.poster_url())?;

    // The picker offers the *other* candidates, so it never contains the
    // one already on screen.
    let alternatives = with_art
        .iter()
        .filter(|r| r.poster_url().as_deref() != Some(poster_url.as_str()))
        .take(MAX_CANDIDATES)
        .filter_map(|r| {
            Some(Poster {
                url: r.poster_url()?,
                title: r.display_title(),
                year: r.year(),
            })
        })
        .collect();

    let kind = if first.media_type == "tv" { "TV" } else { "Movie" };
    Some(MovieInfo {
        title: first.display_title(),
        year: first.year(),
        overview: first
            .overview
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string(),
        poster_url,
        rating: first
            .vote_average
            .filter(|r| *r > 0.0)
            .map(|r| format!("{r:.1}"))
            .unwrap_or_default(),
        kind: kind.to_string(),
        page_url: first.page_url(),
        alternatives,
    })
}

/// Reduces a release name to the title TMDB should be searched for.
///
/// Delegates to `torrent-name-parser` -- the Rust stand-in for the
/// `parse-torrent-title` library TorrServer uses for this -- rather than
/// stripping a list of known junk tokens, because the parser understands
/// the *structure* of a release name and a token list cannot.
///
/// Two corrections on top of the parser, both found by testing real
/// release names against the live API (TMDB matches a multi-word query
/// essentially *literally*, so any residue at the end of the title makes
/// the search return nothing rather than something approximate):
/// - A year RANGE survives into the parser's title (`The Matrix 1-4 Pack
///   1999-` for a box set, where it reports only the later year), so the
///   title is cut at the first year-like token in it.
/// - A name the parser could not really make sense of (`ubuntu-26.04-…iso`
///   → `ubuntu-26 04-snapshot3-desktop`) is rejected unless it recognised
///   some media marker; otherwise a request is spent to render a film
///   that has nothing to do with the download.
fn search_query(release_name: &str) -> String {
    /// Real titles are far shorter than this; anything longer is a parse
    /// failure rather than a title.
    const MAX_LEN: usize = 80;

    let Ok(parsed) = torrent_name_parser::Metadata::from_str(release_name) else {
        return String::new();
    };

    // A resolution or a year is what the parser recognises in an actual
    // release. Neither means it was guessing at a filename, a dataset or
    // a music folder -- none of which TMDB can answer for.
    if parsed.resolution().is_none() && parsed.year().is_none() {
        return String::new();
    }

    let mut title = parsed.title().trim().to_string();
    if let Some(pos) = first_year_offset(&title) {
        title.truncate(pos);
    }
    // A dangling `[`, `-` or `(` is left where a tag followed the title
    // (`One Piece 1178 [`), and must not reach the API.
    let title = title
        .trim_end_matches(['[', '(', '-', ' ', '/', ','])
        .trim()
        .to_string();

    if title.chars().count() < 2 || title.chars().count() > MAX_LEN {
        return String::new();
    }
    title
}

/// Byte offset of the first year-like token (`1900`-`2099`) in `text`.
///
/// Written by hand rather than with the `regex` crate: this runs once per
/// page view on a short string, and "four digits not part of a longer
/// number, in a plausible year range" is the whole rule.
fn first_year_offset(text: &str) -> Option<usize> {
    const MIN_YEAR: u32 = 1900;
    const MAX_YEAR: u32 = 2099;

    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for i in 0..chars.len().saturating_sub(3) {
        let window = &chars[i..i + 4];
        if !window.iter().all(|(_, c)| c.is_ascii_digit()) {
            continue;
        }
        let digits: String = window.iter().map(|(_, c)| *c).collect();
        let Ok(year) = digits.parse::<u32>() else {
            continue;
        };
        if !(MIN_YEAR..=MAX_YEAR).contains(&year) {
            continue;
        }
        // Reject a run that's part of a longer number ("1080" in
        // "10800", or the "201" of "20123").
        let prev_is_digit = i > 0 && chars[i - 1].1.is_ascii_digit();
        let next_is_digit = chars.get(i + 4).is_some_and(|(_, c)| c.is_ascii_digit());
        if !prev_is_digit && !next_is_digit {
            return Some(window[0].0);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_junk_is_parsed_out_of_the_search_query() {
        // The case that motivated using a parser: a token-stripping
        // heuristic produced "Django Unchained 2012 DDP5" here, and TMDB
        // returns zero results for that.
        assert_eq!(
            search_query("Django.Unchained.2012.1080p.BluRay.DDP5.1.x265.10bit-GalaxyRG265[TGx]"),
            "Django Unchained"
        );
        assert_eq!(
            search_query("The.Matrix.1999.2160p.UHD.BluRay.REMUX.HDR.HEVC"),
            "The Matrix"
        );
        assert_eq!(
            search_query("Dune.2021.2160p.WEB-DL.DDP5.1.Atmos.DV.HDR.HEVC-CMRG"),
            "Dune"
        );
        assert_eq!(search_query("Heat.1995.1080p.BluRay.x264"), "Heat");
    }

    #[test]
    fn a_numeric_title_is_not_mistaken_for_a_year() {
        // "13" is the title, "2010" is the year.
        assert_eq!(search_query("13.2010.1080p.BluRay.x264"), "13");
    }

    #[test]
    fn a_trailing_bracket_from_an_episode_tag_is_stripped() {
        // The parser leaves "One Piece 1178 [" -- the dangling bracket
        // must not reach TMDB. The episode number stays in the query
        // because a series is identified by its name, and "One Piece"
        // alone is what TMDB wants; the bracket is the only problem.
        let q = search_query("[Erai-raws] One Piece - 1178 [1080p][Multiple Subtitle]");
        assert!(!q.ends_with('['), "{q:?} still ends with a bracket");
        assert!(q.starts_with("One Piece"), "{q:?}");
    }

    #[test]
    fn the_first_year_like_token_ends_the_title() {
        assert_eq!(first_year_offset("The Matrix 1-4 Pack 1999-"), Some(20));
        // A resolution must not be mistaken for one.
        assert_eq!(first_year_offset("Django Unchained 1080p"), None);
        assert_eq!(first_year_offset("Django 2012 1080p"), Some(7));
        // Part of a longer number is not a year.
        assert_eq!(first_year_offset("Movie 120123"), None);
        // Out of range for a film release.
        assert_eq!(first_year_offset("Movie 1789"), None);
        assert_eq!(first_year_offset("no digits here"), None);
    }

    #[test]
    fn a_box_set_title_is_cut_before_the_year_range() {
        // A year RANGE survives into the parser's title ("1999-") and
        // alone makes TMDB match nothing.
        assert_eq!(
            search_query("The Matrix 1-4 Pack 1999-2021 REMASTERED 1080p BluRay HEVC x265 5.1 BONE"),
            "The Matrix 1-4 Pack"
        );
    }

    #[test]
    fn a_non_media_filename_yields_no_query() {
        // No year is recognised, so the parser was guessing at a
        // filename -- searching it would render an unrelated film.
        assert_eq!(search_query("ubuntu-26.04-snapshot3-desktop-amd64.iso"), "");
    }

    #[test]
    fn junk_input_yields_no_query() {
        for junk in ["", "   ", ".", "..."] {
            assert_eq!(search_query(junk), "", "input {junk:?}");
        }
    }

    fn result(
        media_type: &str,
        poster: Option<&str>,
        title: &str,
        date: Option<&str>,
    ) -> SearchResult {
        SearchResult {
            id: 42,
            media_type: media_type.to_string(),
            title: Some(title.to_string()),
            name: None,
            release_date: date.map(str::to_string),
            first_air_date: None,
            overview: Some("A plot.".to_string()),
            poster_path: poster.map(str::to_string),
            vote_average: Some(8.14),
        }
    }

    #[test]
    fn a_poster_less_result_is_skipped_for_one_that_has_artwork() {
        let info = build(
            vec![
                result("movie", None, "Wrong", Some("2001-01-01")),
                result("movie", Some("/p.jpg"), "Right", Some("2012-12-25")),
            ],
            None,
        )
        .expect("second result has a poster");
        assert_eq!(info.title, "Right");
        assert_eq!(info.year, "2012");
        assert_eq!(info.poster_url, "https://image.tmdb.org/t/p/w300/p.jpg");
        assert_eq!(info.rating, "8.1");
        assert_eq!(info.kind, "Movie");
        assert_eq!(info.page_url, "https://www.themoviedb.org/movie/42");
        // Nothing else had artwork, so there is nothing to switch to.
        assert!(info.alternatives.is_empty());
    }

    #[test]
    fn tv_results_use_name_and_first_air_date() {
        let mut tv = result("tv", Some("/t.jpg"), "ignored", None);
        tv.title = None;
        tv.name = Some("Breaking Bad".to_string());
        tv.release_date = None;
        tv.first_air_date = Some("2008-01-20".to_string());
        let info = build(vec![tv], None).expect("has a poster");
        assert_eq!(info.title, "Breaking Bad");
        assert_eq!(info.year, "2008");
        assert_eq!(info.kind, "TV");
        assert_eq!(info.page_url, "https://www.themoviedb.org/tv/42");
    }

    #[test]
    fn no_results_at_all_yields_nothing() {
        assert!(build(Vec::new(), None).is_none());
        // All poster-less: nothing to show.
        assert!(build(vec![result("movie", None, "X", Some("2000-01-01"))], None).is_none());
    }

    #[test]
    fn alternatives_are_offered_for_switching() {
        let info = build(
            vec![
                result("movie", Some("/first.jpg"), "First", Some("2012-01-01")),
                result("movie", Some("/second.jpg"), "Second", Some("2021-01-01")),
                result("tv", Some("/third.jpg"), "Third", Some("2019-05-05")),
            ],
            None,
        )
        .expect("has a poster");
        assert_eq!(info.poster_url, "https://image.tmdb.org/t/p/w300/first.jpg");
        assert_eq!(info.alternatives.len(), 2);
        assert_eq!(info.alternatives[0].title, "Second");
        assert_eq!(info.alternatives[0].year, "2021");
        assert_eq!(info.alternatives[1].title, "Third");
        // The one on screen is never also offered as an alternative.
        assert!(
            !info
                .alternatives
                .iter()
                .any(|p| p.url == info.poster_url),
            "the shown poster appeared in its own alternatives"
        );
    }

    #[test]
    fn a_saved_poster_wins_over_the_first_match() {
        let results = vec![
            result("movie", Some("/first.jpg"), "First", Some("2012-01-01")),
            result("movie", Some("/second.jpg"), "Second", Some("2021-01-01")),
        ];
        let info = build(
            results,
            Some("https://image.tmdb.org/t/p/w300/second.jpg".to_string()),
        )
        .expect("has a poster");
        assert_eq!(info.poster_url, "https://image.tmdb.org/t/p/w300/second.jpg");
        // The title still describes the best match, which is what the
        // summary is for; only the artwork is overridden.
        assert_eq!(info.title, "First");
        // And the previously-shown poster becomes selectable instead.
        assert_eq!(info.alternatives.len(), 1);
        assert_eq!(info.alternatives[0].title, "First");
    }

    #[test]
    fn a_stale_saved_poster_is_ignored() {
        // A value saved for a *different* torrent's match must not be
        // shown as this one's artwork.
        let results = vec![result("movie", Some("/first.jpg"), "First", Some("2012-01-01"))];
        let info = build(
            results,
            Some("https://image.tmdb.org/t/p/w300/other.jpg".to_string()),
        )
        .expect("has a poster");
        assert_eq!(info.poster_url, "https://image.tmdb.org/t/p/w300/first.jpg");
    }
}
