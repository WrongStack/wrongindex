//! WI-502 gate: the native writer's `index.db` for a fixture is
//! logically identical to the one WrongStack's own indexer wrote (every table,
//! FTS content by rowid, `sqlite_stat1`), apart from per-run values (times,
//! random epoch) and the absolute project root.

use std::path::{Path, PathBuf};

use wrongindex_engine::dbdump::{Volatile, diff, dump};
use wrongindex_engine::native::{NativeError, Rebuild, index_fresh, rebuild, rebuild_replacing};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Long-name absolute path (CI temp dirs use 8.3 short names).
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

fn oracle_root(fixture: &str) -> (String, String) {
    (
        format!(r"C:\wrongindex-oracle\{fixture}\project"),
        format!(r"C:\wrongindex-oracle\{fixture}\index"),
    )
}

#[test]
fn config_only_matches_the_wrongstack_oracle() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("project");
    let index = base.join("index");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);

    let report = index_fresh(&root, &index).unwrap();
    assert_eq!(
        (
            report.files_indexed,
            report.files_empty,
            report.symbols_indexed
        ),
        (2, 1, 10)
    );

    let (oroot, oindex) = oracle_root("config-only");
    let mut want_v = Volatile::logical();
    want_v.rewrite = vec![(oroot, "<root>".into()), (oindex, "<indexDir>".into())];
    let mut got_v = Volatile::logical();
    got_v.rewrite = vec![
        (root.display().to_string(), "<root>".into()),
        (index.display().to_string(), "<indexDir>".into()),
    ];
    let want = dump(
        &repo().join("tests/fixtures/oracle/config-only/index.db"),
        &want_v,
    )
    .unwrap();
    let got = dump(&index.join("index.db"), &got_v).unwrap();
    let d = diff(&want, &got, 10);
    assert!(
        d.is_empty(),
        "native index differs from the oracle:\n{}",
        d.join("\n")
    );

    // Same physical page count as WrongStack's file (no STAT4, no free pages).
    let pages = |p: &Path| {
        let c =
            rusqlite::Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let n: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap();
        let f: i64 = c
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        (n, f)
    };
    assert_eq!(
        pages(&index.join("index.db")),
        pages(&repo().join("tests/fixtures/oracle/config-only/index.db"))
    );

    // The profile inspector accepts it as a WrongStack v5 index.
    let ins = wrongindex_storage::inspect_index(&index.join("index.db"));
    assert_eq!(
        serde_json::to_value(ins.verdict).unwrap(),
        "compatible",
        "{ins:?}"
    );
}

