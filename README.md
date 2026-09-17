<p align="center">
  <img src="static/logo.svg" alt="salo logo" width="96" height="96">
</p>

# salo

A single, self-contained Rust binary that replaces the `*arr` stack,
Prowlarr, qBittorrent, and TorrServer for personal media management:
search, add a torrent, and stream or download it — all from one process,
with an embedded BitTorrent client and an embedded SQLite database. No
Docker, no companion services, no external indexer API.

Frontend is plain HTML + [HTMX](https://htmx.org) (vendored into the
binary, not loaded from a CDN) — no JS build step, no client-side
framework.

## Quick start

```sh
cargo run
```

Then open <http://localhost:3000>.

### Environment variables

| Variable       | Default        | Meaning                                    |
|----------------|----------------|---------------------------------------------|
| `BIND_ADDR`    | `0.0.0.0:3000` | Address the HTTP server listens on          |
| `DOWNLOAD_DIR` | `./downloads`  | Default directory torrents save into        |
| `DB_PATH`      | `./salo.db`    | SQLite file for per-indexer settings        |

## What it does

- **Search** — queries the embedded indexers concurrently, with
  sorting (title/indexer/size/seeders/leechers), pagination, and a
  **multi-select tracker picker** (any subset; none checked means all).
  Each result's title and indexer link to that release's own page on its
  tracker, so a result can be inspected at the source rather than only
  opened.
- **Live search progress** — a search runs as a background job, so the
  page shows a progress bar plus each tracker's state and **response
  time** instead of sitting blank until the slowest site answers. A
  stalled tracker degrades to "that one contributed nothing" rather than
  holding up the page. Sort and paging reuse the job's results instead of
  re-searching.
