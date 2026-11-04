//! Durable job execution. Jobs are claimed from the registry in admission
//! order, up to the `parallelJobs` setting at once and one per project; completion is recorded before it becomes observable via `job.get`.
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
use wrongindex_registry::now_ms;

/// Fallback poll interval; admission normally wakes the worker immediately.
const IDLE_POLL: Duration = Duration::from_secs(5);

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
        let parallel = shared.settings.get().parallel_jobs.max(1);
        if running.len() < parallel {
            let queued = match shared.registry(|r| r.queued_jobs()).await {
                Ok(q) => q,
                Err(e) => {
                    warn!("worker_queue_read_failed", {"error": e.to_string()});
                    Vec::new()
                }
            };
            let in_flight = busy.lock().map(|b| b.clone()).unwrap_or_default();
            for job in pick(queued, &in_flight, parallel - running.len()) {
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
        // WAL care (see `wal`) in the slots jobs left, never beside a job
        // of the same project.
        if running.len() < parallel {
            let in_flight = busy.lock().map(|b| b.clone()).unwrap_or_default();
            for (pid, dir) in shared.wal.due(std::time::Instant::now()) {
                if running.len() >= parallel {
                    break;
                }
                if in_flight.contains(&pid) {
                    continue;
                }
                shared.wal.start(&pid);
                if let Ok(mut b) = busy.lock() {
                    b.insert(pid.clone());
                }
                let marker = Busy {
                    projects: busy.clone(),
                    project_id: pid.clone(),
                };
                running.spawn(checkpoint(shared.clone(), pid, dir, marker));
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
    if let Ok(mut p) = shared.progress.lock() {
        p.insert(
            job.project_id.clone(),
            serde_json::json!({"jobId": job_id, "phase": "starting", "atMs": now_ms()}),
        );
    }
    let started = std::time::Instant::now();
    let outcome = execute(&shared, &job.project_id, job.kind, job.params.clone()).await;
    shared.metrics.job_finished(
        job.kind.as_str(),
        if outcome.is_ok() {
            "succeeded"
        } else {
            "failed"
        },
        started.elapsed().as_millis() as u64,
    );
    if let Ok(mut p) = shared.progress.lock() {
        p.remove(&job.project_id);
    }
    let ok = outcome.is_ok();
    // SQLite said the index is damaged: nothing writes it from now on (see
    // `damage`), whichever job found out.
    if let Err(e) = &outcome
        && e.code == ErrorCode::DatabaseCorrupt
        && shared.watch.note_corrupt(&job.project_id, e)
    {
        warn!("index_corrupt", {"projectId": job.project_id, "jobId": job_id, "error": e.message});
    }
    let id = job_id.clone();
    match shared.registry(move |r| r.complete_job(&id, outcome)).await {
        Ok(done) => {
            prune_now_and_then(&shared).await;
            info!("job_completed", {
                "jobId": done.job_id, "projectId": done.project_id,
                "kind": done.kind.as_str(), "state": done.state.as_str(), "ok": ok,
            });
            crate::events::job_event(&shared, "job.finished", &done);
            // A periodic reconciliation that changed the index found what
            // no watcher event had reported (see `schedule`).
            if ok
                && crate::schedule::is_periodic(&done)
                && let Some(n) = done.result.as_ref().and_then(files_changed)
                && n > 0
            {
                shared.metrics.missed("periodic", n);
                warn!("reconcile_found_changes", {
                    "projectId": done.project_id, "jobId": done.job_id, "files": n,
                    "generation": done.result.as_ref().map(|r| r["generation"].clone()),
                });
            }
            // An explicit write job that succeeded closes a watcher circuit
            // (WI-208); the watcher's own batches settle in the supervisor.
            if ok && done.kind != JobKind::Check {
                shared
                    .watch
                    .note_job_succeeded(&done.project_id, &done.job_id);
            }
        }
        Err(e) => {
            warn!("job_completion_not_recorded", {"jobId": job_id, "error": e.to_string()})
        }
    }
    // Another project's queued job may be waiting for this slot.
    shared.wake_worker.notify_one();
}

/// A truncating WAL checkpoint of an owned index (see `wal`), holding the
/// project's job slot and the ownership guard.
async fn checkpoint(shared: Arc<Shared>, project_id: String, dir: std::path::PathBuf, _busy: Busy) {
    let outcome = match shared.owners.begin_run(&project_id) {
        None => Err("the index is no longer owned by this service".to_string()),
        Some(_run) => tokio::task::spawn_blocking(move || {
            wrongindex_storage::wal::truncate(&dir).map_err(|e| e.to_string())
        })
        .await
        .unwrap_or_else(|e| Err(e.to_string())),
    };
    if let Some(change) = shared
        .wal
        .finish(&project_id, &outcome, std::time::Instant::now())
    {
        if change["level"] == "ok" {
            info!("wal_ok", {"projectId": project_id});
        }
        shared
            .events
            .publish("resource.warning", Some(&project_id), change);
    }
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
            // The profile, and SQLite's own check of every page and of the
            // full-text index (read-only).
            let db = Path::new(&project.index_dir).join("index.db");
            let inspection =
                tokio::task::spawn_blocking(move || wrongindex_storage::inspect_index_checked(&db))
                    .await
                    .map_err(|e| {
                        ErrorBody::new(ErrorCode::Internal, format!("inspection task failed: {e}"))
                    })?;
            if inspection.verdict == wrongindex_storage::Verdict::Corrupt {
                let what = inspection
                    .integrity
                    .as_ref()
                    .map(|i| i.summary())
                    .filter(|p| !p.is_empty())
                    .unwrap_or_else(|| "a read failed".into());
                let e =
                    crate::damage::corrupt(&format!("the index failed SQLite's check ({what})"));
                if shared.watch.note_corrupt(project_id, &e) {
                    warn!("index_corrupt", {"projectId": project_id, "error": e.message});
                }
            } else if inspection.integrity.as_ref().is_some_and(|i| i.ok) {
                shared.watch.clear_corrupt(project_id, "check");
            }
            let sound = matches!(
                inspection.verdict,
                wrongindex_storage::Verdict::Compatible
                    | wrongindex_storage::Verdict::CompatibleWithUnknownData
            ) && inspection.integrity.as_ref().is_none_or(|i| i.ok);
            let mut out = serde_json::to_value(inspection)
                .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
            if sound {
                out["contents"] = check_contents(shared, &project).await;
            }
            Ok(out)
        }
        JobKind::Restore => {
            let params = params.unwrap_or_default();
            let fence: OwnerFence =
                serde_json::from_value(params["fence"].clone()).map_err(|_| {
                    ErrorBody::new(ErrorCode::OwnerConflict, "job carries no ownership fence")
                })?;
            let name = params["backup"].as_str().unwrap_or_default().to_string();
            let pid = project_id.to_string();
            shared
                .registry(move |r| r.validate_fence(&pid, &fence))
                .await?;
            let _run = shared.owners.begin_run(project_id).ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::OwnerConflict,
                    "ownership guard not held by this instance",
                )
            })?;
            ensure_room(&project.index_dir, kind).await?;
            // The compatibility worker pools open stores; they must close
            // before the file is replaced.
            if shared.engine.configured() {
                let _ = shared
                    .engine
                    .call(
                        "release",
                        serde_json::json!({
                            "projectRoot": project.checkout_root,
                            "indexDir": project.index_dir,
                        }),
                        Duration::from_secs(30),
                    )
                    .await;
            }
            let dir = std::path::PathBuf::from(&project.index_dir);
            let epoch = fence.owner_epoch;
            let restored = tokio::task::spawn_blocking(move || {
                let (from, previous) = wrongindex_storage::backup::restore(&dir, &name, epoch)?;
                let stamp = wrongindex_storage::backup::graph_stamp(&dir);
                Ok::<_, wrongindex_storage::backup::BackupError>((from, previous, stamp))
            })
            .await
            .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
            let (from, previous, stamp) = restored.map_err(|e| {
                use wrongindex_storage::backup::BackupError;
                let code = match &e {
                    BackupError::Busy(_) => ErrorCode::DatabaseBusy,
                    BackupError::NotFound(_) | BackupError::Invalid(..) => {
                        ErrorCode::InvalidRequest
                    }
                    _ => ErrorCode::Internal,
                };
                ErrorBody::new(code, e.to_string())
            })?;
            info!("index_restored", {"projectId": project_id, "from": from.display().to_string()});
            // A backup is older than the checkout: a watched project catches
            // up before it is called current again.
            shared.watch.mark_reconcile(project_id, "restored");
            let generation = publish(shared, project_id, fence, stamp.clone()).await?;
            Ok(serde_json::json!({
                "restoredFrom": from.display().to_string(),
                "previousKeptAs": previous.map(|p| p.display().to_string()),
                "graphStamp": stamp,
                "generation": generation,
            }))
        }
        JobKind::Full | JobKind::Incremental | JobKind::Reconcile | JobKind::Rebuild => {
            // A missing root is not an empty project: nothing is indexed and
            // nothing deleted until it is back.
            let root = project.checkout_root.clone();
            if !tokio::task::spawn_blocking(move || Path::new(&root).is_dir())
                .await
                .unwrap_or(false)
            {
                let msg = format!(
                    "the checkout root {} is unavailable; nothing was indexed or deleted",
                    project.checkout_root
                );
                record_failure(shared, project_id, &msg).await;
                return Err(ErrorBody::new(ErrorCode::ProjectUnavailable, msg));
            }
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
            // A damaged index is written by nothing but a maintenance
            // rebuild (see `damage`). Looked at again first: the file may
            // have been replaced since.
            if kind != JobKind::Rebuild
                && let Some(said) = shared.watch.corrupt(project_id)
            {
                let dir = std::path::PathBuf::from(&project.index_dir);
                let again = tokio::task::spawn_blocking(move || crate::damage::still_damaged(&dir))
                    .await
                    .ok()
                    .flatten();
                match again {
                    Some((false, _)) => {
                        shared
                            .watch
                            .clear_corrupt(project_id, "a write job found the index sound");
                    }
                    other => {
                        let why = other
                            .map(|(_, why)| why)
                            .filter(|w| !w.is_empty())
                            .unwrap_or(said);
                        let e = crate::damage::corrupt(&format!(
                            "the index is damaged ({why}); nothing was written"
                        ));
                        record_failure(shared, project_id, &e.message).await;
                        return Err(e);
                    }
                }
            }
            // Not enough room for this run: refuse before anything is
            // written (a watched project waits for space, see `disk`).
            if let Err(e) = ensure_room(&project.index_dir, kind).await {
                record_failure(shared, project_id, &e.message).await;
                return Err(e);
            }

            let replace = kind == JobKind::Rebuild;
            let backend = match shared.backend_for(project.backend_preference) {
                Ok(b) => b,
                Err(e) => {
                    record_failure(shared, project_id, &e.message).await;
                    return Err(e);
                }
            };
            let mut native_skipped: Option<String> = None;
            // A watcher/edit job names its paths: WrongStack's targeted run
            // (`runIndexer({ files })`), not a whole-project pass.
            let targets: Option<Vec<String>> = (kind == JobKind::Incremental)
                .then(|| serde_json::from_value::<Vec<String>>(params["paths"].clone()).ok())
                .flatten()
                .filter(|p| !p.is_empty());
            if backend != Backend::Legacy {
                match run_native(shared, &project, replace, targets.as_deref()).await {
                    Ok((stamp, detail)) => {
                        // The index is now a baseline index the native writer
                        // wrote or accepted.
                        shared.owners.clear_maintenance(project_id);
                        let generation = publish(shared, project_id, fence, stamp.clone()).await?;
                        note_backend(shared, project_id, wrongindex_contracts::Backend::Native)
                            .await;
                        return Ok(json!({
                            "backend": "native",
                            "native": detail,
                            "graphStamp": stamp,
                            "generation": generation,
                        }));
                    }
                    Err(NativeOutcome::Unsupported(why))
                        if !replace && backend == Backend::Auto && shared.engine.configured() =>
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
            if replace && backend == Backend::Legacy {
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
                    note_backend(shared, project_id, wrongindex_contracts::Backend::Legacy).await;
                    if let Some(why) = native_skipped {
                        out["nativeSkipped"] = json!(why);
                    }
                    Ok(out)
                }
                Err(e) => {
                    // The compatibility worker reports a full disk and a
                    // damaged index in text.
                    let e = crate::damage::classify(crate::disk::classify(e));
                    record_failure(shared, project_id, &e.to_string()).await;
                    Err(e)
                }
            }
        }
    }
}

