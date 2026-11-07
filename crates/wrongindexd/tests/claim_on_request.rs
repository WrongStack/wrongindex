//! Claims that race (found by M05: `project add --watch pinned` followed at
//! once by `index --full --wait` was refused with OWNER_CONFLICT "claim
//! ownership first", because the supervisor claims pinned projects on its
//! next tick):
//!
//! - an index request for a project whose policy has the service own it
//!   (pinned) claims it, as the supervisor would;
//! - a manual project is still refused until it is claimed;
//! - concurrent claims of one project all succeed with one acquisition.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongindex_contracts::{ErrorCode, method};
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
    Client::connect(&DataRoot::resolve(Some(home)).unwrap(), "t")
        .await
        .unwrap()
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

/// A config-only project (indexed natively without any engine).
async fn register(c: &mut Client, base: &Path, name: &str, policy: &str) -> String {
    let root = base.join(name);
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": base.join(format!("{name}-index")), "watchPolicy": policy}),
            None,
        )
        .await
        .unwrap();
    reg["project"]["projectId"].as_str().unwrap().to_string()
}

async fn wait_job(c: &mut Client, admitted: &Value) -> Value {
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

async fn journal(c: &mut Client, pid: &str) -> Vec<String> {
    let s = c
        .call_raw(method::OWNERSHIP_STATUS, Some(pid), json!({}), None)
        .await
        .unwrap();
    s["journal"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["phase"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pinned_project_indexes_right_after_registration() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start(&base.join("home")).await;
    // Several in a row: each request races the supervisor's next tick.
    for n in 0..3 {
        let pid = register(&mut c, &base, &format!("pinned{n}"), "pinned").await;
        let admitted = c
            .call_raw(
                method::INDEX_REQUEST,
                Some(&pid),
                json!({"kind": "full"}),
                None,
            )
            .await
            .unwrap_or_else(|e| panic!("index request right after registration refused: {e}"));
        let job = wait_job(&mut c, &admitted).await;
        assert_eq!(job["state"], "succeeded", "{job}");
        let phases = journal(&mut c, &pid).await;
        assert!(!phases.iter().any(|p| p == "conflict"), "{phases:?}");
        assert_eq!(
            phases.iter().filter(|p| *p == "negotiating").count(),
            1,
            "one acquisition: {phases:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manual_project_is_refused_until_claimed() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start(&base.join("home")).await;
    let pid = register(&mut c, &base, "manual", "manual").await;
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
    assert!(journal(&mut c, &pid).await.is_empty(), "nothing claimed");
    c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let admitted = c
        .call_raw(
            method::INDEX_REQUEST,
            Some(&pid),
            json!({"kind": "full"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(wait_job(&mut c, &admitted).await["state"], "succeeded");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_claims_of_one_project_all_succeed_once() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let (_d, mut c) = start(&home).await;
    let pid = register(&mut c, &base, "contended", "manual").await;
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let (home, pid) = (home.clone(), pid.clone());
        tasks.push(tokio::spawn(async move {
            let mut c = connect(&home).await;
            c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
                .await
        }));
    }
    let mut epochs = Vec::new();
    for t in tasks {
        let status = t
            .await
            .unwrap()
            .unwrap_or_else(|e| panic!("a concurrent claim failed: {e}"));
        assert_eq!(status["state"], "owned", "{status}");
        epochs.push(status["ownerEpoch"].as_i64().unwrap());
    }
    assert!(epochs.windows(2).all(|w| w[0] == w[1]), "{epochs:?}");
    let phases = journal(&mut c, &pid).await;
    assert_eq!(
        phases.iter().filter(|p| *p == "negotiating").count(),
        1,
        "one acquisition: {phases:?}"
    );
    assert!(!phases.iter().any(|p| p == "conflict"), "{phases:?}");
}

/// Found by the v0.1.3 live test: a claim took over an index that
/// WrongStack's own index server had open and was writing. A claim is now
/// refused while another program has the index open, before any file is
/// created, and the program is named; once it lets go, the claim succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn a_claim_is_refused_while_another_program_has_the_index_open() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start(&base.join("home")).await;
    let pid = register(&mut c, &base, "held", "manual").await;
    let index_dir = base.join("held-index");
    // An index exists: claim, index, release.
    c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let admitted = c
        .call_raw(
            method::INDEX_REQUEST,
            Some(&pid),
            json!({"kind": "full"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(wait_job(&mut c, &admitted).await["state"], "succeeded");
    c.call_raw(method::OWNERSHIP_RELEASE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let listing = |d: &Path| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let backups =
        |d: &Path| std::fs::read_dir(d.join(".wrongindex-backups")).map_or(0, |r| r.count());
    let phases_before = journal(&mut c, &pid).await.len();
    let backups_before = backups(&index_dir);

    // This test process stands in for WrongStack: a connection in a read
    // transaction, as an index server or a session keeps one.
    let reader = rusqlite::Connection::open(index_dir.join("index.db")).unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM files;")
        .unwrap();
    let files_before = listing(&index_dir);
    for maintenance in [false, true] {
        let err = c
            .call_raw(
                method::OWNERSHIP_ACQUIRE,
                Some(&pid),
                json!({"maintenance": maintenance}),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some(ErrorCode::OwnerConflict), "{err}");
        let msg = err.to_string();
        assert!(
            msg.contains(&format!("pid {}", std::process::id()))
                && msg.contains("Nothing was changed"),
            "{msg}"
        );
    }
    // Refused before anything: no journal entry, no backup, no new file.
    assert_eq!(journal(&mut c, &pid).await.len(), phases_before);
    assert_eq!(backups(&index_dir), backups_before);
    assert_eq!(listing(&index_dir), files_before);
    let s = c
        .call_raw(method::OWNERSHIP_STATUS, Some(&pid), json!({}), None)
        .await
        .unwrap();
    assert_ne!(s["state"], "owned", "{s}");

    // Once it lets go, the claim goes through.
    reader.execute_batch("COMMIT;").unwrap();
    drop(reader);
    let status = c
        .call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    assert_eq!(status["state"], "owned", "{status}");
}
