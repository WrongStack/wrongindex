//! Supervised compatibility worker (WI-300/301). Preview backend only.
//!
//! One long-lived Bun process runs WrongStack's in-process indexer and query
//! services (`packages/legacy-worker/worker.ts`). The daemon decides what runs
//! and when; the worker never elects itself or starts a project server.
//! Requests are serialized; each has a deadline, and a timeout or I/O failure
//! kills the process so the next call starts a fresh one.
//!
//! The worker runs in its own Job Object (`confine`): started suspended and
//! resumed only once inside it, so it and everything it starts share one
//! memory cap (the `workerMemoryMb` setting) and die together when the
//! worker is stopped or restarted. A request the cap stopped is reported as
//! such and the worker starts afresh on the next one.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
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
    /// Its job: dropping the worker ends its whole tree.
    job: Arc<crate::confine::Job>,
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
    /// Requests the memory cap stopped.
    pub memory_cap_hits: AtomicU64,
    /// The `workerMemoryMb` setting (0 = automatic).
    memory_mb: AtomicU64,
    /// The running worker's job, to change its cap live and report it.
    job: std::sync::Mutex<Weak<crate::confine::Job>>,
}

impl Engine {
    pub fn new(config: Option<EngineConfig>) -> Self {
        Self {
            config,
            worker: Mutex::new(None),
            next_id: AtomicU64::new(1),
            starts: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            memory_cap_hits: AtomicU64::new(0),
            memory_mb: AtomicU64::new(0),
            job: std::sync::Mutex::new(Weak::new()),
        }
    }

    fn running_job(&self) -> Option<Arc<crate::confine::Job>> {
        self.job.lock().ok().and_then(|j| j.upgrade())
    }