fn page_counts(p: &Path) -> (i64, i64) {
    let c = rusqlite::Connection::open_with_flags(p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    let n: i64 = c.query_row("PRAGMA page_count", [], |r| r.get(0)).unwrap();
    let f: i64 = c
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap();
    (n, f)
}

fn meta(db: &Path, key: &str) -> String {
    let c = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    c.query_row("SELECT value FROM metadata WHERE key = ?1", [key], |r| {
        r.get(0)
    })
    .unwrap()
}

/// Rust crate with a TOML manifest: module owners, an import ref resolved
/// through the crate's module root (`to_file`), `crate:` package labels.
#[test]
fn rs_basic_matches_the_wrongstack_oracle() {
    use wrongindex_engine::relations::{detect_module_roots, hex_sha256, structure_json};

    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("project");
    let index = base.join("index");
    copy_dir(&repo().join("tests/fixtures/projects/rs-basic"), &root);

    let report = index_fresh(&root, &index).unwrap();
    assert_eq!(
        (
            report.files_indexed,
            report.files_empty,
            report.symbols_indexed
        ),
        (3, 0, 10)
    );

    let (oroot, oindex) = oracle_root("rs-basic");
    let mut want_v = Volatile::logical();
    want_v.rewrite = vec![
        (oroot.clone(), "<root>".into()),
        (oindex, "<indexDir>".into()),
    ];
    // A hash over the absolute module-root paths: checked separately below.
    want_v.metadata_keys.push("relation_structure");
    let mut got_v = Volatile::logical();
    got_v.rewrite = vec![
        (root.display().to_string(), "<root>".into()),
        (index.display().to_string(), "<indexDir>".into()),
    ];
    got_v.metadata_keys.push("relation_structure");
    let oracle_db = repo().join("tests/fixtures/oracle/rs-basic/index.db");
    let want = dump(&oracle_db, &want_v).unwrap();
    let got = dump(&index.join("index.db"), &got_v).unwrap();
    let d = diff(&want, &got, 10);
    assert!(
        d.is_empty(),
        "native index differs from the oracle:\n{}",
        d.join("\n")
    );

    // relation_structure = sha256(JSON.stringify(roots)): the native value
    // hashes the native root; with the oracle's root substituted it must be
    // the oracle's value.
    let files: Vec<String> = {
        let c = rusqlite::Connection::open(index.join("index.db")).unwrap();
        c.prepare("SELECT file FROM files")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let root_s = root.display().to_string();
    let structure = detect_module_roots(&root_s, &files);
    let json = structure_json(&structure.roots);
    assert_eq!(structure.roots.len(), 1, "{json}");
    assert_eq!(
        meta(&index.join("index.db"), "relation_structure"),
        hex_sha256(json.as_bytes())
    );
    let portable = |s: &str| s.replace('\\', "/");
    let as_oracle = json.replace(&portable(&root_s), &portable(&oroot));
    assert_eq!(
        meta(&oracle_db, "relation_structure"),
        hex_sha256(as_oracle.as_bytes())
    );

    assert_eq!(
        page_counts(&index.join("index.db")),
        page_counts(&oracle_db)
    );
}

/// Index a fixture natively and compare it with WrongStack's recorded index:
/// every table, `relation_structure` (with the oracle's root substituted),
/// page count. Returns `(files indexed, empty files, symbols)`.
fn assert_matches_oracle(fixture: &str) -> (usize, usize, usize) {
    use wrongindex_engine::relations::{detect_module_roots, hex_sha256, structure_json};

    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("project");
    let index = base.join("index");
    copy_dir(&repo().join("tests/fixtures/projects").join(fixture), &root);

    let report = index_fresh(&root, &index).unwrap();

    let (oroot, oindex) = oracle_root(fixture);
    let mut want_v = Volatile::logical();
    want_v.rewrite = vec![
        (oroot.clone(), "<root>".into()),
        (oindex, "<indexDir>".into()),
    ];
    want_v.metadata_keys.push("relation_structure");
    let mut got_v = Volatile::logical();
    got_v.rewrite = vec![
        (root.display().to_string(), "<root>".into()),
        (index.display().to_string(), "<indexDir>".into()),
    ];
    got_v.metadata_keys.push("relation_structure");
    let oracle_db = repo()
        .join("tests/fixtures/oracle")
        .join(fixture)
        .join("index.db");
    let want = dump(&oracle_db, &want_v).unwrap();
    let got = dump(&index.join("index.db"), &got_v).unwrap();
    let d = diff(&want, &got, 10);
    assert!(
        d.is_empty(),
        "{fixture}: native index differs from the oracle:
{}",
        d.join(
            "
"
        )
    );

    let files: Vec<String> = {
        let c = rusqlite::Connection::open(index.join("index.db")).unwrap();
        c.prepare("SELECT file FROM files")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let root_s = root.display().to_string();
    let json = structure_json(&detect_module_roots(&root_s, &files).roots);
    assert_eq!(
        meta(&index.join("index.db"), "relation_structure"),
        hex_sha256(json.as_bytes())
    );
    let portable = |s: &str| s.replace('\\', "/");
    let as_oracle = json.replace(&portable(&root_s), &portable(&oroot));
    assert_eq!(
        meta(&oracle_db, "relation_structure"),
        hex_sha256(as_oracle.as_bytes()),
        "{fixture}: {json}"
    );
    assert_eq!(
        page_counts(&index.join("index.db")),
        page_counts(&oracle_db),
        "{fixture}"
    );
    let ins = wrongindex_storage::inspect_index(&index.join("index.db"));
    assert_eq!(
        serde_json::to_value(ins.verdict).unwrap(),
        "compatible",
        "{ins:?}"
    );
    (
        report.files_indexed,
        report.files_empty,
        report.symbols_indexed,
    )
}

/// npm package with TS and TSX sources (WI-505/506): classes, interfaces,
/// imports bound through the package, JSX, `<module>` owners, npm labels.
#[test]
fn basic_ts_matches_the_wrongstack_oracle() {
    assert_eq!(assert_matches_oracle("basic-ts"), (4, 0, 14));
}

/// TypeScript files under directory names with spaces and non-ASCII letters.
#[test]
fn paths_unicode_matches_the_wrongstack_oracle() {
    assert_eq!(assert_matches_oracle("paths-unicode"), (3, 0, 4));
}

/// TypeScript files with a BOM, CRLF, astral characters, invalid UTF-8 and
/// no content.
#[test]
fn encodings_match_the_wrongstack_oracle() {
    assert_eq!(assert_matches_oracle("encodings"), (5, 1, 11));
}

#[test]
fn unsupported_projects_and_existing_indexes_are_refused_without_writing() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    // Every language WrongStack detects is native now; a project with no
    // indexable files is still refused, and nothing is created.
    let empty_root = base.join("empty");
    std::fs::create_dir_all(&empty_root).unwrap();
    std::fs::write(empty_root.join("notes.bin"), b"\x00\x01").unwrap();
    let index = base.join("idx-empty");
    let err = index_fresh(&empty_root, &index).unwrap_err();
    assert!(err.to_string().contains("no indexable files"), "{err}");
    assert!(matches!(err, NativeError::Unsupported(_)), "{err}");
    assert!(
        !index.exists(),
        "nothing may be created for an unsupported project"
    );

    let root = base.join("cfg");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let index = base.join("idx");
    index_fresh(&root, &index).unwrap();
    let before = std::fs::read(index.join("index.db")).unwrap();
    let err = index_fresh(&root, &index).unwrap_err();
    assert!(matches!(err, NativeError::Unsupported(_)), "{err}");
    assert_eq!(std::fs::read(index.join("index.db")).unwrap(), before);

    // A Kubernetes manifest is indexed with its infra refs (formerly
    // refused; parity is covered by the KAT and the differential tests).
    let k8s = base.join("k8s-like");
    std::fs::create_dir_all(&k8s).unwrap();
    std::fs::write(
        k8s.join("deploy.yaml"),
        "apiVersion: v1\nkind: Pod\nmetadata:\n  name: p\nspec:\n  containers:\n    - image: busybox\n",
    )
    .unwrap();
    let idx = base.join("idx-k8s");
    index_fresh(&k8s, &idx).unwrap();
    let c = rusqlite::Connection::open(idx.join("index.db")).unwrap();
    let image: String = c
        .query_row(
            "SELECT to_name FROM refs WHERE call_type = 'import'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(image, "busybox");
}

#[test]
fn rebuild_is_a_no_op_without_changes_and_replaces_atomically_otherwise() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("p");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let index = base.join("idx");
    std::fs::create_dir_all(&index).unwrap();
    std::fs::write(index.join(".wrongindex-owner.lock"), b"").unwrap();

    let Rebuild::Written(first) = rebuild(&root, &index).unwrap() else {
        panic!("first run must write");
    };
    match rebuild(&root, &index).unwrap() {
        Rebuild::Unchanged {
            graph_stamp,
            symbols_indexed,
        } => {
            assert_eq!(graph_stamp, Some(first.graph_stamp.clone()));
            assert_eq!(symbols_indexed, first.symbols_indexed);
        }
        other => panic!("expected no-op, got {other:?}"),
    }
    // A content-preserving touch is still a no-op.
    let f = root.join("package.json");
    std::fs::write(&f, std::fs::read(&f).unwrap()).unwrap();
    assert!(matches!(
        rebuild(&root, &index).unwrap(),
        Rebuild::Unchanged { .. }
    ));

    std::fs::write(
        root.join("config/new.yaml"),
        "added: 1
",
    )
    .unwrap();
    // A new file: WrongStack's run is incremental, and so is the native one
    // (in place, WI-515).
    let Rebuild::Updated {
        report: second,
        changed_files,
    } = rebuild(&root, &index).unwrap()
    else {
        panic!("a new file must update the index");
    };
    assert_eq!(second.symbols_indexed, 11);
    assert_eq!(changed_files, 1);
    // Only the index file was written: the owner lock and nothing else remain.
    let mut names: Vec<String> = std::fs::read_dir(&index)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec![".wrongindex-owner.lock", "index.db"]);

    // Another open connection (e.g. a WrongStack reader) does not block the
    // in-place update (WAL), as with WrongStack's own incremental run: the
    // reader keeps its snapshot until it reads again.
    std::fs::write(
        root.join("config/more.yaml"),
        "x: 1
",
    )
    .unwrap();
    let reader = rusqlite::Connection::open(index.join("index.db")).unwrap();
    let count = |c: &rusqlite::Connection| -> i64 {
        c.query_row("SELECT count(*) FROM symbols", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count(&reader), 11);
    assert!(matches!(
        rebuild(&root, &index).unwrap(),
        Rebuild::Updated { .. }
    ));
    assert_eq!(count(&reader), 12);
    drop(reader);
    let leftovers: Vec<String> = std::fs::read_dir(&index)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".wrongindex-staging"))
        .collect();
    assert!(leftovers.is_empty(), "no staging directory is left");
    assert!(matches!(
        rebuild(&root, &index).unwrap(),
        Rebuild::Unchanged { .. }
    ));
}

