//! Read-only schema/profile inspection (WI-107, compatibility §5 step 1).
//!
//! Opening a shared index through WrongStack's ordinary writer constructor is
//! already a mutation (it can drop/rebuild tables, repair columns, rebuild FTS,
//! or drop `symbol_vectors`). This inspector never does any of that: it opens
//! the database with `SQLITE_OPEN_READ_ONLY` plus `query_only`, and reports a
//! verdict so admission can reject writes before any mutable initialization.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;

use crate::profile::{
    ADDITIVE_TABLES, Column, DEPRECATED_COLUMNS, Profile, REPAIRABLE_COLUMNS, baseline_profile,
    is_fts_shadow,
};

/// Overall admission verdict, ordered from best to worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Matches the baseline profile exactly (optional tables allowed).
    Compatible,
    /// Matches, and carries extra tables/columns/keys that must be preserved.
    CompatibleWithUnknownData,
    /// Older/partial database the baseline initializer would repair in place.
    /// Repair is a mutation and requires ownership + backup first.
    RepairRequired,
    /// Older `SCHEMA_VERSION`: the baseline initializer would DROP all tables.
    OlderSchema,
    /// Same version but structurally different from the profile.
    Divergent,
    /// Newer or non-numeric schema version: unknown to this build.
    NewerOrUnknownSchema,
    /// Readable SQLite, but not a WrongStack index.
    NotAnIndex,
    /// SQLite reports the file damaged: a read failed with
    /// `SQLITE_CORRUPT`, or its own check (`PRAGMA quick_check`, see
    /// [`crate::integrity`]) found damaged pages or a damaged full-text index.
    Corrupt,
    /// File exists but cannot be opened/read as SQLite.
    Unreadable,
    /// No `index.db` at the given location.
    Missing,
}

