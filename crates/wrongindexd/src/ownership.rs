//! Index ownership negotiation (WI-109, compatibility §5-6).
//!
//! Acquisition order: read-only inspection (refuse unsupported profiles before
//! creating anything) -> epoch bump journaled in the registry -> OS guard
//! beside the index -> journal the outcome. The guard lives in memory for as
//! long as this daemon owns the index; the OS drops it if the daemon dies,
//! and startup recovery never trusts a previous instance's recorded state.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Notify;

use serde_json::{Value, json};
use wrongindex_contracts::{BASELINE_PROFILE_ID, ErrorBody, ErrorCode, Ownership, OwnershipStatus};
use wrongindex_registry::now_ms;
use wrongindex_storage::Verdict;
use wrongindex_storage::owner_lock::{AcquireError, OwnerGuard, OwnerInfo};

use crate::server::Shared;
use crate::{info, warn};

#[derive(Default)]
pub struct Owners {
    guards: Mutex<HashMap<String, OwnerGuard>>,
    /// Runs in flight per project (write jobs and reads that open the
    /// store). A guard is never released while its project has one (INV-01:
    /// the OS lock covers the whole write). Counted: a read finishing must
    /// not end a concurrent write's protection.
    runs: Mutex<HashMap<String, usize>>,
    run_done: Notify,
    /// Projects claimed for maintenance over an index the regular admission
    /// refuses. Until a maintenance rebuild succeeds, other write jobs
    /// re-run admission first (the claim must not let the compatibility
    /// worker write into such an index). Cleared on release.
    maintenance: Mutex<HashSet<String>>,
    /// One claim at a time per project. The supervisor (pinned, session),
    /// an explicit `ownership.acquire` and an index request can claim
    /// together; the later one waits and then finds the guard held, instead
    /// of failing on the lock its own process just took.
    claims: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// Held for the duration of one write run; dropping it ends the run.
pub struct RunGuard<'a> {
    owners: &'a Owners,
    project_id: String,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut r) = self.owners.runs.lock()
            && let Some(n) = r.get_mut(&self.project_id)
        {
            *n -= 1;
            if *n == 0 {
                r.remove(&self.project_id);
            }
        }
        self.owners.run_done.notify_waiters();
    }
}

