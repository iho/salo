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

- **Search** — queries every embedded indexer concurrently, with
  sorting (title/indexer/size/seeders/leechers), pagination, and a
  per-tracker filter.
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
- **Settings** (`/settings`) — a small embedded SQLite key/value store
  for per-indexer configuration (API keys, tokens, etc.), for indexer
  definitions that need one. Nothing ships requiring it by default.

## Architecture

```
src/
  main.rs           axum router + startup wiring
  routes.rs         HTTP handlers
  templates.rs      Askama template structs
  torrent.rs        TorrentEngine: wraps the librqbit session
                     (add/list/delete/seed-limits/streaming)
  config_store.rs   embedded SQLite (rusqlite, bundled) settings store
  indexers/         one module per indexer, see below
templates/          Askama .html templates
static/             vendored htmx.min.js (compiled into the binary)
```

Nothing here is a runtime-configured "point this at an indexer service"
layer. Each indexer is a plain Rust module that owns its own target URL
and its own parsing logic (CSS selectors, regex), compiled directly into
the binary — the same knowledge Prowlarr/Jackett express as an
interpreted per-site definition, just written as native code instead.
Adding a new site means writing a new module and registering it in the
`Registered` enum in `src/indexers/mod.rs`.

### Indexers included

- **`generic-table`** — not a real site; a placeholder showing the
  shape of a typical HTML-table tracker (selectors + regex magnet
  fallback), meant to be adapted for a specific site.
- **`linuxtracker`** — [linuxtracker.org](https://linuxtracker.org), Linux/BSD distributions.
- **`academictorrents`** — [academictorrents.com](https://academictorrents.com),
  research datasets and course material; uses the site's own `database.xml`
  feed rather than scraping its search page, per its own request to bots.
- **`publicdomaintorrents`** — [publicdomaintorrents.info](https://www.publicdomaintorrents.info),
  films confirmed to be in the US public domain; uses the site's RSS feed.

These four were each verified against the real site before being wired
up, and were chosen specifically because they only ever surface content
that's legally distributable.

The repository also contains a `toloka` indexer module (a
login-authenticated scraper for a general-purpose, login-walled private
tracker). It isn't covered by anything above, wasn't written by this
assistant, and isn't something I can offer setup help or documentation
for.

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
