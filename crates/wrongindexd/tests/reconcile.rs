//! Reconciliation beyond the watcher's events (indexing-pipeline §10):
//!
//! * a backup restored under a watched project is reconciled with the
//!   checkout: edits made after the backup are indexed again, and the project
//!   is not called current before that;
//! * the periodic reconciliation finds an edit whose watch events were lost,
//!   and publishes nothing while nothing changed.

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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let ev = tokio::time::timeout_at(deadline, stream.next())
            .await
            .unwrap_or_else(|_| panic!("no {what} within 60 s"))
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

/// Wait until a search finds `q`.
async fn found(c: &mut Client, pid: &str, q: &str, within: Duration) {
    let deadline = Instant::now() + within;
    while hits(c, pid, q).await == 0 {
        assert!(
            Instant::now() < deadline,
            "{q} not indexed: {}",
            status(c, pid).await
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Run a job to its end.
async fn run(c: &mut Client, pid: &str, m: &str, params: Value) -> Value {
    let admitted = c.call_raw(m, Some(pid), params, None).await.unwrap();
    let id = admitted["job"]["jobId"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let job = c
            .call_raw(method::JOB_GET, None, json!({"jobId": id}), None)
            .await
            .unwrap();
        if job["state"] == "failed" || job["state"] == "succeeded" {
            return job;
        }
        assert!(Instant::now() < deadline, "{job}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A small TypeScript project, registered pinned and indexed.
async fn project(c: &mut Client, stream: &mut EventStream, base: &Path) -> (String, PathBuf) {
    let root = base.join("demo");
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
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": base.join("idx"), "watchPolicy": "pinned"}),
            None,
        )
        .await
        .unwrap();
    let pid = reg["project"]["projectId"].as_str().unwrap().to_string();
    event_until(stream, "current", |e| {
        e["kind"] == "project.state" && e["projectId"] == pid && e["data"]["freshness"] == "current"
    })
    .await;
    (pid, root)
}

/// A backup is older than the checkout: restoring it under a watched project
/// must not leave the edits made since out of the index while the project
/// says it is current.
#[tokio::test(flavor = "multi_thread")]
async fn a_restored_index_catches_up_with_the_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let (_d, mut c) = start_with(&home, &[]).await;
    let mut stream = subscribe(&home, &mut c).await;
    let (pid, root) = project(&mut c, &mut stream, &base).await;

    // A backup of the index as it is now (taken when it is claimed).
    let configure = |policy: &'static str| json!({"watchPolicy": policy});
    c.call_raw(
        method::PROJECT_CONFIGURE,
        Some(&pid),
        configure("manual"),
        None,
    )
    .await
    .unwrap();
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
    let name = listed["backups"][0]["name"].as_str().unwrap().to_string();
    c.call_raw(
        method::PROJECT_CONFIGURE,
        Some(&pid),
        configure("pinned"),
        None,
    )
    .await
    .unwrap();
    current(&mut c, &pid).await;

    // An edit after the backup, indexed by the watcher.
    std::fs::write(
        root.join("src/m3.ts"),
        "export function addedAfterBackup(): number { return 3; }\n",
    )
    .unwrap();
    found(&mut c, &pid, "addedAfterBackup", Duration::from_secs(30)).await;

    // The backup goes back in: the edit is not in it.
    let restored = run(
        &mut c,
        &pid,
        method::INDEX_REQUEST,
        json!({"kind": "restore", "backup": name}),
    )
    .await;
    assert_eq!(restored["state"], "succeeded", "{restored}");
    // Never current while the edit is missing.
    let s = status(&mut c, &pid).await;
    let back = hits(&mut c, &pid, "addedAfterBackup").await;
    assert!(s["freshness"] != "current" || back > 0, "{s}");

    // Reconciled with the checkout: the edit is back, the project current.
    found(&mut c, &pid, "addedAfterBackup", Duration::from_secs(30)).await;
    assert_eq!(hits(&mut c, &pid, "fn_3").await, 0, "the old m3.ts stayed");
    current(&mut c, &pid).await;
}

/// An edit no watch event reported (the debug-only hook loses them) is found
/// by the periodic reconciliation (`reconcileMinutes`; seconds under the
/// debug-only hook). Until then the project stays current - no change is
/// known - and runs that find nothing publish nothing.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(not(debug_assertions), ignore = "the hooks exist in debug builds only")]
async fn a_missed_edit_is_found_by_the_periodic_reconciliation() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let flag = base.join("lose-events");
    let (_d, mut c) = start_with(
        &home,
        &[
            ("WRONGINDEX_DEBUG_RECONCILE_EVERY_SECS", "3"),
            ("WRONGINDEX_DEBUG_DROP_EVENTS_FILE", flag.to_str().unwrap()),
        ],
    )
    .await;
    let mut stream = subscribe(&home, &mut c).await;
    let (pid, root) = project(&mut c, &mut stream, &base).await;
    // The watcher reports (a file in an ignored directory: an event, no
    // run), so it is not taken for a silent one below.
    std::fs::create_dir_all(root.join("node_modules")).unwrap();
    std::fs::write(root.join("node_modules/seen.txt"), "x").unwrap();

    // Periodic runs come, find nothing, publish nothing.
    let generation = current(&mut c, &pid).await["generation"].clone();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let quiet = periodic(&mut c, &pid)
            .await
            .into_iter()
            .filter(|j| j["state"] == "succeeded")
            .count();
        if quiet >= 2 {
            break;
        }
        assert!(Instant::now() < deadline, "no periodic runs");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    for j in periodic(&mut c, &pid).await {
        assert_eq!(j["result"]["native"]["outcome"], "unchanged", "{j}");
    }
    let s = current(&mut c, &pid).await;
    assert_eq!(s["generation"], generation, "{s}");
    let verified = s["lastVerifiedMs"].as_i64().unwrap();

    // An edit whose events are lost.
    std::fs::write(&flag, "").unwrap();
    std::fs::write(
        root.join("src/m5.ts"),
        "export function missedByTheWatcher(): number { return 5; }\n",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    std::fs::remove_file(&flag).unwrap();
    let s = status(&mut c, &pid).await;
    assert_eq!(s["pendingPaths"], 0, "the watcher saw it: {s}");

    found(&mut c, &pid, "missedByTheWatcher", Duration::from_secs(30)).await;
    assert_eq!(hits(&mut c, &pid, "fn_5").await, 0, "the old m5.ts stayed");
    let s = current(&mut c, &pid).await;
    assert!(s["generation"].as_i64() > generation.as_i64(), "{s}");
    assert!(s["lastVerifiedMs"].as_i64().unwrap() > verified, "{s}");
    let finder = periodic(&mut c, &pid)
        .await
        .into_iter()
        .find(|j| j["result"]["native"]["outcome"] == "updated")
        .expect("no periodic run found the edit");
    assert_eq!(finder["result"]["native"]["changedFiles"], 1, "{finder}");
    let m = c
        .call_raw(method::DAEMON_METRICS, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(m["missedChanges"]["periodic"]["files"], 1, "{m}");
    assert!(
        m["reconciliations"]["periodic"].as_u64().unwrap() >= 3,
        "{m}"
    );
    assert_eq!(m["limits"]["reconcileMinutes"], 15, "{m}");
    // One lost edit from a watcher that does report is no reason to poll.
    assert_eq!(
        status(&mut c, &pid).await["watchHealth"],
        "native",
        "a reporting watcher was given up"
    );
}

/// A file rewritten with the same size and its old timestamp, its events
/// lost: Git's stat cache calls it unchanged, so a whole-project run does
/// not read it (as WrongStack's does not). The check reads every indexed
/// file and compares bytes; the file is indexed by the watcher's targeted
/// run.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(not(debug_assertions), ignore = "the hook exists in debug builds only")]
async fn a_rewrite_git_cannot_see_is_found_by_the_check() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let flag = base.join("lose-events");
    let (_d, mut c) = start_with(
        &home,
        &[("WRONGINDEX_DEBUG_DROP_EVENTS_FILE", flag.to_str().unwrap())],
    )
    .await;
    let mut stream = subscribe(&home, &mut c).await;

    // A committed checkout whose files are an hour old (never "racily
    // clean": Git trusts its stat data for them).
    let root = base.join("demo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"demo","version":"1.0.0"}"#,
    )
    .unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(3600);
    for i in 0..10 {
        let f = root.join(format!("src/m{i}.ts"));
        std::fs::write(
            &f,
            format!("export function fn_{i}(a: number): number {{ return a + {i}; }}\n"),
        )
        .unwrap();
        set_mtime(&f, old);
    }
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .current_dir(&root)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    // Size and modification time only, as on many file systems.
    git(&["config", "core.checkStat", "minimal"]);
    git(&["config", "core.trustctime", "false"]);
    git(&["add", "."]);
    git(&["commit", "-qm", "x"]);
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": base.join("idx"), "watchPolicy": "pinned"}),
            None,
        )
        .await
        .unwrap();
    let pid = reg["project"]["projectId"].as_str().unwrap().to_string();
    event_until(&mut stream, "current", |e| {
        e["kind"] == "project.state" && e["projectId"] == pid && e["data"]["freshness"] == "current"
    })
    .await;
    current(&mut c, &pid).await;

    // Same size, old timestamp, events lost.
    let m4 = root.join("src/m4.ts");
    let before = std::fs::read_to_string(&m4).unwrap();
    let after = before.replace("fn_4", "zz_4");
    assert_eq!(before.len(), after.len());
    std::fs::write(&flag, "").unwrap();
    std::fs::write(&m4, &after).unwrap();
    set_mtime(&m4, old);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    std::fs::remove_file(&flag).unwrap();

    // A whole-project run does not see it: Git calls the file clean and its
    // stored stamp matches, so it is not read.
    let full = run(&mut c, &pid, method::INDEX_REQUEST, json!({"kind": "full"})).await;
    assert_eq!(full["state"], "succeeded", "{full}");
    assert_eq!(full["result"]["native"]["outcome"], "unchanged", "{full}");
    assert_eq!(hits(&mut c, &pid, "zz_4").await, 0);

    // The check reads the bytes.
    let check = run(&mut c, &pid, method::INDEX_CHECK, json!({})).await;
    assert_eq!(check["state"], "succeeded", "{check}");
    let contents = &check["result"]["contents"];
    assert_eq!(contents["changed"], 1, "{check}");
    assert_eq!(contents["missing"], 0, "{check}");
    assert_eq!(contents["checked"], 11, "{check}");
    assert_eq!(contents["queued"], true, "{check}");
    assert!(
        contents["sample"][0].as_str().unwrap().ends_with("m4.ts"),
        "{check}"
    );
    found(&mut c, &pid, "zz_4", Duration::from_secs(30)).await;
    assert_eq!(hits(&mut c, &pid, "fn_4").await, 0, "the old m4.ts stayed");
    current(&mut c, &pid).await;
    let m = c
        .call_raw(method::DAEMON_METRICS, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(m["missedChanges"]["check"]["files"], 1, "{m}");

    // Checked again: everything agrees.
    let check = run(&mut c, &pid, method::INDEX_CHECK, json!({})).await;
    let contents = &check["result"]["contents"];
    assert_eq!(contents["changed"], 0, "{check}");
    assert!(contents.get("queued").is_none(), "{check}");
}

fn set_mtime(f: &Path, t: std::time::SystemTime) {
    std::fs::File::options()
        .write(true)
        .open(f)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

/// The project's periodic reconciliations, newest first.
async fn periodic(c: &mut Client, pid: &str) -> Vec<Value> {
    c.call_raw(
        method::JOB_LIST,
        None,
        json!({"projectId": pid, "limit": 100}),
        None,
    )
    .await
    .unwrap()["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|j| j["kind"] == "reconcile" && j["params"]["periodic"] == true)
        .cloned()
        .collect()
}

/// Wait until the project is watched, idle and current.
async fn current(c: &mut Client, pid: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let s = status(c, pid).await;
        if s["freshness"] == "current" && s["watchHealth"] == "native" {
            return s;
        }
        assert!(Instant::now() < deadline, "not current: {s}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