impl Owners {
    /// Start a write run, atomically with checking that this instance holds
    /// the project's guard. `None` when it does not.
    pub fn begin_run(&self, project_id: &str) -> Option<RunGuard<'_>> {
        let guards = self.guards.lock().ok()?;
        if !guards.contains_key(project_id) {
            return None;
        }
        *self
            .runs
            .lock()
            .ok()?
            .entry(project_id.to_string())
            .or_insert(0) += 1;
        drop(guards);
        Some(RunGuard {
            owners: self,
            project_id: project_id.to_string(),
        })
    }

    /// Remove the guard only if no write run is active (same lock order as
    /// `begin_run`, so a run cannot start in between).
    fn take_if_idle(&self, project_id: &str) -> Result<Option<OwnerGuard>, ()> {
        let mut guards = self.guards.lock().map_err(|_| ())?;
        let runs = self.runs.lock().map_err(|_| ())?;
        if runs.contains_key(project_id) {
            return Err(());
        }
        Ok(guards.remove(project_id))
    }

    /// Wait (bounded) until the project has no write run, then take its guard.
    async fn take_when_idle(
        &self,
        project_id: &str,
        wait: Duration,
    ) -> Result<Option<OwnerGuard>, ErrorBody> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.run_done.notified();
            match self.take_if_idle(project_id) {
                Ok(g) => return Ok(g),
                Err(()) => {
                    if tokio::time::timeout_at(deadline, notified).await.is_err() {
                        return Err(ErrorBody::new(
                            ErrorCode::OwnerConflict,
                            "a write run is in progress; ownership was not released",
                        ));
                    }
                }
            }
        }
    }

    pub fn maintenance_pending(&self, project_id: &str) -> bool {
        self.maintenance
            .lock()
            .map(|m| m.contains(project_id))
            .unwrap_or(true)
    }

    pub fn clear_maintenance(&self, project_id: &str) {
        if let Ok(mut m) = self.maintenance.lock() {
            m.remove(project_id);
        }
    }

    fn mark_maintenance(&self, project_id: &str) {
        if let Ok(mut m) = self.maintenance.lock() {
            m.insert(project_id.to_string());
        }
    }

    fn claim_lock(&self, project_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut claims = self.claims.lock().unwrap_or_else(|p| p.into_inner());
        claims.entry(project_id.to_string()).or_default().clone()
    }

    pub fn holds(&self, project_id: &str) -> bool {
        self.guards
            .lock()
            .map(|g| g.contains_key(project_id))
            .unwrap_or(false)
    }

    fn insert(&self, project_id: &str, guard: OwnerGuard) {
        if let Ok(mut g) = self.guards.lock() {
            g.insert(project_id.to_string(), guard);
        }
    }

    /// Take every guard whose project has no run in flight; the others stay
    /// held, and their project ids are returned beside (same lock order as
    /// `begin_run`, so no run can start in between).
    fn take_all_idle(&self) -> (Vec<(String, OwnerGuard)>, Vec<String>) {
        let (Ok(mut guards), Ok(runs)) = (self.guards.lock(), self.runs.lock()) else {
            return (Vec::new(), Vec::new());
        };
        let busy: Vec<String> = guards
            .keys()
            .filter(|p| runs.contains_key(*p))
            .cloned()
            .collect();
        let idle: Vec<String> = guards
            .keys()
            .filter(|p| !runs.contains_key(*p))
            .cloned()
            .collect();
        let taken = idle
            .into_iter()
            .filter_map(|p| guards.remove(&p).map(|g| (p, g)))
            .collect();
        (taken, busy)
    }

    /// [`Self::take_all_idle`], after waiting up to `wait` for the runs in
    /// flight to end.
    async fn take_all_when_idle(&self, wait: Duration) -> (Vec<(String, OwnerGuard)>, Vec<String>) {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.run_done.notified();
            let busy = self.runs.lock().map(|r| !r.is_empty()).unwrap_or(false);
            if !busy || tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.take_all_idle();
            }
        }
    }
}

