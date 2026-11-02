//! WI-404: missed event sequences recover without false success. A
//! subscriber that stops reading falls behind the bounded event buffer while
//! a job fails; it is told so (`stream.gap`), and what it re-reads after the
//! gap (`events.snapshot`, `job.get`) reports the failure - the stream is a
//! hint, the daemon's state is the truth.

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

async fn connect(home: &Path) -> Client {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(c) = Client::connect(&DataRoot::resolve(Some(home)).unwrap(), "t").await {
            return c;
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

async fn flood(c: &mut Client, n: usize) {
    for i in 0..n {
        c.call_raw(
            method::SETTINGS_SET,
            None,
            json!({"settings": {"watchDebounceMs": 400 + (i % 2) as u64}}),
            None,
        )
        .await
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lagging_subscriber_is_told_and_the_snapshot_reports_the_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let child = Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
        .arg("--data-root")
        .arg(&home)
        .args(["--backend", "native"])
        .env_remove("WRONGINDEX_ENGINE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _d = Daemon(child);
    let mut c = connect(&home).await;
    let root = base.join("p");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": base.join("idx")}),
            None,
        )
        .await
        .unwrap();
    let pid = reg["project"]["projectId"].as_str().unwrap().to_string();
    c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let ok = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(ok["state"], "succeeded", "{ok}");

    // A subscriber that stops reading.
    let (ack, mut stream) = connect(&home).await.subscribe(None, None, None).await.unwrap();
    assert_eq!(ack["gap"], false);

    // Fill what the transport buffers, fail a job, then push the failure
    // out of the bounded event buffer.
    flood(&mut c, 600).await;
    std::fs::remove_dir_all(&root).unwrap();
    let failed = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(failed["state"], "failed", "{failed}");
    let failed_id = failed["jobId"].as_str().unwrap().to_string();
    flood(&mut c, 2400).await;

    // The subscriber reads on: it is told it missed events.
    let mut gap = None;
    let mut saw_failure = false;
    let mut saw_success_after_failure = false;
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let next = tokio::time::timeout(Duration::from_secs(5), stream.next()).await;
        let Ok(Ok(Some(ev))) = next else { break };
        if ev["kind"] == "stream.gap" {
            gap = Some(ev.clone());
            break;
        }
        if ev["data"]["jobId"] == failed_id.as_str() {
            saw_failure |= ev["kind"] == "job.failed";
            saw_success_after_failure |= ev["kind"] == "job.succeeded";
        }
    }
    let gap = gap.expect("no stream.gap for a subscriber that fell behind");
    assert!(gap["data"]["missed"].as_u64().unwrap_or(0) > 0, "{gap}");
    assert!(!saw_success_after_failure);

    // What it re-reads is the truth: the job failed, the project is not
    // reported current.
    let snap = c
        .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
        .await
        .unwrap();
    let project = snap["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["project"]["projectId"] == pid.as_str())
        .cloned()
        .unwrap_or_else(|| panic!("{snap}"));
    assert_ne!(project["freshness"], "current", "{project}");
    assert!(
        project["lastError"].as_str().is_some_and(|e| !e.is_empty())
            || project["freshness"] == "unavailable"
            || project["freshness"] == "degraded",
        "{project}"
    );
    let job = c
        .call_raw(method::JOB_GET, None, json!({"jobId": failed_id}), None)
        .await
        .unwrap();
    assert_eq!(job["state"], "failed", "{job}");
    eprintln!(
        "gap: missed {}; job.failed seen before the gap: {saw_failure}; snapshot freshness {}",
        gap["data"]["missed"], project["freshness"]
    );
}
