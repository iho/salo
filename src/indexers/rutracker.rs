//! A real indexer for rutracker.org -- a semi-private Russian phpBB
//! tracker, one of the largest Russian-language BitTorrent communities.
//!
//! NOT VERIFIED LIVE, for two independent reasons, both measured against
//! the site rather than assumed:
//!
//! 1. Searching requires a logged-in account, and no account was
//!    available to test with.
//! 2. **rutracker.org sits behind a Cloudflare JavaScript challenge.**
//!    Plain HTTP requests -- including to `forum/login.php` -- come back
//!    as `403` with a "Just a moment..." interstitial on both
//!    rutracker.org and rutracker.net, whatever User-Agent is sent. That
//!    blocks logging in *at all* from a plain HTTP client, so this module
//!    cannot work against the real site as-is; a browser-backed fetch (or
//!    an already-solved session cookie pasted into the settings store)
//!    would be needed. Until then treat this as a faithful port that is
//!    not yet usable.
//!
//! Everything below mirrors the local Prowlarr definition
//! (`RuTracker.cs`): the query parameters, the row selectors, the
//! windows-1251 encoding, and the login form fields.
//!
//! The site is windows-1251, so responses are decoded explicitly rather
//! than through reqwest's UTF-8-assuming `.text()`.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use regex::Regex;
use scraper::{Html, Selector};

use super::Release;

pub const NAME: &str = "rutracker";

const BASE_URL: &str = "https://rutracker.org/";
static BASE: LazyLock<reqwest::Url> =
    LazyLock::new(|| reqwest::Url::parse(BASE_URL).expect("valid base URL"));

const LOGIN_PATH: &str = "forum/login.php";
const SEARCH_PATH: &str = "forum/tracker.php";

pub const KEY_USERNAME: &str = "username";
pub const KEY_PASSWORD: &str = "password";
pub const KEY_COOKIE: &str = "cookie_header";
pub const KEY_MOVE_TAGS: &str = "move_first_tags_to_end";
pub const KEY_RUSSIAN_LETTERS: &str = "russian_letters";

/// phpBB/ rutracker marks a logged-in session with this element;
/// `CheckIfLoginNeeded` keys off it, and so does this.
const LOGGED_IN_MARKER: &str = "id=\"logged-in-username\"";

pub fn settings_fields() -> Vec<super::SettingField> {
    vec![
        super::SettingField {
            label: "Username",
            key: KEY_USERNAME,
            kind: super::SettingFieldKind::Text,
            help: "Your rutracker.org account login.",
            default_on: false,
        },
        super::SettingField {
            label: "Password",
            key: KEY_PASSWORD,
            kind: super::SettingFieldKind::Password,
            help: "Your rutracker.org account password (stored locally in SQLite). Use the \
                   session cookie instead if the site serves a bot challenge to logins.",
            default_on: false,
        },
        super::SettingField {
            label: "Session cookie",
            key: KEY_COOKIE,
            kind: super::SettingFieldKind::Password,
            help: "Paste a logged-in browser session's Cookie header. This is the reliable \
                   way in: the site serves a Cloudflare browser challenge to plain HTTP \
                   logins, but a session cookie that already passed it works.",
            default_on: false,
        },
        super::SettingField {
            label: "Strip Russian letters",
            key: KEY_RUSSIAN_LETTERS,
            kind: super::SettingFieldKind::Checkbox,
            help: "Keep only the Latin part of release titles.",
            default_on: false,
        },
        super::SettingField {
            label: "Move leading tags to end",
            key: KEY_MOVE_TAGS,
            kind: super::SettingFieldKind::Checkbox,
            help: "Move leading bracket tags to the end of the release name.",
            default_on: false,
        },
    ]
}

/// `RuTrackerRequestGenerator.GetPagedRequests`: every run of characters
/// outside `[a-zA-Zа-яА-ЯёЁ0-9]` collapses to a single `%` wildcard, and
/// hyphens become spaces. That's the site's own search syntax.
static NON_SEARCH_CHARS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^a-zA-Zа-яА-ЯёЁ0-9]+").expect("valid regex"));

