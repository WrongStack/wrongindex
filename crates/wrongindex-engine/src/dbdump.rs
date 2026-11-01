//! Canonical, read-only dump of an `index.db` for differential tests.
//!
//! Every table in `sqlite_master` (FTS shadow tables included) is read in a
//! deterministic order and rendered as JSON values, so two databases written
//! by different engines can be compared row for row. Columns whose values are
//! legitimately run-specific (times, random stamps) are replaced by a marker
//! through [`Volatile`].

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};

/// `(table, column)` pairs, and metadata keys, whose values differ per run.
#[derive(Debug, Clone, Default)]
pub struct Volatile {
    pub columns: Vec<(&'static str, &'static str)>,
    pub metadata_keys: Vec<&'static str>,
    /// Tables whose content is ignored (only their presence is compared).
    pub tables: Vec<&'static str>,
    /// Text substitutions applied to every TEXT value (e.g. a project root
    /// that differs between the two databases), longest first.
    pub rewrite: Vec<(String, String)>,
}

impl Volatile {
    /// Values that differ between two WrongStack runs over identical input.
    pub fn wrongstack_run() -> Self {
        Self {
            columns: vec![("files", "mtime_ms"), ("files", "last_indexed")],
            metadata_keys: vec!["graph_stamp", "last_indexed", "relation_epoch"],
            tables: vec![],
            rewrite: vec![],
        }
    }

    /// Logical comparison across SQLite builds: FTS5 shadow tables (segment
    /// layout, docsize encoding) are represented by the virtual table's own
    /// `rowid, text` rows instead.
    pub fn logical() -> Self {
        Self {
            tables: vec![
                "symbols_fts_config",
                "symbols_fts_content",
                "symbols_fts_data",
                "symbols_fts_docsize",
                "symbols_fts_idx",
            ],
            ..Self::wrongstack_run()
        }
    }
}

pub type Dump = BTreeMap<String, Vec<Value>>;

pub fn dump(path: &Path, volatile: &Volatile) -> rusqlite::Result<Dump> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "query_only", true)?;
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let virtual_tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND sql LIKE 'CREATE VIRTUAL TABLE%'")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    let mut out = Dump::new();
    for t in tables {
        if volatile.tables.contains(&t.as_str()) {
            out.insert(t, vec![json!("<ignored>")]);
            continue;
        }
        let quoted = format!("\"{}\"", t.replace('"', "\"\""));
        // A virtual table's rowid is part of its content (FTS rowid = symbol id).
        let select = if virtual_tables.contains(&t) {
            format!("SELECT rowid AS rowid, * FROM {quoted}")
        } else {
            format!("SELECT * FROM {quoted}")
        };
        let mut stmt = conn.prepare(&select)?;
        let names: Vec<String> = stmt.column_names().map(|s| s.to_string()).collect();
        let mut rows: Vec<Value> = stmt
            .query_map([], |r| {
                let mut row = serde_json::Map::new();
                for (i, n) in names.iter().enumerate() {
                    let v = if volatile.columns.contains(&(t.as_str(), n.as_str())) {
                        json!("<volatile>")
                    } else {
                        match render(r.get_ref(i)?) {
                            Value::String(mut s) => {
                                for (from, to) in &volatile.rewrite {
                                    s = s.replace(from.as_str(), to);
                                }
                                Value::String(s)
                            }
                            other => other,
                        }
                    };
                    row.insert(n.clone(), v);
                }
                Ok(Value::Object(row))
            })?
            .collect::<Result<_, _>>()?;
        if t == "metadata" {
            for row in &mut rows {
                let key = row["key"].as_str().unwrap_or_default().to_string();
                if volatile.metadata_keys.contains(&key.as_str()) {
                    row["value"] = json!("<volatile>");
                }
            }
        }
        // Row order is not part of the contract; compare as sorted sets.
        rows.sort_by_key(|r| r.to_string());
        out.insert(t, rows);
    }
    Ok(out)
}

fn render(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => {
            json!({ "blob": b.iter().map(|x| format!("{x:02x}")).collect::<String>() })
        }
    }
}

/// Human-readable differences between two dumps (empty when identical).
pub fn diff(want: &Dump, got: &Dump, max_rows: usize) -> Vec<String> {
    let mut out = Vec::new();
    let names: std::collections::BTreeSet<&String> = want.keys().chain(got.keys()).collect();
    for t in names {
        match (want.get(t), got.get(t)) {
            (Some(_), None) => out.push(format!("table {t}: missing")),
            (None, Some(_)) => out.push(format!("table {t}: unexpected")),
            (Some(w), Some(g)) if w != g => {
                let only_w: Vec<&Value> = w.iter().filter(|r| !g.contains(r)).collect();
                let only_g: Vec<&Value> = g.iter().filter(|r| !w.contains(r)).collect();
                out.push(format!(
                    "table {t}: {} rows want, {} rows got",
                    w.len(),
                    g.len()
                ));
                for r in only_w.iter().take(max_rows) {
                    out.push(format!("  - {r}"));
                }
                for r in only_g.iter().take(max_rows) {
                    out.push(format!("  + {r}"));
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_oracle_databases_have_no_diff_and_run_values_are_masked() {
        let db = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/oracle/config-only/index.db");
        let v = Volatile::wrongstack_run();
        let a = dump(&db, &v).unwrap();
        let b = dump(&db, &v).unwrap();
        assert!(diff(&a, &b, 5).is_empty());
        assert_eq!(a["symbols"].len(), 10);
        assert!(a.contains_key("symbols_fts_data"));
        let fts = &dump(&db, &Volatile::logical()).unwrap()["symbols_fts"];
        assert_eq!(fts.len(), 10);
        assert!(
            fts.iter()
                .all(|r| r["rowid"].is_i64() && r["text"].is_string())
        );
        let meta = &a["metadata"];
        let stamp = meta.iter().find(|r| r["key"] == "graph_stamp").unwrap();
        assert_eq!(stamp["value"], "<volatile>");
        assert!(a["files"].iter().all(|r| r["mtime_ms"] == "<volatile>"));
    }

    #[test]
    fn differences_are_reported_per_table() {
        let mut a = Dump::new();
        a.insert("t".into(), vec![json!({"x": 1})]);
        let mut b = a.clone();
        b.insert("t".into(), vec![json!({"x": 2})]);
        b.insert("u".into(), vec![]);
        let d = diff(&a, &b, 5);
        assert!(
            d.iter()
                .any(|l| l.starts_with("table t: 1 rows want, 1 rows got"))
        );
        assert!(d.contains(&"table u: unexpected".to_string()));
    }
}
