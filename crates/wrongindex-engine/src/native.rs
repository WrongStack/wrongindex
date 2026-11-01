//! Native full-index writer (WI-502).
//!
//! Writes a WrongStack v5 `index.db` for a project whose indexable files are
//! all TypeScript/JavaScript (ts, tsx, js, jsx), JSON, YAML, Rust or a
//! language WrongStack parses with its regex extractor (md, toml, html, css,
//! sql, proto, graphql, scala, vue, svelte, dart, lua, r, zig, haskell,
//! other; see [`crate::parse::parse_file`]),
//! reproducing one full WrongStack run
//! (`runIndexerWithStore`, first run into an empty index directory): batch
//! writes with module owners and refs, then `finalizeIndexRun` - name
//! resolution, the relation pass ([`crate::relations`],
//! [`crate::resolver`]), import binding ([`crate::binding`]), the rank pass
//! ([`crate::rank`]) and `PRAGMA optimize`. Every situation not ported is
//! refused with [`NativeError::Unsupported`] before anything is written:
//!
//! - any other language (Go, Python and the tree-sitter languages), or a
//!   parser result flagged as not ported (TS/JS files with syntax errors
//!   TypeScript recovers from, or script-level `await` it parses
//!   differently - see [`crate::parse::ts`]);
//! - Git-listed symbolic links that resolve outside the checkout;
//! - files over 5 MiB, incomplete discovery, or no indexable files;
//! - an index directory that already holds an `index.db`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params_from_iter};

use crate::js::utf16;
use crate::parse::{self, Parsed, ParsedRef, ParsedSymbol};
use crate::rows;

pub const SCHEMA_SQL: &str =
    include_str!("../../../contracts/profiles/wrongstack-index-v5-baseline/schema.sql");
pub const MAX_INDEX_FILE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_SQL_VARIABLES: usize = 900;
const SYMBOL_COLUMNS: usize = 14;
const REF_COLUMNS: usize = 8;

/// `SYMBOL_INDEX_SQL` then `REFS_INDEX_SQL` (`writer-schema.ts`): the
/// secondary indexes a cleared run drops before its batches and rebuilds,
/// in this order, at the start of finalize.
const SECONDARY_INDEXES: &[&str] = &[
    "idx_s_name",
    "idx_s_kind",
    "idx_s_lang",
    "idx_s_file",
    "idx_s_lang_kind",
    "idx_s_name_id",
    "idx_r_from",
    "idx_r_to_id",
    "idx_r_to_name",
    "idx_r_call_type",
    "idx_r_to_name_lang",
    "idx_r_module",
    "idx_r_to_file",
];

/// `resolveRefsWithStatement` (`writer-refs.ts`), the full form a first run
/// executes.
const RESOLVE_REFS_SQL: &str = "UPDATE refs
       SET to_id = s.id
       FROM (
             SELECT sym.name AS name, lf.family AS family, MIN(sym.id) AS id
               FROM symbols sym
               JOIN lang_family lf ON lf.lang = sym.lang
              GROUP BY sym.name, lf.family
             UNION ALL
             SELECT sym.name AS name, '*' AS family, MIN(sym.id) AS id
               FROM symbols sym
              GROUP BY sym.name
           ) AS s,
           lang_family AS rf
       WHERE refs.to_id IS NULL
         AND refs.to_name IS NOT NULL
         AND rf.lang = refs.lang
         AND s.name = refs.to_name
         AND s.family = rf.family
         AND (refs.to_file IS NULL OR refs.to_file <> '')
         AND NOT (refs.call_type = 'import' AND refs.module IS NOT NULL AND rf.family = 'js')";