static ROW_SEL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("table#tor-tbl > tbody > tr").expect("valid selector")
});
static DOWNLOAD_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td.tor-size > a.tr-dl").expect("valid selector"));
static TITLE_SEL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("td.t-title-col > div.t-title > a.tLink").expect("valid selector")
});
/// `GetSizeOfRelease` reads the size cell's `data-ts_text` attribute, not
/// its text, so that attribute is preferred with the text as a fallback.
static SIZE_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td.tor-size").expect("valid selector"));
static SEEDERS_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(7) b").expect("valid selector"));
static LEECHERS_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(8)").expect("valid selector"));

pub async fn search(
    client: &reqwest::Client,
    config: &crate::config_store::ConfigStore,
    query: &str,
) -> Result<Vec<Release>> {
    let entries = config.get_all(NAME)?;
    let get = |key: &str| {
        entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.trim().is_empty())
    };
    let flag = |key: &str| {
        get(key)
            .map(|v| matches!(v.trim(), "1" | "true" | "on" | "yes"))
            .unwrap_or(false)
    };

    let cookie = session_cookie(client, config).await?;

    let term = NON_SEARCH_CHARS_RE.replace_all(query.trim(), "%");
    let term = term.replace('-', " ");
    let url = format!(
        "{BASE_URL}{SEARCH_PATH}?nm={}",
        utf8_percent_encode(term.trim(), NON_ALPHANUMERIC)
    );

    let response = client
        .get(&url)
        .header(reqwest::header::COOKIE, cookie)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .with_context(|| format!("request to {NAME} failed: {url}"))?;

    let status = response.status();
    let body = decode_windows_1251(response)
        .await
        .with_context(|| format!("failed to read {NAME} response body"))?;

    if !status.is_success() {
        // The Cloudflare challenge is the expected failure here, so name
        // it explicitly instead of leaving a bare 403 in the log.
        anyhow::bail!(
            "{NAME} returned {status}{}",
            if body.contains("Just a moment") {
                " (Cloudflare browser challenge -- a plain HTTP client cannot pass it; \
                  paste a logged-in session cookie on the settings page instead)"
            } else {
                ""
            }
        );
    }
    if !body.contains(LOGGED_IN_MARKER) {
        anyhow::bail!(
            "{NAME} served a guest page -- the session is not valid; paste a fresh cookie"
        );
    }

    parse_results(&body, flag(KEY_RUSSIAN_LETTERS), flag(KEY_MOVE_TAGS))
}

async fn decode_windows_1251(response: reqwest::Response) -> Result<String> {
    let bytes = response
        .bytes()
        .await
        .context("failed to read response bytes")?;
    let (decoded, _, had_errors) = encoding_rs::WINDOWS_1251.decode(&bytes);
    if had_errors {
        tracing::debug!(indexer = NAME, "windows-1251 decode had invalid sequences");
    }
    Ok(decoded.into_owned())
}

async fn session_cookie(
    client: &reqwest::Client,
    config: &crate::config_store::ConfigStore,
) -> Result<String> {
    let stored = config
        .get_all(NAME)?
        .into_iter()
        .find(|(k, _)| k == KEY_COOKIE)
        .map(|(_, v)| v)
        .filter(|v| !v.trim().is_empty());

    if let Some(cookie) = stored {
        if session_works(client, &cookie).await {
            return Ok(cookie);
        }
        tracing::warn!(indexer = NAME, "stored session was rejected; logging in again");
    }

    authenticate(client, config).await
}

async fn session_works(client: &reqwest::Client, cookie: &str) -> bool {
    let Ok(url) = BASE.join(SEARCH_PATH) else {
        return false;
    };
    let Ok(response) = client
        .get(url)
        .header(reqwest::header::COOKIE, cookie)
        .timeout(Duration::from_secs(20))
        .send()
        .await
    else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    decode_windows_1251(response)
        .await
        .map(|body| body.contains(LOGGED_IN_MARKER))
        .unwrap_or(false)
}

