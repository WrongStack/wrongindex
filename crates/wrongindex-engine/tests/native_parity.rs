//! WI-502 gate: the native writer's `index.db` for a fixture is
//! logically identical to the one WrongStack's own indexer wrote (every table,
//! FTS content by rowid, `sqlite_stat1`), apart from per-run values (times,
//! random epoch) and the absolute project root.

use std::path::{Path, PathBuf};

use wrongindex_engine::dbdump::{Volatile, diff, dump};
use wrongindex_engine::native::{NativeError, Rebuild, index_fresh, rebuild};

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
        Rebuild::Unchanged { graph_stamp } => {
            assert_eq!(graph_stamp, Some(first.graph_stamp.clone()))
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
    let Rebuild::Written(second) = rebuild(&root, &index).unwrap() else {
        panic!("a new file must rewrite the index");
    };
    assert_eq!(second.symbols_indexed, 11);
    // Only the index file was replaced: the owner lock and nothing else remain.
    let mut names: Vec<String> = std::fs::read_dir(&index)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec![".wrongindex-owner.lock", "index.db"]);

    // A WAL file next to the index means another connection: refuse.
    std::fs::write(
        root.join("config/more.yaml"),
        "x: 1
",
    )
    .unwrap();
    // Another open connection (e.g. a WrongStack reader) keeps the WAL alive:
    // the swap is refused and the old index stays in place.
    let reader = rusqlite::Connection::open(index.join("index.db")).unwrap();
    let n: i64 = reader
        .query_row("SELECT count(*) FROM symbols", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 11);
    assert!(matches!(rebuild(&root, &index), Err(NativeError::Busy(_))));
    drop(reader);
    let leftovers: Vec<String> = std::fs::read_dir(&index)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".wrongindex-staging"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "the staging directory is always removed"
    );
    assert!(matches!(
        rebuild(&root, &index).unwrap(),
        Rebuild::Written(_)
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
    assert_eq!(assert_matches_oracle("py-basic"), (3, 0, 9));
}

/// Go through `go run` of WrongStack's own `go/ast` program.
#[test]
fn go_basic_matches_the_wrongstack_oracle() {
    if wrongindex_engine::parse::go::go().is_none() {
        eprintln!("NOT RUN: no Go toolchain");
        return;
    }
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
    assert_eq!(assert_matches_oracle("treesitter-mix"), (6, 0, 17));
}
