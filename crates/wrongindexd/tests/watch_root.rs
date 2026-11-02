//! WI-207: a watched checkout root that disappears (moved away, unmounted)
//! never empties the index, and watching resumes once the root is back.
//! Native backend: no engine needed.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongindex_contracts::method;
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

#[tokio::test(flavor = "multi_thread")]
async fn a_vanished_root_keeps_the_index_and_watching_resumes_when_it_returns() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let checkout = tmp.path().join("proj");
    copy_dir(&fixture(), &checkout);
    let (daemon, mut c) = start(&home).await;
    let pid = register(&mut c, &checkout, &tmp.path().join("idx"), "pinned").await;
    wait_for(&mut c, &pid, "initial current", 60, |s| {
        s["freshness"] == "current"
    })
    .await;
    assert!(
        search_names(&mut c, &pid, "greet")
            .await
            .contains(&"greet".to_string())
    );

    // The root goes away (moved, unmounted, network drive gone).
    let away = tmp.path().join("proj-away");
    let mut moved = false;
    for _ in 0..50 {
        if std::fs::rename(&checkout, &away).is_ok() {
            moved = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(moved, "could not move the watched root away");
    let s = wait_for(&mut c, &pid, "root reported unavailable", 30, |s| {
        s["freshness"] != "current"
    })
    .await;
    eprintln!("while away: {s}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    // The index was not emptied by a run over a missing root.
    assert!(
        search_names(&mut c, &pid, "greet")
            .await
            .contains(&"greet".to_string()),
        "index emptied while the root was away"
    );

    // The root returns; an edit afterwards is indexed again.
    std::fs::rename(&away, &checkout).unwrap();
    let greet = checkout.join("src").join("greet.ts");
    wait_for(&mut c, &pid, "current after return", 60, |s| {
        s["freshness"] == "current" && s["watchHealth"] == "native"
    })
    .await;
    let before = status(&mut c, &pid).await["generation"].as_i64().unwrap();
    let original = std::fs::read_to_string(&greet).unwrap();
    std::fs::write(
        &greet,
        format!(
            "{original}
export function afterReturn(): number {{ return 3; }}
"
        ),
    )
    .unwrap();
    wait_for(&mut c, &pid, "edit after return", 60, |s| {
        s["generation"].as_i64() > Some(before) && s["freshness"] == "current"
    })
    .await;
    assert!(
        search_names(&mut c, &pid, "afterReturn")
            .await
            .contains(&"afterReturn".to_string())
    );
    stop(c, daemon).await;
}
