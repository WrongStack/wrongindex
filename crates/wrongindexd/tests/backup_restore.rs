//! Ownership transfer backups and restore (compatibility §6.2, WI-308):
//! each claim of an existing index takes a consistent backup first, and a
//! `restore` job brings a backup back (keeping the replaced file).

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
async fn each_claim_backs_up_the_index_and_a_restore_job_brings_it_back() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start(&base.join("home")).await;

    let root = base.join("proj");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let index = base.join("proj-index");
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

    // No index yet: the claim has nothing to back up.
    c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let none = c
        .call_raw(method::INDEX_BACKUPS, Some(&pid), json!({}), None)
        .await
        .unwrap();
    assert_eq!(none["backups"].as_array().unwrap().len(), 0);
    let first = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(first["state"], "succeeded", "{first}");
    let first_stamp = first["result"]["graphStamp"].clone();

    // A second claim (after a release) backs the index up before writing.
    c.call_raw(method::OWNERSHIP_RELEASE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let listed = c
        .call_raw(method::INDEX_BACKUPS, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let backups = listed["backups"].as_array().unwrap();
    assert_eq!(backups.len(), 1, "{listed}");
    let name = backups[0]["name"].as_str().unwrap().to_string();

    // Change the project; the index moves on.
    let extra = root.join("extra.json");
    std::fs::write(
        &extra,
        "{\"added\": true}
",
    )
    .unwrap();
    let second = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(second["state"], "succeeded", "{second}");
    assert_ne!(second["result"]["graphStamp"], first_stamp);

    // A restore without a name is refused at admission.
    let refused = c
        .call_raw(
            method::INDEX_REQUEST,
            Some(&pid),
            json!({"kind": "restore"}),
            None,
        )
        .await;
    assert!(refused.is_err());

    let restored = run_job(&mut c, &pid, json!({"kind": "restore", "backup": name})).await;
    assert_eq!(restored["state"], "succeeded", "{restored}");
    assert_eq!(restored["result"]["graphStamp"], first_stamp, "{restored}");
    assert!(restored["result"]["previousKeptAs"].is_string());
    let after = c
        .call_raw(method::INDEX_BACKUPS, Some(&pid), json!({}), None)
        .await
        .unwrap();
    assert_eq!(after["backups"].as_array().unwrap().len(), 2, "{after}");

    // The restored index is served: the added file is gone from it.
    let stats = c
        .call_raw(
            method::INDEX_QUERY,
            Some(&pid),
            json!({"op": "stats", "args": {}}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(stats["generation"], restored["result"]["generation"]);

    // A name outside the backup directory is refused.
    let bad = run_job(
        &mut c,
        &pid,
        json!({"kind": "restore", "backup": r"..\index.db"}),
    )
    .await;
    assert_eq!(bad["state"], "failed", "{bad}");
}
