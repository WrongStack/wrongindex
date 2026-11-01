//! Supervised compatibility worker (WI-300/301). Preview backend only.
//!
//! One long-lived Bun process runs WrongStack's in-process indexer and query
//! services (`packages/legacy-worker/worker.ts`). The daemon decides what runs
//! and when; the worker never elects itself or starts a project server.
//! Requests are serialized; each has a deadline, and a timeout or I/O failure
//! kills the process so the next call starts a fresh one.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use wrongindex_contracts::{ErrorBody, ErrorCode};

use crate::{info, warn};

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub bun: PathBuf,
    pub worker_script: PathBuf,
    pub engine_dir: PathBuf,
}

impl EngineConfig {
    /// Explicit values, else `WRONGINDEX_ENGINE` / `WRONGINDEX_ENGINE_WORKER` /
    /// `WRONGINDEX_BUN`. No engine directory means no indexing backend.
    pub fn resolve(
        engine: Option<PathBuf>,
        worker: Option<PathBuf>,
        bun: Option<PathBuf>,
    ) -> Option<Self> {
        let env = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let engine_dir = engine.or_else(|| env("WRONGINDEX_ENGINE"))?;
        let worker_script = worker
            .or_else(|| env("WRONGINDEX_ENGINE_WORKER"))
            .or_else(default_worker_script)?;
        let bun = bun
            .or_else(|| env("WRONGINDEX_BUN"))
            .unwrap_or_else(|| PathBuf::from("bun"));
        Some(Self {
            bun,
            worker_script,
            engine_dir,
        })
    }
}

