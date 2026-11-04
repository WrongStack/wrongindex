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
//! dirty set and run afterwards, so nothing falls into a gap.
//!
//! Scheduling: at most one automatic job per project. Ready batches become
//! fenced jobs (incremental with paths, or full for reconciliation); a
//! finished job settles only the revisions it covered.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};
use serde::Serialize;
use serde_json::json;
use tokio::sync::{mpsc, watch as tokio_watch};
use wrongindex_contracts::{Freshness, JobKind, JobState, Ownership, WatchPolicy};
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
}

struct Lease {
    project_id: String,
    client: String,
    expires: Instant,
}

#[derive(Default)]
struct ProjectWatch {
    watcher: Option<notify::RecommendedWatcher>,
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
}

pub struct Supervisor {
    projects: Mutex<HashMap<String, ProjectWatch>>,
    leases: Mutex<HashMap<String, Lease>>,
    tx: mpsc::Sender<WatchMsg>,
    cfg: Mutex<DebounceConfig>,
    grace: Mutex<Duration>,
    paused: AtomicBool,
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
            last_error: w.and_then(|w| w.last_error.clone()),
            initial_scan_done: w.is_some_and(|w| w.initial_done),
            consecutive_failures: w.map(|w| w.dirty.failures()).unwrap_or(0),
            retry_in_ms: w
                .and_then(|w| w.dirty.retry_in(now))
                .map(|d| d.as_millis() as u64),
            parked,
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
        if last_error.is_some() {
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

    fn attach(&self, project_id: &str, root: &Path, index_dir: &Path) -> Result<(), String> {
        let tx = self.tx.clone();
        let pid = project_id.to_string();
        let overflowed = Arc::new(AtomicBool::new(false));
        let flag = overflowed.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
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
        let mut projects = self.projects.lock().map_err(|_| "poisoned".to_string())?;
        let w = projects.entry(project_id.to_string()).or_default();
        w.watcher = Some(watcher);
        w.root = wrongindex_storage::wrongstack_paths::node_resolve(root);
        w.index_dir = wrongindex_storage::wrongstack_paths::node_resolve(index_dir);
        w.gitignore = Some(Gitignore::load(root));
        w.health = "native";
        w.overflowed = overflowed;
        w.last_error = None;
        w.retry_at = None;
        w.initial_done = false;
        w.disk_wait = None;
        // Watcher first, then the full attach reconciliation.
        w.dirty.mark_reconcile("attach", Instant::now());
        Ok(())
    }

    fn detach(&self, project_id: &str) {
        if let Ok(mut p) = self.projects.lock()
            && let Some(w) = p.get_mut(project_id)
            && w.watcher.take().is_some()
        {
            w.health = "off";
            w.dirty = DirtySet::default();
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
                    shared.watch.expire_leases();
                    let projects = reconcile_desired(&shared).await;
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

/// Admit the periodic reconciliation that is due first, if any (see
/// `schedule`): one per round, only for a project the watcher believes
/// current.
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
        return;
    }
}

/// Attach and detach watchers to match the desired state; returns the
/// registered projects.
async fn reconcile_desired(shared: &Arc<Shared>) -> Vec<wrongindex_contracts::ProjectRecord> {
    let Ok(projects) = shared.registry(|r| r.list_projects()).await else {
        return Vec::new();
    };
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
        let root_present = Path::new(&p.checkout_root).is_dir();
        if attached && !root_present {
            shared.watch.detach(&p.project_id);
            root_unavailable(shared, &p.project_id);
            continue;
        }
        if attached || retry_at.is_some_and(|r| now < r) {
            continue;
        }
        if !root_present {
            root_unavailable(shared, &p.project_id);
            continue;
        }
        // Watching implies writing: take ownership first.
        if !shared.owners.holds(&p.project_id)
            && let Err(e) = crate::ownership::acquire(shared, &p.project_id, false).await
        {
            set_failure(shared, &p.project_id, format!("ownership: {e}"));
            continue;
        }
        match shared.watch.attach(
            &p.project_id,
            Path::new(&p.checkout_root),
            Path::new(&p.index_dir),
        ) {
            Ok(()) => {
                info!("watch_attached", {"projectId": p.project_id, "policy": p.watch_policy.as_str()})
            }
            Err(e) => set_failure(shared, &p.project_id, format!("watcher: {e}")),
        }
    }
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
            Err(e) => set_failure(shared, &pid, format!("admission: {e}")),
        }
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
