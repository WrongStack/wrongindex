//! In-place incremental run (WI-515): WrongStack's non-forced whole-project
//! run over an existing index (`runIndexerAtomic` with `clearedAll = false`),
//! ported step for step so the result is the same database WrongStack's own
//! incremental run leaves - ids from `next_symbol_id`, carried and stale
//! ranks, scoped ref resolution, relations and binding.
//!
//! Sources: `indexer.ts`, `indexer-batch-read.ts`, `indexer-batch-commit.ts`,
//! `indexer-finalize.ts`, `index-store-batches.ts`, `writer-mutations.ts`,
//! `writer-deletion.ts`, `writer-refs.ts`, `graph-rank-pass.ts`.
//!
//! Only an index in the current steady state is updated here (every version
//! marker current, no vectors); anything else is left to the fresh rebuild.
//! With `.wrongstack/memories` present the run also heals SAGE anchors of
//! moved or renamed symbols, as WrongStack's does (see [`crate::sage`]).
//!
//! The targeted run (`runIndexer({ files })`, WrongStack's watcher/edit path)
//! is the same pipeline over an explicit file list: directories expanded
//! (`expandTargetedDirectories`), a missing path deletes its rows, no stale
//! sweep, no Git trust, `projectScan: false` relations, and the targeted
//! rules for `PRAGMA optimize` and `last_indexed`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::Connection;

use super::{
    FileRow, GitTrust, MAX_INDEX_FILE_BYTES, NativeError, ParsedSymbol, Report, Scan, Source,
    assign_refs, batch_width, get_meta, insert_fts, insert_refs, insert_symbols, module_owner,
    needs_module_owner, now_ms, parse_scan, rows, set_meta, timing,
};
use crate::binding::{chunked_rows, in_list_chunks, pad_to_bucket, placeholders};
use crate::relations::Changes;

/// `RANK_REFRESH_FILE_THRESHOLD`.
const RANK_REFRESH_FILE_THRESHOLD: i64 = 25;

/// A previous `files` row (`FileMeta`).
#[derive(Clone)]
struct Meta {
    mtime_ms: i64,
    symbol_count: i64,
    content_hash: String,
    git_blob: String,
}

/// Ordered set of strings (JS `Set` iteration order).
#[derive(Default)]
struct Ordered {
    list: Vec<String>,
    seen: HashSet<String>,
}

impl Ordered {
    fn add(&mut self, s: &str) {
        if self.seen.insert(s.to_string()) {
            self.list.push(s.to_string());
        }
    }
    fn len(&self) -> usize {
        self.list.len()
    }
}

/// What an incremental run did.
pub struct Outcome {
    pub report: Report,
    pub changed_files: usize,
}

/// Why this index is not updated in place (the caller rebuilds it fresh).
fn steady_state(tx: &Connection) -> rusqlite::Result<Option<String>> {
    let want: &[(&str, &str)] = &[
        ("relation_graph_version", "2"),
        ("ref_resolution_version", "2"),
        ("module_owner_version", "2"),
        ("ref_binding_version", crate::binding::REF_BINDING_VERSION),
        (
            "module_resolution_version",
            crate::relations::MODULE_RESOLUTION_VERSION,
        ),
        ("rank_version", crate::rank::RANK_VERSION),
    ];
    for (k, v) in want {
        if get_meta(tx, k)?.as_deref() != Some(*v) {
            return Ok(Some(format!("metadata {k} is not {v}")));
        }
    }
    let has_vectors: i64 = tx.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'symbol_vectors'",
        [],
        |r| r.get(0),
    )?;
    if has_vectors > 0 {
        let vectors: i64 = tx.query_row("SELECT COUNT(*) FROM symbol_vectors", [], |r| r.get(0))?;
        if vectors > 0 {
            return Ok(Some("symbol vectors present".into()));
        }
    }
    Ok(None)
}

/// `invalidateIncomingRefsForFiles`: the names declared in `files`, after
/// NULLing every ref resolved into them.
fn invalidate_incoming(tx: &Connection, files: &[String]) -> rusqlite::Result<Vec<String>> {
    let mut names = Vec::new();
    let mut at = 0;
    for n in in_list_chunks(files.len(), crate::binding::MAX_SQL_VARS) {
        let bucket = pad_to_bucket(&files[at..at + n]);
        at += n;
        let ph = placeholders(bucket.len());
        let mut st = tx.prepare_cached(&format!(
            "SELECT DISTINCT name FROM symbols WHERE file IN ({ph})"
        ))?;
        let rows = st.query_map(rusqlite::params_from_iter(bucket.iter()), |r| {
            r.get::<_, String>(0)
        })?;
        for r in rows {
            names.push(r?);
        }
        tx.prepare_cached(&format!(
            "UPDATE refs SET to_id = NULL
         WHERE to_id IN (SELECT id FROM symbols WHERE file IN ({ph}))"
        ))?
        .execute(rusqlite::params_from_iter(bucket.iter()))?;
    }
    Ok(names)
}