    /// Apply the `workerMemoryMb` setting: to the next worker, and to the
    /// running one at once.
    pub fn set_memory_mb(&self, mb: u64) {
        if self.memory_mb.swap(mb, Ordering::SeqCst) == mb {
            return;
        }
        if let Some(job) = self.running_job() {
            let cap = crate::confine::cap_bytes(mb);
            match job.set_cap(cap) {
                Ok(()) => info!("engine_memory_cap", {"bytes": cap}),
                Err(e) => warn!("engine_memory_cap_failed", {"error": e}),
            }
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
            Some(c) => {
                let job = self.running_job();
                json!({
                    "backend": "legacy-bun-worker",
                    "native": false,
                    "engineDir": c.engine_dir.display().to_string(),
                    "workerScript": c.worker_script.display().to_string(),
                    "bun": c.bun.display().to_string(),
                    "starts": self.starts.load(Ordering::Relaxed),
                    "failures": self.failures.load(Ordering::Relaxed),
                    "memoryCapBytes": job.as_ref().map_or_else(
                        || crate::confine::cap_bytes(self.memory_mb.load(Ordering::Relaxed)),
                        |j| j.cap(),
                    ),
                    "memoryCapHits": self.memory_cap_hits.load(Ordering::Relaxed),
                    // The running worker's tree; none while it is not running.
                    "peakMemoryBytes": job.map(|j| j.peak()),
                })
            }
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
        let cap = crate::confine::cap_bytes(self.memory_mb.load(Ordering::SeqCst));
        let (mut child, job) = crate::confine::spawn_confined(&mut cmd, cap).map_err(|e| {
            ErrorBody::new(
                ErrorCode::Internal,
                format!(
                    "cannot start compatibility worker ({}): {e}",
                    cfg.bun.display()
                ),
            )
        })?;
        let job = Arc::new(job);
        // Published before the setting is read again: a change made since
        // the cap was read is applied either here or by `set_memory_mb`.
        if let Ok(mut j) = self.job.lock() {
            *j = Arc::downgrade(&job);
        }
        let now = crate::confine::cap_bytes(self.memory_mb.load(Ordering::SeqCst));
        if now != cap
            && let Err(e) = job.set_cap(now)
        {
            warn!("engine_memory_cap_failed", {"error": e});
        }
        let stdin = child.stdin.take().ok_or_else(|| internal("worker stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| internal("worker stdout"))?;
        let mut w = Worker {
            child,
            job: job.clone(),
            stdin,
            lines: BufReader::new(stdout).lines(),
            hello: Value::Null,
        };
        self.starts.fetch_add(1, Ordering::Relaxed);
        let hello =
            Self::exchange(&mut w, 0, "hello", Value::Null, Duration::from_secs(60)).await?;
        info!("engine_started", {
            "pid": w.child.id(), "engine": hello["engine"]["kind"], "memoryCapBytes": w.job.cap(),
        });
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
        let mut result = Self::exchange(w, id, op, args, deadline).await;
        // Whatever the worker says or does not say: when its memory cap
        // stopped an allocation, that is why, and it starts afresh.
        let capped = result.is_err() && w.job.cap_hit();
        if let (true, Err(e)) = (capped, &result) {
            self.memory_cap_hits.fetch_add(1, Ordering::Relaxed);
            result = Err(memory_cap_error(op, w.job.cap(), e));
        }
        if let Err(e) = &result
            && (capped || e.code != ErrorCode::IndexDegraded)
        {
            self.failures.fetch_add(1, Ordering::Relaxed);
            warn!("engine_restart", {"op": op, "error": e.to_string(), "memoryCap": capped});
            if let Some(mut dead) = guard.take() {
                let _ = dead.child.kill().await;
            }
        }
        result
    }

    /// Make the worker close any store it holds for this index. Never starts
    /// a worker; if the running one cannot confirm, it is killed so the OS
    /// closes its handles. Called before ownership is handed back.
    pub async fn release_store(&self, project_root: &str, index_dir: &str) {
        let mut guard = self.worker.lock().await;
        let Some(w) = guard.as_mut() else { return };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let args = json!({"projectRoot": project_root, "indexDir": index_dir});
        if let Err(e) = Self::exchange(w, id, "release", args, Duration::from_secs(10)).await {
            warn!("engine_release_failed", {"error": e.to_string()});
            if let Some(mut dead) = guard.take() {
                let _ = dead.child.kill().await;
                let _ = dead.child.wait().await;
            }
        }
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

/// A request the worker's memory cap stopped. Not retryable as it is: the
/// same request needs the same memory.
fn memory_cap_error(op: &str, cap: usize, cause: &ErrorBody) -> ErrorBody {
    ErrorBody::new(
        ErrorCode::IndexDegraded,
        format!(
            "the compatibility worker reached its memory cap of {} MiB during '{op}' and was stopped; it starts afresh on the next request. Raise the workerMemoryMb setting if this project needs more ({})",
            cap >> 20,
            cause.message
        ),
    )
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

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// A stand-in for `worker.ts` speaking the same protocol, run by Node
    /// (or `WRONGINDEX_TEST_JS`): `hog` commits `mb` MiB (with `catch`, an
    /// allocation failure is answered as an error, as `worker.ts` answers
    /// any failure; without, it ends the process), `spawn` starts a
    /// long-lived detached child (outside the job Node keeps for the
    /// children it waits for, as a program that daemonizes would) and
    /// answers its pid.
    const FAKE_WORKER: &str = r#"
const readline = require('node:readline');
const { spawn } = require('node:child_process');
const say = (v) => process.stdout.write(JSON.stringify(v) + '\n');
readline.createInterface({ input: process.stdin }).on('line', (line) => {
  const { id, op, args } = JSON.parse(line);
  const reply = (result) => say({ id, ok: true, result });
  if (op === 'hello') return reply({ engine: { kind: 'fake' } });
  if (op === 'ping') return reply({ pid: process.pid });
  if (op === 'spawn') {
    const c = spawn(process.execPath, ['-e', 'setTimeout(() => {}, 120000)'], { stdio: 'ignore', detached: true });
    c.unref();
    return reply({ pid: c.pid });
  }
  if (op === 'hog') {
    const take = () => Buffer.alloc(args.mb * 1048576, 1).length;
    if (!args.catch) return reply({ held: take() });
    try {
      return reply({ held: take() });
    } catch (e) {
      return say({ id, ok: false, error: { name: e.name, message: e.message } });
    }
  }
  reply({});
});
"#;

    const MIB: u64 = 1 << 20;
    const T: Duration = Duration::from_secs(60);

    fn fake_engine(dir: &Path) -> Engine {
        let script = dir.join("fake-worker.cjs");
        std::fs::write(&script, FAKE_WORKER).unwrap();
        Engine::new(Some(EngineConfig {
            bun: std::env::var_os("WRONGINDEX_TEST_JS")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("node")),
            worker_script: script,
            engine_dir: dir.to_path_buf(),
        }))
    }

    async fn pid(e: &Engine) -> u64 {
        e.call("ping", Value::Null, T)
            .await
            .expect("needs node on PATH (or WRONGINDEX_TEST_JS)")["pid"]
            .as_u64()
            .unwrap()
    }

    /// Whether process `pid` ends within `wait`.
    fn ends_within(pid: u32, wait: Duration) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        // SAFETY: a handle this function opens and closes.
        unsafe {
            let h = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if h.is_null() {
                return true;
            }
            let ended = WaitForSingleObject(h, wait.as_millis() as u32) == WAIT_OBJECT_0;
            CloseHandle(h);
            ended
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_worker_is_capped_and_starts_afresh_when_the_cap_stops_it() {
        let tmp = tempfile::tempdir().unwrap();
        let e = fake_engine(tmp.path());
        e.set_memory_mb(512);
        // Within the cap.
        let r = e.call("hog", json!({"mb": 64}), T).await.unwrap();
        assert_eq!(r["held"], 64 * MIB);
        let first = pid(&e).await;
        let health = e.describe();
        assert_eq!(health["memoryCapBytes"], 512 * MIB);
        assert!(health["peakMemoryBytes"].as_u64().unwrap() >= 64 * MIB);
        assert_eq!(health["memoryCapHits"], 0);

        // Over it, and the worker dies of it.
        let err = e.call("hog", json!({"mb": 1024}), T).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::IndexDegraded);
        assert!(err.message.contains("memory cap of 512 MiB"), "{err}");
        assert!(err.message.contains("workerMemoryMb"), "{err}");
        assert!(!err.retryable);
        let second = pid(&e).await;
        assert_ne!(first, second);

        // Over it, and the worker answers with an error, as `worker.ts`
        // does: still the cap, and still a fresh worker afterwards.
        let err = e
            .call("hog", json!({"mb": 1024, "catch": true}), T)
            .await
            .unwrap_err();
        assert!(err.message.contains("memory cap of 512 MiB"), "{err}");
        assert!(err.message.contains("RangeError"), "{err}");
        let third = pid(&e).await;
        assert_ne!(second, third);
        assert_eq!(e.memory_cap_hits.load(Ordering::Relaxed), 2);
        assert_eq!(e.starts.load(Ordering::Relaxed), 3);

        // Raised while it runs: the same request fits, no restart.
        e.set_memory_mb(2048);
        assert_eq!(e.describe()["memoryCapBytes"], 2048 * MIB);
        let r = e.call("hog", json!({"mb": 1024}), T).await.unwrap();
        assert_eq!(r["held"], 1024 * MIB);
        assert_eq!(pid(&e).await, third);
        e.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn what_the_worker_starts_ends_with_it() {
        let tmp = tempfile::tempdir().unwrap();
        let e = fake_engine(tmp.path());
        let worker = pid(&e).await as u32;
        let grandchild = e.call("spawn", Value::Null, T).await.unwrap()["pid"]
            .as_u64()
            .unwrap() as u32;
        assert!(!ends_within(grandchild, Duration::from_millis(500)));
        // Stopped: the worker ends once its input closes; its detached
        // child would run on.
        e.shutdown().await;
        assert!(ends_within(worker, Duration::from_secs(10)));
        assert!(
            ends_within(grandchild, Duration::from_secs(10)),
            "the worker's child outlived it"
        );
    }
}