/// Acquire ownership of a project's index for this daemon instance.
///
/// `maintenance` (explicit user request only) admits an index the regular
/// admission refuses - unreadable, not an index, another older schema,
/// divergent - so that a maintenance `rebuild` job may replace it while
/// keeping it as a backup. A newer or unknown schema is refused even then:
/// replacing it would downgrade data a newer WrongStack wrote.
pub async fn acquire(
    shared: &Arc<Shared>,
    project_id: &str,
    maintenance: bool,
) -> Result<OwnershipStatus, ErrorBody> {
    let claim = shared.owners.claim_lock(project_id);
    let _claiming = claim.lock().await;
    let pid = project_id.to_string();
    let project = shared.registry(move |r| r.get_project(&pid)).await?;
    if shared.owners.holds(project_id) {
        // Re-claiming for maintenance while already owning: the caller
        // intends a rebuild; regular writes re-check admission until then.
        if maintenance {
            shared.owners.mark_maintenance(project_id);
        }
        let pid = project_id.to_string();
        return shared.registry(move |r| r.ownership_status(&pid)).await;
    }

    // 1. Admission: inspect read-only before creating any file.
    let index_dir = PathBuf::from(&project.index_dir);
    let db = index_dir.join("index.db");
    let inspection = tokio::task::spawn_blocking(move || wrongindex_storage::inspect_index(&db))
        .await
        .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
    let regular = inspection.write_admissible || inspection.verdict == Verdict::Missing;
    let admitted = regular || (maintenance && inspection.verdict != Verdict::NewerOrUnknownSchema);
    if !admitted {
        return Err(ErrorBody::new(
            ErrorCode::ProfileUnsupported,
            if inspection.verdict == Verdict::NewerOrUnknownSchema {
                format!(
                    "index verdict is {:?}: written by a newer WrongStack; update WrongIndex. The index is left untouched",
                    inspection.verdict
                )
            } else {
                format!(
                    "index verdict is {:?}; ownership refused and the index left untouched (a maintenance claim allows a rebuild that keeps it as a backup)",
                    inspection.verdict
                )
            },
        ));
    }

    // 1b. Another program with the index open: WrongStack's own index
    //     server, a WrongStack session, or anything else that may be writing
    //     it now. Taking the index over would put a second writer beside it
    //     (INV-01): the guard only stops writers that look for it. Refused
    //     before any file is created; a later claim asks again.
    let dir = index_dir.clone();
    let open_by = tokio::task::spawn_blocking(move || crate::holders::of(&dir))
        .await
        .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
    if !open_by.is_empty() {
        warn!("ownership_refused_open", {"projectId": project_id, "openBy": open_by});
        return Err(ErrorBody::new(
            ErrorCode::OwnerConflict,
            format!(
                "the index is open by {}, which may be writing it (WrongStack's index server or a WrongStack session). WrongIndex takes an index over only when no other program has it open: close that program, or let WrongStack hand the index over (indexing.service: wrongindex). Nothing was changed.",
                crate::holders::describe(&open_by)
            ),
        ));
    }

    // 2. Epoch bump + journal.
    let owner = shared.instance_id.clone();
    let (pid, o) = (project_id.to_string(), owner.clone());
    let epoch = shared.registry(move |r| r.begin_acquire(&pid, &o)).await?;

    // 3. OS guard.
    let owner_info = OwnerInfo {
        owner: "wrongindexd".into(),
        instance_id: owner.clone(),
        pid: std::process::id(),
        epoch,
        profile_id: BASELINE_PROFILE_ID.into(),
        acquired_at_ms: now_ms(),
    };
    let dir = index_dir.clone();
    let attempt = tokio::task::spawn_blocking(move || OwnerGuard::acquire(&dir, owner_info))
        .await
        .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;

    // 4. Journal outcome.
    let (pid, o) = (project_id.to_string(), owner.clone());
    // 5. Backup before the new owner writes anything (compatibility §6.2):
    //    a consistent copy of the index as the previous owner left it. No
    //    backup, no transfer.
    let attempt = match attempt {
        Ok(guard) => {
            let dir = index_dir.clone();
            let keep = shared.settings.get().backup_keep;
            let me = release_owner_key(shared);
            // This service taking back what it released itself, untouched
            // since (a restart): its own last state, not a transfer. Copying
            // it again would cost a full copy of every index at every start
            // and push the backups of real transfers out of the kept ones.
            match tokio::task::spawn_blocking(move || {
                let why = if maintenance {
                    Err("maintenance claim".to_string())
                } else {
                    wrongindex_storage::backup::release_check(&dir, &me, epoch)
                };
                match why {
                    Ok(()) => Ok((None, None)),
                    Err(why) => wrongindex_storage::backup::snapshot(&dir, epoch, keep)
                        .map(|b| (b, Some(why))),
                }
            })
            .await
            {
                Ok(Ok((backup, why))) => Ok((guard, backup, why)),
                Ok(Err(e)) => Err(Err(format!("backup before transfer failed: {e}"))),
                Err(e) => Err(Err(format!("backup task failed: {e}"))),
            }
        }
        Err(e) => Err(Ok(e)),
    };
    match attempt {
        Err(Err(why)) => {
            let evidence = json!({ "error": why });
            let _ = shared
                .registry(move |r| r.finish_acquire(&pid, &o, epoch, Err(evidence)))
                .await;
            warn!("ownership_backup_failed", {"projectId": project_id, "epoch": epoch, "error": why});
            Err(ErrorBody::new(ErrorCode::Internal, why))
        }
        Ok((guard, backup, why)) => {
            let evidence = json!({
                "lock": index_dir.join(wrongindex_storage::owner_lock::LOCK_FILE).display().to_string(),
                "verdict": inspection.verdict,
                "maintenance": maintenance,
                "backup": backup.map(|b| b.display().to_string()),
                // Why a backup was taken, or that none was needed: this
                // service's own release, untouched since.
                "backupReason": why.unwrap_or_else(|| "unchanged since this service released it".into()),
            });
            let status = shared
                .registry(move |r| r.finish_acquire(&pid, &o, epoch, Ok(evidence)))
                .await?;
            shared.owners.insert(project_id, guard);
            if !regular {
                shared.owners.mark_maintenance(project_id);
            }
            info!("ownership_acquired", {"projectId": project_id, "epoch": epoch, "maintenance": !regular});
            shared.events.publish(
                "ownership.changed",
                Some(project_id),
                json!({"state": status.state.as_str(), "epoch": epoch}),
            );
            Ok(status)
        }
        Err(Ok(AcquireError::Held { holder })) => {
            let evidence = json!({ "holder": holder });
            let _ = shared
                .registry(move |r| r.finish_acquire(&pid, &o, epoch, Err(evidence)))
                .await;
            warn!("ownership_conflict", {"projectId": project_id, "epoch": epoch});
            Err(ErrorBody::new(
                ErrorCode::OwnerConflict,
                match holder {
                    Some(h) => format!(
                        "index is owned by {} (instance {}, pid {}, epoch {})",
                        h.owner, h.instance_id, h.pid, h.epoch
                    ),
                    None => "index is locked by another process".to_string(),
                },
            ))
        }
        Err(Ok(AcquireError::Io(e))) => {
            let evidence = json!({ "error": e.to_string() });
            let _ = shared
                .registry(move |r| r.finish_acquire(&pid, &o, epoch, Err(evidence)))
                .await;
            Err(ErrorBody::new(
                ErrorCode::Internal,
                format!("cannot create ownership guard: {e}"),
            ))
        }
    }
}