/// `LANG_FAMILY` (`languages.ts`) in object order, then the wildcard row.
pub const LANG_FAMILY: &[(&str, &str)] = &[
    ("ts", "js"),
    ("tsx", "js"),
    ("js", "js"),
    ("jsx", "js"),
    ("vue", "js"),
    ("svelte", "js"),
    ("go", "go"),
    ("py", "py"),
    ("rs", "rs"),
    ("java", "jvm"),
    ("kotlin", "jvm"),
    ("scala", "jvm"),
    ("csharp", "dotnet"),
    ("c", "c"),
    ("cpp", "c"),
    ("ruby", "ruby"),
    ("php", "php"),
    ("swift", "swift"),
    ("dart", "dart"),
    ("elixir", "elixir"),
    ("haskell", "haskell"),
    ("zig", "zig"),
    ("lua", "lua"),
    ("r", "r"),
    ("shell", "shell"),
    ("sql", "sql"),
    ("json", "data"),
    ("yaml", "data"),
    ("toml", "data"),
    ("html", "web"),
    ("css", "web"),
    ("proto", "proto"),
    ("graphql", "graphql"),
    ("md", "other"),
    ("other", "other"),
    ("", "*"),
];

/// Tables `PRAGMA optimize` analyzes in a WrongStack run, in its order.
const ANALYZE_ORDER: &[&str] = &[
    "symbols_fts_config",
    "symbols_fts_idx",
    "file_rank",
    "symbol_rank",
    "refs",
    "files",
    "metadata",
    "symbols",
    "lang_family",
];

#[derive(Debug, thiserror::Error)]
pub enum NativeError {
    #[error("project files changed during indexing")]
    SourceChanged,
    #[error("the index is in use elsewhere ({0})")]
    Busy(String),
    #[error("not supported by the native engine yet: {0}")]
    Unsupported(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub files_indexed: usize,
    pub files_empty: usize,
    pub symbols_indexed: usize,
    pub graph_stamp: String,
}

struct FileRow {
    path: String,
    lang: &'static str,
    mtime_ms: i64,
    content_hash: String,
    symbols: Vec<ParsedSymbol>,
    refs: Vec<ParsedRef>,
}

pub(crate) fn ladder(mut remaining: usize, max: usize) -> Vec<usize> {
    let mut out = Vec::new();
    while remaining > 0 {
        let cap = remaining.min(max);
        let size = 1usize << (usize::BITS - 1 - cap.leading_zeros());
        out.push(size);
        remaining -= size;
    }
    out
}

fn batch_width() -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    (cores * 4).min(40)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Git trust evidence from discovery (Git checkout roots only).
struct GitTrust {
    clean_blobs: std::collections::BTreeMap<String, String>,
    snapshot_key: Option<String>,
}

/// Stack for the parsing thread. Parsers recurse on input structure; the
/// TS parser has an explicit nesting cap, and this large reserved stack
/// (committed only as used) is the second line of defence: a stack overflow
/// aborts the whole process and cannot be caught like a panic.
const PARSE_STACK_BYTES: usize = 256 * 1024 * 1024;

/// Read and parse every discovered file on a dedicated large-stack thread.
fn collect(project_root: &Path) -> Result<(Vec<FileRow>, Option<GitTrust>), NativeError> {
    let root = project_root.to_path_buf();
    std::thread::Builder::new()
        .name("wrongindex-parse".into())
        .stack_size(PARSE_STACK_BYTES)
        .spawn(move || collect_on_this_thread(&root))?
        .join()
        .unwrap_or_else(|_| Err(NativeError::Unsupported("parser thread panicked".into())))
}

/// Read and parse every discovered file; refuse anything unsupported.
fn collect_on_this_thread(
    project_root: &Path,
) -> Result<(Vec<FileRow>, Option<GitTrust>), NativeError> {
    let d = wrongindex_discovery::discover(project_root, &[]);
    if !d.complete {
        return Err(NativeError::Unsupported(format!(
            "incomplete discovery: {}",
            d.errors.join("; ")
        )));
    }
    if !d.outside_root.is_empty() {
        // WrongStack follows these links; WrongIndex never reads outside the
        // checkout, so the result could not be identical.
        return Err(NativeError::Unsupported(format!(
            "Git-listed links that resolve outside the checkout ({})",
            d.outside_root.join(", ")
        )));
    }
    let git = (d.source == wrongindex_discovery::Source::Git).then(|| GitTrust {
        clean_blobs: d.clean_blobs.clone(),
        snapshot_key: d.snapshot_key.clone(),
    });
    if d.files.is_empty() {
        return Err(NativeError::Unsupported(
            "a project with no indexable files".into(),
        ));
    }
    let mut rows = Vec::with_capacity(d.files.len());
    // Python is parsed afterwards, in interpreter batches: (row, text).
    let mut py: Vec<(usize, String)> = Vec::new();
    for path in d.files {
        let lang = match wrongindex_discovery::lang::detect_lang(&path) {
            Some(l) if parse::is_native_lang(l) => l,
            other => {
                return Err(NativeError::Unsupported(format!(
                    "language {} ({path})",
                    other.unwrap_or("unknown")
                )));
            }
        };
        let meta = std::fs::metadata(&path)?;
        if meta.len() > MAX_INDEX_FILE_BYTES {
            return Err(NativeError::Unsupported(format!(
                "file over 5 MiB ({path})"
            )));
        }
        let mtime_ms = meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let bytes = std::fs::read(&path)?;
        let content_hash = wrongindex_storage::content_hash::file_content_hash(&bytes);
        let text = String::from_utf8_lossy(&bytes);
        if lang == "py" {
            py.push((rows.len(), text.into_owned()));
            rows.push(FileRow {
                path,
                lang,
                mtime_ms,
                content_hash,
                symbols: Vec::new(),
                refs: Vec::new(),
            });
            continue;
        }
        let content = utf16(&text);
        // A parser panic (e.g. a regex construct the JS-regex engine refuses
        // to approximate) must never take the service down: the project is
        // reported as unsupported and left to the compatibility worker.
        let parsed: Parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parse::parse_file(&path, lang, &content)
        }))
        .map_err(|p| {
            let why = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "parser panic".into());
            NativeError::Unsupported(format!("{lang} parser failed on {path}: {why}"))
        })?;
        if let Some(why) = parsed.unsupported {
            return Err(NativeError::Unsupported(format!("{why} ({path})")));
        }
        rows.push(FileRow {
            path,
            lang,
            mtime_ms,
            content_hash,
            symbols: parsed.symbols,
            refs: parsed.refs,
        });
    }
    if !py.is_empty() {
        let files: Vec<parse::py::PyFile<'_>> = py
            .iter()
            .map(|(i, text)| parse::py::PyFile {
                file: &rows[*i].path,
                text,
            })
            .collect();
        let parsed = parse::py::parse_all(&files).map_err(NativeError::Unsupported)?;
        for ((i, _), p) in py.iter().zip(parsed) {
            rows[*i].symbols = p.symbols;
            rows[*i].refs = p.refs;
        }
    }
    Ok((rows, git))
}

