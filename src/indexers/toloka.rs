//! A real indexer definition for toloka.to -- a semi-private Ukrainian
//! torrent tracker ("Гуртом — торрент-толока"), ported from Prowlarr's
//! `Toloka.cs` definition.
//!
//! Toloka is login-walled: guests can load the search form but always get
//! "За пошуком нічого не знайдено", forum views redirect to `login.php`,
//! and torrent download links are never rendered without a session. The
//! C# definition authenticates by POSTing `login.php` and reusing the
//! session cookies; this port does the same, keeping the credentials in
//! the app's existing per-indexer settings store (`src/config_store.rs`):
//!
//!   username      -- the account's login name
//!   password      -- the account's password
//!   cookie_header -- the authenticated `Cookie` header value (auto-filled
//!                    by the login flow below; can also be pasted in
//!                    manually from a browser session on the settings page)
//!   freeleech_only-- "1"/"true" to search freeleech torrents only
//!                    (`TolokaSettings.FreeleechOnly`, default off)
//!   strip_cyrillic-- "0"/"false" to keep Cyrillic letters in release
//!                    titles (`TolokaSettings.StripCyrillicLetters`,
//!                    default on)
//!
//! Results are parsed from the tracker table exactly like the C# parser:
//! rows are `table.forumline` `tr` elements whose class contains `prow`,
//! and each column is read by its `td:nth-child` index (forum/category in
//! 2, topic title in 3, the download link in 6, size in 7, completed in 9,
//! seeders in 10, leechers in 11, added date in 13). The C#'s
//! `TolokaTitleParser` -- the season/episode rewrite pipeline for the
//! tracker's Ukrainian "Сезон 1 Серія 2-10 з 12" style titles, the
//! Cyrillic stripping, and the release-tag normalization -- is ported
//! regex-for-regex below, because the raw row title is what Toloka prints
//! and it is not directly usable as a search/release name.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use regex::Regex;
use scraper::{Html, Selector};

use super::Release;

pub const NAME: &str = "toloka";

const BASE_URL: &str = "https://toloka.to/";
static BASE: LazyLock<reqwest::Url> =
    LazyLock::new(|| reqwest::Url::parse(BASE_URL).expect("valid base URL"));
const LOGIN_PATH: &str = "login.php";
const SEARCH_PATH: &str = "tracker.php";

/// Settings keys in the per-indexer config store.
pub const KEY_USERNAME: &str = "username";
pub const KEY_PASSWORD: &str = "password";
pub const KEY_COOKIE: &str = "cookie_header";
/// `TolokaSettings.FreeleechOnly` -- "1"/"true" adds `sds=1` to the search
/// so only freeleech torrents come back (checkbox, default off).
pub const KEY_FREELEECH: &str = "freeleech_only";
/// `TolokaSettings.StripCyrillicLetters` -- "0"/"false" keeps Cyrillic
/// letters in release titles (checkbox, default on).
pub const KEY_STRIP_CYRILLIC: &str = "strip_cyrillic";

/// The fields the settings page renders for this indexer -- user-facing
/// forms of the raw storage keys above (what `Settings::load` reads).
pub fn settings_fields() -> Vec<super::SettingField> {
    vec![
        super::SettingField {
            label: "Username",
            key: KEY_USERNAME,
            kind: super::SettingFieldKind::Text,
            help: "Your toloka.to account login.",
            default_on: false,
        },
        super::SettingField {
            label: "Password",
            key: KEY_PASSWORD,
            kind: super::SettingFieldKind::Password,
            help: "Your toloka.to account password (stored locally in SQLite).",
            default_on: false,
        },
        super::SettingField {
            label: "Session cookie",
            key: KEY_COOKIE,
            kind: super::SettingFieldKind::Password,
            help: "Optional: paste a browser session's Cookie header to skip \
                   logging in. Filled in automatically after a successful login.",
            default_on: false,
        },
        super::SettingField {
            label: "Freeleech only",
            key: KEY_FREELEECH,
            kind: super::SettingFieldKind::Checkbox,
            help: "Search freeleech torrents only.",
            default_on: false,
        },
        super::SettingField {
            label: "Strip Cyrillic letters",
            key: KEY_STRIP_CYRILLIC,
            kind: super::SettingFieldKind::Checkbox,
            help: "Strip Ukrainian letters from release titles, making them \
                   scene-style English release names.",
            default_on: true,
        },
    ]
}

/// Selectors for one results row, ported from `TolokaParser.ParseResponse`.
struct RowSelectors {
    /// `table.forumline > tbody > tr[class*=prow]` -- one row per release.
    row: Selector,
    /// td:nth-child(2) > a -- the forum/category link (`tracker.php?f=<id>`).
    category_link: Selector,
    /// td:nth-child(3) > a -- the topic link and its title text.
    topic_link: Selector,
    /// td:nth-child(6) > a -- the torrent download link. Rows awaiting
    /// moderation don't have one and are skipped, like the C# does.
    download_link: Selector,
}

