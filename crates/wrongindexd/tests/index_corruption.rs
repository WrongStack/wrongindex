//! A damaged index (architecture §9 "Corruption": stop writes, retain
//! diagnostic evidence, offer a verified rebuild):
//!
//! * garbage over pages of a watched project's `index.db`: the watcher's
//!   run fails with `DATABASE_CORRUPT`; the project shows `watchHealth:
//!   index_corrupt`, freshness `degraded`; nothing more is sent to the
//!   writer (no retries, nothing counted toward parking); a read answers
//!   `DATABASE_CORRUPT`; an explicit run is refused; the check names the
//!   damage; the file stays byte for byte as it was;
//! * a maintenance rebuild keeps the damaged file byte for byte, checks the
//!   new index, and the pending edit is indexed by itself;
//! * a damaged full-text index that still answers queries: the check finds
//!   it, writes stop, and answers say the index is damaged;
//! * the scheduled check (`integrityCheckHours`) finds it with nobody asking.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongindex_contracts::{ErrorCode, method};
use wrongindex_ipc::{CallError, Client, DataRoot, EventStream};

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

async fn start(home: &Path) -> (Daemon, Client) {
    start_with(home, &[]).await
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

async fn jobs(c: &mut Client, pid: &str) -> usize {
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
        .len()
}

async fn search(c: &mut Client, pid: &str, q: &str) -> Result<Value, CallError> {
    c.call_raw(
        method::INDEX_QUERY,
        Some(pid),
        json!({"op": "search", "args": {"query": q, "limit": 3}}),
        None,
    )
    .await
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

/// A project of 120 TypeScript files, registered pinned and indexed.
async fn project(
    c: &mut Client,
    stream: &mut EventStream,
    base: &Path,
) -> (String, PathBuf, PathBuf, i64) {
    let root = base.join("demo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{"name":"demo","version":"1.0.0"}"#,
    )
    .unwrap();
    for i in 0..60 {
        let body: String = (0..20)
            .map(|j| {
                format!("export function fn_{i}_{j}(a: number): number {{ return helper_{i}(a) + {j}; }}\n")
            })
            .collect();
        std::fs::write(
            root.join(format!("src/m{i}.ts")),
            format!("import {{ helper_{i} }} from './h{i}';\n{body}"),
        )
        .unwrap();
        std::fs::write(
            root.join(format!("src/h{i}.ts")),
            format!("export function helper_{i}(x: number): number {{ return x * {i}; }}\n"),
        )
        .unwrap();
    }
    let index_dir = base.join("idx");
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": index_dir, "watchPolicy": "pinned"}),
            None,
        )
        .await
        .unwrap();
    let pid = reg["project"]["projectId"].as_str().unwrap().to_string();
    let current = event_until(stream, "current", |e| {
        e["kind"] == "project.state" && e["projectId"] == pid && e["data"]["freshness"] == "current"
    })
    .await;
    // The log folded in (see `wal`): the damage below is in the file itself.
    let wal = index_dir.join("index.db-wal");
    let deadline = Instant::now() + Duration::from_secs(30);
    while std::fs::metadata(&wal).is_ok_and(|m| m.len() > 0) {
        assert!(Instant::now() < deadline, "the WAL stayed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    (
        pid,
        root,
        index_dir,
        current["data"]["generation"].as_i64().unwrap(),
    )
}

fn remote(e: CallError) -> wrongindex_contracts::ErrorBody {
    match e {
        CallError::Remote(b) => b,
        other => panic!("expected an error answer, got {other}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_damaged_index_is_left_alone_until_a_verified_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let (_d, mut c) = start(&home).await;
    let mut stream = subscribe(&home, &mut c).await;
    let (pid, root, index_dir, generation) = project(&mut c, &mut stream, &base).await;
    search(&mut c, &pid, "fn_3_4").await.unwrap();

    // Garbage over pages in the middle (the header intact).
    let db = index_dir.join("index.db");
    let mut bytes = std::fs::read(&db).unwrap();
    let page = 4096;
    let pages = bytes.len() / page;
    assert!(pages > 50, "{pages} pages");
    for (n, b) in bytes[page * (pages * 2 / 5)..page * (pages * 3 / 5)]
        .iter_mut()
        .enumerate()
    {
        *b = (n as u8).wrapping_mul(31).wrapping_add(7);
    }
    std::fs::write(&db, &bytes).unwrap();

    // 1. A watched edit: the writer finds the damage (DATABASE_CORRUPT, not
    //    "unsupported", which would hand the file to the compatibility
    //    worker), and the project stops.
    let jobs_before = jobs(&mut c, &pid).await;
    std::fs::write(
        root.join("src/m1.ts"),
        "export const addedAfterDamage = 1;\n",
    )
    .unwrap();
    let stopped = event_until(&mut stream, "index_corrupt state", |e| {
        e["kind"] == "project.state"
            && e["projectId"] == pid
            && e["data"]["watchHealth"] == "index_corrupt"
    })
    .await;
    assert_eq!(stopped["data"]["freshness"], "degraded", "{stopped}");
    assert!(
        stopped["data"]["lastError"]
            .as_str()
            .unwrap()
            .starts_with("DATABASE_CORRUPT: "),
        "{stopped}"
    );
    let s = status(&mut c, &pid).await;
    assert_eq!(s["lastJob"]["kind"], "incremental", "{s}");
    assert_eq!(s["lastJob"]["error"]["code"], "DATABASE_CORRUPT", "{s}");
    assert_eq!(s["lastJob"]["error"]["retryable"], false, "{s}");

    // 2. Nothing more is sent to the writer: no retries, nothing counted
    //    toward parking, the file as it was.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let s = status(&mut c, &pid).await;
    assert_eq!(jobs(&mut c, &pid).await, jobs_before + 1, "retried: {s}");
    assert!(s["pendingPaths"].as_u64().unwrap() >= 1, "{s}");
    assert_eq!(s["consecutiveFailures"], 0, "{s}");
    assert_eq!(s["parked"], false, "{s}");
    assert_eq!(s["watchHealth"], "index_corrupt", "{s}");
    assert_eq!(std::fs::read(&db).unwrap(), bytes, "the index was written");

    // A read says what is wrong and what to do.
    let e = remote(search(&mut c, &pid, "fn_3_4").await.unwrap_err());
    assert_eq!(e.code, ErrorCode::DatabaseCorrupt, "{e}");
    assert!(!e.retryable, "{e}");
    assert!(e.message.contains("Rebuild the index"), "{e}");

    // 3. The check names the damage, in one line.
    let check = run(&mut c, &pid, method::INDEX_CHECK, json!({})).await;
    let r = &check["result"];
    assert_eq!(r["verdict"], "corrupt", "{check}");
    assert_eq!(r["writeAdmissible"], false, "{check}");
    assert_eq!(r["integrity"]["ok"], false, "{check}");
    let s = status(&mut c, &pid).await;
    let last = s["lastError"].as_str().unwrap();
    assert!(last.contains("failed SQLite's check"), "{last}");
    assert!(!last.contains('\n'), "{last}");

    // 4. An explicit run is refused; nothing written.
    let job = run(&mut c, &pid, method::INDEX_REQUEST, json!({"kind": "full"})).await;
    assert_eq!(job["state"], "failed", "{job}");
    assert_eq!(job["error"]["code"], "DATABASE_CORRUPT", "{job}");
    assert!(
        job["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nothing was written"),
        "{job}"
    );
    assert_eq!(std::fs::read(&db).unwrap(), bytes, "the index was written");
    assert_eq!(
        status(&mut c, &pid).await["generation"],
        generation,
        "published"
    );

    // 5. The maintenance rebuild: the damaged file kept byte for byte, the
    //    new index checked, the pending edit indexed by itself.
    let job = run(
        &mut c,
        &pid,
        method::INDEX_REQUEST,
        json!({"kind": "rebuild"}),
    )
    .await;
    assert_eq!(job["state"], "succeeded", "{job}");
    let native = &job["result"]["native"];
    assert_eq!(native["integrity"]["ok"], true, "{job}");
    let kept = PathBuf::from(native["backup"].as_str().unwrap());
    assert_eq!(
        std::fs::read(&kept).unwrap(),
        bytes,
        "not kept byte for byte"
    );
    let back = event_until(&mut stream, "current again", |e| {
        e["kind"] == "project.state"
            && e["projectId"] == pid
            && e["data"]["freshness"] == "current"
            && e["data"]["pending"] == false
    })
    .await;
    assert_eq!(back["data"]["watchHealth"], "native", "{back}");
    assert_eq!(back["data"]["lastError"], Value::Null, "{back}");
    let deadline = Instant::now() + Duration::from_secs(30);
    let found = loop {
        let r = search(&mut c, &pid, "addedAfterDamage").await.unwrap();
        if r["result"]["total"].as_u64().unwrap_or(0) > 0 {
            break r;
        }
        assert!(Instant::now() < deadline, "the edit was not indexed: {r}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(found.get("indexCorrupt").is_none(), "{found}");
    let check = run(&mut c, &pid, method::INDEX_CHECK, json!({})).await;
    assert_eq!(check["result"]["verdict"], "compatible", "{check}");
    assert_eq!(check["result"]["integrity"]["ok"], true, "{check}");
}

/// A full-text index that no longer matches its content still answers
/// queries; SQLite's check finds it.
#[tokio::test(flavor = "multi_thread")]
async fn a_damaged_full_text_index_is_found_by_the_check() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let (_d, mut c) = start(&home).await;
    let mut stream = subscribe(&home, &mut c).await;
    let (pid, _root, index_dir, _) = project(&mut c, &mut stream, &base).await;
    {
        // A throw-away fixture: written behind the service's back on purpose.
        let conn = rusqlite::Connection::open(index_dir.join("index.db")).unwrap();
        conn.execute(
            "UPDATE symbols_fts_content SET c0 = 'nothing like it' WHERE id = (SELECT min(id) FROM symbols_fts_content)",
            [],
        )
        .unwrap();
    }
    let r = search(&mut c, &pid, "fn_3_4").await.unwrap();
    assert!(r["result"]["total"].as_u64().unwrap() > 0, "{r}");
    assert!(r.get("indexCorrupt").is_none(), "{r}");

    let check = run(&mut c, &pid, method::INDEX_CHECK, json!({})).await;
    assert_eq!(check["result"]["verdict"], "corrupt", "{check}");
    let problems = check["result"]["integrity"]["problems"].to_string();
    assert!(problems.contains("symbols_fts"), "{check}");
    event_until(&mut stream, "index_corrupt state", |e| {
        e["kind"] == "project.state"
            && e["projectId"] == pid
            && e["data"]["watchHealth"] == "index_corrupt"
    })
    .await;
    // Answers still come, saying the index is damaged.
    let r = search(&mut c, &pid, "fn_3_4").await.unwrap();
    assert!(
        r["indexCorrupt"].as_str().unwrap().contains("symbols_fts"),
        "{r}"
    );

    let job = run(
        &mut c,
        &pid,
        method::INDEX_REQUEST,
        json!({"kind": "rebuild"}),
    )
    .await;
    assert_eq!(job["state"], "succeeded", "{job}");
    event_until(&mut stream, "current again", |e| {
        e["kind"] == "project.state" && e["projectId"] == pid && e["data"]["freshness"] == "current"
    })
    .await;
    let r = search(&mut c, &pid, "fn_3_4").await.unwrap();
    assert!(r.get("indexCorrupt").is_none(), "{r}");
}

/// The other way back: a backup (taken when the index was claimed) restored
/// over the damaged file, right after the check that found the damage.
#[tokio::test(flavor = "multi_thread")]
async fn a_damaged_index_is_restored_from_a_backup_after_a_check() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let (_d, mut c) = start(&home).await;
    let mut stream = subscribe(&home, &mut c).await;
    let (pid, _root, index_dir, _) = project(&mut c, &mut stream, &base).await;
    // A claim over an existing index backs it up first.
    c.call_raw(
        method::PROJECT_CONFIGURE,
        Some(&pid),
        json!({"watchPolicy": "manual"}),
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

    let db = index_dir.join("index.db");
    let mut bytes = std::fs::read(&db).unwrap();
    let pages = bytes.len() / 4096;
    for b in &mut bytes[4096 * (pages / 2)..4096 * (pages / 2 + 4)] {
        *b = 0x5a;
    }
    std::fs::write(&db, &bytes).unwrap();

    let check = run(&mut c, &pid, method::INDEX_CHECK, json!({})).await;
    assert_eq!(check["result"]["verdict"], "corrupt", "{check}");
    let restored = run(
        &mut c,
        &pid,
        method::INDEX_REQUEST,
        json!({"kind": "restore", "backup": name}),
    )
    .await;
    assert_eq!(restored["state"], "succeeded", "{restored}");
    let kept = PathBuf::from(restored["result"]["previousKeptAs"].as_str().unwrap());
    assert_eq!(
        std::fs::read(&kept).unwrap(),
        bytes,
        "not kept byte for byte"
    );
    let check = run(&mut c, &pid, method::INDEX_CHECK, json!({})).await;
    assert_eq!(check["result"]["verdict"], "compatible", "{check}");
    let s = status(&mut c, &pid).await;
    assert_eq!(s["lastError"], Value::Null, "{s}");
    search(&mut c, &pid, "fn_3_4").await.unwrap();
}

/// Nobody asks: the scheduled health check (`integrityCheckHours`; seconds
/// under the debug-only `WRONGINDEX_DEBUG_CHECK_EVERY_SECS`) checks the owned
/// index, shows its time and result in the project's status, finds a damaged
/// full-text index, stops writes, and is not repeated on a damaged index.
#[tokio::test(flavor = "multi_thread")]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the schedule hook exists in debug builds only"
)]
async fn the_scheduled_check_finds_damage_nobody_asked_about() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let (_d, mut c) = start_with(&home, &[("WRONGINDEX_DEBUG_CHECK_EVERY_SECS", "3")]).await;
    let mut stream = subscribe(&home, &mut c).await;
    let (pid, _root, index_dir, _) = project(&mut c, &mut stream, &base).await;

    // A first scheduled check: in the status, with its time and result.
    let deadline = Instant::now() + Duration::from_secs(60);
    let first = loop {
        let s = status(&mut c, &pid).await;
        if s["lastCheck"]["state"] == "succeeded" {
            break s["lastCheck"].clone();
        }
        assert!(Instant::now() < deadline, "no scheduled check: {s}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(first["params"]["scheduled"], true, "{first}");
    assert_eq!(first["result"]["verdict"], "compatible", "{first}");
    assert_eq!(first["result"]["integrity"]["ok"], true, "{first}");
    assert!(first["completedAtMs"].as_i64().is_some(), "{first}");

    {
        let conn = rusqlite::Connection::open(index_dir.join("index.db")).unwrap();
        conn.execute(
            "UPDATE symbols_fts_content SET c0 = 'nothing like it' WHERE id = (SELECT min(id) FROM symbols_fts_content)",
            [],
        )
        .unwrap();
    }
    let stopped = event_until(&mut stream, "index_corrupt state", |e| {
        e["kind"] == "project.state"
            && e["projectId"] == pid
            && e["data"]["watchHealth"] == "index_corrupt"
    })
    .await;
    assert!(
        stopped["data"]["lastError"]
            .as_str()
            .unwrap()
            .contains("symbols_fts"),
        "{stopped}"
    );
    let s = status(&mut c, &pid).await;
    assert_eq!(s["lastCheck"]["params"]["scheduled"], true, "{s}");
    assert_eq!(s["lastCheck"]["result"]["verdict"], "corrupt", "{s}");

    // Not checked again on a timer while it is known damaged.
    let before = check_jobs(&mut c, &pid).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert_eq!(check_jobs(&mut c, &pid).await, before, "checked again");
}

async fn check_jobs(c: &mut Client, pid: &str) -> usize {
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
        .filter(|j| j["kind"] == "check")
        .count()
}