#[test]
fn pathologically_nested_sources_are_refused_without_crashing() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("deep");
    std::fs::create_dir_all(&root).unwrap();
    // 200k nested parentheses: would overflow a recursive parser's stack.
    let deep = format!(
        "export const x = {}1{};\n",
        "(".repeat(200_000),
        ")".repeat(200_000)
    );
    std::fs::write(root.join("deep.ts"), deep).unwrap();
    let index = base.join("idx");
    // The oxc port refuses it before recursing; the file then goes to
    // WrongStack's own TypeScript parser (Bun/Node.js child), which either
    // indexes it or fails as a child - never this process.
    match index_fresh(&root, &index) {
        Ok(_) => {}
        Err(NativeError::Unsupported(_)) => assert!(
            !index.exists(),
            "nothing may be written for a refused project"
        ),
        other => panic!("expected an index or a refusal, got {other:?}"),
    }

    // Ordinary nesting is unaffected.
    let ok = base.join("ok");
    std::fs::create_dir_all(&ok).unwrap();
    std::fs::write(
        ok.join("a.ts"),
        format!(
            "export const y = {}1{};\n",
            "(".repeat(100),
            ")".repeat(100)
        ),
    )
    .unwrap();
    index_fresh(&ok, &base.join("idx-ok")).unwrap();
}

