//! Live progress for an in-flight search: what each tracker is doing, how
//! long it took, and how many results it returned.
//!
//! `search_all` merges every tracker with `join_all`, so a search takes as
//! long as its slowest member and the page shows nothing until the last
//! one lands. Instead, the UI starts a *job* here: this runs the same
//! search in the background, recording each tracker's state as it goes,
//! and the page polls the job for a progress bar plus response times.
//!
//! Jobs are in-memory only and bounded -- they exist to drive one page's
//! progress display, not to be a durable record (that's the SQLite store).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::indexers::Release;

/// How long a finished job is kept so a late poll still finds it, and how
/// many are kept at once (oldest evicted first).
const JOB_TTL: Duration = Duration::from_secs(300);
const MAX_JOBS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TrackerState {
    Pending,
    Running,
    Done,
    Failed,
}

impl TrackerState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    pub fn is_finished(self) -> bool {
        matches!(self, Self::Done | Self::Failed)
    }
}

#[derive(Clone)]
pub struct TrackerProgress {
    pub name: &'static str,
    pub state: TrackerState,
    /// Milliseconds this tracker took, once finished.
    pub elapsed_ms: Option<u64>,
    pub results: usize,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct SearchProgress {
    pub query: String,
    /// Trackers being searched, in the order the UI should list them.
    pub trackers: Vec<TrackerProgress>,
    pub started: Instant,
    pub finished: bool,
    /// Every release the job collected (empty until finished) -- lets a
    /// follow-up sort/page render reuse the results instead of searching
    /// every tracker again.
    pub releases: Vec<Release>,
}

impl SearchProgress {
    pub fn done_count(&self) -> usize {
        self.trackers.iter().filter(|t| t.state.is_finished()).count()
    }

    pub fn total(&self) -> usize {
        self.trackers.len()
    }