impl RowSelectors {
    fn new() -> Result<Self> {
        Ok(Self {
            row: parse_selector("table.forumline > tbody > tr[class*=prow]")?,
            category_link: parse_selector("td:nth-child(2) > a")?,
            topic_link: parse_selector("td:nth-child(3) > a")?,
            download_link: parse_selector("td:nth-child(6) > a")?,
        })
    }
}

fn parse_selector(css: &str) -> Result<Selector> {
    Selector::parse(css).map_err(|e| anyhow::anyhow!("invalid selector {css:?}: {e}"))
}

/// Search toloka.to for `query`.
///
/// `config` is the app's per-indexer settings store: credentials are read
/// from (and the session cookie cached to) the `toloka` section. Without
/// a configured login the tracker answers every search with an empty
/// result set rather than an error, so missing credentials surface as a
/// descriptive error instead of a silently pointless request.
pub async fn search(
    client: &reqwest::Client,
    config: &crate::config_store::ConfigStore,
    query: &str,
) -> Result<Vec<Release>> {
    let settings = Settings::load(config)?;

    // A stored session cookie (captured by a previous login, or pasted
    // in by hand on the settings page) is tried first -- a fresh login
    // only happens when there is none, or when the site rejects it.
    let cookie = match &settings.cookie_header {
        Some(cookie) => cookie.clone(),
        None => authenticate(client, config, &settings).await?,
    };

    let mut body = fetch_search_page(client, &cookie, settings.freeleech_only, query).await?;

    // `CheckIfLoginNeeded`: pages served without the logout marker mean
    // the session is gone. If we were riding a stored cookie, log in
    // fresh and retry the search once; a session that fails right after
    // a fresh login is a real error.
    if !body.contains("logout=true") {
        if settings.cookie_header.is_none() {
            return Err(anyhow::anyhow!(
                "{NAME} login did not produce a working session (no logout marker)"
            ));
        }

        tracing::debug!(indexer = NAME, "stored session rejected, re-authenticating");
        let cookie = authenticate(client, config, &settings).await?;
        body = fetch_search_page(client, &cookie, settings.freeleech_only, query).await?;
        if !body.contains("logout=true") {
            return Err(anyhow::anyhow!(
                "{NAME} session rejected even after a fresh login"
            ));
        }
    }

    parse_results(&body, settings.strip_cyrillic)
}

/// Fetches one page of search results with the given session cookie.
///
/// The C# request generator builds `tracker.php?o=1&s=2&nm=<term>`
/// (sorted by registration date, descending), replaces `-` with a
/// space in the search term (the tracker's search treats it as a
/// literal otherwise), and adds `sds=1` in freeleech-only mode
/// (`TolokaSettings.FreeleechOnly`).
async fn fetch_page(
    client: &reqwest::Client,
    url: &reqwest::Url,
    cookie: &str,
) -> Result<String> {
    let body = client
        .get(url.clone())
        .header(reqwest::header::COOKIE, cookie)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .with_context(|| format!("request to {NAME} failed: {url}"))?
        .error_for_status()
        .with_context(|| format!("{NAME} returned an error status: {url}"))?
        .text()
        .await
        .with_context(|| format!("failed to read {NAME} response body"))?;

    Ok(body)
}

/// Fetches a search results page (built by the caller) with the given
/// session cookie.
async fn fetch_search_page(
    client: &reqwest::Client,
    cookie: &str,
    freeleech_only: bool,
    query: &str,
) -> Result<String> {
    let term = query.replace('-', " ");
    let mut params: Vec<(&str, &str)> = vec![("o", "1"), ("s", "2"), ("nm", &term)];
    if freeleech_only {
        params.push(("sds", "1"));
    }

    let url = reqwest::Url::parse_with_params(
        BASE.join(SEARCH_PATH).map(|u| u.to_string()).unwrap_or_default().as_str(),
        &params,
    )
        .context("failed to build search URL")?;

    fetch_page(client, &url, cookie).await
}