/// Release ownership. `keep_desired` keeps the intent for the next instance.
pub async fn release(
    shared: &Arc<Shared>,
    project_id: &str,
    keep_desired: bool,
) -> Result<OwnershipStatus, ErrorBody> {
    // Never drop the OS lock under an in-flight write (INV-01).
    let taken = shared
        .owners
        .take_when_idle(project_id, Duration::from_secs(30))
        .await?;
    let Some(guard) = taken else {
        let pid = project_id.to_string();
        let status = shared.registry(move |r| r.ownership_status(&pid)).await?;
        if status.state == Ownership::Owned {
            // Recorded as owned without a live guard: never claim a release
            // happened; leave it for recovery.
            return Err(ErrorBody::new(
                ErrorCode::OwnerConflict,
                "registry records ownership but this instance holds no guard",
            ));
        }
        return Ok(status);
    };
    let epoch = guard.info().epoch;
    shared.owners.clear_maintenance(project_id);
    // No run is in flight (take_when_idle); close the engine's pooled handle
    // before the lock is handed back so no handle outlives ownership.
    let pid = project_id.to_string();
    if let Ok(project) = shared.registry(move |r| r.get_project(&pid)).await {
        shared
            .engine
            .release_store(&project.checkout_root, &project.index_dir)
            .await;
    }
    // An explicit release hands the index over (to WrongStack, another
    // service): the next claim is a transfer and backs it up. Only the
    // release at shutdown is marked (see `release_all_for_shutdown`).
    tokio::task::spawn_blocking(move || {
        let _ = std::fs::remove_file(
            guard
                .index_dir()
                .join(wrongindex_storage::backup::RELEASED_FILE),
        );
        guard.release()
    })
    .await
    .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
    let (pid, owner) = (project_id.to_string(), shared.instance_id.clone());
    let status = shared
        .registry(move |r| r.record_release(&pid, &owner, epoch, keep_desired))
        .await?;
    info!("ownership_released", {"projectId": project_id, "epoch": epoch, "keepDesired": keep_desired});
    shared.events.publish(
        "ownership.changed",
        Some(project_id),
        json!({"state": status.state.as_str(), "epoch": epoch}),
    );
    Ok(status)
}

/// Shutdown: release every guard but keep the intent. Each index is marked
/// as this service left it, so that the next start takes it back without a
/// backup when nobody wrote it meanwhile: a restart is not a transfer.
pub async fn release_all_for_shutdown(shared: &Arc<Shared>) {
    // A run that outlived the drain budget - an in-process restore, backup
    // or check on the blocking pool, which the runtime waits for at exit -
    // may still write the index: its lock is kept (INV-01) and the OS drops
    // it with this process. Recorded as owned, as after a crash: the next
    // start never trusts it, and without a release mark backs the index up.
    let (idle, busy) = shared.owners.take_all_when_idle(SHUTDOWN_RUN_WAIT).await;
    for pid in &busy {
        warn!("ownership_kept_for_running_work", {"projectId": pid});
    }
    for (pid, guard) in idle {
        let epoch = guard.info().epoch;
        let me = release_owner_key(shared);
        let _ = tokio::task::spawn_blocking(move || release_marked(guard, &me)).await;
        let (p, owner) = (pid.clone(), shared.instance_id.clone());
        if let Err(e) = shared
            .registry(move |r| r.record_release(&p, &owner, epoch, true))
            .await
        {
            warn!("ownership_release_not_recorded", {"projectId": pid, "error": e.to_string()});
        }
    }
}

