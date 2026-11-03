//! Connection handling, authentication, and method dispatch.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, watch};
use wrongindex_contracts::{
    AuthRequest, AuthResponse, BASELINE_PROFILE_ID, BackendPreference, EndpointInfo, ErrorBody,
    ErrorCode, Health, Hello, IndexRequestParams, JobAdmission, JobKind, JobState, Limits,
    MAX_FRAME_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR, ProjectStatus, RegisterProjectParams,
    RegisterProjectResult, Request, Response, WatchPolicy, method,
};
use wrongindex_ipc::endpoint::{generate_token, token_eq};
use wrongindex_ipc::frame::{FrameError, read_body, read_header, write_frame};
use wrongindex_ipc::{DataRoot, Discovery};
use wrongindex_registry::{Registry, now_ms};

use crate::{info, warn};

/// Authentication must complete within this window.
const AUTH_DEADLINE: Duration = Duration::from_secs(5);
/// Once a frame header arrives, its body must arrive within this window.
const BODY_DEADLINE: Duration = Duration::from_secs(10);
/// Auth frames are tiny; never buffer a large unauthenticated payload.
const MAX_AUTH_FRAME_BYTES: usize = 16 * 1024;
const MAX_CONNECTIONS: usize = 64;
const MAX_QUEUED_JOBS: usize = 1_000;
const MAX_PAGE_SIZE: usize = 500;

pub struct Config {
    pub root: DataRoot,
    pub drain: Duration,
    pub engine: Option<crate::engine::EngineConfig>,
    pub backend: Backend,
}

/// Which engine writes indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Backend {
    /// Native when the project is supported, else the compatibility worker.
    Auto,
    /// Native engine only; unsupported projects fail with NOT_IMPLEMENTED.
    Native,
    /// WrongStack compatibility worker only.
    Legacy,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Auto => "auto",
            Backend::Native => "native",
            Backend::Legacy => "legacy",
        }
    }
}

#[derive(Debug)]
pub enum StartError {
    AlreadyRunning(String),
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for StartError {
    fn from(e: E) -> Self {
        StartError::Other(e.into())
    }
}

pub struct Shared {
    pub registry: Arc<Mutex<Registry>>,
    pub instance_id: String,
    token: String,
    pub started_at_ms: i64,
    pub draining: AtomicBool,
    pub shutdown: watch::Sender<bool>,
    pub wake_worker: Notify,
    pub owners: crate::ownership::Owners,
    connections: AtomicUsize,
    sqlite_version: String,
    pub engine: crate::engine::Engine,
    pub watch: crate::watch::Supervisor,
    pub events: crate::events::EventBus,
    pub backend: Backend,
    pub settings: crate::settings::Store,
    /// Phase of the job running per project (native engine runs).
    pub progress: Mutex<std::collections::HashMap<String, Value>>,
    pub metrics: crate::metrics::Metrics,
}

impl Shared {
    /// The engine for a project: its preference within what this service
    /// runs (`--backend`). A preference the service cannot honour is an
    /// error, never a silent switch to the other engine.
    pub fn backend_for(&self, pref: BackendPreference) -> Result<Backend, ErrorBody> {
        match (self.backend, pref) {
            (b, BackendPreference::Auto) => Ok(b),
            (Backend::Legacy, BackendPreference::Native) => Err(ErrorBody::new(
                ErrorCode::NotImplemented,
                "the project asks for the native engine, but this service runs with --backend legacy",
            )),
            (Backend::Native, BackendPreference::Legacy) => Err(ErrorBody::new(
                ErrorCode::NotImplemented,
                "the project asks for the compatibility worker, but this service runs with --backend native",
            )),
            (_, BackendPreference::Native) => Ok(Backend::Native),
            (_, BackendPreference::Legacy) => Ok(Backend::Legacy),
        }
    }

    /// Push the current settings into the running parts (watcher timing,
    /// session grace, pause); the worker and native runs read them per use.
    pub fn apply_settings(&self) {
        let s = self.settings.get();
        self.watch.configure(&s);
        self.wake_worker.notify_one();
    }

    /// Run a registry operation on the blocking pool; SQLite work never runs
    /// on the async reactor.
    pub async fn registry<T, F>(self: &Arc<Self>, f: F) -> Result<T, ErrorBody>
    where
        T: Send + 'static,
        F: FnOnce(&mut Registry) -> Result<T, ErrorBody> + Send + 'static,
    {
        let reg = self.registry.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = reg
                .lock()
                .map_err(|_| ErrorBody::new(ErrorCode::Internal, "registry lock poisoned"))?;
            f(&mut guard)
        })
        .await
        .map_err(|e| ErrorBody::new(ErrorCode::Internal, format!("registry task failed: {e}")))?
    }

    /// Enter draining exactly once: announce it on the event stream before
    /// subscribers are told to stop, so they see why the stream closes.
    pub fn begin_drain(&self) {
        if !self.draining.swap(true, Ordering::SeqCst) {
            self.events.publish("daemon.draining", None, json!({}));
        }
        let _ = self.shutdown.send(true);
    }

    /// Whether write jobs can run at all with this configuration.
    pub fn can_write(&self) -> bool {
        self.backend != Backend::Legacy || self.engine.configured()
    }

    fn hello(&self, selected_profile: Option<String>) -> Hello {
        Hello {
            daemon_instance_id: self.instance_id.clone(),
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            supported_profile_ids: vec![BASELINE_PROFILE_ID.to_string()],
            selected_profile_id: selected_profile,
            profile_fingerprint: Some(
                wrongindex_storage::profile::baseline_semantic()
                    .profile_fingerprint
                    .clone(),
            ),
            capabilities: vec![
                "register".into(),
                "check".into(),
                "jobs".into(),
                "events".into(),
            ],
            limits: Limits {
                max_frame_bytes: MAX_FRAME_BYTES,
                max_page_size: MAX_PAGE_SIZE,
                max_inflight: 1,
            },
        }
    }
}

