//! Watch supervision, session leases, and the background scheduler
//! (WI-201..204, WI-206/207).
//!
//! Desired state (once per second): a project is watched when its policy is
//! `pinned`, or `session` with a live lease (or within the grace period after
//! the last lease ended), and an indexing engine is configured. Watching needs
//! ownership; a pinned/session project is claimed automatically, and a
//! conflict is reported with a retry back-off, never a tight loop.
//!
//! Attach order (indexing-pipeline §1): register the OS watcher first, then
//! queue a full reconciliation; events arriving during that scan stay in the
//! dirty set and run afterwards, so nothing falls into a gap. A checkout
//! whose file system reports no changes is polled instead (see `poll`); the
//! poller's first pass comes before the reconciliation there. So is one
//! whose watcher turns out silent: nothing reported since the attach while a
//! periodic reconciliation found changes ([`Supervisor::note_missed`]).
//!
//! Scheduling: at most one automatic job per project. Ready batches become
//! fenced jobs (incremental with paths, or full for reconciliation); a
//! finished job settles only the revisions it covered.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};
use serde::Serialize;
use serde_json::json;
use tokio::sync::{mpsc, watch as tokio_watch};
use wrongindex_contracts::{Freshness, JobKind, JobState, Ownership, WatchMode, WatchPolicy};
use wrongindex_discovery::{DEFAULT_IGNORE_DIRS, Gitignore, lang};

use crate::dirty::{Batch, DebounceConfig, DirtySet};
use crate::server::Shared;
use crate::{info, warn};

pub const LEASE: Duration = Duration::from_secs(60);
pub const HEARTBEAT: Duration = Duration::from_secs(15);
pub const LAST_CLIENT_GRACE: Duration = Duration::from_secs(120);
const ATTACH_RETRY: Duration = Duration::from_secs(60);
const EVENT_QUEUE: usize = 10_000;

pub enum WatchMsg {
    Event {
        project_id: String,
        paths: Vec<PathBuf>,
        rescan: bool,
    },
    Error {
        project_id: String,
        message: String,
    },
    /// A poller's first pass is done: what is there now is its baseline,
    /// so the attach reconciliation can run (see `poll`).
    Armed {
        project_id: String,
    },
}

