//! Write-ahead log retention (WI-603; indexing-pipeline §12) with a real
//! outside reader on a watched index:
//!
//! * while the reader keeps a read transaction open, every run grows the
//!   WAL; the service's checkpoint gives up without disturbing the reader
//!   (its snapshot is unchanged) and the WAL is reported `large`, blocked by
//!   a reader (`resource.warning`, `resource: "wal"`, and in the snapshot);
//! * once the transaction ends - the connection stays open, as a pooled
//!   reader's would - the WAL is copied into the index and cut to zero
//!   bytes, the warning clears, and the reader sees the latest generation.

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

/// 400 JSON files, 41 keys each; `rev` renames every key.
fn write_files(root: &Path, rev: u32) {
    let dir = root.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..400 {
        let keys: Vec<String> = (0..41)
            .map(|k| format!("\"key_{i}_{k}_rev{rev}\": \"value {k} of file {i}, revision {rev}, with some padding\""))
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

async fn generation(c: &mut Client, pid: &str) -> (i64, String) {
    let s = c
        .call_raw(method::PROJECT_GET, Some(pid), json!({}), None)
        .await
        .unwrap();
    (
        s["generation"].as_i64().unwrap_or(0),
        s["freshness"].as_str().unwrap_or("").to_string(),
    )
}

async fn wait_current_after(c: &mut Client, pid: &str, after: i64) -> i64 {
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let (g, f) = generation(c, pid).await;
        if g > after && f == "current" {
            return g;
        }
        assert!(Instant::now() < deadline, "no new generation (at {g}, {f})");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
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

fn count(reader: &rusqlite::Connection, name: &str) -> i64 {
    reader
        .query_row(
            "SELECT count(*) FROM symbols WHERE name = ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reader_holding_a_transaction_keeps_the_wal_until_it_lets_go() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    let root = base.join("proj");
    let index = base.join("idx");
    write_files(&root, 0);
    let daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
            .arg("--data-root")
            .arg(&home)
            .env_remove("WRONGINDEX_ENGINE")
            .env_remove("WRONGINDEX_FAULT_FREE_BYTES_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut c = loop {
        if let Ok(c) = Client::connect(&DataRoot::resolve(Some(&home)).unwrap(), "t").await {
            break c;
        }
        assert!(Instant::now() < deadline, "daemon not ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": index, "watchPolicy": "pinned"}),
            None,
        )
        .await
        .unwrap();
    let pid = reg["project"]["projectId"].as_str().unwrap().to_string();
    let mut generation_now = wait_current_after(&mut c, &pid, 0).await;
    let snap = c
        .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
        .await
        .unwrap();
    let (_, mut stream) = Client::connect(&DataRoot::resolve(Some(&home)).unwrap(), "t")
        .await
        .unwrap()
        .subscribe(snap["seq"].as_u64(), snap["instanceId"].as_str(), None)
        .await
        .unwrap();

    // An outside reader holds a read transaction across several runs.
    let reader = rusqlite::Connection::open_with_flags(
        index.join("index.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader.execute_batch("BEGIN").unwrap();
    assert_eq!(count(&reader, "key_0_0_rev0"), 1);
    let mut sizes = Vec::new();
    for rev in 1..=4 {
        write_files(&root, rev);
        generation_now = wait_current_after(&mut c, &pid, generation_now).await;
        sizes.push(wal_bytes(&index));
    }
    assert!(
        sizes.windows(2).all(|w| w[0] < w[1]),
        "the WAL did not grow: {sizes:?}"
    );
    let large = event_until(&mut stream, "wal large, blocked", |e| {
        e["kind"] == "resource.warning"
            && e["projectId"] == pid
            && e["data"]["resource"] == "wal"
            && e["data"]["level"] == "large"
            && e["data"]["blockedByReader"] == true
    })
    .await;
    let d = &large["data"];
    assert_eq!(d["projectId"], pid, "{d}");
    // Which program has the index open: this test, which holds the reader.
    assert!(
        d["openBy"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["pid"] == std::process::id()
                && h["name"].as_str().is_some_and(|n| !n.is_empty())),
        "{d}"
    );
    assert!(
        d["walBytes"].as_u64().unwrap() > 2 * d["indexBytes"].as_u64().unwrap(),
        "{d}"
    );
    // The reader was not disturbed: still its snapshot.
    assert_eq!(count(&reader, "key_0_0_rev0"), 1);
    assert_eq!(count(&reader, "key_0_0_rev4"), 0);
    let snap = c
        .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
        .await
        .unwrap();
    assert!(
        snap["resources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["resource"] == "wal" && r["projectId"] == pid && r["level"] == "large"),
        "{snap}"
    );

    // The transaction ends; the connection stays open.
    reader.execute_batch("COMMIT").unwrap();
    let ok = event_until(&mut stream, "wal ok", |e| {
        e["kind"] == "resource.warning"
            && e["projectId"] == pid
            && e["data"]["resource"] == "wal"
            && e["data"]["level"] == "ok"
    })
    .await;
    assert_eq!(ok["data"]["blockedByReader"], false, "{ok}");
    assert_eq!(wal_bytes(&index), 0, "the WAL was not cut");
    assert_eq!(count(&reader, "key_0_0_rev4"), 1);
    assert_eq!(count(&reader, "key_0_0_rev0"), 0);
    let (g, f) = generation(&mut c, &pid).await;
    assert_eq!((g, f.as_str()), (generation_now, "current"));
    let snap = c
        .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(snap["resources"], json!([]), "{snap}");
    drop(reader);
    drop(daemon);
}