/// How long shutdown waits for runs still in flight after the drain budget.
const SHUTDOWN_RUN_WAIT: Duration = Duration::from_secs(5);

/// Who releases an index, for [`wrongindex_storage::backup::mark_released`]:
/// this service's data root (owner epochs are counted per registry), not the
/// instance, which a restart changes.
fn release_owner_key(shared: &Shared) -> String {
    shared.data_root.to_string_lossy().to_lowercase()
}

/// Release the guard and mark the files as this service left them. The mark
/// is written while the lock is still held, so no other writer can come in
/// between.
fn release_marked(guard: wrongindex_storage::owner_lock::OwnerGuard, owner: &str) {
    let (dir, epoch) = (guard.index_dir().to_path_buf(), guard.info().epoch);
    wrongindex_storage::backup::mark_released(&dir, owner, epoch);
    guard.release();
}

/// Startup: re-acquire projects whose ownership is still desired. Failures
/// are recorded (conflict / unsupported) and never retried in a tight loop.
pub async fn restore(shared: &Arc<Shared>, desired: Vec<String>) -> Value {
    let mut out = Vec::new();
    for pid in desired {
        // A maintenance claim is never restored: it was one explicit request.
        let result = acquire(shared, &pid, false).await;
        out.push(json!({
            "projectId": pid,
            "ok": result.is_ok(),
            "error": result.err().map(|e| e.to_string()),
        }));
    }
    Value::Array(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read finishing while a write job runs on the same project must not
    /// make the guard releasable (it did when runs were a plain set).
    #[test]
    fn overlapping_runs_are_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let guard = OwnerGuard::acquire(
            tmp.path(),
            OwnerInfo {
                owner: "test".into(),
                instance_id: "i".into(),
                pid: std::process::id(),
                epoch: 1,
                profile_id: BASELINE_PROFILE_ID.into(),
                acquired_at_ms: 0,
            },
        )
        .unwrap();
        let owners = Owners::default();
        owners.insert("p", guard);
        let write = owners.begin_run("p").unwrap();
        let read = owners.begin_run("p").unwrap();
        drop(read);
        assert!(owners.take_if_idle("p").is_err(), "write still running");
        drop(write);
        assert!(matches!(owners.take_if_idle("p"), Ok(Some(_))));
    }

    fn guard(dir: &std::path::Path) -> OwnerGuard {
        OwnerGuard::acquire(
            dir,
            OwnerInfo {
                owner: "test".into(),
                instance_id: "i".into(),
                pid: std::process::id(),
                epoch: 1,
                profile_id: BASELINE_PROFILE_ID.into(),
                acquired_at_ms: 0,
            },
        )
        .unwrap()
    }

    /// Shutdown releases the locks of idle projects only: a write still in
    /// flight keeps its project's lock (INV-01) until it ends or the process
    /// exits. It used to release every lock.
    #[tokio::test]
    async fn shutdown_keeps_the_lock_of_a_run_in_flight() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let owners = Owners::default();
        owners.insert("idle", guard(a.path()));
        owners.insert("busy", guard(b.path()));
        let run = owners.begin_run("busy").unwrap();
        let started = std::time::Instant::now();
        let (taken, kept) = owners.take_all_when_idle(Duration::from_millis(300)).await;
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "waited for the run"
        );
        assert_eq!(
            taken.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            ["idle"]
        );
        assert_eq!(kept, ["busy"]);
        assert!(owners.holds("busy"), "the busy lock is still held");
        assert!(
            wrongindex_storage::owner_lock::is_held(b.path()),
            "nobody else can take the index"
        );
        // Once the run ends, the next pass takes it at once.
        drop(run);
        let (taken, kept) = owners.take_all_when_idle(Duration::from_secs(5)).await;
        assert_eq!(taken.len(), 1);
        assert!(kept.is_empty());
    }
}