impl Verdict {
    /// Whether a future writer may operate on this database without a
    /// migration step. M1 never writes regardless.
    pub fn write_admissible(self) -> bool {
        matches!(
            self,
            Verdict::Compatible | Verdict::CompatibleWithUnknownData
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    UnknownData,
    Repair,
    Incompatible,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub severity: Severity,
    pub code: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub files: Option<i64>,
    pub symbols: Option<i64>,
    pub refs: Option<i64>,
    pub fts_rows: Option<i64>,
    pub file_concepts: Option<i64>,
    pub file_vectors: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    pub index_path: String,
    pub profile_id: String,
    pub verdict: Verdict,
    pub write_admissible: bool,
    pub schema_version: Option<String>,
    pub file_bytes: Option<u64>,
    pub wal_bytes: Option<u64>,
    pub journal_mode: Option<String>,
    pub symbol_vectors_present: bool,
    pub counts: Counts,
    /// Metadata keys present in the database (values of volatile keys included;
    /// this is local diagnostics, not a shared fixture).
    pub metadata: BTreeMap<String, String>,
    pub unknown_metadata_keys: Vec<String>,
    pub findings: Vec<Finding>,
    /// SQLite's own check, when one ran ([`inspect_index_checked`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub integrity: Option<crate::integrity::Integrity>,
}

struct Report {
    findings: Vec<Finding>,
}

impl Report {
    fn add(&mut self, severity: Severity, code: &'static str, detail: impl Into<String>) {
        self.findings.push(Finding {
            severity,
            code,
            detail: detail.into(),
        });
    }
}

/// Inspect `index.db` at `path` against the baseline profile without mutating it.
pub fn inspect_index(path: &Path) -> Inspection {
    inspect_with_profile(path, baseline_profile())
}

/// [`inspect_index`] plus SQLite's own consistency check of every page and
/// of the full-text index ([`crate::integrity`]); reads the whole file, so
/// it takes time on a large index. A damaged file is [`Verdict::Corrupt`],
/// except one a newer WrongStack wrote, which stays
/// [`Verdict::NewerOrUnknownSchema`] (nothing here may replace it), and one
/// that is not SQLite at all ([`Verdict::Unreadable`]).
pub fn inspect_index_checked(path: &Path) -> Inspection {
    inspect_with(path, baseline_profile(), true)
}

pub fn inspect_with_profile(path: &Path, profile: &Profile) -> Inspection {
    inspect_with(path, profile, false)
}

fn inspect_with(path: &Path, profile: &Profile, check: bool) -> Inspection {
    let mut out = Inspection {
        index_path: path.display().to_string(),
        profile_id: profile.id.clone(),
        verdict: Verdict::Compatible,
        write_admissible: false,
        schema_version: None,
        file_bytes: None,
        wal_bytes: None,
        journal_mode: None,
        symbol_vectors_present: false,
        counts: Counts::default(),
        metadata: BTreeMap::new(),
        unknown_metadata_keys: Vec::new(),
        findings: Vec::new(),
        integrity: None,
    };

    let meta = match std::fs::metadata(path) {
        Ok(m) if m.is_file() => m,
        Ok(_) => {
            return finish(
                out,
                Verdict::Unreadable,
                "not_a_file",
                "path is not a regular file",
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return finish(out, Verdict::Missing, "missing", "index.db does not exist");
        }
        Err(e) => return finish(out, Verdict::Unreadable, "stat_failed", e.to_string()),
    };
    out.file_bytes = Some(meta.len());
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    out.wal_bytes = std::fs::metadata(&wal).ok().map(|m| m.len());

    let conn = match open_read_only(path) {
        Ok(c) => c,
        Err(e) => return finish(out, Verdict::Unreadable, "open_failed", e.to_string()),
    };

    let mut report = Report {
        findings: Vec::new(),
    };
    let mut verdict = match analyze(&conn, profile, &mut out, &mut report) {
        Ok(v) => v,
        // Damaged pages; a file that is not SQLite at all stays unreadable.
        Err(e) if e.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseCorrupt) => {
            report.add(Severity::Incompatible, "read_failed", e.to_string());
            Verdict::Corrupt
        }
        Err(e) => {
            report.add(Severity::Incompatible, "read_failed", e.to_string());
            Verdict::Unreadable
        }
    };
    if check {
        match crate::integrity::check_connection(&conn) {
            Ok(i) => {
                if !i.ok {
                    report.add(
                        Severity::Incompatible,
                        "integrity_check_failed",
                        i.problems.join("; "),
                    );
                    if !matches!(verdict, Verdict::NewerOrUnknownSchema | Verdict::Unreadable) {
                        verdict = Verdict::Corrupt;
                    }
                }
                out.integrity = Some(i);
            }
            // Locked or an I/O error: nothing learned about the file.
            Err(e) => report.add(Severity::Info, "integrity_check_not_run", e.to_string()),
        }
    }
    out.findings = report.findings;
    out.verdict = verdict;
    out.write_admissible = verdict.write_admissible();
    out
}

fn finish(
    mut out: Inspection,
    verdict: Verdict,
    code: &'static str,
    detail: impl Into<String>,
) -> Inspection {
    out.verdict = verdict;
    out.write_admissible = false;
    out.findings.push(Finding {
        severity: Severity::Incompatible,
        code,
        detail: detail.into(),
    });
    out
}

fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(2))?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

#[derive(Debug)]
struct ColumnInfo {
    name: String,
    decl_type: String,
    notnull: bool,
    default: Option<String>,
    pk: i64,
}

fn table_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<ColumnInfo>> {
    // Table names come from sqlite_master/profile, never from user input; quote anyway.
    let sql = format!("PRAGMA table_xinfo(\"{}\")", table.replace('"', "\"\""));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        Ok(ColumnInfo {
            name: r.get("name")?,
            decl_type: r.get("type")?,
            notnull: r.get::<_, i64>("notnull")? == 1,
            default: r.get("dflt_value")?,
            pk: r.get("pk")?,
        })
    })?;
    rows.collect()
}

fn count(conn: &Connection, table: &str) -> Option<i64> {
    conn.query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |r| {
        r.get(0)
    })
    .ok()
}

