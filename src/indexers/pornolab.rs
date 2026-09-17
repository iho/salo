//! A real indexer for pornolab.net -- a semi-private Russian phpBB
//! tracker (adult content).
//!
//! NOT VERIFIED LIVE. This was written from the local Prowlarr definition
//! (`PornoLab.cs`) but could not be exercised end to end: the site only
//! serves search results to a logged-in session, and no account was
//! available to test with. Everything below -- selectors, query
//! parameters, the login form fields -- mirrors the definition as
//! written, but treat the parse as unproven until a real search with real
//! credentials has been seen to work.
//!
//! The site is served in **windows-1251**, not UTF-8, so responses are
//! decoded explicitly (the shared reqwest client's `.text()` would mangle
//! every Cyrillic title in a row).
//!
//! Auth: phpBB, POST to `forum/login.php` with `login_username` /
//! `login_password` / `login=Login`, and the session is confirmed by the
//! landing page containing the site's "logged in as" banner -- the same
//! shape as toloka's login (see `toloka.rs`).

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use regex::Regex;
use scraper::{Html, Selector};

use super::Release;

pub const NAME: &str = "pornolab";

const BASE_URL: &str = "https://pornolab.net/";
static BASE: LazyLock<reqwest::Url> =
    LazyLock::new(|| reqwest::Url::parse(BASE_URL).expect("valid base URL"));

const LOGIN_PATH: &str = "forum/login.php";
const SEARCH_PATH: &str = "forum/tracker.php";

pub const KEY_USERNAME: &str = "username";
pub const KEY_PASSWORD: &str = "password";
pub const KEY_COOKIE: &str = "cookie_header";
pub const KEY_STRIP_RUSSIAN: &str = "strip_russian";

/// phpBB reports a logged-in session with this banner; its absence means
/// the response is a guest view (see `CheckIfLoginNeeded`).
const LOGGED_IN_MARKER: &str = "Вы зашли как:";

pub fn settings_fields() -> Vec<super::SettingField> {
    vec![
        super::SettingField {
            label: "Username",
            key: KEY_USERNAME,
            kind: super::SettingFieldKind::Text,
            help: "Your pornolab.net account login.",
            default_on: false,
        },
        super::SettingField {
            label: "Password",
            key: KEY_PASSWORD,
            kind: super::SettingFieldKind::Password,
            help: "Your pornolab.net account password (stored locally in SQLite).",
            default_on: false,
        },
        super::SettingField {
            label: "Session cookie",
            key: KEY_COOKIE,
            kind: super::SettingFieldKind::Password,
            help: "Optional: paste a browser session's Cookie header to skip logging in. \
                   Filled in automatically after a successful login.",
            default_on: false,
        },
        super::SettingField {
            label: "Strip Russian letters",
            key: KEY_STRIP_RUSSIAN,
            kind: super::SettingFieldKind::Checkbox,
            help: "Strip Cyrillic letters from release names, leaving the Latin part.",
            default_on: false,
        },
    ]
}

/// `PornoLabParser.StripRussianRegex`, ported literally. `\p{IsCyrillic}`
/// in .NET is `\p{Cyrillic}` in the `regex` crate.
static STRIP_RUSSIAN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\r?\n|\([\p{Cyrillic}\W]+\))|(^[\p{Cyrillic}\W\d]+/ )|([\p{Cyrillic} \-]+,+)|([\p{Cyrillic}]+)")
        .expect("valid regex")
});

static ROW_SEL: LazyLock<Selector> = LazyLock::new(|| {
    Selector::parse("table#tor-tbl > tbody > tr").expect("valid selector")
});
static DOWNLOAD_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.tr-dl").expect("valid selector"));
static DETAILS_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.tLink").expect("valid selector"));
static SIZE_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(6) u").expect("valid selector"));
static SEEDERS_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(7) b").expect("valid selector"));
static LEECHERS_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("td:nth-child(8)").expect("valid selector"));

