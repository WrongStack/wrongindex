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
    /// Projects with a write run in flight. A guard is never released while
    /// its project is here (INV-01: the OS lock covers the whole write).
    runs: Mutex<HashSet<String>>,
    run_done: Notify,
}

/// Held for the duration of one write run; dropping it ends the run.
pub struct RunGuard<'a> {
    owners: &'a Owners,
    project_id: String,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut r) = self.owners.runs.lock() {
            r.remove(&self.project_id);
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
        self.runs.lock().ok()?.insert(project_id.to_string());
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
        if runs.contains(project_id) {
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

    fn take_all(&self) -> Vec<(String, OwnerGuard)> {
        self.guards
            .lock()
            .map(|mut g| g.drain().collect())
            .unwrap_or_default()
    }
}

/// Acquire ownership of a project's index for this daemon instance.
pub async fn acquire(shared: &Arc<Shared>, project_id: &str) -> Result<OwnershipStatus, ErrorBody> {
    let pid = project_id.to_string();
    let project = shared.registry(move |r| r.get_project(&pid)).await?;
    if shared.owners.holds(project_id) {
        let pid = project_id.to_string();
        return shared.registry(move |r| r.ownership_status(&pid)).await;
    }

    // 1. Admission: inspect read-only before creating any file.
    let index_dir = PathBuf::from(&project.index_dir);
    let db = index_dir.join("index.db");
    let inspection = tokio::task::spawn_blocking(move || wrongindex_storage::inspect_index(&db))
        .await
        .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
    if !(inspection.write_admissible || inspection.verdict == Verdict::Missing) {
        return Err(ErrorBody::new(
            ErrorCode::ProfileUnsupported,
            format!(
                "index verdict is {:?}; ownership refused and the index left untouched",
                inspection.verdict
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
    match attempt {
        Ok(guard) => {
            let evidence = json!({
                "lock": index_dir.join(wrongindex_storage::owner_lock::LOCK_FILE).display().to_string(),
                "verdict": inspection.verdict,
            });
            let status = shared
                .registry(move |r| r.finish_acquire(&pid, &o, epoch, Ok(evidence)))
                .await?;
            shared.owners.insert(project_id, guard);
            info!("ownership_acquired", {"projectId": project_id, "epoch": epoch});
            shared.events.publish(
                "ownership.changed",
                Some(project_id),
                json!({"state": status.state.as_str(), "epoch": epoch}),
            );
            Ok(status)
        }
        Err(AcquireError::Held { holder }) => {
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
        Err(AcquireError::Io(e)) => {
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
    tokio::task::spawn_blocking(move || guard.release())
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

/// Shutdown: release every guard but keep the intent.
pub async fn release_all_for_shutdown(shared: &Arc<Shared>) {
    for (pid, guard) in shared.owners.take_all() {
        let epoch = guard.info().epoch;
        let _ = tokio::task::spawn_blocking(move || guard.release()).await;
        let (p, owner) = (pid.clone(), shared.instance_id.clone());
        if let Err(e) = shared
            .registry(move |r| r.record_release(&p, &owner, epoch, true))
            .await
        {
            warn!("ownership_release_not_recorded", {"projectId": pid, "error": e.to_string()});
        }
    }
}

/// Startup: re-acquire projects whose ownership is still desired. Failures
/// are recorded (conflict / unsupported) and never retried in a tight loop.
pub async fn restore(shared: &Arc<Shared>, desired: Vec<String>) -> Value {
    let mut out = Vec::new();
    for pid in desired {
        let result = acquire(shared, &pid).await;
        out.push(json!({
            "projectId": pid,
            "ok": result.is_ok(),
            "error": result.err().map(|e| e.to_string()),
        }));
    }
    Value::Array(out)
}
