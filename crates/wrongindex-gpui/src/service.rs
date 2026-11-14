//! Client actor: every service call, file dialog result and slow local job
//! runs here, on its own Tokio runtime. GPUI never performs blocking IPC or
//! SQLite work; it reads the shared [`Snapshot`] and sends [`Command`]s.

use crate::logic::{self, ProjectAction};
use crate::model::{ACTIVITY_CAP, Command, Detail, Machine, Outcome, Preview, Search, Snapshot};
use crate::system;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wrongindex_contracts::{ErrorCode, RegisterProjectParams, method};
use wrongindex_ipc::{CallError, Client, DataRoot};

pub const CLIENT_NAME: &str = "wrongindex-gpui";

/// The latest snapshot and a version that changes with every update.
#[derive(Default)]
pub struct Shared {
    state: Mutex<Snapshot>,
    version: AtomicU64,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut Snapshot)) {
        f(&mut self.state.lock().unwrap_or_else(|p| p.into_inner()));
        self.version.fetch_add(1, Ordering::Release);
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    pub fn read(&self) -> Snapshot {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

pub struct Service {
    commands: tokio::sync::mpsc::Sender<(u64, Command)>,
    pub shared: Arc<Shared>,
    pub outcomes: std::sync::mpsc::Receiver<Outcome>,
}

impl Service {
    /// Queue a command; `false` when the queue is full or the actor is gone.
    pub fn send(&self, sequence: u64, command: Command) -> bool {
        self.commands.try_send((sequence, command)).is_ok()
    }
}

struct Ctx {
    root: DataRoot,
    daemon: PathBuf,
    shared: Arc<Shared>,
    selected: Mutex<Option<String>>,
    /// Refreshes run one at a time, so an older reading never overwrites a
    /// newer one.
    refresh_lock: tokio::sync::Mutex<()>,
    kick: tokio::sync::Notify,
    updater: system::Updater,
    outcomes: Mutex<std::sync::mpsc::Sender<Outcome>>,
}

/// A failed request: the service's stable code, when it gave one, and a
/// message for the window.
struct Fail {
    code: Option<ErrorCode>,
    message: String,
}

impl From<CallError> for Fail {
    fn from(e: CallError) -> Self {
        let message = match &e {
            CallError::Remote(body) => {
                let code = serde_json::to_value(body.code)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| "INTERNAL".into());
                format!("{code}: {}", body.message)
            }
            CallError::Unavailable(_) => {
                "The background service is unavailable. Start it to connect to your projects."
                    .into()
            }
            other => other.to_string(),
        };
        Fail {
            code: e.code(),
            message,
        }
    }
}

impl From<String> for Fail {
    fn from(message: String) -> Self {
        Fail {
            code: None,
            message,
        }
    }
}

impl From<&str> for Fail {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

type Done = Result<Option<String>, Fail>;

pub fn spawn(root: DataRoot, daemon: PathBuf) -> Service {
    let (commands, requests) = tokio::sync::mpsc::channel(64);
    let (outcomes, received) = std::sync::mpsc::channel();
    let shared = Arc::new(Shared::default());
    let ctx = Ctx {
        updater: system::Updater::new(root.path()),
        root,
        daemon,
        shared: shared.clone(),
        selected: Mutex::new(None),
        refresh_lock: tokio::sync::Mutex::new(()),
        kick: tokio::sync::Notify::new(),
        outcomes: Mutex::new(outcomes),
    };
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(4)
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(run(Arc::new(ctx), requests));
        // Do not wait for a machine scan or a CLI child the window no longer needs.
        runtime.shutdown_background();
    });
    Service {
        commands,
        shared,
        outcomes: received,
    }
}

async fn run(ctx: Arc<Ctx>, mut requests: tokio::sync::mpsc::Receiver<(u64, Command)>) {
    tokio::spawn(refresher(ctx.clone()));
    tokio::spawn(pump_events(ctx.clone()));
    while let Some((sequence, command)) = requests.recv().await {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let mut select = None;
            let result = handle(&ctx, command, &mut select).await;
            // The outcome is reported with the state it produced.
            refresh(&ctx).await;
            let outcome = match result {
                Ok(message) => Outcome {
                    sequence,
                    ok: true,
                    message,
                    select,
                },
                Err(fail) => Outcome {
                    sequence,
                    ok: false,
                    message: Some(fail.message),
                    select,
                },
            };
            let _ = ctx
                .outcomes
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .send(outcome);
        });
    }
}

