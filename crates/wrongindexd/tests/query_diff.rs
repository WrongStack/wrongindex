//! WI-302 query differential: every recorded oracle query
//! (`tests/fixtures/oracle/<fixture>/queries.json`) is replayed through the
//! real daemon (`index.query`, ownership held, compatibility worker) against a
//! disposable copy of the oracle-produced `index.db`, and the normalized result
//! must equal WrongStack's recorded result exactly.
//!
//! WrongStack stores absolute paths, so the copy is placed at the oracle's own
//! layout (`<work-root>/<fixture>/{project,index}`, default work root
//! `C:/wrongindex-oracle`). Needs a prepared engine and bun, so it is ignored by
//! default:
//!
//!   WRONGINDEX_TEST_ENGINE=C:/wrongindex-oracle/wrongstack-<sha> \
//!     cargo test -p wrongindexd --test query_diff -- --ignored

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use wrongindex_contracts::method;
use wrongindex_ipc::{Client, DataRoot};

/// Marks a work directory as created by this test, so only those are removed.
const MARKER: &str = ".wrongindex-query-diff";

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

fn work_root() -> PathBuf {
    std::env::var_os("WRONGINDEX_ORACLE_WORK_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\wrongindex-oracle"))
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

/// Prepare `<work-root>/<name>` exactly as the fixture builder laid it out.
/// Refuses to touch a directory this test did not create (e.g. a fixture
/// build in progress).
fn stage(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let work = work_root().join(name);
    if work.exists() {
        assert!(
            work.join(MARKER).exists(),
            "{} exists and was not created by this test; refusing to overwrite",
            work.display()
        );
        std::fs::remove_dir_all(&work).unwrap();
    }
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join(MARKER), b"").unwrap();
    let root = work.join("project");
    let index = work.join("index");
    copy_dir(&repo().join("tests/fixtures/projects").join(name), &root);
    std::fs::create_dir_all(&index).unwrap();
    std::fs::copy(
        repo()
            .join("tests/fixtures/oracle")
            .join(name)
            .join("index.db"),
        index.join("index.db"),
    )
    .unwrap();
    (work, root, index)
}

fn is_volatile(key: &str) -> bool {
    matches!(
        key,
        "lastIndexed"
            | "last_indexed"
            | "durationMs"
            | "elapsedMs"
            | "indexedAt"
            | "generatedAt"
            | "mtimeMs"
            | "mtime_ms"
            | "timestamp"
    )
}

/// Port of `normalize` in `scripts/oracle/build-fixtures.ts`.
fn normalize(v: &Value, root: &str, index: &str, key: &str) -> Value {
    if is_volatile(key) && (v.is_number() || v.is_string()) {
        return json!("<volatile>");
    }
    match v {
        Value::String(s) => {
            let mut out = s.clone();
            for (needle, label) in [(root, "<root>"), (index, "<indexDir>")] {
                for form in [needle.to_string(), needle.replace('\\', "/")] {
                    if out.contains(&form) {
                        out = out.replace(&form, label);
                        out = slash_tokens(&out, label);
                    }
                }
            }
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| normalize(x, root, index, "")).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), normalize(x, root, index, k)))
                .collect::<Map<_, _>>(),
        ),
        other => other.clone(),
    }
}

/// `label[^\s"'),]*` with backslashes turned into forward slashes.
fn slash_tokens(s: &str, label: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(label) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail[label.len()..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | ','))
            .map(|e| e + label.len())
            .unwrap_or(tail.len());
        out.push_str(&tail[..end].replace('\\', "/"));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Recorded args use `<root>/rel`; the live call needs absolute paths.
fn absolutize(v: &Value, root: &Path) -> Value {
    match v {
        Value::String(s) => match s.strip_prefix("<root>/") {
            Some(rel) => json!(root.join(rel.replace('/', "\\")).display().to_string()),
            None => v.clone(),
        },
        Value::Array(a) => Value::Array(a.iter().map(|x| absolutize(x, root)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), absolutize(x, root)))
                .collect(),
        ),
        other => other.clone(),
    }
}

