//! WI-005 / WI-407: a watched project with files deeper than Windows'
//! classic 260-character path limit is indexed, watched and served; an edit
//! of the deepest file is picked up by the watcher's targeted run. Native
//! backend: no engine needed.

#![cfg(windows)]

use std::path::Path;
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

#[tokio::test(flavor = "multi_thread")]
async fn files_past_260_characters_are_indexed_watched_and_served() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let checkout = tmp.path().join("proj");
    let deep = checkout
        .join("src")
        .join("alpha".repeat(12))
        .join("bravo".repeat(12))
        .join("charlie".repeat(9))
        .join("delta".repeat(12));
    std::fs::create_dir_all(&deep).unwrap();
    let file = deep.join("deep_module.ts");
    assert!(file.to_string_lossy().len() > 260, "{}", file.display());
    std::fs::write(
        &file,
        "export function deepGreeting(name: string): string {
  return name;
}
",
    )
    .unwrap();
    std::fs::write(
        checkout.join("src").join("main.ts"),
        "export const top = 1;
",
    )
    .unwrap();
    let (daemon, mut c) = start(&home).await;
    let pid = register(&mut c, &checkout, &tmp.path().join("idx"), "pinned").await;
    wait_for(&mut c, &pid, "initial current", 60, |s| {
        s["freshness"] == "current"
    })
    .await;
    assert!(
        search_names(&mut c, &pid, "deepGreeting")
            .await
            .contains(&"deepGreeting".to_string())
    );

    let before = status(&mut c, &pid).await["generation"].as_i64().unwrap();
    let original = std::fs::read_to_string(&file).unwrap();
    std::fs::write(
        &file,
        format!(
            "{original}export function deeperStill(): number {{ return 5; }}
"
        ),
    )
    .unwrap();
    wait_for(&mut c, &pid, "deep edit indexed", 60, |s| {
        s["generation"].as_i64() > Some(before) && s["freshness"] == "current"
    })
    .await;
    assert!(
        search_names(&mut c, &pid, "deeperStill")
            .await
            .contains(&"deeperStill".to_string()),
        "the edit of a file past 260 characters was not indexed"
    );
    let jobs = c
        .call_raw(
            method::JOB_LIST,
            Some(&pid),
            json!({"projectId": pid}),
            None,
        )
        .await
        .unwrap();
    let last = jobs["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["kind"] == "incremental" && j["state"] == "succeeded")
        .cloned()
        .unwrap_or_else(|| panic!("no watcher run: {jobs}"));
    assert_eq!(last["result"]["native"]["outcome"], "updated", "{last}");
    stop(c, daemon).await;
}
