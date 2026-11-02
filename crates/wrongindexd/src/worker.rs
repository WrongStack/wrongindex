//! Durable job execution. Jobs are claimed from the registry in admission
//! order, up to `PARALLEL_JOBS` at once and one per project; completion is recorded before it becomes observable via `job.get`.
//!
//! `check` jobs inspect the project's `index.db` read-only. Write jobs run on
//! the native engine when it supports the project (`--backend auto|native`)
//! and otherwise on the WrongStack compatibility worker (`auto|legacy`).

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::watch;
use tokio::task::JoinSet;
use wrongindex_contracts::{ErrorBody, ErrorCode, JobKind, JobRecord, JobState, OwnerFence};

use crate::server::{Backend, Shared};
use crate::{info, warn};

/// Fallback poll interval; admission normally wakes the worker immediately.
const IDLE_POLL: Duration = Duration::from_secs(5);

/// Jobs run at once. At most one per project, so a slow or hostile project
/// occupies one slot and every other project still progresses in the other
/// (security review WI-604). Native runs are separate confined processes;
/// compatibility-worker calls still serialize on the one WrongStack worker.
pub const PARALLEL_JOBS: usize = 2;

/// Projects with a job in flight; removed when the job's task ends (also
/// on panic).
struct Busy {
    projects: Arc<std::sync::Mutex<HashSet<String>>>,
    project_id: String,
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Ok(mut b) = self.projects.lock() {
            b.remove(&self.project_id);
        }
    }
}

/// The next jobs to start: in admission order, skipping projects that
/// already have a job in flight or picked, at most `slots`.
fn pick(queued: Vec<JobRecord>, busy: &HashSet<String>, slots: usize) -> Vec<JobRecord> {
    let mut taken: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for job in queued {
        if out.len() == slots {
            break;
        }
        if busy.contains(&job.project_id) || !taken.insert(job.project_id.clone()) {
            continue;
        }
        out.push(job);
    }
    out
}

pub async fn run(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    let busy: Arc<std::sync::Mutex<HashSet<String>>> = Arc::default();
    let mut running: JoinSet<()> = JoinSet::new();
    loop {
        while running.try_join_next().is_some() {}
        if *shutdown.borrow() {
            break;
        }
        let mut started = false;
        if running.len() < PARALLEL_JOBS {
            let queued = match shared.registry(|r| r.queued_jobs()).await {
                Ok(q) => q,
                Err(e) => {
                    warn!("worker_queue_read_failed", {"error": e.to_string()});
                    Vec::new()
                }
            };
            let in_flight = busy.lock().map(|b| b.clone()).unwrap_or_default();
            for job in pick(queued, &in_flight, PARALLEL_JOBS - running.len()) {
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
                if let Ok(mut b) = busy.lock() {
                    b.insert(job.project_id.clone());
                }
                let marker = Busy {
                    projects: busy.clone(),
                    project_id: job.project_id.clone(),
                };
                running.spawn(run_job(shared.clone(), job, marker));
                started = true;
            }
        }
        if started {
            continue;
        }
        tokio::select! {
            _ = shared.wake_worker.notified() => {}
            Some(_) = running.join_next(), if !running.is_empty() => {}
            _ = shutdown.changed() => {}
            _ = tokio::time::sleep(IDLE_POLL) => {}
        }
    }
    // Drain: no new job starts; the ones in flight finish (the server bounds
    // this with its drain budget).
    while running.join_next().await.is_some() {}
}