/// Statements of `schema.sql`: tables first, then the FTS table, then indexes.
pub(crate) fn schema_statements() -> Vec<String> {
    let mut stmts: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in SCHEMA_SQL.lines() {
        if cur.is_empty() && line.starts_with("--") {
            continue;
        }
        cur.push_str(line);
        cur.push('\n');
        if line.trim_end().ends_with(';') {
            stmts.push(std::mem::take(&mut cur));
        }
    }
    let rank = |s: &String| {
        if s.starts_with("CREATE TABLE") {
            0
        } else if s.starts_with("CREATE VIRTUAL TABLE") {
            1
        } else {
            2
        }
    };
    stmts.sort_by_key(rank); // stable
    stmts
}

pub(crate) fn get_meta(tx: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    tx.query_row("SELECT value FROM metadata WHERE key = ?", [key], |r| {
        r.get(0)
    })
    .optional()
}

/// `setMetadata`: an equal value is not rewritten.
pub(crate) fn set_meta(tx: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    if get_meta(tx, key)?.as_deref() == Some(value) {
        return Ok(());
    }
    tx.execute(
        "INSERT OR REPLACE INTO metadata(key, value) VALUES (?1, ?2)",
        [key, value],
    )?;
    Ok(())
}

/// `moduleOwnerSymbol(file, lang)`.
fn module_owner() -> ParsedSymbol {
    ParsedSymbol {
        kind: "mod",
        name: rows::MODULE_OWNER_NAME.into(),
        line: 1,
        col: 0,
        signature: String::new(),
        doc_comment: String::new(),
        scope: String::new(),
        text: String::new(),
        is_exported: None,
        end_line: None,
    }
}

