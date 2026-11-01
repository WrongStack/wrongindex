//! WI-511 gate: every recorded WrongStack query (`queries.json`, 8 fixtures)
//! answered natively over a copy of the oracle `index.db`, read-only.
//! Ported operations must match exactly; operations not ported yet are
//! counted as NOT RUN and reported, never silently treated as passing.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Map, Value, json};
use wrongindex_engine::query::{QueryError, Reader};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
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

fn absolutize(v: &Value, root: &str) -> Value {
    match v {
        Value::String(s) => match s.strip_prefix("<root>/") {
            Some(rel) => json!(format!("{root}\\{}", rel.replace('/', "\\"))),
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

#[derive(Default, Debug)]
struct Tally {
    pass: usize,
    fail: usize,
    not_run: usize,
}

#[test]
fn native_queries_match_recorded_wrongstack_results() {
    let tmp = tempfile::tempdir().unwrap();
    let mut tally: BTreeMap<String, Tally> = BTreeMap::new();
    let mut failures = Vec::new();
    let mut fixtures: Vec<String> = std::fs::read_dir(repo().join("tests/fixtures/oracle"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    fixtures.sort();
    assert!(fixtures.len() >= 8);
    for f in &fixtures {
        let src = repo().join("tests/fixtures/oracle").join(f);
        let recorded: Value =
            serde_json::from_slice(&std::fs::read(src.join("queries.json")).unwrap()).unwrap();
        // A private copy, so even a buggy reader could not touch the fixture.
        let index = tmp.path().join(f);
        std::fs::create_dir_all(&index).unwrap();
        std::fs::copy(src.join("index.db"), index.join("index.db")).unwrap();
        let reader = Reader::open(&index).unwrap();
        let root = format!(r"C:\wrongindex-oracle\{f}\project");
        let index_s = index.display().to_string();
        for (i, op) in recorded["operations"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            let name = op["op"].as_str().unwrap().to_string();
            let t = tally.entry(name.clone()).or_default();
            let mut args = absolutize(&op["args"], &root);
            // The recorder strips `projectRoot` (build-fixtures.ts); the
            // recorded call carried it (`context` relativises against it).
            args["projectRoot"] = json!(root);
            match (reader.call(&name, &args), op.get("result"), op.get("error")) {
                (Err(QueryError::Unsupported(_)), _, _) => t.not_run += 1,
                (Ok(v), Some(want), _) if normalize(&v, &root, &index_s, "") == *want => {
                    t.pass += 1
                }
                (got, want_ok, want_err) => {
                    t.fail += 1;
                    if failures.len() < 12 {
                        let got = match got {
                            Ok(v) => normalize(&v, &root, &index_s, "").to_string(),
                            Err(e) => format!("error: {e}"),
                        };
                        failures.push(format!(
                            "{f}#{i} {name} {}\n  want: {}\n  got:  {}",
                            op["args"],
                            want_ok.or(want_err).unwrap(),
                            got.chars().take(1500).collect::<String>()
                        ));
                    }
                }
            }
        }
        drop(reader);
        // Reading never modified the copy.
        assert_eq!(
            std::fs::read(index.join("index.db")).unwrap(),
            std::fs::read(src.join("index.db")).unwrap(),
            "{f}: a native read changed index.db"
        );
    }
    for (op, t) in &tally {
        eprintln!(
            "{op:14} pass {:4}  fail {:3}  not run {:4}",
            t.pass, t.fail, t.not_run
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(tally["stats"].pass == 8, "stats must be served natively");
    assert_eq!(tally["search"].pass, 178);
    assert_eq!(tally["context"].pass, 16);
    assert_eq!(tally["vectorSearch"].pass, 8);
    for (op, n) in [
        ("packageGraph", 8),
        ("fileGraph", 18),
        ("symbolGraph", 30),
        ("incomingCalls", 243),
        ("outgoingCalls", 162),
    ] {
        assert_eq!(tally[op].pass, n, "{op} must be served natively");
    }
}

#[test]
fn reader_never_leaves_wal_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = repo().join("tests/fixtures/oracle/basic-ts/index.db");
    std::fs::copy(&src, tmp.path().join("index.db")).unwrap();
    {
        let r = Reader::open(tmp.path()).unwrap();
        r.stats().unwrap();
    }
    let names: Vec<String> = std::fs::read_dir(tmp.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["index.db"]);
}

#[test]
fn indexes_wrongstack_would_rewrite_on_open_are_not_served() {
    let tmp = tempfile::tempdir().unwrap();
    let src = repo().join("tests/fixtures/oracle/basic-ts/index.db");
    let db = tmp.path().join("index.db");
    std::fs::copy(&src, &db).unwrap();
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute("DELETE FROM symbols_fts WHERE rowid = 1", [])
            .unwrap();
    }
    let r = Reader::open(tmp.path()).unwrap();
    assert!(matches!(
        r.call("search", &json!({"query": "greet"})),
        Err(QueryError::Unsupported(_))
    ));
    drop(r);
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute("UPDATE metadata SET value = '4' WHERE key = 'version'", [])
            .unwrap();
    }
    let r = Reader::open(tmp.path()).unwrap();
    assert!(matches!(
        r.call("stats", &json!({})),
        Err(QueryError::Unsupported(_))
    ));
}