pub async fn run(cfg: Config) -> Result<(), StartError> {
    let endpoint = cfg.root.endpoint_name()?;

    // 1. Singleton: the first pipe instance is the OS-backed guard. A losing
    //    starter returns here without touching the data root at all: not the
    //    registry, not discovery, and not the private directories (several
    //    starters re-hardening the same directories at once could fail).
    #[cfg(windows)]
    let (listener, first) = match wrongindex_ipc::win::PipeListener::bind(&endpoint) {
        Ok(x) => x,
        Err(e) if e.raw_os_error() == Some(5) => {
            // Never assume the holder is our own daemon: a name pre-created by
            // another account must be reported, not treated as "running".
            use wrongindex_ipc::win::{ExistingOwner, PipeListener};
            return Err(match PipeListener::existing_owner(&endpoint).await {
                ExistingOwner::SameUser { .. } => StartError::AlreadyRunning(endpoint),
                ExistingOwner::Foreign { detail } => StartError::Other(anyhow::anyhow!(
                    "endpoint {endpoint} is held by a process that is not this user's WrongIndex daemon ({detail}); refusing to start"
                )),
            });
        }
        Err(e) => return Err(anyhow::Error::new(e).context("binding endpoint").into()),
    };
    cfg.root
        .ensure_private_dirs()
        .with_context(|| format!("creating data root {}", cfg.root.path().display()))?;
    crate::log::init(&cfg.root.logs_dir());
    #[cfg(not(windows))]
    return Err(anyhow::anyhow!(
        "the WrongIndex daemon currently supports Windows only (Unix sockets are M8)"
    )
    .into());

    // 2. Registry: migrate and recover work interrupted by a previous instance.
    let mut registry = Registry::open(&cfg.root.registry_db())
        .map_err(|e| anyhow::anyhow!("opening registry: {e}"))?;
    let recovery = registry
        .recover_interrupted()
        .map_err(|e| anyhow::anyhow!("recovering registry: {e}"))?;
    // Ownership recorded by a previous instance is never trusted: its OS
    // guard died with it. Desired ownership is re-negotiated below.
    let desired_ownership = registry
        .recover_ownership()
        .map_err(|e| anyhow::anyhow!("recovering ownership: {e}"))?;

    // 3. Credential + discovery.
    let instance_id = format!("d_{}", uuid::Uuid::new_v4().simple());
    let token = generate_token()?;
    let started_at_ms = now_ms();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let (watch_supervisor, watch_rx) = crate::watch::Supervisor::new();
    let shared = Arc::new(Shared {
        registry: Arc::new(Mutex::new(registry)),
        instance_id: instance_id.clone(),
        token,
        started_at_ms,
        draining: AtomicBool::new(false),
        shutdown: shutdown_tx,
        wake_worker: Notify::new(),
        owners: crate::ownership::Owners::default(),
        connections: AtomicUsize::new(0),
        sqlite_version: wrongindex_storage::sqlite_runtime().version,
        engine: crate::engine::Engine::new(cfg.engine.clone()),
        watch: watch_supervisor,
        events: crate::events::EventBus::new(instance_id.clone()),
        backend: cfg.backend,
        settings: crate::settings::Store::load(&cfg.root.path().join("settings.json")),
        progress: Mutex::new(std::collections::HashMap::new()),
        metrics: crate::metrics::Metrics::default(),
    });
    shared.apply_settings();
    let info = EndpointInfo {
        endpoint: endpoint.clone(),
        daemon_instance_id: instance_id.clone(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        started_at_ms,
    };
    Discovery::publish(&cfg.root, &info, &shared.token)?;
    info!("ready", {
        "instanceId": instance_id,
        "endpoint": endpoint,
        "dataRoot": cfg.root.path().display().to_string(),
        "requeued": recovery.requeued,
        "failedInterrupted": recovery.failed,
    });

    if !desired_ownership.is_empty() {
        crate::worker::prune(&shared).await;
        let restored = crate::ownership::restore(&shared, desired_ownership).await;
        info!("ownership_restored", {"results": restored});
    }

    let worker = tokio::spawn(crate::worker::run(shared.clone(), shutdown_rx.clone()));
    let watcher = tokio::spawn(crate::watch::run(
        shared.clone(),
        watch_rx,
        shutdown_rx.clone(),
    ));

    // 4. Accept loop.
    #[cfg(windows)]
    {
        let mut server = first;
        loop {
            tokio::select! {
                connected = server.connect() => {
                    if let Err(e) = connected {
                        warn!("accept_failed", {"error": e.to_string()});
                        server = listener.next()?;
                        continue;
                    }
                    let conn = server;
                    server = listener.next()?;
                    tokio::spawn(handle_connection(shared.clone(), conn));
                }
                _ = shutdown_rx.changed() => break,
                _ = tokio::signal::ctrl_c() => {
                    info!("signal_shutdown");
                    break;
                }
            }
        }
    }

    // 5. Drain: refuse new mutating work, let the worker finish its current
    //    job within the budget, then retract only our own discovery metadata.
    shared.begin_drain();
    if tokio::time::timeout(cfg.drain, worker).await.is_err() {
        warn!("drain_budget_exceeded", {"seconds": cfg.drain.as_secs()});
    }
    let _ = tokio::time::timeout(Duration::from_secs(5), watcher).await;
    shared.engine.shutdown().await;
    crate::ownership::release_all_for_shutdown(&shared).await;
    let retracted = Discovery::retract(&cfg.root, &instance_id).unwrap_or(false);
    info!("stopped", {"instanceId": instance_id, "discoveryRetracted": retracted});
    Ok(())
}

struct ConnectionGuard(Arc<Shared>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn handle_connection<S>(shared: Arc<Shared>, mut conn: S)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let active = shared.connections.fetch_add(1, Ordering::SeqCst) + 1;
    let _guard = ConnectionGuard(shared.clone());
    if active > MAX_CONNECTIONS {
        let _ = write_frame(
            &mut conn,
            &AuthResponse::Error {
                error: ErrorBody::new(ErrorCode::QueueSaturated, "too many connections"),
            },
            MAX_FRAME_BYTES,
        )
        .await;
        return;
    }

    // Authentication precedes every other frame.
    let auth: AuthRequest = match tokio::time::timeout(AUTH_DEADLINE, async {
        match read_header(&mut conn, MAX_AUTH_FRAME_BYTES).await? {
            None => Err(FrameError::Truncated),
            Some(size) => read_body(&mut conn, size).await,
        }
    })
    .await
    {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => {
            let _ = write_frame(
                &mut conn,
                &AuthResponse::Error {
                    error: ErrorBody::new(
                        ErrorCode::Unauthenticated,
                        format!("invalid authentication frame: {e}"),
                    ),
                },
                MAX_FRAME_BYTES,
            )
            .await;
            return;
        }
        Err(_) => return,
    };
    if !token_eq(&auth.token, &shared.token) {
        warn!("auth_rejected", {"client": auth.client_name});
        let _ = write_frame(
            &mut conn,
            &AuthResponse::Error {
                error: ErrorBody::new(ErrorCode::Unauthenticated, "invalid credential"),
            },
            MAX_FRAME_BYTES,
        )
        .await;
        return;
    }
    if auth.protocol_major != PROTOCOL_MAJOR {
        let _ = write_frame(
            &mut conn,
            &AuthResponse::Error {
                error: ErrorBody::new(
                    ErrorCode::ProtocolUnsupported,
                    format!(
                        "protocol {} unsupported; daemon speaks {PROTOCOL_MAJOR}",
                        auth.protocol_major
                    ),
                ),
            },
            MAX_FRAME_BYTES,
        )
        .await;
        return;
    }
    let selected = if auth.supported_profile_ids.is_empty()
        || auth
            .supported_profile_ids
            .iter()
            .any(|p| p == BASELINE_PROFILE_ID)
    {
        Some(BASELINE_PROFILE_ID.to_string())
    } else {
        let _ = write_frame(
            &mut conn,
            &AuthResponse::Error {
                error: ErrorBody::new(
                    ErrorCode::ProfileUnsupported,
                    format!("no common profile; daemon supports {BASELINE_PROFILE_ID}"),
                ),
            },
            MAX_FRAME_BYTES,
        )
        .await;
        return;
    };
    let client = auth.client_name.clone();
    if write_frame(
        &mut conn,
        &AuthResponse::Ok {
            hello: shared.hello(selected),
        },
        MAX_FRAME_BYTES,
    )
    .await
    .is_err()
    {
        return;
    }

    let mut shutdown_rx = shared.shutdown.subscribe();
    loop {
        let header = tokio::select! {
            h = read_header(&mut conn, MAX_FRAME_BYTES) => h,
            _ = shutdown_rx.changed() => return,
        };
        let size = match header {
            Ok(Some(size)) => size,
            Ok(None) => return,
            Err(FrameError::TooLarge { size, max }) => {
                // The oversized body is never read; the stream can't be resynced.
                let _ = write_frame(
                    &mut conn,
                    &Response::err(
                        "",
                        ErrorBody::new(
                            ErrorCode::InvalidRequest,
                            format!("frame of {size} bytes exceeds {max}"),
                        ),
                    ),
                    MAX_FRAME_BYTES,
                )
                .await;
                return;
            }
            Err(_) => return,
        };
        let request: Request = match tokio::time::timeout(BODY_DEADLINE, read_body(&mut conn, size))
            .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(FrameError::Invalid(e))) => {
                // Body fully consumed, so the stream stays in sync.
                let resp = Response::err(
                    "",
                    ErrorBody::new(ErrorCode::InvalidRequest, format!("malformed request: {e}")),
                );
                if write_frame(&mut conn, &resp, MAX_FRAME_BYTES)
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
            _ => return,
        };
        let is_shutdown = request.method == method::DAEMON_SHUTDOWN;
        if request.method == method::EVENTS_SUBSCRIBE {
            // The connection becomes a one-way event stream until the client
            // closes it or the daemon shuts down.
            stream_events(&shared, conn, request, shutdown_rx).await;
            return;
        }
        let response = dispatch(&shared, &client, request).await;
        let accepted = response.error.is_none();
        if write_frame(&mut conn, &response, MAX_FRAME_BYTES)
            .await
            .is_err()
        {
            return;
        }
        if is_shutdown && accepted {
            info!("shutdown_requested", {"client": client});
            shared.begin_drain();
            return;
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SubscribeParams {
    #[serde(default)]
    since_seq: Option<u64>,
    #[serde(default)]
    since_instance_id: Option<String>,
    #[serde(default)]
    project_id: Option<String>,
}

/// Serve `events.subscribe`: acknowledge with the stream position, replay
/// retained events after `sinceSeq`, then forward live events. A subscriber
/// that falls behind the bounded buffer receives a `stream.gap` event and must
/// resynchronize with `events.snapshot`; memory never grows per subscriber.
async fn stream_events<C>(
    shared: &Arc<Shared>,
    mut conn: C,
    request: Request,
    mut shutdown_rx: watch::Receiver<bool>,
) where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let id = request.request_id.clone();
    if request.protocol != PROTOCOL_MAJOR {
        let _ = write_frame(
            &mut conn,
            &Response::err(
                id,
                ErrorBody::new(
                    ErrorCode::ProtocolUnsupported,
                    format!("envelope protocol {}", request.protocol),
                ),
            ),
            MAX_FRAME_BYTES,
        )
        .await;
        return;
    }
    let p: SubscribeParams = match parse(request.params) {
        Ok(p) => p,
        Err(e) => {
            let _ = write_frame(&mut conn, &Response::err(id, e), MAX_FRAME_BYTES).await;
            return;
        }
    };
    let filter = p.project_id.or(request.project_id);
    let sub = shared
        .events
        .subscribe(p.since_seq, p.since_instance_id.as_deref());
    let ack = Response::ok(
        id,
        json!({
            "instanceId": shared.instance_id,
            "currentSeq": sub.current_seq,
            "gap": sub.gap,
            "replayed": sub.replay.len(),
        }),
    );
    if write_frame(&mut conn, &ack, MAX_FRAME_BYTES).await.is_err() {
        return;
    }
    let wanted = |e: &crate::events::Event| {
        filter.is_none() || e.project_id.is_none() || e.project_id == filter
    };
    for ev in sub.replay.iter().filter(|e| wanted(e)) {
        if write_frame(&mut conn, ev, MAX_FRAME_BYTES).await.is_err() {
            return;
        }
    }
    let mut rx = sub.rx;
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    if wanted(&ev)
                        && write_frame(&mut conn, &ev, MAX_FRAME_BYTES).await.is_err()
                    {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    let gap = json!({
                        "kind": "stream.gap",
                        "instanceId": shared.instance_id,
                        "data": {"missed": missed},
                    });
                    if write_frame(&mut conn, &gap, MAX_FRAME_BYTES).await.is_err() {
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
            // Any inbound frame or EOF ends the stream; subscribers only read.
            _ = read_header(&mut conn, MAX_FRAME_BYTES) => return,
            _ = shutdown_rx.changed() => {
                // Flush what was published before shutdown (the drain notice).
                while let Ok(ev) = rx.try_recv() {
                    if !wanted(&ev)
                        || write_frame(&mut conn, &ev, MAX_FRAME_BYTES).await.is_ok()
                    {
                        continue;
                    }
                    return;
                }
                return;
            }
        }
    }
}

/// Observable status of one project (registry + watcher state).
async fn project_status(shared: &Arc<Shared>, pid: String) -> Result<ProjectStatus, ErrorBody> {
    let p2 = pid.clone();
    let (project, last_job, ownership, publication) = shared
        .registry(move |r| {
            Ok((
                r.get_project(&p2)?,
                r.latest_job(&p2)?,
                r.ownership_status(&p2)?,
                r.publication_state(&p2)?,
            ))
        })
        .await?;
    let ws = shared.watch.status(&pid);
    let freshness = crate::watch::Supervisor::freshness(
        &ws,
        ws.last_error
            .as_deref()
            .or(publication.last_error.as_deref()),
    );
    Ok(ProjectStatus {
        project,
        freshness,
        ownership: ownership.state,
        activity: match last_job.as_ref() {
            Some(j) if j.state == JobState::Running => {
                if j.kind == JobKind::Check {
                    "checking".into()
                } else {
                    "indexing".into()
                }
            }
            _ => "dormant".into(),
        },
        watch_health: ws.watch_health.clone(),
        last_job,
        pending_paths: ws.pending_paths,
        reconcile_reason: ws.reconcile_reason.clone(),
        sessions: ws.sessions,
        generation: publication.generation,
        last_indexed_ms: publication.last_index_ms,
        last_error: ws.last_error.clone().or(publication.last_error.clone()),
        progress: shared
            .progress
            .lock()
            .ok()
            .and_then(|p| p.get(&pid).cloned()),
        consecutive_failures: ws.consecutive_failures,
        next_retry_ms: ws.retry_in_ms.map(|ms| now_ms() + ms as i64),
        parked: ws.parked,
    })
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, ErrorBody> {
    let params = if params.is_null() { json!({}) } else { params };
    serde_json::from_value(params)
        .map_err(|e| ErrorBody::new(ErrorCode::InvalidRequest, format!("invalid params: {e}")))
}

fn to_value<T: serde::Serialize>(v: T) -> Result<Value, ErrorBody> {
    serde_json::to_value(v).map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))
}

fn require_project(req: &Request) -> Result<String, ErrorBody> {
    req.project_id
        .clone()
        .ok_or_else(|| ErrorBody::new(ErrorCode::InvalidRequest, "projectId is required"))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JobGetParams {
    job_id: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct JobListParams {
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn dispatch(shared: &Arc<Shared>, client: &str, req: Request) -> Response {
    let id = req.request_id.clone();
    match dispatch_inner(shared, client, req).await {
        Ok(v) => Response::ok(id, v),
        Err(e) => Response::err(id, e),
    }
}

async fn dispatch_inner(
    shared: &Arc<Shared>,
    client: &str,
    req: Request,
) -> Result<Value, ErrorBody> {
    if req.protocol != PROTOCOL_MAJOR {
        return Err(ErrorBody::new(
            ErrorCode::ProtocolUnsupported,
            format!("envelope protocol {}", req.protocol),
        ));
    }
    let read_only = matches!(
        req.method.as_str(),
        method::DAEMON_HELLO | method::DAEMON_HEALTH
    );
    if shared.draining.load(Ordering::SeqCst) && !read_only {
        return Err(ErrorBody::new(
            ErrorCode::ShuttingDown,
            "daemon is draining",
        ));
    }
    match req.method.as_str() {
        method::DAEMON_HELLO => to_value(shared.hello(Some(BASELINE_PROFILE_ID.to_string()))),
        method::DAEMON_HEALTH => {
            let (projects, queued, running) = shared
                .registry(|r| {
                    Ok((
                        r.list_projects()?.len(),
                        r.count_jobs(JobState::Queued)?,
                        r.count_jobs(JobState::Running)?,
                    ))
                })
                .await?;
            to_value(Health {
                daemon_instance_id: shared.instance_id.clone(),
                daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                pid: std::process::id(),
                started_at_ms: shared.started_at_ms,
                uptime_ms: now_ms() - shared.started_at_ms,
                draining: shared.draining.load(Ordering::SeqCst),
                project_count: projects,
                queued_jobs: queued,
                running_jobs: running,
                sqlite_version: shared.sqlite_version.clone(),
                backend_inventory: vec![
                    "inspector:read-only (in-process)".into(),
                    shared.engine.describe().to_string(),
                    json!({
                        "backend": "native",
                        "selection": shared.backend.as_str(),
                        "writes": "every WrongStack language (Python and Go through their toolchains, tree-sitter languages through bun or node); fresh builds, in-place incremental runs, targeted (watcher) runs and no-ops as WrongStack; enrichment rows kept as WrongStack keeps them; indexes with extra schema or another version are left to the compatibility worker",
                        "queries": wrongindex_engine::query::NATIVE_OPS,
                    })
                    .to_string(),
                    json!({"engineHello": shared.engine.hello().await}).to_string(),
                ],
                paused: shared.settings.get().paused,
                settings_revision: shared.settings.snapshot()["revision"].as_u64().unwrap_or(0),
            })
        }
        method::DAEMON_SHUTDOWN => {
            Ok(json!({ "draining": true, "instanceId": shared.instance_id }))
        }
        method::PROJECT_REGISTER => {
            let p: RegisterProjectParams = parse(req.params)?;
            let (project, created) = shared.registry(move |r| r.register_project(&p)).await?;
            info!("project_registered", {"projectId": project.project_id, "created": created, "client": client});
            if created {
                shared
                    .events
                    .publish("project.registered", Some(&project.project_id), json!({}));
            }
            to_value(RegisterProjectResult { project, created })
        }
        method::PROJECT_LIST => {
            let projects = shared.registry(|r| r.list_projects()).await?;
            Ok(json!({ "projects": projects }))
        }
        method::PROJECT_GET => {
            let pid = require_project(&req)?;
            to_value(project_status(shared, pid).await?)
        }
        method::EVENTS_SNAPSHOT => {
            // Read the position first: anything published while the snapshot
            // is assembled is replayed by a subscribe from this sequence.
            let seq = shared.events.current_seq();
            let ids: Vec<String> = shared
                .registry(|r| r.list_projects())
                .await?
                .into_iter()
                .map(|p| p.project_id)
                .collect();
            let mut projects = Vec::with_capacity(ids.len());
            for pid in ids {
                match project_status(shared, pid).await {
                    Ok(s) => projects.push(s),
                    // Detached concurrently: its event follows the snapshot.
                    Err(e) if e.code == ErrorCode::ProjectNotFound => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(json!({
                "instanceId": shared.instance_id,
                "seq": seq,
                "projects": projects,
            }))
        }
        method::PROJECT_DETACH => {
            let pid = require_project(&req)?;
            // Never leave a guard or an "owned" record behind a removed project.
            if shared.owners.holds(&pid) {
                crate::ownership::release(shared, &pid, false).await?;
            }
            let project = shared.registry(move |r| r.detach_project(&pid)).await?;
            info!("project_detached", {"projectId": project.project_id, "client": client});
            shared
                .events
                .publish("project.detached", Some(&project.project_id), json!({}));
            Ok(json!({
                "project": project,
                "preserved": { "checkoutRoot": project.checkout_root, "indexDir": project.index_dir },
            }))
        }
        method::INDEX_REQUEST | method::INDEX_CHECK => {
            let pid = require_project(&req)?;
            let params: IndexRequestParams = if req.method == method::INDEX_CHECK {
                IndexRequestParams {
                    kind: JobKind::Check,
                    paths: Vec::new(),
                    backup: None,
                }
            } else {
                parse(req.params.clone())?
            };
            let mut params_json = to_value(&params)?;
            if params.kind != JobKind::Check {
                // Write work needs the engine and a valid ownership fence;
                // the fence travels with the job and is re-validated at run.
                if !shared.can_write() {
                    return Err(ErrorBody::new(
                        ErrorCode::NotImplemented,
                        format!(
                            "'{}' jobs need an indexing engine; start wrongindexd with --engine <WrongStack dir> or --backend auto|native",
                            params.kind.as_str()
                        ),
                    ));
                }
                let p2 = pid.clone();
                let (project, fence) = shared
                    .registry(move |r| Ok((r.get_project(&p2)?, r.fence(&p2)?)))
                    .await
                    .map_err(|e| {
                        if e.code == ErrorCode::OwnerConflict {
                            ErrorBody::new(
                                ErrorCode::OwnerConflict,
                                format!("{}; claim ownership first (ownership.acquire)", e.message),
                            )
                        } else {
                            e
                        }
                    })?;
                if params.kind == JobKind::Restore && params.backup.is_none() {
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidRequest,
                        "restore jobs need a backup name (see index.backups)",
                    ));
                }
                if params.kind == JobKind::Incremental && params.paths.is_empty() {
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidRequest,
                        "incremental jobs need at least one path",
                    ));
                }
                let root = std::path::Path::new(&project.checkout_root);
                let paths = params
                    .paths
                    .iter()
                    .map(|p| crate::engine::contained_path(root, p))
                    .collect::<Result<Vec<_>, _>>()?;
                params_json["paths"] = json!(paths);
                params_json["fence"] = to_value(fence)?;
            }
            let key = req.idempotency_key.clone();
            let client_name = client.to_string();
            let admission = shared
                .registry(move |r| {
                    if r.count_jobs(JobState::Queued)? >= MAX_QUEUED_JOBS {
                        return Err(ErrorBody::new(
                            ErrorCode::QueueSaturated,
                            "job queue is full",
                        ));
                    }
                    r.admit_job(
                        &pid,
                        params.kind,
                        &params_json,
                        &client_name,
                        key.as_deref(),
                    )
                })
                .await?;
            shared.wake_worker.notify_one();
            if !admission.deduplicated {
                crate::events::job_event(shared, "job.queued", &admission.job);
            }
            to_value(JobAdmission {
                job: admission.job,
                deduplicated: admission.deduplicated,
            })
        }
        method::INDEX_QUERY => {
            let pid = require_project(&req)?;
            #[derive(Deserialize)]
            struct QueryParams {
                op: String,
                #[serde(default)]
                args: Value,
            }
            let q: QueryParams = parse(req.params)?;
            let p2 = pid.clone();
            let (project, ownership, mut publication) = shared
                .registry(move |r| {
                    Ok((
                        r.get_project(&p2)?,
                        r.ownership_status(&p2)?,
                        r.publication_state(&p2)?,
                    ))
                })
                .await?;
            // Opening the index through the WrongStack engine runs its schema
            // initializer (a potential write), so reads also require ownership.
            // The run guard also keeps ownership (and the engine's handle on
            // the store) from being released while this read is in flight.
            let in_use = shared.owners.begin_run(&pid);
            if ownership.state != wrongindex_contracts::Ownership::Owned || in_use.is_none() {
                return Err(ErrorBody::new(
                    ErrorCode::OwnerConflict,
                    "queries through the preview engine require ownership; claim the project first",
                ));
            }
            let mut args = if q.args.is_object() {
                q.args
            } else {
                json!({})
            };
            // The daemon, never the client, decides which index is read.
            args["projectRoot"] = json!(project.checkout_root);
            args["indexDir"] = json!(project.index_dir);
            // Native read first (read-only connection, never mutates the
            // index); operations it has not ported fall back to the worker.
            let backend = shared.backend_for(project.backend_preference)?;
            let mut native_skipped = None;
            let mut served = None;
            // The `graph_stamp` the native answer was read with.
            let mut read_stamp: Option<Option<String>> = None;
            if backend != Backend::Legacy {
                let (dir, op, a2) = (
                    std::path::PathBuf::from(&project.index_dir),
                    q.op.clone(),
                    args.clone(),
                );
                let r = tokio::task::spawn_blocking(move || {
                    wrongindex_engine::query::Reader::open(&dir)?.call_snapshot(&op, &a2)
                })
                .await
                .map_err(|e| ErrorBody::new(ErrorCode::Internal, format!("query task: {e}")))?;
                use wrongindex_engine::query::QueryError;
                match r {
                    Ok((v, stamp)) => {
                        read_stamp = Some(stamp);
                        served = Some((v, "native"));
                    }
                    Err(QueryError::Unsupported(why))
                        if backend == Backend::Auto && shared.engine.configured() =>
                    {
                        native_skipped = Some(why);
                    }
                    Err(QueryError::Unsupported(why)) => {
                        return Err(ErrorBody::new(
                            ErrorCode::NotImplemented,
                            format!("native engine: {why}"),
                        ));
                    }
                    Err(QueryError::InvalidArgs(m)) => {
                        return Err(ErrorBody::new(ErrorCode::InvalidRequest, m));
                    }
                    Err(QueryError::Sqlite(e)) => {
                        return Err(ErrorBody::new(
                            ErrorCode::IndexDegraded,
                            format!("reading the index failed: {e}"),
                        ));
                    }
                }
            }
            let (result, backend) = match served {
                Some(x) => x,
                None => (
                    shared
                        .engine
                        .call(
                            "query",
                            json!({"op": q.op, "args": args}),
                            Duration::from_secs(60),
                        )
                        .await?,
                    "legacy-bun-worker",
                ),
            };
            drop(in_use);
            // A run commits its generation before the service records the
            // publication; an answer read in between describes the new one.
            // Label it with that generation once it is recorded (moments
            // later), or say the publication is pending (WI-513).
            // Only while a write job runs for the project: an index the
            // service adopted (never published here) is labelled as before.
            let mut pending = false;
            let job_running = || {
                shared
                    .progress
                    .lock()
                    .map(|p| p.contains_key(&pid))
                    .unwrap_or(false)
            };
            if let Some(stamp) = read_stamp
                && stamp.is_some()
                && stamp != publication.graph_stamp
                && job_running()
            {
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let finished = !job_running();
                    let p2 = pid.clone();
                    publication = shared.registry(move |r| r.publication_state(&p2)).await?;
                    if publication.graph_stamp == stamp {
                        break;
                    }
                    // Finished without publishing it (fenced off), or still
                    // unrecorded at the deadline: say so.
                    if finished || Instant::now() >= deadline {
                        pending = true;
                        publication.graph_stamp = stamp;
                        break;
                    }
                }
            }
            let mut out = json!({
                "projectId": pid,
                "daemonInstanceId": shared.instance_id,
                "generation": publication.generation,
                "graphStamp": publication.graph_stamp,
                "observedAt": now_ms(),
                "backend": backend,
                "result": result,
            });
            if let Some(why) = native_skipped {
                out["nativeSkipped"] = json!(why);
            }
            if pending {
                out["publicationPending"] = json!(true);
            }
            Ok(out)
        }
        method::INDEX_BACKUPS => {
            let pid = require_project(&req)?;
            let project = shared.registry(move |r| r.get_project(&pid)).await?;
            let dir = std::path::PathBuf::from(&project.index_dir);
            let backups = tokio::task::spawn_blocking(move || {
                wrongindex_storage::backup::list(&dir)
                    .into_iter()
                    .filter_map(|p| {
                        let meta = std::fs::metadata(&p).ok()?;
                        let modified = meta
                            .modified()
                            .ok()?
                            .duration_since(std::time::UNIX_EPOCH)
                            .ok()?
                            .as_millis() as i64;
                        Some(json!({
                            "name": p.file_name()?.to_string_lossy(),
                            "bytes": meta.len(),
                            "modifiedAtMs": modified,
                        }))
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?;
            Ok(json!({ "projectId": project.project_id, "backups": backups }))
        }
        method::SETTINGS_GET => Ok(shared.settings.snapshot()),
        method::DAEMON_METRICS => {
            let (queued, running) = shared
                .registry(|r| {
                    Ok((
                        r.count_jobs(JobState::Queued)?,
                        r.count_jobs(JobState::Running)?,
                    ))
                })
                .await?;
            let settings = shared.settings.get();
            let mut m = shared.metrics.snapshot();
            m["since"] = json!(shared.started_at_ms);
            m["queue"] = json!({"queued": queued, "running": running});
            m["connections"] = json!(shared.connections.load(Ordering::SeqCst));
            m["eventSeq"] = json!(shared.events.current_seq());
            // The bounds the service enforces (quotas).
            m["limits"] = json!({
                "maxQueuedJobs": MAX_QUEUED_JOBS,
                "maxFrameBytes": wrongindex_contracts::MAX_FRAME_BYTES,
                "maxDirtyPathsPerProject": crate::dirty::DebounceConfig::default().cap,
                "maxLogBytes": crate::log::MAX_LOG_BYTES,
                "maxIndexFileBytes": wrongindex_engine::native::MAX_INDEX_FILE_BYTES,
                "parallelJobs": settings.parallel_jobs,
                "nativeMemoryMb": settings.native_memory_mb,
                "backupKeep": settings.backup_keep,
                "watchParkAfterFailures": settings.watch_park_after_failures,
            });
            Ok(m)
        }
        method::SETTINGS_SET => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct SetParams {
                settings: Value,
                #[serde(default)]
                expected_revision: Option<u64>,
            }
            let p: SetParams = parse(req.params)?;
            let s2 = shared.clone();
            let snap = tokio::task::spawn_blocking(move || {
                s2.settings.set(&p.settings, p.expected_revision)
            })
            .await
            .map_err(|e| ErrorBody::new(ErrorCode::Internal, e.to_string()))?
            .map_err(|m| ErrorBody::new(ErrorCode::InvalidRequest, m))?;
            shared.apply_settings();
            info!("settings_changed", {"revision": snap["revision"], "client": client});
            shared.events.publish(
                "settings.changed",
                None,
                json!({"revision": snap["revision"]}),
            );
            Ok(snap)
        }
        method::JOB_CANCEL => {
            let p: JobGetParams = parse(req.params)?;
            let (job, canceled) = shared.registry(move |r| r.cancel_job(&p.job_id)).await?;
            if canceled {
                info!("job_canceled", {"jobId": job.job_id, "projectId": job.project_id, "client": client});
                crate::events::job_event(shared, "job.finished", &job);
            }
            Ok(json!({
                "job": job,
                "canceled": canceled,
                "reason": if canceled {
                    None
                } else if job.state == JobState::Running {
                    Some("running jobs complete; the preview engine has no cooperative cancellation")
                } else {
                    Some("job already finished")
                },
            }))
        }
        method::JOB_GET => {
            let p: JobGetParams = parse(req.params)?;
            let job = shared.registry(move |r| r.get_job(&p.job_id)).await?;
            to_value(job)
        }
        method::PROJECT_CONFIGURE => {
            let pid = require_project(&req)?;
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct ConfigureParams {
                #[serde(default)]
                watch_policy: Option<WatchPolicy>,
                #[serde(default)]
                backend_preference: Option<BackendPreference>,
            }
            let p: ConfigureParams = parse(req.params)?;
            if p.watch_policy.is_none() && p.backend_preference.is_none() {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidRequest,
                    "nothing to configure: give watchPolicy and/or backendPreference",
                ));
            }
            if let Some(pref) = p.backend_preference {
                // Refuse before changing anything.
                shared.backend_for(pref)?;
            }
            let (pid2, policy, pref) = (pid.clone(), p.watch_policy, p.backend_preference);
            let before = shared
                .registry({
                    let pid = pid.clone();
                    move |r| r.get_project(&pid)
                })
                .await?;
            let project = shared
                .registry(move |r| {
                    if let Some(policy) = policy {
                        r.configure_project(&pid2, policy)?;
                    }
                    match pref {
                        Some(pref) => r.set_backend_preference(&pid2, pref),
                        None => r.get_project(&pid2),
                    }
                })
                .await?;
            if pref.is_some_and(|p| p != before.backend_preference) {
                // Engine switch (WI-600): the revision moved, so work admitted
                // for the old engine is fenced off and a running job's result
                // is not published. The compatibility worker closes its handle
                // on the index; an owned project then reconciles with the new
                // engine under the new fence.
                shared
                    .engine
                    .release_store(&project.checkout_root, &project.index_dir)
                    .await;
                let p2 = project.project_id.clone();
                let queued = shared
                    .registry(move |r| {
                        let fence = r.fence(&p2)?;
                        let params = json!({"kind": "reconcile", "paths": [], "fence": fence});
                        r.admit_job(&p2, JobKind::Reconcile, &params, "backend-switch", None)
                    })
                    .await;
                match queued {
                    Ok(a) => {
                        shared.wake_worker.notify_one();
                        crate::events::job_event(shared, "job.queued", &a.job);
                    }
                    // Not owned: the next claim reconciles anyway.
                    Err(e) if e.code == ErrorCode::OwnerConflict => {}
                    Err(e) => return Err(e),
                }
                info!("backend_preference_changed", {"projectId": project.project_id, "from": before.backend_preference.as_str(), "to": project.backend_preference.as_str()});
            }
            info!("project_configured", {"projectId": project.project_id, "watchPolicy": project.watch_policy.as_str(), "revision": project.configuration_revision});
            shared.events.publish(
                "project.configured",
                Some(&project.project_id),
                json!({
                    "watchPolicy": project.watch_policy.as_str(),
                    "backendPreference": project.backend_preference.as_str(),
                    "configurationRevision": project.configuration_revision,
                }),
            );
            to_value(project)
        }
        method::SESSION_ACQUIRE => {
            let pid = require_project(&req)?;
            let p2 = pid.clone();
            shared.registry(move |r| r.get_project(&p2)).await?;
            let session_id = shared.watch.acquire_lease(&pid, client);
            to_value(wrongindex_contracts::SessionLease {
                session_id,
                project_id: pid,
                lease_ms: crate::watch::LEASE.as_millis() as u64,
                heartbeat_ms: crate::watch::HEARTBEAT.as_millis() as u64,
            })
        }
        method::SESSION_HEARTBEAT => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct SessionParams {
                session_id: String,
            }
            let p: SessionParams = parse(req.params)?;
            if !shared.watch.heartbeat(&p.session_id) {
                return Err(ErrorBody::new(
                    ErrorCode::InvalidRequest,
                    "unknown or expired session; acquire a new one",
                ));
            }
            Ok(json!({"ok": true, "leaseMs": crate::watch::LEASE.as_millis() as u64}))
        }
        method::SESSION_RELEASE => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct SessionParams {
                session_id: String,
            }
            let p: SessionParams = parse(req.params)?;
            let released = shared.watch.release_lease(&p.session_id);
            Ok(json!({"released": released.is_some(), "projectId": released}))
        }
        method::OWNERSHIP_ACQUIRE => {
            let pid = require_project(&req)?;
            let maintenance = req.params["maintenance"].as_bool().unwrap_or(false);
            to_value(crate::ownership::acquire(shared, &pid, maintenance).await?)
        }
        method::OWNERSHIP_RELEASE => {
            let pid = require_project(&req)?;
            to_value(crate::ownership::release(shared, &pid, false).await?)
        }
        method::OWNERSHIP_STATUS => {
            let pid = require_project(&req)?;
            let held = shared.owners.holds(&pid);
            let (status, journal) = shared
                .registry(move |r| Ok((r.ownership_status(&pid)?, r.ownership_journal(&pid, 20)?)))
                .await?;
            Ok(json!({ "status": status, "guardHeld": held, "journal": journal }))
        }
        method::JOB_LIST => {
            let p: JobListParams = parse(req.params)?;
            let project = p.project_id.or(req.project_id);
            let limit = p.limit.unwrap_or(50).min(MAX_PAGE_SIZE);
            let jobs = shared
                .registry(move |r| r.list_jobs(project.as_deref(), limit))
                .await?;
            Ok(json!({ "jobs": jobs }))
        }
        other => Err(ErrorBody::new(
            ErrorCode::MethodNotFound,
            format!("unknown method '{other}'"),
        )),
    }
}