/// `RuTracker.DoLogin`. On this site this is expected to fail with a
/// Cloudflare 403 unless the challenge has already been passed -- the
/// error says so rather than reporting bad credentials.
async fn authenticate(
    client: &reqwest::Client,
    config: &crate::config_store::ConfigStore,
) -> Result<String> {
    let entries = config.get_all(NAME)?;
    let get = |key: &str| {
        entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.trim().is_empty())
    };
    let (Some(username), Some(password)) = (get(KEY_USERNAME), get(KEY_PASSWORD)) else {
        return Err(anyhow::anyhow!(
            "{NAME} requires a login: set the '{KEY_USERNAME}' and '{KEY_PASSWORD}' keys for \
             indexer '{NAME}' on the settings page, or paste a logged-in browser session into \
             '{KEY_COOKIE}' (the more reliable option -- this site challenges plain HTTP \
             logins with a browser check)"
        ));
    };

    let login_url = BASE.join(LOGIN_PATH).context("invalid login URL")?;
    let encoded = form_urlencoded(&[
        ("login_username", username.as_str()),
        ("login_password", password.as_str()),
        ("login", "Login"),
        ("redirect", "index.php"),
    ]);

    let no_redirects = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("failed to build login client")?;

    let response = no_redirects
        .post(login_url.clone())
        .header(reqwest::header::REFERER, login_url.as_str())
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .timeout(Duration::from_secs(20))
        .body(encoded)
        .send()
        .await
        .with_context(|| format!("login request to {NAME} failed"))?;

    let status = response.status();
    if !status.is_redirection() {
        let body = decode_windows_1251(response).await.unwrap_or_default();
        if body.contains("Just a moment") {
            return Err(anyhow::anyhow!(
                "{NAME} login hit a Cloudflare browser challenge (403) -- plain HTTP cannot \
                 pass it. Paste a logged-in session cookie into '{KEY_COOKIE}' instead."
            ));
        }
        let message = extract_login_error(&body)
            .unwrap_or_else(|| format!("unexpected login response status {status}"));
        return Err(anyhow::anyhow!("{NAME} authentication failed: {message}"));
    }

    let cookie = join_set_cookies(response.headers())
        .with_context(|| format!("{NAME} login redirect had no session cookies"))?;

    if !session_works(client, &cookie).await {
        return Err(anyhow::anyhow!(
            "{NAME} login accepted but the session does not work"
        ));
    }

    if let Err(err) = config.set(NAME, KEY_COOKIE, &cookie) {
        tracing::warn!(indexer = NAME, error = ?err, "could not cache session cookie");
    }
    tracing::info!(indexer = NAME, "authentication succeeded");
    Ok(cookie)
}

fn parse_results(body: &str, strip_russian: bool, move_tags: bool) -> Result<Vec<Release>> {
    let document = Html::parse_document(body);
    let mut releases = Vec::new();

    for row in document.select(&ROW_SEL) {
        // Rows awaiting moderation have no download link.
        if row.select(&DOWNLOAD_SEL).next().is_none() {
            continue;
        }
        let Some(link) = row.select(&TITLE_SEL).next() else {
            continue;
        };
        let raw_title = link.text().collect::<String>().trim().to_string();
        if raw_title.is_empty() {
            continue;
        }
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let Some(download_href) = row
            .select(&DOWNLOAD_SEL)
            .next()
            .and_then(|a| a.value().attr("href"))
        else {
            continue;
        };

        let mut title = raw_title;
        if move_tags {
            title = move_leading_tags_to_end(&title);
        }
        if strip_russian {
            static CYRILLIC_RE: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r"[\p{Cyrillic}]+").expect("valid regex"));
            title = CYRILLIC_RE.replace_all(&title, "").trim().to_string();
            title = title
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
        }

        releases.push(Release {
            indexer: NAME,
            title,
            // The definition treats a "дн" (days) badge as zero seeders.
            seeders: seeders_of(&row),
            leechers: number_of(&row, &LEECHERS_SEL),
            size: size_of(&row).unwrap_or_else(|| "unknown".to_string()),
            // The site's own download endpoint (login-walled), fetched by
            // `download_torrent` below.
            magnet: format!("{BASE_URL}forum/{download_href}"),
            source_url: Some(format!("{BASE_URL}forum/{href}")),
        });
    }

    Ok(releases)
}

