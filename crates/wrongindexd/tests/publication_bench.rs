//! WI-513: large-run publication benchmark. A real project (a Git checkout
//! named by `WRONGINDEX_BENCH_PROJECT`, cloned so the original is never
//! touched) is indexed natively through the daemon; then, while a second
//! client queries `stats` every 20 ms, the index is rebuilt in a private
//! staging file and swapped in (`rebuild`), and a 300-file edit is written in
//! place (`full`, incremental). Every answer must describe one complete
//! generation (its symbol count is that generation's count, never a
//! partial one); availability and latency are measured and printed.
//!
//!   WRONGINDEX_BENCH_PROJECT=<checkout> cargo test --release -p wrongindexd \
//!     --test publication_bench -- --ignored --nocapture

#![cfg(windows)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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

async fn connect(home: &Path) -> Client {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(c) = Client::connect(&DataRoot::resolve(Some(home)).unwrap(), "bench").await {
            return c;
        }
        assert!(Instant::now() < deadline, "daemon not ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn run_job(c: &mut Client, pid: &str, params: Value) -> (Value, Duration) {
    let t = Instant::now();
    let admitted = c
        .call_raw(method::INDEX_REQUEST, Some(pid), params, None)
        .await
        .unwrap();
    let id = admitted["job"]["jobId"].as_str().unwrap().to_string();
    loop {
        let job = c
            .call_raw(method::JOB_GET, None, json!({"jobId": id}), None)
            .await
            .unwrap();
        if matches!(job["state"].as_str(), Some("succeeded" | "failed")) {
            return (job, t.elapsed());
        }
        assert!(
            t.elapsed() < Duration::from_secs(1800),
            "job did not finish: {job}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[derive(Default)]
struct Probe {
    ok: usize,
    /// Answers labelled `publicationPending` (read after a commit, before
    /// the service recorded it, for longer than the label wait).
    pending: usize,
    failed: Vec<String>,
    latencies_us: Vec<u64>,
    /// generation -> totalSymbols values seen with it
    seen: BTreeMap<i64, Vec<i64>>,
}

async fn probe_loop(home: PathBuf, pid: String, stop: Arc<AtomicBool>) -> Probe {
    let mut c = connect(&home).await;
    let mut p = Probe::default();
    while !stop.load(Ordering::Relaxed) {
        let t = Instant::now();
        match c
            .call_raw(
                method::INDEX_QUERY,
                Some(&pid),
                json!({"op": "stats", "args": {}}),
                None,
            )
            .await
        {
            Ok(v) => {
                p.ok += 1;
                if v["publicationPending"] == true {
                    p.pending += 1;
                }
                p.latencies_us.push(t.elapsed().as_micros() as u64);
                let generation = v["generation"].as_i64().unwrap_or(-1);
                let total = v["result"]["totalSymbols"].as_i64().unwrap_or(-1);
                let e = p.seen.entry(generation).or_default();
                if !e.contains(&total) {
                    e.push(total);
                }
            }
            Err(e) => p.failed.push(e.to_string()),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    p
}

fn pct(v: &mut [u64], q: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_unstable();
    let i = ((v.len() as f64 - 1.0) * q).round() as usize;
    v[i] as f64 / 1000.0
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs WRONGINDEX_BENCH_PROJECT (a Git checkout)"]
async fn large_runs_never_publish_a_partial_generation() {
    let Ok(src) = std::env::var("WRONGINDEX_BENCH_PROJECT") else {
        eprintln!("NOT RUN: set WRONGINDEX_BENCH_PROJECT");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("project");
    let ok = Command::new("git")
        .args(["clone", "-q", "--no-hardlinks", &src])
        .arg(&root)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git clone failed");
    let home = base.join("home");
    let child = Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
        .arg("--data-root")
        .arg(&home)
        .args(["--backend", "native"])
        .env_remove("WRONGINDEX_ENGINE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _d = Daemon(child);
    let mut c = connect(&home).await;
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": base.join("idx")}),
            None,
        )
        .await
        .unwrap();
    let pid = reg["project"]["projectId"].as_str().unwrap().to_string();
    c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let (first, fresh) = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(first["state"], "succeeded", "{first}");
    let files = first["result"]["native"]["filesIndexed"]
        .as_u64()
        .unwrap_or(0);

    let stop = Arc::new(AtomicBool::new(false));
    let probe = tokio::spawn(probe_loop(home.clone(), pid.clone(), stop.clone()));
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 1. Fresh write in staging, swapped in.
    let (rebuilt, rebuild_time) = run_job(&mut c, &pid, json!({"kind": "rebuild"})).await;
    assert_eq!(rebuilt["state"], "succeeded", "{rebuilt}");
    tokio::time::sleep(Duration::from_millis(500)).await;

    // 2. A 300-file edit, written in place.
    let mut edited = 0;
    let out = Command::new("git")
        .args(["ls-files", "*.py", "*.ts", "*.go", "*.rs", "*.js"])
        .current_dir(&root)
        .output()
        .unwrap();
    // One new declaration per file, so the new generation's symbol count
    // differs from the old one and a partial generation would show.
    for (i, rel) in String::from_utf8_lossy(&out.stdout)
        .lines()
        .take(300)
        .enumerate()
    {
        let p = root.join(rel);
        let decl = match p.extension().and_then(|e| e.to_str()) {
            Some("py") => format!("\n\ndef bench_added_{i}():\n    return {i}\n"),
            Some("go") => format!("\nfunc BenchAdded{i}() int {{ return {i} }}\n"),
            Some("rs") => format!("\npub fn bench_added_{i}() -> u32 {{ {i} }}\n"),
            _ => format!("\nexport function benchAdded{i}() {{ return {i}; }}\n"),
        };
        if let Ok(mut t) = std::fs::read_to_string(&p) {
            t.push_str(&decl);
            if std::fs::write(&p, t).is_ok() {
                edited += 1;
            }
        }
    }
    let (updated, update_time) = run_job(&mut c, &pid, json!({"kind": "full"})).await;
    assert_eq!(updated["state"], "succeeded", "{updated}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop.store(true, Ordering::Relaxed);
    let mut p = probe.await.unwrap();

    let total = p.ok + p.failed.len();
    let summary = json!({
        "files": files,
        "freshIndexSecs": fresh.as_secs_f64(),
        "rebuildSecs": rebuild_time.as_secs_f64(),
        "rebuildOutcome": rebuilt["result"]["native"]["outcome"],
        "editedFiles": edited,
        "inPlaceSecs": update_time.as_secs_f64(),
        "inPlaceOutcome": updated["result"]["native"]["outcome"],
        "queries": total,
        "answered": p.ok,
        "publicationPending": p.pending,
        "availabilityPct": if total > 0 { p.ok as f64 * 100.0 / total as f64 } else { 0.0 },
        "latencyMs": {"p50": pct(&mut p.latencies_us, 0.5), "p95": pct(&mut p.latencies_us, 0.95), "p99": pct(&mut p.latencies_us, 0.99), "max": pct(&mut p.latencies_us, 1.0)},
        "generationsSeen": p.seen.iter().map(|(g, t)| json!({"generation": g, "totalSymbols": t})).collect::<Vec<_>>(),
        "failures": p.failed.iter().take(5).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    for (g, totals) in &p.seen {
        assert_eq!(
            totals.len(),
            1,
            "generation {g} answered with different symbol counts {totals:?}: a partial generation was visible"
        );
    }
    assert!(p.seen.len() >= 2, "the probe saw only one generation");
    let counts: Vec<i64> = p.seen.values().map(|t| t[0]).collect();
    assert!(
        counts.windows(2).any(|w| w[0] != w[1]),
        "no generation changed the symbol count: the probe proves nothing"
    );
    assert!(
        p.failed.is_empty(),
        "queries failed during the runs: {:?}",
        p.failed
    );
}
