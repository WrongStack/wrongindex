//! WI-600: the engine is chosen per project. Switching it moves the
//! configuration revision (work admitted for the old engine is fenced off),
//! queues a reconciliation with the new engine, and a choice the service
//! cannot honour is refused without changing anything.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongindex_contracts::method;
use wrongindex_ipc::{Client, DataRoot};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn long(p: &Path) -> PathBuf {
    let c = std::fs::canonicalize(p).unwrap();
    PathBuf::from(c.display().to_string().trim_start_matches(r"\\?\"))
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for e in std::fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to);
        } else {
            std::fs::copy(e.path(), to).unwrap();
        }
    }
}

async fn start_with(home: &Path, extra: &[&str]) -> (Daemon, Client) {
    let child = Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
        .arg("--data-root")
        .arg(home)
        .args(extra)
        .env_remove("WRONGINDEX_ENGINE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let daemon = Daemon(child);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(c) = Client::connect(&DataRoot::resolve(Some(home)).unwrap(), "t").await {
            return (daemon, c);
        }
        assert!(Instant::now() < deadline, "daemon not ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn run_job(c: &mut Client, pid: &str, params: Value) -> Value {
    let admitted = c
        .call_raw(method::INDEX_REQUEST, Some(pid), params, None)
        .await
        .unwrap();
    let id = admitted["job"]["jobId"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let job = c
            .call_raw(method::JOB_GET, None, json!({"jobId": id}), None)
            .await
            .unwrap();
        if matches!(job["state"].as_str(), Some("succeeded" | "failed")) {
            return job;
        }
        assert!(Instant::now() < deadline, "job did not finish: {job}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn register_and_claim(c: &mut Client, root: &Path, index: &Path) -> String {
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": index}),
            None,
        )
        .await
        .unwrap();
    let pid = reg["project"]["projectId"].as_str().unwrap().to_string();
    c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    pid
}

async fn configure(c: &mut Client, pid: &str, params: Value) -> Result<Value, String> {
    c.call_raw(method::PROJECT_CONFIGURE, Some(pid), params, None)
        .await
        .map_err(|e| e.to_string())
}

/// The newest job admitted by `client` for the project, once finished.
async fn switch_job(c: &mut Client, pid: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let jobs = c
            .call_raw(
                method::JOB_LIST,
                None,
                json!({"projectId": pid, "limit": 5}),
                None,
            )
            .await
            .unwrap();
        let job = jobs["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["kind"] == "reconcile")
            .cloned();
        if let Some(j) = job
            && matches!(j["state"].as_str(), Some("succeeded" | "failed"))
        {
            return j;
        }
        assert!(
            Instant::now() < deadline,
            "no finished reconcile job: {jobs}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_is_chosen_per_project_and_switching_reconciles() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    // No compatibility engine configured.
    let (_d, mut c) = start_with(&base.join("home"), &["--backend", "auto"]).await;
    let root = base.join("p");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let pid = register_and_claim(&mut c, &root, &base.join("idx")).await;
    let first = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(first["result"]["backend"], "native", "{first}");
    let rev0 = c
        .call_raw(method::PROJECT_GET, Some(&pid), json!({}), None)
        .await
        .unwrap()["project"]["configurationRevision"]
        .as_i64()
        .unwrap();

    // Switch to the compatibility worker: recorded, revision moved, a
    // reconciliation queued for it - which fails here (no engine), writing
    // nothing.
    let p = configure(&mut c, &pid, json!({"backendPreference": "legacy"}))
        .await
        .unwrap();
    assert_eq!(p["backendPreference"], "legacy");
    assert_eq!(p["configurationRevision"].as_i64().unwrap(), rev0 + 1);
    let job = switch_job(&mut c, &pid).await;
    assert_eq!(job["state"], "failed", "{job}");
    let gen_before = c
        .call_raw(method::PROJECT_GET, Some(&pid), json!({}), None)
        .await
        .unwrap()["generation"]
        .clone();
    assert_eq!(gen_before, 1);

    // Back to native: reconciled by the native engine under the new fence.
    configure(&mut c, &pid, json!({"backendPreference": "native"}))
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let jobs = c
            .call_raw(
                method::JOB_LIST,
                None,
                json!({"projectId": pid, "limit": 1}),
                None,
            )
            .await
            .unwrap();
        let j = &jobs["jobs"][0];
        if j["kind"] == "reconcile" && j["state"] == "succeeded" {
            assert_eq!(j["result"]["backend"], "native", "{j}");
            break;
        }
        assert!(Instant::now() < deadline, "{jobs}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Watch policy and engine are configured independently.
    let both = configure(
        &mut c,
        &pid,
        json!({"watchPolicy": "manual", "backendPreference": "auto"}),
    )
    .await
    .unwrap();
    assert_eq!(both["watchPolicy"], "manual");
    assert_eq!(both["backendPreference"], "auto");
    assert!(configure(&mut c, &pid, json!({})).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_choice_the_service_cannot_honour_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start_with(&base.join("home"), &["--backend", "native"]).await;
    let root = base.join("p");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let pid = register_and_claim(&mut c, &root, &base.join("idx")).await;
    let before = c
        .call_raw(method::PROJECT_GET, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let err = configure(&mut c, &pid, json!({"backendPreference": "legacy"}))
        .await
        .unwrap_err();
    assert!(err.contains("--backend native"), "{err}");
    let after = c
        .call_raw(method::PROJECT_GET, Some(&pid), json!({}), None)
        .await
        .unwrap();
    assert_eq!(after["project"], before["project"]);
}

async fn search(c: &mut Client, pid: &str) -> Value {
    c.call_raw(
        method::INDEX_QUERY,
        Some(pid),
        json!({"op": "search", "args": {"query": "greet", "limit": 5}}),
        None,
    )
    .await
    .unwrap()
}

/// With WrongStack's engine: the same queries give the same answers on
/// either engine, switched back and forth on one index.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine) and bun"]
async fn switching_engines_keeps_the_answers() {
    let engine = std::env::var("WRONGINDEX_TEST_ENGINE").expect("WRONGINDEX_TEST_ENGINE");
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start_with(
        &base.join("home"),
        &["--backend", "auto", "--engine", &engine],
    )
    .await;
    let root = base.join("p");
    copy_dir(&repo().join("tests/fixtures/projects/basic-ts"), &root);
    let pid = register_and_claim(&mut c, &root, &base.join("idx")).await;
    let first = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(first["state"], "succeeded", "{first}");
    let native = search(&mut c, &pid).await;
    assert_eq!(native["backend"], "native");
    configure(&mut c, &pid, json!({"backendPreference": "legacy"}))
        .await
        .unwrap();
    let job = switch_job(&mut c, &pid).await;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["backend"], "legacy-bun-worker", "{job}");
    let legacy = search(&mut c, &pid).await;
    assert_eq!(legacy["backend"], "legacy-bun-worker");
    assert_eq!(legacy["result"], native["result"]);
    configure(&mut c, &pid, json!({"backendPreference": "native"}))
        .await
        .unwrap();
    let back = search(&mut c, &pid).await;
    assert_eq!(back["backend"], "native");
    assert_eq!(back["result"], native["result"]);
}