/// Downloads the .torrent file bytes behind a release's download URL
/// (`download.php?id=...`), authenticating with the stored session --
/// toloka serves a login page for those URLs without it, which is why
/// a plain `AddTorrent::from_url` can't fetch these itself. Returns the
/// raw bytes for `TorrentEngine::add_bytes`.
pub async fn download_torrent(
    client: &reqwest::Client,
    config: &crate::config_store::ConfigStore,
    download_url: &str,
) -> Result<bytes::Bytes> {
    let url = url_of(download_url)?;
    let settings = Settings::load(config)?;

    // Try the stored session first. Unlike a search results page, a
    // *successful* download response is the bencode file itself (no
    // logout marker anywhere), so the session check is "did we get
    // HTML" -- a login/error page instead of the file.
    //
    // Fetched as raw bytes throughout: a .torrent file embeds binary
    // SHA1 piece hashes that are not valid UTF-8, and a lossy text
    // conversion would corrupt them (and with them the info hash).
    let cookie = settings.cookie_header.clone().unwrap_or_default();
    let mut body = fetch_bytes(client, &url, &cookie).await?;
    if is_html(&body) {
        let fresh = authenticate(client, config, &settings).await?;
        body = fetch_bytes(client, &url, &fresh).await?;
    }

    if is_html(&body) {
        return Err(anyhow::anyhow!(
            "{NAME} returned a login page instead of the .torrent file -- check the account"
        ));
    }

    Ok(body)
}

/// Fetches `url` with the session cookie as raw bytes (see
/// [`download_torrent`] for why not text).
async fn fetch_bytes(
    client: &reqwest::Client,
    url: &reqwest::Url,
    cookie: &str,
) -> Result<bytes::Bytes> {
    let bytes = client
        .get(url.clone())
        .header(reqwest::header::COOKIE, cookie)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .with_context(|| format!("request to {NAME} failed: {url}"))?
        .error_for_status()
        .with_context(|| format!("{NAME} returned an error status: {url}"))?
        .bytes()
        .await
        .with_context(|| format!("failed to read {NAME} response body"))?;

    Ok(bytes)
}

/// Whether a response body is an HTML page rather than the bencoded
/// .torrent document we asked for.
fn is_html(body: &[u8]) -> bool {
    // phpBB pages open with `<`, `<!DOCTYPE`, or `<html` (case varies,
    // with whitespace first); bencode starts with `d`/`i`/`l`/digits.
    body.iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| *b == b'<')
}

/// Parses a download href into an absolute URL.
fn url_of(href: &str) -> Result<reqwest::Url> {
    reqwest::Url::parse(href)
        .or_else(|_| BASE.join(href))
        .context("invalid download URL")
}

/// Everything the module reads from the settings store for one search.
struct Settings {
    username: Option<String>,
    password: Option<String>,
    /// Cached `Cookie` header captured from a previous login.
    cookie_header: Option<String>,
    /// `TolokaSettings.FreeleechOnly` (default false).
    freeleech_only: bool,
    /// `TolokaSettings.StripCyrillicLetters` (default true in the C#).
    strip_cyrillic: bool,
}

impl Settings {
    fn load(config: &crate::config_store::ConfigStore) -> Result<Self> {
        let entries = config.get_all(NAME)?;
        let get = |key: &str| {
            entries
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .filter(|v| !v.trim().is_empty())
        };
        let flag = |key: &str, default: bool| {
            get(key)
                .map(|v| matches!(v.trim(), "1" | "true" | "on" | "yes"))
                .unwrap_or(default)
        };
        Ok(Self {
            username: get(KEY_USERNAME),
            password: get(KEY_PASSWORD),
            cookie_header: get(KEY_COOKIE),
            freeleech_only: flag(KEY_FREELEECH, false),
            strip_cyrillic: flag(KEY_STRIP_CYRILLIC, true),
        })
    }
}