/// How a project is watched.
enum Watching {
    /// The operating system reports changes.
    Native(#[allow(dead_code)] notify::RecommendedWatcher),
    /// Its file system does not: the checkout is walked (see `poll`).
    Poll(crate::poll::Poller),
}

/// `watchHealth` of a project watched by polling.
pub const POLLING: &str = "polling";

struct Lease {
    project_id: String,
    client: String,
    expires: Instant,
}

#[derive(Default)]
struct ProjectWatch {
    watcher: Option<Watching>,
    root: PathBuf,
    index_dir: PathBuf,
    gitignore: Option<Gitignore>,
    dirty: DirtySet,
    inflight: Option<(String, Batch)>,
    health: &'static str,
    last_error: Option<String>,
    retry_at: Option<Instant>,
    grace_until: Option<Instant>,
    overflowed: Arc<AtomicBool>,
    initial_done: bool,
    /// A batch was refused for disk space: free bytes the index's volume
    /// needs before the pending work runs again (no backoff, not counted
    /// toward parking).
    disk_wait: Option<u64>,
    /// SQLite reported the index damaged (see `damage`): what it said. No
    /// work is sent until a rebuild, a restore or a passing check.
    corrupt: Option<String>,
    /// The project's `watchMode` when it was attached.
    mode: WatchMode,
    /// Events the operating system's watcher reported since it was attached,
    /// before any filter (the index's own writes included).
    native_events: u64,
    /// The operating system's watcher was found silent (see
    /// [`Supervisor::note_missed`]): why this project is polled from now on,
    /// for as long as this service runs.
    silent: Option<String>,
    /// Files the last runs left without new rows because they could not be
    /// read or parsed (see [`Supervisor::note_write_result`]).
    incomplete: Option<Incomplete>,
}

/// Files whose index rows are older than the files (INV-05): the project is
/// degraded, not current, until a run indexes them.
#[derive(Debug)]
struct Incomplete {
    /// Absolute paths, as the run reported them.
    files: std::collections::BTreeSet<String>,
    /// Consecutive runs that still left some of them out.
    tries: u32,
    /// When the watcher asks for them again.
    retry_at: Instant,
}

/// First retry of files a run could not read or parse; doubles per run that
/// still leaves them out, up to [`crate::dirty::PARK_PROBE`].
const INCOMPLETE_RETRY: Duration = Duration::from_secs(30);

impl Incomplete {
    fn describe(&self) -> String {
        let n = self.files.len();
        format!(
            "{n} file{} could not be read or parsed in the last run (in use by another program, access denied, or no parser available); {} previous index rows are kept and {} tried again",
            if n == 1 { "" } else { "s" },
            if n == 1 { "its" } else { "their" },
            if n == 1 { "it is" } else { "they are" },
        )
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchStatus {
    pub watching: bool,
    pub watch_health: String,
    pub pending_paths: usize,
    pub reconcile_reason: Option<String>,
    pub inflight_job: Option<String>,
    pub sessions: usize,
    pub overflows: u64,
    pub last_error: Option<String>,
    pub initial_scan_done: bool,
    /// Consecutive failed watcher batches (WI-208).
    pub consecutive_failures: u32,
    /// When the next automatic attempt is allowed, after failures.
    pub retry_in_ms: Option<u64>,
    /// The circuit parked the project: one probe per 15 minutes until a
    /// batch or an explicit job succeeds.
    pub parked: bool,
    /// Watched by polling: how, and whether a pass is overdue.
    pub polling: Option<wrongindex_contracts::PollingStatus>,
}

pub struct Supervisor {
    projects: Mutex<HashMap<String, ProjectWatch>>,
    leases: Mutex<HashMap<String, Lease>>,
    tx: mpsc::Sender<WatchMsg>,
    cfg: Mutex<DebounceConfig>,
    grace: Mutex<Duration>,
    paused: AtomicBool,
    /// `pollIntervalSeconds` in ms, shared with every poller; 0 = no polling.
    poll_every_ms: Arc<AtomicU64>,
}

impl Supervisor {
    pub fn new() -> (Self, mpsc::Receiver<WatchMsg>) {
        let (tx, rx) = mpsc::channel(EVENT_QUEUE);
        (
            Self {
                projects: Mutex::new(HashMap::new()),
                leases: Mutex::new(HashMap::new()),
                tx,
                cfg: Mutex::new(DebounceConfig::default()),
                grace: Mutex::new(LAST_CLIENT_GRACE),
                paused: AtomicBool::new(false),
                poll_every_ms: Arc::new(AtomicU64::new(
                    wrongindex_contracts::DaemonSettings::default().poll_interval_seconds * 1000,
                )),
            },
            rx,
        )
    }

    /// Apply service settings (debounce, grace, pause).
    pub fn configure(&self, s: &wrongindex_contracts::DaemonSettings) {
        if let Ok(mut c) = self.cfg.lock() {
            c.debounce = Duration::from_millis(s.watch_debounce_ms);
            c.max_wait = Duration::from_millis(s.watch_max_wait_ms);
            c.park_after = s.watch_park_after_failures;
        }
        if let Ok(mut g) = self.grace.lock() {
            *g = Duration::from_secs(s.session_grace_seconds);
        }
        self.paused.store(s.paused, Ordering::SeqCst);
        self.poll_every_ms.store(
            s.poll_interval_seconds.saturating_mul(1000),
            Ordering::SeqCst,
        );
    }

    /// The `watchMode` the project was attached with, while it is watched.
    fn attached_mode(&self, project_id: &str) -> Option<WatchMode> {
        self.projects.lock().ok().and_then(|g| {
            g.get(project_id)
                .filter(|w| w.watcher.is_some())
                .map(|w| w.mode)
        })
    }

    /// Whether the project is watched by polling.
    fn is_polling(&self, project_id: &str) -> bool {
        self.projects.lock().is_ok_and(|g| {
            g.get(project_id)
                .is_some_and(|w| matches!(w.watcher, Some(Watching::Poll(_))))
        })
    }

    fn debounce(&self) -> DebounceConfig {
        self.cfg.lock().map(|c| *c).unwrap_or_default()
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    // ── sessions ────────────────────────────────────────────────────────────

    pub fn acquire_lease(&self, project_id: &str, client: &str) -> String {
        let id = format!("s_{}", uuid::Uuid::new_v4().simple());
        if let Ok(mut l) = self.leases.lock() {
            l.insert(
                id.clone(),
                Lease {
                    project_id: project_id.to_string(),
                    client: client.to_string(),
                    expires: Instant::now() + LEASE,
                },
            );
        }
        id
    }

    /// Extend a lease; `false` if it already expired or never existed.
    pub fn heartbeat(&self, session_id: &str) -> bool {
        self.leases
            .lock()
            .ok()
            .and_then(|mut l| {
                l.get_mut(session_id).map(|lease| {
                    lease.expires = Instant::now() + LEASE;
                })
            })
            .is_some()
    }

    pub fn release_lease(&self, session_id: &str) -> Option<String> {
        let lease = self.leases.lock().ok()?.remove(session_id)?;
        self.note_lease_end(&lease.project_id);
        Some(lease.project_id)
    }

    fn note_lease_end(&self, project_id: &str) {
        if self.session_count(project_id) == 0
            && let Ok(mut p) = self.projects.lock()
        {
            p.entry(project_id.to_string()).or_default().grace_until =
                Some(Instant::now() + self.grace.lock().map(|g| *g).unwrap_or(LAST_CLIENT_GRACE));
        }
    }

    fn session_count(&self, project_id: &str) -> usize {
        self.leases
            .lock()
            .map(|l| l.values().filter(|x| x.project_id == project_id).count())
            .unwrap_or(0)
    }

    /// Whether the project's policy has the service own and watch it now:
    /// `pinned` always; `session` while a client holds a lease or the grace
    /// period after the last one runs.
    pub fn wants(&self, project_id: &str, policy: WatchPolicy) -> bool {
        match policy {
            WatchPolicy::Pinned => true,
            WatchPolicy::Session => {
                self.session_count(project_id) > 0
                    || self.projects.lock().is_ok_and(|p| {
                        p.get(project_id)
                            .and_then(|w| w.grace_until)
                            .is_some_and(|g| Instant::now() < g)
                    })
            }
            WatchPolicy::Manual | WatchPolicy::Disabled => false,
        }
    }

    fn expire_leases(&self) {
        let now = Instant::now();
        let expired: Vec<(String, String, String)> = self
            .leases
            .lock()
            .map(|mut l| {
                let ids: Vec<String> = l
                    .iter()
                    .filter(|(_, v)| v.expires <= now)
                    .map(|(k, _)| k.clone())
                    .collect();
                ids.into_iter()
                    .filter_map(|id| l.remove(&id).map(|x| (id, x.project_id, x.client)))
                    .collect()
            })
            .unwrap_or_default();
        for (id, pid, client) in expired {
            info!("session_expired", {"sessionId": id, "projectId": pid, "client": client});
            self.note_lease_end(&pid);
        }
    }

    // ── status ──────────────────────────────────────────────────────────────

    pub fn status(&self, project_id: &str) -> WatchStatus {
        let sessions = self.session_count(project_id);
        let cfg = self.debounce();
        let now = Instant::now();
        let p = self.projects.lock();
        let w = p.as_ref().ok().and_then(|p| p.get(project_id));
        let parked = w.is_some_and(|w| w.watcher.is_some() && w.dirty.parked(&cfg));
        let disk_wait = w.is_some_and(|w| w.watcher.is_some() && w.disk_wait.is_some());
        let corrupt = w.is_some_and(|w| w.watcher.is_some() && w.corrupt.is_some());
        WatchStatus {
            watching: w.is_some_and(|w| w.watcher.is_some()),
            watch_health: if corrupt {
                INDEX_CORRUPT.to_string()
            } else if disk_wait {
                DISK_FULL.to_string()
            } else if parked {
                PARKED.to_string()
            } else {
                w.map(|w| if w.health.is_empty() { "off" } else { w.health })
                    .unwrap_or("off")
                    .to_string()
            },
            pending_paths: w.map(|w| w.dirty.len()).unwrap_or(0),
            reconcile_reason: w.and_then(|w| w.dirty.reconcile_reason().map(str::to_string)),
            inflight_job: w.and_then(|w| w.inflight.as_ref().map(|(j, _)| j.clone())),
            sessions,
            overflows: w.map(|w| w.dirty.overflows()).unwrap_or(0),
            // A file without its rows is a degraded index (INV-05), shown as
            // the project's error until a run indexes it.
            last_error: w.and_then(|w| {
                w.last_error
                    .clone()
                    .or_else(|| w.incomplete.as_ref().map(Incomplete::describe))
            }),
            initial_scan_done: w.is_some_and(|w| w.initial_done),
            consecutive_failures: w.map(|w| w.dirty.failures()).unwrap_or(0),
            retry_in_ms: w
                .and_then(|w| w.dirty.retry_in(now))
                .map(|d| d.as_millis() as u64),
            parked,
            polling: w.and_then(|w| match &w.watcher {
                Some(Watching::Poll(p)) => Some(p.status()),
                _ => None,
            }),
        }
    }

    /// An explicit (non-watcher) write job for the project succeeded: what
    /// made the watcher's batches fail is gone, so the circuit closes and
    /// pending work runs without waiting out the backoff.
    pub fn note_job_succeeded(&self, project_id: &str, job_id: &str) {
        let Ok(mut g) = self.projects.lock() else {
            return;
        };
        let Some(w) = g.get_mut(project_id) else {
            return;
        };
        if w.inflight.as_ref().is_some_and(|(j, _)| j == job_id) {
            return; // the watcher's own batch: settled with its result
        }
        let failures = w.dirty.failures();
        if w.dirty.clear_failures() {
            w.last_error = None;
            info!("watch_circuit_closed", {
                "projectId": project_id, "jobId": job_id, "consecutiveFailures": failures,
            });
        }
        // A write just went through: there was room after all.
        if w.disk_wait.take().is_some() {
            w.last_error = None;
            info!("watch_disk_wait_ended", {"projectId": project_id, "jobId": job_id});
        }
        // A rebuild or restore put a sound index in place.
        if w.corrupt.take().is_some() {
            w.last_error = None;
            info!("index_corrupt_cleared", {"projectId": project_id, "jobId": job_id});
        }
    }

    /// A write job succeeded with `result`: track the files it could not read
    /// or parse (`unreadableFiles`, `unparsedFiles`). A whole-project run
    /// says which files are still without rows; a targeted run settles the
    /// paths it was given. Returns how many files are still left out.
    pub fn note_write_result(
        &self,
        project_id: &str,
        whole_project: bool,
        targets: &[String],
        result: Option<&serde_json::Value>,
    ) -> usize {
        let native = result.map(|r| &r["native"]);
        let reported: std::collections::BTreeSet<String> = ["unreadableFiles", "unparsedFiles"]
            .iter()
            .flat_map(|k| {
                native
                    .and_then(|n| n[*k].as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(str::to_string))
            })
            .collect();
        let Ok(mut g) = self.projects.lock() else {
            return 0;
        };
        if reported.is_empty() && g.get(project_id).is_none_or(|w| w.incomplete.is_none()) {
            return 0;
        }
        let w = g.entry(project_id.to_string()).or_default();
        let mut files = w
            .incomplete
            .take()
            .map(|i| (i.files, i.tries))
            .unwrap_or_default();
        if whole_project {
            files.0.clear();
        } else {
            let done: HashSet<String> = targets.iter().map(|t| t.to_lowercase()).collect();
            files.0.retain(|f| !done.contains(&f.to_lowercase()));
        }
        let grew = reported.iter().any(|f| !files.0.contains(f));
        files.0.extend(reported);
        if files.0.is_empty() {
            info!("incomplete_files_indexed", {"projectId": project_id});
            return 0;
        }
        let tries = if grew { 0 } else { files.1.saturating_add(1) };
        let delay = INCOMPLETE_RETRY
            .saturating_mul(1u32 << tries.min(16))
            .min(crate::dirty::PARK_PROBE);
        let n = files.0.len();
        warn!("incomplete_files", {"projectId": project_id, "files": n, "retryInSeconds": delay.as_secs()});
        w.incomplete = Some(Incomplete {
            files: files.0,
            tries,
            retry_at: Instant::now() + delay,
        });
        n
    }

    /// Ask again for the files a run could not read or parse, once their
    /// back-off is over and nothing else is pending for the project. A file
    /// that is readable again may get no change event of its own (a program
    /// that only held it open): without this it would wait for the periodic
    /// reconciliation.
    pub fn retry_incomplete(&self) {
        let now = Instant::now();
        let cfg = self.debounce();
        let Ok(mut g) = self.projects.lock() else {
            return;
        };
        for w in g.values_mut() {
            if w.watcher.is_none() || w.inflight.is_some() || !w.dirty.is_empty() {
                continue;
            }
            let Some(inc) = w.incomplete.as_mut() else {
                continue;
            };
            if inc.retry_at > now {
                continue;
            }
            for f in &inc.files {
                w.dirty.mark(f.clone(), now, &cfg);
            }
            // The run reports again; until then, no second request.
            inc.retry_at = now + crate::dirty::PARK_PROBE;
        }
    }

    /// SQLite reported the project's index damaged: stop sending it work.
    /// `true` when the project was not marked yet.
    pub fn note_corrupt(&self, project_id: &str, e: &wrongindex_contracts::ErrorBody) -> bool {
        let Ok(mut g) = self.projects.lock() else {
            return false;
        };
        let w = g.entry(project_id.to_string()).or_default();
        w.last_error = Some(e.to_string());
        w.corrupt.replace(e.message.clone()).is_none()
    }

    /// What SQLite said, while the project's index is marked damaged.
    pub fn corrupt(&self, project_id: &str) -> Option<String> {
        self.projects
            .lock()
            .ok()
            .and_then(|g| g.get(project_id).and_then(|w| w.corrupt.clone()))
    }

    /// The index was found sound (a check that passed, or a write job that
    /// looked again): pending work runs again.
    pub fn clear_corrupt(&self, project_id: &str, why: &str) {
        if let Ok(mut g) = self.projects.lock()
            && let Some(w) = g.get_mut(project_id)
            && w.corrupt.take().is_some()
        {
            w.last_error = None;
            info!("index_corrupt_cleared", {"projectId": project_id, "by": why});
        }
    }

    /// Freshness is separate from activity (WI-ADR-012): an idle project with
    /// pending paths or a failed run is never reported current.
    pub fn freshness(status: &WatchStatus, last_error: Option<&str>) -> Freshness {
        if status.watch_health == "failed" || status.watch_health == ROOT_UNAVAILABLE {
            return Freshness::Unavailable;
        }
        if !status.watching {
            return Freshness::Unknown;
        }
        // Parked work, work waiting for disk space, or a damaged index does
        // not progress until something changes: not merely pending.
        if status.parked || status.watch_health == DISK_FULL || status.watch_health == INDEX_CORRUPT
        {
            return Freshness::Degraded;
        }
        if !status.initial_scan_done
            || status.pending_paths > 0
            || status.reconcile_reason.is_some()
            || status.inflight_job.is_some()
        {
            return Freshness::Pending;
        }
        // Polling: current only while its passes keep coming (one pass long
        // ago says nothing about now).
        if last_error.is_some() || status.polling.as_ref().is_some_and(|p| p.overdue) {
            return Freshness::Degraded;
        }
        Freshness::Current
    }

    // ── events ──────────────────────────────────────────────────────────────

    fn apply(&self, msg: WatchMsg) {
        let now = Instant::now();
        let Ok(mut projects) = self.projects.lock() else {
            return;
        };
        match msg {
            WatchMsg::Error {
                project_id,
                message,
            } => {
                if let Some(w) = projects.get_mut(&project_id) {
                    w.dirty.mark_reconcile("watcher_error", now);
                    w.last_error = Some(message);
                }
            }
            WatchMsg::Armed { project_id } => {
                if let Some(w) = projects.get_mut(&project_id)
                    && matches!(w.watcher, Some(Watching::Poll(_)))
                {
                    w.dirty.mark_reconcile("attach", now);
                }
            }
            WatchMsg::Event {
                project_id,
                paths,
                rescan,
            } => {
                let Some(w) = projects.get_mut(&project_id) else {
                    return;
                };
                if w.watcher.is_none() {
                    return;
                }
                #[cfg(debug_assertions)]
                if events_lost() {
                    return;
                }
                if matches!(w.watcher, Some(Watching::Native(_))) {
                    w.native_events += 1;
                }
                if rescan || w.overflowed.swap(false, Ordering::SeqCst) {
                    w.dirty.mark_reconcile(
                        if rescan {
                            "watcher_rescan"
                        } else {
                            "event_queue_overflow"
                        },
                        now,
                    );
                    return;
                }
                for path in paths {
                    self.classify(w, &path, now);
                }
            }
        }
    }

    fn classify(&self, w: &mut ProjectWatch, path: &Path, now: Instant) {
        let full = wrongindex_storage::wrongstack_paths::node_resolve(path);
        if full.starts_with(&w.index_dir) {
            return; // the index and its sidecars never trigger work
        }
        let Ok(rel) = full.strip_prefix(&w.root) else {
            return;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() {
            if !w.root.is_dir() {
                w.dirty.mark_reconcile("root_unavailable", now);
            }
            return;
        }
        if rel.split('/').any(|seg| DEFAULT_IGNORE_DIRS.contains(&seg)) {
            return;
        }
        if rel == ".gitignore" {
            w.gitignore = Some(Gitignore::load(&w.root));
            w.dirty.mark_reconcile("ignore_policy_changed", now);
            return;
        }
        let is_dir = full.is_dir();
        if w.gitignore
            .as_ref()
            .is_some_and(|g| g.is_ignored(&rel, is_dir))
        {
            return;
        }
        let full_s = full.to_string_lossy().into_owned();
        // Directories and vanished paths go through WrongStack's targeted
        // expansion (a renamed/deleted directory arrives as one event).
        if is_dir || !full.exists() || lang::is_indexable(&full_s) {
            w.dirty.mark(full_s, now, &self.debounce());
        }
    }

    /// Watch `root`: through the operating system's change notifications,
    /// or by polling where its file system refuses them. Returns the watch
    /// health.
    fn attach(
        &self,
        project_id: &str,
        root: &Path,
        index_dir: &Path,
        mode: WatchMode,
    ) -> Result<&'static str, String> {
        // Test hook: opening the root for watching is slow, as on a share
        // that answers, slowly.
        #[cfg(debug_assertions)]
        debug_delay("WRONGINDEX_DEBUG_SLOW_ATTACH", root);
        let overflowed = Arc::new(AtomicBool::new(false));
        let silent = self
            .projects
            .lock()
            .ok()
            .and_then(|g| g.get(project_id).and_then(|w| w.silent.clone()));
        let reason = match mode {
            WatchMode::Poll => Some("polling chosen for this project (watchMode poll)".to_string()),
            WatchMode::Auto => silent.or_else(|| crate::poll::refused(root)),
        };
        let watching = match reason {
            None => Watching::Native(self.native(project_id, root, overflowed.clone())?),
            Some(reason) => {
                let every_ms = self.poll_every_ms.load(Ordering::SeqCst);
                if every_ms == 0 {
                    return Err(format!("{reason}; polling is off (pollIntervalSeconds 0)"));
                }
                info!("watch_polling", {"projectId": project_id, "reason": reason, "everyMs": every_ms});
                Watching::Poll(crate::poll::Poller::start(
                    project_id,
                    root,
                    index_dir,
                    self.poll_every_ms.clone(),
                    reason,
                    self.tx.clone(),
                )?)
            }
        };
        let polled = matches!(watching, Watching::Poll(_));
        let mut projects = self.projects.lock().map_err(|_| "poisoned".to_string())?;
        let w = projects.entry(project_id.to_string()).or_default();
        w.watcher = Some(watching);
        w.root = wrongindex_storage::wrongstack_paths::node_resolve(root);
        w.index_dir = wrongindex_storage::wrongstack_paths::node_resolve(index_dir);
        w.gitignore = Some(Gitignore::load(root));
        w.health = if polled { POLLING } else { "native" };
        w.overflowed = overflowed;
        w.last_error = None;
        w.retry_at = None;
        w.initial_done = false;
        w.disk_wait = None;
        w.mode = mode;
        w.native_events = 0;
        // Watcher first, then the full attach reconciliation (a poller asks
        // for it after its first pass).
        if !polled {
            w.dirty.mark_reconcile("attach", Instant::now());
        }
        Ok(w.health)
    }

    fn native(
        &self,
        project_id: &str,
        root: &Path,
        flag: Arc<AtomicBool>,
    ) -> Result<notify::RecommendedWatcher, String> {
        let tx = self.tx.clone();
        let pid = project_id.to_string();
        // Test hook, debug builds only: the watcher is set up and never
        // reports, as on a file system that accepts the request and never
        // answers it.
        #[cfg(debug_assertions)]
        let silent = std::env::var_os("WRONGINDEX_DEBUG_SILENT_WATCHER").is_some();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            #[cfg(debug_assertions)]
            if silent {
                return;
            }
            let msg = match res {
                Ok(ev) => WatchMsg::Event {
                    project_id: pid.clone(),
                    rescan: ev.need_rescan(),
                    paths: ev.paths,
                },
                Err(e) => WatchMsg::Error {
                    project_id: pid.clone(),
                    message: e.to_string(),
                },
            };
            if tx.try_send(msg).is_err() {
                // Bounded queue full: lose detail, never correctness.
                flag.store(true, Ordering::SeqCst);
            }
        })
        .map_err(|e| e.to_string())?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .map_err(|e| e.to_string())?;
        Ok(watcher)
    }

    /// A periodic reconciliation found `files` changed files that no event
    /// had reported. Where the operating system's watcher has reported
    /// nothing at all since it was attached, it is silent: some file systems
    /// (SMB servers among them) accept the request for change notifications
    /// and never answer it, which [`crate::poll::refused`] cannot tell. Such a
    /// project (`watchMode` auto) is polled from then on: the watcher is
    /// dropped here and the next round attaches a poller, whose first pass
    /// and the attach reconciliation follow. `true` when it switched.
    ///
    /// A watcher that reported anything is not silent; a missed change there
    /// (an event queue the OS dropped) is only logged by the caller. With
    /// polling off (`pollIntervalSeconds` 0) the watcher stays and the
    /// project shows why its edits wait for the next reconciliation.
    pub fn note_missed(&self, project_id: &str, files: u64) -> bool {
        let Ok(mut g) = self.projects.lock() else {
            return false;
        };
        let Some(w) = g.get_mut(project_id) else {
            return false;
        };
        if !matches!(w.watcher, Some(Watching::Native(_)))
            || w.native_events > 0
            || w.mode != WatchMode::Auto
        {
            return false;
        }
        let reason = format!(
            "the file system accepted the request for change notifications but reported nothing, while a periodic reconciliation found {files} changed file{}",
            if files == 1 { "" } else { "s" }
        );
        if self.poll_every_ms.load(Ordering::SeqCst) == 0 {
            warn!("watch_silent", {"projectId": project_id, "files": files, "polling": false});
            w.last_error = Some(format!(
                "watcher: {reason}; polling is off (pollIntervalSeconds 0), so edits are found only by the periodic reconciliation"
            ));
            return false;
        }
        warn!("watch_silent", {"projectId": project_id, "files": files, "polling": true});
        w.silent = Some(reason);
        w.watcher = None;
        w.health = "off";
        w.dirty = DirtySet::default();
        w.inflight = None;
        w.initial_done = false;
        w.disk_wait = None;
        true
    }

    /// How many projects have a watcher attached.
    pub fn watched(&self) -> usize {
        self.projects
            .lock()
            .map_or(0, |g| g.values().filter(|w| w.watcher.is_some()).count())
    }

    /// Drop the watch state of every project that is no longer registered:
    /// its watcher (and the directory handle or poller behind it), pending
    /// paths and in-flight batch. Returns how many were dropped.
    pub fn forget_unregistered(&self, registered: &HashSet<&str>) -> usize {
        let Ok(mut g) = self.projects.lock() else {
            return 0;
        };
        let before = g.len();
        g.retain(|pid, _| registered.contains(pid.as_str()));
        before - g.len()
    }

    fn detach(&self, project_id: &str) {
        if let Ok(mut p) = self.projects.lock()
            && let Some(w) = p.get_mut(project_id)
            && w.watcher.take().is_some()
        {
            w.health = "off";
            w.dirty = DirtySet::default();
            // The job of a batch in flight runs on, but it settles nothing:
            // the next attach starts a dirty set whose revisions begin again,
            // and the old batch would clear its attach reconciliation.
            w.inflight = None;
            w.initial_done = false;
            w.disk_wait = None;
            info!("watch_detached", {"projectId": project_id});
        }
    }

    /// Watched, its first reconciliation done, nothing pending, in flight,
    /// failing, waiting for disk space or known damaged: a project the
    /// watcher believes current.
    pub fn settled(&self, project_id: &str) -> bool {
        self.projects.lock().is_ok_and(|g| {
            g.get(project_id).is_some_and(|w| {
                w.watcher.is_some()
                    && w.initial_done
                    && w.dirty.is_empty()
                    && w.inflight.is_none()
                    && w.dirty.failures() == 0
                    && w.disk_wait.is_none()
                    && w.corrupt.is_none()
            })
        })
    }

    /// Files found to differ from the index go to the watcher's next batch
    /// (WrongStack's targeted run, as if their events had come). `false`
    /// when the project is not watched.
    pub fn mark_paths<'a>(
        &self,
        project_id: &str,
        paths: impl Iterator<Item = &'a String>,
    ) -> bool {
        let cfg = self.debounce();
        let Ok(mut p) = self.projects.lock() else {
            return false;
        };
        let Some(w) = p.get_mut(project_id).filter(|w| w.watcher.is_some()) else {
            return false;
        };
        let now = Instant::now();
        for path in paths {
            w.dirty.mark(path.clone(), now, &cfg);
        }
        true
    }