/// `resolveRefsForNamesUnsafe(names)`.
fn resolve_names(tx: &Connection, names: &[String]) -> rusqlite::Result<usize> {
    let list: Vec<String> = names.iter().filter(|n| !n.is_empty()).cloned().collect();
    let mut total = 0;
    let mut at = 0;
    for n in in_list_chunks(list.len(), crate::binding::MAX_SQL_VARS) {
        let chunk = pad_to_bucket(&list[at..at + n]);
        at += n;
        let ph = placeholders(chunk.len());
        let sql = format!(
            "UPDATE refs
         SET to_id = s.id
         FROM (
               SELECT sym.name AS name, lf.family AS family, MIN(sym.id) AS id
                 FROM symbols sym
                 JOIN lang_family lf ON lf.lang = sym.lang
                WHERE sym.name IN ({ph})
                GROUP BY sym.name, lf.family
               UNION ALL
               SELECT sym.name AS name, '*' AS family, MIN(sym.id) AS id
                 FROM symbols sym
                WHERE sym.name IN ({ph})
                GROUP BY sym.name
             ) AS s,
             lang_family AS rf
         WHERE refs.to_name IN ({ph})
           AND refs.to_id IS NULL
           AND rf.lang = refs.lang
           AND s.name = refs.to_name
           AND s.family = rf.family
           AND (refs.to_file IS NULL OR refs.to_file <> '')
           AND NOT (refs.call_type = 'import' AND refs.module IS NOT NULL AND rf.family = 'js')"
        );
        let binds: Vec<&String> = chunk.iter().chain(&chunk).chain(&chunk).collect();
        total += tx
            .prepare_cached(&sql)?
            .execute(rusqlite::params_from_iter(binds))?;
    }
    Ok(total)
}

/// `recordFtsChurn(rows)`.
fn record_churn(tx: &Connection, rows: usize) -> rusqlite::Result<()> {
    if rows == 0 {
        return Ok(());
    }
    let current: i64 = get_meta(tx, "fts_churn_since_maintain")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    set_meta(
        tx,
        "fts_churn_since_maintain",
        &(current + rows as i64).to_string(),
    )
}

fn upsert_file(
    tx: &Connection,
    file: &str,
    lang: &str,
    mtime_ms: i64,
    content_hash: &str,
    symbol_count: i64,
) -> rusqlite::Result<()> {
    tx.prepare_cached(
        "INSERT INTO files(file, lang, mtime_ms, content_hash, symbol_count, last_indexed, git_blob)
     VALUES (?, ?, ?, ?, ?, ?, '')
     ON CONFLICT(file) DO UPDATE SET
       lang = excluded.lang,
       mtime_ms = excluded.mtime_ms,
       content_hash = excluded.content_hash,
       symbol_count = excluded.symbol_count,
       last_indexed = excluded.last_indexed,
       git_blob = excluded.git_blob",
    )?
    .execute(rusqlite::params![
        file,
        lang,
        mtime_ms,
        content_hash,
        symbol_count,
        now_ms()
    ])?;
    Ok(())
}

/// Delete one file's symbols with their FTS rows, outgoing refs and rank
/// rows; returns the symbols deleted.
fn delete_symbol_rows(tx: &Connection, file: &str) -> rusqlite::Result<usize> {
    tx.prepare_cached(
        "DELETE FROM symbols_fts WHERE rowid IN (SELECT id FROM symbols WHERE file = ?)",
    )?
    .execute([file])?;
    tx.prepare_cached("DELETE FROM refs WHERE from_id IN (SELECT id FROM symbols WHERE file = ?)")?
        .execute([file])?;
    tx.prepare_cached(
        "DELETE FROM symbol_rank WHERE symbol_id IN (SELECT id FROM symbols WHERE file = ?)",
    )?
    .execute([file])?;
    tx.prepare_cached("DELETE FROM symbols WHERE file = ?")?
        .execute([file])
}

/// `replaceEmptyFile(meta)`.
fn replace_empty_file(tx: &Connection, f: &FileRow) -> rusqlite::Result<()> {
    let affected = invalidate_incoming(tx, std::slice::from_ref(&f.path))?;
    let deleted = delete_symbol_rows(tx, &f.path)?;
    record_churn(tx, deleted)?;
    upsert_file(tx, &f.path, f.lang, f.mtime_ms, &f.content_hash, 0)?;
    resolve_names(tx, &affected)?;
    Ok(())
}

/// `deleteFile(file)` (`deleteIndexedFile`).
fn delete_file(tx: &Connection, file: &str) -> rusqlite::Result<()> {
    let affected = invalidate_incoming(tx, &[file.to_string()])?;
    tx.prepare_cached(
        "DELETE FROM symbols_fts WHERE rowid IN (SELECT id FROM symbols WHERE file = ?)",
    )?
    .execute([file])?;
    tx.prepare_cached("DELETE FROM refs WHERE from_id IN (SELECT id FROM symbols WHERE file = ?)")?
        .execute([file])?;
    tx.prepare_cached(
        "DELETE FROM symbol_rank WHERE symbol_id IN (SELECT id FROM symbols WHERE file = ?)",
    )?
    .execute([file])?;
    tx.prepare_cached("DELETE FROM file_rank WHERE file = ?")?
        .execute([file])?;
    let deleted = tx
        .prepare_cached("DELETE FROM symbols WHERE file = ?")?
        .execute([file])?;
    record_churn(tx, deleted)?;
    tx.prepare_cached("DELETE FROM files WHERE file = ?")?
        .execute([file])?;
    resolve_names(tx, &affected)?;
    Ok(())
}