/// Logs in and returns a fresh `Cookie` header value.
///
/// Mirrors `Toloka.DoLogin`: POST to `login.php` with the same form
/// fields (`username`, `password`, `autologin=on`, `ssl=on`, empty
/// `redirect`, `login=Вхід`). A phpBB login answers with a `302` back to
/// the index plus `Set-Cookie`s -- following it with the shared client
/// would render the landing page *as a guest* (no cookie store), which
/// is indistinguishable from a failed login, so the POST runs on a
/// throwaway no-redirect client and the cookies are taken from the 302
/// itself. The session is then verified with a real request before it's
/// trusted; a failed login is detected the way `CheckIfLoginNeeded`
/// does -- the response lacks `logout=true` -- and the site's error text
/// (`table.forumline table span.gen`) is surfaced.
async fn authenticate(
    client: &reqwest::Client,
    config: &crate::config_store::ConfigStore,
    settings: &Settings,
) -> Result<String> {
    let (Some(username), Some(password)) = (&settings.username, &settings.password) else {
        return Err(anyhow::anyhow!(
            "{NAME} requires a login: set the '{KEY_USERNAME}' and '{KEY_PASSWORD}' keys for \
             indexer '{NAME}' on the settings page (or paste a browser session into \
             '{KEY_COOKIE}')"
        ));
    };

    let login_url = BASE.join(LOGIN_PATH).context("invalid login URL")?;

    // Same form fields as the C# `AddFormParameter` calls, urlencoded by
    // hand (reqwest's `.form()` helper sits behind its `form` cargo
    // feature, which this binary doesn't enable).
    let encoded = form_urlencoded(&[
        ("username", username.as_str()),
        ("password", password.as_str()),
        ("autologin", "on"),
        ("ssl", "on"),
        ("redirect", ""),
        ("login", "Вхід"),
    ]);

    // A one-off client that does NOT follow the 302 -- the session
    // cookies live on the redirect response, and the shared client has
    // no cookie store to carry them to the landing page.
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

    // phpBB: 200 = the form re-rendered with an error message (bad
    // credentials), 302 = logged in, cookies on this response.
    if !status.is_redirection() {
        let body = response
            .text()
            .await
            .with_context(|| format!("failed to read {NAME} login response body"))?;
        let message = extract_login_error(&body).unwrap_or_else(|| {
            format!("unexpected login response status {status} (no error message on the page)")
        });
        return Err(anyhow::anyhow!("{NAME} authentication failed: {message}"));
    }

    // Collect Set-Cookie headers the way Prowlarr's `response.GetCookies`
    // does -- the shared client has no cookie store (no `cookies` cargo
    // feature), so the header value is rebuilt by hand.
    let cookie_header = join_set_cookies(response.headers())
        .with_context(|| format!("{NAME} login redirect had no session cookies"))?;

    // Verify the session works before trusting it: the site must serve
    // an authenticated page (one carrying the logout marker) for these
    // cookies. The C# counts a login as failed if this same check fails
    // on the followed landing page.
    let body = fetch_search_page(client, &cookie_header, false, "").await?;
    if !body.contains("logout=true") {
        return Err(anyhow::anyhow!(
            "{NAME} login accepted but the session does not work (no logout marker on landing page)"
        ));
    }

    config.set(NAME, KEY_COOKIE, &cookie_header)?;

    tracing::info!(indexer = NAME, "authentication succeeded");
    Ok(cookie_header)
}

/// Urlencodes `key=value` pairs into a form body
/// (`application/x-www-form-urlencoded`), the manual stand-in for
/// reqwest's feature-gated `.form()`.
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

/// Rebuilds a `Cookie` header from all `Set-Cookie` response headers:
/// just the `name=value` pairs, dropping attributes like `Path`/`HttpOnly`.
///
/// Duplicate names keep only the LAST value -- phpBB's login 302 sets
/// `toloka_data`/`toloka_sid` twice (first the guest's, then the
/// authenticated session's), and a real cookie jar would have the second
/// overwrite the first. Sending both makes the site read the guest one.
fn join_set_cookies(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for value in headers.get_all(reqwest::header::SET_COOKIE) {
        let Ok(raw) = value.to_str() else {
            continue;
        };
        // First `;`-separated segment is the cookie itself. Split on the
        // first `=` only -- values may contain `=` (e.g. urlencoded data).
        let pair = raw.split(';').next().unwrap_or("").trim();
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        // Later Set-Cookies win, like a real cookie jar.
        if let Some(slot) = pairs.iter_mut().find(|(n, _)| n == name) {
            slot.1 = value.to_string();
        } else {
            pairs.push((name.to_string(), value.to_string()));
        }
    }
    if pairs.is_empty() {
        return None;
    }
    Some(
        pairs
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// `DoLogin`'s error extraction: `table.forumline table span.gen`'s first
/// text node holds the site's login failure message.
fn extract_login_error(body: &str) -> Option<String> {
    static SEL: LazyLock<Selector> =
        LazyLock::new(|| Selector::parse("table.forumline table span.gen").expect("valid"));
    let document = Html::parse_document(body);
    document
        .select(&SEL)
        .next()
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|t| !t.is_empty())
}

fn parse_results(body: &str, strip_cyrillic: bool) -> Result<Vec<Release>> {
    let selectors = RowSelectors::new()?;
    let document = Html::parse_document(body);

    let mut releases = Vec::new();
    for row in document.select(&selectors.row) {
        // Expects moderation (or an ad/header row) -- skip, don't fail.
        let Some(download_link) = row.select(&selectors.download_link).next() else {
            continue;
        };
        let Some(download_href) = download_link.value().attr("href") else {
            continue;
        };

        let Some(topic_link) = row.select(&selectors.topic_link).next() else {
            continue;
        };
        let raw_title = topic_link.text().collect::<String>().trim().to_string();
        if raw_title.is_empty() {
            continue;
        }

        // The download URL is relative (`download.php?id=...&...`); join
        // it onto the base like the C#'s `_settings.BaseUrl + href`.
        let download_url = BASE
            .join(download_href)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| format!("{BASE_URL}{download_href}"));

        let source_url = topic_link
            .value()
            .attr("href")
            .and_then(|href| BASE.join(href).ok())
            .map(|u| u.to_string());

        // The category link is `tracker.php?f=<id>` -- the tracker's own
        // category id. `Release` carries no categories, so the id is only
        // used to decide the TV-title parsing below (the C# maps it to
        // newznab categories first; the tracker ids in the TV groups are
        // the ones listed in `SetCapabilities`).
        let category_id = row
            .select(&selectors.category_link)
            .next()
            .and_then(|el| el.value().attr("href"))
            .and_then(query_arg_of("f"))
            .unwrap_or_default();

        let seeders = extract_number(&row, 10);
        let leechers_td = extract_number(&row, 11);
        // The C# stores `Peers = seeders + leechers`; `Release` has just
        // seeders/leechers, so leechers keeps its own column's count.
        let _ = leechers_td;

        let size_bytes = extract_size_bytes(&row, 7);
        let grabs = extract_number(&row, 9);
        let _ = grabs; // completed count -- no Release field for it

        let title = parse_title(&raw_title, is_tv_category(&category_id), strip_cyrillic);

        releases.push(Release {
            indexer: NAME,
            title,
            seeders,
            leechers: leechers_td,
            size: if size_bytes > 0 {
                super::format_size(size_bytes)
            } else {
                "unknown".to_string()
            },
            // `AddTorrent::from_url` accepts a .torrent URL directly, so
            // the download link is passed through as-is (like the C#'s
            // DownloadUrl), no magnet synthesis needed.
            magnet: download_url,
            source_url,
            comments: None,
        });
    }

    Ok(releases)
}