    /// A watched project re-reconciles with its checkout (say an older index
    /// was put back). `false` when it is not watched.
    pub fn mark_reconcile(&self, project_id: &str, reason: &str) -> bool {
        let Ok(mut p) = self.projects.lock() else {
            return false;
        };
        let Some(w) = p.get_mut(project_id).filter(|w| w.watcher.is_some()) else {
            return false;
        };
        w.dirty.mark_reconcile(reason, Instant::now());
        info!("watch_reconcile", {"projectId": project_id, "reason": reason});
        true
    }

    /// A change notification that did not fit the bounded queue set its
    /// project's flag. The flag is otherwise only looked at when that
    /// project's next event arrives - which, for the last edit of a burst
    /// (or a quiet project beside a busy one), may be never: reconcile every
    /// flagged project now. Returns how many.
    pub fn sweep_overflowed(&self) -> usize {
        let now = Instant::now();
        let Ok(mut p) = self.projects.lock() else {
            return 0;
        };
        let mut n = 0usize;
        for (pid, w) in p.iter_mut().filter(|(_, w)| w.watcher.is_some()) {
            if w.overflowed.swap(false, Ordering::SeqCst) {
                w.dirty.mark_reconcile("event_queue_overflow", now);
                warn!("watch_event_queue_overflow", {"projectId": pid});
                n += 1;
            }
        }
        n
    }