fn analyze(
    conn: &Connection,
    profile: &Profile,
    out: &mut Inspection,
    report: &mut Report,
) -> rusqlite::Result<Verdict> {
    // sqlite_master read doubles as "is this SQLite at all".
    let mut objects: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
    {
        let mut stmt = conn.prepare("SELECT type, name, sql FROM sqlite_master")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(1)?,
                (r.get::<_, String>(0)?, r.get::<_, Option<String>>(2)?),
            ))
        })?;
        for row in rows {
            let (name, v) = row?;
            objects.insert(name, v);
        }
    }
    out.journal_mode = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
        .ok();

    if !objects.contains_key("metadata") {
        report.add(
            Severity::Incompatible,
            "no_metadata_table",
            "metadata table is absent",
        );
        return Ok(Verdict::NotAnIndex);
    }
    {
        let mut stmt = conn.prepare("SELECT key, value FROM metadata")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (k, v) = row?;
            out.metadata.insert(k, v);
        }
    }
    let known_keys: BTreeSet<&str> = profile.metadata_keys.iter().map(String::as_str).collect();
    out.unknown_metadata_keys = out
        .metadata
        .keys()
        .filter(|k| !known_keys.contains(k.as_str()))
        .cloned()
        .collect();
    for k in &out.unknown_metadata_keys {
        report.add(
            Severity::UnknownData,
            "unknown_metadata_key",
            format!("metadata key '{k}' is not in the profile; it must be preserved"),
        );
    }

    let version = out.metadata.get("version").cloned();
    out.schema_version = version.clone();
    match version.as_deref().map(str::parse::<i64>) {
        None => {
            report.add(
                Severity::Incompatible,
                "no_version",
                "metadata has no 'version' key",
            );
            return Ok(Verdict::NotAnIndex);
        }
        Some(Err(_)) => {
            report.add(
                Severity::Incompatible,
                "non_numeric_version",
                format!("version {:?} is not numeric", version.unwrap_or_default()),
            );
            return Ok(Verdict::NewerOrUnknownSchema);
        }
        Some(Ok(v)) if v > profile.schema_version => {
            report.add(
                Severity::Incompatible,
                "newer_schema",
                format!(
                    "schema version {v} is newer than supported {}",
                    profile.schema_version
                ),
            );
            return Ok(Verdict::NewerOrUnknownSchema);
        }
        Some(Ok(v)) if v < profile.schema_version => {
            report.add(Severity::Incompatible, "older_schema", format!("schema version {v} is older than {}; the baseline initializer would drop and rebuild every table", profile.schema_version));
            return Ok(Verdict::OlderSchema);
        }
        Some(Ok(_)) => {}
    }

    let mut worst = Verdict::Compatible;
    let mut bump = |v: Verdict| {
        if v > worst {
            worst = v;
        }
    };

    // Tables and columns.
    for (table, expected) in &profile.tables {
        if !objects.contains_key(table) {
            if ADDITIVE_TABLES.contains(&table.as_str()) {
                report.add(
                    Severity::Repair,
                    "missing_additive_table",
                    format!(
                        "table '{table}' is absent (created additively by the baseline initializer)"
                    ),
                );
                bump(Verdict::RepairRequired);
            } else {
                report.add(
                    Severity::Incompatible,
                    "missing_table",
                    format!("required table '{table}' is absent"),
                );
                bump(Verdict::Divergent);
            }
            continue;
        }
        let actual = table_columns(conn, table)?;
        bump(compare_columns(table, expected, &actual, report));
    }
    for (table, expected) in &profile.optional_tables {
        if objects.contains_key(table) {
            if table == "symbol_vectors" {
                out.symbol_vectors_present = true;
            }
            if let Some(expected) = expected {
                let actual = table_columns(conn, table)?;
                bump(compare_columns(table, expected, &actual, report));
            }
        }
    }

    // Virtual FTS table.
    for vt in &profile.virtual_tables {
        match objects.get(&vt.name) {
            None => {
                report.add(
                    Severity::Repair,
                    "missing_fts",
                    format!("virtual table '{}' is absent", vt.name),
                );
                bump(Verdict::RepairRequired);
            }
            Some((_, Some(sql))) if sql.contains("unicode61") => {
                report.add(
                    Severity::Repair,
                    "legacy_fts_tokenizer",
                    "symbols_fts uses unicode61; the baseline rebuilds it with trigram",
                );
                bump(Verdict::RepairRequired);
            }
            Some((_, Some(sql))) if normalize_sql(sql) != normalize_sql(&vt.sql) => {
                report.add(
                    Severity::Incompatible,
                    "fts_definition_differs",
                    format!("symbols_fts definition differs: {sql}"),
                );
                bump(Verdict::Divergent);
            }
            Some(_) => {}
        }
    }

    // Indexes.
    for idx in &profile.indexes {
        match objects.get(&idx.name) {
            None => {
                report.add(
                    Severity::Repair,
                    "missing_index",
                    format!("index '{}' on '{}' is absent", idx.name, idx.table),
                );
                bump(Verdict::RepairRequired);
            }
            Some((kind, sql)) => {
                if kind != "index" || normalize_opt(sql) != normalize_opt(&idx.sql) {
                    report.add(
                        Severity::Incompatible,
                        "index_definition_differs",
                        format!("index '{}' differs from the profile", idx.name),
                    );
                    bump(Verdict::Divergent);
                }
            }
        }
    }

    // Unknown objects must be preserved, never dropped.
    let expected_names: BTreeSet<&str> = profile
        .tables
        .keys()
        .chain(profile.optional_tables.keys())
        .map(String::as_str)
        .chain(profile.virtual_tables.iter().map(|v| v.name.as_str()))
        .chain(profile.indexes.iter().map(|i| i.name.as_str()))
        .collect();
    for (name, (kind, _)) in &objects {
        if expected_names.contains(name.as_str())
            || name.starts_with("sqlite_")
            || is_fts_shadow(name)
        {
            continue;
        }
        report.add(
            Severity::UnknownData,
            "unknown_object",
            format!("{kind} '{name}' is not in the profile; it must be preserved"),
        );
        bump(Verdict::CompatibleWithUnknownData);
    }
    // Semantic markers: same storage schema but a different engine revision.
    // WrongStack treats a stale relation/ref marker as "rebuild everything"
    // on its next run, so a mismatch is not silently compatible.
    for (key, expected) in &crate::profile::baseline_semantic().semantic.version_markers {
        if key == "version" {
            continue;
        }
        match out.metadata.get(key) {
            None => report.add(
                Severity::Info,
                "semantic_marker_absent",
                format!("metadata '{key}' absent (index never finalized by this engine revision); the baseline recomputes it"),
            ),
            Some(actual) if actual != expected => {
                report.add(
                    Severity::Incompatible,
                    "semantic_marker_mismatch",
                    format!("metadata '{key}' is '{actual}', profile expects '{expected}'; the engine semantics differ"),
                );
                bump(Verdict::Divergent);
            }
            Some(_) => {}
        }
    }
    if !out.unknown_metadata_keys.is_empty() {
        bump(Verdict::CompatibleWithUnknownData);
    }

    // Counts and cheap consistency checks.
    out.counts = Counts {
        files: count(conn, "files"),
        symbols: count(conn, "symbols"),
        refs: count(conn, "refs"),
        fts_rows: count(conn, "symbols_fts"),
        file_concepts: count(conn, "file_concepts"),
        file_vectors: count(conn, "file_vectors"),
    };
    if let (Some(s), Some(f)) = (out.counts.symbols, out.counts.fts_rows)
        && s != f
    {
        report.add(
            Severity::Repair,
            "fts_out_of_sync",
            format!("{s} symbols but {f} FTS rows; the baseline initializer rebuilds FTS on open"),
        );
        bump(Verdict::RepairRequired);
    }
    if objects.contains_key("symbols") {
        let max_id: Option<i64> = conn
            .query_row("SELECT MAX(id) FROM symbols", [], |r| r.get(0))
            .optional()?
            .flatten();
        let next = out
            .metadata
            .get("next_symbol_id")
            .and_then(|v| v.parse::<i64>().ok());
        match (max_id, next) {
            (Some(max), Some(next)) if next <= max => {
                report.add(
                    Severity::Info,
                    "next_symbol_id_behind",
                    format!(
                        "next_symbol_id {next} <= MAX(id) {max}; allocation clamps to MAX(id)+1"
                    ),
                );
            }
            (_, None) => report.add(
                Severity::Info,
                "next_symbol_id_unseeded",
                "next_symbol_id absent; seeded from MAX(id)+1 on first write",
            ),
            _ => {}
        }
    }

    Ok(worst)
}