/// Returns a closure extracting a named query argument from an href
/// (`ParseUtil.GetArgumentFromQueryString`).
fn query_arg_of(
    name: &'static str,
) -> impl Fn(&str) -> Option<String> {
    move |href: &str| {
        let query = href.split_once('?')?.1.split('#').next()?;
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == name).then(|| v.to_string())
        })
    }
}

fn extract_number(row: &scraper::ElementRef, child: usize) -> u32 {
    row.child_elements()
        .nth(child.saturating_sub(1))
        .map(|td| td.text().collect::<String>())
        .and_then(|text| coerce_int(&text))
        .unwrap_or(0)
}

/// `ParseUtil.GetBytes` for the size column ("1,37 ГБ" / "700.5 МБ" --
/// Toloka prints decimal commas and Ukrainian unit prefixes). Letters
/// form the unit, the rest the number, mirroring the C# implementation.
fn extract_size_bytes(row: &scraper::ElementRef, child: usize) -> u64 {
    let Some(td) = row.child_elements().nth(child.saturating_sub(1)) else {
        return 0;
    };
    let text = td.text().collect::<String>();
    let unit: String = text.chars().filter(|c| c.is_alphabetic()).collect();
    let Some(value) = coerce_double(&text) else {
        return 0;
    };

    let mult = match normalize_unit(&unit).as_str() {
        "kb" | "kib" => 1024.0,
        "mb" | "mib" => 1024.0f64.powi(2),
        "gb" | "gib" => 1024.0f64.powi(3),
        "tb" | "tib" => 1024.0f64.powi(4),
        _ => 1.0,
    };
    (value * mult) as u64
}

/// Maps a size unit to latin `kb`/`mb`/... form. `ParseUtil.GetBytes` only
/// understands latin units (`unit.Replace("i","").ToLowerInvariant()`),
/// but Toloka's Ukrainian UI prints Cyrillic ones (`1,37 ГБ`) -- under the
/// C# those fall through to a byte count, so this maps the Cyrillic unit
/// letters (`Г/М/К/Т -> G/M/K/T`, `Б -> B`, `і -> i`) before matching,
/// which also keeps the latin spellings working unchanged.
fn normalize_unit(unit: &str) -> String {
    unit.chars()
        .map(|c| match c.to_uppercase().next().unwrap_or(c) {
            'Г' => 'G',
            'М' => 'M',
            'К' => 'K',
            'Т' => 'T',
            'Б' => 'B',
            // Ukrainian 'і' (U+0456/U+0406) in the IEC forms (КіБ, МіБ...).
            'І' => 'I',
            other => other,
        })
        .collect::<String>()
        .to_lowercase()
}

