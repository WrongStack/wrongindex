//! SQLite's own consistency check of an index (architecture §9
//! "Corruption").
//!
//! `PRAGMA quick_check` reads every page and verifies the b-trees, and since
//! SQLite 3.44 it also runs the full-text index's own check (FTS5's
//! `xIntegrity`): a `symbols_fts` whose inverted index no longer matches its
//! content is reported here, while queries on it may still answer (wrongly).
//! It never writes: the connection is opened read-only with `query_only`.

use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

/// Problems reported at most (`PRAGMA quick_check(N)`).
const MAX_PROBLEMS: u32 = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Integrity {
    /// SQLite found nothing wrong.
    pub ok: bool,
    /// What SQLite reported (at most 20 lines), or the error that stopped
    /// the check (a damaged page can make reading fail outright).
    pub problems: Vec<String>,
    pub checked_at_ms: i64,
    pub duration_ms: u64,
}

impl Integrity {
    /// One line for messages: the first problem SQLite named, and how many
    /// more there are (it stops at 20).
    pub fn summary(&self) -> String {
        let lines: Vec<&str> = self
            .problems
            .iter()
            .flat_map(|p| p.lines())
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("*** in database"))
            .collect();
        match lines.len() {
            0 => String::new(),
            1 => lines[0].to_string(),
            2 => format!("{}, and 1 other problem", lines[0]),
            n if n >= MAX_PROBLEMS as usize => {
                format!("{}, and {} or more other problems", lines[0], n - 1)
            }
            n => format!("{}, and {} other problems", lines[0], n - 1),
        }
    }
}

/// SQLite says the file is damaged, or is not a database at all.
pub fn is_corrupt(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Check `conn`'s main database. `Err` only for what says nothing about the
/// file (the database is locked, an I/O error); damage is `Ok` with
/// `ok: false`.
pub fn check_connection(conn: &Connection) -> rusqlite::Result<Integrity> {
    let started = Instant::now();
    let checked_at_ms = now_ms();
    let lines = (|| {
        let mut stmt = conn.prepare(&format!("PRAGMA quick_check({MAX_PROBLEMS})"))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<String>>>()
    })();
    let duration_ms = started.elapsed().as_millis() as u64;
    let problems = match lines {
        Ok(lines) if lines.len() == 1 && lines[0] == "ok" => Vec::new(),
        Ok(lines) => lines,
        Err(e) if is_corrupt(&e) => vec![e.to_string()],
        Err(e) => return Err(e),
    };
    Ok(Integrity {
        ok: problems.is_empty(),
        problems,
        checked_at_ms,
        duration_ms,
    })
}

/// Check the database at `db` (read-only).
pub fn quick_check(db: &Path) -> rusqlite::Result<Integrity> {
    let started = Instant::now();
    let checked_at_ms = now_ms();
    let opened = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .and_then(|c| {
        c.busy_timeout(Duration::from_secs(5))?;
        c.pragma_update(None, "query_only", true)?;
        Ok(c)
    });
    match opened {
        Ok(conn) => check_connection(&conn),
        Err(e) if is_corrupt(&e) => Ok(Integrity {
            ok: false,
            problems: vec![e.to_string()],
            checked_at_ms,
            duration_ms: started.elapsed().as_millis() as u64,
        }),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fts_db(dir: &Path) -> std::path::PathBuf {
        let db = dir.join("index.db");
        let c = Connection::open(&db).unwrap();
        c.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE VIRTUAL TABLE symbols_fts USING fts5(text, tokenize = 'trigram');",
        )
        .unwrap();
        let tx = c.unchecked_transaction().unwrap();
        for i in 0..3000 {
            tx.execute(
                "INSERT INTO symbols_fts(rowid, text) VALUES (?1, ?2)",
                rusqlite::params![i, format!("function name_{i} helper value {}", i * 7)],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        db
    }

    #[test]
    fn a_sound_index_passes() {
        let dir = tempfile::tempdir().unwrap();
        let db = fts_db(dir.path());
        let i = quick_check(&db).unwrap();
        assert!(i.ok, "{i:?}");
        assert!(i.problems.is_empty());
    }

    /// The full-text index no longer matching its content: queries still
    /// answer, the check does not pass.
    #[test]
    fn a_damaged_full_text_index_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let db = fts_db(dir.path());
        {
            let c = Connection::open(&db).unwrap();
            c.execute(
                "UPDATE symbols_fts_content SET c0 = 'something else entirely' WHERE id = 5",
                [],
            )
            .unwrap();
            let n: i64 = c
                .query_row(
                    "SELECT count(*) FROM symbols_fts WHERE symbols_fts MATCH 'helper'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 3000, "the query does not notice");
        }
        let before = std::fs::read(&db).unwrap();
        let i = quick_check(&db).unwrap();
        assert!(!i.ok);
        assert!(
            i.problems.iter().any(|p| p.contains("symbols_fts")),
            "{i:?}"
        );
        assert_eq!(std::fs::read(&db).unwrap(), before, "the check wrote");
    }

    /// Garbage over pages in the middle of the file (the header intact).
    #[test]
    fn damaged_pages_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let db = fts_db(dir.path());
        let mut bytes = std::fs::read(&db).unwrap();
        let page = 4096;
        let pages = bytes.len() / page;
        assert!(pages > 10, "{pages}");
        for b in &mut bytes[page * (pages / 2)..page * (pages / 2 + 3)] {
            *b = 0x5a;
        }
        std::fs::write(&db, &bytes).unwrap();
        let i = quick_check(&db).unwrap();
        assert!(!i.ok, "{i:?}");
        assert!(!i.problems.is_empty());
        let one = i.summary();
        assert!(!one.contains('\n') && !one.starts_with("***"), "{one}");
        assert!(
            one.starts_with("Tree ") || one.contains("malformed"),
            "{one}"
        );
    }

    #[test]
    fn the_summary_is_one_line() {
        let mut i = Integrity {
            ok: false,
            problems: vec!["*** in database main ***\nPage 5: a\nPage 6: b".into()],
            checked_at_ms: 0,
            duration_ms: 0,
        };
        assert_eq!(i.summary(), "Page 5: a, and 1 other problem");
        i.problems = vec![(0..20).map(|n| format!("Page {n}: x\n")).collect()];
        assert_eq!(i.summary(), "Page 0: x, and 19 or more other problems");
        i.problems = vec!["database disk image is malformed".into()];
        assert_eq!(i.summary(), "database disk image is malformed");
    }

    #[test]
    fn a_file_that_is_not_a_database_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        std::fs::write(&db, vec![b'x'; 8192]).unwrap();
        let i = quick_check(&db).unwrap();
        assert!(!i.ok);
        assert!(i.problems[0].contains("not a database"), "{i:?}");
    }
}