fn compare_columns(
    table: &str,
    expected: &[Column],
    actual: &[ColumnInfo],
    report: &mut Report,
) -> Verdict {
    let mut worst = Verdict::Compatible;
    let mut bump = |v: Verdict| {
        if v > worst {
            worst = v;
        }
    };
    for e in expected {
        match actual.iter().find(|a| a.name == e.name) {
            None if REPAIRABLE_COLUMNS.contains(&(table, e.name.as_str())) => {
                report.add(
                    Severity::Repair,
                    "missing_repairable_column",
                    format!(
                        "{table}.{} is absent (added by repairMissingColumns)",
                        e.name
                    ),
                );
                bump(Verdict::RepairRequired);
            }
            None => {
                report.add(
                    Severity::Incompatible,
                    "missing_column",
                    format!("{table}.{} is absent", e.name),
                );
                bump(Verdict::Divergent);
            }
            Some(a) => {
                if !a.decl_type.eq_ignore_ascii_case(&e.decl_type)
                    || a.notnull != e.notnull
                    || a.pk != e.pk
                    || normalize_opt(&a.default) != normalize_opt(&e.default)
                {
                    report.add(
                        Severity::Incompatible,
                        "column_definition_differs",
                        format!(
                            "{table}.{}: expected {} notnull={} default={:?} pk={}, found {} notnull={} default={:?} pk={}",
                            e.name, e.decl_type, e.notnull, e.default, e.pk, a.decl_type, a.notnull, a.default, a.pk
                        ),
                    );
                    bump(Verdict::Divergent);
                }
            }
        }
    }
    for a in actual {
        if expected.iter().any(|e| e.name == a.name) {
            continue;
        }
        if DEPRECATED_COLUMNS.contains(&(table, a.name.as_str())) {
            report.add(
                Severity::Repair,
                "deprecated_column",
                format!(
                    "{table}.{} is deprecated; the baseline drops it on open",
                    a.name
                ),
            );
            bump(Verdict::RepairRequired);
        } else {
            report.add(
                Severity::UnknownData,
                "unknown_column",
                format!(
                    "{table}.{} is not in the profile; it must be preserved",
                    a.name
                ),
            );
            bump(Verdict::CompatibleWithUnknownData);
        }
    }
    worst
}