    /// Every attached project re-reconciles (e.g. after a suspend).
    pub fn mark_all_reconcile(&self, reason: &str) {
        let now = Instant::now();
        if let Ok(mut p) = self.projects.lock() {
            let mut n = 0usize;
            for w in p.values_mut().filter(|w| w.watcher.is_some()) {
                w.dirty.mark_reconcile(reason, now);
                n += 1;
            }
            if n > 0 {
                info!("watch_reconcile_all", {"reason": reason, "projects": n});
            }
        }
    }

    pub fn detach_all(&self) {
        if let Ok(mut p) = self.projects.lock() {
            for w in p.values_mut() {
                w.watcher = None;
                w.health = "off";
            }
        }
    }
}

/// Test hook, debug builds only: while the file named by
/// `WRONGINDEX_DEBUG_DROP_EVENTS_FILE` exists, watch events are lost without
/// a word, as an OS queue that overflowed silently or a share that never
/// reports would lose them.
#[cfg(debug_assertions)]
fn events_lost() -> bool {
    std::env::var_os("WRONGINDEX_DEBUG_DROP_EVENTS_FILE").is_some_and(|f| Path::new(&f).exists())
}

/// Whether a checkout root is a directory now. On a share that went away
/// this can take seconds (SMB times out).
fn root_is_dir(root: &Path) -> bool {
    // Test hook, debug builds only: `WRONGINDEX_DEBUG_SLOW_ROOT=<text>:<ms>`
    // makes the check of a root containing `<text>` take `<ms>`, as on a
    // network share that stopped answering.
    #[cfg(debug_assertions)]
    debug_delay("WRONGINDEX_DEBUG_SLOW_ROOT", root);
    root.is_dir()
}

/// Test hooks, debug builds only: `<var>=<text>:<ms>` makes the operation
/// on a root containing `<text>` take `<ms>` longer.
#[cfg(debug_assertions)]
fn debug_delay(var: &str, root: &Path) {
    if let Ok(v) = std::env::var(var)
        && let Some((text, ms)) = v.rsplit_once(':')
        && let Ok(ms) = ms.parse::<u64>()
        && root.to_string_lossy().contains(text)
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// How long one supervisor round waits for the attaches it started.
const ATTACH_WAIT: Duration = Duration::from_millis(200);

/// How many projects take ownership for watching at once: a claim may back
/// the index up (`VACUUM INTO`, seconds for a large index) on the blocking
/// pool, which the registry needs too.
const ATTACH_CLAIMS: usize = 2;

/// A watcher being attached off the supervisor: taking ownership (which may
/// back the index up) and opening the root for watching (the file-system
/// probe, the watcher's directory handle and change request, `.gitignore`)
/// take as long as the index and the share, and the supervisor must not
/// wait for them.
struct PendingAttach {
    answer: tokio::sync::oneshot::Receiver<Result<&'static str, String>>,
    since: Instant,
    policy: &'static str,
    mode: &'static str,
}

/// Attaches in flight, by project.
struct Attaching {
    pending: HashMap<String, PendingAttach>,
    claims: Arc<tokio::sync::Semaphore>,
}

impl Default for Attaching {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            claims: Arc::new(tokio::sync::Semaphore::new(ATTACH_CLAIMS)),
        }
    }
}