/// `needsModuleOwner(symbols, refs)`.
fn needs_module_owner(symbols: &[ParsedSymbol], refs: &[ParsedRef]) -> bool {
    let Some(first) = symbols.iter().map(|s| s.line).min() else {
        return true;
    };
    refs.iter()
        .any(|r| r.call_type == "import" || r.line < first)
}

/// A ref ready to insert (`fromId` assigned).
struct RefRow<'a> {
    from_id: i64,
    r: &'a ParsedRef,
}

/// `assignRefsToSymbols(refs, inserted)`; `inserted` is `(id, symbol)`.
fn assign_refs<'a>(refs: &'a [ParsedRef], inserted: &[(i64, &ParsedSymbol)]) -> Vec<RefRow<'a>> {
    if refs.is_empty() || inserted.is_empty() {
        return Vec::new();
    }
    let mut ordered: Vec<&(i64, &ParsedSymbol)> = inserted.iter().collect();
    ordered.sort_by(|a, b| {
        a.1.line
            .cmp(&b.1.line)
            .then(a.1.col.cmp(&b.1.col))
            .then(a.0.cmp(&b.0))
    });
    let module_owner = ordered
        .iter()
        .find(|(_, s)| s.kind == "mod" && s.name == rows::MODULE_OWNER_NAME)
        .map(|(id, _)| *id);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in refs {
        let mut owner: Option<i64> = None;
        if r.call_type == "import" && module_owner.is_some() {
            owner = module_owner;
        } else {
            for (id, s) in &ordered {
                if s.line > r.line {
                    break;
                }
                owner = Some(*id);
            }
        }
        if owner.is_none() && r.call_type == "import" {
            owner = Some(ordered[0].0);
        }
        let Some(owner) = owner.filter(|&o| o > 0) else {
            continue;
        };
        let key = format!(
            "{owner}:{}:{}:{}",
            r.to_name,
            r.call_type,
            r.module.as_deref().unwrap_or("")
        );
        if !seen.insert(key) {
            continue;
        }
        out.push(RefRow { from_id: owner, r });
    }
    out
}

/// Index `project_root` into a new `index_dir/index.db`.
///
/// Like `runIndexerWithStore`, a run whose Git snapshot moved while files
/// were read is discarded and retried once.
pub fn index_fresh(project_root: &Path, index_dir: &Path) -> Result<Report, NativeError> {
    let db_path: PathBuf = index_dir.join("index.db");
    if db_path.exists() {
        return Err(NativeError::Unsupported(
            "an existing index (only fresh indexes are written natively)".into(),
        ));
    }
    match index_once(project_root, index_dir, &db_path) {
        Err(NativeError::SourceChanged) => {
            remove_fresh_db(&db_path);
            index_once(project_root, index_dir, &db_path).inspect_err(|e| {
                if matches!(e, NativeError::SourceChanged) {
                    remove_fresh_db(&db_path);
                }
            })
        }
        other => other,
    }
}

/// Result of [`rebuild`].
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "outcome")]
pub enum Rebuild {
    /// The index already describes exactly the current files (same paths
    /// and content hashes): nothing is written and the published stamp is
    /// kept, as WrongStack does for a no-op run.
    Unchanged {
        graph_stamp: Option<String>,
    },
    Written(Report),
}