/// Installed layout `<exe dir>/legacy-worker/worker.ts`, else the source tree
/// layout `<repo>/packages/legacy-worker/worker.ts` (exe in `target/<profile>`).
fn default_worker_script() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [
        dir.join("legacy-worker").join("worker.ts"),
        dir.join("../../packages/legacy-worker/worker.ts"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    hello: Value,
}

pub struct Engine {
    config: Option<EngineConfig>,
    worker: Mutex<Option<Worker>>,
    next_id: AtomicU64,
    pub starts: AtomicU64,
    pub failures: AtomicU64,
}

impl Engine {
    pub fn new(config: Option<EngineConfig>) -> Self {
        Self {
            config,
            worker: Mutex::new(None),
            next_id: AtomicU64::new(1),
            starts: AtomicU64::new(0),
            failures: AtomicU64::new(0),
        }
    }

    pub fn configured(&self) -> bool {
        self.config.is_some()
    }

    pub fn describe(&self) -> Value {
        match &self.config {
            None => {
                json!({"backend": "none", "note": "no indexing engine configured; only read-only checks run"})
            }
            Some(c) => json!({
                "backend": "legacy-bun-worker",
                "native": false,
                "engineDir": c.engine_dir.display().to_string(),
                "workerScript": c.worker_script.display().to_string(),
                "bun": c.bun.display().to_string(),
                "starts": self.starts.load(Ordering::Relaxed),
                "failures": self.failures.load(Ordering::Relaxed),
            }),
        }
    }

    async fn spawn(&self, cfg: &EngineConfig) -> Result<Worker, ErrorBody> {
        let mut cmd = Command::new(&cfg.bun);
        cmd.arg(&cfg.worker_script)
            .arg("--engine")
            .arg(&cfg.engine_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = cmd.spawn().map_err(|e| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!(
                    "cannot start compatibility worker ({}): {e}",
                    cfg.bun.display()
                ),
            )
        })?;
        let stdin = child.stdin.take().ok_or_else(|| internal("worker stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| internal("worker stdout"))?;
        let mut w = Worker {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            hello: Value::Null,
        };
        self.starts.fetch_add(1, Ordering::Relaxed);
        let hello =
            Self::exchange(&mut w, 0, "hello", Value::Null, Duration::from_secs(60)).await?;
        info!("engine_started", {"pid": w.child.id(), "engine": hello["engine"]["kind"]});
        w.hello = hello;
        Ok(w)
    }

    async fn exchange(
        w: &mut Worker,
        id: u64,
        op: &str,
        args: Value,
        deadline: Duration,
    ) -> Result<Value, ErrorBody> {
        let line = json!({"id": id, "op": op, "args": args}).to_string() + "\n";
        let io = async {
            w.stdin.write_all(line.as_bytes()).await?;
            w.stdin.flush().await?;
            loop {
                match w.lines.next_line().await? {
                    None => return Err(std::io::Error::other("worker exited")),
                    Some(l) => {
                        let Ok(v) = serde_json::from_str::<Value>(&l) else {
                            continue;
                        };
                        if v["id"].as_u64() == Some(id) {
                            return Ok(v);
                        }
                    }
                }
            }
        };
        let v = match tokio::time::timeout(deadline, io).await {
            Err(_) => {
                return Err(ErrorBody::new(
                    ErrorCode::DeadlineExceeded,
                    format!(
                        "compatibility worker '{op}' exceeded {}s",
                        deadline.as_secs()
                    ),
                ));
            }
            Ok(Err(e)) => return Err(internal(&format!("compatibility worker: {e}"))),
            Ok(Ok(v)) => v,
        };
        if v["ok"].as_bool() == Some(true) {
            Ok(v["result"].clone())
        } else {
            Err(ErrorBody::new(
                ErrorCode::IndexDegraded,
                format!(
                    "{}: {}",
                    v["error"]["name"].as_str().unwrap_or("Error"),
                    v["error"]["message"].as_str().unwrap_or("worker error")
                ),
            ))
        }
    }

    /// Send one request. A transport failure or timeout restarts the worker
    /// on the next call; an engine-reported error leaves it running.
    pub async fn call(
        &self,
        op: &str,
        args: Value,
        deadline: Duration,
    ) -> Result<Value, ErrorBody> {
        let cfg = self.config.as_ref().ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::NotImplemented,
                "no indexing engine configured (start wrongindexd with --engine <WrongStack dir>)",
            )
        })?;
        let mut guard = self.worker.lock().await;
        if guard.is_none() {
            *guard = Some(self.spawn(cfg).await?);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let w = guard.as_mut().ok_or_else(|| internal("worker missing"))?;
        let result = Self::exchange(w, id, op, args, deadline).await;
        if let Err(e) = &result
            && e.code != ErrorCode::IndexDegraded
        {
            self.failures.fetch_add(1, Ordering::Relaxed);
            warn!("engine_restart", {"op": op, "error": e.to_string()});
            if let Some(mut dead) = guard.take() {
                let _ = dead.child.kill().await;
            }
        }
        result
    }

    pub async fn hello(&self) -> Option<Value> {
        self.worker.lock().await.as_ref().map(|w| w.hello.clone())
    }

    pub async fn shutdown(&self) {
        if let Some(mut w) = self.worker.lock().await.take() {
            drop(w.stdin);
            if tokio::time::timeout(Duration::from_secs(5), w.child.wait())
                .await
                .is_err()
            {
                let _ = w.child.kill().await;
            }
        }
    }
}

fn internal(msg: &str) -> ErrorBody {
    ErrorBody::new(ErrorCode::Internal, msg.to_string())
}

/// Absolute path of a project-relative or absolute `path` under `root`,
/// refusing anything that escapes the checkout.
pub fn contained_path(root: &Path, path: &str) -> Result<String, ErrorBody> {
    let joined = wrongindex_storage::wrongstack_paths::node_resolve(&root.join(path));
    let root = wrongindex_storage::wrongstack_paths::node_resolve(root);
    let (a, b) = (
        joined.to_string_lossy().to_lowercase(),
        root.to_string_lossy().to_lowercase(),
    );
    if a == b
        || !a.starts_with(&format!(
            "{}{}",
            b.trim_end_matches(['\\', '/']),
            std::path::MAIN_SEPARATOR
        ))
    {
        return Err(ErrorBody::new(
            ErrorCode::InvalidRequest,
            format!("path '{path}' is outside the project checkout"),
        ));
    }
    Ok(joined.to_string_lossy().into_owned())
}