async fn refresher(ctx: Arc<Ctx>) {
    loop {
        refresh(&ctx).await;
        tokio::select! {
            _ = ctx.kick.notified() => tokio::time::sleep(Duration::from_millis(200)).await,
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}

/// Follow the service's event stream into Activity. The stream never is the
/// source of truth: each event only asks for a fresh reading.
async fn pump_events(ctx: Arc<Ctx>) {
    let mut position: Option<(String, u64)> = None;
    loop {
        if let Ok(client) = Client::connect(&ctx.root, CLIENT_NAME).await {
            let (since_instance, since_seq) = match &position {
                Some((i, s)) => (Some(i.clone()), Some(*s)),
                None => (None, None),
            };
            if let Ok((ack, mut stream)) = client
                .subscribe(since_seq, since_instance.as_deref(), None)
                .await
            {
                let instance = ack["instanceId"].as_str().unwrap_or_default().to_string();
                position = Some((instance, ack["currentSeq"].as_u64().unwrap_or(0)));
                ctx.kick.notify_one();
                while let Ok(Some(event)) = stream.next().await {
                    if event["kind"] != "stream.gap" {
                        if let (Some(p), Some(seq)) = (position.as_mut(), event["seq"].as_u64()) {
                            p.1 = seq;
                        }
                        ctx.shared.update(|s| {
                            s.activity.push_front(event);
                            s.activity.truncate(ACTIVITY_CAP);
                        });
                    }
                    ctx.kick.notify_one();
                }
            }
        }
        ctx.kick.notify_one();
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn connect(root: &DataRoot) -> Result<Client, Fail> {
    tokio::time::timeout(Duration::from_secs(5), Client::connect(root, CLIENT_NAME))
        .await
        .map_err(|_| Fail::from("Connection timed out. The service may be stopped."))?
        .map_err(Fail::from)
}

async fn call(
    client: &mut Client,
    method: &str,
    project: Option<&str>,
    params: Value,
) -> Result<Value, Fail> {
    tokio::time::timeout(
        Duration::from_secs(10),
        client.call_raw(method, project, params, None),
    )
    .await
    .map_err(|_| Fail::from("Service request timed out; check its actual state before retrying."))?
    .map_err(Fail::from)
}

/// Start the service installed next to this program, or report one that
/// already answers; bounded to 30 seconds. A draining service is left to
/// finish its job before a successor starts.
async fn start(root: &DataRoot, daemon: &Path) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut child: Option<wrongindex_ipc::launch::Detached> = None;
        let mut last_spawn = tokio::time::Instant::now();
        loop {
            match Client::connect(root, CLIENT_NAME).await {
                Ok(_) => return Ok(()),
                Err(error) if error.code() == Some(ErrorCode::ShuttingDown) => {
                    // The old instance retains the endpoint until its jobs
                    // finish. Do not mistake it for a ready service or spawn
                    // a replacement while it is still draining.
                }
                Err(CallError::Unavailable(_)) => {
                    let should_spawn = match child.as_mut() {
                        None => true,
                        Some(child) => match child.try_wait().map_err(|e| e.to_string())? {
                            None => false,
                            Some(3) => last_spawn.elapsed() >= Duration::from_millis(500),
                            Some(code) => {
                                return Err(match root.take_start_error() {
                                    Some(reason) => {
                                        format!("The service could not start: {reason}")
                                    }
                                    None => format!(
                                        "Background service exited with code {code}; check its logs."
                                    ),
                                });
                            }
                        },
                    };
                    if should_spawn {
                        child = Some(
                            wrongindex_ipc::launch::spawn_detached(daemon, root)
                                .map_err(|e| e.to_string())?,
                        );
                        last_spawn = tokio::time::Instant::now();
                    }
                }
                Err(error) => return Err(error.to_string()),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| {
        "Service did not become ready within 30 seconds. It may still be shutting down.".to_string()
    })?
}

/// Sign-in start: start the service (or find it running) and return,
/// without a window.
pub fn start_service_now(root: &DataRoot, daemon: &Path) -> Result<(), String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(start(root, daemon))
}

struct Reading {
    health: Value,
    events: Value,
    metrics: Value,
    settings: Value,
    jobs: Vec<Value>,
    detail: Option<Detail>,
    /// The selected project no longer exists.
    lost_selection: bool,
}

async fn read_service(root: &DataRoot, selected: Option<&str>) -> Result<Reading, Fail> {
    let mut c = connect(root).await?;
    let health = call(&mut c, method::DAEMON_HEALTH, None, json!({})).await?;
    let events = call(&mut c, method::EVENTS_SNAPSHOT, None, json!({})).await?;
    let metrics = call(&mut c, method::DAEMON_METRICS, None, json!({})).await?;
    let settings = call(&mut c, method::SETTINGS_GET, None, json!({})).await?;
    let jobs = call(&mut c, method::JOB_LIST, None, json!({"limit": 50})).await?;
    let mut reading = Reading {
        health,
        events,
        metrics,
        settings,
        jobs: jobs["jobs"].as_array().cloned().unwrap_or_default(),
        detail: None,
        lost_selection: false,
    };
    let Some(id) = selected else {
        return Ok(reading);
    };
    let status = match call(&mut c, method::PROJECT_GET, Some(id), json!({})).await {
        Ok(status) => status,
        Err(fail) if fail.code == Some(ErrorCode::ProjectNotFound) => {
            reading.lost_selection = true;
            return Ok(reading);
        }
        Err(fail) => return Err(fail),
    };
    let jobs = call(
        &mut c,
        method::JOB_LIST,
        None,
        json!({"projectId": id, "limit": 25}),
    )
    .await?;
    let ownership = call(&mut c, method::OWNERSHIP_STATUS, Some(id), json!({})).await?;
    let backups = call(&mut c, method::INDEX_BACKUPS, Some(id), json!({}))
        .await
        .map(|b| b["backups"].as_array().cloned().unwrap_or_default())
        .unwrap_or_default();
    // Statistics only for the published generation of an intact index; an
    // unindexed project legitimately has none.
    let stats = call(
        &mut c,
        method::INDEX_QUERY,
        Some(id),
        json!({"op": "stats", "args": {}}),
    )
    .await
    .ok()
    .filter(|r| {
        r["indexCorrupt"].is_null()
            && r["publicationPending"] != true
            && r["generation"] == status["generation"]
    })
    .map(|r| r["result"].clone());
    reading.detail = Some(Detail {
        project_id: id.to_string(),
        status,
        jobs: jobs["jobs"].as_array().cloned().unwrap_or_default(),
        ownership,
        backups,
        stats,
    });
    Ok(reading)
}

async fn refresh(ctx: &Ctx) {
    let _one = ctx.refresh_lock.lock().await;
    let selected = ctx
        .selected
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let result = read_service(&ctx.root, selected.as_deref()).await;
    if let Ok(r) = &result
        && r.lost_selection
    {
        let mut s = ctx.selected.lock().unwrap_or_else(|p| p.into_inner());
        if *s == selected {
            *s = None;
        }
    }
    ctx.shared.update(|s| match result {
        Ok(r) => {
            s.connected = true;
            s.reason = None;
            s.health = r.health;
            s.events = r.events;
            s.metrics = r.metrics;
            s.settings = r.settings;
            s.jobs = r.jobs;
            s.detail = r.detail;
        }
        Err(fail) => {
            // No live value is shown while disconnected.
            s.connected = false;
            s.reason = Some(fail.message);
            s.health = Value::Null;
            s.events = Value::Null;
            s.metrics = Value::Null;
            s.settings = Value::Null;
            s.jobs.clear();
            s.detail = None;
        }
    });
}

fn blocking_err(e: tokio::task::JoinError) -> Fail {
    Fail::from(format!("background task failed: {e}"))
}

async fn handle(ctx: &Ctx, command: Command, select: &mut Option<String>) -> Done {
    match command {
        Command::Start => {
            start(&ctx.root, &ctx.daemon).await?;
            Ok(Some("Background service is ready.".into()))
        }
        Command::Stop => {
            let mut c = connect(&ctx.root).await?;
            call(&mut c, method::DAEMON_SHUTDOWN, None, json!({})).await?;
            Ok(Some(
                "Index service is stopping. Registrations and indexes are kept.".into(),
            ))
        }
        Command::Select(id) => {
            *ctx.selected.lock().unwrap_or_else(|p| p.into_inner()) = id;
            Ok(None)
        }
        Command::Preview(path) => {
            let preview = tokio::task::spawn_blocking(move || preview(path))
                .await
                .map_err(blocking_err)??;
            ctx.shared.update(|s| s.preview = Some(preview));
            Ok(None)
        }
        Command::DismissPreview => {
            ctx.shared.update(|s| s.preview = None);
            Ok(None)
        }
        Command::Register {
            root,
            index_dir,
            policy,
        } => {
            let params = RegisterProjectParams {
                checkout_root: root,
                index_dir,
                display_name: None,
                watch_policy: policy,
                repository_identity: None,
            };
            let mut c = connect(&ctx.root).await?;
            let reply = call(
                &mut c,
                method::PROJECT_REGISTER,
                None,
                serde_json::to_value(params).unwrap_or_default(),
            )
            .await?;
            ctx.shared.update(|s| s.preview = None);
            if let Some(id) = reply["project"]["projectId"].as_str() {
                *ctx.selected.lock().unwrap_or_else(|p| p.into_inner()) = Some(id.to_string());
                *select = Some(id.to_string());
            }
            Ok(Some(if reply["created"] == true {
                "Project registered. Claim ownership to index it; work is shown as jobs.".into()
            } else {
                "Project was already registered.".into()
            }))
        }
        Command::Action { project, action } => {
            action.validate()?;
            let (m, mut params) = action.request();
            let mut c = connect(&ctx.root).await?;
            match &action {
                ProjectAction::Retry(job_id) => {
                    let job = call(&mut c, method::JOB_GET, None, json!({"jobId": job_id})).await?;
                    params = logic::retry_params(&project, &job)?;
                }
                // Idempotent when this service already owns the index.
                ProjectAction::Rebuild => {
                    call(
                        &mut c,
                        method::OWNERSHIP_ACQUIRE,
                        Some(&project),
                        json!({"maintenance": true}),
                    )
                    .await?;
                }
                ProjectAction::IndexNow => {
                    call(&mut c, method::OWNERSHIP_ACQUIRE, Some(&project), json!({})).await?;
                }
                _ => {}
            }
            let reply = call(&mut c, m, Some(&project), params).await?;
            if action == ProjectAction::Remove {
                let mut s = ctx.selected.lock().unwrap_or_else(|p| p.into_inner());
                if s.as_deref() == Some(project.as_str()) {
                    *s = None;
                }
            }
            let mut message = action.done_message();
            if let Some(job) = reply["job"]["jobId"].as_str()
                && !matches!(action, ProjectAction::Retry(_))
            {
                message.push_str(&format!(" Job {job}."));
            }
            Ok(Some(message))
        }
        Command::CancelJob(job_id) => {
            let mut c = connect(&ctx.root).await?;
            let r = call(&mut c, method::JOB_CANCEL, None, json!({"jobId": job_id})).await?;
            if r["canceled"] == true {
                Ok(Some("Job canceled.".into()))
            } else {
                Err(format!(
                    "Not canceled: {}",
                    r["reason"]
                        .as_str()
                        .unwrap_or("the job is no longer queued")
                )
                .into())
            }
        }
        Command::OpenFolder(project) => {
            let mut c = connect(&ctx.root).await?;
            let v = call(&mut c, method::PROJECT_GET, Some(&project), json!({})).await?;
            let root = v["project"]["checkoutRoot"]
                .as_str()
                .ok_or("project has no checkout root")?;
            system::open_folder(root)?;
            Ok(None)
        }
        Command::ApplySettings { patch, revision } => {
            if !patch.is_object() {
                return Err("settings must be an object".into());
            }
            let paused = patch
                .as_object()
                .filter(|o| o.len() == 1)
                .and_then(|o| o.get("paused"))
                .and_then(Value::as_bool);
            let mut c = connect(&ctx.root).await?;
            // Only the service's reply is shown: a refused change keeps the
            // applied values on screen.
            let applied = call(
                &mut c,
                method::SETTINGS_SET,
                None,
                json!({"settings": patch, "expectedRevision": revision}),
            )
            .await?;
            ctx.shared.update(|s| s.settings = applied);
            Ok(Some(match paused {
                Some(true) => "Background indexing paused.".into(),
                Some(false) => "Background indexing resumed.".into(),
                None => "Settings applied.".into(),
            }))
        }
        Command::ScanMachine => {
            let home = wrongindex_storage::wstack_global_root()
                .ok_or("cannot determine the WrongStack home")?;
            let scan =
                tokio::task::spawn_blocking(move || wrongindex_storage::machine::scan(&home))
                    .await
                    .map_err(blocking_err)?;
            let mut v = serde_json::to_value(&scan).map_err(|e| e.to_string())?;
            // Works with the service stopped: most of these indexes are
            // written by WrongStack.
            let projects = match connect(&ctx.root).await {
                Ok(mut c) => call(&mut c, method::EVENTS_SNAPSHOT, None, json!({}))
                    .await
                    .ok(),
                Err(_) => None,
            };
            v["serviceConnected"] = json!(projects.is_some());
            if let Some(s) = projects {
                logic::mark_registered(&mut v, s["projects"].as_array().map_or(&[][..], |a| a));
            }
            ctx.shared.update(|s| {
                s.machine = Machine {
                    scan: Some(Arc::new(v)),
                    error: None,
                    at_ms: logic::now_ms(),
                }
            });
            Ok(None)
        }
        Command::Search { project, query } => {
            let q = logic::search_term(&query)?.to_string();
            let mut c = connect(&ctx.root).await?;
            let result = call(
                &mut c,
                method::INDEX_QUERY,
                Some(&project),
                json!({"op": "search", "args": {"query": q, "limit": 50}}),
            )
            .await?;
            ctx.shared.update(|s| {
                s.search = Some(Search {
                    project_id: project,
                    query: q,
                    result,
                })
            });
            Ok(None)
        }
        Command::DiagnosticsPreview { redact } => {
            let v = system::diagnostics(ctx.root.path(), redact, None).await?;
            ctx.shared.update(|s| s.diagnostics = Some(v));
            Ok(None)
        }
        Command::ExportDiagnostics { redact, path } => {
            system::diagnostics(ctx.root.path(), redact, Some(&path)).await?;
            Ok(Some(format!("Diagnostics saved to {}", path.display())))
        }
        Command::UpdateCheck { quiet } => {
            let r = system::update_check().await;
            let failed = r.as_ref().err().cloned();
            ctx.shared.update(|s| s.update_check = Some(r));
            match failed {
                Some(e) if !quiet => Err(format!("The update check failed: {e}").into()),
                _ => Ok(None),
            }
        }
        Command::UpdateStart => {
            ctx.updater.start()?;
            ctx.shared.update(|s| s.update_last = ctx.updater.last());
            Ok(Some(
                "Updating… This window closes while the new version is installed and opens again afterwards."
                    .into(),
            ))
        }
        Command::UpdateLast => {
            let last = ctx.updater.last();
            ctx.shared.update(|s| s.update_last = last);
            Ok(None)
        }
        Command::AutostartRead => {
            let r = system::autostart_status();
            ctx.shared.update(|s| s.autostart = Some(r));
            Ok(None)
        }
        Command::Autostart(enabled) => {
            let r = system::set_autostart(enabled);
            let failed = r.as_ref().err().cloned();
            ctx.shared.update(|s| s.autostart = Some(r));
            match failed {
                Some(e) => Err(e.into()),
                None if enabled => Ok(Some(
                    "The index service will start when you sign in.".into(),
                )),
                None => Ok(Some(
                    "The index service will no longer start at sign-in.".into(),
                )),
            }
        }
    }
}

/// Local, read-only preview of what registering `path` would do: the
/// resolved checkout, WrongStack's index location, and the inspector's
/// verdict for any index already there. Nothing is written.
fn preview(path: PathBuf) -> Result<Preview, Fail> {
    if !path.is_absolute() || !path.is_dir() {
        return Err("Enter the absolute path of an existing folder.".into());
    }
    let global =
        wrongindex_storage::wstack_global_root().ok_or("cannot determine the WrongStack home")?;
    let resolved = wrongindex_storage::resolve_index_dir(&path, &global);
    let inspection =
        wrongindex_storage::inspect_index(&Path::new(&resolved.index_dir).join("index.db"));
    Ok(Preview {
        path,
        resolved: serde_json::to_value(&resolved).map_err(|e| e.to_string())?,
        inspection: serde_json::to_value(&inspection).map_err(|e| e.to_string())?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Stdio};
    use std::time::Instant;
    use wrongindex_contracts::WatchPolicy;

    struct Daemon(Child);
    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn daemon_path() -> PathBuf {
        PathBuf::from(std::env::var_os("WRONGINDEX_TEST_DAEMON").expect("WRONGINDEX_TEST_DAEMON"))
    }

    fn spawn_daemon(root: &DataRoot, extra: &[&str], env: &[(&str, &str)]) -> Daemon {
        let mut cmd = std::process::Command::new(daemon_path());
        cmd.arg("--data-root")
            .arg(root.path())
            .args(extra)
            .env_remove("WRONGINDEX_ENGINE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        Daemon(cmd.spawn().unwrap())
    }

    fn next(service: &Service, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            let snapshot = service.shared.read();
            if predicate(&snapshot) {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "GPUI actor did not converge");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Send a command and wait for its outcome; the snapshot read afterwards
    /// reflects the state the command produced.
    fn send(service: &Service, sequence: u64, command: Command) -> (Outcome, Snapshot) {
        assert!(service.send(sequence, command));
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if let Ok(outcome) = service.outcomes.recv_timeout(Duration::from_millis(100))
                && outcome.sequence == sequence
            {
                return (outcome, service.shared.read());
            }
            assert!(Instant::now() < deadline, "no outcome for {sequence}");
        }
    }

    fn write(root: &Path, name: &str) {
        std::fs::write(
            root.join("entry.ts"),
            format!("export function {name}() {{ return 1; }}\n"),
        )
        .unwrap();
    }

    fn project_id(s: &Snapshot) -> String {
        s.projects()[0]["project"]["projectId"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    #[ignore = "needs WRONGINDEX_TEST_DAEMON; real GPUI client actor and daemon"]
    fn actor_registers_indexes_and_closing_the_client_preserves_pinned_watching() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let index = temp.path().join("index");
        std::fs::create_dir(&project).unwrap();
        write(&project, "beforeClientClose");
        let root = DataRoot::resolve(Some(&temp.path().join("service"))).unwrap();
        let _daemon = spawn_daemon(&root, &["--backend", "native"], &[]);
        let service = spawn(root.clone(), daemon_path());
        next(&service, |s| s.connected);
        let (registered, _) = send(
            &service,
            1,
            Command::Register {
                root: project.display().to_string(),
                index_dir: index.display().to_string(),
                policy: WatchPolicy::Pinned,
            },
        );
        assert!(registered.ok, "{:?}", registered.message);
        assert!(registered.select.is_some());
        let current = next(&service, |s| {
            s.projects()
                .first()
                .is_some_and(|p| p["freshness"] == "current" && p["generation"].as_i64() > Some(0))
        });
        let id = project_id(&current);
        send(&service, 2, Command::Select(Some(id.clone())));
        let counts = next(&service, |s| {
            s.detail
                .as_ref()
                .is_some_and(|d| d.project_id == id && d.stats.is_some())
        });
        let stats = counts.detail.unwrap().stats.unwrap();
        assert_eq!(stats["totalFiles"], 1, "{stats}");
        let (checked, _) = send(
            &service,
            3,
            Command::Action {
                project: id.clone(),
                action: ProjectAction::Check,
            },
        );
        assert!(checked.ok, "{:?}", checked.message);
        let (searched, after) = send(
            &service,
            4,
            Command::Search {
                project: id.clone(),
                query: "beforeClientClose".into(),
            },
        );
        assert!(searched.ok, "{:?}", searched.message);
        assert!(
            after.search.unwrap().result["result"]
                .to_string()
                .contains("beforeClientClose")
        );
        drop(service);
        write(&project, "afterClientClose");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut client = connect(&root).await.ok().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let result = call(
                    &mut client,
                    method::INDEX_QUERY,
                    Some(&id),
                    json!({"op":"search","args":{"query":"afterClientClose","limit":5}}),
                )
                .await
                .ok()
                .unwrap();
                if result["result"].to_string().contains("afterClientClose") {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "pinned watcher stopped after GPUI client drop"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        // Registration and data survive the client; only this test's daemon
        // and disposable roots are removed by their guards.
        assert!(index.join("index.db").exists());
    }

    #[test]
    #[ignore = "needs WRONGINDEX_TEST_DAEMON; maintenance actions through the GPUI actor"]
    fn actor_backs_up_restores_and_removes_without_touching_sources() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let index = temp.path().join("index");
        std::fs::create_dir(&project).unwrap();
        write(&project, "kept");
        let root = DataRoot::resolve(Some(&temp.path().join("service"))).unwrap();
        let _daemon = spawn_daemon(&root, &["--backend", "native"], &[]);
        let service = spawn(root.clone(), daemon_path());
        next(&service, |s| s.connected);
        let (r, _) = send(
            &service,
            1,
            Command::Register {
                root: project.display().to_string(),
                index_dir: index.display().to_string(),
                policy: WatchPolicy::Manual,
            },
        );
        assert!(r.ok, "{:?}", r.message);
        let id = r.select.unwrap();
        let (claimed, _) = send(
            &service,
            2,
            Command::Action {
                project: id.clone(),
                action: ProjectAction::IndexNow,
            },
        );
        assert!(claimed.ok, "{:?}", claimed.message);
        // A manual project is never reported current; its first generation
        // is published all the same.
        next(&service, |s| {
            s.project(&id)
                .is_some_and(|p| p["generation"].as_i64() > Some(0))
        });
        // A path-like backup name never reaches the service.
        let (refused, _) = send(
            &service,
            3,
            Command::Action {
                project: id.clone(),
                action: ProjectAction::Restore(r"..\index.db".into()),
            },
        );
        assert!(!refused.ok);
        let (backed, _) = send(
            &service,
            4,
            Command::Action {
                project: id.clone(),
                action: ProjectAction::Backup,
            },
        );
        assert!(backed.ok, "{:?}", backed.message);
        send(&service, 5, Command::Select(Some(id.clone())));
        let listed = next(&service, |s| {
            s.detail.as_ref().is_some_and(|d| !d.backups.is_empty())
        });
        let name = listed.detail.unwrap().backups[0]["name"]
            .as_str()
            .unwrap()
            .to_string();
        let (restored, _) = send(
            &service,
            6,
            Command::Action {
                project: id.clone(),
                action: ProjectAction::Restore(name),
            },
        );
        assert!(restored.ok, "{:?}", restored.message);
        // Detaching waits for the project's jobs (the service refuses it
        // while one runs, and the window shows that refusal).
        next(&service, |s| {
            !s.jobs.is_empty()
                && s.jobs
                    .iter()
                    .all(|j| !matches!(j["state"].as_str(), Some("queued" | "running")))
        });
        let (removed, after) = send(
            &service,
            7,
            Command::Action {
                project: id.clone(),
                action: ProjectAction::Remove,
            },
        );
        assert!(removed.ok, "{:?}", removed.message);
        assert!(after.project(&id).is_none());
        assert!(after.detail.is_none());
        assert!(project.join("entry.ts").exists());
        assert!(index.join("index.db").exists());
    }

    #[test]
    #[ignore = "needs Windows IPC; real offline GPUI client actor"]
    fn offline_actor_reports_errors_without_inventing_projects_or_writing_an_index() {
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(&temp.path().join("service"))).unwrap();
        let service = spawn(root, temp.path().join("missing-daemon.exe"));
        let offline = next(&service, |s| s.reason.is_some());
        assert!(!offline.connected);
        assert!(offline.projects().is_empty());
        let (failed, after) = send(&service, 1, Command::Start);
        assert!(!failed.ok);
        assert!(failed.message.is_some());
        assert!(!after.connected);
        assert!(after.projects().is_empty());
        let (refused, _) = send(
            &service,
            2,
            Command::Search {
                project: "p_1".into(),
                query: "   ".into(),
            },
        );
        assert!(!refused.ok);
        assert!(!temp.path().join("index.db").exists());
    }

    #[test]
    #[ignore = "needs WRONGINDEX_TEST_DAEMON; real settings mutation through the GPUI actor"]
    fn settings_changes_are_acknowledged_and_preserve_other_settings() {
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(&temp.path().join("service"))).unwrap();
        let _daemon = spawn_daemon(&root, &[], &[]);
        let service = spawn(root.clone(), daemon_path());
        next(&service, |s| s.connected);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut client = connect(&root).await.ok().unwrap();
            call(
                &mut client,
                method::SETTINGS_SET,
                None,
                json!({"settings":{"reconcileMinutes":17}}),
            )
            .await
            .ok()
            .unwrap();
        });
        for (sequence, paused) in [(1u64, true), (2, false)] {
            let (acknowledged, after) = send(
                &service,
                sequence,
                Command::ApplySettings {
                    patch: json!({"paused": paused}),
                    revision: None,
                },
            );
            assert!(acknowledged.ok, "{:?}", acknowledged.message);
            assert_eq!(after.settings["settings"]["paused"], paused);
            runtime.block_on(async {
                let mut client = connect(&root).await.ok().unwrap();
                let actual = call(&mut client, method::SETTINGS_GET, None, json!({}))
                    .await
                    .ok()
                    .unwrap();
                assert_eq!(actual["settings"]["paused"], paused);
                assert_eq!(actual["settings"]["reconcileMinutes"], 17);
                assert_eq!(actual["revision"], sequence + 1);
            });
        }
        // A stale revision is refused and leaves the applied values.
        let (stale, after) = send(
            &service,
            3,
            Command::ApplySettings {
                patch: json!({"backupKeep": 9}),
                revision: Some(1),
            },
        );
        assert!(!stale.ok);
        assert_ne!(after.settings["settings"]["backupKeep"], 9);
    }

    #[test]
    #[ignore = "needs WRONGINDEX_TEST_DAEMON; real cold start and already-running GPUI client"]
    fn actor_start_authenticates_and_preserves_an_already_running_instance() {
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(&temp.path().join("service"))).unwrap();
        let service = spawn(root.clone(), daemon_path());
        next(&service, |s| !s.connected && s.reason.is_some());
        let (started, first) = send(&service, 1, Command::Start);
        let (repeated, second) = send(&service, 2, Command::Start);
        let (stopping, _) = send(&service, 3, Command::Stop);
        drop(service);
        assert!(stopping.ok, "{:?}", stopping.message);
        assert!(
            first.connected,
            "Start must acknowledge a real authenticated service"
        );
        assert!(second.connected);
        assert_eq!(
            first.health["daemonInstanceId"],
            second.health["daemonInstanceId"]
        );
        assert_eq!(
            started.message.as_deref(),
            Some("Background service is ready.")
        );
        assert_eq!(
            repeated.message.as_deref(),
            Some("Background service is ready.")
        );
    }

    #[test]
    #[ignore = "needs WRONGINDEX_TEST_DAEMON; sign-in start without a window"]
    fn sign_in_start_starts_the_service_once() {
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(&temp.path().join("service"))).unwrap();
        start_service_now(&root, &daemon_path()).unwrap();
        // Already running: reported as started, not started twice.
        start_service_now(&root, &daemon_path()).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut c = connect(&root).await.ok().unwrap();
            call(&mut c, method::DAEMON_SHUTDOWN, None, json!({}))
                .await
                .ok()
                .unwrap();
        });
    }

    #[test]
    #[ignore = "needs a debug WRONGINDEX_TEST_DAEMON with the native stall hook"]
    fn debug_start_waits_for_draining_service_and_preserves_its_job() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("slowproj");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("config.json"), "{\"keepAfterRestart\":true}").unwrap();
        let root = DataRoot::resolve(Some(&temp.path().join("service"))).unwrap();
        let _first = spawn_daemon(
            &root,
            &["--drain-seconds", "60"],
            &[("WRONGINDEX_NATIVE_STALL", "slowproj:8")],
        );
        let service = spawn(root.clone(), daemon_path());
        next(&service, |s| s.connected);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (old_instance, job_id) = runtime.block_on(async {
            let mut client = connect(&root).await.ok().unwrap();
            let old_instance = client.hello().daemon_instance_id.clone();
            let reg = call(&mut client, method::PROJECT_REGISTER, None, json!({"checkoutRoot":project,"indexDir":temp.path().join("index"),"watchPolicy":"manual"})).await.ok().unwrap();
            let id = reg["project"]["projectId"].as_str().unwrap();
            call(&mut client, method::OWNERSHIP_ACQUIRE, Some(id), json!({})).await.ok().unwrap();
            let admitted = call(&mut client, method::INDEX_REQUEST, Some(id), json!({"kind":"full"})).await.ok().unwrap();
            let job_id = admitted["job"]["jobId"].as_str().unwrap().to_string();
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let job = call(&mut client, method::JOB_GET, None, json!({"jobId":job_id})).await.ok().unwrap();
                if job["state"] == "running" { break; }
                assert!(Instant::now() < deadline, "fixture job did not start");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            call(&mut client, method::DAEMON_SHUTDOWN, None, json!({})).await.ok().unwrap();
            let refused = Client::connect(&root, "drain-precondition").await;
            assert_eq!(refused.err().and_then(|e| e.code()), Some(ErrorCode::ShuttingDown), "fixture must be draining when Start is pressed");
            (old_instance, job_id)
        });
        let (_, acknowledged) = send(&service, 1, Command::Start);
        // Always stop a reachable successor before asserting, including on a
        // failed assertion. Only the original child has a process guard.
        let successor = runtime.block_on(async {
            let Ok(mut client) = connect(&root).await else {
                return None;
            };
            let instance = client.hello().daemon_instance_id.clone();
            let job = call(&mut client, method::JOB_GET, None, json!({"jobId":job_id}))
                .await
                .ok();
            let stopped = call(&mut client, method::DAEMON_SHUTDOWN, None, json!({}))
                .await
                .is_ok();
            Some((instance, job, stopped))
        });
        drop(service);
        assert!(
            acknowledged.connected,
            "Start was acknowledged before an authenticated successor was ready"
        );
        let (instance, job, stopped) =
            successor.expect("Start must launch an authenticated successor after the drain");
        assert_ne!(instance, old_instance);
        assert_eq!(
            job.unwrap()["state"],
            "succeeded",
            "Start must let the old job drain instead of killing it"
        );
        assert!(stopped);
    }
}