pub async fn search(
    client: &reqwest::Client,
    config: &crate::config_store::ConfigStore,
    query: &str,
) -> Result<Vec<Release>> {
    let strip_russian = config
        .get_all(NAME)?
        .iter()
        .find(|(k, _)| k == KEY_STRIP_RUSSIAN)
        .map(|(_, v)| matches!(v.trim(), "1" | "true" | "on" | "yes"))
        .unwrap_or(false);

    let cookie = session_cookie(client, config).await?;

    // `GetPagedRequests`: `o=1`, `s=2` (sort by seeders, descending), and
    // hyphens in the term become spaces.
    let term = query.trim().replace('-', " ");
    let url = format!(
        "{BASE_URL}{SEARCH_PATH}?o=1&s=2&nm={}",
        utf8_percent_encode(&term, NON_ALPHANUMERIC)
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
        anyhow::bail!("{NAME} returned {status}");
    }
    // A guest view of the tracker means the session died -- say so rather
    // than reporting "no results", which is what an empty parse would
    // otherwise look like.
    if !body.contains(LOGGED_IN_MARKER) {
        anyhow::bail!(
            "{NAME} served a guest page -- the session is no longer valid; \
             check the account or paste a fresh cookie"
        );
    }

    parse_results(&body, strip_russian)
}

/// Reads (and decodes) a windows-1251 response body.
///
/// `reqwest::Response::text()` assumes UTF-8 and does not consult a
/// declared charset, so these pages are read as bytes and decoded with
/// `encoding_rs` instead -- otherwise every Cyrillic title arrives as
/// replacement characters.
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

/// Returns a working `Cookie` header, logging in if the stored one isn't
/// usable. Mirrors toloka's flow: try the stored cookie, then a fresh
/// login.
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
    decode_windows_1251(response)
        .await
        .map(|body| body.contains(LOGGED_IN_MARKER))
        .unwrap_or(false)
}

