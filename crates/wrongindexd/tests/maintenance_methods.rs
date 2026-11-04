//! The maintenance methods of architecture §8.2, as durable jobs:
//!
//! * `index.backup`: a consistent copy of the index on request, kept with
//!   the transfer backups under `backupKeep`, restorable; refused without
//!   ownership; an index that does not exist yet is not "backed up";
//! * `index.checkpoint`: the write-ahead log folded into the index and cut
//!   to zero, or `DATABASE_BUSY` (retryable) while an outside reader holds a
//!   read transaction - that reader is not disturbed and the log is left as
//!   it was;
//! * a refusal because the index is open names the programs that have it
//!   open (here: this test program), for a checkpoint and for a restore.

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

fn long(p: &Path) -> PathBuf {
    let c = std::fs::canonicalize(p).unwrap();
    PathBuf::from(c.display().to_string().trim_start_matches(r"\\?\"))
}

async fn start(home: &Path) -> (Daemon, Client) {
    let child = Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
        .arg("--data-root")
        .arg(home)
        .env_remove("WRONGINDEX_ENGINE")
        .env_remove("WRONGINDEX_FAULT_FREE_BYTES_FILE")
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

/// 200 JSON files, 20 keys each; `rev` renames every key.
fn write_files(root: &Path, rev: u32) {
    let dir = root.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..200 {
        let keys: Vec<String> = (0..20)
            .map(|k| format!("\"key_{i}_{k}_rev{rev}\": \"value {k} of file {i}, revision {rev}\""))
            .collect();
        std::fs::write(
            dir.join(format!("f{i}.json")),
            format!("{{{}}}", keys.join(",")),
        )
        .unwrap();
    }
}

fn wal_bytes(index: &Path) -> u64 {
    std::fs::metadata(index.join("index.db-wal"))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Admit with `m` (and `params`) and wait for the job's end.
async fn run(c: &mut Client, pid: &str, m: &str, params: Value) -> Value {
    let admitted = c.call_raw(m, Some(pid), params, None).await.unwrap();
    let id = admitted["job"]["jobId"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(120);
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

/// A manual project, claimed and indexed once.
async fn indexed(c: &mut Client, base: &Path, name: &str) -> (String, PathBuf, PathBuf) {
    let root = base.join(name);
    let index = base.join(format!("{name}-idx"));
    write_files(&root, 0);
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
    let full = run(c, &pid, method::INDEX_REQUEST, json!({"kind": "full"})).await;
    assert_eq!(full["state"], "succeeded", "{full}");
    (pid, root, index)
}

fn count(db: &rusqlite::Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |r| r.get(0)).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_backup_on_request_is_kept_with_the_others() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start(&base.join("home")).await;
    let (pid, _root, index) = indexed(&mut c, &base, "proj").await;
    c.call_raw(
        method::SETTINGS_SET,
        None,
        json!({"settings": {"backupKeep": 2}}),
        None,
    )
    .await
    .unwrap();

    let mut names = Vec::new();
    for _ in 0..3 {
        let job = run(&mut c, &pid, method::INDEX_BACKUP, json!({})).await;
        assert_eq!(job["state"], "succeeded", "{job}");
        assert_eq!(job["kind"], "backup", "{job}");
        let r = &job["result"];
        assert!(r["bytes"].as_u64().unwrap() > 0, "{r}");
        assert!(r["kept"].as_u64().unwrap() <= 2, "{r}");
        names.push(r["backup"].as_str().unwrap().to_string());
        // Backup names carry the time in milliseconds.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The newest two are kept, newest first.
    let listed = c
        .call_raw(method::INDEX_BACKUPS, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let kept: Vec<&str> = listed["backups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(kept, [names[2].as_str(), names[1].as_str()], "{listed}");

    // A consistent copy of what is indexed, which SQLite finds sound.
    let backup = rusqlite::Connection::open_with_flags(
        index.join(".wrongindex-backups").join(&names[2]),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let ok: String = backup
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ok, "ok");
    assert_eq!(count(&backup, "SELECT count(*) FROM files"), 200);
    drop(backup);
    // And it can be put back.
    let restored = run(
        &mut c,
        &pid,
        method::INDEX_REQUEST,
        json!({"kind": "restore", "backup": names[2]}),
    )
    .await;
    assert_eq!(restored["state"], "succeeded", "{restored}");

    // No index yet: nothing to back up, said so.
    let empty_root = base.join("empty");
    std::fs::create_dir_all(&empty_root).unwrap();
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": empty_root, "indexDir": base.join("empty-idx")}),
            None,
        )
        .await
        .unwrap();
    let empty = reg["project"]["projectId"].as_str().unwrap().to_string();
    c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&empty), json!({}), None)
        .await
        .unwrap();
    let none = run(&mut c, &empty, method::INDEX_BACKUP, json!({})).await;
    assert_eq!(none["state"], "failed", "{none}");
    assert_eq!(none["error"]["code"], "INVALID_REQUEST", "{none}");
    assert!(!base.join("empty-idx").join("index.db").exists());

    // Without ownership: refused at admission.
    c.call_raw(method::OWNERSHIP_RELEASE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let refused = c
        .call_raw(method::INDEX_BACKUP, Some(&pid), json!({}), None)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("OWNER_CONFLICT"), "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkpoint_folds_the_log_in_or_says_a_reader_holds_it() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start(&base.join("home")).await;
    let (pid, root, index) = indexed(&mut c, &base, "proj").await;

    // An outside reader holds a read transaction across a run: the log
    // cannot be folded in.
    let reader = rusqlite::Connection::open_with_flags(
        index.join("index.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let old = "SELECT count(*) FROM symbols WHERE name = 'key_0_0_rev0'";
    assert_eq!(count(&reader, old), 1);
    write_files(&root, 1);
    let full = run(&mut c, &pid, method::INDEX_REQUEST, json!({"kind": "full"})).await;
    assert_eq!(full["state"], "succeeded", "{full}");
    let held = wal_bytes(&index);
    assert!(held > 0, "no write-ahead log behind the reader");

    let busy = run(&mut c, &pid, method::INDEX_CHECKPOINT, json!({})).await;
    assert_eq!(busy["state"], "failed", "{busy}");
    assert_eq!(busy["kind"], "checkpoint", "{busy}");
    assert_eq!(busy["error"]["code"], "DATABASE_BUSY", "{busy}");
    assert_eq!(busy["error"]["retryable"], true, "{busy}");
    // Who has it open is named: this test program holds the reader.
    let me = format!("(pid {})", std::process::id());
    let said = busy["error"]["message"].as_str().unwrap();
    assert!(said.contains("Open by: ") && said.contains(&me), "{said}");
    // A restore is refused the same way, and names it too; nothing changes.
    let backup = run(&mut c, &pid, method::INDEX_BACKUP, json!({})).await;
    assert_eq!(backup["state"], "succeeded", "{backup}");
    let restore = run(
        &mut c,
        &pid,
        method::INDEX_REQUEST,
        json!({"kind": "restore", "backup": backup["result"]["backup"]}),
    )
    .await;
    assert_eq!(restore["state"], "failed", "{restore}");
    assert_eq!(restore["error"]["code"], "DATABASE_BUSY", "{restore}");
    let said = restore["error"]["message"].as_str().unwrap();
    assert!(said.contains(&me), "{said}");
    // Nothing changed for the reader, and the log is still there.
    assert_eq!(count(&reader, old), 1);
    assert!(wal_bytes(&index) >= held);

    // The transaction ends; the connection stays open, as a pooled
    // reader's would. Now the log is folded in and cut to zero (by this
    // job, or by the service's own care just before it).
    reader.execute_batch("COMMIT").unwrap();
    let done = run(&mut c, &pid, method::INDEX_CHECKPOINT, json!({})).await;
    assert_eq!(done["state"], "succeeded", "{done}");
    let outcome = done["result"]["outcome"].as_str().unwrap();
    eprintln!("checkpoint after the reader let go: {outcome}");
    assert!(matches!(outcome, "truncated" | "empty"), "{done}");
    if outcome == "truncated" {
        assert!(done["result"]["walBytes"].as_u64().unwrap() > 0, "{done}");
    }
    assert_eq!(wal_bytes(&index), 0, "{done}");
    assert_eq!(count(&reader, old), 0);
    assert_eq!(
        count(
            &reader,
            "SELECT count(*) FROM symbols WHERE name = 'key_0_0_rev1'"
        ),
        1
    );
    drop(reader);

    // Without ownership: refused at admission.
    c.call_raw(method::OWNERSHIP_RELEASE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let refused = c
        .call_raw(method::INDEX_CHECKPOINT, Some(&pid), json!({}), None)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("OWNER_CONFLICT"), "{refused}");
}
