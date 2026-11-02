//! WI-403: a native run reports its phases while it runs (`job.progress`
//! events, `progress` in the project status) and names the files it could
//! not read (over the 5 MiB limit) in its result.

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

async fn start(home: &Path) -> (Daemon, Client) {
    start_with(home, &["--backend", "auto"]).await
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

#[tokio::test(flavor = "multi_thread")]
async fn a_native_run_reports_phases_and_oversized_files() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let (_d, mut c) = start(&home).await;

    let root = base.join("proj");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    // Over the read limit: no rows, named in the result.
    let big = root.join("huge.json");
    let mut body = String::from("{\"items\": [");
    while body.len() < 5 * 1024 * 1024 + 1024 {
        body.push_str("\"0123456789abcdef0123456789abcdef\",");
    }
    body.push_str("\"end\"]}\n");
    std::fs::write(&big, body).unwrap();

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

    let snap = c
        .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
        .await
        .unwrap();
    let instance = snap["instanceId"].as_str().unwrap().to_string();
    let seq = snap["seq"].as_u64().unwrap();
    let (_ack, mut live) = Client::connect(&DataRoot::resolve(Some(&home)).unwrap(), "t")
        .await
        .unwrap()
        .subscribe(Some(seq), Some(&instance), Some(&pid))
        .await
        .unwrap();

    let job = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(job["state"], "succeeded", "{job}");
    let native = &job["result"]["native"];
    let oversized = native["oversizedFiles"].as_array().expect("oversizedFiles");
    assert_eq!(oversized.len(), 1, "{native}");
    assert!(oversized[0].as_str().unwrap().ends_with("huge.json"));

    // Phases arrived as events, in order, tagged with the job.
    let mut phases = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), live.next())
            .await
            .expect("event")
            .unwrap()
            .unwrap();
        if ev["kind"] == "job.progress" {
            assert_eq!(ev["data"]["jobId"], job["jobId"], "{ev}");
            phases.push(ev["data"]["phase"].as_str().unwrap().to_string());
        }
        if ev["kind"] == "job.finished" || Instant::now() > deadline {
            break;
        }
    }
    for want in ["discover", "parse", "write"] {
        assert!(
            phases.iter().any(|p| p == want),
            "{want} missing from {phases:?}"
        );
    }
    let pos = |p: &str| phases.iter().position(|x| x == p).unwrap();
    assert!(
        pos("discover") < pos("parse") && pos("parse") < pos("write"),
        "{phases:?}"
    );

    // Measured: the job and its native phases are counted.
    let m = c
        .call_raw(method::DAEMON_METRICS, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(m["jobs"]["full"]["succeeded"], 1, "{m}");
    assert!(
        m["nativePhases"]["discover"]["count"].as_u64() >= Some(1),
        "{m}"
    );
    assert_eq!(m["limits"]["maxIndexFileBytes"], 5 * 1024 * 1024);

    // Nothing is left behind once the job has finished.
    let status = c
        .call_raw(method::PROJECT_GET, Some(&pid), json!({}), None)
        .await
        .unwrap();
    assert!(status.get("progress").is_none(), "{status}");
    // The project records the engine that wrote it.
    assert_eq!(status["project"]["backend"], "native", "{status}");
}
