//! M2 acceptance through the preview engine: policies, leases, the real
//! Windows watcher, debounce/scheduling, freshness, deletion, and restart
//! reconciliation. Ignored without an engine:
//!
//!   WRONGINDEX_TEST_ENGINE=C:/wrongindex-oracle/wrongstack-<sha> \
//!     cargo test -p wrongindexd --test watch -- --ignored

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
            .arg("--engine")
            .arg(std::env::var("WRONGINDEX_TEST_ENGINE").expect("WRONGINDEX_TEST_ENGINE"))
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
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine) and bun"]
async fn pinned_project_follows_edits_and_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let checkout = tmp.path().join("pinned");
    copy_dir(&fixture(), &checkout);
    let manual_checkout = tmp.path().join("manual");
    copy_dir(&fixture(), &manual_checkout);

    let (daemon, mut c) = start(&home).await;
    let pid = register(&mut c, &checkout, &tmp.path().join("idx-pinned"), "pinned").await;
    let manual = register(
        &mut c,
        &manual_checkout,
        &tmp.path().join("idx-manual"),
        "manual",
    )
    .await;

    // Pinned: claimed, attached, initial reconciliation, then current.
    let s = wait_for(&mut c, &pid, "initial current", 60, |s| {
        s["freshness"] == "current"
    })
    .await;
    assert_eq!(s["ownership"], "owned");
    assert_eq!(s["watchHealth"], "native");
    assert_eq!(s["generation"], 1, "{s}");
    assert!(
        search_names(&mut c, &pid, "greet")
            .await
            .contains(&"greet".to_string())
    );

    // Manual: never watched or claimed automatically.
    let m = status(&mut c, &manual).await;
    assert_eq!(m["watchHealth"], "off");
    assert_eq!(m["ownership"], "unowned");
    assert_eq!(m["freshness"], "unknown");

    // External edit -> debounced incremental run -> new generation.
    let greet = checkout.join("src").join("greet.ts");
    let original = std::fs::read_to_string(&greet).unwrap();
    std::fs::write(
        &greet,
        format!("{original}\nexport function watchedAdd(): number {{ return 1; }}\n"),
    )
    .unwrap();
    let started = Instant::now();
    let s = wait_for(&mut c, &pid, "edit generation", 30, |s| {
        s["generation"].as_i64() >= Some(2) && s["freshness"] == "current"
    })
    .await;
    let latency = started.elapsed();
    assert!(
        search_names(&mut c, &pid, "watchedAdd")
            .await
            .contains(&"watchedAdd".to_string()),
        "{s}"
    );
    eprintln!("edit -> current generation latency: {latency:?}");

    // New file, then deletion of a file: its symbols disappear.
    let extra = checkout.join("src").join("extra.ts");
    std::fs::write(&extra, "export function extraOnly(): void {}\n").unwrap();
    wait_for(&mut c, &pid, "new file", 30, |s| {
        s["generation"].as_i64() >= Some(3) && s["freshness"] == "current"
    })
    .await;
    assert!(
        search_names(&mut c, &pid, "extraOnly")
            .await
            .contains(&"extraOnly".to_string())
    );
    std::fs::remove_file(&extra).unwrap();
    wait_for(&mut c, &pid, "deletion", 30, |s| {
        s["generation"].as_i64() >= Some(4) && s["freshness"] == "current"
    })
    .await;
    assert!(
        !search_names(&mut c, &pid, "extraOnly")
            .await
            .contains(&"extraOnly".to_string())
    );

    // Touch without content change: no new generation (INV-11).
    let before = status(&mut c, &pid).await["generation"].as_i64().unwrap();
    std::fs::write(&greet, std::fs::read(&greet).unwrap()).unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let s = wait_for(&mut c, &pid, "touch settles", 30, |s| {
        s["freshness"] == "current"
    })
    .await;
    assert_eq!(
        s["generation"].as_i64().unwrap(),
        before,
        "touch must not publish"
    );

    // Restart with an edit made while the daemon is down: reconciliation
    // on reattach picks it up (INV-06).
    stop(c, daemon).await;
    std::fs::write(
        &greet,
        format!("{original}\nexport function whileDown(): number {{ return 2; }}\n"),
    )
    .unwrap();
    let (daemon, mut c) = start(&home).await;
    wait_for(&mut c, &pid, "restart reconcile", 60, |s| {
        s["freshness"] == "current"
    })
    .await;
    let names = search_names(&mut c, &pid, "whileDown").await;
    assert!(names.contains(&"whileDown".to_string()), "{names:?}");
    assert!(
        !search_names(&mut c, &pid, "watchedAdd")
            .await
            .contains(&"watchedAdd".to_string())
    );
    stop(c, daemon).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine) and bun"]
async fn session_lease_starts_watching_and_policy_change_stops_it() {
    let tmp = tempfile::tempdir().unwrap();
    let checkout = tmp.path().join("sess");
    copy_dir(&fixture(), &checkout);
    let (daemon, mut c) = start(&tmp.path().join("home")).await;
    let pid = register(&mut c, &checkout, &tmp.path().join("idx"), "session").await;

    // No lease: not watched.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(status(&mut c, &pid).await["watchHealth"], "off");

    let lease = c
        .call_raw(method::SESSION_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let sid = lease["sessionId"].as_str().unwrap().to_string();
    let s = wait_for(&mut c, &pid, "session watch", 60, |s| {
        s["freshness"] == "current"
    })
    .await;
    assert_eq!(s["sessions"], 1);
    c.call_raw(
        method::SESSION_HEARTBEAT,
        None,
        json!({"sessionId": sid}),
        None,
    )
    .await
    .unwrap();

    // Release keeps watching through the grace period.
    c.call_raw(
        method::SESSION_RELEASE,
        None,
        json!({"sessionId": sid}),
        None,
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let s = status(&mut c, &pid).await;
    assert_eq!(s["sessions"], 0);
    assert_eq!(s["watchHealth"], "native", "grace period keeps the watch");

    // Switching to manual bumps the revision and stops the watch.
    let p = c
        .call_raw(
            method::PROJECT_CONFIGURE,
            Some(&pid),
            json!({"watchPolicy": "manual"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(p["configurationRevision"], 2);
    wait_for(&mut c, &pid, "detach", 10, |s| s["watchHealth"] == "off").await;

    // An expired/unknown session cannot be revived.
    assert!(
        c.call_raw(
            method::SESSION_HEARTBEAT,
            None,
            json!({"sessionId": sid}),
            None
        )
        .await
        .is_err()
    );
    stop(c, daemon).await;
}
