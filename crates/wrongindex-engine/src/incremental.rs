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
//! marker current, no SAGE memories to heal, no vectors); anything else is
//! left to the fresh rebuild.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::Connection;

use super::{
    FileRow, NativeError, ParsedSymbol, Report, Scan, Source, assign_refs, batch_width, get_meta,
    insert_fts, insert_refs, insert_symbols, module_owner, needs_module_owner, now_ms, parse_scan,
    rows, set_meta, timing,
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
fn steady_state(tx: &Connection, project_root: &Path) -> rusqlite::Result<Option<String>> {
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
    if project_root.join(".wrongstack").join("memories").exists() {
        return Ok(Some("SAGE memories would be healed".into()));
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
pub(super) fn applicable(project_root: &Path, db: &Path) -> rusqlite::Result<Option<String>> {
    if std::env::var("WRONGINDEX_NATIVE_INCREMENTAL").is_ok_and(|v| v == "0") {
        return Ok(Some("disabled by WRONGINDEX_NATIVE_INCREMENTAL=0".into()));
    }
    let conn = Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "query_only", true)?;
    steady_state(&conn, project_root)
}

/// One incremental whole-project run over `db`, an existing baseline index
/// [`applicable`] accepted.
pub(super) fn update(project_root: &Path, db: &Path, scan: Scan) -> Result<Outcome, NativeError> {
    let t_all = std::time::Instant::now();
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
    if let Some(why) = steady_state(&tx, project_root)? {
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

    let Scan { files, git } = scan;
    let discovered: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    let failed: HashSet<String> = files
        .iter()
        .filter(|f| f.failed)
        .map(|f| f.path.clone())
        .collect();
    let mut symbols_indexed: i64 = 0;

    // Git trust: a clean file whose stamp matches is skipped unread.
    let mut candidates: Vec<Source> = files
        .into_iter()
        .filter(|f| {
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
    for f in &mut candidates {
        f.read()?;
    }

    // Read phase outcome per file: failed, unchanged (content hash), or to
    // parse. Parsing does not depend on the database, so it runs first.
    #[derive(PartialEq)]
    enum Kind {
        Failed,
        Skip,
        Parse,
    }
    let kinds: Vec<Kind> = candidates
        .iter()
        .map(|f| {
            if f.failed {
                Kind::Failed
            } else if meta
                .get(f.path.as_str())
                .is_some_and(|m| !m.content_hash.is_empty() && m.content_hash == f.content_hash)
            {
                Kind::Skip
            } else {
                Kind::Parse
            }
        })
        .collect();
    let mut skip_rows: Vec<Option<(String, &'static str, i64, String)>> = Vec::new();
    let mut to_parse: Vec<Source> = Vec::new();
    for (f, k) in candidates.into_iter().zip(&kinds) {
        match k {
            Kind::Parse => {
                skip_rows.push(None);
                to_parse.push(f);
            }
            _ => skip_rows.push(Some((f.path, f.lang, f.mtime_ms, f.content_hash))),
        }
    }
    let (parsed_rows, _) = parse_scan(Scan {
        files: to_parse,
        git: None,
    })?;
    let mut parsed_iter = parsed_rows.into_iter();
    // Re-assemble in discovery order.
    enum Item {
        Failed,
        Skip(String, &'static str, i64, String),
        Parsed(FileRow),
    }
    let items: Vec<Item> = kinds
        .iter()
        .zip(skip_rows)
        .map(|(k, s)| match (k, s) {
            (Kind::Parse, _) => Item::Parsed(parsed_iter.next().expect("parsed row")),
            (Kind::Failed, Some(_)) => Item::Failed,
            (_, Some((p, l, m, h))) => Item::Skip(p, l, m, h),
            (_, None) => unreachable!(),
        })
        .collect();

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
                Item::Failed => {}
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
    let discovered_set: HashSet<&str> = discovered.iter().map(String::as_str).collect();
    for (file, _) in &existing {
        if !discovered_set.contains(file.as_str()) {
            delete_file(&tx, file)?;
            deleted.add(file);
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
    set_meta(&tx, "module_owner_version", "2")?;
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
    if changed_files > 0 || files_indexed >= 50 {
        tx.execute_batch("PRAGMA optimize")?;
    }
    let mut graph_stamp = get_meta(&tx, "graph_stamp")?.unwrap_or_default();
    if changed_files > 0 || relations_changed {
        graph_stamp = format!("{}:{changed_files}", now_ms());
        set_meta(&tx, "graph_stamp", &graph_stamp)?;
    }
    set_meta(&tx, "last_indexed", &now_ms().to_string())?;
    // The commit is the publication: before it, readers see the previous
    // generation and a crash rolls everything back (WAL).
    super::fault_point("before-commit");
    tx.commit()?;
    super::fault_point("after-commit");
    conn.close().map_err(|(_, e)| e)?;
    timing("incremental", t_all);
    Ok(Outcome {
        report: Report {
            files_indexed,
            files_empty,
            symbols_indexed: symbols_indexed as usize,
            graph_stamp,
        },
        changed_files,
    })
}