/// Python through the interpreter WrongStack would pick (the oracle was
/// recorded with `python` = 3.14.8; `python3` was the Store stub).
#[test]
fn py_basic_matches_the_wrongstack_oracle() {
    if wrongindex_engine::parse::py::python().is_none() {
        eprintln!("NOT RUN: no Python interpreter");
        return;
    }
    let _serial = toolchain_serial();
    assert_eq!(assert_matches_oracle("py-basic"), (3, 0, 9));
}

/// Toolchain fixtures run one at a time. Their parsers keep WrongStack's
/// own per-chunk deadlines (`batchTimeoutMs`: 15 s + 1.5 s per file); on a
/// 2-core CI runner, a cold `go build`, Node compiling the WASM grammars and
/// Python started together pushed the tree-sitter chunk past 24 s once
/// (2026-11-02, run for b063752). The product keeps the deadline; the
/// tests stop competing with each other.
fn toolchain_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Go through `go run` of WrongStack's own `go/ast` program.
#[test]
fn go_basic_matches_the_wrongstack_oracle() {
    if wrongindex_engine::parse::go::go().is_none() {
        eprintln!("NOT RUN: no Go toolchain");
        return;
    }
    let _serial = toolchain_serial();
    assert_eq!(assert_matches_oracle("go-basic"), (2, 0, 6));
}

/// C, Java, Ruby, shell, SQL and a Dockerfile through WrongStack's own
/// tree-sitter route (bundled visitor, queries, runtime and grammars).
#[test]
fn treesitter_mix_matches_the_wrongstack_oracle() {
    if wrongindex_engine::parse::treesitter::runtime().is_none() {
        eprintln!("NOT RUN: neither Bun nor Node.js on PATH");
        return;
    }
    let _serial = toolchain_serial();
    assert_eq!(assert_matches_oracle("treesitter-mix"), (6, 0, 17));
}