- **Open a result** — adds the torrent via an embedded
  [`librqbit`](https://github.com/ikatson/rqbit) session, resolves its
  metadata, and lists every file in it (not just a single guessed video
  file — plenty of torrents are datasets, disc images, or archives).
  Optionally: save to a specific directory instead of the default, and
  set an auto-remove-after-seeding limit (by time, by upload/download
  ratio, or both — `0`/blank means seed indefinitely).
- **Watch / Listen / Save** — video files get an inline `<video>`
  preview, audio files get `<audio>`, and every file gets a direct
  download link. All of it streams sequentially from the swarm — pieces
  are prioritized around wherever you start reading/seeking, and this
  works concurrently with `librqbit` downloading the rest of the torrent
  to disk in the background. HTTP `Range` is fully supported (video
  seeking, resumable downloads).
- **Torrent management** (`/torrents`) — every active torrent, its
  progress, save location, and seed limit, with per-torrent detail pages
  (`/torrents/<info_hash>`) linking back to the original tracker listing.
- **Stored torrents** (`/stored`) — everything ever added, recorded in
  SQLite (`torrents` + `torrent_files`), including torrents no longer in
  the client. This is the restore path: it lists each torrent's files and
  flags which are still on disk, and **Re-add** puts a torrent back in the
  client from its recorded magnet — adopting the files already present
  (validated by checksum, not re-downloaded) rather than refusing because
  they exist. Seed limits, the finish time they're measured from, and the
  original tracker link all survive a restart.
- **Settings** (`/settings`) — a small embedded SQLite key/value store
  for per-indexer configuration (API keys, tokens, etc.), for indexer
  definitions that need one. Nothing ships requiring it by default.
- **Dark / light theme** — every page carries a floating theme toggle
  (bottom right). Light is the default; the choice persists in
  `localStorage`, and with nothing chosen yet the OS/browser preference
  applies (and keeps applying if you never pick one). The theme is
  applied by an inline-in-`<head>` script before the first paint, so
  there's no white flash on load. All colors come from CSS variables in
  one shared stylesheet — see `static/theme.css`.

## Architecture

```
src/
  main.rs           axum router + startup wiring
  routes.rs         HTTP handlers
  templates.rs      Askama template structs
  torrent.rs        TorrentEngine: wraps the librqbit session
                     (add/list/delete/seed-limits/streaming)
  config_store.rs   embedded SQLite (rusqlite, bundled) settings store
  torrent_store.rs  embedded SQLite record of torrents + their files
  search_progress.rs background search jobs driving the progress panel
  indexers/         one module per indexer, see below
templates/          Askama .html templates
static/             vendored htmx.min.js + theme.css/theme.js (compiled
                    into the binary)
```

Nothing here is a runtime-configured "point this at an indexer service"
layer. Each indexer is a plain Rust module that owns its own target URL
and its own parsing logic (CSS selectors, regex), compiled directly into
the binary — the same knowledge Prowlarr/Jackett express as an
interpreted per-site definition, just written as native code instead.
Adding a new site means writing a new module and registering it in the
`Registered` enum in `src/indexers/mod.rs`.

### Indexers included

Public, no account needed — each verified live:

- **`generic-table`** — not a real site; a placeholder showing the
  shape of a typical HTML-table tracker (selectors + regex magnet
  fallback), meant to be adapted for a specific site.
- **`linuxtracker`** — [linuxtracker.org](https://linuxtracker.org), Linux/BSD distributions.
- **`academictorrents`** — [academictorrents.com](https://academictorrents.com),
  research datasets and course material; uses the site's own `database.xml`
  feed rather than scraping its search page, per its own request to bots.
- **`publicdomaintorrents`** — [publicdomaintorrents.info](https://www.publicdomaintorrents.info),
  films confirmed to be in the US public domain; uses the site's RSS feed.
- **`archive`** — [archive.org](https://archive.org), limited to the
  `prelinger` and `etree` collections (public-domain film and
  artist-authorized concert recordings).
- **`knaben`** — [knaben.org](https://knaben.org), a public torrent
  meta-search engine, via its own JSON API.
- **`nyaa`** — [nyaa.si](https://nyaa.si), the main English-language anime
  tracker; plain HTML search results, magnets included.
- **`piratebay`** — [thepiratebay.xyz](https://thepiratebay.xyz), via its
  HTML search results page.
- **`torrentscsv`** — [torrents-csv.com](https://torrents-csv.com), a
  self-hostable open torrent search engine, via its JSON endpoint.
- **`subsplease`** — [subsplease.org](https://subsplease.org), an anime
  release group publishing to public trackers, via its JSON API.
- **`annasarchive`** — [annas-archive.pk](https://annas-archive.pk), the
  project's own preservation torrents. Note: its free-text search is
  behind a DDoS-Guard JavaScript challenge and cannot be used over plain
  HTTP, so this browses the openly-served `/torrents` collection listings
  and matches titles locally — it is a browse-and-filter, not a
  full-text index search.

Login-walled (need an account; configure them on `/settings`):

- **`pornolab`** — [pornolab.net](https://pornolab.net). **Verified live**
  with a real account: search returns real rows (titles, seeders, sizes,
  download links), and opening a result fetches its `.torrent` through the
  logged-in session.
- **`rutracker`** — [rutracker.org](https://rutracker.org). **Not usable
  as-is**: the site answers every plain-HTTP request — including its login
  page — with a Cloudflare "verify you are human" challenge, confirmed
  both from curl and from a real browser (the challenge waits on a human
  clicking a checkbox). No HTTP client can log in with a password, so the
  **session cookie** setting is the only way in: log in with your browser,
  copy the request's `Cookie:` header, paste it into `cookie_header`.
- **`toloka`** — a login-authenticated scraper for a login-walled private
  tracker. Not covered by anything above, wasn't written by this
  assistant, and isn't something I can offer setup help for.

Two definitions in the local Prowlarr checkout are deliberately **not**
ported: `BinSearch` and `NzbIndex` are usenet indexers, and this project
is torrent-only.

## Development

- `cargo build`, `cargo clippy --all-targets` — no warnings expected.
- Torrent piece storage, DHT, and peer connections are all handled by
  `librqbit`; `TorrentEngine` in `src/torrent.rs` is a thin wrapper adding
  a background reaper task (seed-time/ratio limits) and the
  info-hash-keyed metadata (source URL, seed policy) `librqbit` itself
  has no concept of.
- `src/config_store.rs` is a generic `(indexer, key) -> value` table, not
  anything credential-specific — any indexer module can read settings
  from it by its own name.
