//! WI-606: the native service needs no Bun/Node engine, no per-project
//! daemon and no helper program for the languages it parses in process. The
//! service runs with a PATH holding only the Windows system directory (no
//! bun, node, python, go or git) and indexes and serves TS/JS, Rust and JSON
//! projects natively; its process tree is the service and its own confined
//! child (`wrongindexd --native-rebuild`).

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

fn system_only_path() -> String {
    let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    format!(r"{windir}\System32")
}

async fn start_bare(home: &Path) -> (Daemon, Client) {
    let child = Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
        .arg("--data-root")
        .arg(home)
        .args(["--backend", "native"])
        .env_remove("WRONGINDEX_ENGINE")
        .env("PATH", system_only_path())
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

#[tokio::test(flavor = "multi_thread")]
async fn the_native_service_needs_no_engine_daemon_or_toolchain_for_in_process_languages() {
    // Nothing the in-process languages could fall back to is reachable.
    for exe in ["bun.exe", "node.exe", "python.exe", "go.exe", "git.exe"] {
        assert!(
            !Path::new(&system_only_path()).join(exe).exists(),
            "{exe} in System32 would weaken this test"
        );
    }
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start_bare(&base.join("home")).await;
    let health = c
        .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
        .await
        .unwrap();
    assert!(
        health["backendInventory"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b.as_str().unwrap_or("").contains("\"backend\":\"native\"")),
        "{health}"
    );
    for fixture in ["basic-ts", "rs-basic", "config-only", "encodings"] {
        let root = base.join(fixture);
        copy_dir(&repo().join("tests/fixtures/projects").join(fixture), &root);
        let reg = c
            .call_raw(
                method::PROJECT_REGISTER,
                None,
                json!({"checkoutRoot": root, "indexDir": base.join(format!("{fixture}-idx"))}),
                None,
            )
            .await
            .unwrap();
        let pid = reg["project"]["projectId"].as_str().unwrap().to_string();
        c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
            .await
            .unwrap();
        let job = run_job(&mut c, &pid, json!({"kind": "full"})).await;
        assert_eq!(job["state"], "succeeded", "{fixture}: {job}");
        assert_eq!(job["result"]["backend"], "native", "{fixture}: {job}");
        let stats = c
            .call_raw(
                method::INDEX_QUERY,
                Some(&pid),
                json!({"op": "stats", "args": {}}),
                None,
            )
            .await
            .unwrap();
        assert_eq!(stats["backend"], "native", "{fixture}: {stats}");
        assert!(
            stats["result"]["totalSymbols"].as_u64().unwrap_or(0) > 0,
            "{fixture}: {stats}"
        );
    }
}
