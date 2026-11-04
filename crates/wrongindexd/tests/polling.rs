//! Polling, for a checkout whose file system reports no changes
//! (indexing-pipeline §2, §9):
//!
//! * a polled project is indexed and kept current: an edit, a new file and
//!   a deleted file reach the index; a change in an ignored directory does
//!   not cause a run; the status says it is polled and how;
//! * with polling off such a project is reported failed, at attach and when
//!   the setting changes while it is polled;
//! * polling chosen for a project (`watchMode: poll`, for a share whose
//!   server accepts the request but never reports): a local checkout that
//!   reports changes is polled anyway, keeps its mode across a restart, and
//!   goes back to the system's notifications with `auto`, no revision
//!   change either way;
//! * a real WSL share (`--ignored`, `WRONGINDEX_TEST_WSL_DISTRO`): the file
//!   system refuses change notifications, which is found without any hook,
//!   and edits made there are indexed.
//!
//! The first two use the debug hook `WRONGINDEX_DEBUG_FORCE_POLLING`, as if
//! the file system had refused.

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

fn long(p: &Path) -> PathBuf {
    let c = std::fs::canonicalize(p).unwrap();
    PathBuf::from(c.display().to_string().trim_start_matches(r"\\?\"))
}

async fn start_with(home: &Path, env: &[(&str, &str)]) -> (Daemon, Client) {
    let child = Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
        .arg("--data-root")
        .arg(home)
        .env_remove("WRONGINDEX_ENGINE")
        .env_remove("WRONGINDEX_DEBUG_FORCE_POLLING")
        .envs(env.iter().copied())
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

async fn subscribe(home: &Path, c: &mut Client) -> EventStream {
    let snap = c
        .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
        .await
        .unwrap();
    Client::connect(&DataRoot::resolve(Some(home)).unwrap(), "t")
        .await
        .unwrap()
        .subscribe(snap["seq"].as_u64(), snap["instanceId"].as_str(), None)
        .await
        .unwrap()
        .1
}

async fn event_until(
    stream: &mut EventStream,
    what: &str,
    matches: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    loop {
        let ev = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap_or_else(|_| panic!("no {what} within 90 s"))
            .unwrap()
            .unwrap_or_else(|| panic!("stream ended before {what}"));
        if matches(&ev) {
            return ev;
        }
    }
}

async fn status(c: &mut Client, pid: &str) -> Value {
    c.call_raw(method::PROJECT_GET, Some(pid), json!({}), None)
        .await
        .unwrap()
}

async fn settings(c: &mut Client, patch: Value) {
    c.call_raw(method::SETTINGS_SET, None, json!({"settings": patch}), None)
        .await
        .unwrap();
}

async fn hits(c: &mut Client, pid: &str, q: &str) -> u64 {
    c.call_raw(
        method::INDEX_QUERY,
        Some(pid),
        json!({"op": "search", "args": {"query": q, "limit": 3}}),
        None,
    )
    .await
    .unwrap()["result"]["total"]
        .as_u64()
        .unwrap_or(0)
}

/// Wait until a search for `q` finds `want` (0: gone, 1: there).
async fn until_hits(c: &mut Client, pid: &str, q: &str, want: bool, within: Duration) {
    let deadline = Instant::now() + within;
    while (hits(c, pid, q).await > 0) != want {
        assert!(
            Instant::now() < deadline,
            "{q} {}: {}",
            if want { "not indexed" } else { "still indexed" },
            status(c, pid).await
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn write_project(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"demo","version":"1.0.0"}"#,
    )
    .unwrap();
    for i in 0..10 {
        std::fs::write(
            root.join(format!("src/m{i}.ts")),
            format!("export function fn_{i}(a: number): number {{ return a + {i}; }}\n"),
        )
        .unwrap();
    }
}

/// Register `root` pinned with its index in `index_dir`.
async fn register(c: &mut Client, root: &Path, index_dir: &Path) -> String {
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": index_dir, "watchPolicy": "pinned"}),
            None,
        )
        .await
        .unwrap();
    reg["project"]["projectId"].as_str().unwrap().to_string()
}

async fn current(stream: &mut EventStream, pid: &str) {
    event_until(stream, "current", |e| {
        e["kind"] == "project.state" && e["projectId"] == pid && e["data"]["freshness"] == "current"
    })
    .await;
}

/// Edit, add and delete files in a polled checkout; they must reach the
/// index within a few intervals.
async fn edits_reach_the_index(c: &mut Client, pid: &str, root: &Path) {
    let within = Duration::from_secs(30);
    std::fs::write(
        root.join("src/m3.ts"),
        "export function editedWhilePolled(): number { return 3; }\n",
    )
    .unwrap();
    until_hits(c, pid, "editedWhilePolled", true, within).await;
    std::fs::write(
        root.join("src/added.ts"),
        "export function addedWhilePolled(): number { return 4; }\n",
    )
    .unwrap();
    until_hits(c, pid, "addedWhilePolled", true, within).await;
    std::fs::remove_file(root.join("src/m5.ts")).unwrap();
    until_hits(c, pid, "fn_5", false, within).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkout_without_change_notifications_is_polled() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let (_d, mut c) = start_with(&home, &[("WRONGINDEX_DEBUG_FORCE_POLLING", "1")]).await;
    settings(&mut c, json!({"pollIntervalSeconds": 1})).await;
    let mut stream = subscribe(&home, &mut c).await;
    let root = base.join("demo");
    write_project(&root);
    std::fs::create_dir_all(root.join("node_modules/dep")).unwrap();
    std::fs::write(root.join("node_modules/dep/i.ts"), "export const x = 1;\n").unwrap();
    let pid = register(&mut c, &root, &base.join("idx")).await;
    current(&mut stream, &pid).await;

    // How it is watched is visible.
    let s = status(&mut c, &pid).await;
    assert_eq!(s["watchHealth"], "polling", "{s}");
    assert_eq!(s["polling"]["everyMs"], 1000, "{s}");
    assert_eq!(s["polling"]["overdue"], false, "{s}");
    assert!(
        s["polling"]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("test hook")),
        "{s}"
    );
    // package.json and the ten sources; node_modules is not walked.
    assert_eq!(s["polling"]["files"], 11, "{s}");
    assert!(s["polling"]["lastPassMs"].as_i64().is_some(), "{s}");
    assert_eq!(s["generation"], 1, "{s}");

    edits_reach_the_index(&mut c, &pid, &root).await;
    current(&mut stream, &pid).await;

    // A change in an ignored directory: no run, nothing published.
    let before = status(&mut c, &pid).await;
    std::fs::write(root.join("node_modules/dep/i.ts"), "export const y = 2;\n").unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let after = status(&mut c, &pid).await;
    assert_eq!(after["generation"], before["generation"], "{after}");
    assert_eq!(
        after["lastJob"]["jobId"], before["lastJob"]["jobId"],
        "{after}"
    );
    assert_eq!(after["freshness"], "current", "{after}");
}

#[tokio::test(flavor = "multi_thread")]
async fn with_polling_off_such_a_checkout_is_reported_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let (_d, mut c) = start_with(&home, &[("WRONGINDEX_DEBUG_FORCE_POLLING", "1")]).await;
    settings(&mut c, json!({"pollIntervalSeconds": 1})).await;
    let mut stream = subscribe(&home, &mut c).await;
    let a = base.join("a");
    write_project(&a);
    let pa = register(&mut c, &a, &base.join("idx-a")).await;
    current(&mut stream, &pa).await;

    // Turned off while it is polled.
    settings(&mut c, json!({"pollIntervalSeconds": 0})).await;
    let ev = event_until(&mut stream, "a failed watch", |e| {
        e["kind"] == "project.state" && e["projectId"] == pa && e["data"]["watchHealth"] == "failed"
    })
    .await;
    assert_eq!(ev["data"]["freshness"], "unavailable", "{ev}");
    let s = status(&mut c, &pa).await;
    assert!(
        s["lastError"]
            .as_str()
            .is_some_and(|e| e.contains("pollIntervalSeconds 0")),
        "{s}"
    );
    assert!(s.get("polling").is_none(), "{s}");

    // Off when the project is attached.
    let b = base.join("b");
    write_project(&b);
    let pb = register(&mut c, &b, &base.join("idx-b")).await;
    let ev = event_until(&mut stream, "b failed", |e| {
        e["kind"] == "project.state" && e["projectId"] == pb && e["data"]["watchHealth"] == "failed"
    })
    .await;
    assert!(
        ev["data"]["lastError"]
            .as_str()
            .is_some_and(|e| e.contains("test hook") && e.contains("pollIntervalSeconds 0")),
        "{ev}"
    );
}

async fn configure(c: &mut Client, pid: &str, patch: Value) -> Value {
    c.call_raw(method::PROJECT_CONFIGURE, Some(pid), patch, None)
        .await
        .unwrap()
}

async fn health(stream: &mut EventStream, pid: &str, want: &'static str) {
    event_until(stream, want, |e| {
        e["kind"] == "project.state"
            && e["projectId"] == pid
            && e["data"]["watchHealth"] == want
            && e["data"]["freshness"] == "current"
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn polling_can_be_chosen_for_a_project() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let root = base.join("demo");
    write_project(&root);
    let (d, mut c) = start_with(&home, &[]).await;
    settings(&mut c, json!({"pollIntervalSeconds": 1})).await;
    let mut stream = subscribe(&home, &mut c).await;
    let pid = register(&mut c, &root, &base.join("idx")).await;
    health(&mut stream, &pid, "native").await;
    let s = status(&mut c, &pid).await;
    assert_eq!(s["project"]["watchMode"], "auto", "{s}");
    let revision = s["project"]["configurationRevision"].clone();

    // Polling chosen: watched anew by polling, though the folder reports.
    let p = configure(&mut c, &pid, json!({"watchMode": "poll"})).await;
    assert_eq!(p["watchMode"], "poll", "{p}");
    assert_eq!(p["configurationRevision"], revision, "{p}");
    health(&mut stream, &pid, "polling").await;
    let s = status(&mut c, &pid).await;
    assert!(
        s["polling"]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("watchMode poll")),
        "{s}"
    );
    std::fs::write(
        root.join("src/m2.ts"),
        "export function editedWhilePolledByChoice(): number { return 2; }\n",
    )
    .unwrap();
    until_hits(
        &mut c,
        &pid,
        "editedWhilePolledByChoice",
        true,
        Duration::from_secs(30),
    )
    .await;

    // Kept across a restart.
    drop(stream);
    drop(c);
    drop(d);
    let (_d, mut c) = start_with(&home, &[]).await;
    let mut stream = subscribe(&home, &mut c).await;
    health(&mut stream, &pid, "polling").await;

    // Back to the system's notifications.
    let p = configure(&mut c, &pid, json!({"watchMode": "auto"})).await;
    assert_eq!(p["configurationRevision"], revision, "{p}");
    health(&mut stream, &pid, "native").await;
    assert!(status(&mut c, &pid).await.get("polling").is_none());
    std::fs::write(
        root.join("src/m4.ts"),
        "export function editedWithNotifications(): number { return 4; }\n",
    )
    .unwrap();
    until_hits(
        &mut c,
        &pid,
        "editedWithNotifications",
        true,
        Duration::from_secs(30),
    )
    .await;

    // Anything else is refused.
    let bad = c
        .call_raw(
            method::PROJECT_CONFIGURE,
            Some(&pid),
            json!({"watchMode": "fast"}),
            None,
        )
        .await
        .unwrap_err();
    assert!(bad.to_string().contains("INVALID_REQUEST"), "{bad}");
}

/// A real file system without change notifications: a WSL distribution's
/// share. Run with `WRONGINDEX_TEST_WSL_DISTRO=<name>` and `--ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a WSL distribution (WRONGINDEX_TEST_WSL_DISTRO)"]
async fn a_wsl_share_is_polled_and_its_edits_are_indexed() {
    let distro = std::env::var("WRONGINDEX_TEST_WSL_DISTRO")
        .expect("WRONGINDEX_TEST_WSL_DISTRO names a WSL distribution");
    let name = format!("wi-poll-test-{}", std::process::id());
    let linux = format!("/tmp/{name}");
    let made = Command::new("wsl.exe")
        .args(["-d", &distro, "-e", "mkdir", "-p", &linux])
        .status()
        .unwrap();
    assert!(made.success());
    struct Cleanup(String, String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = Command::new("wsl.exe")
                .args(["-d", &self.0, "-e", "rm", "-rf", &self.1])
                .status();
        }
    }
    let _cleanup = Cleanup(distro.clone(), linux.clone());
    let root = PathBuf::from(format!(r"\\wsl.localhost\{distro}\tmp\{name}"));
    assert!(root.is_dir(), "{} is not reachable", root.display());
    write_project(&root);

    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let (_d, mut c) = start_with(&home, &[]).await;
    settings(&mut c, json!({"pollIntervalSeconds": 1})).await;
    let mut stream = subscribe(&home, &mut c).await;
    let pid = register(&mut c, &root, &base.join("idx")).await;
    current(&mut stream, &pid).await;
    let s = status(&mut c, &pid).await;
    assert_eq!(s["watchHealth"], "polling", "{s}");
    assert!(
        s["polling"]["reason"]
            .as_str()
            .is_some_and(|r| r.contains("Windows error")),
        "{s}"
    );
    edits_reach_the_index(&mut c, &pid, &root).await;
    // An edit made inside the distribution, not through the share.
    let edit = Command::new("wsl.exe")
        .args([
            "-d",
            &distro,
            "-e",
            "sh",
            "-c",
            &format!(
                "echo 'export function editedInLinux(): number {{ return 9; }}' > {linux}/src/m7.ts"
            ),
        ])
        .status()
        .unwrap();
    assert!(edit.success());
    until_hits(&mut c, &pid, "editedInLinux", true, Duration::from_secs(30)).await;
    let s = status(&mut c, &pid).await;
    assert_eq!(s["consecutiveFailures"], 0, "{s}");
}