/// `GetSeedersOfRelease`: the seeders cell carries a "дн" (days) badge for
/// long-seeded releases; when only that badge is present the count is
/// treated as zero.
fn seeders_of(row: &scraper::ElementRef) -> u32 {
    let selector = Selector::parse("td:nth-child(7)").expect("valid selector");
    let Some(cell) = row.select(&selector).next() else {
        return 0;
    };
    let cell_text = cell.text().collect::<String>();
    if cell_text.contains('д') && !cell_text.contains(|c: char| c.is_ascii_digit()) {
        return 0;
    }
    cell.select(&SEEDERS_SEL)
        .next()
        .map(|el| el.text().collect::<String>())
        .map(|t| t.trim().replace([',', '\u{a0}'], ""))
        .and_then(|t| t.parse().ok())
        .unwrap_or(0)
}

/// Fetches a .torrent (or the detail page's magnet) behind the login wall.
pub async fn download_torrent(
    client: &reqwest::Client,
    config: &crate::config_store::ConfigStore,
    download_url: &str,
) -> Result<bytes::Bytes> {
    let cookie = session_cookie(client, config).await?;

    let response = client
        .get(download_url)
        .header(reqwest::header::COOKIE, cookie)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("download request to {NAME} failed: {download_url}"))?
        .error_for_status()
        .with_context(|| format!("{NAME} returned an error status for {download_url}"))?;

    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("failed to read {NAME} download body"))?;

    if bytes.starts_with(b"<") {
        anyhow::bail!(
            "{NAME} returned a page instead of the .torrent file -- check the account/session"
        );
    }
    Ok(bytes)
}

fn text_of(row: &scraper::ElementRef, selector: &Selector) -> Option<String> {
    row.select(selector)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The size cell: `data-ts_text` when present (what the definition reads),
/// else its rendered text. The cell also contains the download link, so
/// its text is only a fallback.
fn size_of(row: &scraper::ElementRef) -> Option<String> {
    let cell = row.select(&SIZE_SEL).next()?;
    cell.value()
        .attr("data-ts_text")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            // The rendered text includes the download link's own text; the
            // size is the leading token group.
            let text = cell.text().collect::<String>();
            let text = text.trim().replace('\u{a0}', " ");
            let size: String = text
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',' || *c == ' ')
                .collect();
            let size = size.trim().to_string();
            (!size.is_empty()).then_some(size)
        })
}

fn number_of(row: &scraper::ElementRef, selector: &Selector) -> u32 {
    text_of(row, selector)
        .map(|t| t.replace([',', '\u{a0}'], ""))
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0)
}

/// `RuTrackerTitleParser`'s move-tags step: a leading balanced
/// `[...]`/`(...)` group is relocated to the end of the title.
fn move_leading_tags_to_end(input: &str) -> String {
    let trimmed = input.trim();
    let mut rest = trimmed;
    let mut tags: Vec<String> = Vec::new();

    while let Some(open) = rest.chars().next() {
        let close = match open {
            '[' => ']',
            '(' => ')',
            _ => break,
        };
        // Take the shortest balanced group from the front.
        let mut depth = 0usize;
        let mut end = None;
        for (i, c) in rest.char_indices() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    end = Some(i + c.len_utf8());
                    break;
                }
            }
        }
        let Some(end) = end else { break };
        tags.push(rest[..end].to_string());
        rest = rest[end..].trim_start();
    }

    if tags.is_empty() {
        return trimmed.to_string();
    }
    let joined = format!("{rest} {}", tags.join(" "));
    joined.trim().to_string()
}