/// The native writer replaces the whole file, so it must refuse any existing
/// index it would not reproduce exactly: one carrying data WrongStack must
/// preserve, another schema version, or an unreadable file. Each is left
/// byte for byte; a plain baseline index is still replaced.
#[test]
fn rebuild_never_replaces_an_index_it_would_not_reproduce() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("p");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let fresh = |name: &str| {
        let index = base.join(name);
        std::fs::create_dir_all(&index).unwrap();
        assert!(matches!(
            rebuild(&root, &index).unwrap(),
            Rebuild::Written(_)
        ));
        index
    };
    let change = |n: u32| {
        std::fs::write(root.join("changed.json"), format!("{{\"n\": {n}}}\n")).unwrap();
    };
    let refused = |index: &Path, n: u32, want: &str| {
        let before = std::fs::read(index.join("index.db")).unwrap();
        change(n);
        match rebuild(&root, index) {
            Err(NativeError::Unsupported(why)) => assert!(why.contains(want), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert_eq!(std::fs::read(index.join("index.db")).unwrap(), before);
    };

    // Extra data WrongStack would keep (an unknown table).
    let extra = fresh("extra");
    {
        let c = rusqlite::Connection::open(extra.join("index.db")).unwrap();
        c.execute_batch("CREATE TABLE x_plugin_notes(id INTEGER PRIMARY KEY, note TEXT); INSERT INTO x_plugin_notes(note) VALUES ('keep me');")
            .unwrap();
        c.pragma_update(None, "journal_mode", "DELETE").unwrap();
    }
    refused(&extra, 1, "outside the baseline (x_plugin_notes)");

    // Another schema version.
    let older = fresh("older");
    {
        let c = rusqlite::Connection::open(older.join("index.db")).unwrap();
        c.execute("UPDATE metadata SET value = '4' WHERE key = 'version'", [])
            .unwrap();
        c.pragma_update(None, "journal_mode", "DELETE").unwrap();
    }
    refused(&older, 2, "has schema version 4");

    // An unreadable file.
    let broken = fresh("broken");
    std::fs::write(broken.join("index.db"), b"definitely not sqlite").unwrap();
    refused(&broken, 3, "cannot be read");

    // A baseline index is still written (updated in place).
    let plain = fresh("plain");
    change(4);
    assert!(matches!(
        rebuild(&root, &plain).unwrap(),
        Rebuild::Updated { .. }
    ));
}

/// Rows WrongStack's enrichment passes (`/codebase-map --enrich/--embed`)
/// write: concepts, file vectors, subsystems, concept edges.
const ENRICH: [&str; 4] = [
    "INSERT INTO file_concepts(file, content_hash, summary, state, model, updated_at) SELECT file, content_hash, 'what it is for', 'ready', 'm', 7 FROM files ORDER BY file LIMIT 2",
    "INSERT INTO file_vectors(file, vector, source_hash, provider) SELECT file, x'0001ff', 'h', 'p' FROM files ORDER BY file LIMIT 1",
    "INSERT INTO subsystems(id, name, summary, member_files, model, updated_at) VALUES ('core', 'Core', 's', '[\"a\"]', 'm', 9)",
    "INSERT INTO concept_edges(from_id, to_id, relation) VALUES ('core', 'ui', 'uses')",
];

fn enrichment(index: &Path) -> Vec<String> {
    let c = rusqlite::Connection::open(index.join("index.db")).unwrap();
    let mut out = Vec::new();
    for t in [
        "file_concepts",
        "file_vectors",
        "subsystems",
        "concept_edges",
    ] {
        let mut st = c
            .prepare(&format!("SELECT * FROM {t} ORDER BY 1, 2"))
            .unwrap();
        let n = st.column_count();
        let mut rows = st.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            let cells: Vec<String> = (0..n)
                .map(|i| format!("{:?}", r.get::<_, rusqlite::types::Value>(i).unwrap()))
                .collect();
            out.push(format!("{t}: {}", cells.join(" | ")));
        }
    }
    out
}

/// An enriched index whose project then changes, as WrongStack's indexer
/// finds it after `/codebase-map --enrich/--embed`.
fn enriched_index(base: &Path, name: &str) -> (PathBuf, PathBuf, Vec<String>) {
    let root = base.join(name);
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let index = base.join(format!("{name}-idx"));
    std::fs::create_dir_all(&index).unwrap();
    assert!(matches!(
        rebuild(&root, &index).unwrap(),
        Rebuild::Written(_)
    ));
    {
        let c = rusqlite::Connection::open(index.join("index.db")).unwrap();
        for sql in ENRICH {
            assert!(c.execute(sql, []).unwrap() > 0, "{sql}");
        }
        c.pragma_update(None, "journal_mode", "DELETE").unwrap();
    }
    let rows = enrichment(&index);
    assert_eq!(rows.len(), 5, "{rows:?}");
    std::fs::write(root.join("added.json"), "{\"x\": 1}\n").unwrap();
    (root, index, rows)
}