/// Take ownership (watching implies writing), then attach the watcher on a
/// thread of its own. The error says which step failed.
async fn claim_and_attach(
    shared: Arc<Shared>,
    project_id: String,
    root: PathBuf,
    index_dir: PathBuf,
    mode: WatchMode,
    claims: Arc<tokio::sync::Semaphore>,
) -> Result<&'static str, String> {
    if !shared.owners.holds(&project_id) {
        let _slot = claims
            .acquire()
            .await
            .map_err(|e| format!("ownership: {e}"))?;
        if shared.draining.load(Ordering::SeqCst) {
            return Err("ownership: the service is shutting down".into());
        }
        crate::ownership::acquire(&shared, &project_id, false)
            .await
            .map_err(|e| format!("ownership: {e}"))?;
    }
    let (tx, answer) = tokio::sync::oneshot::channel();
    let s2 = shared.clone();
    std::thread::Builder::new()
        .name("wrongindex-watch-attach".into())
        .spawn(move || {
            let _ = tx.send(s2.watch.attach(&project_id, &root, &index_dir, mode));
        })
        .map_err(|e| format!("watcher: {e}"))?;
    answer
        .await
        .map_err(|_| "watcher: the attach ended without an answer".to_string())?
        .map_err(|e| format!("watcher: {e}"))
}

/// Take the attaches that end within `wait` in all.
async fn collect_attaches(shared: &Arc<Shared>, attaching: &mut Attaching, wait: Duration) {
    let deadline = tokio::time::Instant::now() + wait;
    let mut done = Vec::new();
    for (pid, a) in attaching.pending.iter_mut() {
        if let Ok(answer) = tokio::time::timeout_at(deadline, &mut a.answer).await {
            done.push((pid.clone(), answer));
        }
    }
    for (pid, answer) in done {
        let Some(a) = attaching.pending.remove(&pid) else {
            continue;
        };
        match answer {
            Ok(Ok(health)) => info!("watch_attached", {
                "projectId": pid, "policy": a.policy, "watchHealth": health, "watchMode": a.mode,
            }),
            Ok(Err(e)) => set_failure(shared, &pid, e),
            Err(_) => set_failure(
                shared,
                &pid,
                "watcher: the attach ended without an answer".into(),
            ),
        }
    }
}

/// How long one supervisor round waits for the root checks it started.
const ROOT_PROBE_WAIT: Duration = Duration::from_millis(100);
/// A root whose check has not answered within this long is unavailable.
const ROOT_PROBE_STALE: Duration = Duration::from_secs(10);

/// Whether each checkout root is there, checked off the supervisor: on a
/// share that stopped answering one check takes seconds, and a supervisor
/// that waited for it would hold back the watch of every other project. One
/// check per project at a time, each on a thread of its own (not the
/// blocking pool, which SQLite work needs); the supervisor waits for the
/// checks of one round at most [`ROOT_PROBE_WAIT`] in all and otherwise
/// goes on with the last answer. There is no cap beyond one check per
/// project: one spent in registration order left the roots past it
/// unchecked and their projects unwatched, and the threads are bounded by
/// the projects anyway.
#[derive(Default)]
struct RootProbes {
    probes: HashMap<String, RootProbe>,
}

#[derive(Default)]
struct RootProbe {
    /// The last answer; `None` before the first.
    present: Option<bool>,
    running: Option<(Instant, tokio::sync::oneshot::Receiver<bool>)>,
}

impl RootProbes {
    /// Start a check of `root` unless one is still running.
    fn start(&mut self, project_id: &str, root: &str) {
        let probe = self.probes.entry(project_id.to_string()).or_default();
        if probe.running.is_some() {
            return;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let root = PathBuf::from(root);
        let spawned = std::thread::Builder::new()
            .name("wrongindex-root-check".into())
            .spawn(move || {
                let _ = tx.send(root_is_dir(&root));
            });
        if spawned.is_ok() {
            probe.running = Some((Instant::now(), rx));
        }
    }

    /// Take the answers that arrive within `wait`; a check that has not
    /// answered for [`ROOT_PROBE_STALE`] counts as an unavailable root.
    async fn collect(&mut self, wait: Duration) {
        let deadline = tokio::time::Instant::now() + wait;
        for probe in self.probes.values_mut() {
            let Some((since, rx)) = probe.running.as_mut() else {
                continue;
            };
            match tokio::time::timeout_at(deadline, rx).await {
                Ok(answer) => {
                    // An answer that came too late is no better than none
                    // (else such a root would be watched again and given up
                    // on in turn, with a full reconciliation each time); a
                    // check thread that died leaves the answer unknown.
                    let late = since.elapsed() >= ROOT_PROBE_STALE;
                    probe.present = match answer {
                        Ok(_) if late => Some(false),
                        Ok(present) => Some(present),
                        Err(_) => probe.present,
                    };
                    probe.running = None;
                }
                Err(_) if since.elapsed() >= ROOT_PROBE_STALE => probe.present = Some(false),
                Err(_) => {}
            }
        }
    }

    fn present(&self, project_id: &str) -> Option<bool> {
        self.probes.get(project_id).and_then(|p| p.present)
    }

    fn retain(&mut self, registered: &HashSet<&str>) {
        self.probes
            .retain(|pid, _| registered.contains(pid.as_str()));
    }
}

/// The supervisor checks in about once a second; a wall-clock gap far
/// beyond that means the machine slept (or the clock jumped).
const SUSPEND_GAP: Duration = Duration::from_secs(30);

fn suspended_between(before: std::time::SystemTime, after: std::time::SystemTime) -> bool {
    match after.duration_since(before) {
        Ok(gap) => gap >= SUSPEND_GAP,
        // The clock went backwards: treat as a jump too.
        Err(e) => e.duration() >= SUSPEND_GAP,
    }
}

/// Supervisor loop.
pub async fn run(
    shared: Arc<Shared>,
    mut rx: mpsc::Receiver<WatchMsg>,
    mut shutdown: tokio_watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut last_reconcile = Instant::now() - Duration::from_secs(10);
    let mut last_disk = Instant::now() - DISK_CHECK;
    let mut last_check_round = Instant::now();
    let mut last_reconcile_round = Instant::now();
    let mut last_wall = std::time::SystemTime::now();
    let mut roots = RootProbes::default();
    let mut attaching = Attaching::default();
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(m) => shared.watch.apply(m),
                None => return,
            },
            _ = tick.tick() => {
                if last_reconcile.elapsed() >= Duration::from_secs(1) {
                    last_reconcile = Instant::now();
                    let wall = std::time::SystemTime::now();
                    if suspended_between(last_wall, wall) {
                        // Watch events across a suspend are not guaranteed:
                        // catch up with a reconciliation (a no-op when
                        // nothing changed).
                        shared.watch.mark_all_reconcile("resumed");
                    }
                    last_wall = wall;
                    shared.watch.sweep_overflowed();
                    shared.watch.retry_incomplete();
                    shared.watch.expire_leases();
                    let projects = reconcile_desired(&shared, &mut roots, &mut attaching).await;
                    let monitor_due = last_disk.elapsed() >= DISK_CHECK;
                    if monitor_due {
                        last_disk = Instant::now();
                    }
                    check_disk(&shared, &projects, monitor_due).await;
                    let settings = shared.settings.get();
                    if let Some(s) =
                        crate::schedule::Schedule::checks(settings.integrity_check_hours)
                        && last_check_round.elapsed() >= s.round()
                    {
                        last_check_round = Instant::now();
                        schedule_checks(&shared, &projects, s).await;
                    }
                    if let Some(s) =
                        crate::schedule::Schedule::reconciles(settings.reconcile_minutes)
                        && last_reconcile_round.elapsed() >= s.round()
                    {
                        last_reconcile_round = Instant::now();
                        schedule_reconciles(&shared, &projects, s).await;
                    }
                    crate::server::publish_state_changes(&shared).await;
                }
                settle_finished(&shared).await;
                dispatch_ready(&shared).await;
            }
            _ = shutdown.changed() => {
                shared.watch.detach_all();
                return;
            }
        }
    }
}

