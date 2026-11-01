//! Durable job execution. Jobs are claimed from the registry in admission
//! order; completion is recorded before it becomes observable via `job.get`.
//!
//! M1 executes only `check` jobs: a read-only profile inspection of the
//! project's existing `index.db`. Indexing backends attach in M2/M3.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::watch;
use wrongindex_contracts::{ErrorBody, ErrorCode, JobKind, OwnerFence};

use crate::server::Shared;
use crate::{info, warn};

/// Fallback poll interval; admission normally wakes the worker immediately.
const IDLE_POLL: Duration = Duration::from_secs(5);

pub async fn run(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        let queued = match shared.registry(|r| r.queued_jobs()).await {
            Ok(q) => q,
            Err(e) => {
                warn!("worker_queue_read_failed", {"error": e.to_string()});
                Vec::new()
            }
        };
        if queued.is_empty() {
            tokio::select! {
                _ = shared.wake_worker.notified() => {}
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(IDLE_POLL) => {}
            }
            continue;
        }
        for job in queued {
            if *shutdown.borrow() {
                return;
            }
            let job_id = job.job_id.clone();
            let claimed = {
                let id = job_id.clone();
                shared.registry(move |r| r.mark_running(&id)).await
            };
            match claimed {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    warn!("worker_claim_failed", {"jobId": job_id, "error": e.to_string()});
                    continue;
                }
            }
            let outcome = execute(&shared, &job.project_id, job.kind, job.params.clone()).await;
            let ok = outcome.is_ok();
            let id = job_id.clone();
            match shared.registry(move |r| r.complete_job(&id, outcome)).await {
                Ok(done) => info!("job_completed", {
                    "jobId": done.job_id, "projectId": done.project_id,
                    "kind": done.kind.as_str(), "state": done.state.as_str(), "ok": ok,
                }),
                Err(e) => {
                    warn!("job_completion_not_recorded", {"jobId": job_id, "error": e.to_string()})
                }
            }
        }
    }
}

async fn execute(
    shared: &Arc<Shared>,
    project_id: &str,
    kind: JobKind,
    params: Option<serde_json::Value>,
) -> Result<serde_json::Value, ErrorBody> {
    let pid = project_id.to_string();
    let project = shared.registry(move |r| r.get_project(&pid)).await?;
    match kind {
        JobKind::Check => {
            let db = Path::new(&project.index_dir).join("index.db");
            let inspection =
                tokio::task::spawn_blocking(move || wrongindex_storage::inspect_index(&db))
                    .await
                    .map_err(|e| {
                        ErrorBody::new(ErrorCode::Internal, format!("inspection task failed: {e}"))
                    })?;
            serde_json::to_value(inspection)
                .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))
        }
        JobKind::Full | JobKind::Incremental | JobKind::Reconcile => {
            let params = params.unwrap_or_default();
            let fence: OwnerFence =
                serde_json::from_value(params["fence"].clone()).map_err(|_| {
                    ErrorBody::new(ErrorCode::OwnerConflict, "job carries no ownership fence")
                })?;
            // INV-14: stale work (owner epoch or configuration changed since
            // admission, or ownership lost) never runs.
            let pid = project_id.to_string();
            shared
                .registry(move |r| r.validate_fence(&pid, &fence))
                .await?;
            if !shared.owners.holds(project_id) {
                return Err(ErrorBody::new(
                    ErrorCode::OwnerConflict,
                    "ownership guard not held by this instance",
                ));
            }
            let mut args = json!({
                "projectRoot": project.checkout_root,
                "indexDir": project.index_dir,
            });
            if kind == JobKind::Incremental {
                args["files"] = params["paths"].clone();
            }
            let run = shared
                .engine
                .call("index", args, Duration::from_secs(30 * 60))
                .await;
            let pid = project_id.to_string();
            match run {
                Ok(result) => {
                    let stamp = result["graphStamp"].as_str().map(str::to_string);
                    let publication = shared
                        .registry(move |r| r.record_publication(&pid, stamp.as_deref(), None))
                        .await?;
                    let mut out = result;
                    out["generation"] = json!(publication.generation);
                    Ok(out)
                }
                Err(e) => {
                    let msg = e.to_string();
                    let _ = shared
                        .registry(move |r| r.record_publication(&pid, None, Some(&msg)))
                        .await;
                    Err(e)
                }
            }
        }
    }
}
