//! Native full-index writer, first slice (WI-502).
//!
//! Writes a WrongStack v5 `index.db` for a project whose indexable files are
//! all JSON or YAML, reproducing one full WrongStack run
//! (`runIndexerWithStore`, first run into an empty index directory). Every
//! situation this slice has not ported is refused with
//! [`NativeError::Unsupported`] before anything is written:
//!
//! - any other language, or a YAML file that could carry infra references
//!   (`infra-parser.ts`: compose, GitHub workflows, OpenAPI, Kubernetes);
//! - Git-listed symbolic links that resolve outside the checkout;
//! - files over 5 MiB, incomplete discovery, or no indexable files;
//! - an index directory that already holds an `index.db`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params_from_iter};
use sha2::{Digest, Sha256};

use crate::js::utf16;
use crate::parse::{Parsed, ParsedSymbol, json, yaml};
use crate::rows;

pub const SCHEMA_SQL: &str =
    include_str!("../../../contracts/profiles/wrongstack-index-v5-baseline/schema.sql");
pub const MAX_INDEX_FILE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_SQL_VARIABLES: usize = 900;
const SYMBOL_COLUMNS: usize = 14;

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
}

/// `toPortablePath` then `derivePackageFromLayout(...) ?? '(root)'` — the
/// label of every file when no module root owns it (no marker probe runs for
/// a JSON/YAML-only project).
pub fn package_label(file: &str) -> String {
    let portable = file.replace('\\', "/");
    for (marker, prefix) in [("/packages/", "@wrongstack/"), ("/apps/", "app:")] {
        if let Some(i) = portable.find(marker) {
            let segment = portable[i + marker.len()..].split('/').next().unwrap_or("");
            if !segment.is_empty() {
                return format!("{prefix}{segment}");
            }
        }
    }
    "(root)".to_string()
}

/// Conservative superset of `detectInfraArtifactKind` for YAML: anything
/// that might produce infra refs is refused.
fn may_have_infra_refs(file: &str, content: &str) -> bool {
    let p = file.replace('\\', "/").to_lowercase();
    let base = p.rsplit('/').next().unwrap_or("");
    base.starts_with("docker-compose.")
        || base.starts_with("compose.")
        || base.starts_with("openapi.")
        || base.starts_with("swagger.")
        || base.contains(".k8s.")
        || p.contains(".github/workflows/")
        || p.contains("k8s/")
        || p.contains("kubernetes/")
        || p.contains("manifests/")
        || (content.contains("apiVersion") && content.contains("kind"))
}

fn ladder(mut remaining: usize, max: usize) -> Vec<usize> {
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

/// Read and parse every discovered file; refuse anything unsupported.
fn collect(project_root: &Path) -> Result<(Vec<FileRow>, Option<GitTrust>), NativeError> {
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
    for path in d.files {
        let lang = match wrongindex_discovery::lang::detect_lang(&path) {
            Some(l @ ("json" | "yaml")) => l,
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
        if lang == "yaml" && may_have_infra_refs(&path, &text) {
            return Err(NativeError::Unsupported(format!(
                "YAML infra artifact references ({path})"
            )));
        }
        let content = utf16(&text);
        let parsed: Parsed = if lang == "json" {
            json::parse(&path, &content, None)
        } else {
            yaml::parse(&content)
        };
        if let Some(why) = parsed.unsupported {
            return Err(NativeError::Unsupported(format!("{why} ({path})")));
        }
        rows.push(FileRow {
            path,
            lang,
            mtime_ms,
            content_hash,
            symbols: parsed.symbols,
        });
    }
    Ok((rows, git))
}

/// Statements of `schema.sql`: tables first, then the FTS table, then indexes.
fn schema_statements() -> Vec<String> {
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

fn set_meta(tx: &Connection, key: &str, value: &str) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO metadata(key, value) VALUES (?1, ?2)",
        [key, value],
    )?;
    Ok(())
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

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut next_id: i64 = 1;
    let mut churn: usize = 0;
    let mut files_empty = 0;
    let mut savepoint = 0usize;
    for batch in files.chunks(batch_width()) {
        let now = now_ms();
        // Files with nothing to write are upserted first, outside a savepoint.
        for f in batch.iter().filter(|f| f.symbols.is_empty()) {
            tx.execute(
                "INSERT INTO files(file, lang, mtime_ms, content_hash, symbol_count, last_indexed, git_blob)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, '')",
                rusqlite::params![f.path, f.lang, f.mtime_ms, f.content_hash, now_ms()],
            )?;
            files_empty += 1;
        }
        let entries: Vec<&FileRow> = batch.iter().filter(|f| !f.symbols.is_empty()).collect();
        let total: usize = entries.iter().map(|f| f.symbols.len()).sum();
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
        let mut id = start;
        for f in &entries {
            let ends = rows::end_lines(&f.symbols);
            for (s, end) in f.symbols.iter().zip(ends) {
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
                    V::Integer(rows::is_exported(&s.signature) as i64),
                    V::Text(rows::signature_hash(s)),
                ]);
                fts_rows.push((
                    id,
                    rows::indexable_text(&s.name, &s.signature, &s.doc_comment),
                ));
                id += 1;
            }
        }
        insert_symbols(&tx, &sym_rows)?;
        insert_fts(&tx, &fts_rows)?;
        for f in &entries {
            tx.execute(
                "INSERT INTO files(file, lang, mtime_ms, content_hash, symbol_count, last_indexed, git_blob)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, '')",
                rusqlite::params![
                    f.path,
                    f.lang,
                    f.mtime_ms,
                    f.content_hash,
                    f.symbols.len() as i64,
                    now
                ],
            )?;
        }
        churn += total;
        set_meta(&tx, "fts_churn_since_maintain", &churn.to_string())?;
        tx.execute_batch(&format!("RELEASE index_write_{savepoint}"))?;
    }

    // Finalize: relation pass (no module roots for JSON/YAML-only projects).
    for f in &files {
        tx.execute(
            "UPDATE files SET package = ?1 WHERE file = ?2",
            [package_label(&f.path), f.path.clone()],
        )?;
    }
    let structure = hex_sha256(b"[]");
    set_meta(&tx, "relation_structure", &structure)?;
    set_meta(&tx, "relation_epoch", &uuid::Uuid::new_v4().to_string())?;
    set_meta(&tx, "module_resolution_version", "1")?;
    set_meta(&tx, "ref_binding_version", "2")?;
    set_meta(&tx, "ref_resolution_version", "2")?;
    set_meta(&tx, "module_owner_version", "2")?;
    set_meta(&tx, "relation_graph_version", "2")?;
    // Rank pass: no wiring graph, `replaceRanks([], [])` in its own savepoint.
    savepoint += 1;
    tx.execute_batch(&format!(
        "SAVEPOINT index_write_{savepoint}; DELETE FROM symbol_rank; DELETE FROM file_rank; RELEASE index_write_{savepoint};"
    ))?;
    set_meta(&tx, "rank_version", "1")?;
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
    for t in ANALYZE_ORDER {
        tx.execute_batch(&format!("ANALYZE \"{t}\""))?;
    }
    tx.execute_batch("DROP TABLE IF EXISTS sqlite_stat4")?;
    Ok(())
}

fn hex_sha256(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
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
        assert_eq!(package_label(r"C:\w\config-only\project\a.json"), "(root)");
        assert_eq!(
            package_label(r"C:\r\packages\core\x.yaml"),
            "@wrongstack/core"
        );
        assert_eq!(package_label("/r/apps/web/x.yaml"), "app:web");
    }
}