/// How often the volumes in use are looked at for `resource.warning`.
const DISK_CHECK: Duration = Duration::from_secs(5);
/// How long the supervisor waits for one disk check.
const DISK_CHECK_BUDGET: Duration = Duration::from_secs(2);

/// Disk space (see `disk`): end the wait of projects whose index volume has
/// room again (every tick, only while a project waits), and report volume
/// level changes as `resource.warning` (every `DISK_CHECK`). The same check
/// looks at the owned indexes' write-ahead logs (see `wal`).
async fn check_disk(
    shared: &Arc<Shared>,
    projects: &[wrongindex_contracts::ProjectRecord],
    monitor_due: bool,
) {
    let waiting: Vec<(String, PathBuf, u64)> = shared
        .watch
        .projects
        .lock()
        .map(|g| {
            g.iter()
                .filter_map(|(pid, w)| {
                    w.disk_wait
                        .map(|need| (pid.clone(), w.index_dir.clone(), need))
                })
                .collect()
        })
        .unwrap_or_default();
    if waiting.is_empty() && !monitor_due {
        return;
    }
    let indexes: Vec<(String, PathBuf)> = if monitor_due {
        projects
            .iter()
            .map(|p| (p.project_id.clone(), PathBuf::from(&p.index_dir)))
            .collect()
    } else {
        Vec::new()
    };
    // Owned indexes for WAL care (see `wal`): those with a job running are
    // left as they are this time.
    let running: Vec<String> = shared
        .progress
        .lock()
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();
    type Owned = Vec<(String, PathBuf)>;
    // A damaged index is left as it is, its log included.
    let (owned_idle, owned_busy): (Owned, Owned) = indexes
        .iter()
        .filter(|(pid, _)| shared.owners.holds(pid) && shared.watch.corrupt(pid).is_none())
        .cloned()
        .partition(|(pid, _)| !running.contains(pid));
    let s2 = shared.clone();
    let check = tokio::task::spawn_blocking(move || {
        // One check at a time; a volume that does not answer (a share that
        // went away) holds only this one.
        let _slot = s2.disk.slot()?;
        let room: Vec<(String, u64, Option<u64>)> = waiting
            .into_iter()
            .map(|(pid, dir, need)| (pid, need, crate::disk::free_bytes(&dir)))
            .collect();
        let changes = if monitor_due {
            s2.disk.check(&s2.data_root, &indexes)
        } else {
            Vec::new()
        };
        let wals: Vec<crate::wal::Seen> = owned_idle
            .into_iter()
            .map(|(project_id, dir)| crate::wal::Seen {
                wal: wrongindex_storage::wal::wal_bytes(&dir),
                index: std::fs::metadata(dir.join("index.db"))
                    .map(|m| m.len())
                    .unwrap_or(0),
                project_id,
                dir,
            })
            .collect();
        Some((room, changes, wals))
    });
    // The supervisor does not wait on a slow volume: a check that takes
    // longer finishes in the background and its result is dropped.
    let Ok(Ok(Some((room, changes, wals)))) = tokio::time::timeout(DISK_CHECK_BUDGET, check).await
    else {
        return;
    };
    if monitor_due {
        let busy: Vec<String> = owned_busy.into_iter().map(|(pid, _)| pid).collect();
        for (pid, change) in shared.wal.observe(&wals, &busy, Instant::now()) {
            if change["level"] == "large" {
                warn!("wal_large", {"projectId": pid, "wal": change.clone()});
            } else {
                info!("wal_ok", {"projectId": pid});
            }
            shared
                .events
                .publish("resource.warning", Some(&pid), change);
        }
        if !shared.wal.due(Instant::now()).is_empty() {
            shared.wake_worker.notify_one();
        }
    }
    for (pid, need, free) in room {
        // Unknown free space is never a reason to wait.
        if free.is_none_or(|f| f >= need)
            && let Ok(mut g) = shared.watch.projects.lock()
            && let Some(w) = g.get_mut(&pid)
            && w.disk_wait.take().is_some()
        {
            info!("watch_disk_space_available", {
                "projectId": pid, "freeBytes": free, "neededBytes": need,
            });
        }
    }
    for change in changes {
        if change["level"] == "ok" {
            info!("disk_space_ok", change.clone());
        } else {
            warn!("disk_space_low", change.clone());
        }
        shared.events.publish("resource.warning", None, change);
    }
}

/// Admit the scheduled health check that is due first, if any (see
/// `schedule`): one per round.
async fn schedule_checks(
    shared: &Arc<Shared>,
    projects: &[wrongindex_contracts::ProjectRecord],
    s: crate::schedule::Schedule,
) {
    if shared.watch.is_paused() {
        return;
    }
    let running: Vec<String> = shared
        .progress
        .lock()
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();
    for p in projects {
        let pid = p.project_id.clone();
        if !shared.owners.holds(&pid) {
            continue;
        }
        let ws = shared.watch.status(&pid);
        let watcher_busy = ws.pending_paths > 0
            || ws.inflight_job.is_some()
            || ws.reconcile_reason.is_some()
            || running.contains(&pid);
        let p2 = pid.clone();
        let Ok((last_check, queued)) = shared
            .registry(move |r| {
                Ok((
                    r.latest_job_of_kind(&p2, JobKind::Check)?,
                    r.latest_job(&p2)?
                        .is_some_and(|j| matches!(j.state, JobState::Queued | JobState::Running)),
                ))
            })
            .await
        else {
            continue;
        };
        let candidate = crate::schedule::Candidate {
            owned: true,
            known_damaged: shared.watch.corrupt(&pid).is_some(),
            busy: watcher_busy || queued,
            last: last_check.as_ref(),
            jitter_ms: 0,
        };
        if !crate::schedule::due(
            s,
            shared.started_at_ms,
            wrongindex_registry::now_ms(),
            &candidate,
        ) {
            continue;
        }
        let p3 = pid.clone();
        match shared
            .registry(move |r| {
                r.admit_job(
                    &p3,
                    JobKind::Check,
                    &crate::schedule::check_params(),
                    "schedule",
                    None,
                )
            })
            .await
        {
            Ok(a) => {
                info!("integrity_check_scheduled", {"projectId": pid, "jobId": a.job.job_id});
                shared.wake_worker.notify_one();
            }
            Err(e) => {
                warn!("integrity_check_not_scheduled", {"projectId": pid, "error": e.message})
            }
        }
        return;
    }
}

/// Admit the periodic reconciliations that are due (see `schedule`):
/// longest overdue first, as many per round as lets every project have its
/// turn within one period, only for projects the watcher believes current.
async fn schedule_reconciles(
    shared: &Arc<Shared>,
    projects: &[wrongindex_contracts::ProjectRecord],
    s: crate::schedule::Schedule,
) {
    if shared.watch.is_paused() {
        return;
    }
    let running: Vec<String> = shared
        .progress
        .lock()
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default();
    let mut due: Vec<(String, Option<i64>)> = Vec::new();
    let watched = shared.watch.watched();
    for p in projects {
        let pid = p.project_id.clone();
        if !shared.watch.settled(&pid) || running.contains(&pid) || !shared.owners.holds(&pid) {
            continue;
        }
        let p2 = pid.clone();
        let Ok((verified, attempt, queued)) = shared
            .registry(move |r| {
                Ok((
                    r.latest_verification(&p2)?,
                    r.latest_job_of_kind(&p2, JobKind::Reconcile)?,
                    r.latest_job(&p2)?
                        .is_some_and(|j| matches!(j.state, JobState::Queued | JobState::Running)),
                ))
            })
            .await
        else {
            continue;
        };
        let candidate = crate::schedule::Candidate {
            owned: true,
            known_damaged: false,
            busy: queued,
            // A failed attempt counts too: no retry before the next period.
            last: crate::schedule::later(verified.as_ref(), attempt.as_ref()),
            jitter_ms: s.jitter_ms(&pid),
        };
        if !crate::schedule::due(
            s,
            shared.started_at_ms,
            wrongindex_registry::now_ms(),
            &candidate,
        ) {
            continue;
        }
        due.push((pid, candidate.last.map(crate::schedule::done_at)));
    }
    // Longest overdue first: with more projects than rounds in a period,
    // taking the first due one in registration order would serve the same
    // first projects for ever.
    let limit = crate::schedule::per_round(s, watched);
    for pid in crate::schedule::pick_overdue(due, limit) {
        let p3 = pid.clone();
        let admitted = shared
            .registry(move |r| {
                let fence = r.fence(&p3)?;
                r.admit_job(
                    &p3,
                    JobKind::Reconcile,
                    &crate::schedule::reconcile_params(&fence),
                    "periodic",
                    None,
                )
            })
            .await;
        match admitted {
            Ok(a) => {
                shared.metrics.reconciliation("periodic");
                info!("periodic_reconcile_scheduled", {"projectId": pid, "jobId": a.job.job_id});
                shared.wake_worker.notify_one();
            }
            Err(e) => {
                warn!("periodic_reconcile_not_scheduled", {"projectId": pid, "error": e.message})
            }
        }
    }
}