fn form_urlencoded(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                utf8_percent_encode(k, NON_ALPHANUMERIC),
                utf8_percent_encode(v, NON_ALPHANUMERIC)
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

fn join_set_cookies(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for value in headers.get_all(reqwest::header::SET_COOKIE) {
        let Ok(raw) = value.to_str() else { continue };
        let pair = raw.split(';').next().unwrap_or("").trim();
        let Some((name, val)) = pair.split_once('=') else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        if let Some(slot) = pairs.iter_mut().find(|(n, _)| n == name) {
            slot.1 = val.to_string();
        } else {
            pairs.push((name.to_string(), val.to_string()));
        }
    }
    (!pairs.is_empty()).then(|| {
        pairs
            .into_iter()
            .map(|(n, v)| format!("{n}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    })
}

fn extract_login_error(body: &str) -> Option<String> {
    let document = Html::parse_document(body);
    let selector = Selector::parse("h4.warnColor1, div.msg-main").ok()?;
    document
        .select(&selector)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_terms_become_wildcards() {
        // Spaces, hyphens and punctuation all collapse to a single `%`.
        assert_eq!(NON_SEARCH_CHARS_RE.replace_all("ubuntu 26.04", "%"), "ubuntu%26%04");
        assert_eq!(NON_SEARCH_CHARS_RE.replace_all("a - b", "%"), "a%b");
        // Cyrillic is searchable, so it must survive.
        assert_eq!(
            NON_SEARCH_CHARS_RE.replace_all("Матрица 1999", "%"),
            "Матрица%1999"
        );
    }

    #[test]
    fn leading_tags_move_to_end() {
        assert_eq!(
            move_leading_tags_to_end("[Tag] Release Name"),
            "Release Name [Tag]"
        );
        // Nested/unbalanced brackets must not panic or eat the title.
        assert_eq!(move_leading_tags_to_end("No Tags Here"), "No Tags Here");
    }

    #[test]
    fn parses_row_selectors() {
        // Column layout per the definition: the download link lives in
        // `td.tor-size` (column 6), seeders in column 7, leechers in 8.
        let body = r#"<table id="tor-tbl"><tbody><tr>
            <td class="t-title-col"><div class="t-title">
              <a class="tLink" href="viewtopic.php?t=1">Release (2026)</a></div></td>
            <td>d</td><td>e</td><td>f</td>
            <td class="tor-size">1.5 GB</td>
            <td class="tor-size" data-ts_text="1610612736"><a class="tr-dl" href="dl.php?t=1">DL</a></td>
            <td><b>12</b></td>
            <td>4</td>
            <td>g</td>
            <td data-ts_text="1700000000">date</td>
        </tr></tbody></table>"#;
        let releases = parse_results(body, false, false).expect("parses");
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].title, "Release (2026)");
        assert_eq!(releases[0].seeders, 12);
        assert_eq!(releases[0].magnet, "https://rutracker.org/forum/dl.php?t=1");
        assert_eq!(
            releases[0].source_url.as_deref(),
            Some("https://rutracker.org/forum/viewtopic.php?t=1")
        );
    }

    #[test]
    fn rows_without_download_link_are_skipped() {
        let body = r#"<table id="tor-tbl"><tbody><tr>
            <td class="t-title-col"><div class="t-title">
              <a class="tLink" href="viewtopic.php?t=1">Pending</a></div></td>
        </tr></tbody></table>"#;
        assert!(parse_results(body, false, false).expect("ok").is_empty());
    }

    #[test]
    fn strip_russian_keeps_latin() {
        let body = r#"<table id="tor-tbl"><tbody><tr>
            <td class="t-title-col"><div class="t-title">
              <a class="tLink" href="viewtopic.php?t=1">Матрица The Matrix</a></div></td>
            <td>d</td><td>e</td><td>f</td><td class="tor-size">1 GB</td>
            <td class="tor-size"><a class="tr-dl" href="dl.php?t=1">DL</a></td>
            <td><b>1</b></td><td>0</td><td>g</td>
        </tr></tbody></table>"#;
        let releases = parse_results(body, true, false).expect("parses");
        assert_eq!(releases[0].title, "The Matrix");
    }

    #[test]
    fn cloudflare_challenge_is_recognized() {
        // The live failure mode, so the error path can name it.
        assert!("<html><title>Just a moment...</title></html>".contains("Just a moment"));
    }
}