/// Bring `index_dir/index.db` up to date with a full native run.
///
/// The new index is built in a private staging directory and then moved
/// over the existing file, so readers never see a partial database. The
/// replacement is refused while the old file has WAL/shared-memory files
/// (another connection may have it open).
pub fn rebuild(project_root: &Path, index_dir: &Path) -> Result<Rebuild, NativeError> {
    let db = index_dir.join("index.db");
    if db.exists() {
        let (files, _) = collect(project_root)?;
        let mut current: Vec<(String, String)> = files
            .iter()
            .map(|f| (f.path.clone(), f.content_hash.clone()))
            .collect();
        current.sort();
        if let Some((stored, stamp)) = stored_state(&db)?
            && stored == current
        {
            return Ok(Rebuild::Unchanged { graph_stamp: stamp });
        }
    }
    let staging = index_dir.join(format!(
        ".wrongindex-staging-{}",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        let report = index_fresh(project_root, &staging)?;
        for suffix in ["-wal", "-shm"] {
            let mut p = db.as_os_str().to_owned();
            p.push(suffix);
            if Path::new(&p).exists() {
                return Err(NativeError::Busy(format!(
                    "{} exists",
                    PathBuf::from(p).display()
                )));
            }
        }
        std::fs::rename(staging.join("index.db"), &db)?;
        Ok(Rebuild::Written(report))
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result
}

/// `(file, content_hash)` rows (sorted) and `graph_stamp` of an existing
/// WrongStack v5 index; `None` when it is not one.
/// Sorted `(file, content_hash)` rows plus the stored `graph_stamp`.
type StoredState = (Vec<(String, String)>, Option<String>);

fn stored_state(db: &Path) -> Result<Option<StoredState>, NativeError> {
    // Read-write without create, but query-only: a read-only connection to a
    // WAL database leaves `-wal`/`-shm` behind, which would block the swap.
    let conn = Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "query_only", true)?;
    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'version'",
            [],
            |r| r.get(0),
        )
        .ok();
    if version.as_deref() != Some("5") {
        return Ok(None);
    }
    let mut rows: Vec<(String, String)> = conn
        .prepare("SELECT file, content_hash FROM files")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    rows.sort();
    let stamp = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'graph_stamp'",
            [],
            |r| r.get(0),
        )
        .ok();
    Ok(Some((rows, stamp)))
}

/// Remove a database this run created and never committed.
fn remove_fresh_db(db: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = db.as_os_str().to_owned();
        p.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(p));
    }
}

