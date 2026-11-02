//! Native backend through the real daemon, without any WrongStack engine:
//! a JSON/YAML-only project is indexed, re-indexed as a no-op, updated after
//! an edit, and followed by the watcher; an unsupported project fails with a
//! clear NOT_IMPLEMENTED reason instead of being written.

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

async fn register(c: &mut Client, root: &Path, index: &Path) -> String {
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": index}),
            None,
        )
        .await
        .unwrap();
    let pid = reg["project"]["projectId"].as_str().unwrap().to_string();
    c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    pid
}

async fn run_job(c: &mut Client, pid: &str, params: Value) -> Value {
    let admitted = c
        .call_raw(method::INDEX_REQUEST, Some(pid), params, None)
        .await
        .unwrap();
    let id = admitted["job"]["jobId"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let job = c
            .call_raw(method::JOB_GET, None, json!({"jobId": id}), None)
            .await
            .unwrap();
        if matches!(job["state"].as_str(), Some("succeeded" | "failed")) {
            return job;
        }
        assert!(Instant::now() < deadline, "job did not finish: {job}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn status(c: &mut Client, pid: &str) -> Value {
    c.call_raw(method::PROJECT_GET, Some(pid), json!({}), None)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn native_backend_indexes_updates_and_watches_without_an_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start(&base.join("home")).await;
    let health = c
        .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
        .await
        .unwrap();
    assert!(
        health
            .to_string()
            .contains("\\\"backend\\\":\\\"native\\\""),
        "{health}"
    );

    let root = base.join("cfg");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let index = base.join("cfg-index");
    let pid = register(&mut c, &root, &index).await;

    // 1. Full run, natively.
    let job = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(job["state"], "succeeded", "{job}");
    let r = &job["result"];
    assert_eq!(r["backend"], "native");
    assert_eq!(r["native"]["outcome"], "written");
    assert_eq!(r["native"]["symbolsIndexed"], 10);
    assert_eq!(r["generation"], 1);
    // Native reads, no engine: stats is served; an op not ported yet says so.
    let q = c
        .call_raw(
            method::INDEX_QUERY,
            Some(&pid),
            json!({"op": "stats", "args": {}}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(q["backend"], "native");
    assert_eq!(q["result"]["totalSymbols"], 10);
    assert_eq!(q["generation"], 1);
    let err = c
        .call_raw(
            method::INDEX_QUERY,
            Some(&pid),
            json!({"op": "notAnOp", "args": {}}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.code(),
        Some(wrongindex_contracts::ErrorCode::NotImplemented)
    );
    let check = run_job(&mut c, &pid, json!({"kind": "check"})).await;
    assert_eq!(check["result"]["verdict"], "compatible", "{check}");

    // 2. Nothing changed: no write, generation kept (INV-11).
    let job = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(job["result"]["native"]["outcome"], "unchanged", "{job}");
    // Reported as WrongStack reports a no-op run: nothing indexed, every
    // stored symbol counted.
    assert_eq!(job["result"]["native"]["filesIndexed"], 0, "{job}");
    assert_eq!(job["result"]["native"]["symbolsIndexed"], 10, "{job}");
    assert_eq!(job["result"]["generation"], 1);

    // 3. An edit, through an incremental request: new generation.
    std::fs::write(
        root.join("config/app.yaml"),
        "server:\n  port: 1\nnewKey: 2\n",
    )
    .unwrap();
    let job = run_job(
        &mut c,
        &pid,
        json!({"kind": "incremental", "paths": [root.join("config/app.yaml")]}),
    )
    .await;
    assert_eq!(job["state"], "succeeded", "{job}");
    assert_eq!(job["result"]["generation"], 2);

    // 4. Pinned: the watcher follows edits on its own.
    c.call_raw(
        method::PROJECT_CONFIGURE,
        Some(&pid),
        json!({"watchPolicy": "pinned"}),
        None,
    )
    .await
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let s = status(&mut c, &pid).await;
        if s["freshness"] == "current" && s["watchHealth"] != "off" {
            break;
        }
        assert!(Instant::now() < deadline, "never became current: {s}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let before = status(&mut c, &pid).await["generation"].as_i64().unwrap();
    std::fs::write(root.join("config/watched.json"), "{\"watched\": true}").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let s = status(&mut c, &pid).await;
        if s["generation"].as_i64().unwrap() > before && s["freshness"] == "current" {
            break;
        }
        assert!(Instant::now() < deadline, "watcher edit not published: {s}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 5. Unsupported project (no indexable files): refused with the
    // reason, nothing written.
    let go = base.join("empty");
    std::fs::create_dir_all(&go).unwrap();
    std::fs::write(go.join("blob.bin"), b"\x00\x01").unwrap();
    let go_index = base.join("go-index");
    let go_pid = register(&mut c, &go, &go_index).await;
    let job = run_job(&mut c, &go_pid, json!({"kind": "full"})).await;
    assert_eq!(job["state"], "failed");
    assert_eq!(job["error"]["code"], "NOT_IMPLEMENTED", "{job}");
    assert!(
        job["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no indexable files"),
        "{job}"
    );
    assert!(!go_index.join("index.db").exists());

    // 6. A hostile source that overflows a recursive parser (a unary chain
    //    escapes any bracket pre-scan) only ends the isolated child: the job
    //    fails as unsupported and the daemon keeps serving.
    let evil = base.join("evil");
    std::fs::create_dir_all(&evil).unwrap();
    std::fs::write(
        evil.join("a.ts"),
        format!(
            "export const x = {}y;
",
            "!".repeat(2_000_000)
        ),
    )
    .unwrap();
    let evil_pid = register(&mut c, &evil, &base.join("evil-index")).await;
    let job = run_job(&mut c, &evil_pid, json!({"kind": "full"})).await;
    assert_eq!(job["state"], "failed", "{job}");
    assert_eq!(job["error"]["code"], "NOT_IMPLEMENTED", "{job}");
    assert!(
        job["error"]["message"]
            .as_str()
            .unwrap()
            .contains("crashed"),
        "the isolated child must have crashed, not the daemon: {job}"
    );
    let health = c
        .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(health["draining"], false);
    let s = status(&mut c, &pid).await;
    assert!(s["generation"].as_i64().unwrap() >= 2, "{s}");
}

async fn search(c: &mut Client, pid: &str, query: &str) -> Vec<String> {
    let q = c
        .call_raw(
            method::INDEX_QUERY,
            Some(pid),
            json!({"op": "search", "args": {"query": query, "limit": 20}}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(q["backend"], "native", "{q}");
    let hits = q["result"]["results"]
        .as_array()
        .or_else(|| q["result"].as_array())
        .cloned()
        .unwrap_or_default();
    hits.iter()
        .filter_map(|h| h["name"].as_str().map(str::to_string))
        .collect()
}

/// WI-515 step 3: an incremental job (watcher batch, edit hook) runs as
/// WrongStack's targeted run over its paths - the native child gets the list,
/// touches only those files (a deleted one loses its rows, a directory
/// expands), and an edit nobody named is not picked up until a full run.
#[tokio::test(flavor = "multi_thread")]
async fn incremental_jobs_are_targeted_native_updates() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start(&base.join("home")).await;
    let root = base.join("p");
    copy_dir(&repo().join("tests/fixtures/projects/basic-ts"), &root);
    let pid = register(&mut c, &root, &base.join("idx")).await;
    let job = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(job["state"], "succeeded", "{job}");

    // Named: a new file in a new directory (by directory), an edit; not
    // named: another edit.
    std::fs::create_dir_all(root.join("src/extra")).unwrap();
    std::fs::write(
        root.join("src/extra/added.ts"),
        "export function targetedAddition(): number { return 1; }\n",
    )
    .unwrap();
    let greet = std::fs::read_to_string(root.join("src/greet.ts")).unwrap();
    std::fs::write(
        root.join("src/greet.ts"),
        format!("{greet}\nexport function namedEdit(): number {{ return 2; }}\n"),
    )
    .unwrap();
    let runner = std::fs::read_to_string(root.join("src/runner.ts")).unwrap();
    std::fs::write(
        root.join("src/runner.ts"),
        format!("{runner}\nexport function unnamedEdit(): number {{ return 3; }}\n"),
    )
    .unwrap();
    let job = run_job(
        &mut c,
        &pid,
        json!({"kind": "incremental", "paths": [root.join("src/extra"), "src/greet.ts"]}),
    )
    .await;
    assert_eq!(job["state"], "succeeded", "{job}");
    let native = &job["result"]["native"];
    assert_eq!(native["outcome"], "updated", "{job}");
    assert_eq!(native["targets"], 2, "{job}");
    assert_eq!(native["changedFiles"], 2, "{job}");
    assert_eq!(job["result"]["generation"], 2, "{job}");
    assert!(
        search(&mut c, &pid, "targetedAddition")
            .await
            .contains(&"targetedAddition".to_string())
    );
    assert!(
        search(&mut c, &pid, "namedEdit")
            .await
            .contains(&"namedEdit".to_string())
    );
    assert!(
        !search(&mut c, &pid, "unnamedEdit")
            .await
            .contains(&"unnamedEdit".to_string())
    );

    // A deleted path loses its rows; nothing else is looked at.
    std::fs::remove_dir_all(root.join("src/extra")).unwrap();
    let job = run_job(
        &mut c,
        &pid,
        json!({"kind": "incremental", "paths": [root.join("src/extra")]}),
    )
    .await;
    assert_eq!(job["result"]["native"]["changedFiles"], 1, "{job}");
    assert!(
        !search(&mut c, &pid, "targetedAddition")
            .await
            .contains(&"targetedAddition".to_string())
    );
    assert!(
        !search(&mut c, &pid, "unnamedEdit")
            .await
            .contains(&"unnamedEdit".to_string())
    );

    // A full run catches up with the edit nobody named.
    let job = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(job["result"]["native"]["outcome"], "updated", "{job}");
    assert!(
        search(&mut c, &pid, "unnamedEdit")
            .await
            .contains(&"unnamedEdit".to_string())
    );
}