async fn diff_fixture(c: &mut Client, name: &str) -> (usize, Vec<String>) {
    let recorded: Value = serde_json::from_slice(
        &std::fs::read(
            repo()
                .join("tests/fixtures/oracle")
                .join(name)
                .join("queries.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let (work, root, index) = stage(name);
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
        .unwrap_or_else(|e| panic!("{name}: claim failed: {e}"));

    let (root_s, index_s) = (root.display().to_string(), index.display().to_string());
    let mut mismatches = Vec::new();
    let ops = recorded["operations"].as_array().unwrap();
    for (i, op) in ops.iter().enumerate() {
        let args = absolutize(&op["args"], &root);
        let live = c
            .call_raw(
                method::INDEX_QUERY,
                Some(&pid),
                json!({"op": op["op"], "args": args}),
                None,
            )
            .await;
        let ok = match (&live, op.get("result"), op.get("error")) {
            (Ok(v), Some(expected), _) => {
                normalize(&v["result"], &root_s, &index_s, "") == *expected
            }
            (Err(e), None, Some(expected)) => {
                let msg = normalize(&json!(e.to_string()), &root_s, &index_s, "");
                let want = expected["message"].as_str().unwrap_or_default();
                msg.as_str().is_some_and(|m| m.contains(want))
            }
            _ => false,
        };
        if !ok {
            let got = match &live {
                Ok(v) => normalize(&v["result"], &root_s, &index_s, "").to_string(),
                Err(e) => format!("error: {e}"),
            };
            mismatches.push(format!(
                "{name}#{i} {} {}\n  want: {}\n  got:  {}",
                op["op"],
                op["args"],
                op.get("result").or(op.get("error")).unwrap(),
                got.chars().take(2000).collect::<String>()
            ));
        }
    }

    c.call_raw(method::PROJECT_DETACH, Some(&pid), json!({}), None)
        .await
        .unwrap();
    // Releasing ownership must also close the engine's handle on the store:
    // the copied index is deletable right away. The marker goes last so an
    // interrupted cleanup can be retried.
    std::fs::remove_dir_all(&index)
        .unwrap_or_else(|e| panic!("{name}: index still in use after release: {e}"));
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::remove_dir_all(&work).unwrap();
    (ops.len(), mismatches)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine) and bun"]
async fn oracle_queries_match_through_the_daemon() {
    let engine = std::env::var("WRONGINDEX_TEST_ENGINE")
        .expect("set WRONGINDEX_TEST_ENGINE to a prepared WrongStack engine directory");
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let _daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
            .arg("--data-root")
            .arg(&home)
            .arg("--engine")
            .arg(engine)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut c = loop {
        if let Ok(c) = Client::connect(&DataRoot::resolve(Some(&home)).unwrap(), "diff").await {
            break c;
        }
        assert!(Instant::now() < deadline, "daemon not ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    let mut fixtures: Vec<String> = std::fs::read_dir(repo().join("tests/fixtures/oracle"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    fixtures.sort();
    assert!(fixtures.len() >= 8);
    let (mut total, mut all) = (0, Vec::new());
    for f in &fixtures {
        let (n, m) = diff_fixture(&mut c, f).await;
        eprintln!("{f}: {n} operations, {} mismatches", m.len());
        total += n;
        all.extend(m);
    }
    assert!(
        all.is_empty(),
        "{} of {total} oracle queries differ:\n{}",
        all.len(),
        all.join("\n")
    );
    eprintln!("all {total} oracle queries identical through the daemon");
}

#[test]
fn normalization_matches_the_fixture_builder() {
    let root = r"C:\w\basic-ts\project";
    let index = r"C:\w\basic-ts\index";
    let v = json!({
        "file": r"C:\w\basic-ts\project\src\a.ts",
        "msg": r"no index at C:\w\basic-ts\index\index.db, try again",
        "lastIndexed": 123,
        "nested": [{"mtimeMs": "x", "path": "C:/w/basic-ts/project/b.ts"}],
    });
    assert_eq!(
        normalize(&v, root, index, ""),
        json!({
            "file": "<root>/src/a.ts",
            "msg": "no index at <indexDir>/index.db, try again",
            "lastIndexed": "<volatile>",
            "nested": [{"mtimeMs": "<volatile>", "path": "<root>/b.ts"}],
        })
    );
    assert_eq!(
        absolutize(&json!({"f": "<root>/src/a.ts", "q": "x"}), Path::new(root)),
        json!({"f": r"C:\w\basic-ts\project\src\a.ts", "q": "x"})
    );
}
