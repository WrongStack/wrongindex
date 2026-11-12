//! One service per data root, for as long as that service has work in hand,
//! and one that stays reachable and tidy:
//!
//! - a second service cannot start while the first is still draining (the
//!   endpoint used to be free from the moment the first stopped accepting);
//! - a service whose discovery files went missing publishes them again;
//! - a detached project is no longer watched, and a detach that is refused
//!   leaves the project owned;
//! - an empty project is current, and its first file is indexed;
//! - a checkout root that is slow to answer, or slow to open for watching,
//!   holds back no other project.
//!
//! The slow job is simulated with the debug-only `WRONGINDEX_NATIVE_STALL`
//! hook in the native child, the slow root with `WRONGINDEX_DEBUG_SLOW_ROOT`.

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

/// How long the slow project's native child stalls.
const STALL_SECS: u64 = 6;

fn spawn(home: &Path) -> Child {
    spawn_with(home, &[])
}

fn spawn_with(home: &Path, envs: &[(&str, &str)]) -> Child {
    Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
        .arg("--data-root")
        .arg(home)
        .args(["--drain-seconds", "60"])
        .env_remove("WRONGINDEX_ENGINE")
        .env("WRONGINDEX_NATIVE_STALL", format!("slowproj:{STALL_SECS}"))
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

async fn connect(home: &Path) -> Client {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(c) = Client::connect(&DataRoot::resolve(Some(home)).unwrap(), "t").await {
            return c;
        }
        assert!(Instant::now() < deadline, "service not ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn register(c: &mut Client, root: &Path, index: &Path, policy: &str) -> String {
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": index, "watchPolicy": policy}),
            None,
        )
        .await
        .unwrap();
    reg["project"]["projectId"].as_str().unwrap().to_string()
}

async fn status(c: &mut Client, pid: &str) -> Value {
    c.call_raw(method::PROJECT_GET, Some(pid), json!({}), None)
        .await
        .unwrap()
}

async fn job(c: &mut Client, id: &str) -> Value {
    c.call_raw(method::JOB_GET, None, json!({"jobId": id}), None)
        .await
        .unwrap()
}

async fn until(
    c: &mut Client,
    pid: &str,
    secs: u64,
    what: &str,
    ok: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let s = status(c, pid).await;
        if ok(&s) {
            return s;
        }
        assert!(Instant::now() < deadline, "{what}: {s}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn wait_exit(child: &mut Child, secs: u64) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return status.code();
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_service_cannot_start_while_the_first_drains() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let mut first = Daemon(spawn(&home));
    let mut c = connect(&home).await;
    let root = base.join("slowproj");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let pid = register(&mut c, &root, &base.join("index"), "manual").await;
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
    let job_id = admitted["job"]["jobId"].as_str().unwrap().to_string();
    let started = Instant::now();
    while job(&mut c, &job_id).await["state"] != "running" {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "job never started"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Stop: the service stops taking work and finishes the job in hand.
    c.call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
        .await
        .unwrap();
    drop(c);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // It takes no new session meanwhile: a client must not take it for a
    // running service (and, say, skip starting one).
    let refused = Client::connect(&DataRoot::resolve(Some(&home)).unwrap(), "t").await;
    match refused {
        Err(e) => assert_eq!(
            e.code(),
            Some(wrongindex_contracts::ErrorCode::ShuttingDown),
            "{e}"
        ),
        Ok(_) => panic!("a draining service accepted a new session"),
    }

    // A second service on the same data root, while the first still drains.
    let mut second = Daemon(spawn(&home));
    let code = wait_exit(&mut second.0, 4);
    assert!(
        first.0.try_wait().unwrap().is_none(),
        "the first service must still be draining for this to prove anything"
    );
    assert_eq!(
        code,
        Some(3),
        "a second service started beside one that was still draining"
    );

    // The first finishes its job and goes; only then can the next one start,
    // and it finds the job done, not interrupted.
    assert_eq!(
        wait_exit(&mut first.0, 60),
        Some(0),
        "the first service drains and exits"
    );
    let _third = Daemon(spawn(&home));
    let mut c = connect(&home).await;
    let done = job(&mut c, &job_id).await;
    assert_eq!(done["state"], "succeeded", "{done}");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_discovery_files_are_published_again() {
    let tmp = tempfile::tempdir().unwrap();
    let home = long(tmp.path()).join("home");
    let _d = Daemon(spawn(&home));
    let c = connect(&home).await;
    let instance = c.hello().daemon_instance_id.clone();
    drop(c);

    // A cleanup takes the runtime files of a running service.
    let root = DataRoot::resolve(Some(&home)).unwrap();
    std::fs::remove_file(root.endpoint_file()).unwrap();
    std::fs::remove_file(root.token_file()).unwrap();
    assert!(Client::connect(&root, "t").await.is_err());

    let c = connect(&home).await;
    assert_eq!(
        c.hello().daemon_instance_id,
        instance,
        "the same service, reachable again"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_detached_project_is_forgotten_and_a_refused_detach_keeps_ownership() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let _d = Daemon(spawn(&home));
    let mut c = connect(&home).await;
    let fixture = repo().join("tests/fixtures/projects/config-only");

    // 1. A detach refused for a running job leaves the project owned.
    let slow_root = base.join("slowproj");
    copy_dir(&fixture, &slow_root);
    let slow = register(&mut c, &slow_root, &base.join("slow-index"), "manual").await;
    c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&slow), json!({}), None)
        .await
        .unwrap();
    let admitted = c
        .call_raw(
            method::INDEX_REQUEST,
            Some(&slow),
            json!({"kind": "full"}),
            None,
        )
        .await
        .unwrap();
    let job_id = admitted["job"]["jobId"].as_str().unwrap().to_string();
    let refused = c
        .call_raw(method::PROJECT_DETACH, Some(&slow), json!({}), None)
        .await;
    assert!(
        refused.is_err(),
        "a project with an active job is not detached"
    );
    assert_eq!(status(&mut c, &slow).await["ownership"], "owned");
    let deadline = Instant::now() + Duration::from_secs(60);
    let done = loop {
        let j = job(&mut c, &job_id).await;
        if matches!(j["state"].as_str(), Some("succeeded" | "failed")) {
            break j;
        }
        assert!(Instant::now() < deadline, "{j}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(done["state"], "succeeded", "{done}");

    // 2. A watched project, detached: its watcher goes with it.
    let root = base.join("watched");
    copy_dir(&fixture, &root);
    let pid = register(&mut c, &root, &base.join("index"), "pinned").await;
    until(&mut c, &pid, 60, "never became current", |s| {
        s["freshness"] == "current" && s["watchHealth"] == "native"
    })
    .await;
    c.call_raw(method::PROJECT_DETACH, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let log = home.join("logs").join("wrongindexd.log");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if std::fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("watch_forgot_detached")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the detached project's watcher was kept"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // An edit in the old checkout is nobody's business any more.
    std::fs::write(root.join("after-detach.json"), "{\"after\": true}").unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !text.contains("watch_admission_failed") && !text.contains("watch_attach_failed"),
        "work was dispatched for a detached project"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_project_is_current_and_its_first_file_is_indexed() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let _d = Daemon(spawn(&home));
    let mut c = connect(&home).await;
    let root = base.join("fresh");
    std::fs::create_dir_all(&root).unwrap();
    let pid = register(&mut c, &root, &base.join("index"), "pinned").await;
    let s = until(
        &mut c,
        &pid,
        60,
        "an empty project never became current",
        |s| s["freshness"] == "current",
    )
    .await;
    assert_eq!(s["consecutiveFailures"], 0, "{s}");
    assert_eq!(s["parked"], false, "{s}");

    std::fs::write(root.join("first.json"), "{\"firstKey\": 1}").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let found = c
            .call_raw(
                method::INDEX_QUERY,
                Some(&pid),
                json!({"op": "search", "args": {"query": "firstKey"}}),
                None,
            )
            .await;
        if let Ok(v) = &found
            && v["result"]["results"]
                .as_array()
                .is_some_and(|r| r.iter().any(|h| h["name"] == "firstKey"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the first file was never indexed: {found:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// INV-05: a watched project with a file another program holds open is not
/// current. The file keeps its old rows, the project is degraded and says
/// why, and once the file can be read again it is indexed without any change
/// event (closing a handle makes none) and the project is current.
#[tokio::test(flavor = "multi_thread")]
async fn a_file_in_use_keeps_a_watched_project_degraded_until_it_is_indexed() {
    use std::os::windows::fs::OpenOptionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let _d = Daemon(spawn(&home));
    let mut c = connect(&home).await;
    let root = base.join("held");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("ok.json"), "{\"okKey\": 1}").unwrap();
    std::fs::write(root.join("held.json"), "{\"heldKey\": 1}").unwrap();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(root.join("held.json"))
        .unwrap();
    let pid = register(&mut c, &root, &base.join("index"), "pinned").await;
    let s = until(&mut c, &pid, 60, "never degraded", |s| {
        s["freshness"] == "degraded"
    })
    .await;
    assert!(
        s["lastError"]
            .as_str()
            .is_some_and(|e| e.contains("could not be read")),
        "{s}"
    );
    assert!(
        found(&mut c, &pid, "okKey").await,
        "the readable file is indexed"
    );
    assert!(!found(&mut c, &pid, "heldKey").await);

    drop(lock);
    let s = until(
        &mut c,
        &pid,
        90,
        "never current after the file was released",
        |s| s["freshness"] == "current",
    )
    .await;
    assert_eq!(s["lastError"], Value::Null, "{s}");
    assert!(
        found(&mut c, &pid, "heldKey").await,
        "the released file is indexed"
    );
}

/// One checkout root that takes long to answer (a network share that
/// stopped responding: one check of it took about 2.5 s here) holds back no
/// other project, and a root that answers that late counts as unavailable.
/// The watcher supervisor used to check every root itself, every second, so
/// one slow root delayed the edits of every project by its whole delay
/// (measured with a 3 s root: edits admitted after 3.6 s at the median
/// instead of 0.43 s).
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_checkout_root_does_not_hold_back_the_other_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    // Far beyond the bound below: a supervisor that waited for this root
    // would miss it on almost every edit.
    let _d = Daemon(spawn_with(
        &home,
        &[("WRONGINDEX_DEBUG_SLOW_ROOT", "slowroot:25000")],
    ));
    let mut c = connect(&home).await;
    let fast = base.join("fast");
    let slow = base.join("slowroot");
    for (dir, key) in [(&fast, "fastStart"), (&slow, "slowStart")] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("start.json"), format!("{{\"{key}\": 1}}")).unwrap();
    }
    let fast_id = register(&mut c, &fast, &base.join("index-fast"), "pinned").await;
    let slow_id = register(&mut c, &slow, &base.join("index-slow"), "pinned").await;
    until(
        &mut c,
        &fast_id,
        60,
        "the fast project never became current",
        |s| s["freshness"] == "current",
    )
    .await;
    for key in ["fastEditOne", "fastEditTwo", "fastEditThree"] {
        std::fs::write(fast.join("edit.json"), format!("{{\"{key}\": 1}}")).unwrap();
        let t0 = Instant::now();
        while !found(&mut c, &fast_id, key).await {
            assert!(
                t0.elapsed() < Duration::from_secs(8),
                "an edit of the fast project waited for the slow root"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    let s = until(
        &mut c,
        &slow_id,
        60,
        "the slow root was never reported",
        |s| s["watchHealth"] == "root_unavailable",
    )
    .await;
    assert_ne!(s["freshness"], "current", "{s}");
}

/// Opening a root for watching (the watcher's directory handle and change
/// request, the file-system probe, `.gitignore`) on a share that answers
/// slowly holds back no other project, and the slow one is watched once it
/// is open. The supervisor used to attach on its own task.
#[tokio::test(flavor = "multi_thread")]
async fn a_slow_watcher_attach_does_not_hold_back_the_other_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let _d = Daemon(spawn_with(
        &home,
        &[("WRONGINDEX_DEBUG_SLOW_ATTACH", "slowattach:20000")],
    ));
    let mut c = connect(&home).await;
    let fast = base.join("fast");
    let slow = base.join("slowattach");
    for (dir, key) in [(&fast, "fastStart"), (&slow, "slowStart")] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("start.json"), format!("{{\"{key}\": 1}}")).unwrap();
    }
    let fast_id = register(&mut c, &fast, &base.join("index-fast"), "pinned").await;
    until(
        &mut c,
        &fast_id,
        60,
        "the fast project never became current",
        |s| s["freshness"] == "current",
    )
    .await;
    // Registered now, so its attach overlaps the edits below.
    let slow_id = register(&mut c, &slow, &base.join("index-slow"), "pinned").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    for key in ["fastEditOne", "fastEditTwo", "fastEditThree"] {
        std::fs::write(fast.join("edit.json"), format!("{{\"{key}\": 1}}")).unwrap();
        let t0 = Instant::now();
        while !found(&mut c, &fast_id, key).await {
            assert!(
                t0.elapsed() < Duration::from_secs(8),
                "an edit of the fast project waited for the slow attach"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    // Meanwhile it says why it is not watched.
    let s = until(
        &mut c,
        &slow_id,
        30,
        "the slow attach was never reported",
        |s| {
            s["lastError"]
                .as_str()
                .is_some_and(|e| e.contains("has not opened for watching"))
        },
    )
    .await;
    assert_ne!(s["freshness"], "current", "{s}");
    until(
        &mut c,
        &slow_id,
        120,
        "the slow project was never watched",
        |s| s["freshness"] == "current" && s["watchHealth"] == "native",
    )
    .await;
    assert!(found(&mut c, &slow_id, "slowStart").await);
}

async fn found(c: &mut Client, pid: &str, q: &str) -> bool {
    c.call_raw(
        method::INDEX_QUERY,
        Some(pid),
        json!({"op": "search", "args": {"query": q}}),
        None,
    )
    .await
    .map(|v| {
        v["result"]["results"]
            .as_array()
            .is_some_and(|r| !r.is_empty())
    })
    .unwrap_or(false)
}

/// A restart takes back its own indexes without copying them again (a
/// full `VACUUM INTO` of every index at every start, which also pushed the
/// backups of real transfers out of the kept ones), but an index another
/// program wrote while the service was stopped is backed up first.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_backs_up_only_an_index_written_by_someone_else() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let root = base.join("proj");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let index = base.join("index");
    let backups = || {
        std::fs::read_dir(index.join(".wrongindex-backups"))
            .map(|d| d.count())
            .unwrap_or(0)
    };
    let stop = |mut c: Client, mut d: Daemon| async move {
        c.call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
            .await
            .unwrap();
        drop(c);
        assert_eq!(wait_exit(&mut d.0, 30), Some(0));
    };

    // Registered, claimed and indexed (the first claim of a new index has
    // nothing to back up).
    let d = Daemon(spawn(&home));
    let mut c = connect(&home).await;
    let pid = register(&mut c, &root, &index, "manual").await;
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
    let job_id = admitted["job"]["jobId"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    while job(&mut c, &job_id).await["state"] != "succeeded" {
        assert!(Instant::now() < deadline, "the first run never finished");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    stop(c, d).await;
    let after_first = backups();

    // Two restarts: the service's own index, untouched - no copy.
    for _ in 0..2 {
        let d = Daemon(spawn(&home));
        let mut c = connect(&home).await;
        until(&mut c, &pid, 30, "ownership not restored", |s| {
            s["ownership"] == "owned"
        })
        .await;
        stop(c, d).await;
    }
    assert_eq!(backups(), after_first, "a restart copied its own index");

    // Another program writes the index while the service is stopped.
    {
        let db = rusqlite::Connection::open(index.join("index.db")).unwrap();
        db.execute(
            "INSERT OR REPLACE INTO metadata(key, value) VALUES ('foreign_probe', '1')",
            [],
        )
        .unwrap();
    }
    let d = Daemon(spawn(&home));
    let mut c = connect(&home).await;
    until(&mut c, &pid, 30, "ownership not restored", |s| {
        s["ownership"] == "owned"
    })
    .await;
    assert_eq!(
        backups(),
        after_first + 1,
        "a foreign write was not backed up"
    );
    let newest = std::fs::read_dir(index.join(".wrongindex-backups"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .max()
        .unwrap();
    let probe: String = rusqlite::Connection::open(&newest)
        .unwrap()
        .query_row(
            "SELECT value FROM metadata WHERE key = 'foreign_probe'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(probe, "1", "the backup holds the foreign write");
    stop(c, d).await;
}
