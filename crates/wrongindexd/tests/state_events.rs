//! `project.state` events (architecture §8.3: project state and watch
//! health): a client follows a project from registration to current, an
//! edit through pending back to current with a new generation, and nothing
//! is published while nothing changes.

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

/// The next `project.state` event for `pid`, or `None` after `wait`.
async fn next_state(stream: &mut EventStream, pid: &str, wait: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let ev = tokio::time::timeout_at(deadline, stream.next())
            .await
            .ok()?
            .unwrap()?;
        if ev["kind"] == "project.state" && ev["projectId"] == pid {
            return Some(ev);
        }
    }
}

/// Events until one matches `done`; returns them all (in order).
async fn states_until(
    stream: &mut EventStream,
    pid: &str,
    done: impl Fn(&Value) -> bool,
) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let ev = next_state(stream, pid, Duration::from_secs(60))
            .await
            .unwrap_or_else(|| panic!("no matching project.state within 60 s; seen {seen:?}"));
        let stop = done(&ev["data"]);
        seen.push(ev);
        if stop {
            return seen;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn clients_follow_freshness_through_project_state_events() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let (_d, mut c) = start(&home).await;
    // A quiet period long enough that "pending" outlasts a supervisor tick
    // (states are checked once a second).
    c.call_raw(
        method::SETTINGS_SET,
        None,
        json!({"settings": {"watchDebounceMs": 3000, "watchMaxWaitMs": 6000}}),
        None,
    )
    .await
    .unwrap();
    let snap = c
        .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
        .await
        .unwrap();
    let (_, mut stream) = connect(&home)
        .await
        .subscribe(snap["seq"].as_u64(), snap["instanceId"].as_str(), None)
        .await
        .unwrap();

    let root = base.join("cfg");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": base.join("cfg-index"), "watchPolicy": "pinned"}),
            None,
        )
        .await
        .unwrap();
    let pid = reg["project"]["projectId"].as_str().unwrap().to_string();

    // 1. Registration -> claimed, watched, indexed: current.
    let first = states_until(&mut stream, &pid, |d| d["freshness"] == "current").await;
    let current = &first.last().unwrap()["data"];
    assert_eq!(current["watchHealth"], "native", "{current}");
    assert_eq!(current["ownership"], "owned", "{current}");
    let generation = current["generation"].as_i64().unwrap();
    assert!(generation >= 1, "{current}");

    // 2. Nothing changes: nothing is published.
    assert!(
        next_state(&mut stream, &pid, Duration::from_secs(3))
            .await
            .is_none(),
        "a state event without a change"
    );

    // 3. An edit: pending, then current with the next generation.
    std::fs::write(root.join("extra.json"), r#"{"addedKey": 1}"#).unwrap();
    let after = states_until(&mut stream, &pid, |d| {
        d["freshness"] == "current" && d["generation"].as_i64().unwrap() > generation
    })
    .await;
    let freshness: Vec<&str> = after
        .iter()
        .map(|e| e["data"]["freshness"].as_str().unwrap())
        .collect();
    assert_eq!(freshness.first(), Some(&"pending"), "{freshness:?}");
    assert!(
        after.iter().any(|e| e["data"]["pending"] == true),
        "{after:?}"
    );
    let seqs: Vec<u64> = after.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
}