fn normalize_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_opt(sql: &Option<String>) -> Option<String> {
    sql.as_deref().map(normalize_sql)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/oracle")
            .join(name)
            .join("index.db")
    }

    fn copy_fixture(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("index.db");
        std::fs::copy(fixture(name), &dst).unwrap();
        (dir, dst)
    }

    fn sha(path: &Path) -> String {
        hex::encode(Sha256::digest(std::fs::read(path).unwrap()))
    }

    fn mutate(path: &Path, sql: &str) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(sql).unwrap();
        // Leave a single-file database so the read-only open sees the change.
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    }

    #[test]
    fn oracle_fixtures_are_compatible_and_untouched() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/oracle");
        let mut names: Vec<String> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join("index.db").is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        // A run that discovers no fixtures must not pass silently.
        assert!(
            names.len() >= 8,
            "expected the oracle corpus, found {names:?}"
        );
        for name in &names {
            let (_dir, db) = copy_fixture(name);
            let before = sha(&db);
            let r = inspect_index(&db);
            assert_eq!(r.verdict, Verdict::Compatible, "{name}: {:#?}", r.findings);
            assert!(r.write_admissible);
            assert_eq!(r.schema_version.as_deref(), Some("5"));
            assert!(r.counts.symbols.unwrap() > 0);
            assert_eq!(r.counts.symbols, r.counts.fts_rows);
            assert!(
                r.unknown_metadata_keys.is_empty(),
                "{:?}",
                r.unknown_metadata_keys
            );
            assert_eq!(
                before,
                sha(&db),
                "{name}: inspection changed index.db bytes"
            );
        }
    }

    #[test]
    fn missing_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let r = inspect_index(&dir.path().join("index.db"));
        assert_eq!(r.verdict, Verdict::Missing);
        assert!(!r.write_admissible);
    }

    #[test]
    fn non_sqlite_file_is_unreadable_and_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("index.db");
        std::fs::write(
            &p,
            b"definitely not sqlite, just some bytes padding padding",
        )
        .unwrap();
        let before = sha(&p);
        let r = inspect_index(&p);
        assert_eq!(r.verdict, Verdict::Unreadable, "{:#?}", r.findings);
        assert_eq!(before, sha(&p));
    }

    #[test]
    fn foreign_sqlite_is_not_an_index() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("index.db");
        Connection::open(&p)
            .unwrap()
            .execute_batch("CREATE TABLE other(x)")
            .unwrap();
        assert_eq!(inspect_index(&p).verdict, Verdict::NotAnIndex);
    }

    #[test]
    fn newer_schema_is_rejected_without_mutation() {
        let (_dir, db) = copy_fixture("basic-ts");
        mutate(&db, "UPDATE metadata SET value = '6' WHERE key = 'version'");
        let before = sha(&db);
        let r = inspect_index(&db);
        assert_eq!(r.verdict, Verdict::NewerOrUnknownSchema);
        assert!(!r.write_admissible);
        assert_eq!(before, sha(&db));
    }

    #[test]
    fn older_schema_is_reported_as_destructive_rebuild() {
        let (_dir, db) = copy_fixture("basic-ts");
        mutate(&db, "UPDATE metadata SET value = '4' WHERE key = 'version'");
        assert_eq!(inspect_index(&db).verdict, Verdict::OlderSchema);
    }

    #[test]
    fn unknown_data_is_compatible_but_flagged() {
        let (_dir, db) = copy_fixture("basic-ts");
        mutate(
            &db,
            "CREATE TABLE future_feature(x); INSERT INTO metadata VALUES ('future_key', '1');",
        );
        let r = inspect_index(&db);
        assert_eq!(
            r.verdict,
            Verdict::CompatibleWithUnknownData,
            "{:#?}",
            r.findings
        );
        assert_eq!(r.unknown_metadata_keys, vec!["future_key".to_string()]);
        assert!(r.write_admissible);
    }

    #[test]
    fn missing_rank_tables_require_repair() {
        let (_dir, db) = copy_fixture("basic-ts");
        mutate(&db, "DROP TABLE symbol_rank; DROP TABLE file_rank;");
        let r = inspect_index(&db);
        assert_eq!(r.verdict, Verdict::RepairRequired, "{:#?}", r.findings);
        assert!(!r.write_admissible);
    }

    #[test]
    fn semantic_marker_mismatch_is_divergent() {
        let (_dir, db) = copy_fixture("basic-ts");
        mutate(
            &db,
            "UPDATE metadata SET value = '3' WHERE key = 'relation_graph_version'",
        );
        let r = inspect_index(&db);
        assert_eq!(r.verdict, Verdict::Divergent, "{:#?}", r.findings);
        assert!(
            r.findings
                .iter()
                .any(|f| f.code == "semantic_marker_mismatch")
        );
    }

    #[test]
    fn missing_core_column_is_divergent() {
        let (_dir, db) = copy_fixture("basic-ts");
        mutate(&db, "ALTER TABLE symbols DROP COLUMN doc_comment");
        let r = inspect_index(&db);
        assert_eq!(r.verdict, Verdict::Divergent, "{:#?}", r.findings);
    }

    #[test]
    fn fts_desync_requires_repair() {
        let (_dir, db) = copy_fixture("basic-ts");
        mutate(
            &db,
            "DELETE FROM symbols_fts WHERE rowid = (SELECT MIN(rowid) FROM symbols_fts)",
        );
        let r = inspect_index(&db);
        assert_eq!(r.verdict, Verdict::RepairRequired, "{:#?}", r.findings);
        assert!(r.findings.iter().any(|f| f.code == "fts_out_of_sync"));
    }
}
