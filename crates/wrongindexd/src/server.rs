//! Connection handling, authentication, and method dispatch.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, watch};
use wrongindex_contracts::{
    AuthRequest, AuthResponse, BASELINE_PROFILE_ID, EndpointInfo, ErrorBody, ErrorCode, Freshness,
    Health, Hello, IndexRequestParams, JobAdmission, JobKind, JobState, Limits, MAX_FRAME_BYTES,
    Ownership, PROTOCOL_MAJOR, PROTOCOL_MINOR, ProjectStatus, RegisterProjectParams,
    RegisterProjectResult, Request, Response, method,
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
    connections: AtomicUsize,
    sqlite_version: String,
}

impl Shared {
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

    fn hello(&self, selected_profile: Option<String>) -> Hello {
        Hello {
            daemon_instance_id: self.instance_id.clone(),
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            supported_profile_ids: vec![BASELINE_PROFILE_ID.to_string()],
            selected_profile_id: selected_profile,
            capabilities: vec!["register".into(), "check".into(), "jobs".into()],
            limits: Limits {
                max_frame_bytes: MAX_FRAME_BYTES,
                max_page_size: MAX_PAGE_SIZE,
                max_inflight: 1,
            },
        }
    }
}

pub async fn run(cfg: Config) -> Result<(), StartError> {
    cfg.root
        .ensure_private_dirs()
        .with_context(|| format!("creating data root {}", cfg.root.path().display()))?;
    crate::log::init(&cfg.root.logs_dir());
    let endpoint = cfg.root.endpoint_name()?;

    // 1. Singleton: the first pipe instance is the OS-backed guard. A losing
    //    starter returns here without touching the registry or discovery.
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

    // 3. Credential + discovery.
    let instance_id = format!("d_{}", uuid::Uuid::new_v4().simple());
    let token = generate_token()?;
    let started_at_ms = now_ms();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let shared = Arc::new(Shared {
        registry: Arc::new(Mutex::new(registry)),
        instance_id: instance_id.clone(),
        token,
        started_at_ms,
        draining: AtomicBool::new(false),
        shutdown: shutdown_tx,
        wake_worker: Notify::new(),
        connections: AtomicUsize::new(0),
        sqlite_version: wrongindex_storage::sqlite_runtime().version,
    });
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

    let worker = tokio::spawn(crate::worker::run(shared.clone(), shutdown_rx.clone()));

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
    shared.draining.store(true, Ordering::SeqCst);
    let _ = shared.shutdown.send(true);
    if tokio::time::timeout(cfg.drain, worker).await.is_err() {
        warn!("drain_budget_exceeded", {"seconds": cfg.drain.as_secs()});
    }
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
            shared.draining.store(true, Ordering::SeqCst);
            let _ = shared.shutdown.send(true);
            return;
        }
    }
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
                backend_inventory: vec!["inspector:read-only (in-process)".into()],
            })
        }
        method::DAEMON_SHUTDOWN => {
            Ok(json!({ "draining": true, "instanceId": shared.instance_id }))
        }
        method::PROJECT_REGISTER => {
            let p: RegisterProjectParams = parse(req.params)?;
            let (project, created) = shared.registry(move |r| r.register_project(&p)).await?;
            info!("project_registered", {"projectId": project.project_id, "created": created, "client": client});
            to_value(RegisterProjectResult { project, created })
        }
        method::PROJECT_LIST => {
            let projects = shared.registry(|r| r.list_projects()).await?;
            Ok(json!({ "projects": projects }))
        }
        method::PROJECT_GET => {
            let pid = require_project(&req)?;
            let (project, last_job) = shared
                .registry(move |r| Ok((r.get_project(&pid)?, r.latest_job(&pid)?)))
                .await?;
            // M1 has no watcher or writer: report only what is observable.
            to_value(ProjectStatus {
                project,
                freshness: Freshness::Unknown,
                ownership: Ownership::Unowned,
                activity: if last_job
                    .as_ref()
                    .is_some_and(|j| j.state == JobState::Running)
                {
                    "checking".into()
                } else {
                    "dormant".into()
                },
                watch_health: "off".into(),
                last_job,
            })
        }
        method::PROJECT_DETACH => {
            let pid = require_project(&req)?;
            let project = shared.registry(move |r| r.detach_project(&pid)).await?;
            info!("project_detached", {"projectId": project.project_id, "client": client});
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
                }
            } else {
                parse(req.params.clone())?
            };
            if params.kind != JobKind::Check {
                return Err(ErrorBody::new(
                    ErrorCode::NotImplemented,
                    format!(
                        "'{}' jobs need an indexing backend; this preview build only performs read-only checks",
                        params.kind.as_str()
                    ),
                ));
            }
            let key = req.idempotency_key.clone();
            let client_name = client.to_string();
            let params_json = to_value(&params)?;
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
            to_value(JobAdmission {
                job: admission.job,
                deduplicated: admission.deduplicated,
            })
        }
        method::JOB_GET => {
            let p: JobGetParams = parse(req.params)?;
            let job = shared.registry(move |r| r.get_job(&p.job_id)).await?;
            to_value(job)
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