/// WI-512: WrongStack's indexer never writes the enrichment tables, so its
/// runs keep their rows verbatim; the native writer does the same, in place
/// (incremental) and when it builds a fresh file (rows carried across).
#[test]
fn enrichment_rows_survive_native_runs_as_they_do_wrongstack_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());

    let (root, index, rows) = enriched_index(&base, "in-place");
    assert!(matches!(
        rebuild(&root, &index).unwrap(),
        Rebuild::Updated { .. }
    ));
    assert_eq!(enrichment(&index), rows);

    // A state the incremental writer does not take (a stale rank version):
    // a fresh file, published with the rows carried over.
    let (root, index, rows) = enriched_index(&base, "fresh");
    {
        let c = rusqlite::Connection::open(index.join("index.db")).unwrap();
        c.execute(
            "UPDATE metadata SET value = '0' WHERE key = 'rank_version'",
            [],
        )
        .unwrap();
        c.pragma_update(None, "journal_mode", "DELETE").unwrap();
    }
    assert!(matches!(
        rebuild(&root, &index).unwrap(),
        Rebuild::Written(_)
    ));
    assert_eq!(enrichment(&index), rows);
    assert!(
        index_files(&index)
            .iter()
            .any(|f| f.ends_with("added.json")),
        "the change itself was indexed: {:?}",
        index_files(&index)
    );
}

/// A relation-graph version change makes WrongStack clear the whole index,
/// enrichment included (`clearAll`); a maintenance rebuild is WrongStack's
/// forced run, which clears too (the old file is kept as a backup).
#[test]
fn enrichment_is_dropped_exactly_where_wrongstack_clears_it() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());

    let (root, index, _) = enriched_index(&base, "graph-stale");
    {
        let c = rusqlite::Connection::open(index.join("index.db")).unwrap();
        c.execute(
            "UPDATE metadata SET value = '1' WHERE key = 'relation_graph_version'",
            [],
        )
        .unwrap();
        c.pragma_update(None, "journal_mode", "DELETE").unwrap();
    }
    assert!(matches!(
        rebuild(&root, &index).unwrap(),
        Rebuild::Written(_)
    ));
    assert!(enrichment(&index).is_empty());

    let (root, index, _) = enriched_index(&base, "forced");
    let replaced = rebuild_replacing(&root, &index).unwrap();
    assert!(enrichment(&index).is_empty());
    let backup = PathBuf::from(replaced.backup.expect("old file kept"));
    assert_eq!(
        enrichment_at(&backup).len(),
        5,
        "the backup still holds the enrichment"
    );
}

fn enrichment_at(db: &Path) -> Vec<String> {
    let dir = db.parent().unwrap().join("backup-view");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(db, dir.join("index.db")).unwrap();
    enrichment(&dir)
}

fn index_files(index: &Path) -> Vec<String> {
    let c = rusqlite::Connection::open(index.join("index.db")).unwrap();
    let mut st = c.prepare("SELECT file FROM files ORDER BY file").unwrap();
    st.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Maintenance replaces what the regular writer refuses, keeping a backup,
/// but never an index a newer WrongStack wrote (security review WI-604).
#[test]
fn maintenance_rebuild_never_replaces_a_newer_schema() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("p");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let with_version = |name: &str, v: &str| {
        let index = base.join(name);
        std::fs::create_dir_all(&index).unwrap();
        assert!(matches!(
            rebuild(&root, &index).unwrap(),
            Rebuild::Written(_)
        ));
        let c = rusqlite::Connection::open(index.join("index.db")).unwrap();
        c.execute("UPDATE metadata SET value = ?1 WHERE key = 'version'", [v])
            .unwrap();
        c.pragma_update(None, "journal_mode", "DELETE").unwrap();
        index
    };
    for (name, v) in [("newer", "6"), ("unknown", "v6")] {
        let index = with_version(name, v);
        let before = std::fs::read(index.join("index.db")).unwrap();
        match rebuild_replacing(&root, &index) {
            Err(NativeError::Unsupported(why)) => {
                assert!(why.contains("newer WrongStack"), "{why}")
            }
            other => panic!("expected a refusal for version {v}, got {other:?}"),
        }
        assert_eq!(std::fs::read(index.join("index.db")).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(&index).unwrap().count(),
            1,
            "nothing else written"
        );
    }
    // An older schema is replaced and kept as a backup.
    let older = with_version("older", "4");
    let before = std::fs::read(older.join("index.db")).unwrap();
    let r = rebuild_replacing(&root, &older).unwrap();
    let backup = r.backup.expect("backup kept");
    assert_eq!(std::fs::read(&backup).unwrap(), before);
}