fn index_once(
    project_root: &Path,
    index_dir: &Path,
    db_path: &Path,
) -> Result<Report, NativeError> {
    let (files, git) = collect(project_root)?;
    std::fs::create_dir_all(index_dir)?;
    let mut conn = Connection::open(db_path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", 15000)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;

    // Store open: schema, version, symbols_text_dropped, next_symbol_id.
    for stmt in schema_statements() {
        conn.execute_batch(&stmt)?;
    }
    for (lang, family) in LANG_FAMILY {
        conn.execute(
            "INSERT OR REPLACE INTO lang_family(lang, family) VALUES (?1, ?2)",
            [lang, family],
        )?;
    }
    set_meta(&conn, "version", "5")?;
    set_meta(&conn, "symbols_text_dropped", "1")?;
    set_meta(&conn, "next_symbol_id", "1")?;
    // A cleared run bulk-loads without the secondary indexes
    // (`deferSecondaryIndexes`).
    for name in SECONDARY_INDEXES {
        conn.execute_batch(&format!("DROP INDEX IF EXISTS {name}"))?;
    }

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut next_id: i64 = 1;
    let mut churn: usize = 0;
    let mut files_empty = 0;
    let mut savepoint = 0usize;
    for batch in files.chunks(batch_width()) {
        let now = now_ms();
        // Files with nothing to write are upserted first, outside a savepoint.
        for f in batch
            .iter()
            .filter(|f| f.symbols.is_empty() && f.refs.is_empty())
        {
            tx.execute(
                "INSERT INTO files(file, lang, mtime_ms, content_hash, symbol_count, last_indexed, git_blob)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, '')",
                rusqlite::params![f.path, f.lang, f.mtime_ms, f.content_hash, now_ms()],
            )?;
            files_empty += 1;
        }
        // Each written file with its symbols, module owner first when needed.
        let entries: Vec<(&FileRow, Vec<ParsedSymbol>)> = batch
            .iter()
            .filter(|f| !(f.symbols.is_empty() && f.refs.is_empty()))
            .map(|f| {
                let mut syms = Vec::with_capacity(f.symbols.len() + 1);
                if needs_module_owner(&f.symbols, &f.refs) {
                    syms.push(module_owner());
                }
                syms.extend(f.symbols.iter().cloned());
                (f, syms)
            })
            .collect();
        let total: usize = entries.iter().map(|(_, s)| s.len()).sum();
        if entries.is_empty() {
            continue;
        }
        savepoint += 1;
        tx.execute_batch(&format!("SAVEPOINT index_write_{savepoint}"))?;
        let start = next_id;
        next_id += total as i64;
        set_meta(&tx, "next_symbol_id", &next_id.to_string())?;

        // (id, row) in insertion order.
        let mut sym_rows: Vec<Vec<rusqlite::types::Value>> = Vec::with_capacity(total);
        let mut fts_rows: Vec<(i64, String)> = Vec::with_capacity(total);
        let mut ref_rows: Vec<RefRow> = Vec::new();
        let mut id = start;
        for (f, syms) in &entries {
            let ends = rows::end_lines(syms);
            let mut inserted: Vec<(i64, &ParsedSymbol)> = Vec::with_capacity(syms.len());
            for (s, end) in syms.iter().zip(ends) {
                use rusqlite::types::Value as V;
                sym_rows.push(vec![
                    V::Integer(id),
                    V::Text(f.lang.into()),
                    V::Text(s.kind.into()),
                    V::Text(s.name.clone()),
                    V::Text(f.path.clone()),
                    V::Integer(s.line as i64),
                    V::Integer(s.col as i64),
                    V::Text(s.signature.clone()),
                    V::Text(s.doc_comment.clone()),
                    V::Text(s.scope.clone()),
                    V::Integer(end as i64),
                    V::Integer(rows::is_exported(s, f.lang) as i64),
                    V::Text(rows::signature_hash(s)),
                ]);
                fts_rows.push((
                    id,
                    rows::indexable_text(&s.name, &s.signature, &s.doc_comment),
                ));
                inserted.push((id, s));
                id += 1;
            }
            ref_rows.extend(assign_refs(&f.refs, &inserted));
        }
        insert_symbols(&tx, &sym_rows)?;
        insert_fts(&tx, &fts_rows)?;
        insert_refs(&tx, &ref_rows)?;
        for (f, syms) in &entries {
            tx.execute(
                "INSERT INTO files(file, lang, mtime_ms, content_hash, symbol_count, last_indexed, git_blob)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, '')",
                rusqlite::params![
                    f.path,
                    f.lang,
                    f.mtime_ms,
                    f.content_hash,
                    syms.len() as i64,
                    now
                ],
            )?;
        }
        churn += total;
        set_meta(&tx, "fts_churn_since_maintain", &churn.to_string())?;
        tx.execute_batch(&format!("RELEASE index_write_{savepoint}"))?;
    }

    // finalizeIndexRun: secondary indexes, name resolution, relations,
    // binding, ranks.
    for name in SECONDARY_INDEXES {
        tx.execute_batch(&secondary_index_sql(name))?;
    }
    tx.execute_batch(RESOLVE_REFS_SQL)?;
    crate::relations::relation_pass(&tx, &project_root.to_string_lossy())?;
    crate::binding::bind_refs_all(&tx)?;
    set_meta(
        &tx,
        "ref_binding_version",
        crate::binding::REF_BINDING_VERSION,
    )?;
    set_meta(&tx, "ref_resolution_version", "2")?;
    set_meta(&tx, "module_owner_version", "2")?;
    set_meta(&tx, "relation_graph_version", "2")?;
    let (sym_ranks, file_ranks) = crate::rank::rank_rows(&tx)?;
    savepoint += 1;
    crate::rank::replace_ranks(
        &tx,
        &format!("index_write_{savepoint}"),
        &sym_ranks,
        &file_ranks,
    )?;
    set_meta(&tx, "rank_version", crate::rank::RANK_VERSION)?;
    set_meta(&tx, "rank_stale_files", "0")?;
    // Git trust pass: every file Git reports clean gets `blob:contentHash`;
    // a new stamp is only written if the checkout did not move meanwhile.
    if let Some(git) = &git {
        let stamps: Vec<(String, &str)> = files
            .iter()
            .filter_map(|f| {
                git.clean_blobs
                    .get(&f.path)
                    .map(|b| (format!("{b}:{}", f.content_hash), f.path.as_str()))
            })
            .collect();
        if !stamps.is_empty() {
            let now = wrongindex_discovery::discover(project_root, &[]);
            if now.snapshot_key != git.snapshot_key
                || now.files != files.iter().map(|f| f.path.clone()).collect::<Vec<_>>()
            {
                return Err(NativeError::SourceChanged);
            }
        }
        for (stamp, file) in stamps {
            tx.execute(
                "UPDATE files SET git_blob = ?1 WHERE file = ?2",
                [stamp.as_str(), file],
            )?;
        }
    }
    write_stat1(&tx)?;
    let changed = files.len();
    let graph_stamp = format!("{}:{changed}", now_ms());
    set_meta(&tx, "graph_stamp", &graph_stamp)?;
    set_meta(&tx, "last_indexed", &now_ms().to_string())?;
    tx.commit()?;
    conn.close().map_err(|(_, e)| e)?;

    Ok(Report {
        files_indexed: files.len() - files_empty,
        files_empty,
        symbols_indexed: (next_id - 1) as usize,
        graph_stamp,
    })
}

/// `sqlite_stat1` as WrongStack's `PRAGMA optimize` leaves it.
///
/// The workspace builds SQLite without STAT4 (`.cargo/config.toml`), like
/// WrongStack's runtime. A build that still has STAT4 would create
/// `sqlite_stat4`; it is dropped so the schema stays identical (its pages
/// then remain on the freelist).
fn write_stat1(tx: &Connection) -> rusqlite::Result<()> {
    // `PRAGMA optimize` (SQLite >= 3.46) analyzes with a temporary
    // `analysis_limit` of 2000.
    tx.execute_batch("PRAGMA analysis_limit=2000")?;
    for t in ANALYZE_ORDER {
        tx.execute_batch(&format!("ANALYZE \"{t}\""))?;
    }
    tx.execute_batch("PRAGMA analysis_limit=0")?;
    tx.execute_batch("DROP TABLE IF EXISTS sqlite_stat4")?;
    Ok(())
}

/// The `CREATE INDEX` statement of one secondary index, from `schema.sql`.
fn secondary_index_sql(name: &str) -> String {
    let prefix = format!("CREATE INDEX {name} ON ");
    schema_statements()
        .into_iter()
        .find(|s| s.starts_with(&prefix))
        .unwrap_or_else(|| panic!("schema.sql declares {name}"))
}

fn insert_refs(tx: &Connection, rows: &[RefRow]) -> rusqlite::Result<()> {
    use rusqlite::types::Value as V;
    let mut at = 0;
    for n in ladder(rows.len(), MAX_SQL_VARIABLES / REF_COLUMNS) {
        let sql = format!(
            "INSERT INTO refs(from_id, to_name, to_id, call_type, line, lang, module, to_file) VALUES {}",
            vec!["(?, ?, ?, ?, ?, ?, ?, ?)"; n].join(", ")
        );
        let flat: Vec<V> = rows[at..at + n]
            .iter()
            .flat_map(|x| {
                [
                    V::Integer(x.from_id),
                    V::Text(x.r.to_name.clone()),
                    V::Null,
                    V::Text(x.r.call_type.into()),
                    V::Integer(x.r.line as i64),
                    V::Text(x.r.lang.into()),
                    x.r.module.clone().map_or(V::Null, V::Text),
                    V::Null,
                ]
            })
            .collect();
        tx.execute(&sql, params_from_iter(flat))?;
        at += n;
    }
    Ok(())
}

fn insert_symbols(tx: &Connection, rows: &[Vec<rusqlite::types::Value>]) -> rusqlite::Result<()> {
    let mut at = 0;
    for n in ladder(rows.len(), MAX_SQL_VARIABLES / SYMBOL_COLUMNS) {
        let one = "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
        let sql = format!(
            "INSERT INTO symbols(id, lang, kind, name, file, line, col, signature, doc_comment, scope, end_line, is_exported, signature_hash) VALUES {}",
            vec![one; n].join(", ")
        );
        tx.execute(&sql, params_from_iter(rows[at..at + n].iter().flatten()))?;
        at += n;
    }
    Ok(())
}

fn insert_fts(tx: &Connection, rows: &[(i64, String)]) -> rusqlite::Result<()> {
    let mut at = 0;
    for n in ladder(rows.len(), MAX_SQL_VARIABLES / 2) {
        let sql = format!(
            "INSERT INTO symbols_fts(rowid, text) VALUES {}",
            vec!["(?, ?)"; n].join(", ")
        );
        let flat: Vec<rusqlite::types::Value> = rows[at..at + n]
            .iter()
            .flat_map(|(id, t)| {
                [
                    rusqlite::types::Value::Integer(*id),
                    rusqlite::types::Value::Text(t.clone()),
                ]
            })
            .collect();
        tx.execute(&sql, params_from_iter(flat))?;
        at += n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_matches_writer_helpers() {
        assert_eq!(ladder(10, 64), vec![8, 2]);
        assert_eq!(ladder(1, 64), vec![1]);
        assert_eq!(ladder(130, 64), vec![64, 64, 2]);
        assert_eq!(ladder(0, 64), Vec::<usize>::new());
    }

    #[test]
    fn schema_statements_create_tables_before_indexes() {
        let s = schema_statements();
        assert_eq!(
            s.iter().filter(|x| x.starts_with("CREATE TABLE")).count(),
            11
        );
        let first_index = s
            .iter()
            .position(|x| x.starts_with("CREATE INDEX"))
            .unwrap();
        assert!(
            s[..first_index]
                .iter()
                .any(|x| x.starts_with("CREATE VIRTUAL TABLE"))
        );
        assert!(
            s[first_index..]
                .iter()
                .all(|x| x.starts_with("CREATE INDEX"))
        );
    }

    #[test]
    fn package_labels_follow_the_layout_fallback() {
        let label = |f: &str| {
            crate::relations::derive_package_from_layout(f).unwrap_or_else(|| "(root)".into())
        };
        assert_eq!(label(r"C:\w\config-only\project\a.json"), "(root)");
        assert_eq!(label(r"C:\r\packages\core\x.yaml"), "@wrongstack/core");
        assert_eq!(label("/r/apps/web/x.yaml"), "app:web");
    }

    fn sym(line: usize, col: usize) -> ParsedSymbol {
        ParsedSymbol::simple("function", &utf16("f"), line, col, &utf16("fn f()"))
    }

    fn mk_ref(to: &str, call: &'static str, line: usize, module: Option<&str>) -> ParsedRef {
        ParsedRef {
            to_name: to.into(),
            call_type: call,
            line,
            lang: "rs",
            module: module.map(Into::into),
        }
    }

    #[test]
    fn refs_are_owned_like_assign_refs_to_symbols() {
        let owner = module_owner();
        let (a, b) = (sym(3, 4), sym(3, 0));
        let refs = vec![
            mk_ref("x", "import", 9, Some("x")),
            mk_ref("x", "import", 9, Some("x")),
            mk_ref("y", "call", 3, None),
            mk_ref("z", "call", 2, None),
        ];
        let inserted = vec![(5, &owner), (6, &a), (7, &b)];
        let got: Vec<(i64, &str)> = assign_refs(&refs, &inserted)
            .iter()
            .map(|x| (x.from_id, x.r.to_name.as_str()))
            .collect();
        // An import goes to the module owner; a call to the last symbol at
        // or before its line in (line, col, id) order; duplicates are dropped.
        assert_eq!(got, vec![(5, "x"), (6, "y"), (5, "z")]);
        assert!(needs_module_owner(&[], &[]));
        assert!(needs_module_owner(std::slice::from_ref(&a), &refs[..1]));
        assert!(!needs_module_owner(std::slice::from_ref(&a), &refs[2..3]));
        assert!(needs_module_owner(std::slice::from_ref(&a), &refs[3..]));
    }
}
