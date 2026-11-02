//! Maintenance rebuild (WI-603): an index the regular claim and writer
//! refuse is replaced only on explicit request, with the old file kept as a
//! backup; a newer schema is never replaced.

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
    let child = Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
        .arg("--data-root")
        .arg(home)
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

async fn try_job(c: &mut Client, pid: &str, params: Value) -> Value {
    run_job(c, pid, params).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_index_is_replaced_only_by_an_explicit_rebuild_and_kept_as_backup() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start(&base.join("home")).await;

    let root = base.join("cfg");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let index = base.join("cfg-index");
    std::fs::create_dir_all(&index).unwrap();
    let garbage = b"this is not a sqlite database".to_vec();
    std::fs::write(index.join("index.db"), &garbage).unwrap();
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

    // 1. The regular claim refuses it and leaves it alone.
    let err = c
        .call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("maintenance claim"), "{err}");
    assert_eq!(std::fs::read(index.join("index.db")).unwrap(), garbage);

    // 2. A maintenance claim admits it; a regular write still refuses it.
    c.call_raw(
        method::OWNERSHIP_ACQUIRE,
        Some(&pid),
        json!({"maintenance": true}),
        None,
    )
    .await
    .unwrap();
    let job = try_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(job["state"], "failed", "{job}");
    assert!(
        job["error"]["message"]
            .as_str()
            .unwrap()
            .contains("left untouched"),
        "{job}"
    );
    assert_eq!(std::fs::read(index.join("index.db")).unwrap(), garbage);

    // 3. The explicit rebuild replaces it and keeps the old bytes.
    let job = try_job(&mut c, &pid, json!({"kind": "rebuild"})).await;
    assert_eq!(job["state"], "succeeded", "{job}");
    let r = &job["result"]["native"];
    assert_eq!(r["outcome"], "replaced", "{job}");
    let backup = PathBuf::from(r["backup"].as_str().unwrap());
    assert!(
        backup
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("index.db.wrongindex-backup-"),
        "{backup:?}"
    );
    assert_eq!(std::fs::read(&backup).unwrap(), garbage);
    let i = wrongindex_storage::inspect_index(&index.join("index.db"));
    assert_eq!(i.verdict, wrongindex_storage::Verdict::Compatible, "{i:?}");

    // 4. From now on regular runs work (no-op: nothing changed).
    let job = try_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["native"]["outcome"], "unchanged", "{job}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_schema_is_never_admitted_even_for_maintenance() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start(&base.join("home")).await;
    let root = base.join("cfg");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let index = base.join("cfg-index");
    // A baseline index whose metadata claims a newer schema version.
    wrongindex_engine_free_index(&root, &index);
    let before = std::fs::read(index.join("index.db")).unwrap();
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
    let err = c
        .call_raw(
            method::OWNERSHIP_ACQUIRE,
            Some(&pid),
            json!({"maintenance": true}),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("newer WrongStack"), "{err}");
    assert_eq!(std::fs::read(index.join("index.db")).unwrap(), before);
}

/// A native index (through the daemon binary's child mode) with
/// `metadata.version` raised to 99.
fn wrongindex_engine_free_index(root: &Path, index: &Path) {
    std::fs::create_dir_all(index).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
        .arg("--native-rebuild")
        .arg(root)
        .arg(index)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let c = rusqlite::Connection::open(index.join("index.db")).unwrap();
    c.execute("UPDATE metadata SET value = '99' WHERE key = 'version'", [])
        .unwrap();
    c.pragma_update(None, "journal_mode", "DELETE").unwrap();
}
