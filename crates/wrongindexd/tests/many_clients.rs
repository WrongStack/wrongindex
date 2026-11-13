//! One service, many clients at once (one machine, many projects, several
//! WrongStack sessions and agents searching in parallel):
//!
//! - searches beyond the query slots wait for one instead of failing;
//! - a client turned away because every connection is taken is told so
//!   (`QUEUE_SATURATED`), not left with a broken pipe.
//!
//! Slow queries are simulated with the debug-only
//! `WRONGINDEX_DEBUG_SLOW_QUERY_MS` hook.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongindex_contracts::{
    AuthRequest, AuthResponse, ErrorCode, MAX_FRAME_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR, method,
};
use wrongindex_ipc::{CallError, Client, DataRoot, Discovery, read_frame, write_frame};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn long(p: &Path) -> PathBuf {
    let c = std::fs::canonicalize(p).unwrap();
    PathBuf::from(c.display().to_string().trim_start_matches(r"\\?\"))
}

fn spawn(home: &Path, envs: &[(&str, &str)]) -> Daemon {
    Daemon(
        Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
            .arg("--data-root")
            .arg(home)
            .env_remove("WRONGINDEX_ENGINE")
            .envs(envs.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

async fn connect(home: &Path) -> Result<Client, CallError> {
    Client::connect(&DataRoot::resolve(Some(home)).unwrap(), "t").await
}

async fn ready(home: &Path) -> Client {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(c) = connect(home).await {
            return c;
        }
        assert!(Instant::now() < deadline, "service not ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A pinned project with one indexed file, current.
async fn project(c: &mut Client, base: &Path) -> String {
    let root = base.join("proj");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.json"), "{\"sharedKey\": 1}").unwrap();
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": base.join("index"), "watchPolicy": "pinned"}),
            None,
        )
        .await
        .unwrap();
    let pid = reg["project"]["projectId"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let s = c
            .call_raw(method::PROJECT_GET, Some(&pid), json!({}), None)
            .await
            .unwrap();
        if s["freshness"] == "current" {
            return pid;
        }
        assert!(Instant::now() < deadline, "never current: {s}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn search(c: &mut Client, pid: &str) -> Result<Value, CallError> {
    c.call_raw(
        method::INDEX_QUERY,
        Some(pid),
        json!({"op": "search", "args": {"query": "sharedKey"}}),
        None,
    )
    .await
}

/// Twelve searches at once with four query slots, each holding its slot
/// 300 ms: all of them answer. A search that found every slot taken used to
/// fail at once with `QUEUE_SATURATED` (here: eight of twelve), and the
/// WrongStack adapter does not retry it, so an agent's parallel searches
/// failed whenever another session was searching too.
#[tokio::test(flavor = "multi_thread")]
async fn searches_beyond_the_query_slots_wait_for_one() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let _d = spawn(&home, &[("WRONGINDEX_DEBUG_SLOW_QUERY_MS", "300")]);
    let mut c = ready(&home).await;
    let pid = project(&mut c, &base).await;
    let mut clients = Vec::new();
    for _ in 0..12 {
        clients.push(connect(&home).await.unwrap());
    }
    let started = Instant::now();
    let tasks: Vec<_> = clients
        .into_iter()
        .map(|mut c| {
            let pid = pid.clone();
            tokio::spawn(async move { search(&mut c, &pid).await })
        })
        .collect();
    let mut failed = Vec::new();
    for t in tasks {
        match t.await.unwrap() {
            Ok(v) => assert!(
                v["result"]["results"]
                    .as_array()
                    .is_some_and(|r| !r.is_empty()),
                "{v}"
            ),
            Err(e) => failed.push(e.to_string()),
        }
    }
    assert!(
        failed.is_empty(),
        "{} of 12 failed: {failed:?}",
        failed.len()
    );
    // Three rounds of four, not twelve in a row.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

/// The handshake of a client that writes its first frame `after` the pipe
/// opened, as a client on a loaded machine does.
async fn late_handshake(home: &Path, after: Duration) -> Result<AuthResponse, String> {
    let d = Discovery::load(&DataRoot::resolve(Some(home)).unwrap()).unwrap();
    let mut pipe = wrongindex_ipc::win::connect(&d.info.endpoint, Duration::from_secs(5))
        .await
        .map_err(|e| format!("open: {e}"))?;
    tokio::time::sleep(after).await;
    let auth = AuthRequest {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        token: d.token.clone(),
        client_name: "late".into(),
        client_version: "0".into(),
        supported_profile_ids: vec![wrongindex_contracts::BASELINE_PROFILE_ID.to_string()],
    };
    write_frame(&mut pipe, &auth, MAX_FRAME_BYTES)
        .await
        .map_err(|e| format!("write: {e}"))?;
    read_frame::<_, AuthResponse>(&mut pipe, MAX_FRAME_BYTES)
        .await
        .map_err(|e| format!("read: {e}"))?
        .ok_or_else(|| "closed".to_string())
}

/// With every connection taken, the next client is told why, with a code it
/// can act on, instead of a transport error (`os error 232`, "the pipe is
/// being closed") that reads as a service that is not running: the service
/// answered and closed before reading the client's first frame, so a client
/// that wrote it a moment later (565 of 1,200 concurrent sessions in one
/// process on this machine) found the pipe gone.
#[tokio::test(flavor = "multi_thread")]
async fn a_client_turned_away_for_too_many_connections_is_told_so() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let _d = spawn(&home, &[]);
    let mut held = vec![ready(&home).await];
    // The service takes 64 sessions; keep them all open.
    while held.len() < 64 {
        held.push(connect(&home).await.unwrap());
    }
    for after in [0, 300] {
        match late_handshake(&home, Duration::from_millis(after)).await {
            Ok(AuthResponse::Error { error }) => {
                assert_eq!(error.code, ErrorCode::QueueSaturated, "{error:?}");
                assert!(error.retryable, "{error:?}");
            }
            Ok(AuthResponse::Ok { .. }) => panic!("a 65th session was accepted"),
            Err(e) => panic!("turned away without a reason (first frame after {after} ms): {e}"),
        }
    }
    match connect(&home).await {
        Err(CallError::Remote(e)) => assert_eq!(e.code, ErrorCode::QueueSaturated, "{e:?}"),
        Err(e) => panic!("turned away without a reason: {e}"),
        Ok(_) => panic!("a 65th session was accepted"),
    }
    // Once one is closed, a new client gets in.
    drop(held.pop());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match connect(&home).await {
            Ok(_) => break,
            Err(e) => assert!(Instant::now() < deadline, "never let in again: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
