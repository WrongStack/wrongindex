//! WI-208: a project whose watcher runs keep failing is parked after
//! `watchParkAfterFailures` failures in a row - reported, never silent - and
//! an explicit job that succeeds closes the circuit so pending edits run at
//! once. Native backend: no engine needed.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongindex_contracts::method;
use wrongindex_ipc::{Client, DataRoot};
use wrongindex_registry::now_ms;

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn start(root: &Path) -> (Daemon, Client) {
    let daemon = Daemon {
        child: Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
            .arg("--data-root")
            .arg(root)
            .arg("--backend")
            .arg("native")
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

async fn stop(mut c: Client, daemon: Daemon) {
    c.call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
        .await
        .unwrap();
    let mut daemon = daemon;
    let deadline = Instant::now() + Duration::from_secs(20);
    while daemon.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "daemon did not stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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

async fn status(c: &mut Client, pid: &str) -> Value {
    c.call_raw(method::PROJECT_GET, Some(pid), json!({}), None)
        .await
        .unwrap()
}

/// Poll project status until `pred` holds.
async fn wait_for(
    c: &mut Client,
    pid: &str,
    what: &str,
    secs: u64,
    pred: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let s = status(c, pid).await;
        if pred(&s) {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {s}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn search_names(c: &mut Client, pid: &str, q: &str) -> Vec<String> {
    let v = c
        .call_raw(
            method::INDEX_QUERY,
            Some(pid),
            json!({"op": "search", "args": {"query": q, "limit": 10}}),
            None,
        )
        .await
        .unwrap();
    v["result"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

async fn register(c: &mut Client, checkout: &Path, index: &Path, policy: &str) -> String {
    let v = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": checkout, "indexDir": index, "watchPolicy": policy}),
            None,
        )
        .await
        .unwrap();
    v["project"]["projectId"].as_str().unwrap().to_string()
}

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/projects/basic-ts")
}

async fn jobs(c: &mut Client, pid: &str) -> Vec<Value> {
    c.call_raw(method::JOB_LIST, Some(pid), json!({"projectId": pid}), None)
        .await
        .unwrap()["jobs"]
        .as_array()
        .unwrap()
        .clone()
}

fn append(path: &Path, text: &str) {
    let original = std::fs::read_to_string(path).unwrap();
    std::fs::write(path, format!("{original}{text}")).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_watcher_failures_park_the_project_until_an_explicit_success() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let checkout = tmp.path().join("proj");
    let index = tmp.path().join("idx");
    copy_dir(&fixture(), &checkout);
    let (daemon, mut c) = start(&home).await;
    c.call_raw(
        method::SETTINGS_SET,
        None,
        json!({"settings": {"watchParkAfterFailures": 2}}),
        None,
    )
    .await
    .unwrap();
    let pid = register(&mut c, &checkout, &index, "pinned").await;
    wait_for(&mut c, &pid, "initial current", 60, |s| {
        s["freshness"] == "current"
    })
    .await;

    // The index gains a table the native writer does not know: every run
    // over it is refused (the file is left as it is).
    let db = rusqlite::Connection::open(index.join("index.db")).unwrap();
    db.execute_batch("CREATE TABLE foreign_extension(x)")
        .unwrap();
    drop(db);
    append(
        &checkout.join("src").join("greet.ts"),
        "
export function whileFailing(): number { return 1; }
",
    );
    let parked = wait_for(&mut c, &pid, "parked", 60, |s| s["parked"] == true).await;
    eprintln!("parked: {parked}");
    assert_eq!(parked["watchHealth"], "parked");
    assert_eq!(parked["freshness"], "degraded");
    assert_eq!(parked["consecutiveFailures"], 2);
    let next = parked["nextRetryMs"]
        .as_i64()
        .expect("nextRetryMs while parked");
    assert!(
        next > now_ms() + 10 * 60 * 1000,
        "parked probe is minutes away: {next}"
    );
    assert!(parked["lastError"].as_str().is_some_and(|e| !e.is_empty()));

    // Parked: more edits do not start more runs.
    let runs = jobs(&mut c, &pid).await.len();
    append(
        &checkout.join("src").join("runner.ts"),
        "
export function stillParked(): number { return 2; }
",
    );
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        jobs(&mut c, &pid).await.len(),
        runs,
        "a parked project ran again"
    );
    assert_eq!(status(&mut c, &pid).await["parked"], true);

    // The cause is removed and an explicit run succeeds: the circuit closes
    // and the pending edits are indexed without waiting for the probe.
    let db = rusqlite::Connection::open(index.join("index.db")).unwrap();
    db.execute_batch("DROP TABLE foreign_extension").unwrap();
    drop(db);
    let admitted = c
        .call_raw(
            method::INDEX_REQUEST,
            Some(&pid),
            json!({"kind": "full"}),
            None,
        )
        .await
        .unwrap();
    let job = admitted["job"]["jobId"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let j = c
            .call_raw(method::JOB_GET, None, json!({"jobId": job}), None)
            .await
            .unwrap();
        if j["state"] == "succeeded" {
            break;
        }
        assert!(j["state"] != "failed", "explicit run failed: {j}");
        assert!(
            Instant::now() < deadline,
            "explicit run did not finish: {j}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let healed = wait_for(&mut c, &pid, "current after the explicit run", 60, |s| {
        s["freshness"] == "current"
    })
    .await;
    assert_eq!(healed["parked"], false);
    assert_eq!(healed["consecutiveFailures"], 0);
    assert!(healed.get("nextRetryMs").is_none(), "{healed}");
    for name in ["whileFailing", "stillParked"] {
        assert!(
            search_names(&mut c, &pid, name)
                .await
                .contains(&name.to_string()),
            "{name} not indexed after the circuit closed"
        );
    }
    stop(c, daemon).await;
}