#[cfg(windows)]
fn junction(link: &Path, target: &Path) {
    let ok = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "mklink /J {}", link.display());
}

/// Security review (WI-604) for targeted runs: a named path that leaves the
/// checkout through a junction is refused before anything behind it is
/// stat'ed or read, as a file or as a directory to expand; a junction that
/// stays inside, and a checkout reached through a junction, are fine.
#[cfg(windows)]
#[test]
fn targeted_paths_never_leave_the_checkout_through_links() {
    use wrongindex_engine::native::update_files;
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("p");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let index = base.join("idx");
    std::fs::create_dir_all(&index).unwrap();
    assert!(matches!(
        rebuild(&root, &index).unwrap(),
        Rebuild::Written(_)
    ));
    let secret = base.join("secret");
    std::fs::create_dir_all(&secret).unwrap();
    std::fs::write(secret.join("leak.json"), "{\"secret\": 1}\n").unwrap();
    junction(&root.join("config").join("linked"), &secret);
    let before = std::fs::read(index.join("index.db")).unwrap();
    for target in [
        root.join("config/linked/leak.json"),
        root.join("config/linked"),
        root.join("config/linked/missing.json"),
    ] {
        let t = target.to_string_lossy().into_owned();
        match update_files(&root, &index, std::slice::from_ref(&t)) {
            Err(NativeError::Unsupported(why)) => {
                assert!(why.contains("leaves the checkout"), "{why}")
            }
            other => panic!("{t}: expected a refusal, got {other:?}"),
        }
        assert_eq!(std::fs::read(index.join("index.db")).unwrap(), before);
    }

    // Inside to inside: indexed like any other path.
    std::fs::remove_dir(root.join("config/linked")).unwrap();
    junction(&root.join("alias"), &root.join("config"));
    let t = root.join("alias/app.yaml").to_string_lossy().into_owned();
    let r = update_files(&root, &index, std::slice::from_ref(&t)).unwrap();
    assert!(
        matches!(
            r,
            Rebuild::Updated {
                changed_files: 1,
                ..
            }
        ),
        "{r:?}"
    );

    // A checkout reached through a junction is still inside itself.
    let via = base.join("via");
    junction(&via, &root);
    let index2 = base.join("idx2");
    std::fs::create_dir_all(&index2).unwrap();
    std::fs::remove_dir(root.join("alias")).unwrap();
    assert!(matches!(
        rebuild(&via, &index2).unwrap(),
        Rebuild::Written(_)
    ));
    std::fs::write(root.join("config/new.json"), "{\"n\": 1}\n").unwrap();
    let t = via.join("config/new.json").to_string_lossy().into_owned();
    let r = update_files(&via, &index2, std::slice::from_ref(&t)).unwrap();
    assert!(
        matches!(
            r,
            Rebuild::Updated {
                changed_files: 1,
                ..
            }
        ),
        "{r:?}"
    );
}