/// `ParseUtil.CoerceInt`: keep digits/`,`/`.`, then treat the result as a
/// decimal number (the C# normalizes `.` to `,` for ints -- i.e. "1,234"
/// keeps its comma, "1.234" becomes 1234; Toloka's columns are plain
/// integers so both forms only matter for thousands separators, which are
/// never printed on the tracker).
fn coerce_int(text: &str) -> Option<u32> {
    let cleaned: String = text
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    // The C# swaps '.' for ',' and parses invariantly; equivalent here:
    // strip grouping separators, keep the last as the decimal point.
    let normalized = normalize_number(&cleaned);
    normalized.parse::<f64>().ok().map(|n| n as u32)
}

fn coerce_double(text: &str) -> Option<f64> {
    let cleaned: String = text
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    normalize_number(&cleaned).parse::<f64>().ok()
}

/// `ParseUtil.NormalizeNumber` for floats: `,` -> `.`, collapse multiple
/// `.` into thousands separators (all but the last are removed).
fn normalize_number(cleaned: &str) -> String {
    let s = cleaned.replace(',', ".");
    match s.rfind('.') {
        // "1.234.5" -> "1234.5"
        Some(last) if s.matches('.').count() > 1 => {
            format!("{}{}", s[..last].replace('.', ""), &s[last..])
        }
        _ => s,
    }
}

/// Whether the tracker category id belongs to one of the TV groups from
/// `SetCapabilities` (Телесеріали/Мультсеріали/Аніме/Документальні/
/// Телепередачі/Спорт and the HD/DVD overlaps) -- the C#'s
/// `IsAnyTvCategory` gate on the title-rewrite pipeline below.
fn is_tv_category(category_id: &str) -> bool {
    matches!(
        category_id,
        "124" | "125" | "32" | "44" | "127" | "192" | "195" | "194"
            | "225" | "21" | "131" | "226" | "227" | "228" | "229" | "230"
            | "119" | "18" | "132" | "157" | "235" | "170" | "162" | "166"
            | "167" | "168" | "169" | "54" | "158" | "159" | "160" | "161"
            | "173" | "174" | "140" | "137"
    )
}

