//! WI-405: service settings are validated, persisted, applied live, and
//! survive a restart; pausing background indexing holds watcher work (the
//! project is never reported current while edits wait).

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
async fn settings_apply_live_persist_and_pause_holds_watcher_work() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let checkout = tmp.path().join("proj");
    copy_dir(&fixture(), &checkout);
    let (daemon, mut c) = start(&home).await;

    let snap = c
        .call_raw(method::SETTINGS_GET, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(snap["revision"], 0);
    assert_eq!(snap["settings"]["parallelJobs"], 2);

    // Refused changes leave everything as it was.
    for bad in [
        json!({"parallelJobs": 0}),
        json!({"unknownKey": 1}),
        json!({"watchDebounceMs": 5000, "watchMaxWaitMs": 600}),
    ] {
        assert!(
            c.call_raw(method::SETTINGS_SET, None, json!({"settings": bad}), None)
                .await
                .is_err()
        );
    }
    let stale = c
        .call_raw(
            method::SETTINGS_SET,
            None,
            json!({"settings": {"parallelJobs": 3}, "expectedRevision": 7}),
            None,
        )
        .await;
    assert!(stale.is_err());

    let pid = register(&mut c, &checkout, &tmp.path().join("idx"), "pinned").await;
    wait_for(&mut c, &pid, "initial current", 60, |s| {
        s["freshness"] == "current"
    })
    .await;

    // Pause: an edit waits, and the project is not reported current.
    let set = c
        .call_raw(
            method::SETTINGS_SET,
            None,
            json!({"settings": {"paused": true, "watchDebounceMs": 200}, "expectedRevision": 0}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(set["revision"], 1);
    let health = c
        .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(health["paused"], true);
    assert_eq!(health["settingsRevision"], 1);
    let before = status(&mut c, &pid).await["generation"].as_i64().unwrap();
    let greet = checkout.join("src").join("greet.ts");
    let original = std::fs::read_to_string(&greet).unwrap();
    std::fs::write(
        &greet,
        format!(
            "{original}
export function whilePaused(): number {{ return 4; }}
"
        ),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let s = status(&mut c, &pid).await;
    assert_eq!(s["generation"].as_i64().unwrap(), before, "{s}");
    assert_ne!(s["freshness"], "current", "{s}");

    // Resume: the waiting edit is indexed.
    c.call_raw(
        method::SETTINGS_SET,
        None,
        json!({"settings": {"paused": false}}),
        None,
    )
    .await
    .unwrap();
    wait_for(&mut c, &pid, "edit after resume", 60, |s| {
        s["generation"].as_i64() > Some(before) && s["freshness"] == "current"
    })
    .await;
    assert!(
        search_names(&mut c, &pid, "whilePaused")
            .await
            .contains(&"whilePaused".to_string())
    );

    // Persisted: a restarted service applies the same settings.
    stop(c, daemon).await;
    let (daemon, mut c) = start(&home).await;
    let snap = c
        .call_raw(method::SETTINGS_GET, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(snap["revision"], 2);
    assert_eq!(snap["settings"]["watchDebounceMs"], 200);
    assert_eq!(snap["settings"]["paused"], false);
    stop(c, daemon).await;
}
