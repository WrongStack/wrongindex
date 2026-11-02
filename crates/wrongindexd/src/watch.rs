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
}

pub struct Supervisor {
    projects: Mutex<HashMap<String, ProjectWatch>>,
    leases: Mutex<HashMap<String, Lease>>,
    tx: mpsc::Sender<WatchMsg>,
    pub cfg: DebounceConfig,
}

impl Supervisor {
    pub fn new() -> (Self, mpsc::Receiver<WatchMsg>) {
        let (tx, rx) = mpsc::channel(EVENT_QUEUE);
        (
            Self {
                projects: Mutex::new(HashMap::new()),
                leases: Mutex::new(HashMap::new()),
                tx,
                cfg: DebounceConfig::default(),
            },
            rx,
        )
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
                Some(Instant::now() + LAST_CLIENT_GRACE);
        }
    }

    fn session_count(&self, project_id: &str) -> usize {
        self.leases
            .lock()
            .map(|l| l.values().filter(|x| x.project_id == project_id).count())
            .unwrap_or(0)
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
        let p = self.projects.lock();
        let w = p.as_ref().ok().and_then(|p| p.get(project_id));
        WatchStatus {
            watching: w.is_some_and(|w| w.watcher.is_some()),
            watch_health: w
                .map(|w| if w.health.is_empty() { "off" } else { w.health })
                .unwrap_or("off")
                .to_string(),
            pending_paths: w.map(|w| w.dirty.len()).unwrap_or(0),
            reconcile_reason: w.and_then(|w| w.dirty.reconcile_reason().map(str::to_string)),
            inflight_job: w.and_then(|w| w.inflight.as_ref().map(|(j, _)| j.clone())),
            sessions,
            overflows: w.map(|w| w.dirty.overflows()).unwrap_or(0),
            last_error: w.and_then(|w| w.last_error.clone()),
            initial_scan_done: w.is_some_and(|w| w.initial_done),
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
            w.dirty.mark(full_s, now, &self.cfg);
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
            info!("watch_detached", {"projectId": project_id});
        }
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
                    reconcile_desired(&shared).await;
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

async fn reconcile_desired(shared: &Arc<Shared>) {
    let Ok(projects) = shared.registry(|r| r.list_projects()).await else {
        return;
    };
    let now = Instant::now();
    for p in projects {
        let sessions = shared.watch.session_count(&p.project_id);
        let (attached, grace_until, retry_at) = {
            let g = shared.watch.projects.lock();
            let w = g.as_ref().ok().and_then(|g| g.get(&p.project_id));
            (
                w.is_some_and(|w| w.watcher.is_some()),
                w.and_then(|w| w.grace_until),
                w.and_then(|w| w.retry_at),
            )
        };
        let desired = shared.can_write()
            && match p.watch_policy {
                WatchPolicy::Pinned => true,
                WatchPolicy::Session => sessions > 0 || grace_until.is_some_and(|g| now < g),
                WatchPolicy::Manual | WatchPolicy::Disabled => false,
            };
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
}

/// How often a missing checkout root is looked for again.
const ROOT_POLL: Duration = Duration::from_secs(2);

/// `watchHealth` of a project whose checkout root is missing.
pub const ROOT_UNAVAILABLE: &str = "root_unavailable";

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
        if let Ok(mut g) = shared.watch.projects.lock()
            && let Some(w) = g.get_mut(&pid)
            && let Some((_, batch)) = w.inflight.take()
        {
            w.dirty.settle(&batch, ok, Instant::now());
            if !ok {
                warn!("watch_batch_failed", {
                    "projectId": pid,
                    "consecutiveFailures": w.dirty.failures(),
                });
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
    let now = Instant::now();
    let ready: Vec<(String, Batch)> = shared
        .watch
        .projects
        .lock()
        .map(|g| {
            g.iter()
                .filter(|(_, w)| {
                    w.watcher.is_some()
                        && w.inflight.is_none()
                        && w.dirty.ready(now, &shared.watch.cfg)
                })
                .filter_map(|(pid, w)| w.dirty.take().map(|b| (pid.clone(), b)))
                .collect()
        })
        .unwrap_or_default();
    for (pid, batch) in ready {
        let kind = if batch.reconcile.is_some() {
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
