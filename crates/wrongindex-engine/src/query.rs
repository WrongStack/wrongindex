//! Native read operations over a WrongStack v5 `index.db` (WI-511 slice).
//!
//! The database is opened read-only (`query_only`, no schema initializer, no
//! writes), so serving a query never mutates the index and does not require
//! ownership. Each operation is a port of the `callInline` service with the
//! same name and is gated by the recorded oracle queries
//! (`tests/fixtures/oracle/*/queries.json`). Operations not ported yet return
//! [`QueryError::Unsupported`] so a caller can fall back.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};

pub const SCHEMA_VERSION: i64 = 5;

/// Operations answered natively (the rest return `Unsupported`).
pub const NATIVE_OPS: &[&str] = &["stats"];

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("not supported by the native engine yet: {0}")]
    Unsupported(String),
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// A read-only view of one index directory.
pub struct Reader {
    conn: Connection,
    index_dir: String,
}

impl Reader {
    pub fn open(index_dir: &Path) -> Result<Self, QueryError> {
        let db = index_dir.join("index.db");
        // Read-write without create, but query-only: a read-only connection
        // to a WAL database leaves `-wal`/`-shm` behind after it closes.
        let conn = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.pragma_update(None, "query_only", true)?;
        conn.pragma_update(None, "busy_timeout", 15000)?;
        Ok(Self {
            conn,
            index_dir: index_dir.display().to_string(),
        })
    }

    /// Run `op` with WrongStack's argument shape (`projectRoot`/`indexDir`
    /// are ignored: the reader already knows its index).
    pub fn call(&self, op: &str, args: &Value) -> Result<Value, QueryError> {
        let _ = args;
        match op {
            "stats" => self.stats(),
            other => Err(QueryError::Unsupported(format!("query op '{other}'"))),
        }
    }

    /// `getStatsWithStatement` (`writer-admin.ts`).
    pub fn stats(&self) -> Result<Value, QueryError> {
        let c = &self.conn;
        let count = |sql: &str| -> rusqlite::Result<i64> { c.query_row(sql, [], |r| r.get(0)) };
        let group = |sql: &str| -> rusqlite::Result<Map<String, Value>> {
            let mut m = Map::new();
            let mut stmt = c.prepare(sql)?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (k, n) = row?;
                m.insert(k, json!(n));
            }
            Ok(m)
        };
        let last: Option<String> = c
            .query_row(
                "SELECT value FROM metadata WHERE key = 'last_indexed'",
                [],
                |r| r.get(0),
            )
            .ok();
        let size =
            std::fs::metadata(Path::new(&self.index_dir).join("index.db")).map_or(0, |m| m.len());
        Ok(json!({
            "totalSymbols": count("SELECT COUNT(*) FROM symbols")?,
            "totalFiles": count("SELECT COUNT(*) FROM files")?,
            "byLang": group("SELECT lang, COUNT(*) FROM symbols GROUP BY lang")?,
            "byKind": group("SELECT kind, COUNT(*) FROM symbols GROUP BY kind")?,
            "indexPath": self.index_dir,
            "lastIndexed": last.as_deref().map_or(Value::Null, js_number),
            "sizeBytes": size,
            "version": SCHEMA_VERSION,
        }))
    }
}

/// `Number(s)` as JSON: integral values as integers, NaN/Infinity as null.
pub fn js_number(s: &str) -> Value {
    let t = s.trim();
    let v: f64 = if t.is_empty() {
        0.0
    } else {
        t.parse().unwrap_or(f64::NAN)
    };
    if !v.is_finite() {
        Value::Null
    } else if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}
