//! Disk pressure (architecture §9 "Disk full", indexing-pipeline §12), with
//! the volume's free space set through the daemon's debug-only
//! `WRONGINDEX_FAULT_FREE_BYTES_FILE` hook:
//!
//! * the volume going short is published (`resource.warning`, level `full`)
//!   and is part of `events.snapshot`;
//! * a watched edit is refused with `DISK_FULL` before anything is written,
//!   the project waits (`watchHealth: disk_full`, freshness `degraded`)
//!   without retries on a timer and without counting toward parking;
//! * an explicit index request is refused the same way;
//! * once there is room again, the warning clears and the pending edit is
//!   indexed by itself.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongindex_contracts::method;
use wrongindex_ipc::{Client, DataRoot, EventStream};

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

async fn start(home: &Path, free_file: &Path) -> (Daemon, Client) {
    let child = Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
        .arg("--data-root")
        .arg(home)
        .env_remove("WRONGINDEX_ENGINE")
        .env("WRONGINDEX_FAULT_FREE_BYTES_FILE", free_file)
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

/// Events until one matches; returns it.
async fn event_until(
    stream: &mut EventStream,
    what: &str,
    matches: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let ev = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap_or_else(|_| panic!("no {what} within 60 s"))
            .unwrap()
            .unwrap_or_else(|| panic!("stream ended before {what}"));
        if matches(&ev) {
            return ev;
        }
    }
}

async fn status(c: &mut Client, pid: &str) -> Value {
    c.call_raw(method::PROJECT_GET, Some(pid), json!({}), None)
        .await
        .unwrap()
}

async fn jobs(c: &mut Client, pid: &str) -> Vec<Value> {
    c.call_raw(
        method::JOB_LIST,
        None,
        json!({"projectId": pid, "limit": 100}),
        None,
    )
    .await
    .unwrap()["jobs"]
        .as_array()
        .unwrap()
        .clone()
}

const MIB: u64 = 1024 * 1024;

