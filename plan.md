
> **Role:** You are an expert systems programmer and backend engineer. Your goal is to design and write the foundational code for a hyper-optimized, ultra-low-resource monolithic application that replaces the `*arr` stack, Prowlarr, qBittorrent, and TorrServer.
> **Context & Philosophy:** Modern self-hosted media stacks are bloated with heavy runtimes, React SPAs, and swarms of Docker containers. We are building the antidote: a single, self-contained, statically compiled binary that can run on a potato (target idle RAM: <50MB). There are no external databases, no external torrent clients, and no external scraping proxies.
> **The Tech Stack:**
> * **Backend:** Rust
> * **Web Framework:** `axum` (for handling HTTP routing and serving HTMX fragments)
> * **Templating:** `askama` (compiled, type-safe HTML templates)
> * **Frontend:** Pure HTML + HTMX (zero client-side state, no heavy JS frameworks)
> * **Scraping:** `reqwest` (HTTP client), `scraper` (CSS selectors), and `regex`
> * **Torrent Engine:** `librqbit` (native Rust BitTorrent implementation)
> 
> 
> **Core Requirements & Features to Implement:**
> 1. **Search & Scrape Module:** Write a function using `reqwest` and `scraper` that targets a public torrent tracker (use a generic HTML table structure as a placeholder). Parse the HTML to extract release titles, seeders, and file sizes. Use `regex` to reliably extract the `magnet:?` URI from the page. Do NOT use any external indexing APIs.
> 2. **HTMX Interface:** Create a basic `askama` template featuring a search form. Write the `axum` route that accepts the form submission, triggers the scraper, and returns just the HTML fragment containing the results table.
> 3. **Embedded Torrenting & Streaming:** Write the integration with `librqbit`. When a user selects a magnet link via the UI, the app must initiate a sequential download of the torrent pieces. Expose an HTTP stream route in `axum` that pipes the downloaded video chunks directly to an HTML5 `<video>` tag for instant playback.
> 
> 
> **Constraints:**
> * Do not suggest Docker, microservices, or external daemon processes.
> * Keep the logic strictly within one binary.
> * Provide robust error handling for network timeouts and parsing failures (e.g., when a website's DOM structure inevitably changes).
> * Focus on extreme resource efficiency, using async concurrency to keep I/O operations from blocking the main thread.
> 
> 
> Please provide the project structure (`Cargo.toml` dependencies) and the core `main.rs` implementation demonstrating these three pillars working together.

