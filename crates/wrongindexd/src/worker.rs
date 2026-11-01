//! Durable job execution. Jobs are claimed from the registry in admission
//! order; completion is recorded before it becomes observable via `job.get`.
//!
//! `check` jobs inspect the project's `index.db` read-only. Write jobs run on
//! the native engine when it supports the project (`--backend auto|native`)
//! and otherwise on the WrongStack compatibility worker (`auto|legacy`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::watch;
use wrongindex_contracts::{ErrorBody, ErrorCode, JobKind, JobState, OwnerFence};

use crate::server::{Backend, Shared};
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
                Ok(true) => {
                    let mut running = job.clone();
                    running.state = JobState::Running;
                    crate::events::job_event(&shared, "job.started", &running);
                }
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
                Ok(done) => {
                    info!("job_completed", {
                        "jobId": done.job_id, "projectId": done.project_id,
                        "kind": done.kind.as_str(), "state": done.state.as_str(), "ok": ok,
                    });
                    crate::events::job_event(&shared, "job.finished", &done);
                }
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
            // The run guard keeps the OS ownership lock from being released
            // until this write has finished and been recorded.
            let _run = shared.owners.begin_run(project_id).ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::OwnerConflict,
                    "ownership guard not held by this instance",
                )
            })?;

            let mut native_skipped: Option<String> = None;
            if shared.backend != Backend::Legacy {
                match run_native(shared, &project).await {
                    Ok((stamp, detail)) => {
                        let generation = publish(shared, project_id, fence, stamp.clone()).await?;
                        return Ok(json!({
                            "backend": "native",
                            "native": detail,
                            "graphStamp": stamp,
                            "generation": generation,
                        }));
                    }
                    Err(NativeOutcome::Unsupported(why))
                        if shared.backend == Backend::Auto && shared.engine.configured() =>
                    {
                        native_skipped = Some(why);
                    }
                    Err(NativeOutcome::Unsupported(why)) => {
                        let msg = format!("native engine: {why}");
                        record_failure(shared, project_id, &msg).await;
                        return Err(ErrorBody::new(ErrorCode::NotImplemented, msg));
                    }
                    Err(NativeOutcome::Failed(e)) => {
                        record_failure(shared, project_id, &e.message).await;
                        return Err(e);
                    }
                }
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
            match run {
                Ok(result) => {
                    let stamp = result["graphStamp"].as_str().map(str::to_string);
                    let generation = publish(shared, project_id, fence, stamp).await?;
                    let mut out = result;
                    out["generation"] = json!(generation);
                    out["backend"] = json!("legacy-bun-worker");
                    if let Some(why) = native_skipped {
                        out["nativeSkipped"] = json!(why);
                    }
                    Ok(out)
                }
                Err(e) => {
                    record_failure(shared, project_id, &e.to_string()).await;
                    Err(e)
                }
            }
        }
    }
}

enum NativeOutcome {
    /// The project is outside what the native engine writes (yet).
    Unsupported(String),
    Failed(ErrorBody),
}

/// Full native run (no-op when nothing changed). Returns the graph stamp to
/// publish and a description of what happened.
async fn run_native(
    shared: &Arc<Shared>,
    project: &wrongindex_contracts::ProjectRecord,
) -> Result<(Option<String>, serde_json::Value), NativeOutcome> {
    use wrongindex_engine::native::{NativeError, Rebuild, rebuild};
    // The compatibility worker may hold a pooled handle on this index; close
    // it so the native swap is not refused for our own reader.
    shared
        .engine
        .release_store(&project.checkout_root, &project.index_dir)
        .await;
    let root = std::path::PathBuf::from(&project.checkout_root);
    let dir = std::path::PathBuf::from(&project.index_dir);
    let started = std::time::Instant::now();
    let r = tokio::task::spawn_blocking(move || rebuild(&root, &dir))
        .await
        .map_err(|e| {
            NativeOutcome::Failed(ErrorBody::new(
                ErrorCode::Internal,
                format!("native task failed: {e}"),
            ))
        })?;
    let ms = started.elapsed().as_millis() as u64;
    match r {
        Ok(outcome) => {
            let stamp = match &outcome {
                Rebuild::Unchanged { graph_stamp } => graph_stamp.clone(),
                Rebuild::Written(report) => Some(report.graph_stamp.clone()),
            };
            let mut detail = serde_json::to_value(&outcome).unwrap_or_default();
            detail["durationMs"] = json!(ms);
            Ok((stamp, detail))
        }
        Err(NativeError::Unsupported(why)) => Err(NativeOutcome::Unsupported(why)),
        Err(NativeError::Busy(why)) => Err(NativeOutcome::Failed(ErrorBody::new(
            ErrorCode::DatabaseBusy,
            format!("native engine: {why}"),
        ))),
        Err(e) => Err(NativeOutcome::Failed(ErrorBody::new(
            ErrorCode::Internal,
            format!("native engine: {e}"),
        ))),
    }
}

/// Record a publication. The fence is re-checked together with it: if the
/// configuration or epoch changed during the run, the result is recorded but
/// the job fails so its paths run again.
async fn publish(
    shared: &Arc<Shared>,
    project_id: &str,
    fence: OwnerFence,
    stamp: Option<String>,
) -> Result<i64, ErrorBody> {
    let pid = project_id.to_string();
    let publication = shared
        .registry(move |r| {
            let fenced = r.validate_fence(&pid, &fence);
            let p = r.record_publication(
                &pid,
                stamp.as_deref(),
                fenced.as_ref().err().map(|e| e.message.as_str()),
            )?;
            fenced.map(|()| p)
        })
        .await?;
    shared.events.publish(
        "index.published",
        Some(project_id),
        json!({
            "generation": publication.generation,
            "graphStamp": publication.graph_stamp,
        }),
    );
    Ok(publication.generation)
}

async fn record_failure(shared: &Arc<Shared>, project_id: &str, msg: &str) {
    let (pid, msg) = (project_id.to_string(), msg.to_string());
    let _ = shared
        .registry(move |r| r.record_publication(&pid, None, Some(&msg)))
        .await;
}