#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the free-space hook exists in debug builds only"
)]
async fn a_full_disk_holds_writes_until_there_is_room() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let free = base.join("free-bytes.txt");
    std::fs::write(&free, (100 * 1024 * MIB).to_string()).unwrap();
    let (_d, mut c) = start(&home, &free).await;
    let snap = c
        .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(snap["resources"], json!([]), "{snap}");
    let (_, mut stream) = Client::connect(&DataRoot::resolve(Some(&home)).unwrap(), "t")
        .await
        .unwrap()
        .subscribe(snap["seq"].as_u64(), snap["instanceId"].as_str(), None)
        .await
        .unwrap();

    let root = base.join("cfg");
    let index_dir = base.join("cfg-index");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": index_dir, "watchPolicy": "pinned"}),
            None,
        )
        .await
        .unwrap();
    let pid = reg["project"]["projectId"].as_str().unwrap().to_string();
    let current = event_until(&mut stream, "current", |e| {
        e["kind"] == "project.state" && e["projectId"] == pid && e["data"]["freshness"] == "current"
    })
    .await;
    let generation = current["data"]["generation"].as_i64().unwrap();
    let index_bytes = std::fs::read(index_dir.join("index.db")).unwrap();

    // 1. The volume goes short: published, and in the snapshot.
    std::fs::write(&free, (100 * MIB).to_string()).unwrap();
    let warning = event_until(&mut stream, "resource.warning full", |e| {
        e["kind"] == "resource.warning" && e["data"]["level"] == "full"
    })
    .await;
    let w = &warning["data"];
    assert_eq!(w["resource"], "disk", "{w}");
    assert_eq!(w["freeBytes"], 100 * MIB, "{w}");
    assert_eq!(w["fullBytes"], 256 * MIB, "{w}");
    assert!(w["volume"].as_str().unwrap().ends_with('\\'), "{w}");
    assert_eq!(w["projects"], json!([pid]), "{w}");
    // What WrongIndex keeps there: the index, the service's own files.
    let u = &w["usage"];
    assert!(u["indexBytes"].as_u64().unwrap() > 0, "{w}");
    assert!(u["serviceBytes"].as_u64().unwrap() > 0, "{w}");
    assert!(u["logBytes"].as_u64().unwrap() > 0, "{w}");
    for k in ["walBytes", "backupBytes", "stagingBytes"] {
        assert!(u[k].is_u64(), "{k}: {w}");
    }
    assert!(warning.get("projectId").is_none(), "{warning}");
    let snap = c
        .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(snap["resources"].as_array().unwrap().len(), 1, "{snap}");
    assert_eq!(snap["resources"][0]["level"], "full", "{snap}");

    // 2. A watched edit: refused before anything is written; the project
    //    waits for space.
    let jobs_before = jobs(&mut c, &pid).await.len();
    std::fs::write(root.join("extra.json"), r#"{"addedKey": 1}"#).unwrap();
    let waiting = event_until(&mut stream, "disk_full state", |e| {
        e["kind"] == "project.state"
            && e["projectId"] == pid
            && e["data"]["watchHealth"] == "disk_full"
    })
    .await;
    let d = &waiting["data"];
    assert_eq!(d["freshness"], "degraded", "{d}");
    assert_eq!(d["generation"], generation, "{d}");
    assert!(
        d["lastError"]
            .as_str()
            .unwrap()
            .starts_with("DISK_FULL: only 100 MB free on "),
        "{d}"
    );
    let s = status(&mut c, &pid).await;
    assert_eq!(s["lastJob"]["state"], "failed", "{s}");
    assert_eq!(s["lastJob"]["error"]["code"], "DISK_FULL", "{s}");
    assert_eq!(s["lastJob"]["error"]["retryable"], false, "{s}");
    assert!(s["pendingPaths"].as_u64().unwrap() >= 1, "{s}");
    assert_eq!(
        std::fs::read(index_dir.join("index.db")).unwrap(),
        index_bytes,
        "the index was written"
    );

    // No retries on a timer, and nothing counted toward parking.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let s = status(&mut c, &pid).await;
    assert_eq!(jobs(&mut c, &pid).await.len(), jobs_before + 1, "retried");
    assert_eq!(s["consecutiveFailures"], 0, "{s}");
    assert_eq!(s["parked"], false, "{s}");
    assert_eq!(s["watchHealth"], "disk_full", "{s}");

    // 3. An explicit request is refused the same way.
    let admitted = c
        .call_raw(
            method::INDEX_REQUEST,
            Some(&pid),
            json!({"kind": "full"}),
            None,
        )
        .await
        .unwrap();
    let id = admitted["job"]["jobId"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    let job = loop {
        let job = c
            .call_raw(method::JOB_GET, None, json!({"jobId": id}), None)
            .await
            .unwrap();
        if job["state"] == "failed" || job["state"] == "succeeded" {
            break job;
        }
        assert!(Instant::now() < deadline, "{job}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(job["state"], "failed", "{job}");
    assert_eq!(job["error"]["code"], "DISK_FULL", "{job}");

    // 4. Room again: the warning clears and the edit is indexed by itself.
    //    The wait is checked every second and volume levels every five, so
    //    either event can come first.
    std::fs::write(&free, (100 * 1024 * MIB).to_string()).unwrap();
    let (mut ok, mut back) = (None, None);
    while ok.is_none() || back.is_none() {
        let ev = event_until(&mut stream, "resource.warning ok and current", |e| {
            (e["kind"] == "resource.warning" && e["data"]["level"] == "ok")
                || (e["kind"] == "project.state"
                    && e["projectId"] == pid
                    && e["data"]["freshness"] == "current")
        })
        .await;
        if ev["kind"] == "resource.warning" {
            ok = Some(ev);
        } else {
            back = Some(ev);
        }
    }
    let ok = ok.unwrap();
    assert_eq!(ok["data"]["projects"], json!([pid]), "{ok}");
    let back = back.unwrap();
    let d = &back["data"];
    assert!(d["generation"].as_i64().unwrap() > generation, "{d}");
    assert_eq!(d["watchHealth"], "native", "{d}");
    assert_eq!(d["lastError"], Value::Null, "{d}");
    let s = status(&mut c, &pid).await;
    assert_eq!(s["pendingPaths"], 0, "{s}");
    assert_eq!(s["lastJob"]["state"], "succeeded", "{s}");
    let snap = c
        .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(snap["resources"], json!([]), "{snap}");
}