/// Attach and detach watchers to match the desired state; returns the
/// registered projects.
async fn reconcile_desired(
    shared: &Arc<Shared>,
    roots: &mut RootProbes,
    attaching: &mut Attaching,
) -> Vec<wrongindex_contracts::ProjectRecord> {
    let Ok(projects) = shared.registry(|r| r.list_projects()).await else {
        return Vec::new();
    };
    // A detached project keeps nothing here: its watcher would hold the
    // checkout open and every later edit would be dispatched to a project
    // that no longer exists.
    let registered: HashSet<&str> = projects.iter().map(|p| p.project_id.as_str()).collect();
    let forgotten = shared.watch.forget_unregistered(&registered);
    if forgotten > 0 {
        info!("watch_forgot_detached", {"projects": forgotten});
    }
    roots.retain(&registered);
    // An attach that ends for a project detached meanwhile is dropped by
    // `forget_unregistered` next round.
    attaching
        .pending
        .retain(|pid, _| registered.contains(pid.as_str()));
    collect_attaches(shared, attaching, Duration::ZERO).await;
    let writable = shared.can_write();
    for p in &projects {
        if writable && shared.watch.wants(&p.project_id, p.watch_policy) {
            roots.start(&p.project_id, &p.checkout_root);
        }
    }
    roots.collect(ROOT_PROBE_WAIT).await;
    let now = Instant::now();
    for p in &projects {
        let (attached, retry_at) = {
            let g = shared.watch.projects.lock();
            let w = g.as_ref().ok().and_then(|g| g.get(&p.project_id));
            (
                w.is_some_and(|w| w.watcher.is_some()),
                w.and_then(|w| w.retry_at),
            )
        };
        let desired = shared.can_write() && shared.watch.wants(&p.project_id, p.watch_policy);
        if !desired {
            if attached {
                shared.watch.detach(&p.project_id);
            }
            continue;
        }
        // A root that is gone (moved, unmounted, a network drive away) is
        // not reported by the watcher on Windows; the old watch is dead and
        // the index can no longer be called current. Detach, and attach
        // again once the root is back (the attach reconciliation catches up).
        // Not known yet (its first check is still running): next round.
        let Some(root_present) = roots.present(&p.project_id) else {
            continue;
        };
        if attached && !root_present {
            shared.watch.detach(&p.project_id);
            root_unavailable(shared, &p.project_id);
            continue;
        }
        // The project's watch mode changed: watch it anew (next round).
        if let Some(mode) = shared.watch.attached_mode(&p.project_id)
            && mode != p.watch_mode
        {
            shared.watch.detach(&p.project_id);
            info!("watch_mode_changed", {
                "projectId": p.project_id, "from": mode.as_str(), "to": p.watch_mode.as_str(),
            });
            continue;
        }
        // Polling turned off: a polled project can no longer be watched.
        if attached
            && shared.watch.poll_every_ms.load(Ordering::SeqCst) == 0
            && shared.watch.is_polling(&p.project_id)
        {
            shared.watch.detach(&p.project_id);
            set_failure(
                shared,
                &p.project_id,
                "watcher: polling is off (pollIntervalSeconds 0)".into(),
            );
            continue;
        }
        if let Some(a) = attaching.pending.get(&p.project_id) {
            // An attach that does not end (a share that holds the open of
            // its root) cannot be cancelled; the project says why it is not
            // watched meanwhile.
            if a.since.elapsed() >= ROOT_PROBE_STALE {
                attach_waiting(shared, &p.project_id, a.since.elapsed());
            }
            continue;
        }
        if attached || retry_at.is_some_and(|r| now < r) {
            continue;
        }
        if !root_present {
            root_unavailable(shared, &p.project_id);
            continue;
        }
        let (tx, answer) = tokio::sync::oneshot::channel();
        let work = claim_and_attach(
            shared.clone(),
            p.project_id.clone(),
            PathBuf::from(&p.checkout_root),
            PathBuf::from(&p.index_dir),
            p.watch_mode,
            attaching.claims.clone(),
        );
        tokio::spawn(async move {
            let _ = tx.send(work.await);
        });
        attaching.pending.insert(
            p.project_id.clone(),
            PendingAttach {
                answer,
                since: Instant::now(),
                policy: p.watch_policy.as_str(),
                mode: p.watch_mode.as_str(),
            },
        );
    }
    // An ordinary root is open at once: attached in this round, as before.
    collect_attaches(shared, attaching, ATTACH_WAIT).await;
    projects
}

/// How often a missing checkout root is looked for again.
const ROOT_POLL: Duration = Duration::from_secs(2);

/// `watchHealth` of a project whose checkout root is missing.
pub const ROOT_UNAVAILABLE: &str = "root_unavailable";
/// Watch health of a project the circuit parked (WI-208).
pub const PARKED: &str = "parked";
/// Watch health of a project whose pending work waits for disk space.
pub const DISK_FULL: &str = "disk_full";
/// Watch health of a project whose index SQLite reports damaged.
pub const INDEX_CORRUPT: &str = "index_corrupt";

fn root_unavailable(shared: &Arc<Shared>, project_id: &str) {
    if let Ok(mut g) = shared.watch.projects.lock() {
        let w = g.entry(project_id.to_string()).or_default();
        if w.health != ROOT_UNAVAILABLE {
            warn!("watch_root_unavailable", {"projectId": project_id});
        }
        w.health = ROOT_UNAVAILABLE;
        w.last_error = Some("the checkout root is unavailable".into());
        // Checking for the root is cheap: look again soon.
        w.retry_at = Some(Instant::now() + ROOT_POLL);
    }
}

/// A project whose attach has been running for `waited`: its error says so
/// while no watcher is attached.
fn attach_waiting(shared: &Arc<Shared>, project_id: &str, waited: Duration) {
    if let Ok(mut g) = shared.watch.projects.lock() {
        let w = g.entry(project_id.to_string()).or_default();
        if w.watcher.is_none() {
            w.last_error = Some(format!(
                "watcher: the checkout root has not opened for watching in {} s",
                waited.as_secs()
            ));
        }
    }
}

fn set_failure(shared: &Arc<Shared>, project_id: &str, message: String) {
    warn!("watch_attach_failed", {"projectId": project_id, "error": message});
    if let Ok(mut g) = shared.watch.projects.lock() {
        let w = g.entry(project_id.to_string()).or_default();
        w.health = "failed";
        w.last_error = Some(message);
        w.retry_at = Some(Instant::now() + ATTACH_RETRY);
    }
}