/// `allocateSymbolIds(count)`.
fn allocate_ids(tx: &Connection, count: usize) -> rusqlite::Result<i64> {
    let max: i64 = tx.query_row("SELECT COALESCE(MAX(id), 0) FROM symbols", [], |r| r.get(0))?;
    let stored: i64 = get_meta(tx, "next_symbol_id")?
        .and_then(|v| v.parse().ok())
        .filter(|v| *v != 0)
        .unwrap_or(1);
    let start = 1.max(stored).max(max + 1);
    tx.execute(
        "INSERT OR REPLACE INTO metadata(key, value) VALUES (?, ?)",
        rusqlite::params!["next_symbol_id", (start + count as i64).to_string()],
    )?;
    Ok(start)
}

type RankKey = (String, String, String, String);

/// `commitBatch(entries, { deleteForFiles, deferResolution })`.
fn commit_batch(
    tx: &Connection,
    entries: &[(&FileRow, Vec<ParsedSymbol>)],
    deferred: &mut Ordered,
) -> rusqlite::Result<()> {
    let delete_for: Vec<String> = entries.iter().map(|(f, _)| f.path.clone()).collect();
    let mut churn: usize = entries.iter().map(|(_, s)| s.len()).sum();
    let mut at = 0;
    for n in in_list_chunks(delete_for.len(), crate::binding::MAX_SQL_VARS / 4) {
        let bucket = &delete_for[at..at + n];
        at += n;
        let c: i64 = tx
            .prepare_cached(&format!(
                "SELECT COUNT(*) AS n FROM symbols WHERE file IN ({})",
                placeholders(bucket.len())
            ))?
            .query_row(rusqlite::params_from_iter(bucket.iter()), |r| r.get(0))?;
        churn += c as usize;
    }
    let mut affected = Ordered::default();
    for (f, syms) in entries {
        for s in syms {
            affected.add(&s.name);
        }
        for r in &f.refs {
            affected.add(&r.to_name);
        }
    }
    for name in invalidate_incoming(tx, &delete_for)? {
        affected.add(&name);
    }
    // Ranks of the declarations being replaced, carried to the new ids.
    let mut carried: HashMap<RankKey, (f64, i64, i64)> = HashMap::new();
    let mut at = 0;
    for n in in_list_chunks(delete_for.len(), crate::binding::MAX_SQL_VARS / 4) {
        let bucket = pad_to_bucket(&delete_for[at..at + n]);
        at += n;
        let ph = placeholders(bucket.len());
        let rows: Vec<(RankKey, (f64, i64, i64))> = tx
            .prepare_cached(&format!(
                "SELECT s.file AS file, s.name AS name, s.kind AS kind, s.scope AS scope,
                r.rank AS rank, r.in_deg AS inDeg, r.out_deg AS outDeg
           FROM symbol_rank r JOIN symbols s ON s.id = r.symbol_id
          WHERE s.file IN ({ph})"
            ))?
            .query_map(rusqlite::params_from_iter(bucket.iter()), |r| {
                Ok((
                    (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?),
                    (r.get(4)?, r.get(5)?, r.get(6)?),
                ))
            })?
            .collect::<Result<_, _>>()?;
        for (k, v) in rows {
            carried.entry(k).or_insert(v);
        }
        for sql in [
            format!(
                "DELETE FROM symbol_rank WHERE symbol_id IN (SELECT id FROM symbols WHERE file IN ({ph}))"
            ),
            format!(
                "DELETE FROM symbols_fts WHERE rowid IN (SELECT id FROM symbols WHERE file IN ({ph}))"
            ),
            format!(
                "DELETE FROM refs WHERE from_id IN (SELECT id FROM symbols WHERE file IN ({ph}))"
            ),
            format!("DELETE FROM symbols WHERE file IN ({ph})"),
        ] {
            tx.prepare_cached(&sql)?
                .execute(rusqlite::params_from_iter(bucket.iter()))?;
        }
    }

    let total: usize = entries.iter().map(|(_, s)| s.len()).sum();
    let mut id = allocate_ids(tx, total)?;
    let mut sym_rows: Vec<Vec<rusqlite::types::Value>> = Vec::with_capacity(total);
    let mut fts_rows: Vec<(i64, String)> = Vec::with_capacity(total);
    let mut ref_rows = Vec::new();
    let mut rank_rows: Vec<(i64, f64, i64, i64)> = Vec::new();
    for (f, syms) in entries {
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
            let key = (
                f.path.clone(),
                s.name.clone(),
                s.kind.to_string(),
                s.scope.clone(),
            );
            if let Some((rank, i, o)) = carried.get(&key) {
                rank_rows.push((id, *rank, *i, *o));
            }
            inserted.push((id, s));
            id += 1;
        }
        ref_rows.extend(assign_refs(&f.refs, &inserted));
    }
    insert_symbols(tx, &sym_rows)?;
    insert_fts(tx, &fts_rows)?;
    insert_refs(tx, &ref_rows)?;
    let mut at = 0;
    for n in super::ladder(rank_rows.len(), (crate::binding::MAX_SQL_VARS / 4).max(1)) {
        let chunk = &rank_rows[at..at + n];
        at += n;
        let sql = format!(
            "INSERT OR REPLACE INTO symbol_rank(symbol_id, rank, in_deg, out_deg)
         VALUES {}",
            vec!["(?, ?, ?, ?)"; n].join(", ")
        );
        use rusqlite::types::Value as V;
        let binds: Vec<V> = chunk
            .iter()
            .flat_map(|(s, r, i, o)| [V::Integer(*s), V::Real(*r), V::Integer(*i), V::Integer(*o)])
            .collect();
        tx.prepare_cached(&sql)?
            .execute(rusqlite::params_from_iter(binds))?;
    }
    for (f, syms) in entries {
        upsert_file(
            tx,
            &f.path,
            f.lang,
            f.mtime_ms,
            &f.content_hash,
            syms.len() as i64,
        )?;
    }
    for name in &affected.list {
        deferred.add(name);
    }
    record_churn(tx, churn)
}