/// `DISK_FULL` when the index's volume lacks the room a `kind` run needs.
async fn ensure_room(index_dir: &str, kind: JobKind) -> Result<(), ErrorBody> {
    let dir = std::path::PathBuf::from(index_dir);
    tokio::task::spawn_blocking(move || {
        crate::disk::ensure_room(&dir, crate::disk::needed(kind, &dir))
    })
    .await
    .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?
}

/// Remember the engine that last wrote this project's index (shown in
/// project status). Best effort: a failure here never fails the job.
async fn note_backend(
    shared: &Arc<Shared>,
    project_id: &str,
    backend: wrongindex_contracts::Backend,
) {
    let pid = project_id.to_string();
    if let Err(e) = shared.registry(move |r| r.set_backend(&pid, backend)).await {
        warn!("backend_not_recorded", {"projectId": project_id, "error": e.to_string()});
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
    targets: Option<&[String]>,
) -> Result<(Option<String>, serde_json::Value), NativeOutcome> {
    // The compatibility worker may hold a pooled handle on this index; close
    // it so the native swap is not refused for our own reader.
    shared
        .engine
        .release_store(&project.checkout_root, &project.index_dir)
        .await;
    let started = std::time::Instant::now();
    let memory_mb = shared.settings.get().native_memory_mb;
    let (s2, pid) = (shared.clone(), project.project_id.clone());
    let on_progress = move |mut step: serde_json::Value| {
        let job_id = s2
            .progress
            .lock()
            .ok()
            .and_then(|p| p.get(&pid).map(|v| v["jobId"].clone()))
            .unwrap_or_default();
        step["jobId"] = job_id;
        step["atMs"] = json!(now_ms());
        if step["done"] == true
            && let (Some(phase), Some(ms)) = (step["phase"].as_str(), step["ms"].as_u64())
        {
            s2.metrics.phase(phase, ms);
        }
        if let Ok(mut p) = s2.progress.lock() {
            p.insert(pid.clone(), step.clone());
        }
        s2.events.publish("job.progress", Some(&pid), step);
    };
    let r = rebuild_in_child(
        &project.checkout_root,
        &project.index_dir,
        replace,
        targets,
        memory_mb,
        on_progress,
    )
    .await;
    let ms = started.elapsed().as_millis() as u64;
    match r {
        Ok(outcome) => {
            let stamp = outcome["graphStamp"].as_str().map(str::to_string);
            let mut detail = outcome;
            detail["durationMs"] = json!(ms);
            if let Some(t) = targets {
                detail["targets"] = json!(t.len());
            }
            Ok((stamp, detail))
        }
        Err(e) => Err(e),
    }
}

/// Deadline for one isolated native run.
const NATIVE_RUN_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Run `native::rebuild` in a child process (`wrongindexd --native-rebuild`),
/// or `native::update_files` for a path list (`--native-update`, the list
/// as a JSON array on the child's stdin).
///
/// Parsers recurse on source structure and oxc has no recursion limit: a
/// hostile file can overflow the stack, which aborts a process and cannot be
/// caught. Isolation confines that to the child; the project is then treated
/// as unsupported (the compatibility worker can still take it in `auto`).
async fn rebuild_in_child(
    root: &str,
    index_dir: &str,
    replace: bool,
    targets: Option<&[String]>,
    memory_mb: u64,
    on_progress: impl Fn(serde_json::Value) + Send + 'static,
) -> Result<serde_json::Value, NativeOutcome> {
    let exe = std::env::current_exe()
        .map_err(|e| NativeOutcome::Failed(ErrorBody::new(ErrorCode::Internal, e.to_string())))?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg(if replace {
        "--native-rebuild-replace"
    } else if targets.is_some() {
        "--native-update"
    } else {
        "--native-rebuild"
    })
    .arg(root)
    .arg(index_dir)
    .stdin(if targets.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    })
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .env("WRONGINDEX_NATIVE_PROGRESS", "1")
    .kill_on_drop(true);
    if memory_mb > 0 {
        cmd.env("WRONGINDEX_NATIVE_MEMORY_MB", memory_mb.to_string());
    }
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = cmd.spawn().map_err(|e| {
        NativeOutcome::Failed(ErrorBody::new(
            ErrorCode::Internal,
            format!("cannot start the native indexer: {e}"),
        ))
    })?;
    if let (Some(t), Some(mut stdin)) = (targets, child.stdin.take()) {
        use tokio::io::AsyncWriteExt;
        let list = serde_json::to_vec(t).unwrap_or_default();
        // A child that died early closes the pipe: its reply says why.
        let _ = stdin.write_all(&list).await;
        drop(stdin);
    }
    // Progress lines on stderr; anything else there is drained and dropped
    // (the pipe must never fill and stall the child).
    let progress_reader = child.stderr.take().map(|stderr| {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.len() > 4096 || !line.starts_with("{\"wiProgress\"") {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                    && v["wiProgress"].is_object()
                {
                    on_progress(v["wiProgress"].clone());
                }
            }
        })
    });
    let out = match tokio::time::timeout(NATIVE_RUN_DEADLINE, child.wait_with_output()).await {
        Ok(Ok(o)) => {
            // Every progress line is handled before the run is reported
            // finished (the job's progress entry is cleared after that).
            if let Some(reader) = progress_reader {
                let _ = tokio::time::timeout(Duration::from_secs(5), reader).await;
            }
            o
        }
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
                Some("diskFull") => NativeOutcome::Failed(ErrorBody::new(
                    ErrorCode::DiskFull,
                    format!("native engine: {msg}; the write was rolled back"),
                )),
                // Never "unsupported": that would hand the damaged file to
                // the compatibility worker.
                Some("corrupt") => {
                    NativeOutcome::Failed(crate::damage::corrupt(&format!("native engine: {msg}")))
                }
                _ => NativeOutcome::Failed(crate::disk::classify(ErrorBody::new(
                    ErrorCode::Internal,
                    format!("native engine: {msg}"),
                ))),
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

/// The deeper verification (see `wrongindex_storage::contents`): every
/// indexed file read and compared with what was indexed. A watched project
/// gets the files that differ indexed (WrongStack's targeted run, as if
/// their events had come); otherwise the result only says what differs.
async fn check_contents(
    shared: &Arc<Shared>,
    project: &wrongindex_contracts::ProjectRecord,
) -> serde_json::Value {
    let db = Path::new(&project.index_dir).join("index.db");
    let root = std::path::PathBuf::from(&project.checkout_root);
    let verified = tokio::task::spawn_blocking(move || {
        wrongindex_storage::contents::verify(
            &db,
            &root,
            wrongindex_engine::native::MAX_INDEX_FILE_BYTES,
        )
    })
    .await;
    let contents = match verified {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return json!({"error": e.to_string()}),
        Err(e) => return json!({"error": e.to_string()}),
    };
    let mut out = serde_json::to_value(contents.summary()).unwrap_or_default();
    if contents.agrees() {
        return out;
    }
    let pid = &project.project_id;
    // Nothing pending before: no event had reported these.
    let unreported = shared.watch.settled(pid);
    let queued = shared.watch.mark_paths(pid, contents.differing());
    out["queued"] = json!(queued);
    let n = (contents.changed.len() + contents.missing.len()) as u64;
    if unreported {
        shared.metrics.missed("check", n);
    }
    warn!("content_drift", {
        "projectId": pid, "changed": contents.changed.len(),
        "missing": contents.missing.len(), "queued": queued,
    });
    out
}

/// Files a whole-project run changed (rows added, rewritten or deleted);
/// `None` when its result does not say (a fresh build writes every file).
fn files_changed(result: &serde_json::Value) -> Option<u64> {
    let native = &result["native"];
    match native["outcome"].as_str() {
        Some("unchanged") => Some(0),
        Some("updated") => native["changedFiles"].as_u64(),
        Some(_) => None,
        // The compatibility worker: WrongStack's own count of parsed files.
        None => result["filesIndexed"].as_u64(),
    }
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
