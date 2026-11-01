//! WI-109 acceptance: one owner per resolved index across independent daemon
//! instances, epoch fencing across restarts, and admission before mutation.
//! Real daemon processes, real registry, real OS locks; nothing mocked.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongindex_contracts::{ErrorCode, method};
use wrongindex_ipc::{Client, DataRoot};
use wrongindex_storage::owner_lock::{LOCK_FILE, OWNER_FILE, is_held, read_owner};

struct Daemon {
    child: Child,
    root: PathBuf,
}

impl Daemon {
    async fn start(root: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
            .arg("--data-root")
            .arg(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Client::connect(&DataRoot::resolve(Some(root)).unwrap(), "t")
            .await
            .is_err()
        {
            assert!(Instant::now() < deadline, "daemon not ready");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Self {
            child,
            root: root.to_path_buf(),
        }
    }

    async fn client(&self) -> Client {
        Client::connect(&DataRoot::resolve(Some(&self.root)).unwrap(), "t")
            .await
            .unwrap()
    }

    async fn stop(mut self) {
        let mut c = self.client().await;
        c.call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                assert!(s.success());
                return;
            }
            assert!(Instant::now() < deadline, "daemon did not stop");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

async fn register(c: &mut Client, checkout: &Path, index: &Path) -> String {
    let v = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": checkout, "indexDir": index}),
            None,
        )
        .await
        .unwrap();
    v["project"]["projectId"].as_str().unwrap().to_string()
}

fn fixture_into(index: &Path) {
    std::fs::create_dir_all(index).unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/oracle/basic-ts/index.db"),
        index.join("index.db"),
    )
    .unwrap();
}

async fn status(c: &mut Client, pid: &str) -> Value {
    c.call_raw(method::OWNERSHIP_STATUS, Some(pid), json!({}), None)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn one_owner_across_daemons_with_epochs_and_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let checkout = tmp.path().join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    let index = tmp.path().join("index");
    fixture_into(&index);
    let db_before = std::fs::read(index.join("index.db")).unwrap();

    let a = Daemon::start(&tmp.path().join("home-a")).await;
    let b = Daemon::start(&tmp.path().join("home-b")).await;
    let mut ca = a.client().await;
    let mut cb = b.client().await;
    let pa = register(&mut ca, &checkout, &index).await;
    let pb = register(&mut cb, &checkout, &index).await;
    // Registration alone never writes into the index directory.
    assert!(!index.join(LOCK_FILE).exists());

    let st = ca
        .call_raw(method::OWNERSHIP_ACQUIRE, Some(&pa), json!({}), None)
        .await
        .unwrap();
    assert_eq!(st["state"], "owned");
    assert_eq!(st["ownerEpoch"], 1);
    assert!(is_held(&index));
    let owner = read_owner(&index).unwrap();
    assert_eq!(owner.epoch, 1);

    // A second, independent daemon cannot take the same index.
    let err = cb
        .call_raw(method::OWNERSHIP_ACQUIRE, Some(&pb), json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::OwnerConflict), "{err}");
    assert!(err.to_string().contains(&owner.instance_id));
    assert_eq!(status(&mut cb, &pb).await["status"]["state"], "conflict");

    // Idempotent re-acquire keeps the epoch.
    let again = ca
        .call_raw(method::OWNERSHIP_ACQUIRE, Some(&pa), json!({}), None)
        .await
        .unwrap();
    assert_eq!(again["ownerEpoch"], 1);
    assert_eq!(
        ca.call_raw(method::PROJECT_GET, Some(&pa), json!({}), None)
            .await
            .unwrap()["ownership"],
        "owned"
    );

    // Graceful restart: guard released, intent kept, re-acquired at epoch 2.
    drop(ca);
    a.stop().await;
    assert!(!is_held(&index), "guard must be released on shutdown");
    let a = Daemon::start(&tmp.path().join("home-a")).await;
    let mut ca = a.client().await;
    let st = status(&mut ca, &pa).await;
    assert_eq!(st["status"]["state"], "owned");
    assert_eq!(st["status"]["ownerEpoch"], 2);
    assert_eq!(st["guardHeld"], true);

    // Crash: the OS drops the guard; the next instance does not trust the
    // recorded state, journals recovery, and re-acquires at a new epoch.
    drop(ca);
    a.kill();
    assert!(!is_held(&index));
    let a = Daemon::start(&tmp.path().join("home-a")).await;
    let mut ca = a.client().await;
    let st = status(&mut ca, &pa).await;
    assert_eq!(st["status"]["ownerEpoch"], 3);
    let phases: Vec<&str> = st["journal"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["phase"].as_str().unwrap())
        .collect();
    assert_eq!(
        &phases[..4],
        ["owned", "negotiating", "recovery_required", "owned"]
    );

    // Explicit release lets the other daemon take over.
    let st = ca
        .call_raw(method::OWNERSHIP_RELEASE, Some(&pa), json!({}), None)
        .await
        .unwrap();
    assert_eq!(st["state"], "unowned");
    assert_eq!(st["desired"], false);
    assert!(read_owner(&index).is_none());
    let st = cb
        .call_raw(method::OWNERSHIP_ACQUIRE, Some(&pb), json!({}), None)
        .await
        .unwrap();
    assert_eq!(st["state"], "owned");

    // Detaching an owned project releases its guard.
    cb.call_raw(method::PROJECT_DETACH, Some(&pb), json!({}), None)
        .await
        .unwrap();
    assert!(!is_held(&index));

    // Ownership never modified the index database itself.
    assert_eq!(std::fs::read(index.join("index.db")).unwrap(), db_before);
    drop(ca);
    drop(cb);
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_index_is_refused_without_creating_files() {
    let tmp = tempfile::tempdir().unwrap();
    let checkout = tmp.path().join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    let index = tmp.path().join("index");
    fixture_into(&index);
    {
        let c = rusqlite::Connection::open(index.join("index.db")).unwrap();
        c.execute_batch(
            "UPDATE metadata SET value = '99' WHERE key = 'version'; PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
    }
    let d = Daemon::start(&tmp.path().join("home")).await;
    let mut c = d.client().await;
    let pid = register(&mut c, &checkout, &index).await;
    let err = c
        .call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::ProfileUnsupported), "{err}");
    assert!(!index.join(LOCK_FILE).exists());
    assert!(!index.join(OWNER_FILE).exists());
    assert_eq!(status(&mut c, &pid).await["status"]["ownerEpoch"], 0);
    drop(c);
    d.stop().await;
}