async fn run_job(shared: Arc<Shared>, job: JobRecord, _busy: Busy) {
    let mut running = job.clone();
    running.state = JobState::Running;
    crate::events::job_event(&shared, "job.started", &running);
    let job_id = job.job_id.clone();
    let outcome = execute(&shared, &job.project_id, job.kind, job.params.clone()).await;
    let ok = outcome.is_ok();
    let id = job_id.clone();
    match shared.registry(move |r| r.complete_job(&id, outcome)).await {
        Ok(done) => {
            prune_now_and_then(&shared).await;
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
    // Another project's queued job may be waiting for this slot.
    shared.wake_worker.notify_one();
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
        JobKind::Full | JobKind::Incremental | JobKind::Reconcile | JobKind::Rebuild => {
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

            let replace = kind == JobKind::Rebuild;
            let mut native_skipped: Option<String> = None;
            if shared.backend != Backend::Legacy {
                match run_native(shared, &project, replace).await {
                    Ok((stamp, detail)) => {
                        // The index is now a baseline index the native writer
                        // wrote or accepted.
                        shared.owners.clear_maintenance(project_id);
                        let generation = publish(shared, project_id, fence, stamp.clone()).await?;
                        return Ok(json!({
                            "backend": "native",
                            "native": detail,
                            "graphStamp": stamp,
                            "generation": generation,
                        }));
                    }
                    Err(NativeOutcome::Unsupported(why))
                        if !replace
                            && shared.backend == Backend::Auto
                            && shared.engine.configured() =>
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

            // WrongStack's own engine follows Git metadata wherever it
            // points; never hand it a checkout whose metadata names a share
            // (WI-604; the native path already refuses it in discovery).
            let root = project.checkout_root.clone();
            let remote = tokio::task::spawn_blocking(move || {
                wrongindex_storage::wrongstack_paths::remote_git_link(std::path::Path::new(&root))
            })
            .await
            .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
            if let Some(target) = remote {
                let msg = format!(
                    "the checkout's Git metadata names a network or device path ({target}); refusing to follow it"
                );
                record_failure(shared, project_id, &msg).await;
                return Err(ErrorBody::new(ErrorCode::Forbidden, msg));
            }
            if !replace && shared.owners.maintenance_pending(project_id) {
                // Claimed for maintenance over an index the regular
                // admission refused: WrongStack's worker may write it only
                // once it is admissible again (normally after a rebuild).
                // The native writer has its own stricter gate.
                let db = Path::new(&project.index_dir).join("index.db");
                let inspection =
                    tokio::task::spawn_blocking(move || wrongindex_storage::inspect_index(&db))
                        .await
                        .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
                if inspection.write_admissible
                    || inspection.verdict == wrongindex_storage::Verdict::Missing
                {
                    shared.owners.clear_maintenance(project_id);
                } else {
                    let msg = format!(
                        "index verdict is {:?}: claimed for maintenance; run a rebuild (index --rebuild) before regular indexing",
                        inspection.verdict
                    );
                    record_failure(shared, project_id, &msg).await;
                    return Err(ErrorBody::new(ErrorCode::ProfileUnsupported, msg));
                }
            }
            if replace && shared.backend == Backend::Legacy {
                return Err(ErrorBody::new(
                    ErrorCode::NotImplemented,
                    "a maintenance rebuild needs the native engine (--backend auto or native)",
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

/// Finished jobs kept per project (watcher edits each add one).
pub const KEEP_JOBS: usize = 1000;
/// Ownership transitions kept per project.
pub const KEEP_TRANSITIONS: usize = 500;

/// Bound the registry's history: every 64 completed jobs (and at startup,
/// see `server::run`).
async fn prune_now_and_then(shared: &Arc<Shared>) {
    static COMPLETED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if COMPLETED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 64 == 63 {
        prune(shared).await;
    }
}

pub async fn prune(shared: &Arc<Shared>) {
    match shared
        .registry(|r| r.prune_history(KEEP_JOBS, KEEP_TRANSITIONS))
        .await
    {
        Ok((0, 0)) => {}
        Ok((jobs, transitions)) => {
            info!("history_pruned", {"jobs": jobs, "transitions": transitions});
        }
        Err(e) => warn!("history_prune_failed", {"error": e.message}),
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
    replace: bool,
) -> Result<(Option<String>, serde_json::Value), NativeOutcome> {
    // The compatibility worker may hold a pooled handle on this index; close
    // it so the native swap is not refused for our own reader.
    shared
        .engine
        .release_store(&project.checkout_root, &project.index_dir)
        .await;
    let started = std::time::Instant::now();
    let r = rebuild_in_child(&project.checkout_root, &project.index_dir, replace).await;
    let ms = started.elapsed().as_millis() as u64;
    match r {
        Ok(outcome) => {
            let stamp = outcome["graphStamp"].as_str().map(str::to_string);
            let mut detail = outcome;
            detail["durationMs"] = json!(ms);
            Ok((stamp, detail))
        }
        Err(e) => Err(e),
    }
}

/// Deadline for one isolated native run.
const NATIVE_RUN_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Run `native::rebuild` in a child process (`wrongindexd --native-rebuild`).
///
/// Parsers recurse on source structure and oxc has no recursion limit: a
/// hostile file can overflow the stack, which aborts a process and cannot be
/// caught. Isolation confines that to the child; the project is then treated
/// as unsupported (the compatibility worker can still take it in `auto`).
async fn rebuild_in_child(
    root: &str,
    index_dir: &str,
    replace: bool,
) -> Result<serde_json::Value, NativeOutcome> {
    let exe = std::env::current_exe()
        .map_err(|e| NativeOutcome::Failed(ErrorBody::new(ErrorCode::Internal, e.to_string())))?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg(if replace {
        "--native-rebuild-replace"
    } else {
        "--native-rebuild"
    })
    .arg(root)
    .arg(index_dir)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let child = cmd.spawn().map_err(|e| {
        NativeOutcome::Failed(ErrorBody::new(
            ErrorCode::Internal,
            format!("cannot start the native indexer: {e}"),
        ))
    })?;
    let out = match tokio::time::timeout(NATIVE_RUN_DEADLINE, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Err(NativeOutcome::Failed(ErrorBody::new(
                ErrorCode::Internal,
                format!("native indexer: {e}"),
            )));
        }
        Err(_) => {
            return Err(NativeOutcome::Failed(ErrorBody::new(
                ErrorCode::DeadlineExceeded,
                "native indexer exceeded its deadline",
            )));
        }
    };
    let reply: Option<serde_json::Value> = serde_json::from_slice(&out.stdout).ok();
    match reply {
        Some(v) if v["ok"] == true => Ok(v["result"].clone()),
        Some(v) => {
            let msg = v["message"]
                .as_str()
                .unwrap_or("native engine error")
                .to_string();
            Err(match v["kind"].as_str() {
                Some("unsupported") => NativeOutcome::Unsupported(msg),
                Some("busy") => NativeOutcome::Failed(ErrorBody::new(
                    ErrorCode::DatabaseBusy,
                    format!("native engine: {msg}"),
                )),
                _ => NativeOutcome::Failed(ErrorBody::new(
                    ErrorCode::Internal,
                    format!("native engine: {msg}"),
                )),
            })
        }
        // Crashed (e.g. stack overflow) or produced no reply: the staging
        // directory it may have left is private and is ignored by readers.
        None => {
            warn!("native_indexer_crashed", {"status": out.status.to_string()});
            Err(NativeOutcome::Unsupported(format!(
                "native indexer crashed ({}); a source file may be too deeply nested, or the run exceeded its memory cap",
                out.status
            )))
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, project: &str) -> JobRecord {
        JobRecord {
            job_id: id.into(),
            project_id: project.into(),
            kind: JobKind::Full,
            state: JobState::Queued,
            attempt: 0,
            admitted_at_ms: 0,
            started_at_ms: None,
            completed_at_ms: None,
            result: None,
            error: None,
            params: None,
        }
    }

    #[test]
    fn pick_is_one_per_project_in_admission_order() {
        let queued = || {
            vec![
                job("1", "a"),
                job("2", "a"),
                job("3", "b"),
                job("4", "c"),
                job("5", "b"),
            ]
        };
        let ids = |v: Vec<JobRecord>| v.into_iter().map(|j| j.job_id).collect::<Vec<_>>();
        let none = HashSet::new();
        assert_eq!(ids(pick(queued(), &none, 2)), ["1", "3"]);
        assert_eq!(ids(pick(queued(), &none, 5)), ["1", "3", "4"]);
        // `a` busy (its slow job runs): the others get the slots.
        let busy: HashSet<String> = ["a".to_string()].into();
        assert_eq!(ids(pick(queued(), &busy, 1)), ["3"]);
        assert_eq!(ids(pick(queued(), &busy, 2)), ["3", "4"]);
        assert!(pick(queued(), &none, 0).is_empty());
    }
}