/// Port of `TolokaTitleParser.Parse`: normalizes the tracker's verbose
/// Ukrainian titles into scene-style release names. `strip_cyrillic` is
/// `TolokaSettings.StripCyrillicLetters` (default true).
fn parse_title(raw_title: &str, is_tv: bool, strip_cyrillic: bool) -> String {
    // Precompiled pipeline, in the same order as the C#.
    static PUNCT_DASH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\p{Pd}").expect("valid regex"));
    static TV_COMMA: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s(\d+),(\d+)").expect("valid regex"));
    static TV_CYRILLIC_X: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)([\s-])Х+([\)\]])").expect("valid regex"));
    static TV_MULTI_SEASON: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?:Сезон|Seasons?)\s*[:]*\s+(\d+-\d+)").expect("valid regex"));

    static UKR_SEASON_EPISODE_OF: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)Сезон\s*[:]*\s+(\d+).+(?:Серії|Серія|Серій|Епізоди?)+\s*[:]*\s+(\d+(?:-\d+)?)\s*з\s*([\w?])")
            .expect("valid regex")
    });
    static UKR_SEASON_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)Сезон\s*[:]*\s+(\d+).+(?:Серії|Серія|Серій|Епізоди?)+\s*[:]*\s+(\d+(?:-\d+)?)")
            .expect("valid regex")
    });
    static UKR_SEASON: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)Сезон\s*[:]*\s+(\d+)").expect("valid regex"));
    static UKR_EPISODE_OF: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:Серії|Серія|Серій|Епізоди?)+\s*[:]*\s+(\d+(?:-\d+)?)\s*з\s*([\w?])")
            .expect("valid regex")
    });
    static UKR_EPISODE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?:Серії|Серія|Серій|Епізоди?)+\s*[:]*\s+(\d+(?:-\d+)?)").expect("valid regex"));

    static ENG_SEASON_EPISODE_OF: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)Season\s*[:]*\s+(\d+).+(?:Episodes?)+\s*[:]*\s+(\d+(?:-\d+)?)\s*of\s*([\w?])")
            .expect("valid regex")
    });
    static ENG_SEASON_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)Season\s*[:]*\s+(\d+).+(?:Episodes?)+\s*[:]*\s+(\d+(?:-\d+)?)")
            .expect("valid regex")
    });
    static ENG_SEASON: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)Season\s*[:]*\s+(\d+(?:-\d+)?)").expect("valid regex"));
    static ENG_EPISODE_OF: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:Episodes?)+\s*[:]*\s+(\d+(?:-\d+)?)\s*of\s*([\w?])")
            .expect("valid regex")
    });
    static ENG_EPISODE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?:Episodes?)+\s*[:]+\s*[:]*\s+(\d+(?:-\d+)?)").expect("valid regex"));

    // \p{IsCyrillic} in .NET == \p{Cyrillic} in the `regex` crate; the
    // C# alternation is preserved group-for-group.
    static STRIP_CYRILLIC: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(\([\p{Cyrillic}\W]+?\))|(^\p{Cyrillic}[\p{Cyrillic}\W\d]+\/ )|([\p{Cyrillic} \-]+,+)|([\p{Cyrillic}]+)")
            .expect("valid regex")
    });

    static RIP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b-Rip\b").expect("valid regex"));
    static HDTV_RIP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bHDTVRip\b").expect("valid regex"));
    static WEB_DL_RIP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bWEB-DLRip\b").expect("valid regex"));
    static WEBDL_RIP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bWEBDLRip\b").expect("valid regex"));
    static WEBDL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bWEBDL\b").expect("valid regex"));
    static PAREN_SLASH_OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(\s*/\s*").expect("valid regex"));
    static SLASH_PAREN_CLOSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*/\s*\)").expect("valid regex"));
    static EMPTY_BRACKETS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[\[\(]\s*[\)\]]").expect("valid regex"));
    static MULTI_SPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("valid regex"));

    // https://www.fileformat.info/info/unicode/category/Pd/list.htm
    let mut title = PUNCT_DASH.replace_all(raw_title, "-").into_owned();

    if is_tv {
        title = TV_COMMA.replace_all(&title, " ${1}-${2}").into_owned();
        title = TV_CYRILLIC_X.replace_all(&title, "${1}XX${2}").into_owned();

        // Special case for multiple seasons.
        title = TV_MULTI_SEASON.replace_all(&title, "S${1}").into_owned();

        title = UKR_SEASON_EPISODE_OF
            .replace_all(&title, "S${1}E${2} of ${3}")
            .into_owned();
        title = UKR_SEASON_EPISODE
            .replace_all(&title, "S${1}E${2}")
            .into_owned();
        title = UKR_SEASON.replace_all(&title, "S${1}").into_owned();
        title = UKR_EPISODE_OF
            .replace_all(&title, "E${1} of ${2}")
            .into_owned();
        title = UKR_EPISODE.replace_all(&title, "E${1}").into_owned();

        title = ENG_SEASON_EPISODE_OF
            .replace_all(&title, "S${1}E${2} of ${3}")
            .into_owned();
        title = ENG_SEASON_EPISODE
            .replace_all(&title, "S${1}E${2}")
            .into_owned();
        title = ENG_SEASON.replace_all(&title, "S${1}").into_owned();
        title = ENG_EPISODE_OF
            .replace_all(&title, "E${1} of ${2}")
            .into_owned();
        title = ENG_EPISODE.replace_all(&title, "E${1}").into_owned();
    }

    // `TolokaSettings.StripCyrillicLetters` (default true).
    if strip_cyrillic {
        title = STRIP_CYRILLIC.replace_all(&title, "").into_owned();
        title = title.trim_matches([' ', '-']).to_string();
    }

    title = RIP.replace_all(&title, "Rip").into_owned();
    title = HDTV_RIP.replace_all(&title, "HDTV").into_owned();
    title = WEB_DL_RIP.replace_all(&title, "WEB-DL").into_owned();
    title = WEBDL_RIP.replace_all(&title, "WEB-DL").into_owned();
    title = WEBDL.replace_all(&title, "WEB-DL").into_owned();

    title = move_first_tags_to_end(&title);

    title = PAREN_SLASH_OPEN.replace_all(&title, "(").into_owned();
    title = SLASH_PAREN_CLOSE.replace_all(&title, ")").into_owned();
    title = EMPTY_BRACKETS.replace_all(&title, "").into_owned();

    title = title
        .trim_matches([
            ' ', '&', ',', '.', '!', '?', '+', '-', '_', '|', '/', '\\', ':', ';', '\u{2BC}', '`',
        ])
        .to_string();

    MULTI_SPACE.replace_all(&title, " ").trim().to_string()
}

/// Port of `MoveFirstTagsToEndOfReleaseTitle`: leading bracketed tags --
/// `(…)`/`[…]` groups at the very start, even when stacked -- are moved
/// behind the rest of the title ("(2019) Title (UA)" -> "Title (UA) (2019)").
///
/// The .NET regexes are balanced-group constructions (`(?(c)(?!))`) that
/// the `regex` crate cannot express, so this implements their behavior
/// directly: repeatedly peel the balanced `(…)`/`[…]` prefix groups off
/// the front of the string and append them to the end.
fn move_first_tags_to_end(input: &str) -> String {
    let trim_chars = [
        ' ', '&', ',', '.', '!', '?', '+', '-', '_', '|', '/', '\\', ':', ';', '\u{2BC}', '`',
    ];
    let mut output = input.trim_matches(&trim_chars).to_string();

    // Peel at most a handful of leading groups (the .NET version walks
    // every match, but a title has never more than a few leading tags).
    let mut peeled = Vec::new();
    loop {
        let trimmed = output.trim_start();
        let Some(tag) = leading_balanced_tag(trimmed) else {
            break;
        };
        peeled.push(tag.to_string());
        output = trimmed[tag.len()..].trim_start().to_string();
    }

    if peeled.is_empty() {
        return output.trim().to_string();
    }

    for tag in peeled {
        output = format!("{output} {tag}").trim().to_string();
    }
    output.trim().to_string()
}