/// `PornoLab.DoLogin`: POST the phpBB form and keep the session cookies.
/// The POST runs on a no-redirect client so the `Set-Cookie` headers on
/// the 302 are readable (the shared client has no cookie store).
async fn authenticate(
    client: &reqwest::Client,
    config: &crate::config_store::ConfigStore,
) -> Result<String> {
    let settings: Vec<(String, String)> = config.get_all(NAME)?;
    let get = |key: &str| {
        settings
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.trim().is_empty())
    };
    let (Some(username), Some(password)) = (get(KEY_USERNAME), get(KEY_PASSWORD)) else {
        return Err(anyhow::anyhow!(
            "{NAME} requires a login: set the '{KEY_USERNAME}' and '{KEY_PASSWORD}' keys for \
             indexer '{NAME}' on the settings page (or paste a browser session into \
             '{KEY_COOKIE}')"
        ));
    };

    let login_url = BASE.join(LOGIN_PATH).context("invalid login URL")?;
    let encoded = form_urlencoded(&[
        ("login_username", username.as_str()),
        ("login_password", password.as_str()),
        ("login", "Login"),
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

fn parse_results(body: &str, strip_russian: bool) -> Result<Vec<Release>> {
    let document = Html::parse_document(body);
    let mut releases = Vec::new();

    for row in document.select(&ROW_SEL) {
        // A row with no download link is one awaiting moderation.
        if row.select(&DOWNLOAD_SEL).next().is_none() {
            continue;
        }

        let Some(link) = row.select(&DETAILS_SEL).next() else {
            continue;
        };
        let raw_title = link.text().collect::<String>().trim().to_string();
        if raw_title.is_empty() {
            continue;
        }
        let Some(href) = link.value().attr("href") else {
            continue;
        };

        let title = if strip_russian {
            STRIP_RUSSIAN_RE.replace_all(&raw_title, "").trim().to_string()
        } else {
            raw_title
        };

        releases.push(Release {
            indexer: NAME,
            title,
            seeders: number_of(&row, &SEEDERS_SEL),
            leechers: number_of(&row, &LEECHERS_SEL),
            size: row
                .select(&SIZE_SEL)
                .next()
                .map(|el| el.text().collect::<String>().trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown".to_string()),
            // `DownloadUrl`: the site's own download endpoint, keyed by
            // the topic id -- fetched with the session by
            // `download_torrent` below.
            magnet: format!(
                "{BASE_URL}forum/dl.php?t={}",
                query_arg(href, "t").unwrap_or_default()
            ),
            source_url: Some(format!("{BASE_URL}forum/{href}")),
        });
    }

    Ok(releases)
}

/// Fetches a .torrent file behind the login wall, for `TorrentEngine::add_bytes`.
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
        anyhow::bail!("{NAME} returned a login page instead of the .torrent file -- check the account");
    }
    Ok(bytes)
}

fn query_arg(query: &str, key: &str) -> Option<String> {
    query
        .split(['?', '&'])
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
}

fn number_of(row: &scraper::ElementRef, selector: &Selector) -> u32 {
    row.select(selector)
        .next()
        .map(|el| el.text().collect::<String>())
        .map(|t| t.trim().replace([',', '\u{a0}'], ""))
        .and_then(|t| t.parse().ok())
        .unwrap_or(0)
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

/// Rebuilds a `Cookie` header from `Set-Cookie` headers; last value wins
/// per name, like a real cookie jar (phpBB sets its session cookie twice
/// on login, guest then authenticated).
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

/// `CheckIfLoginNeeded`'s error extraction: the site's warning heading.
fn extract_login_error(body: &str) -> Option<String> {
    let document = Html::parse_document(body);
    let selector = Selector::parse("h4.warnColor1").ok()?;
    document
        .select(&selector)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shaped after the definition's selectors: `table#tor-tbl` rows, a
    // `a.tr-dl` download link, `a.tLink` title, size in `td:nth-child(6)
    // u`, seeders in `td:nth-child(7) b`, leechers in `td:nth-child(8)`.
    const SAMPLE: &str = r#"<html><body>
      <table id="tor-tbl" class="forumline"><tbody>
        <tr>
          <td class="tLeft"><a class="f" href="tracker.php?f=1670">Эротика</a></td>
          <td><a class="tLink" href="viewtopic.php?t=1234567">Название релиза (Russian) [2026]</a></td>
          <td>x</td><td>y</td>
          <td>z</td>
          <td class="tor-size"><u>2.5 GB</u> <a class="tr-dl" href="dl.php?t=1234567">DL</a></td>
          <td><b>42</b></td>
          <td>7</td>
          <td>3</td>
          <td>w</td>
          <td><u>1700000000</u></td>
        </tr>
        <tr>
          <!-- awaiting moderation: no download link -->
          <td><a class="tLink" href="viewtopic.php?t=999">Pending release</a></td>
        </tr>
      </tbody></table>
    </body></html>"#;

    #[test]
    fn parses_row_and_builds_download_url() {
        let releases = parse_results(SAMPLE, false).expect("parses");
        assert_eq!(releases.len(), 1, "moderation row must be skipped");
        let r = &releases[0];
        assert_eq!(r.title, "Название релиза (Russian) [2026]");
        assert_eq!(r.seeders, 42);
        assert_eq!(r.leechers, 7);
        assert_eq!(r.size, "2.5 GB");
        // The topic id from the details href drives dl.php.
        assert_eq!(r.magnet, "https://pornolab.net/forum/dl.php?t=1234567");
        assert_eq!(
            r.source_url.as_deref(),
            Some("https://pornolab.net/forum/viewtopic.php?t=1234567")
        );
    }

    #[test]
    fn strip_russian_removes_cyrillic_when_enabled() {
        let releases = parse_results(SAMPLE, true).expect("parses");
        assert!(
            !releases[0].title.contains('Н'),
            "cyrillic survived: {}",
            releases[0].title
        );
        // The Latin part is kept.
        assert!(releases[0].title.contains("[2026]"));
    }

    #[test]
    fn missing_leecher_cell_is_not_fatal() {
        // A row truncated before the leechers cell (column 8) must not
        // panic -- leechers read as zero while seeders still parse.
        let body = r#"<table id="tor-tbl"><tbody><tr>
            <td><a class="tLink" href="viewtopic.php?t=1">T</a></td>
            <td>a</td><td>b</td><td>c</td><td>d</td>
            <td class="tor-size"><u>1 GB</u><a class="tr-dl" href="dl.php?t=1">DL</a></td>
            <td><b>5</b></td>
        </tr></tbody></table>"#;
        let releases = parse_results(body, false).expect("parses");
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].seeders, 5);
        assert_eq!(releases[0].leechers, 0);
    }

    #[test]
    fn guest_page_is_detected_by_missing_marker() {
        // The search path refuses to report "no results" for a guest.
        assert!(!LOGGED_IN_MARKER.is_empty());
        assert!(parse_results("<html>guest</html>", false).unwrap().is_empty());
    }

    #[test]
    fn query_arg_extracts_topic_id() {
        assert_eq!(
            query_arg("viewtopic.php?t=1234567&x=1", "t").as_deref(),
            Some("1234567")
        );
        assert_eq!(query_arg("viewtopic.php?x=1", "t"), None);
    }
}