async fn settle_finished(shared: &Arc<Shared>) {
    let inflight: Vec<(String, String)> = shared
        .watch
        .projects
        .lock()
        .map(|g| {
            g.iter()
                .filter_map(|(pid, w)| w.inflight.as_ref().map(|(j, _)| (pid.clone(), j.clone())))
                .collect()
        })
        .unwrap_or_default();
    for (pid, job_id) in inflight {
        let j = job_id.clone();
        let Ok(job) = shared.registry(move |r| r.get_job(&j)).await else {
            continue;
        };
        if !job.state.is_terminal() {
            continue;
        }
        let ok = job.state == JobState::Succeeded;
        let disk_full = job
            .error
            .as_ref()
            .is_some_and(|e| e.code == wrongindex_contracts::ErrorCode::DiskFull);
        let corrupt = job
            .error
            .as_ref()
            .filter(|e| e.code == wrongindex_contracts::ErrorCode::DatabaseCorrupt);
        let cfg = shared.watch.debounce();
        if let Ok(mut g) = shared.watch.projects.lock()
            && let Some(w) = g.get_mut(&pid)
            && let Some((_, batch)) = w.inflight.take()
        {
            if let Some(e) = corrupt {
                // Retrying cannot help and nothing may write the file: the
                // paths stay dirty for after a rebuild or restore (the worker
                // marked the project already; a watcher job only adds it).
                if w.corrupt.replace(e.message.clone()).is_none() {
                    warn!("index_corrupt", {"projectId": pid, "jobId": job_id, "error": e.message});
                }
                w.last_error = Some(e.to_string());
                continue;
            }
            if disk_full {
                // Not the project's fault and no use retrying on a timer:
                // everything stays dirty until the volume has room again.
                let kind = if batch.reconcile.is_some() {
                    JobKind::Full
                } else {
                    JobKind::Incremental
                };
                let need = crate::disk::needed(kind, &w.index_dir);
                if w.disk_wait.replace(need).is_none() {
                    warn!("watch_waiting_for_disk", {
                        "projectId": pid, "jobId": job_id, "neededBytes": need,
                    });
                }
                w.last_error = job.error.as_ref().map(|e| e.to_string());
                continue;
            }
            let was_parked = w.dirty.parked(&cfg);
            w.dirty.settle(&batch, ok, Instant::now(), &cfg);
            if !ok {
                warn!("watch_batch_failed", {
                    "projectId": pid,
                    "consecutiveFailures": w.dirty.failures(),
                });
                if !was_parked && w.dirty.parked(&cfg) {
                    warn!("watch_parked", {
                        "projectId": pid,
                        "consecutiveFailures": w.dirty.failures(),
                        "probeEverySeconds": crate::dirty::PARK_PROBE.as_secs(),
                    });
                }
            }
            if batch.reconcile.is_some() && ok {
                w.initial_done = true;
            }
            w.last_error = if ok {
                None
            } else {
                job.error.as_ref().map(|e| e.to_string())
            };
        }
    }
}

async fn dispatch_ready(shared: &Arc<Shared>) {
    // Paused: batches keep waiting (freshness stays pending, never current).
    if shared.watch.is_paused() {
        return;
    }
    let now = Instant::now();
    let cfg = shared.watch.debounce();
    let ready: Vec<(String, Batch)> = shared
        .watch
        .projects
        .lock()
        .map(|g| {
            g.iter()
                .filter(|(_, w)| {
                    w.watcher.is_some()
                        && w.inflight.is_none()
                        && w.disk_wait.is_none()
                        && w.corrupt.is_none()
                        && w.dirty.ready(now, &cfg)
                })
                .filter_map(|(pid, w)| w.dirty.take().map(|b| (pid.clone(), b)))
                .collect()
        })
        .unwrap_or_default();
    for (pid, batch) in ready {
        let kind = if let Some(reason) = &batch.reconcile {
            shared.metrics.reconciliation(reason);
            JobKind::Full
        } else {
            JobKind::Incremental
        };
        let p2 = pid.clone();
        let paths = batch.paths.clone();
        let admitted = shared
            .registry(move |r| {
                let status = r.ownership_status(&p2)?;
                if status.state != Ownership::Owned {
                    return Err(wrongindex_contracts::ErrorBody::new(
                        wrongindex_contracts::ErrorCode::OwnerConflict,
                        "not owned",
                    ));
                }
                let fence = r.fence(&p2)?;
                let params = json!({"kind": kind.as_str(), "paths": paths, "fence": fence});
                r.admit_job(&p2, kind, &params, "watcher", None)
            })
            .await;
        match admitted {
            Ok(a) => {
                if let Ok(mut g) = shared.watch.projects.lock()
                    && let Some(w) = g.get_mut(&pid)
                {
                    w.inflight = Some((a.job.job_id.clone(), batch));
                }
                shared.wake_worker.notify_one();
            }
            Err(e) => {
                if let Ok(mut g) = shared.watch.projects.lock()
                    && let Some(w) = g.get_mut(&pid)
                {
                    let was_parked = w.dirty.parked(&cfg);
                    w.dirty.settle(&batch, false, Instant::now(), &cfg);
                    w.last_error = Some(format!("admission: {e}"));
                    warn!("watch_admission_failed", {
                        "projectId": pid, "error": e.message,
                        "consecutiveFailures": w.dirty.failures(),
                    });
                    if !was_parked && w.dirty.parked(&cfg) {
                        warn!("watch_parked", {
                            "projectId": pid,
                            "consecutiveFailures": w.dirty.failures(),
                            "probeEverySeconds": crate::dirty::PARK_PROBE.as_secs(),
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod polling_tests {
    use super::*;

    #[test]
    fn a_polled_project_is_current_only_while_its_passes_keep_coming() {
        let mut s = WatchStatus {
            watching: true,
            watch_health: POLLING.into(),
            pending_paths: 0,
            reconcile_reason: None,
            inflight_job: None,
            sessions: 0,
            overflows: 0,
            last_error: None,
            initial_scan_done: true,
            consecutive_failures: 0,
            retry_in_ms: None,
            parked: false,
            polling: Some(wrongindex_contracts::PollingStatus {
                reason: "r".into(),
                every_ms: 5_000,
                last_pass_ms: Some(1),
                pass_ms: 3,
                files: 2,
                overdue: false,
            }),
        };
        assert_eq!(Supervisor::freshness(&s, None), Freshness::Current);
        s.polling.as_mut().unwrap().overdue = true;
        assert_eq!(Supervisor::freshness(&s, None), Freshness::Degraded);
        // Known pending work still says pending.
        s.pending_paths = 1;
        assert_eq!(Supervisor::freshness(&s, None), Freshness::Pending);
    }

    /// INV-05: a file a run could not read keeps its old rows, and the
    /// project says so until a run indexes it.
    #[test]
    fn files_left_without_rows_keep_the_project_degraded() {
        let (sup, _rx) = Supervisor::new();
        let pid = "p1";
        let unreadable = |files: &[&str]| serde_json::json!({"backend": "native", "native": {"unreadableFiles": files}});
        // A whole-project run leaves one file out.
        assert_eq!(
            sup.note_write_result(pid, true, &[], Some(&unreadable(&["C:/p/a.ts"]))),
            1
        );
        let st = sup.status(pid);
        assert!(
            st.last_error
                .as_deref()
                .is_some_and(|e| e.contains("1 file")),
            "{st:?}"
        );
        let watching = WatchStatus {
            watching: true,
            watch_health: "native".into(),
            initial_scan_done: true,
            ..st.clone()
        };
        assert_eq!(
            Supervisor::freshness(&watching, watching.last_error.as_deref()),
            Freshness::Degraded
        );
        // A targeted run of another path changes nothing; of that path, and
        // without a report, settles it.
        assert_eq!(
            sup.note_write_result(pid, false, &["C:/p/b.ts".into()], Some(&unreadable(&[]))),
            1
        );
        assert_eq!(
            sup.note_write_result(pid, false, &["c:/P/A.ts".into()], Some(&unreadable(&[]))),
            0
        );
        assert_eq!(sup.status(pid).last_error, None);
        // A whole-project run that reports nothing settles everything.
        assert_eq!(
            sup.note_write_result(
                pid,
                true,
                &[],
                Some(&unreadable(&["C:/p/x.ts", "C:/p/y.ts"]))
            ),
            2
        );
        assert_eq!(
            sup.note_write_result(pid, true, &[], Some(&serde_json::json!({}))),
            0
        );
        assert_eq!(sup.status(pid).last_error, None);
    }
}

#[cfg(test)]
mod suspend_tests {
    use super::*;

    #[test]
    fn a_long_wall_clock_gap_counts_as_a_suspend() {
        let t = std::time::SystemTime::now();
        assert!(!suspended_between(t, t + Duration::from_secs(2)));
        assert!(suspended_between(t, t + Duration::from_secs(45)));
        assert!(suspended_between(t + Duration::from_secs(45), t));
        assert!(!suspended_between(t + Duration::from_secs(1), t));
    }
}

#[cfg(test)]
mod root_probe_tests {
    use super::*;

    /// Every registered project's root is checked, however many there are.
    /// A cap on the checks in flight, spent in registration order every
    /// round, left the roots past the sixteenth unchecked, so those projects
    /// were never watched.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_root_is_checked_whatever_the_number_of_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let mut probes = RootProbes::default();
        let roots: Vec<(String, String)> = (0..40)
            .map(|i| {
                let dir = tmp.path().join(format!("p{i}"));
                if i != 39 {
                    std::fs::create_dir(&dir).unwrap();
                }
                (format!("p{i}"), dir.display().to_string())
            })
            .collect();
        for _round in 0..2 {
            for (pid, root) in &roots {
                probes.start(pid, root);
            }
            probes.collect(Duration::from_secs(5)).await;
        }
        for (pid, _) in &roots[..39] {
            assert_eq!(probes.present(pid), Some(true), "{pid}");
        }
        assert_eq!(probes.present("p39"), Some(false), "a missing root");
    }
}
