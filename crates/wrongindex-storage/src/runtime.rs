//! Effective SQLite runtime facts (compatibility §3.2). A crate version is not
//! evidence of SQLite behavior, so these are probed at runtime.

use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqliteRuntime {
    pub version: String,
    pub fts5_trigram: bool,
    pub compile_options: Vec<String>,
}

pub fn sqlite_runtime() -> SqliteRuntime {
    let version = rusqlite::version().to_string();
    let Ok(conn) = Connection::open_in_memory() else {
        return SqliteRuntime {
            version,
            fts5_trigram: false,
            compile_options: Vec::new(),
        };
    };
    let fts5_trigram = conn
        .execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(text, tokenize = 'trigram');
             INSERT INTO t(text) VALUES ('greetPerson');",
        )
        .and_then(|_| {
            conn.query_row("SELECT count(*) FROM t WHERE t MATCH 'Pers'", [], |r| {
                r.get::<_, i64>(0)
            })
        })
        .map(|n| n == 1)
        .unwrap_or(false);
    let compile_options = conn
        .prepare("PRAGMA compile_options")
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()
        })
        .unwrap_or_default();
    SqliteRuntime {
        version,
        fts5_trigram,
        compile_options,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundled_sqlite_supports_fts5_trigram() {
        let rt = super::sqlite_runtime();
        assert!(
            rt.fts5_trigram,
            "FTS5 trigram unavailable in {}",
            rt.version
        );
        assert!(rt.compile_options.iter().any(|o| o.contains("FTS5")));
    }
}