/// Security review of the SAGE heal (WI-604): a memory store that is a link
/// out of the checkout is never written; a regular store is healed. The
/// link part needs the symbolic-link privilege (CI runs elevated); without
/// it that part is reported NOT RUN and the regular-store part still runs.
#[cfg(windows)]
#[test]
fn sage_heal_never_writes_through_a_link_out_of_the_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("p");
    copy_dir(&repo().join("tests/fixtures/projects/basic-ts"), &root);
    let index = base.join("idx");
    std::fs::create_dir_all(&index).unwrap();
    assert!(matches!(
        rebuild(&root, &index).unwrap(),
        Rebuild::Written(_)
    ));
    let mem = root.join(".wrongstack").join("memories");
    std::fs::create_dir_all(&mem).unwrap();
    let record = |sym: &str| {
        format!(
            "{{\"text\":\"greets\",\"anchors\":[{{\"path\":\"src/greet.ts\",\"symbol\":\"{sym}\"}}]}}\n"
        )
    };
    let rename = |from: &str, to: &str| {
        let p = root.join("src/greet.ts");
        let t = std::fs::read_to_string(&p).unwrap();
        std::fs::write(
            &p,
            t.replace(&format!("function {from}("), &format!("function {to}(")),
        )
        .unwrap();
    };
    let outside = base.join("outside.jsonl");
    std::fs::write(&outside, record("greet")).unwrap();
    let healed = |r: Rebuild| match r {
        Rebuild::Updated { report, .. } => report.sage_healed,
        other => panic!("expected an in-place run, got {other:?}"),
    };
    let mut from = "greet";
    match std::os::windows::fs::symlink_file(&outside, mem.join("memories.jsonl")) {
        Ok(()) => {
            rename("greet", "greetPerson");
            from = "greetPerson";
            let h = healed(rebuild(&root, &index).unwrap()).expect("a move was detected");
            assert_eq!(h.updated_jsonl_count, 0);
            assert_eq!(std::fs::read_to_string(&outside).unwrap(), record("greet"));
            std::fs::remove_file(mem.join("memories.jsonl")).unwrap();
        }
        Err(e) => eprintln!("NOT RUN (link part): cannot create a file symlink: {e}"),
    }
    std::fs::write(mem.join("memories.jsonl"), record(from)).unwrap();
    rename(from, "greetAgain");
    let h = healed(rebuild(&root, &index).unwrap()).expect("a move was detected");
    assert_eq!(h.updated_jsonl_count, 1);
    assert_eq!(
        std::fs::read_to_string(mem.join("memories.jsonl")).unwrap(),
        record("greetAgain").trim_end().to_string() + "\n"
    );
    assert!(
        std::fs::read_dir(&mem)
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")),
        "no temporary file left behind"
    );
}

/// WI-513: publication never waits for readers. With a reader holding an
/// open read transaction on the live index, a fresh native write (here the
/// maintenance rebuild, which takes the staged path) is published anyway:
/// the reader keeps its snapshot, a new reader sees the new generation,
/// and the old index is kept as a consistent backup.
#[test]
fn publication_does_not_wait_for_readers() {
    let tmp = tempfile::tempdir().unwrap();
    let base = long(tmp.path());
    let root = base.join("p");
    copy_dir(&repo().join("tests/fixtures/projects/config-only"), &root);
    let index = base.join("idx");
    std::fs::create_dir_all(&index).unwrap();
    assert!(matches!(
        rebuild(&root, &index).unwrap(),
        Rebuild::Written(_)
    ));
    let count = |c: &rusqlite::Connection| -> i64 {
        c.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap()
    };
    let reader = rusqlite::Connection::open(index.join("index.db")).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let before = count(&reader);
    std::fs::write(root.join("config/extra.json"), "{\"x\": 1}\n").unwrap();
    let r = rebuild_replacing(&root, &index).unwrap();
    assert_eq!(count(&reader), before, "the open snapshot is kept");
    reader.execute_batch("COMMIT").unwrap();
    let fresh = rusqlite::Connection::open(index.join("index.db")).unwrap();
    assert_eq!(
        count(&fresh),
        before + 1,
        "a new reader sees the new generation"
    );
    let backup = rusqlite::Connection::open(r.backup.expect("backup kept")).unwrap();
    assert_eq!(count(&backup), before, "the backup is the old generation");
    drop((reader, fresh, backup));
    // The staged path of the regular writer, too (a stale rank version
    // sends it there), with a reader open.
    let reader = rusqlite::Connection::open(index.join("index.db")).unwrap();
    reader
        .execute(
            "UPDATE metadata SET value = '0' WHERE key = 'rank_version'",
            [],
        )
        .unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let before = count(&reader);
    std::fs::write(root.join("config/extra2.json"), "{\"y\": 1}\n").unwrap();
    assert!(matches!(
        rebuild(&root, &index).unwrap(),
        Rebuild::Written(_)
    ));
    assert_eq!(count(&reader), before);
    reader.execute_batch("COMMIT").unwrap();
    assert_eq!(count(&reader), before + 1);
}
