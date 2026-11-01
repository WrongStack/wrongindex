//! Native writer vs WrongStack's own indexer on generated projects.
//!
//! Each scenario builds a project, indexes it twice into fresh directories -
//! once with WrongStack (`runIndexerWithStore` through the compatibility
//! worker) and once natively - and requires the two databases to be
//! logically identical (every table, FTS content by rowid, `sqlite_stat1`).
//!
//! Needs a prepared WrongStack engine and bun, so it is ignored by default:
//!
//!   WRONGINDEX_TEST_ENGINE=C:/wrongindex-oracle/wrongstack-<sha> \
//!     cargo test -p wrongindex-engine --test differential -- --ignored

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use wrongindex_engine::dbdump::{Volatile, diff, dump};
use wrongindex_engine::native::index_fresh;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn long(p: &Path) -> PathBuf {
    let c = std::fs::canonicalize(p).unwrap();
    PathBuf::from(c.display().to_string().trim_start_matches(r"\\?\"))
}

/// Index `root` into `index` with WrongStack itself.
fn wrongstack_index(root: &Path, index: &Path) -> Value {
    let engine = std::env::var("WRONGINDEX_TEST_ENGINE").expect("WRONGINDEX_TEST_ENGINE");
    let mut child = Command::new("bun")
        .arg(repo().join("packages/legacy-worker/worker.ts"))
        .arg("--engine")
        .arg(engine)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("bun");
    let mut stdin = child.stdin.take().unwrap();
    let req = json!({"id": 1, "op": "index", "args": {"projectRoot": root, "indexDir": index}});
    writeln!(stdin, "{req}").unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    drop(stdin);
    let _ = child.wait();
    let v: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("worker said: {line}"));
    assert_eq!(v["ok"], true, "WrongStack index failed: {v}");
    v["result"].clone()
}

fn git(root: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn compare(name: &str, root: &Path, base: &Path) -> Vec<String> {
    let a = base.join("idx-wrongstack");
    let b = base.join("idx-native");
    let ws = wrongstack_index(root, &a);
    let native = index_fresh(root, &b).unwrap_or_else(|e| panic!("{name}: native: {e}"));
    let v = Volatile::logical();
    let want = dump(&a.join("index.db"), &v).unwrap();
    let got = dump(&b.join("index.db"), &v).unwrap();
    let mut out: Vec<String> = diff(&want, &got, 8)
        .into_iter()
        .map(|l| format!("{name}: {l}"))
        .collect();
    if ws["symbolsIndexed"] != json!(native.symbols_indexed) {
        out.push(format!(
            "{name}: symbols {} (WrongStack) vs {} (native)",
            ws["symbolsIndexed"], native.symbols_indexed
        ));
    }
    eprintln!(
        "{name}: {} files, {} symbols, {} differences",
        want["files"].len(),
        native.symbols_indexed,
        out.len()
    );
    out
}

fn scenario_files(root: &Path) {
    write(
        root,
        "package.json",
        "{\n  \"name\": \"demo\",\n  \"scripts\": {\"build\": \"x\", \"test:unit\": \"y\"}\n}\n",
    );
    write(
        root,
        "config/app.yaml",
        "server:\n  port: 8080\n  hosts:\n    - a\n    - b\nbase: &b\n  x: 1\nother:\n  <<: *b\nrun: |\n  key: inside\n",
    );
    write(root, "config/empty.json", "");
    write(root, "config/array.json", "[1, 2, 3]");
    write(root, "config/crlf.yaml", "a: 1\r\nb:\r\n  - c: \"x\"\r\n");
    write(
        root,
        "config/bom.json",
        "\u{FEFF}{\"bom\": true, \"public\": 1}",
    );
    write(
        root,
        "data/ünïcödé.json",
        "{\"ключ\": 1, \"😀\": {\"x\": 1}}",
    );
    write(
        root,
        "schemas/user.schema.json",
        "{\"$schema\": \"x\", \"$defs\": {\"User\": {}, \"Team\": {}}}",
    );
    write(
        root,
        "tsconfig.json",
        "{\n  // c\n  \"compilerOptions\": {\"strict\": true, \"outDir\": \"dist\"},\n}\n",
    );
    write(root, "ignored/skip.json", "{\"never\": 1}");
    write(root, ".gitignore", "ignored/\n");
    for i in 0..57 {
        let body = if i % 9 == 0 {
            String::new()
        } else {
            format!("{{\"k{i}\": {i}, \"nested{i}\": {{\"a\": 1}}}}")
        };
        write(root, &format!("many/f{i:03}.json"), &body);
    }
    write(
        root,
        "many/deep/a/b/c.yaml",
        "deep: true\nlist:\n  - name: x\n",
    );
}

#[test]
#[ignore = "needs WRONGINDEX_TEST_ENGINE (prepared WrongStack engine) and bun"]
fn native_matches_wrongstack_on_generated_projects() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let mut failures = Vec::new();

    // 1. Plain directory (filesystem walk).
    let s = base.join("walk");
    let root = s.join("project");
    scenario_files(&root);
    failures.extend(compare("walk", &root, &s));

    // 2. Clean Git checkout: git_blob trust stamps.
    let s = base.join("git-clean");
    let root = s.join("project");
    scenario_files(&root);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    failures.extend(compare("git-clean", &root, &s));
    let stamped = |idx: &Path| -> usize {
        let c = rusqlite::Connection::open_with_flags(
            idx.join("index.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        c.query_row("SELECT count(*) FROM files WHERE git_blob <> ''", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap() as usize
    };
    assert_eq!(
        stamped(&s.join("idx-native")),
        67,
        "every clean file is stamped"
    );

    // 3. Git checkout with modified, staged-new and untracked files.
    let s = base.join("git-dirty");
    let root = s.join("project");
    scenario_files(&root);
    git(&root, &["init", "-q"]);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "init"]);
    write(
        &root,
        "config/app.yaml",
        "server:\n  port: 9090\nchanged: yes\n",
    );
    write(&root, "staged.json", "{\"staged\": 1}");
    git(&root, &["add", "staged.json"]);
    write(&root, "untracked.yaml", "fresh: 1\n");
    std::fs::remove_file(root.join("many/f001.json")).unwrap();
    failures.extend(compare("git-dirty", &root, &s));
    // The modified and the untracked file carry no stamp; a staged-new file
    // whose working copy equals the staged blob is clean and is stamped.
    assert_eq!(stamped(&s.join("idx-native")), 66);

    // 4. Monorepo-style layout: the package label comes from the path.
    let s = base.join("layout");
    let root = s.join("packages").join("widgets");
    write(&root, "package.json", "{\"name\": \"widgets\"}");
    write(&root, "conf/x.yaml", "a: 1\n");
    failures.extend(compare("layout", &root, &s));

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
