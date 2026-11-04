//! SAGE memory and WrongTrace health decoration of CodeMap graph nodes
//! (`decorateGraphNodesWithMemories`, `findAnchoredSageMemories`,
//! `findFileHealthAlerts` in WrongStack's `writer-graph-decorate.ts` and
//! `ecosystem-bridge.ts`).
//!
//! Read-only and best effort, as WrongStack's: a source that cannot be read
//! or parsed contributes nothing. JavaScript semantics are kept where they
//! decide the result (`??`, truthiness, `Number()`/`String()` coercion,
//! `String.prototype.trim`, UTF-16 lengths and slices, a record that would
//! throw is skipped). WrongStack's in-process provider hooks are not
//! registered by any WrongStack host, so they are not modelled.
//!
//! Containment (security review WI-604): the sources live in the checkout's
//! `.wrongstack` directory; a source reached through a link that leaves the
//! checkout is refused, never opened.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Map, Value, json};

use super::QueryError;

/// `limit: 20` in `decorateGraphNodesWithMemories`.
const LIMIT: usize = 20;

// ─── JavaScript semantics ────────────────────────────────────────────────────

/// `Boolean(v)`.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `a ?? b` for an optional property.
fn nullish(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

use crate::paths::js_trim;

/// `s.length` (UTF-16 code units).
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `s.slice(0, n)` in UTF-16 code units (a split pair is replaced).
fn utf16_slice(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
}

/// `Number(v)`.
fn js_number(v: &Value) -> f64 {
    match v {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::String(s) => string_to_number(s),
        Value::Array(a) => match a.as_slice() {
            [] => 0.0,
            [one] => string_to_number(&js_string(one)),
            _ => f64::NAN,
        },
        Value::Object(_) => f64::NAN,
    }
}

/// `StringToNumber`: decimal (with exponent), `0x`/`0o`/`0b`, `Infinity`.
fn string_to_number(s: &str) -> f64 {
    let t = js_trim(s);
    if t.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = t.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return digits.chars().fold(0.0, |acc, c| {
                acc * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            });
        }
    }
    let (sign, body) = match t.as_bytes()[0] {
        b'+' => (1.0, &t[1..]),
        b'-' => (-1.0, &t[1..]),
        _ => (1.0, t),
    };
    if body == "Infinity" {
        return sign * f64::INFINITY;
    }
    // Rust's float grammar is wider (`inf`, `nan`); admit JS's only.
    let ok = !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        && body
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == '.');
    if !ok {
        return f64::NAN;
    }
    body.parse::<f64>().map_or(f64::NAN, |v| sign * v)
}

/// `String(v)` for JSON values.
fn js_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => i.to_string(),
            (None, Some(f)) if f.fract() == 0.0 && f.abs() < 1e21 => format!("{f:.0}"),
            // JS switches to exponent form outside [1e-6, 1e21).
            (None, Some(f)) if f != 0.0 && (f.abs() >= 1e21 || f.abs() < 1e-6) => {
                let e = format!("{f:e}");
                match e.split_once('e') {
                    Some((m, x)) if !x.starts_with('-') => format!("{m}e+{x}"),
                    _ => e,
                }
            }
            (None, Some(f)) => f.to_string(),
            _ => n.to_string(),
        },
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|e| {
                if e.is_null() {
                    String::new()
                } else {
                    js_string(e)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// `SameValueZero` as `Set.has` uses it (objects are never equal here:
/// each parsed record is a fresh object).
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => false,
    }
}

fn seen_has(seen: &[Value], v: &Value) -> bool {
    seen.iter().any(|s| same_value(s, v))
}

/// A JSON number as `JSON.stringify` writes a JS number.
fn num(v: f64) -> Value {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        json!(v as i64)
    } else if v.is_finite() {
        json!(v)
    } else {
        Value::Null
    }
}

// ─── Node path semantics ─────────────────────────────────────────────────────

use crate::paths::{
    index_posix as posix_index_path, node_is_absolute as is_absolute, node_relative as relative,
};

fn resolve(p: &str) -> String {
    wrongindex_storage::wrongstack_paths::node_resolve(Path::new(p))
        .to_string_lossy()
        .into_owned()
}

