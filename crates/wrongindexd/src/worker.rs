//! Durable job execution. Jobs are claimed from the registry in admission
//! order; completion is recorded before it becomes observable via `job.get`.
//!
//! M1 executes only `check` jobs: a read-only profile inspection of the
//! project's existing `index.db`. Indexing backends attach in M2/M3.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use wrongindex_contracts::{ErrorBody, ErrorCode, JobKind};

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
            let outcome = execute(&shared, &job.project_id, job.kind).await;
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
        other => Err(ErrorBody::new(
            ErrorCode::NotImplemented,
            format!("'{}' jobs are not executable in this build", other.as_str()),
        )),
    }
}
