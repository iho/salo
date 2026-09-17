//! The environment variables salo reads at startup, in one place.
//!
//! Every one of these is consumed by `main()` before the HTTP server
//! binds, so none of them can be changed from the settings page: a form
//! that "saved" a new listen address into SQLite would not rebind
//! anything, and would be a lie. The settings page therefore *reports*
//! these values (see [`current`]) instead of offering inputs for them.
//!
//! Add a new variable here and both `main()` and the settings page pick
//! it up -- the list is the single source of truth, so the page cannot
//! drift out of sync with what the process actually reads.

/// Default bind address: all interfaces, port 3000. Deliberately
/// `0.0.0.0` rather than loopback so the UI is reachable from another
/// machine on the LAN out of the box.
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:3000";

/// Default SQLite path, relative to the working directory.
pub const DEFAULT_DB_PATH: &str = "./salo.db";

/// Default download directory, relative to the working directory.
pub const DEFAULT_DOWNLOAD_DIR: &str = "./downloads";

/// Default tokio worker-thread count. See `main()` for why this is 2 and
/// not one-per-core.
pub const DEFAULT_WORKER_THREADS: &str = "2";

/// A variable's declared name, default, and what it does.
pub struct Var {
    pub name: &'static str,
    pub default: &'static str,
    pub help: &'static str,
}

/// Every environment variable salo honours, in the order the settings
/// page lists them.
pub const VARS: &[Var] = &[
    Var {
        name: "BIND_ADDR",
        default: DEFAULT_BIND_ADDR,
        help: "Address the HTTP server listens on. Changing it needs a restart.",
    },
    Var {
        name: "DOWNLOAD_DIR",
        default: DEFAULT_DOWNLOAD_DIR,
        help: "Directory new torrents save into by default. Changing it needs a restart.",
    },
    Var {
        name: "DB_PATH",
        default: DEFAULT_DB_PATH,
        help: "SQLite file holding per-indexer settings and the torrent record. Changing it needs a restart.",
    },
    Var {
        name: "WORKER_THREADS",
        default: DEFAULT_WORKER_THREADS,
        help: "Tokio worker threads. 2 keeps librqbit's async disk I/O off the request path; 1 is the smallest footprint. Changing it needs a restart.",
    },
];

/// The effective value of every variable: the environment's value where
/// set, otherwise the declared default. Returned as `(name, value,
/// is_default, help)` so the settings page can mark which values came
/// from the environment and which are built-in fallbacks.
pub fn current() -> Vec<(&'static str, String, bool, &'static str)> {
    VARS.iter()
        .map(|v| {
            let from_env = std::env::var(v.name).ok().filter(|s| !s.trim().is_empty());
            let is_default = from_env.is_none();
            (
                v.name,
                from_env.unwrap_or_else(|| v.default.to_string()),
                is_default,
                v.help,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variable_is_listed_once() {
        let mut seen: Vec<&str> = VARS.iter().map(|v| v.name).collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), total, "duplicate variable name in VARS");
    }

    #[test]
    fn current_reports_a_value_for_every_variable() {
        // Unset or set, there is always a usable value -- the settings
        // page never has to render an empty cell.
        for (name, value, _, _) in current() {
            assert!(!value.trim().is_empty(), "{name} resolved to nothing");
        }
    }
}