/// If `s` starts with a balanced `(...)` or `[...]` group, returns it.
fn leading_balanced_tag(s: &str) -> Option<&str> {
    let open = s.chars().next()?;
    let close = match open {
        '(' => ')',
        '[' => ']',
        _ => return None,
    };
    let mut depth = 0usize;
    for (idx, ch) in s.char_indices() {
        if ch == open {
            depth += 1;
        } else if ch == close {
            depth -= 1;
            if depth == 0 {
                return Some(&s[..idx + ch.len_utf8()]);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_parser_moves_leading_tags_to_end() {
        // "Назва" stripped, then the leading "(2019)" tag group is moved
        // to the end -- the exact `MoveFirstTagsToEndOfReleaseTitle`
        // behavior (verified against the .NET pipeline's semantics).
        assert_eq!(
            parse_title("Назва (2019) WEB-DLRip", false, true),
            "WEB-DL (2019)"
        );
    }

    #[test]
    fn title_parser_rewrites_season_episodes() {
        // Ukrainian "Сезон 1 Серія 2-10 з 12" -> S1E2-10 of 12 (the one
        // char `з` capture grabbing "1" of "12" re-joins with the
        // leftover "2", the same way .NET's `[\w?]` behaves), with the
        // Cyrillic prefix stripped and the outer tag group kept.
        assert_eq!(
            parse_title("Шоу (Сезон 1 Серія 2-10 з 12)", true, true),
            "(S1E2-10 of 12)"
        );
    }

    #[test]
    fn title_parser_keeps_cyrillic_when_strip_disabled() {
        // `TolokaSettings.StripCyrillicLetters` off: the Ukrainian part
        // is kept, the release-tag fixups still apply.
        assert_eq!(
            parse_title("Назва (2019) WEB-DLRip", false, false),
            "Назва (2019) WEB-DL"
        );
    }

    #[test]
    fn title_parser_keeps_latin_titles() {
        assert_eq!(
            parse_title("Interstellar (2014) WEB-DLRip", false, true),
            "Interstellar (2014) WEB-DL"
        );
    }

    #[test]
    fn size_parser_handles_ukrainian_units() {
        let html = r#"<table><tr><td>x</td><td>1,37 ГБ</td></tr></table>"#;
        let doc = Html::parse_document(html);
        let row = doc.select(&Selector::parse("tr").unwrap()).next().unwrap();
        let bytes = extract_size_bytes(&row, 2);
        assert_eq!(bytes, (1.37 * 1024.0f64.powi(3)) as u64);
    }

    #[test]
    fn category_link_query_arg() {
        let f = query_arg_of("f");
        assert_eq!(f("/tracker.php?f=124"), Some("124".to_string()));
        assert_eq!(f("/tracker.php?o=1&f=32#top"), Some("32".to_string()));
        assert_eq!(f("/tracker.php"), None);
    }

    #[test]
    fn leading_tag_peeling() {
        assert_eq!(leading_balanced_tag("(2019) rest"), Some("(2019)"));
        assert_eq!(leading_balanced_tag("[UA] rest"), Some("[UA]"));
        assert_eq!(leading_balanced_tag("a (b) c"), None);
        assert_eq!(leading_balanced_tag("(unclosed"), None);
    }

    #[test]
    fn set_cookies_last_duplicate_wins() {
        // phpBB's login 302 sets the guest session first, then the
        // authenticated one -- only the last must survive in the header.
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::SET_COOKIE,
            "toloka_data=guest; path=/; httponly".parse().unwrap(),
        );
        headers.append(
            reqwest::header::SET_COOKIE,
            "toloka_sid=first; path=/".parse().unwrap(),
        );
        headers.append(
            reqwest::header::SET_COOKIE,
            "toloka_data=authed; path=/; httponly".parse().unwrap(),
        );
        headers.append(
            reqwest::header::SET_COOKIE,
            "toloka_sid=second; path=/".parse().unwrap(),
        );
        assert_eq!(
            join_set_cookies(&headers).as_deref(),
            Some("toloka_data=authed; toloka_sid=second")
        );
    }
}