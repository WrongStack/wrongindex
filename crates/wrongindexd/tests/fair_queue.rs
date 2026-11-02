//! Fair job scheduling (WI-604): a slow or hostile project occupies at most
//! one job slot, so another project's job finishes while it still runs.
//! The slow project is simulated with the debug-only
//! `WRONGINDEX_NATIVE_STALL` hook in the native child.

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
    PathBuf::from(c.display().to_string().trim_start_matches(r"\?\"))
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

const STALL_SECS: u64 = 40;

async fn start(home: &Path) -> (Daemon, Client) {
    let child = Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
        .arg("--data-root")
        .arg(home)
        .env_remove("WRONGINDEX_ENGINE")
        .env("WRONGINDEX_NATIVE_STALL", format!("slowproj:{STALL_SECS}"))
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

async fn admit(c: &mut Client, pid: &str) -> String {
    let admitted = c
        .call_raw(
            method::INDEX_REQUEST,
            Some(pid),
            json!({"kind": "full"}),
            None,
        )
        .await
        .unwrap();
    admitted["job"]["jobId"].as_str().unwrap().to_string()
}

async fn job(c: &mut Client, id: &str) -> Value {
    c.call_raw(method::JOB_GET, None, json!({"jobId": id}), None)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_project_does_not_hold_up_another() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let (_d, mut c) = start(&base.join("home")).await;
    let fixture = repo().join("tests/fixtures/projects/config-only");
    let slow_root = base.join("slowproj");
    let fast_root = base.join("fastproj");
    copy_dir(&fixture, &slow_root);
    copy_dir(&fixture, &fast_root);
    let slow = register(&mut c, &slow_root, &base.join("slow-index")).await;
    let fast = register(&mut c, &fast_root, &base.join("fast-index")).await;

    // The slow project's job is admitted first and starts first.
    let slow_job = admit(&mut c, &slow).await;
    let started = Instant::now();
    loop {
        if job(&mut c, &slow_job).await["state"] == "running" {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "slow job never started"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // A second job of the slow project waits behind the first (one per
    // project); the other project's job runs in the free slot.
    let slow_job2 = admit(&mut c, &slow).await;
    let fast_job = admit(&mut c, &fast).await;
    let admitted = Instant::now();
    let done = loop {
        let j = job(&mut c, &fast_job).await;
        if matches!(j["state"].as_str(), Some("succeeded" | "failed")) {
            break j;
        }
        assert!(
            admitted.elapsed() < Duration::from_secs(STALL_SECS / 2),
            "the other project's job waited for the slow one: {j}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(done["state"], "succeeded", "{done}");
    assert_eq!(done["result"]["backend"], "native", "{done}");
    // The slow project is still on its first job; its second never ran
    // alongside it.
    assert_eq!(job(&mut c, &slow_job).await["state"], "running");
    assert_eq!(job(&mut c, &slow_job2).await["state"], "queued");
    eprintln!(
        "other project's job finished {:?} after admission while the slow one ran",
        admitted.elapsed()
    );

    // The daemon dies (crash, kill): its native child must die with it, or
    // it would publish `index.db` after the ownership lock is gone (INV-01).
    let children = child_pids(_d.0.id());
    assert!(!children.is_empty(), "the slow native child is running");
    drop(_d);
    let deadline = Instant::now() + Duration::from_secs(10);
    while children.iter().any(|p| alive(*p)) {
        assert!(
            Instant::now() < deadline,
            "children outlived the daemon: {children:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        !base.join("slow-index").join("index.db").exists(),
        "nothing published after the daemon died"
    );
}

fn powershell(script: &str) -> String {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn child_pids(parent: u32) -> Vec<u32> {
    powershell(&format!(
        "Get-CimInstance Win32_Process -Filter 'ParentProcessId={parent}' | Where-Object Name -eq 'wrongindexd.exe' | ForEach-Object {{ $_.ProcessId }}"
    ))
    .lines()
    .filter_map(|l| l.trim().parse().ok())
    .collect()
}

fn alive(pid: u32) -> bool {
    powershell(&format!(
        "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ 'yes' }}"
    ))
    .contains("yes")
}