/// `getRefNamesTargeting(targets)`: distinct (owner file, to_name) of refs
/// whose `to_file` is a target, grouped per file in first-seen order.
fn ref_names_targeting(
    tx: &Connection,
    targets: &[String],
    into: &mut Vec<(String, Vec<String>)>,
) -> rusqlite::Result<()> {
    let rows = chunked_rows(
        tx,
        targets,
        |ph| {
            format!(
                "SELECT DISTINCT s.file, r.to_name
           FROM refs r
           JOIN symbols s ON s.id = r.from_id
          WHERE r.to_file IN ({ph})"
            )
        },
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )?;
    for (file, name) in rows {
        match into.iter_mut().find(|(f, _)| *f == file) {
            Some((_, names)) => {
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            None => into.push((file, vec![name])),
        }
    }
    Ok(())
}

/// Whether `db` is in the steady state [`update`] handles (`None`), or why
/// not. Read through a query-only connection that leaves no WAL files.
pub(super) fn applicable(_project_root: &Path, db: &Path) -> rusqlite::Result<Option<String>> {
    if std::env::var("WRONGINDEX_NATIVE_INCREMENTAL").is_ok_and(|v| v == "0") {
        return Ok(Some("disabled by WRONGINDEX_NATIVE_INCREMENTAL=0".into()));
    }
    let conn = Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "query_only", true)?;
    steady_state(&conn)
}

/// One incremental whole-project run over `db`, an existing baseline index
/// [`applicable`] accepted.
pub(super) fn update(project_root: &Path, db: &Path, scan: Scan) -> Result<Outcome, NativeError> {
    run(project_root, db, Input::Project(scan))
}

/// One targeted run (`runIndexer({ files: targets })`) over `db`, an
/// existing baseline index [`applicable`] accepted. `targets` is not empty
/// (an empty list is a whole-project run in WrongStack).
pub(super) fn update_files(
    project_root: &Path,
    db: &Path,
    targets: &[String],
) -> Result<Outcome, NativeError> {
    run(project_root, db, Input::Files(targets))
}

enum Input<'a> {
    Project(Scan),
    Files(&'a [String]),
}

/// One entry of the run's file list after the stat (`readBatchFiles`).
enum Entry {
    Source(Source),
    /// ENOENT/ENOTDIR in a targeted run: the file was deleted or renamed.
    Missing(String),
    /// Not a regular file, or no language: no rows, no error.
    Ignored,
}

/// Comparable form of a path (`normalizeComparablePath`).
fn comparable(p: &str) -> String {
    let r = wrongindex_storage::wrongstack_paths::node_resolve(Path::new(p))
        .to_string_lossy()
        .into_owned();
    if cfg!(windows) { r.to_lowercase() } else { r }
}

/// `path.relative(root, file)` for a file inside `root` (POSIX separators),
/// `None` when it is not inside (`isWithinProject` false).
fn relative_inside(root: &str, file: &str) -> Option<String> {
    let resolve = |p: &str| {
        wrongindex_storage::wrongstack_paths::node_resolve(Path::new(p))
            .to_string_lossy()
            .into_owned()
    };
    let (r, f) = (resolve(root), resolve(file));
    let sep = std::path::MAIN_SEPARATOR;
    let r = r.trim_end_matches(sep);
    let cut = r.len() + 1;
    if f.len() <= cut || !f.is_char_boundary(cut - 1) || !f[cut - 1..].starts_with(sep) {
        return None;
    }
    let head = &f[..cut - 1];
    let same = if cfg!(windows) {
        head.to_lowercase() == r.to_lowercase()
    } else {
        head == r
    };
    same.then(|| f[cut..].replace('\\', "/"))
}

/// Containment for targeted paths (security review WI-604): a path inside
/// the checkout must stay inside once its symbolic links and junctions are
/// resolved. Links are resolved by reading their targets, never by opening
/// them, so a link to a share or device is never touched. Full discovery
/// never follows such links either (Git-listed ones refuse the run, the walk
/// skips links); WrongStack itself follows them, so the run is refused
/// rather than indexed differently.
struct Containment {
    root: String,
    /// The root with its own links resolved (a checkout reached through a
    /// junction is still inside itself).
    real_root: Option<String>,
}

impl Containment {
    fn new(root: &str) -> Self {
        let real_root = wrongindex_storage::git_containment::resolve_links_locally(Path::new(root))
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        Containment {
            root: root.to_string(),
            real_root,
        }
    }

    /// `Err` (refusal) when `path` leaves the checkout through a link.
    fn check(&self, path: &str) -> Result<(), NativeError> {
        let inside = wrongindex_storage::git_containment::resolve_links_locally(Path::new(path))
            .ok()
            .is_some_and(|real| {
                let real = real.to_string_lossy();
                relative_inside(&self.root, &real).is_some()
                    || self
                        .real_root
                        .as_deref()
                        .is_some_and(|r| relative_inside(r, &real).is_some())
            });
        if inside {
            Ok(())
        } else {
            Err(NativeError::Unsupported(format!(
                "a targeted path leaves the checkout through a link ({path}); WrongIndex never reads outside it"
            )))
        }
    }
}

