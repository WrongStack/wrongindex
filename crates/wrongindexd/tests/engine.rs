//! Compatible-preview acceptance (WI-300..302): real daemon + supervised
//! WrongStack compatibility worker writing a real index, under ownership.
//!
//! Needs a WrongStack engine directory (a clean oracle from
//! `scripts/oracle/prepare-clean-wrongstack.ts`) in `WRONGINDEX_TEST_ENGINE`
//! and `bun` on PATH, so it is ignored by default:
//!
//!   WRONGINDEX_TEST_ENGINE=C:/wrongindex-oracle/wrongstack-<sha> \
//!     cargo test -p wrongindexd --test engine -- --ignored

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongindex_contracts::{ErrorCode, method};
use wrongindex_ipc::{Client, DataRoot};

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn engine_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("WRONGINDEX_TEST_ENGINE")
            .expect("set WRONGINDEX_TEST_ENGINE to a prepared WrongStack engine directory"),
    )
}

async fn start(root: &Path) -> (Daemon, Client) {
    let daemon = Daemon {
        child: Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
            .arg("--data-root")
            .arg(root)
            .arg("--engine")
            .arg(engine_dir())
            // basic-ts is now native under `auto`; this test covers the
            // supervised compatibility worker.
            .arg("--backend")
            .arg("legacy")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(c) = Client::connect(&DataRoot::resolve(Some(root)).unwrap(), "t").await {
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
    let job_id = admitted["job"]["jobId"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let job = c
            .call_raw(method::JOB_GET, None, json!({"jobId": job_id}), None)
            .await
            .unwrap();
        match job["state"].as_str() {
            Some("succeeded") | Some("failed") => return job,
            _ => {}
        }
        assert!(Instant::now() < deadline, "job did not finish: {job}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn search(c: &mut Client, pid: &str, q: &str) -> Value {
    c.call_raw(
        method::INDEX_QUERY,
        Some(pid),
        json!({"op": "search", "args": {"query": q, "limit": 5}}),
        None,
    )
    .await
    .unwrap()
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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine) and bun"]
async fn preview_engine_indexes_queries_and_fences() {
    let tmp = tempfile::tempdir().unwrap();
    let checkout = tmp.path().join("checkout");
    copy_dir(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/projects/basic-ts"),
        &checkout,
    );
    let index = tmp.path().join("index");
    let (daemon, mut c) = start(&tmp.path().join("home")).await;

    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": checkout, "indexDir": index}),
            None,
        )
        .await
        .unwrap();
    let pid = reg["project"]["projectId"].as_str().unwrap().to_string();

    // Writes and reads both require ownership.
    let err = c
        .call_raw(
            method::INDEX_REQUEST,
            Some(&pid),
            json!({"kind": "full"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::OwnerConflict), "{err}");
    let err = c
        .call_raw(
            method::INDEX_QUERY,
            Some(&pid),
            json!({"op": "stats"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::OwnerConflict));

    // A missing index may be claimed (WrongStack would create it too).
    c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap();

    // Full index through the supervised worker.
    let job = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["filesIndexed"], 4);
    assert_eq!(job["result"]["symbolsIndexed"], 14);
    assert_eq!(job["result"]["generation"], 1);

    // The written index is a compatible WrongStack index.
    let check = run_job(&mut c, &pid, json!({"kind": "check"})).await;
    assert_eq!(check["result"]["verdict"], "compatible", "{check}");

    let hits = search(&mut c, &pid, "greet").await;
    assert_eq!(hits["generation"], 1);
    assert_eq!(hits["result"]["results"][0]["name"], "greet", "{hits}");

    // Unchanged full run: no new generation (INV-11).
    let job = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(job["state"], "succeeded");
    assert_eq!(job["result"]["generation"], 1, "{job}");

    // Targeted edit: new generation, new symbol searchable.
    let greet = checkout.join("src").join("greet.ts");
    let mut src = std::fs::read_to_string(&greet).unwrap();
    src.push_str("\nexport function farewell(name: string): string { return `bye ${name}`; }\n");
    std::fs::write(&greet, src).unwrap();
    let job = run_job(
        &mut c,
        &pid,
        json!({"kind": "incremental", "paths": ["src/greet.ts"]}),
    )
    .await;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["generation"], 2, "{job}");
    let hits = search(&mut c, &pid, "farewell").await;
    assert_eq!(hits["generation"], 2);
    assert_eq!(hits["result"]["results"][0]["name"], "farewell", "{hits}");

    // Paths are confined to the checkout.
    let err = c
        .call_raw(
            method::INDEX_REQUEST,
            Some(&pid),
            json!({"kind": "incremental", "paths": ["../../outside.ts"]}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::InvalidRequest), "{err}");

    // The client cannot redirect a query to another index.
    let stats = c
        .call_raw(
            method::INDEX_QUERY,
            Some(&pid),
            json!({"op": "stats", "args": {"indexDir": "C:/elsewhere"}}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(stats["result"]["totalFiles"], 4, "{stats}");

    // Health discloses the non-native preview backend.
    let health = c
        .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
        .await
        .unwrap();
    let inventory = health["backendInventory"].to_string();
    assert!(inventory.contains("legacy-bun-worker"), "{inventory}");
    assert!(inventory.contains("\\\"native\\\":false"), "{inventory}");
    // The worker runs in its own capped job (automatic cap: at least
    // 2 GiB), whose peak it reports.
    let worker: Value = health["backendInventory"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| serde_json::from_str::<Value>(b.as_str()?).ok())
        .find(|b| b["backend"] == "legacy-bun-worker")
        .unwrap();
    assert!(
        worker["memoryCapBytes"].as_u64().unwrap() >= 2 << 30,
        "{worker}"
    );
    assert!(worker["peakMemoryBytes"].as_u64().unwrap() > 0, "{worker}");
    assert_eq!(worker["memoryCapHits"], 0, "{worker}");
    eprintln!("worker after indexing: {worker}");

    // Releasing ownership stops further writes.
    c.call_raw(method::OWNERSHIP_RELEASE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let err = c
        .call_raw(
            method::INDEX_REQUEST,
            Some(&pid),
            json!({"kind": "full"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::OwnerConflict));

    c.call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
        .await
        .unwrap();
    drop(daemon);
}