/// `toRel` of `decorateGraphNodesWithMemories` (and the file normalisation
/// of the two finders).
fn to_rel(project_root: &str, f: &str) -> String {
    let rel = if is_absolute(f) {
        relative(project_root, f)
    } else {
        f.to_string()
    };
    posix_index_path(&rel)
}

// ─── sources ─────────────────────────────────────────────────────────────────

/// A `.wrongstack` source, or `None` when it does not exist. Refused when the
/// path leaves the checkout through a link (checked before anything follows
/// a link).
fn source(project_root: &str, rel: &[&str]) -> Result<Option<PathBuf>, QueryError> {
    let mut p = PathBuf::from(project_root);
    for part in rel {
        p.push(part);
    }
    let inside = |root: &str, real: &Path| {
        let r = resolve(root);
        let r = if cfg!(windows) { r.to_lowercase() } else { r };
        let f = resolve(&real.to_string_lossy());
        let f = if cfg!(windows) { f.to_lowercase() } else { f };
        let sep = std::path::MAIN_SEPARATOR;
        f.starts_with(&format!("{}{sep}", r.trim_end_matches(sep)))
    };
    let real =
        wrongindex_storage::git_containment::resolve_within(Path::new(project_root), &p).ok();
    let real_root =
        wrongindex_storage::git_containment::resolve_links_locally(Path::new(project_root)).ok();
    let ok = real.as_ref().is_some_and(|real| {
        inside(project_root, real)
            || real_root
                .as_ref()
                .is_some_and(|rr| inside(&rr.to_string_lossy(), real))
    });
    if !ok {
        return Err(QueryError::Unsupported(format!(
            "{} leaves the checkout through a link; WrongIndex never reads outside it",
            p.display()
        )));
    }
    Ok(p.is_file().then_some(p))
}

// ─── findAnchoredSageMemories ────────────────────────────────────────────────

/// `AnchoredSageInsight` (the fields the decoration uses).
struct Insight {
    id: Value,
    kind: Value,
    text: String,
    anchor_path: Option<String>,
    anchor_symbol: Option<String>,
}

/// `matchMemoryRecord`; `None` also stands for a record that would throw
/// (its caller skips it either way).
fn match_record(rec: &Value, files: &[String], symbols: &[String]) -> Option<Insight> {
    let Value::Object(o) = rec else {
        // Primitives throw before or inside the match; arrays have no text.
        return None;
    };
    if let Some(status) = o.get("status")
        && truthy(status)
        && status.as_str() != Some("active")
    {
        return None;
    }
    let text = match nullish(o.get("text")).or_else(|| nullish(o.get("content"))) {
        None => String::new(),
        Some(Value::String(s)) => js_trim(s).to_string(),
        Some(_) => return None, // `.trim` of a non-string throws
    };
    if text.is_empty() {
        return None;
    }
    let id = nullish(o.get("id"))
        .cloned()
        .unwrap_or_else(|| json!("sage"));
    let kind = nullish(o.get("kind"))
        .cloned()
        .unwrap_or_else(|| json!("fact"));
    let short = utf16_slice(&text, 280);
    let insight = |anchor_path: Option<String>, anchor_symbol: Option<String>| Insight {
        id: id.clone(),
        kind: kind.clone(),
        text: short.clone(),
        anchor_path,
        anchor_symbol,
    };
    if let Some(Value::Array(anchors)) = o.get("anchors") {
        for anchor in anchors {
            let (path, symbol) = match anchor {
                Value::Null => return None, // `anchor.path` of null throws
                Value::Object(a) => (a.get("path"), a.get("symbol")),
                _ => (None, None),
            };
            let a_path = match path {
                Some(p) if truthy(p) => match p {
                    Value::String(s) => Some(posix_index_path(s)),
                    _ => return None,
                },
                _ => None,
            };
            let a_sym = match nullish(symbol) {
                None => None,
                Some(Value::String(s)) => Some(js_trim(s).to_string()),
                Some(_) => return None,
            };
            if let Some(sym) = a_sym.as_ref().filter(|s| !s.is_empty())
                && symbols.contains(sym)
            {
                return Some(insight(a_path, Some(sym.clone())));
            }
            if let Some(p) = a_path.as_ref().filter(|p| !p.is_empty())
                && files.contains(p)
            {
                let sym = a_sym.filter(|s| !s.is_empty());
                return Some(insight(Some(p.clone()), sym));
            }
        }
    }
    for sym in symbols {
        if utf16_len(sym) >= 4 && text.contains(sym.as_str()) {
            return Some(insight(None, Some(sym.clone())));
        }
    }
    None
}