/// `expandTargetedDirectories` plus the targeted filters of
/// `discoverIndexerFiles`, in WrongStack's order. `indexed` is the `files`
/// table in its own order (`getAllFileMetas`). Nothing outside the checkout
/// is opened (see [`Containment`]).
fn targeted_files(
    project_root: &Path,
    indexed: &[String],
    targets: &[String],
    containment: &Containment,
) -> Result<Vec<String>, NativeError> {
    use wrongindex_discovery::{DEFAULT_IGNORE_DIRS, DEFAULT_IGNORE_FILES, Gitignore, lang};
    let root = wrongindex_storage::wrongstack_paths::node_resolve(project_root);
    let root_s = root.to_string_lossy().into_owned();
    let gi = Gitignore::load(&root);
    let mut expanded: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut add = |f: String, expanded: &mut Vec<String>| {
        if seen.insert(f.clone()) {
            expanded.push(f);
        }
    };
    for t in targets {
        let target = wrongindex_storage::wrongstack_paths::node_resolve(&root.join(t))
            .to_string_lossy()
            .into_owned();
        if lang::is_indexable(&target) || relative_inside(&root_s, &target).is_none() {
            add(target, &mut expanded);
            continue;
        }
        // Before the stat: it follows links.
        containment.check(&target)?;
        let stats = match std::fs::metadata(&target) {
            Ok(m) => Some(m),
            Err(e) if is_missing(&e) => None,
            Err(_) => {
                add(target, &mut expanded);
                continue;
            }
        };
        if stats.as_ref().is_some_and(|m| !m.is_dir()) {
            add(target, &mut expanded);
            continue;
        }
        let prefix = format!("{}{}", comparable(&target), std::path::MAIN_SEPARATOR);
        for f in indexed {
            if comparable(f).starts_with(&prefix) {
                add(f.clone(), &mut expanded);
            }
        }
        if stats.is_none() {
            continue;
        }
        fn walk(
            root: &str,
            dir: &Path,
            gi: &Gitignore,
            out: &mut Vec<String>,
            add: &mut dyn FnMut(String, &mut Vec<String>),
        ) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if DEFAULT_IGNORE_DIRS.contains(&name.as_str()) {
                    continue;
                }
                let full = dir.join(&name);
                let full_s = full.to_string_lossy().into_owned();
                // Dirent semantics: links are neither files nor directories.
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_symlink() {
                    continue;
                }
                if ft.is_dir() {
                    let rel = relative_inside(root, &full_s).unwrap_or_default();
                    if !gi.is_ignored(&rel, true) {
                        walk(root, &full, gi, out, add);
                    }
                } else if ft.is_file() && lang::is_indexable(&full_s) {
                    add(full_s, out);
                }
            }
        }
        walk(&root_s, Path::new(&target), &gi, &mut expanded, &mut add);
    }
    Ok(expanded
        .into_iter()
        .filter(|f| {
            let Some(rel) = relative_inside(&root_s, f) else {
                return false;
            };
            let base = rel.rsplit('/').next().unwrap_or(&rel);
            !(rel.split('/').any(|seg| DEFAULT_IGNORE_DIRS.contains(&seg))
                || DEFAULT_IGNORE_FILES.contains(&base)
                || rel.starts_with(".wrongstack/atlas/")
                || gi.is_ignored(&rel, false))
        })
        .collect())
}

/// `isMissingPathError`: ENOENT or ENOTDIR.
fn is_missing(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// `readBatchFiles` for one file of a targeted run.
fn stat_target(path: String, containment: &Containment) -> Result<Entry, NativeError> {
    // Before the stat and the read: both follow links.
    containment.check(&path)?;
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if is_missing(&e) => return Ok(Entry::Missing(path)),
        Err(_) => return Ok(Entry::Source(failed_source(path, "other"))),
    };
    if !meta.is_file() {
        return Ok(Entry::Ignored);
    }
    let Some(lang) = wrongindex_discovery::lang::detect_lang(&path) else {
        return Ok(Entry::Ignored);
    };
    if !super::parse::is_native_lang(lang) {
        return Err(NativeError::Unsupported(format!(
            "language {lang} ({path})"
        )));
    }
    if meta.len() > MAX_INDEX_FILE_BYTES {
        return Ok(Entry::Source(failed_source(path, lang)));
    }
    // A read failure is per file and keeps the previous rows.
    Ok(Entry::Source(
        Source::load(path.clone(), lang).unwrap_or_else(|_| failed_source(path, lang)),
    ))
}

fn failed_source(path: String, lang: &'static str) -> Source {
    Source {
        path,
        lang,
        mtime_ms: 0,
        content_hash: String::new(),
        failed: true,
        text: String::new(),
        unread: false,
    }
}

/// `store.search('', { file })`: every symbol (the module owner excluded)
/// whose path contains `file` - a LIKE substring match, case-insensitive for
/// ASCII, in table order - as the heal snapshot reads it.
fn heal_snapshot(
    tx: &Connection,
    file: &str,
    out: &mut Vec<crate::sage::Snap>,
) -> rusqlite::Result<()> {
    let portable = file.replace('\\', "/");
    let escaped: String = portable
        .chars()
        .flat_map(|c| match c {
            '\\' | '%' | '_' => vec!['\\', c],
            c => vec![c],
        })
        .collect();
    let mut st = tx.prepare_cached(
        "SELECT id, lang, kind, name, file, line, col, signature, doc_comment FROM symbols WHERE NOT (kind = 'mod' AND name = '<module>') AND replace(file, '\\', '/') LIKE ? ESCAPE '\\'",
    )?;
    let rows = st.query_map([format!("%{escaped}%")], |r| {
        Ok(crate::sage::Snap {
            kind: r.get(2)?,
            name: r.get(3)?,
            file: r.get(4)?,
            signature: r.get(7)?,
        })
    })?;
    for row in rows {
        out.push(row?);
    }
    Ok(())
}