    /// 0-100, for the bar.
    pub fn percent(&self) -> u32 {
        if self.trackers.is_empty() {
            return 100;
        }
        (self.done_count() * 100 / self.trackers.len()) as u32
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// Slowest tracker so far -- the wall-clock cost of the search, since
    /// they all run concurrently.
    pub fn slowest_ms(&self) -> Option<u64> {
        self.trackers.iter().filter_map(|t| t.elapsed_ms).max()
    }
}

type Shared = Arc<Mutex<SearchProgress>>;

static JOBS: LazyLock<Mutex<HashMap<String, (Shared, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Creates a job and returns its id. The caller spawns
/// [`run`] with the returned handle.
pub fn create(query: String, names: Vec<&'static str>) -> (String, Shared) {
    let id = new_id();
    let trackers = names
        .iter()
        .map(|name| TrackerProgress {
            name,
            state: TrackerState::Pending,
            elapsed_ms: None,
            results: 0,
            error: None,
        })
        .collect();
    let progress = SearchProgress {
        query,
        trackers,
        started: Instant::now(),
        finished: false,
        releases: Vec::new(),
    };
    let shared = Arc::new(Mutex::new(progress));

    let mut jobs = JOBS.lock().expect("job registry poisoned");
    evict(&mut jobs);
    jobs.insert(id.clone(), (Arc::clone(&shared), Instant::now()));
    (id, shared)
}

pub fn get(id: &str) -> Option<SearchProgress> {
    let jobs = JOBS.lock().expect("job registry poisoned");
    jobs.get(id).map(|(shared, _)| shared.lock().expect("job poisoned").clone())
}

/// Runs each tracker concurrently, recording state/per-timing as it goes.
/// Mirrors `search_all`'s contract: one tracker failing never fails the
/// search, it's just marked failed with its reason.
pub async fn run(
    progress: Shared,
    client: reqwest::Client,
    config: Arc<crate::config_store::ConfigStore>,
    query: String,
    targets: Vec<&'static str>,
) {
    let mut handles = Vec::new();
    for name in targets {
        let progress = Arc::clone(&progress);
        let client = client.clone();
        let config = Arc::clone(&config);
        let query = query.clone();

        handles.push(tokio::spawn(async move {
            // Mark running for the very first poll.
            if let Some(entry) = lock(&progress).trackers.iter_mut().find(|t| t.name == name) {
                entry.state = TrackerState::Running;
            }

            let started = Instant::now();
            let result = crate::indexers::search_one(&client, &config, name, &query).await;
            let elapsed_ms = started.elapsed().as_millis() as u64;

            let mut guard = lock(&progress);
            if let Some(entry) = guard.trackers.iter_mut().find(|t| t.name == name) {
                entry.elapsed_ms = Some(elapsed_ms);
                match result {
                    Ok(found) => {
                        entry.state = TrackerState::Done;
                        entry.results = found.len();
                        guard.releases.extend(found);
                    }
                    Err(err) => {
                        entry.state = TrackerState::Failed;
                        entry.error = Some(format!("{err:#}"));
                    }
                }
            }
        }));
    }

    for handle in handles {
        // A tracker task can only fail if it panics; mark it done either
        // way so the bar always reaches 100%.
        let _ = handle.await;
    }

    let mut guard = lock(&progress);
    for entry in guard.trackers.iter_mut() {
        if !entry.state.is_finished() {
            entry.state = TrackerState::Failed;
            entry.error = Some("task ended without a result".to_string());
        }
    }
    guard.finished = true;
    guard.releases.sort_by_key(|r| std::cmp::Reverse(r.seeders));
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, SearchProgress> {
    shared.lock().expect("job poisoned")
}

/// Drops expired jobs, then the oldest if still over the cap.
fn evict(jobs: &mut HashMap<String, (Shared, Instant)>) {
    let now = Instant::now();
    jobs.retain(|_, (_, created)| now.duration_since(*created) < JOB_TTL);
    while jobs.len() >= MAX_JOBS {
        let Some(oldest) = jobs
            .iter()
            .min_by_key(|(_, (_, created))| *created)
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        jobs.remove(&oldest);
    }
}

/// A short, unguessable-enough id for one page's job. Not a security
/// boundary (jobs hold only a query string and public search results), so
/// a simple counter-plus-time scheme is enough and keeps this dependency-free.
fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}-{n:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(name: &'static str, state: TrackerState) -> TrackerProgress {
        TrackerProgress {
            name,
            state,
            elapsed_ms: None,
            results: 0,
            error: None,
        }
    }

    #[test]
    fn percent_reflects_finished_trackers() {
        let mut progress = SearchProgress {
            query: "x".into(),
            trackers: vec![
                tracker("a", TrackerState::Done),
                tracker("b", TrackerState::Running),
                tracker("c", TrackerState::Pending),
                tracker("d", TrackerState::Failed),
            ],
            started: Instant::now(),
            finished: false,
            releases: Vec::new(),
        };
        // Done and Failed both count as finished, so 2 of 4.
        assert_eq!(progress.done_count(), 2);
        assert_eq!(progress.percent(), 50);

        progress.trackers.iter_mut().for_each(|t| t.state = TrackerState::Done);
        assert_eq!(progress.percent(), 100);
    }

    #[test]
    fn empty_tracker_list_is_complete_not_a_divide_by_zero() {
        let progress = SearchProgress {
            query: "x".into(),
            trackers: Vec::new(),
            started: Instant::now(),
            finished: true,
            releases: Vec::new(),
        };
        assert_eq!(progress.percent(), 100);
    }

    #[test]
    fn slowest_is_the_wall_clock_cost_of_a_concurrent_search() {
        let progress = SearchProgress {
            query: "x".into(),
            trackers: vec![
                TrackerProgress {
                    elapsed_ms: Some(120),
                    ..tracker("a", TrackerState::Done)
                },
                TrackerProgress {
                    elapsed_ms: Some(940),
                    ..tracker("b", TrackerState::Done)
                },
                // Still running: contributes no timing yet.
                tracker("c", TrackerState::Running),
            ],
            started: Instant::now(),
            finished: false,
            releases: Vec::new(),
        };
        assert_eq!(progress.slowest_ms(), Some(940));
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(new_id(), new_id());
    }
}