/// `findAnchoredSageMemories({ projectRoot, files, symbols, limit: 20 })`.
fn find_anchored(
    project_root: &str,
    files: &[String],
    symbols: &[String],
) -> Result<Vec<Insight>, QueryError> {
    let mut results: Vec<Insight> = Vec::new();
    let mut seen: Vec<Value> = Vec::new();
    let mut norm_files: Vec<String> = Vec::new();
    for f in files {
        if f.is_empty() {
            continue;
        }
        let k = to_rel(project_root, f);
        if !norm_files.contains(&k) {
            norm_files.push(k);
        }
    }
    let mut symbol_set: Vec<String> = Vec::new();
    for s in symbols {
        let t = js_trim(s).to_string();
        if !t.is_empty() && !symbol_set.contains(&t) {
            symbol_set.push(t);
        }
    }
    if norm_files.is_empty() && symbol_set.is_empty() {
        return Ok(results);
    }
    if let Some(db) = source(project_root, &[".wrongstack", "memories", "sage.db"])? {
        let rows = (|| -> rusqlite::Result<Vec<[Value; 5]>> {
            let conn = Connection::open_with_flags(
                &db,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
            conn.pragma_update(None, "trusted_schema", false)?;
            conn.pragma_update(None, "busy_timeout", 2000)?;
            let mut st = conn.prepare(
                "SELECT id, data, kind, importance, status
           FROM memories
           WHERE status = 'active'
           ORDER BY importance DESC
           LIMIT 150",
            )?;
            let out = st
                .query_map([], |r| {
                    let v = |i: usize| -> rusqlite::Result<Value> {
                        Ok(match r.get_ref(i)? {
                            rusqlite::types::ValueRef::Null => Value::Null,
                            rusqlite::types::ValueRef::Integer(n) => json!(n),
                            rusqlite::types::ValueRef::Real(f) => num(f),
                            rusqlite::types::ValueRef::Text(t) => {
                                Value::String(String::from_utf8_lossy(t).into_owned())
                            }
                            // `JSON.parse(String(blob))` never yields a record.
                            rusqlite::types::ValueRef::Blob(_) => json!({ "__blob": true }),
                        })
                    };
                    Ok([v(0)?, v(1)?, v(2)?, v(3)?, v(4)?])
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(out)
        })()
        .unwrap_or_default();
        for [id, data, kind, importance, status] in rows {
            if seen_has(&seen, &id) {
                continue;
            }
            let Value::String(data) = data else {
                continue; // `JSON.parse` of a non-string never yields an object
            };
            let Ok(mut parsed) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            match &mut parsed {
                Value::Object(o) => {
                    for (k, v) in [
                        ("id", id),
                        ("kind", kind),
                        ("importance", importance),
                        ("status", status),
                    ] {
                        if o.get(k).is_none_or(Value::is_null) {
                            o.insert(k.into(), v);
                        }
                    }
                }
                Value::Array(_) => {}
                _ => continue, // `parsed.id ??=` on a primitive throws
            }
            if let Some(m) = match_record(&parsed, &norm_files, &symbol_set) {
                seen.push(m.id.clone());
                results.push(m);
                if results.len() >= LIMIT {
                    break;
                }
            }
        }
    }
    if results.len() < LIMIT
        && let Some(jsonl) = source(project_root, &[".wrongstack", "memories", "memories.jsonl"])?
        && let Ok(bytes) = std::fs::read(&jsonl)
    {
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        for line in raw.split('\n') {
            let trimmed = js_trim(line);
            if trimmed.is_empty() {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if parsed.is_null() {
                continue; // `parsed.id` of null throws
            }
            let id = match &parsed {
                Value::Object(o) => nullish(o.get("id")).cloned(),
                _ => None,
            }
            .unwrap_or_else(|| json!(format!("jsonl-{}", results.len())));
            if seen_has(&seen, &id) {
                continue;
            }
            if let Some(m) = match_record(&parsed, &norm_files, &symbol_set) {
                seen.push(m.id.clone());
                results.push(m);
                if results.len() >= LIMIT {
                    break;
                }
            }
        }
    }
    Ok(results)
}

// ─── findFileHealthAlerts ────────────────────────────────────────────────────

struct Health {
    file: String,
    health_score: f64,
    is_fragile: bool,
    recent_thrashing_count: f64,
    lock_owner: Option<String>,
}

fn find_health(project_root: &str, files: &[String]) -> Result<Vec<Health>, QueryError> {
    let mut norm_files: Vec<String> = Vec::new();
    for f in files {
        if f.is_empty() {
            continue;
        }
        let k = to_rel(project_root, f);
        if !norm_files.contains(&k) {
            norm_files.push(k);
        }
    }
    let mut results: Vec<Health> = Vec::new();
    if norm_files.is_empty() {
        return Ok(results);
    }
    let Some(p) = source(project_root, &[".wrongstack", "wrongtrace-health.json"])? else {
        return Ok(results);
    };
    let Ok(bytes) = std::fs::read(&p) else {
        return Ok(results);
    };
    let Ok(raw) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&bytes)) else {
        return Ok(results);
    };
    let list: &[Value] = match &raw {
        Value::Array(a) => a,
        Value::Object(o) => match o.get("files") {
            Some(Value::Array(a)) => a,
            _ => &[],
        },
        _ => &[],
    };
    let mut seen: Vec<String> = Vec::new();
    for entry in list {
        let o = match entry {
            Value::Null => break, // `entry['file_path']` of null throws: the loop ends
            Value::Object(o) => Some(o),
            _ => None,
        };
        let get = |k: &str| o.and_then(|o| nullish(o.get(k)));
        let raw_path = get("file_path")
            .or_else(|| get("file"))
            .or_else(|| get("path"))
            .map(js_string)
            .unwrap_or_default();
        if raw_path.is_empty() {
            continue;
        }
        let key = to_rel(project_root, &raw_path);
        if !norm_files.contains(&key) || seen.contains(&key) {
            continue;
        }
        let health_score = get("health_score")
            .or_else(|| get("healthScore"))
            .map_or(100.0, js_number);
        let is_fragile = get("is_fragile")
            .or_else(|| get("isFragile"))
            .map_or(health_score < 50.0, truthy);
        let recent_thrashing_count = get("recent_thrashing_count")
            .or_else(|| get("recentThrashingCount"))
            .map_or(0.0, js_number);
        let is_locked = get("is_locked")
            .or_else(|| get("isLocked"))
            .is_some_and(truthy);
        let lock_owner = match (
            o.and_then(|o| o.get("lock_owner")),
            o.and_then(|o| o.get("lockOwner")),
        ) {
            (Some(Value::String(s)), _) => Some(s.clone()),
            (_, Some(Value::String(s))) => Some(s.clone()),
            _ => None,
        };
        if is_fragile || recent_thrashing_count > 0.0 || is_locked {
            seen.push(key.clone());
            results.push(Health {
                file: key,
                health_score,
                is_fragile,
                recent_thrashing_count,
                lock_owner,
            });
        }
    }
    Ok(results)
}

// ─── decorateGraphNodesWithMemories ──────────────────────────────────────────

pub(super) fn decorate_with_memories(
    project_root: &str,
    nodes: &mut [Map<String, Value>],
) -> Result<(), QueryError> {
    if project_root.is_empty() || nodes.is_empty() {
        return Ok(());
    }
    let file_of = |n: &Map<String, Value>| -> Option<String> {
        n.get("file")
            .filter(|f| truthy(f))
            .map(|f| to_rel(project_root, &js_string(f)))
    };
    let mut files: Vec<String> = Vec::new();
    let mut symbols: Vec<String> = Vec::new();
    for n in nodes.iter() {
        if let Some(f) = file_of(n) {
            files.push(f);
        }
        if n.get("kind").and_then(Value::as_str) == Some("symbol")
            && let Some(label) = n.get("label").filter(|l| truthy(l))
        {
            symbols.push(js_string(label));
        }
    }
    if files.is_empty() && symbols.is_empty() {
        return Ok(());
    }
    let insights = find_anchored(project_root, &files, &symbols)?;
    let health = find_health(project_root, &files)?;
    if insights.is_empty() && health.is_empty() {
        return Ok(());
    }
    for n in nodes.iter_mut() {
        let kind = n
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if kind == "package" {
            continue;
        }
        let rel_file = file_of(n);
        if let Some(rf) = &rel_file
            && let Some(h) = health.iter().rev().find(|h| &h.file == rf)
        {
            if h.is_fragile {
                n.insert("isFragile".into(), json!(true));
            }
            if h.recent_thrashing_count > 0.0 {
                n.insert("thrashingCount".into(), num(h.recent_thrashing_count));
            }
            n.insert("healthScore".into(), num(h.health_score));
            if let Some(owner) = h.lock_owner.as_ref().filter(|o| !o.is_empty()) {
                n.insert("lockOwner".into(), json!(owner));
            }
        }
        let label = n.get("label").map(js_string);
        let matched: Vec<&Insight> = insights
            .iter()
            .filter(|m| {
                if kind == "symbol" {
                    match (&m.anchor_symbol, &label) {
                        (Some(a), Some(l)) if !a.is_empty() && a == l => {
                            m.anchor_path.as_ref().is_none_or(|p| p.is_empty())
                                || rel_file.as_ref().is_none_or(|r| r.is_empty())
                                || m.anchor_path.as_deref().map(posix_index_path) == rel_file
                        }
                        _ => false,
                    }
                } else {
                    rel_file.as_ref().is_some_and(|r| !r.is_empty())
                        && m.anchor_path.as_ref().is_some_and(|p| !p.is_empty())
                        && m.anchor_path.as_deref().map(posix_index_path) == rel_file
                }
            })
            .collect();
        if !matched.is_empty() {
            n.insert("memoryCount".into(), json!(matched.len()));
            n.insert(
                "memories".into(),
                Value::Array(
                    matched
                        .iter()
                        .map(|m| json!({"id": m.id, "kind": m.kind, "text": m.text}))
                        .collect(),
                ),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn javascript_coercions() {
        assert_eq!(js_number(&json!("42")), 42.0);
        assert_eq!(js_number(&json!(" 0x10 ")), 16.0);
        assert!(js_number(&json!("abc")).is_nan());
        assert_eq!(js_number(&json!("")), 0.0);
        assert_eq!(js_number(&json!(true)), 1.0);
        assert!(js_number(&json!({})).is_nan());
        assert_eq!(js_string(&json!(12)), "12");
        assert_eq!(js_string(&json!([1, null, "a"])), "1,,a");
        assert_eq!(js_string(&json!(1e21)), "1e+21");
        assert_eq!(js_string(&json!(1.5)), "1.5");
        assert_eq!(js_string(&json!(1e-7)), "1e-7");
        assert!(js_number(&json!("1_0")).is_nan());
        assert!(js_number(&json!("-0x10")).is_nan());
        assert!(js_number(&json!(".")).is_nan());
        assert!(js_number(&json!("5e")).is_nan());
        assert_eq!(js_number(&json!("+.5")), 0.5);
        assert_eq!(js_number(&json!("1.")), 1.0);
        assert_eq!(js_number(&json!("Infinity")), f64::INFINITY);
        assert_eq!(js_trim("\u{feff} x \u{a0}"), "x");
        assert_eq!(js_trim("\u{85}x"), "\u{85}x");
        assert_eq!(utf16_slice("a😀b", 2), "a\u{fffd}");
        assert!(truthy(&json!("0")) && !truthy(&json!(0)));
    }

    #[cfg(windows)]
    #[test]
    fn node_win32_relative() {
        assert_eq!(relative(r"C:\p", r"C:\p\src\a.ts"), r"src\a.ts");
        assert_eq!(relative(r"C:\p", r"c:\P\src\a.ts"), r"src\a.ts");
        assert_eq!(relative(r"C:\p\q", r"C:\p\r\a.ts"), r"..\r\a.ts");
        assert_eq!(relative(r"C:\p", r"D:\x\a.ts"), r"D:\x\a.ts");
        assert_eq!(relative(r"C:\p", r"C:\p"), "");
        assert_eq!(relative(r"C:\", r"C:\a"), "a");
        assert_eq!(relative(r"C:\p", r"C:\pq\a"), r"..\pq\a");
        assert_eq!(relative(r"C:\p\", r"C:\p\x"), "x");
        assert_eq!(relative(r"C:\a\b", r"C:\"), r"..\..");
        assert_eq!(to_rel(r"C:\p", r"C:\p\src\a.ts"), "src/a.ts");
        assert_eq!(to_rel(r"C:\p", "./src/a.ts"), "src/a.ts");
    }
}