/// `.wrongstack/memories` when it exists and stays inside the checkout
/// (a link out of it is never followed: no heal then).
fn memories_dir(project_root: &Path) -> Option<std::path::PathBuf> {
    let dir = project_root.join(".wrongstack").join("memories");
    let root = wrongindex_storage::wrongstack_paths::node_resolve(project_root)
        .to_string_lossy()
        .into_owned();
    let containment = Containment::new(&root);
    containment.check(&dir.to_string_lossy()).ok()?;
    dir.is_dir().then_some(dir)
}

fn run(project_root: &Path, db: &Path, input: Input) -> Result<Outcome, NativeError> {
    let t_all = std::time::Instant::now();
    // `healSageAnchors`: never on a cleared run (this writer never clears).
    let heal = memories_dir(project_root);
    let mut prev_snap: Vec<crate::sage::Snap> = Vec::new();
    let mut next_snap: Vec<crate::sage::Snap> = Vec::new();
    let mut conn = Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "busy_timeout", 15000)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.set_prepared_statement_cache_capacity(256);
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(why) = steady_state(&tx)? {
        return Err(NativeError::Unsupported(format!(
            "the index changed state during the run ({why})"
        )));
    }

    // Previous file rows, in table order.
    let existing: Vec<(String, Meta)> = tx
        .prepare(
            "SELECT file, lang, mtime_ms, symbol_count, last_indexed, content_hash, git_blob FROM files",
        )?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                Meta {
                    mtime_ms: r.get(2)?,
                    symbol_count: r.get(3)?,
                    content_hash: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    git_blob: r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                },
            ))
        })?
        .collect::<Result<_, _>>()?;
    let meta: HashMap<&str, &Meta> = existing.iter().map(|(f, m)| (f.as_str(), m)).collect();

    let project_scan = matches!(input, Input::Project(_));
    let (entries, git): (Vec<Entry>, Option<GitTrust>) = match input {
        Input::Project(Scan { files, git }) => {
            (files.into_iter().map(Entry::Source).collect(), git)
        }
        Input::Files(targets) => {
            super::stage("discover");
            let t = std::time::Instant::now();
            let indexed: Vec<String> = existing.iter().map(|(f, _)| f.clone()).collect();
            let root = wrongindex_storage::wrongstack_paths::node_resolve(project_root);
            let containment = Containment::new(&root.to_string_lossy());
            let list = targeted_files(project_root, &indexed, targets, &containment)?;
            timing("discover", t);
            let t = std::time::Instant::now();
            let mut v = Vec::with_capacity(list.len());
            for path in list {
                v.push(stat_target(path, &containment)?);
            }
            timing("read", t);
            (v, None)
        }
    };
    // Whole-project runs only: the discovered set (stale sweep, Git stamps).
    let discovered: Vec<String> = if project_scan {
        entries
            .iter()
            .filter_map(|e| match e {
                Entry::Source(f) => Some(f.path.clone()),
                _ => None,
            })
            .collect()
    } else {
        Vec::new()
    };
    let failed: HashSet<String> = entries
        .iter()
        .filter_map(|e| match e {
            Entry::Source(f) if f.failed => Some(f.path.clone()),
            _ => None,
        })
        .collect();
    let mut symbols_indexed: i64 = 0;

    // Git trust: a clean file whose stamp matches is skipped unread.
    let mut candidates: Vec<Entry> = entries
        .into_iter()
        .filter(|e| {
            let Entry::Source(f) = e else {
                return true;
            };
            let Some(m) = meta.get(f.path.as_str()) else {
                return true;
            };
            let Some(blob) = git.as_ref().and_then(|g| g.clean_blobs.get(&f.path)) else {
                return true;
            };
            let stamp = if m.content_hash.is_empty() {
                String::new()
            } else {
                format!("{blob}:{}", m.content_hash)
            };
            if m.git_blob.is_empty() || m.git_blob != stamp {
                return true;
            }
            symbols_indexed += m.symbol_count;
            false
        })
        .collect();
    // The scan trusted a stamp this transaction no longer holds.
    for e in &mut candidates {
        if let Entry::Source(f) = e {
            f.read()?;
        }
    }

    // Read phase outcome per file: failed, unchanged (content hash), to
    // parse, or (targeted) missing / ignored. Parsing does not depend on the
    // database, so it runs first.
    enum Item {
        /// No rows written, previous rows kept (failed read, no language).
        Nothing,
        Skip(String, &'static str, i64, String),
        Parse,
        Parsed(FileRow),
        Missing(String),
    }
    let mut pre: Vec<Item> = Vec::with_capacity(candidates.len());
    let mut to_parse: Vec<Source> = Vec::new();
    for e in candidates {
        match e {
            Entry::Missing(p) => pre.push(Item::Missing(p)),
            Entry::Ignored => pre.push(Item::Nothing),
            Entry::Source(f) if f.failed => pre.push(Item::Nothing),
            Entry::Source(f) => {
                if meta
                    .get(f.path.as_str())
                    .is_some_and(|m| !m.content_hash.is_empty() && m.content_hash == f.content_hash)
                {
                    pre.push(Item::Skip(f.path, f.lang, f.mtime_ms, f.content_hash));
                } else {
                    pre.push(Item::Parse);
                    to_parse.push(f);
                }
            }
        }
    }
    let (parsed_rows, _) = parse_scan(Scan {
        files: to_parse,
        git: None,
    })?;
    let mut parsed_iter = parsed_rows.into_iter();
    // Re-assemble in discovery order.
    let items: Vec<Item> = pre
        .into_iter()
        .map(|i| match i {
            Item::Parse => Item::Parsed(parsed_iter.next().expect("parsed row")),
            other => other,
        })
        .collect();

    super::stage("write");
    let t_write = std::time::Instant::now();
    let mut rewritten = Ordered::default();
    let mut added = Ordered::default();
    let mut deleted = Ordered::default();
    let mut rewritten_hashes: HashMap<String, String> = HashMap::new();
    let mut deferred = Ordered::default();
    let mut files_indexed = 0usize;
    let mut files_empty = 0usize;
    let mut note_rewritten =
        |file: &str, hash: &str, rewritten: &mut Ordered, added: &mut Ordered| {
            rewritten.add(file);
            rewritten_hashes.insert(file.to_string(), hash.to_string());
            if !meta.contains_key(file) {
                added.add(file);
            }
        };

    for batch in items.chunks(batch_width()) {
        let mut entries: Vec<(&FileRow, Vec<ParsedSymbol>)> = Vec::new();
        for item in batch {
            match item {
                Item::Nothing | Item::Parse => {}
                // A missing path in a targeted run is authoritative.
                Item::Missing(path) => {
                    if meta.contains_key(path.as_str()) {
                        if heal.is_some() {
                            heal_snapshot(&tx, path, &mut prev_snap)?;
                        }
                        delete_file(&tx, path)?;
                        deleted.add(path);
                    }
                }
                Item::Skip(path, lang, mtime, hash) => {
                    let m = meta[path.as_str()];
                    symbols_indexed += m.symbol_count;
                    // mtime changed, bytes did not: persist the new mtime
                    // (this rewrites `git_blob` to '', as WrongStack does).
                    if m.mtime_ms != *mtime {
                        upsert_file(&tx, path, lang, *mtime, hash, m.symbol_count)?;
                    }
                }
                Item::Parsed(f) => {
                    if f.symbols.is_empty() && f.refs.is_empty() {
                        note_rewritten(&f.path, &f.content_hash, &mut rewritten, &mut added);
                        replace_empty_file(&tx, f)?;
                        files_empty += 1;
                        continue;
                    }
                    let mut syms = Vec::with_capacity(f.symbols.len() + 1);
                    if needs_module_owner(&f.symbols, &f.refs) {
                        syms.push(module_owner());
                    }
                    syms.extend(f.symbols.iter().cloned());
                    note_rewritten(&f.path, &f.content_hash, &mut rewritten, &mut added);
                    entries.push((f, syms));
                }
            }
        }
        if !entries.is_empty() {
            if heal.is_some() {
                for (f, _) in &entries {
                    if meta.contains_key(f.path.as_str()) {
                        heal_snapshot(&tx, &f.path, &mut prev_snap)?;
                    }
                }
                for (f, syms) in &entries {
                    for s in syms {
                        next_snap.push(crate::sage::Snap {
                            name: s.name.clone(),
                            kind: s.kind.to_string(),
                            file: f.path.clone(),
                            signature: Some(s.signature.clone()),
                        });
                    }
                }
            }
            commit_batch(&tx, &entries, &mut deferred)?;
            for (_, syms) in &entries {
                symbols_indexed += syms.len() as i64;
                files_indexed += 1;
            }
        }
    }
    timing("incremental: batches", t_write);

    // finalizeIndexRun.
    let t = std::time::Instant::now();
    // Stale rows: only a whole-project run can tell (not a targeted list).
    if project_scan {
        let discovered_set: HashSet<&str> = discovered.iter().map(String::as_str).collect();
        for (file, _) in &existing {
            if !discovered_set.contains(file.as_str()) {
                if heal.is_some() {
                    heal_snapshot(&tx, file, &mut prev_snap)?;
                }
                delete_file(&tx, file)?;
                deleted.add(file);
            }
        }
    }
    if deferred.len() > 0 {
        resolve_names(&tx, &deferred.list)?;
    }
    // planRefBinding (incremental: version markers are current).
    let structure_before = get_meta(&tx, "relation_structure")?;
    let mut bind_names: Vec<(String, Vec<String>)> = Vec::new();
    let mut targeting: Vec<String> = rewritten.list.clone();
    targeting.extend(deleted.list.iter().cloned());
    ref_names_targeting(&tx, &targeting, &mut bind_names)?;
    let changes = Changes {
        rewritten: rewritten.list.clone(),
        added: added.list.clone(),
        deleted: deleted.list.clone(),
    };
    let relations_resolved = crate::relations::relation_pass_incremental(
        &tx,
        &project_root.to_string_lossy(),
        &changes,
        project_scan,
    )?;
    // runRefBinding.
    let bind_all = get_meta(&tx, "relation_structure")? != structure_before;
    if !bind_all && added.len() > 0 {
        ref_names_targeting(&tx, &added.list, &mut bind_names)?;
    }
    let bindings_changed = if !bind_all && rewritten.len() == 0 && bind_names.is_empty() {
        false
    } else {
        let r = if bind_all {
            crate::binding::bind_refs_all(&tx)?
        } else {
            crate::binding::bind_refs_scoped(&tx, &rewritten.list, &bind_names)?
        };
        set_meta(
            &tx,
            "ref_binding_version",
            crate::binding::REF_BINDING_VERSION,
        )?;
        r.updated > 0
    };
    let relations_changed = relations_resolved || bindings_changed;
    set_meta(&tx, "ref_resolution_version", "2")?;
    // Only a run that saw every file records the module-owner version.
    if project_scan {
        set_meta(&tx, "module_owner_version", "2")?;
    }
    set_meta(&tx, "relation_graph_version", "2")?;
    let changed_files = rewritten.len() + deleted.len();
    timing("incremental: relations + binding", t);

    // shouldRefreshRanks / runGraphRankPass.
    let t = std::time::Instant::now();
    if changed_files > 0 {
        let ranked: i64 = tx.query_row("SELECT COUNT(*) FROM symbol_rank", [], |r| r.get(0))?;
        let refresh = if ranked == 0 {
            true
        } else {
            let stale = get_meta(&tx, "rank_stale_files")?
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0)
                + changed_files as i64;
            if stale >= RANK_REFRESH_FILE_THRESHOLD {
                true
            } else {
                set_meta(&tx, "rank_stale_files", &stale.to_string())?;
                false
            }
        };
        if refresh {
            let (sym_ranks, file_ranks) = crate::rank::rank_rows(&tx)?;
            crate::rank::replace_ranks(&tx, "index_rank", &sym_ranks, &file_ranks)?;
            set_meta(&tx, "rank_version", crate::rank::RANK_VERSION)?;
            set_meta(&tx, "rank_stale_files", "0")?;
        }
    }
    timing("incremental: rank", t);

    // Git blob stamps for every discovered file Git reports clean.
    let t = std::time::Instant::now();
    if let Some(g) = &git
        && g.snapshot_key.is_some()
    {
        let mut updates: Vec<(String, String)> = Vec::new();
        let mut blesses = false;
        for file in &discovered {
            if failed.contains(file) {
                continue;
            }
            let was_rewritten = rewritten_hashes.contains_key(file);
            let previous = meta.get(file.as_str());
            let hash = if was_rewritten {
                rewritten_hashes.get(file).cloned()
            } else {
                previous.map(|m| m.content_hash.clone())
            };
            let Some(hash) = hash else { continue };
            let desired = match g.clean_blobs.get(file) {
                Some(b) if !hash.is_empty() => format!("{b}:{hash}"),
                _ => String::new(),
            };
            let current = if was_rewritten {
                String::new()
            } else {
                previous.map(|m| m.git_blob.clone()).unwrap_or_default()
            };
            if desired == current {
                continue;
            }
            if !desired.is_empty() {
                blesses = true;
            }
            updates.push((file.clone(), desired));
        }
        if blesses {
            let now = wrongindex_discovery::discover(project_root, &[]);
            if now.snapshot_key != g.snapshot_key || now.files != discovered {
                return Err(NativeError::SourceChanged);
            }
        }
        let mut up = tx.prepare_cached("UPDATE files SET git_blob = ? WHERE file = ?")?;
        for (file, blob) in &updates {
            up.execute([blob, file])?;
        }
    }
    timing("incremental: git trust", t);
    if (project_scan && changed_files > 0) || files_indexed >= 50 {
        tx.execute_batch("PRAGMA optimize")?;
    }
    let mut graph_stamp = get_meta(&tx, "graph_stamp")?.unwrap_or_default();
    if changed_files > 0 || relations_changed {
        graph_stamp = format!("{}:{changed_files}", now_ms());
        set_meta(&tx, "graph_stamp", &graph_stamp)?;
    }
    // A targeted run that changed nothing (a watcher echo) leaves it alone.
    if project_scan || changed_files > 0 {
        set_meta(&tx, "last_indexed", &now_ms().to_string())?;
    }
    // The commit is the publication: before it, readers see the previous
    // generation and a crash rolls everything back (WAL).
    super::fault_point("before-commit");
    tx.commit()?;
    super::fault_point("after-commit");
    conn.close().map_err(|(_, e)| e)?;
    // The heal, after the publication (WrongStack runs it just before its
    // commit; here a failed commit never leaves anchors pointing at rows
    // that were not published). Best effort, like WrongStack's.
    let mut sage_healed = None;
    if let Some(dir) = &heal
        && !prev_snap.is_empty()
        && !next_snap.is_empty()
    {
        let t = std::time::Instant::now();
        let root = project_root.to_string_lossy();
        let rel = |s: crate::sage::Snap| crate::sage::Snap {
            file: crate::sage::snapshot_rel(&root, &s.file),
            ..s
        };
        let prev: Vec<_> = prev_snap.into_iter().map(rel).collect();
        let next: Vec<_> = next_snap.into_iter().map(rel).collect();
        let moves = crate::sage::detect_moves(&prev, &next);
        if !moves.is_empty() {
            sage_healed = Some(crate::sage::remap(dir, &moves));
        }
        timing("incremental: SAGE anchors", t);
    }
    timing("incremental", t_all);
    Ok(Outcome {
        report: Report {
            files_indexed,
            files_empty,
            symbols_indexed: symbols_indexed as usize,
            graph_stamp,
            oversized_files: {
                let mut v: Vec<String> = failed.into_iter().collect();
                v.sort();
                v
            },
            sage_healed,
        },
        changed_files,
    })
}
