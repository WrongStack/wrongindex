//! Multi-project soak (WI-602): one service watching several pinned projects
//! (native backend, no engine) under a steady stream of edits. Handles,
//! threads and working set must plateau, every project must converge to
//! `current`, and the last edit of each project must be searchable.
//!
//! `WRONGINDEX_SOAK_SECS` (default 120) sets the edit phase; ignored by
//! default because it is long.
//!
//! ```text
//! cargo test -p wrongindexd --release --test soak -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongindex_contracts::method;
use wrongindex_ipc::{Client, DataRoot};

#[path = "common/process_sample.rs"]
mod process_sample;
use process_sample::sample;

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

async fn status(c: &mut Client, pid: &str) -> Value {
    c.call_raw(method::PROJECT_GET, Some(pid), json!({}), None)
        .await
        .unwrap()
}

/// One edit per project kind: a new declaration with a unique name.
fn edit(root: &Path, kind: &str, n: u64) -> String {
    let name = format!("soak_marker_{n}");
    match kind {
        "json" => std::fs::write(
            root.join("config").join(format!("soak{}.json", n % 7)),
            format!("{{\"{name}\": {n}}}\n"),
        ),
        "rs" => std::fs::write(
            root.join("src").join(format!("soak{}.rs", n % 7)),
            format!("pub fn {name}() -> u64 {{ {n} }}\n"),
        ),
        _ => std::fs::write(
            root.join("src").join(format!("soak{}.ts", n % 7)),
            format!("export function {name}(): number {{ return {n}; }}\n"),
        ),
    }
    .unwrap();
    name
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "long-running soak; set WRONGINDEX_SOAK_SECS and run with --ignored"]
async fn many_projects_under_steady_edits_plateau_and_converge() {
    let secs: u64 = std::env::var("WRONGINDEX_SOAK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let home = base.join("home");
    // Qualification can exercise the exact packaged artifact without Cargo
    // replacing it with a test build of the same version.
    let daemon_exe = std::env::var_os("WRONGINDEX_TEST_DAEMON")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_wrongindexd")));
    let child = Command::new(&daemon_exe)
        .arg("--data-root")
        .arg(&home)
        .env_remove("WRONGINDEX_ENGINE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let daemon_pid = child.id();
    eprintln!(
        "soak subject: {} pid {daemon_pid}, duration {secs}s",
        daemon_exe.display()
    );
    let _daemon = Daemon(child);
    let root = DataRoot::resolve(Some(&home)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut c = loop {
        if let Ok(c) = Client::connect(&root, "soak").await {
            break c;
        }
        assert!(Instant::now() < deadline, "daemon not ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    // Two copies of each fixture: six pinned projects.
    let kinds = [
        ("config-only", "json"),
        ("rs-basic", "rs"),
        ("basic-ts", "ts"),
    ];
    let mut projects: Vec<(String, PathBuf, &str)> = Vec::new();
    for copy in 0..2 {
        for (fixture, kind) in kinds {
            let src = base.join(format!("{fixture}-{copy}"));
            copy_dir(&repo().join("tests/fixtures/projects").join(fixture), &src);
            std::fs::create_dir_all(src.join("src")).unwrap();
            std::fs::create_dir_all(src.join("config")).unwrap();
            let reg = c
                .call_raw(
                    method::PROJECT_REGISTER,
                    None,
                    json!({"checkoutRoot": src, "indexDir": base.join(format!("{fixture}-{copy}-index")), "watchPolicy": "pinned"}),
                    None,
                )
                .await
                .unwrap();
            let pid = reg["project"]["projectId"].as_str().unwrap().to_string();
            projects.push((pid, src, kind));
        }
    }
    // All current before the edit phase.
    let deadline = Instant::now() + Duration::from_secs(120);
    for (pid, _, _) in &projects {
        loop {
            let s = status(&mut c, pid).await;
            if s["freshness"] == "current" && s["generation"].as_i64().unwrap_or(0) > 0 {
                break;
            }
            assert!(Instant::now() < deadline, "never became current: {s}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // Edit phase: one edit every 250 ms, round robin; sample every 5 s.
    let start = Instant::now();
    let mut samples: Vec<(u64, u64, u64)> = Vec::new();
    let mut last_sample = Instant::now() - Duration::from_secs(10);
    let mut last_marker: Vec<String> = vec![String::new(); projects.len()];
    let mut n = 0u64;
    let mut last_tick = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        // Sleep or a stalled observer interrupts the edit workload. A long
        // gap alone cannot prove that Windows slept; either way, this is
        // not continuous-workload acceptance. Keep the original 30 s gate.
        let gap = last_tick.elapsed();
        if gap > Duration::from_secs(30) {
            println!(
                "NOT RUN: the soak workload was interrupted ({} s without a tick after {n} edits); sleep or an observer stall is not continuous-workload acceptance",
                gap.as_secs()
            );
            return;
        }
        last_tick = Instant::now();
        let i = (n as usize) % projects.len();
        let (_, src, kind) = &projects[i];
        last_marker[i] = edit(src, kind, n);
        n += 1;
        if last_sample.elapsed() >= Duration::from_secs(5) {
            if let Some(s) = sample(daemon_pid) {
                samples.push(s);
                eprintln!(
                    "soak sample: elapsed={}s edits={n} workingSet={} handles={} threads={}",
                    start.elapsed().as_secs(),
                    s.0,
                    s.1,
                    s.2
                );
            }
            last_sample = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // Convergence: every project current, its last marker searchable.
    let deadline = Instant::now() + Duration::from_secs(180);
    for (i, (pid, _, _)) in projects.iter().enumerate() {
        loop {
            let s = status(&mut c, pid).await;
            let found = if s["freshness"] == "current" && s["pendingPaths"] == 0 {
                let q = c
                    .call_raw(
                        method::INDEX_QUERY,
                        Some(pid),
                        json!({"op": "search", "args": {"query": last_marker[i], "limit": 5}}),
                        None,
                    )
                    .await;
                q.ok().is_some_and(|v| {
                    v["result"]["results"]
                        .as_array()
                        .is_some_and(|r| r.iter().any(|x| x["name"] == last_marker[i].as_str()))
                })
            } else {
                false
            };
            if found {
                break;
            }
            assert!(Instant::now() < deadline, "{pid} did not converge: {s}");
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    let after = sample(daemon_pid);

    // Plateau: compare the last third of the edit phase with the second.
    eprintln!("soak: {secs} s, {n} edits over {} projects", projects.len());
    for (k, s) in samples.iter().enumerate() {
        eprintln!(
            "  t+{:>4}s  ws {:>7} KiB  handles {:>5}  threads {:>3}",
            k * 5,
            s.0 / 1024,
            s.1,
            s.2
        );
    }
    eprintln!("  after convergence: {after:?}");
    assert!(samples.len() >= 6, "too few samples ({})", samples.len());
    let third = samples.len() / 3;
    let mid = &samples[third..2 * third];
    let last = &samples[2 * third..];
    let max =
        |v: &[(u64, u64, u64)], f: fn(&(u64, u64, u64)) -> u64| v.iter().map(f).max().unwrap();
    let handles = (max(mid, |s| s.1), max(last, |s| s.1));
    let threads = (max(mid, |s| s.2), max(last, |s| s.2));
    let ws = (max(mid, |s| s.0), max(last, |s| s.0));
    eprintln!("  plateau: handles {handles:?}, threads {threads:?}, working set {ws:?}");
    assert!(
        handles.1 <= handles.0 + handles.0 / 4 + 50,
        "handles keep growing: {handles:?}"
    );
    assert!(
        threads.1 <= threads.0 + threads.0 / 4 + 8,
        "threads keep growing: {threads:?}"
    );
    assert!(ws.1 <= ws.0 + ws.0 / 2, "working set keeps growing: {ws:?}");
}
