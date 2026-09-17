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

Read at startup; the settings page reports the values in effect (it
cannot change them, since a new value would need a restart).

| Variable         | Default        | Meaning                                    |
|------------------|----------------|---------------------------------------------|
| `BIND_ADDR`      | `0.0.0.0:3000` | Address the HTTP server listens on          |
| `DOWNLOAD_DIR`   | `./downloads`  | Default directory torrents save into        |
| `DB_PATH`        | `./salo.db`    | SQLite file for per-indexer settings        |
| `WORKER_THREADS` | `2`            | Tokio worker threads (see `src/main.rs`)    |

## Memory usage vs. the traditional stack

A typical self-hosted setup runs Sonarr, Radarr, Prowlarr, and a torrent
client (usually qBittorrent) as separate long-running services — each its
own process/container, its own HTTP server, its own database, its own
.NET or Mono runtime for the `*arr` apps. `salo` is one process, one
embedded SQLite file, one embedded BitTorrent client.

There's no single controlled benchmark running all of these side by side
on identical hardware with identical libraries, so treat the numbers
below as what's actually documented/reported, not a lab measurement:

| Component | Reported RAM | Source |
|---|---|---|
| Sonarr + Radarr, idle, small library | ~200–300 MB combined | [Sonarr forums: "What is normal memory usage for Sonarr?"](https://forums.sonarr.tv/t/what-is-normal-memory-usage-for-sonarr-normal-for-it-to-keep-slowly-growing/19384) |
| Radarr, large library (~600 movies) | ~1.6 GB | [Radarr/Radarr#158 "High memory usage by radarr"](https://github.com/Radarr/Radarr/issues/158) |
| Sonarr, large library, reported growth over time | 600 MB+ and climbing | [Sonarr forums: "Steady Memory Increase"](https://forums.sonarr.tv/t/steady-memory-increase/20463) |
| Prowlarr + rest of the `*arr`/Docker stack | vendor guidance: 2 GB minimum, 4 GB+ recommended | [Prowlarr hardware requirements](https://shop.zimaspace.com/pages/prowlarr-hardware-requirements) |
| qBittorrent | no fixed minimum published; multiple open issues about unbounded growth | [qBittorrent#16612](https://github.com/qbittorrent/qBittorrent/issues/16612), [qBittorrent#13699](https://github.com/qbittorrent/qBittorrent/issues/13699) |
| **salo, idle** (measured on this repo) | **~11.6 MB**, one process | `ps -o rss` right after startup, no torrents added |

Put together, a working `*arr` + torrent-client stack realistically lands
somewhere in the **1–2+ GB** range once you count all four services and a
real library, before the host OS or anything else running on the same
box. `salo` replacing that whole stack with one binary idling at ~11.6 MB
is the difference between needing a dedicated home-server box and running
comfortably on whatever's already idle — a Raspberry Pi, a $5 VPS, a spare
router with USB storage.

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
  progress, live download/upload rates, bytes uploaded, and achieved
  upload/download **ratio** (green once it reaches 1.0), save location,
  and seed limit, with per-torrent detail pages
  (`/torrents/<info_hash>`) linking back to the original tracker listing.
  The rates refresh in place every 2s from `/torrents/stats` (a JSON
  endpoint, not an HTML swap) so a seed-limit field being typed into is
  never rebuilt mid-edit.
- **Per-torrent controls** (`/torrents/<info_hash>`) — the whole lifecycle
  on the torrent's own page, so nothing requires going back to the list:
  **pause/resume** (the "stop seeding" action — the torrent stays in the
  session, files intact, and the paused state is persisted so a restart
  doesn't silently resume it), **change the seed limit** (stop after N
  minutes, at ratio R, or whichever comes first — `0` means indefinitely),
  and **remove** the torrent, optionally deleting its files.
- **Movie info** — with a TMDB API key set in `/settings`, a torrent's
  detail page shows a poster, title, year, rating and plot summary for the
  release it matched, with a link to its TMDB page. Without a key **no
  request is made at all**; the block simply doesn't appear. The same
  release name is resolved the way TorrServer resolves it — via a real
  release-name parser (`torrent-name-parser`), because TMDB matches a
  multi-word query essentially literally and any leftover release tag
  (`1080p`, `x265`, `DDP5`) makes the search return *nothing*. The other
  matches are offered as clickable poster alternatives, and the one you
  pick is remembered; poster-less matches are skipped in favour of ones
  with artwork.
- **Comment counts** — a release's comment count on its tracker, linked
  straight to the comments (nyaa publishes this; other indexers show
  nothing rather than a fabricated `0`).
- **Settings** (`/settings`) — per-indexer configuration, the optional
  TMDB key, plus a read-only report of the environment variables in
  effect. Indexers with nothing to configure aren't listed at all.
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
- `src/env.rs` is the single source of truth for the environment
  variables: the constants are what `main()` reads and what the settings
  page reports, so the two cannot drift apart.
- `src/tmdb.rs` is the only outbound metadata call in the project, and it
  only happens when a key is configured. It deliberately mirrors
  TorrServer's approach (the `torrserver/` submodule): a real release-name
  parser rather than a junk-word list, and every poster candidate kept so
  the user can switch.

## Submodules

- `Prowlarr/` — upstream Prowlarr, the source of the indexer definitions
  salo ports (see `src/indexers/`). Reference only; nothing is built from
  it.
- `torrserver/` — [TorrServer](https://github.com/yourok/torrserver), a
  Go torrent streaming server with a richer metadata UI. Added as a
  reference for how it presents torrents (posters, titles), not built or
  invoked by salo.

  Its movie **posters and info come from TMDB** — the only metadata
  provider it uses (no Kinopoisk/OMDb/IMDb call anywhere in the tree).
  The relevant code:
  - `web/src/components/Add/helpers.js` — `getMoviePosters()` calls
    `GET {APIURL}/3/search/multi?api_key=…&query=…&language=…` and
    renders `{ImageURL}/t/p/w300{poster_path}`. Before searching it
    trims the torrent title with `shortenTitleForPosterSearch()` (cut at
    `" ["`, `" ("`, `" / "`, then the first 4 words / 50 chars), because
    long release names exceed what TMDB's search handles well.
  - `server/settings/btsets.go` — the `TMDBConfig` defaults:
    `APIURL=https://api.themoviedb.org`, `ImageURL=https://image.tmdb.org`,
    plus `ImageURLRu=https://imagetmdb.com` (a TMDB image mirror used
    when the language is Russian).
  - `server/web/api/tmdb.go` + `GET /tmdb/settings` — the server hands
    those four values to the browser, which does the TMDB calls itself;
    the server never proxies TMDB.
  - The API key is either the user's own (Settings → TMDB, or the
    `tmdbkey` bot command) or a build-time `REACT_APP_TMDB_API_KEY`
    baked in from CI, which the JS uses as a fallback.
