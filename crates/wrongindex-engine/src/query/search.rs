//! `search`, `context` and `vectorSearch` (ports of `writer-search.ts`,
//! `writer-search-helpers.ts`, `bm25.ts`, `context-retrieval.ts`,
//! `graph-rank.ts`, `graph-rank-pass.ts`, `writer-rank.ts` and
//! `writer-vectors.ts`).
//!
//! Every statement whose row order is decided by the query plan (no
//! `ORDER BY`) is issued with the same SQL text WrongStack uses, so SQLite
//! walks the same plan and returns rows in the same order.
//!
//! Not reproduced (no recorded fixture exercises them, and a read-only
//! reader cannot): the symbol-vector fusion paths (`vectorsAvailable` is
//! only true when WrongStack's embedding feature flag is on) and the FTS
//! rebuild WrongStack performs at open time when `symbols_fts` is missing or
//! out of sync. A missing `symbols_fts` table takes the BM25 fallback, which
//! is what WrongStack does when FTS5 is unavailable.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, Row, params_from_iter};
use serde_json::{Map, Value, json};
use unicode_general_category::{GeneralCategory, get_general_category};

use super::{QueryError, Reader, js_number};
use crate::js::{self, utf16};
use crate::rows::{MODULE_OWNER_NAME, indexable_text};

// ─── shared helpers ─────────────────────────────────────────────────────────

/// A JS number as `JSON.stringify` + `JSON.parse` would round-trip it:
/// integral values become integers, non-finite values `null`.
fn num(v: f64) -> Value {
    if !v.is_finite() {
        Value::Null
    } else if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        json!(v as i64)
    } else {
        json!(v)
    }
}

fn sql_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null | ValueRef::Blob(_) => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => num(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
    }
}

fn col_json(row: &Row<'_>, i: usize) -> rusqlite::Result<Value> {
    Ok(sql_json(row.get_ref(i)?))
}

fn col_string(row: &Row<'_>, i: usize) -> rusqlite::Result<String> {
    Ok(match row.get_ref(i)? {
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Integer(n) => n.to_string(),
        ValueRef::Real(f) => f.to_string(),
        _ => String::new(),
    })
}

fn col_i64(row: &Row<'_>, i: usize) -> rusqlite::Result<i64> {
    Ok(match row.get_ref(i)? {
        ValueRef::Integer(n) => n,
        ValueRef::Real(f) => f as i64,
        _ => 0,
    })
}

fn col_f64(row: &Row<'_>, i: usize) -> rusqlite::Result<f64> {
    Ok(match row.get_ref(i)? {
        ValueRef::Integer(n) => n as f64,
        ValueRef::Real(f) => f,
        _ => f64::NAN,
    })
}

/// `\p{N}`: Nd, Nl, No.
fn is_number_char(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::DecimalNumber
            | GeneralCategory::LetterNumber
            | GeneralCategory::OtherNumber
    )
}

fn u16len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `s.trim()` (ECMAScript whitespace).
fn js_trim(s: &str) -> String {
    js::string(js::trim(&utf16(s)))
}

/// `s.split(/\s+/)` (ECMAScript whitespace), keeping empty edge pieces.
fn js_split_ws(s: &str) -> Vec<String> {
    let u = utf16(s);
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < u.len() {
        if js::is_space(u[i]) {
            out.push(js::string(&u[start..i]));
            while i < u.len() && js::is_space(u[i]) {
                i += 1;
            }
            start = i;
        } else {
            i += 1;
        }
    }
    out.push(js::string(&u[start..]));
    out
}

/// `tokenise` (`bm25.ts`): `text.replace(/[^\p{L}\p{N}$']/gu, ' ')
/// .toLowerCase().trim().split(/\s+/)`.
fn tokenise(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let replaced: String = text
        .chars()
        .map(|c| {
            if crate::unicode::is_letter(c) || is_number_char(c) || c == '$' || c == '\'' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let cleaned = js_trim(&replaced.to_lowercase());
    if cleaned.is_empty() {
        return Vec::new();
    }
    js_split_ws(&cleaned)
}

/// `escapeLike` (`writer-helpers.ts`).
fn escape_like(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for c in v.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `localeCompare` approximation (ICU root collation is not available
/// without a new dependency). Primary: punctuation/symbols < digits <
/// letters, letters compared case-insensitively; tertiary: lowercase before
/// uppercase; last resort: UTF-16 code-unit order. No recorded fixture
/// depends on these ties.
fn locale_compare(a: &str, b: &str) -> Ordering {
    fn key(c: char) -> (u8, Vec<char>) {
        if c.is_numeric() {
            (1, vec![c])
        } else if c.is_alphabetic() {
            (2, c.to_lowercase().collect())
        } else {
            (0, vec![c])
        }
    }
    let ka: Vec<_> = a.chars().map(key).collect();
    let kb: Vec<_> = b.chars().map(key).collect();
    ka.cmp(&kb)
        .then_with(|| {
            for (x, y) in a.chars().zip(b.chars()) {
                if x != y && x.to_lowercase().eq(y.to_lowercase()) {
                    return if x.is_lowercase() {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    };
                }
            }
            Ordering::Equal
        })
        .then_with(|| utf16(a).cmp(&utf16(b)))
}

fn f64_desc(a: f64, b: f64) -> Ordering {
    // `b - a` as a comparator: NaN compares equal.
    b.partial_cmp(&a).unwrap_or(Ordering::Equal)
}

// ─── search filters (writer-search-helpers.ts) ──────────────────────────────

/// `LSP_TO_INTERNAL_KINDS` (`lsp-kind.ts`).
fn lsp_kinds(k: i64) -> &'static [&'static str] {
    match k {
        2 => &["mod"],
        3 => &["namespace"],
        5 => &["class"],
        6 => &["method"],
        7 | 8 => &["property"],
        10 => &["enum"],
        11 => &["interface", "trait"],
        12 => &["function"],
        13 => &["var", "let"],
        14 => &["const", "static"],
        19 => &["object"],
        23 => &["struct"],
        26 => &["type"],
        _ => &[],
    }
}

#[derive(Default)]
struct Filter {
    kind: Option<String>,
    lang: Option<String>,
    file: Option<String>,
    /// `filter.lspKind` when non-null (`!= null`).
    lsp_kind: Option<Value>,
}

impl Filter {
    fn from_args(args: &Value) -> Self {
        let text = |k: &str| {
            args.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Filter {
            kind: text("kind"),
            lang: text("lang"),
            file: text("file"),
            lsp_kind: args.get("lspKind").filter(|v| !v.is_null()).cloned(),
        }
    }
}

enum Kinds {
    Any,
    Nothing,
    Only(Vec<String>),
}

/// `resolveKindFilter`.
fn resolve_kinds(f: &Filter) -> Kinds {
    let lsp: Option<Vec<String>> = f.lsp_kind.as_ref().map(|v| {
        // A JS object property lookup: numbers and canonical integer strings.
        let k = match v {
            Value::Number(n) => n.as_f64().filter(|x| x.fract() == 0.0).map(|x| x as i64),
            Value::String(s) => s.parse::<i64>().ok().filter(|n| n.to_string() == *s),
            _ => None,
        };
        k.map_or(&[][..], lsp_kinds)
            .iter()
            .map(|s| s.to_string())
            .collect()
    });
    if lsp.as_ref().is_some_and(Vec::is_empty) {
        return Kinds::Nothing;
    }
    if let Some(kind) = &f.kind {
        if lsp.as_ref().is_some_and(|l| !l.contains(kind)) {
            return Kinds::Nothing;
        }
        return Kinds::Only(vec![kind.clone()]);
    }
    match lsp {
        Some(l) => Kinds::Only(l),
        None => Kinds::Any,
    }
}

/// `buildWriterSearchWhere`; `None` when the filter matches nothing.
fn build_where(query: &str, f: &Filter) -> Option<(String, Vec<SqlValue>)> {
    let kinds = match resolve_kinds(f) {
        Kinds::Nothing => return None,
        k => k,
    };
    let mut cond = vec![format!(
        "NOT (kind = 'mod' AND name = '{MODULE_OWNER_NAME}')"
    )];
    let mut vals = Vec::new();
    if let Kinds::Only(ks) = &kinds {
        cond.push(format!("kind IN ({})", vec!["?"; ks.len()].join(", ")));
        vals.extend(ks.iter().cloned().map(SqlValue::Text));
    }
    if let Some(lang) = &f.lang {
        cond.push("lang = ?".into());
        vals.push(SqlValue::Text(lang.clone()));
    }
    if let Some(file) = &f.file {
        cond.push("replace(file, '\\', '/') LIKE ? ESCAPE '\\'".into());
        vals.push(SqlValue::Text(format!(
            "%{}%",
            escape_like(&file.replace('\\', "/"))
        )));
    }
    if !js_trim(query).is_empty() {
        let tokens: Vec<String> = js_split_ws(&query.to_lowercase())
            .into_iter()
            .filter(|t| !t.is_empty())
            .collect();
        let one = "(name LIKE ? ESCAPE '\\' OR signature LIKE ? ESCAPE '\\' OR doc_comment LIKE ? ESCAPE '\\')";
        cond.push(format!("({})", vec![one; tokens.len()].join(" OR ")));
        for t in &tokens {
            let like = format!("%{}%", escape_like(t));
            for _ in 0..3 {
                vals.push(SqlValue::Text(like.clone()));
            }
        }
    }
    Some((format!("WHERE {}", cond.join(" AND ")), vals))
}

// ─── search results ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct Hit {
    id: i64,
    lang: Value,
    kind: Value,
    name: Value,
    file: Value,
    line: Value,
    col: Value,
    signature: Value,
    doc_comment: Value,
    score: f64,
    snippet: String,
    lsp_kind: Option<Value>,
}

impl Hit {
    /// Columns `id, lang, kind, name, file, line, col, signature, doc_comment`.
    fn from_row(row: &Row<'_>, lsp_kind: &Option<Value>) -> rusqlite::Result<Self> {
        Ok(Hit {
            id: col_i64(row, 0)?,
            lang: col_json(row, 1)?,
            kind: col_json(row, 2)?,
            name: col_json(row, 3)?,
            file: col_json(row, 4)?,
            line: col_json(row, 5)?,
            col: col_json(row, 6)?,
            signature: col_json(row, 7)?,
            doc_comment: col_json(row, 8)?,
            score: 0.0,
            snippet: String::new(),
            lsp_kind: lsp_kind.clone(),
        })
    }

    fn name_str(&self) -> &str {
        self.name.as_str().unwrap_or("")
    }

    fn file_str(&self) -> &str {
        self.file.as_str().unwrap_or("")
    }

    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), json!(self.id));
        m.insert("lang".into(), self.lang.clone());
        m.insert("kind".into(), self.kind.clone());
        m.insert("name".into(), self.name.clone());
        m.insert("file".into(), self.file.clone());
        m.insert("line".into(), self.line.clone());
        m.insert("col".into(), self.col.clone());
        m.insert("signature".into(), self.signature.clone());
        m.insert("docComment".into(), self.doc_comment.clone());
        m.insert("score".into(), num(self.score));
        m.insert("snippet".into(), json!(self.snippet));
        if let Some(k) = &self.lsp_kind {
            m.insert("lspKind".into(), k.clone());
        }
        Value::Object(m)
    }
}

const SEARCH_CANDIDATE_SCAN_CAP: i64 = 5000;

/// `searchWithStatement`.
fn plain_search(
    c: &Connection,
    query: &str,
    f: &Filter,
    limit: Option<i64>,
) -> Result<Vec<Hit>, QueryError> {
    let Some((where_, mut vals)) = build_where(query, f) else {
        return Ok(Vec::new());
    };
    let limit_sql = if limit.is_some() { " LIMIT ?" } else { "" };
    let sql = format!(
        "SELECT id, lang, kind, name, file, line, col, signature, doc_comment FROM symbols {where_}{limit_sql}"
    );
    if let Some(l) = limit {
        vals.push(SqlValue::Integer(l));
    }
    let mut stmt = c.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(vals), |r| Hit::from_row(r, &f.lsp_kind))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// `countSearchWithStatement`.
fn count_search(c: &Connection, query: &str, f: &Filter) -> Result<i64, QueryError> {
    let Some((where_, vals)) = build_where(query, f) else {
        return Ok(0);
    };
    Ok(c.query_row(
        &format!("SELECT COUNT(*) AS n FROM symbols {where_}"),
        params_from_iter(vals),
        |r| col_i64(r, 0),
    )?)
}

fn fts_available(c: &Connection) -> Result<bool, QueryError> {
    let n: i64 = c.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='symbols_fts'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// `searchRankedWithStatement`; `limit` is the caller's raw number.
fn search_ranked(
    c: &Connection,
    query: &str,
    f: &Filter,
    limit: Option<f64>,
) -> Result<(Vec<Hit>, i64), QueryError> {
    let raw = match limit {
        Some(l) if l.is_finite() => l.trunc(),
        _ => 20.0,
    };
    let safe = raw.clamp(1.0, 100.0) as i64;
    let tokens = tokenise(query);
    if tokens.is_empty() || !fts_available(c)? {
        return bm25_fallback(c, query, f, safe);
    }
    let kinds = match resolve_kinds(f) {
        Kinds::Nothing => return Ok((Vec::new(), 0)),
        k => k,
    };
    let (long, short): (Vec<&String>, Vec<&String>) = tokens.iter().partition(|t| u16len(t) >= 3);
    if long.is_empty() {
        return bm25_fallback(c, query, f, safe);
    }
    let matcher = long
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut cond = vec!["symbols_fts MATCH ?".to_string()];
    let mut vals = vec![SqlValue::Text(matcher)];
    for t in &short {
        let like = format!("%{}%", escape_like(t));
        cond.push(
            "(s.name LIKE ? ESCAPE '\\' OR s.signature LIKE ? ESCAPE '\\' OR s.doc_comment LIKE ? ESCAPE '\\')"
                .into(),
        );
        for _ in 0..3 {
            vals.push(SqlValue::Text(like.clone()));
        }
    }
    if let Kinds::Only(ks) = &kinds {
        cond.push(format!("s.kind IN ({})", vec!["?"; ks.len()].join(", ")));
        vals.extend(ks.iter().cloned().map(SqlValue::Text));
    }
    if let Some(lang) = &f.lang {
        cond.push("s.lang = ?".into());
        vals.push(SqlValue::Text(lang.clone()));
    }
    if let Some(file) = &f.file {
        cond.push("replace(s.file, '\\', '/') LIKE ? ESCAPE '\\'".into());
        vals.push(SqlValue::Text(format!(
            "%{}%",
            escape_like(&file.replace('\\', "/"))
        )));
    }
    let w = cond.join(" AND ");
    let sql = format!(
        "SELECT s.id, s.lang, s.kind, s.name, s.file, s.line, s.col, s.signature, s.doc_comment,
            -bm25(symbols_fts) AS score,
            snippet(symbols_fts, 0, '', '', '\u{2026}', 12) AS snippet,
            -- Keep this uncorrelated: referencing outer columns turns it into
            -- a per-row subquery and defeats the one-count-per-statement win.
            (SELECT COUNT(*) FROM symbols_fts JOIN symbols s ON s.id = symbols_fts.rowid
              WHERE {w}) AS total_count
     FROM symbols_fts JOIN symbols s ON s.id = symbols_fts.rowid
     WHERE {w}
     ORDER BY
       CASE WHEN lower(s.name) = lower(?) THEN 0
            WHEN lower(s.name) LIKE lower(?) ESCAPE '\\' THEN 1
            ELSE 2 END,
       bm25(symbols_fts), lower(s.name), s.file, s.line, s.col, s.id
     LIMIT ?"
    );
    let q = js_trim(query);
    let mut binds = vals.clone();
    binds.extend(vals);
    binds.push(SqlValue::Text(q.clone()));
    binds.push(SqlValue::Text(format!("{}%", escape_like(&q))));
    binds.push(SqlValue::Integer(safe));
    let mut stmt = c.prepare(&sql)?;
    let mut total = 0i64;
    let mut first = true;
    let mut out = Vec::new();
    let mut rows = stmt.query(params_from_iter(binds))?;
    while let Some(r) = rows.next()? {
        if first {
            total = col_i64(r, 11)?;
            first = false;
        }
        let mut h = Hit::from_row(r, &f.lsp_kind)?;
        h.score = 0.0001f64.max(col_f64(r, 9)?);
        h.snippet = col_string(r, 10)?;
        out.push(h);
    }
    Ok((out, total))
}

/// One `Bm25Doc`.
struct Bm25Doc {
    id: i64,
    tokens: Vec<String>,
    raw: String,
}

/// `searchRankedFallbackWithStatement` with an in-process `Bm25Index`.
fn bm25_fallback(
    c: &Connection,
    query: &str,
    f: &Filter,
    limit: i64,
) -> Result<(Vec<Hit>, i64), QueryError> {
    let total = count_search(c, query, f)?;
    if js_trim(query).is_empty() {
        if total == 0 {
            return Ok((Vec::new(), 0));
        }
        return Ok((plain_search(c, query, f, Some(limit))?, total));
    }
    if total == 0 {
        return Ok((Vec::new(), 0));
    }
    let candidates = plain_search(c, query, f, Some(SEARCH_CANDIDATE_SCAN_CAP))?;
    if candidates.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let by_id: HashMap<i64, Hit> = candidates.into_iter().map(|h| (h.id, h)).collect();

    // `getAllIndexable` + `buildBm25Index`.
    let mut docs = Vec::new();
    {
        let mut stmt = c.prepare("SELECT id, name, signature, doc_comment FROM symbols")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let raw = indexable_text(&col_string(r, 1)?, &col_string(r, 2)?, &col_string(r, 3)?);
            docs.push(Bm25Doc {
                id: col_i64(r, 0)?,
                tokens: tokenise(&raw),
                raw,
            });
        }
    }
    let n = docs.len() as f64;
    let total_len: usize = docs.iter().map(|d| d.tokens.len()).sum();
    let avg = if docs.is_empty() {
        0.0
    } else {
        total_len as f64 / n
    };
    let safe_avg = if avg == 0.0 { 1.0 } else { avg };
    const K1: f64 = 1.5;
    const B: f64 = 0.75;

    // `Bm25Index.score`.
    let q_tokens = tokenise(query);
    let df: Vec<usize> = q_tokens
        .iter()
        .map(|q| {
            docs.iter()
                .filter(|d| d.tokens.iter().any(|t| t.starts_with(q.as_str())))
                .count()
        })
        .collect();
    let mut scored: Vec<(i64, f64)> = Vec::new();
    for d in &docs {
        if !by_id.contains_key(&d.id) {
            continue;
        }
        let mut s = 0.0f64;
        for (qi, q) in q_tokens.iter().enumerate() {
            let tf = d
                .tokens
                .iter()
                .filter(|t| t.starts_with(q.as_str()))
                .count();
            if tf == 0 || df[qi] == 0 {
                continue;
            }
            let dfv = df[qi] as f64;
            let tf = tf as f64;
            let idf = ((n - dfv + 0.5) / (dfv + 0.5) + 1.0).ln();
            let len_ratio = B * (d.tokens.len() as f64 / safe_avg);
            let tfc = (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + len_ratio));
            s += idf * tfc;
        }
        if s > 0.0 {
            scored.push((d.id, s));
        }
    }

    let q = js_trim(query).to_lowercase();
    let rank = |id: i64| -> u8 {
        let name = by_id[&id].name_str().to_lowercase();
        if name == q {
            0
        } else if name.starts_with(q.as_str()) {
            1
        } else {
            2
        }
    };
    scored.sort_by(|a, b| {
        rank(a.0)
            .cmp(&rank(b.0))
            .then_with(|| f64_desc(a.1, b.1))
            .then_with(|| {
                let (l, r) = (&by_id[&a.0], &by_id[&b.0]);
                locale_compare(l.name_str(), r.name_str())
                    .then_with(|| locale_compare(l.file_str(), r.file_str()))
                    .then_with(|| l.line.as_i64().cmp(&r.line.as_i64()))
                    .then_with(|| l.col.as_i64().cmp(&r.col.as_i64()))
                    .then_with(|| l.id.cmp(&r.id))
            })
    });
    let raw_by_id: HashMap<i64, &str> = docs.iter().map(|d| (d.id, d.raw.as_str())).collect();
    let results = scored
        .iter()
        .take(limit.max(0) as usize)
        .map(|&(id, score)| {
            let mut h = by_id[&id].clone();
            h.score = score;
            h.snippet = raw_by_id
                .get(&id)
                .map_or(String::new(), |raw| extract_snippet(raw, &q_tokens, 40));
            h
        })
        .collect();
    Ok((results, total))
}

/// `rawRangeOfLowered` over UTF-16 units.
fn raw_range_of_lowered(raw: &str, low_start: usize, low_end: usize) -> (usize, usize) {
    let (mut start, mut low, mut i) = (0usize, 0usize, 0usize);
    for ch in raw.chars() {
        if low <= low_start {
            start = i;
        }
        if low >= low_end {
            return (start, i);
        }
        low += ch.to_lowercase().map(char::len_utf16).sum::<usize>();
        i += ch.len_utf16();
    }
    (start, u16len(raw))
}

/// `Bm25Index.extractSnippet`.
fn extract_snippet(raw: &str, tokens: &[String], radius: usize) -> String {
    let raw16 = utf16(raw);
    let lower16 = utf16(&raw.to_lowercase());
    for tok in tokens {
        let t16 = utf16(tok);
        if let Some(low_idx) = js::index_of(&lower16, &t16, 0) {
            let (idx, match_end) = if lower16.len() == raw16.len() {
                (low_idx, low_idx + t16.len())
            } else {
                raw_range_of_lowered(raw, low_idx, low_idx + t16.len())
            };
            let start = idx.saturating_sub(radius);
            let end = raw16.len().min(match_end + radius);
            let excerpt = js::string(&raw16[start..end.max(start)]);
            return format!(
                "{}{}{}",
                if start > 0 { "\u{2026}" } else { "" },
                excerpt,
                if end < raw16.len() { "\u{2026}" } else { "" }
            );
        }
    }
    let cut = raw16.len().min(radius * 2);
    format!(
        "{}{}",
        js::string(&raw16[..cut]),
        if raw16.len() > radius * 2 {
            "\u{2026}"
        } else {
            ""
        }
    )
}

fn index_summary(c: &Connection) -> Result<(i64, Value), QueryError> {
    let files: i64 = c.query_row("SELECT COUNT(*) AS n FROM files", [], |r| col_i64(r, 0))?;
    let mut stmt = c.prepare("SELECT value FROM metadata WHERE key = 'last_indexed'")?;
    let mut rows = stmt.query([])?;
    let last = match rows.next()? {
        Some(r) => match r.get_ref(0)? {
            ValueRef::Text(t) => js_number(&String::from_utf8_lossy(t)),
            ValueRef::Integer(n) => json!(n),
            ValueRef::Real(f) => num(f),
            // `Number(null)` is 0; `Number(undefined)` is NaN.
            ValueRef::Null => json!(0),
            ValueRef::Blob(_) => Value::Null,
        },
        None => Value::Null,
    };
    Ok((files, last))
}

fn query_arg(args: &Value) -> Result<&str, QueryError> {
    args.get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| QueryError::InvalidArgs("query must be a string".into()))
}

/// `searchService`.
pub fn search(r: &Reader, args: &Value) -> Result<Value, QueryError> {
    let c = &r.conn;
    let query = query_arg(args)?;
    let filter = Filter::from_args(args);
    let limit = args.get("limit").and_then(Value::as_f64);
    let (results, total) = search_ranked(c, query, &filter, limit)?;
    let mut out = json!({
        "results": results.iter().map(Hit::to_json).collect::<Vec<_>>(),
        "total": total,
    });
    if total == 0 {
        let (files, last) = index_summary(c)?;
        out["indexSummary"] = json!({ "totalFiles": files, "lastIndexed": last });
    }
    Ok(out)
}

// ─── wiring graph (graph-rank.ts, graph-rank-pass.ts, writer-rank.ts) ───────

/// `detectLang` (`languages.ts`).
fn detect_lang(file: &str) -> Option<&'static str> {
    let base = match file.rfind(['/', '\\']) {
        Some(i) => &file[i + 1..],
        None => file,
    };
    let lb = base.to_lowercase();
    if lb.ends_with(".d.ts") || lb.ends_with(".d.mts") || lb.ends_with(".d.cts") {
        return Some("ts");
    }
    let special = match lb.as_str() {
        "makefile" | "gnumakefile" | "dockerfile" | "cmakelists.txt" | "procfile" | "justfile" => {
            Some("other")
        }
        "docker-compose.yml" | "docker-compose.yaml" => Some("yaml"),
        "gemfile" | "rakefile" => Some("ruby"),
        _ => None,
    };
    if special.is_some() {
        return special;
    }
    let dot = lb.rfind('.')?;
    if dot == 0 {
        return None;
    }
    Some(match &lb[dot..] {
        ".ts" | ".mts" | ".cts" => "ts",
        ".tsx" => "tsx",
        ".js" | ".mjs" | ".cjs" => "js",
        ".jsx" => "jsx",
        ".go" => "go",
        ".py" | ".pyi" | ".pyw" => "py",
        ".rs" => "rs",
        ".json" | ".jsonc" => "json",
        ".yaml" | ".yml" => "yaml",
        ".c" | ".h" => "c",
        ".cc" | ".cpp" | ".cxx" | ".hh" | ".hpp" | ".hxx" => "cpp",
        ".java" => "java",
        ".cs" => "csharp",
        ".kt" | ".kts" => "kotlin",
        ".scala" | ".sc" => "scala",
        ".php" => "php",
        ".rb" => "ruby",
        ".swift" => "swift",
        ".dart" => "dart",
        ".lua" => "lua",
        ".r" => "r",
        ".pl" | ".pm" | ".tf" | ".tfvars" => "other",
        ".zig" => "zig",
        ".ex" | ".exs" => "elixir",
        ".hs" | ".lhs" => "haskell",
        ".sh" | ".bash" | ".zsh" | ".ps1" => "shell",
        ".sql" => "sql",
        ".md" | ".mdx" => "md",
        ".toml" => "toml",
        ".html" | ".htm" => "html",
        ".css" | ".scss" | ".less" => "css",
        ".vue" => "vue",
        ".svelte" => "svelte",
        ".proto" => "proto",
        ".graphql" | ".gql" => "graphql",
        _ => return None,
    })
}

/// `LANG_FAMILY` (`languages.ts`).
fn lang_family(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "ts" | "tsx" | "js" | "jsx" | "vue" | "svelte" => "js",
        "go" => "go",
        "py" => "py",
        "rs" => "rs",
        "java" | "kotlin" | "scala" => "jvm",
        "csharp" => "dotnet",
        "c" | "cpp" => "c",
        "ruby" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "dart" => "dart",
        "elixir" => "elixir",
        "haskell" => "haskell",
        "zig" => "zig",
        "lua" => "lua",
        "r" => "r",
        "shell" => "shell",
        "sql" => "sql",
        "json" | "yaml" | "toml" => "data",
        "html" | "css" => "web",
        "proto" => "proto",
        "graphql" => "graphql",
        "md" | "other" => "other",
        _ => return None,
    })
}

/// `FAMILY_BY_LANG` (`writer-rank.ts`): the table plus the `''` wildcard.
fn family_by_lang(lang: &str) -> Option<&'static str> {
    if lang.is_empty() {
        Some("*")
    } else {
        lang_family(lang)
    }
}

/// `familyOf` (`graph-rank-pass.ts`).
fn family_of(file: &str) -> Option<&'static str> {
    detect_lang(file).map(|l| lang_family(l).unwrap_or("other"))
}

fn is_sep(c: char) -> bool {
    c == '\\' || c == '/'
}

/// Length of a win32 path's root (`C:\`, `C:`, `\`, `\\server\share\`).
fn win32_root_len(p: &str) -> usize {
    let b: Vec<char> = p.chars().collect();
    if b.len() >= 2 && b[1] == ':' && b[0].is_ascii_alphabetic() {
        return if b.len() >= 3 && is_sep(b[2]) { 3 } else { 2 };
    }
    if b.len() >= 2 && is_sep(b[0]) && is_sep(b[1]) {
        // UNC: \\server\share\
        let mut i = 2;
        let mut parts = 0;
        while i < b.len() && parts < 2 {
            while i < b.len() && !is_sep(b[i]) {
                i += 1;
            }
            parts += 1;
            if i < b.len() {
                i += 1;
            }
        }
        return b[..i].iter().map(|c| c.len_utf8()).sum();
    }
    if !b.is_empty() && is_sep(b[0]) {
        return 1;
    }
    0
}

/// Node's `path.win32.dirname`.
fn win32_dirname(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let root = win32_root_len(p);
    let rest = &p[root..];
    let trimmed = rest.trim_end_matches(is_sep);
    match trimmed.rfind(is_sep) {
        Some(i) => {
            let head = trimmed[..i].trim_end_matches(is_sep);
            if head.is_empty() {
                p[..root].to_string()
            } else {
                format!("{}{}", &p[..root], head)
            }
        }
        None if root > 0 => p[..root].to_string(),
        None => ".".into(),
    }
}

struct Visibility {
    families: HashMap<String, Option<&'static str>>,
    dirs: HashMap<String, String>,
    imported_dirs: HashMap<String, HashSet<String>>,
}

impl Visibility {
    fn family(&mut self, f: &str) -> Option<&'static str> {
        *self
            .families
            .entry(f.to_string())
            .or_insert_with(|| family_of(f))
    }

    fn dir(&mut self, f: &str) -> String {
        self.dirs
            .entry(f.to_string())
            .or_insert_with(|| win32_dirname(f))
            .clone()
    }

    /// `createImplicitVisibility()(source, target, imports)`.
    fn visible(&mut self, src: &str, tgt: &str, imports: Option<&HashSet<String>>) -> bool {
        let Some(tf) = self.family(tgt) else {
            return false;
        };
        let td = self.dir(tgt);
        if matches!(tf, "go" | "jvm" | "dotnet" | "swift")
            && self.dir(src) == td
            && self.family(src) == Some(tf)
        {
            return true;
        }
        let Some(imports) = imports else {
            return false;
        };
        if !matches!(tf, "go" | "jvm") {
            return false;
        }
        if !self.imported_dirs.contains_key(src) {
            let set: HashSet<String> = imports.iter().map(|f| self.dir(f)).collect();
            self.imported_dirs.insert(src.to_string(), set);
        }
        self.imported_dirs[src].contains(&td)
    }
}

/// `WiringGraph` in CSR form.
struct Graph {
    size: usize,
    ids: Vec<i64>,
    index: HashMap<i64, usize>,
    offsets: Vec<usize>,
    neighbours: Vec<usize>,
    weights: Vec<f64>,
}

/// `loadWiringGraph`.
fn load_graph(c: &Connection) -> Result<(Graph, HashMap<i64, String>), QueryError> {
    // getSymbolGraphFactsWithStatement
    let mut file_of: HashMap<i64, String> = HashMap::new();
    let mut groups: HashMap<(&'static str, String), usize> = HashMap::new();
    let mut members: Vec<(i64, (&'static str, String))> = Vec::new();
    let mut module_of: HashMap<String, i64> = HashMap::new();
    let mut declaring: HashSet<String> = HashSet::new();
    {
        let mut stmt = c.prepare("SELECT id, file, name, lang FROM symbols")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let id = col_i64(r, 0)?;
            let file = col_string(r, 1)?;
            let name = col_string(r, 2)?;
            let lang = col_string(r, 3)?;
            if name == MODULE_OWNER_NAME {
                module_of.insert(file.clone(), id);
            } else {
                declaring.insert(file.clone());
            }
            file_of.insert(id, file);
            let Some(fam) = family_by_lang(&lang) else {
                continue;
            };
            let key = (fam, name);
            *groups.entry(key.clone()).or_default() += 1;
            members.push((id, key));
        }
    }
    let mut candidates: HashMap<i64, usize> = HashMap::new();
    for (id, key) in &members {
        let n = groups[key];
        if n >= 2 {
            candidates.insert(*id, n);
        }
    }
    let shadowed: HashSet<i64> = module_of
        .iter()
        .filter(|(f, _)| declaring.contains(*f))
        .map(|(_, &id)| id)
        .collect();

    // getAllResolvedRefsWithStatement
    let mut refs: Vec<(i64, i64, String)> = Vec::new();
    {
        let mut stmt =
            c.prepare("SELECT from_id, to_id, call_type FROM refs WHERE to_id IS NOT NULL")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let from = col_i64(r, 0)?;
            if shadowed.contains(&from) {
                continue;
            }
            refs.push((from, col_i64(r, 1)?, col_string(r, 2)?));
        }
    }

    // getImportVisibilityWithStatement
    let mut imports: HashMap<String, HashSet<String>> = HashMap::new();
    {
        let mut stmt = c.prepare(
            "
    SELECT DISTINCT s.file AS src, r.to_file AS dst
    FROM refs r
    JOIN symbols s ON s.id = r.from_id
    WHERE r.call_type = 'import' AND r.to_file IS NOT NULL AND r.to_file <> ''
  ",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            imports
                .entry(col_string(r, 0)?)
                .or_default()
                .insert(col_string(r, 1)?);
        }
    }

    // buildWiringGraph
    let mut vis = Visibility {
        families: HashMap::new(),
        dirs: HashMap::new(),
        imported_dirs: HashMap::new(),
    };
    let mut index: HashMap<i64, usize> = HashMap::new();
    let mut ids: Vec<i64> = Vec::new();
    let mut edges: Vec<(usize, usize, f64)> = Vec::new();
    let intern = |id: i64, index: &mut HashMap<i64, usize>, ids: &mut Vec<i64>| -> usize {
        *index.entry(id).or_insert_with(|| {
            ids.push(id);
            ids.len() - 1
        })
    };
    for (from, to, ct) in &refs {
        if !matches!(
            ct.as_str(),
            "call" | "import" | "type_ref" | "inherit" | "implement"
        ) {
            continue;
        }
        if from == to {
            continue;
        }
        let u = intern(*from, &mut index, &mut ids);
        let v = intern(*to, &mut index, &mut ids);
        let homonyms = candidates.get(to).copied().unwrap_or(1);
        let mut w = if homonyms > 1 {
            1.0 / homonyms as f64
        } else {
            1.0
        };
        if let (Some(sf), Some(tf)) = (file_of.get(from), file_of.get(to))
            && sf != tf
        {
            let im = imports.get(sf);
            if vis.visible(sf, tf, im) {
                // Visible by language scoping rules: full weight.
            } else if im.is_none() {
                w *= 0.25;
            } else if !im.is_some_and(|s| s.contains(tf)) {
                w *= 0.02;
            }
        }
        edges.push((u, v, w));
    }
    let size = ids.len();
    let mut counts = vec![0usize; size];
    for &(u, v, _) in &edges {
        counts[u] += 1;
        counts[v] += 1;
    }
    let mut offsets = vec![0usize; size + 1];
    let mut running = 0;
    for u in 0..size {
        offsets[u] = running;
        running += counts[u];
    }
    offsets[size] = running;
    let mut cursor = offsets[..size].to_vec();
    let mut neighbours = vec![0usize; running];
    let mut weights = vec![0f64; running];
    for &(u, v, w) in &edges {
        neighbours[cursor[u]] = v;
        weights[cursor[u]] = w;
        cursor[u] += 1;
        neighbours[cursor[v]] = u;
        weights[cursor[v]] = w;
        cursor[v] += 1;
    }
    Ok((
        Graph {
            size,
            ids,
            index,
            offsets,
            neighbours,
            weights,
        },
        file_of,
    ))
}

/// `pageRank` with the default restart (0.25) and 25 iterations. `seeds`
/// keeps `Map` insertion order.
fn page_rank(g: &Graph, seeds: &[(usize, f64)]) -> Vec<f64> {
    let size = g.size;
    if size == 0 {
        return Vec::new();
    }
    let restart = 0.25f64;
    let walk = 1.0 - restart;
    let mut dist = vec![0f64; size];
    let mut seed_total = 0f64;
    for &(node, w) in seeds {
        if node >= size || w.partial_cmp(&0.0) != Some(Ordering::Greater) {
            continue;
        }
        dist[node] += w;
        seed_total += w;
    }
    if seed_total > 0.0 {
        for d in dist.iter_mut() {
            *d /= seed_total;
        }
    } else {
        dist.fill(1.0 / size as f64);
    }
    let mut cur = dist.clone();
    let mut next = vec![0f64; size];
    for _ in 0..25 {
        next.fill(0.0);
        let mut teleport = 0f64;
        for (u, &mass) in cur.iter().enumerate() {
            if mass == 0.0 {
                continue;
            }
            teleport += restart * mass;
            let (start, end) = (g.offsets[u], g.offsets[u + 1]);
            let degree = end - start;
            if degree == 0 {
                teleport += walk * mass;
                continue;
            }
            let outgoing = walk * mass;
            let share = outgoing / degree as f64;
            let mut moved = 0f64;
            for e in start..end {
                let transfer = share * g.weights[e];
                next[g.neighbours[e]] += transfer;
                moved += transfer;
            }
            teleport += outgoing - moved;
        }
        if teleport > 0.0 {
            for (n, d) in next.iter_mut().zip(&dist) {
                *n += teleport * d;
            }
        }
        std::mem::swap(&mut cur, &mut next);
    }
    let mut max = 0f64;
    for &v in &cur {
        if v > max {
            max = v;
        }
    }
    if max > 0.0 {
        cur.iter().map(|v| v / max).collect()
    } else {
        vec![0.0; size]
    }
}

// ─── context (context-retrieval.ts, index-service.ts) ───────────────────────

/// `posixIndexPath`.
fn posix_index_path(p: &str) -> String {
    let p = p.replace('\\', "/");
    match p.strip_prefix("./") {
        Some(rest) => rest.to_string(),
        None => p,
    }
}

/// Resolve a win32 path into (root, components). Relative paths resolve
/// against `base` (WrongStack resolves against the daemon's cwd; the project
/// root is the closest stand-in).
fn win32_resolve(p: &str, base: Option<&str>) -> (String, Vec<String>) {
    let root_len = win32_root_len(p);
    let absolute = win32_is_absolute(p);
    let (root, mut comps) = match (absolute, base) {
        (false, Some(b)) => win32_resolve(b, None),
        _ => (p[..root_len].replace('/', "\\"), Vec::new()),
    };
    let rest = if absolute { &p[root_len..] } else { p };
    for part in rest.split(is_sep) {
        match part {
            "" | "." => {}
            ".." => {
                comps.pop();
            }
            s => comps.push(s.to_string()),
        }
    }
    (root, comps)
}

/// Node's `path.win32.relative(from, to)`.
fn win32_relative(from: &str, to: &str) -> String {
    let (fr, fc) = win32_resolve(from, None);
    let (tr, tc) = win32_resolve(to, Some(from));
    if fr.to_lowercase() != tr.to_lowercase() {
        return format!("{tr}{}", tc.join("\\"));
    }
    let mut k = 0;
    while k < fc.len() && k < tc.len() && fc[k].to_lowercase() == tc[k].to_lowercase() {
        k += 1;
    }
    let mut parts: Vec<String> = vec!["..".to_string(); fc.len() - k];
    parts.extend(tc[k..].iter().cloned());
    parts.join("\\")
}

fn win32_is_absolute(p: &str) -> bool {
    let b: Vec<char> = p.chars().take(3).collect();
    match b.as_slice() {
        [c, ..] if is_sep(*c) => true,
        [l, ':', s, ..] => l.is_ascii_alphabetic() && is_sep(*s),
        _ => false,
    }
}

/// The `relativeOf` callback of `contextService`.
fn relative_of(root: &str, file: &str) -> String {
    let rel = win32_relative(root, file);
    if rel.is_empty() || rel == ".." || rel.starts_with("..\\") || win32_is_absolute(&rel) {
        posix_index_path(file)
    } else {
        posix_index_path(&rel)
    }
}

struct Scored {
    seed: bool,
    score: f64,
    line: Value,
    entry: Value,
}

struct Bucket {
    file: String,
    best: f64,
    matched: bool,
    symbols: Vec<Scored>,
}

/// `groupIntoEntries`; returns `(entries, totalCandidates)`.
#[allow(clippy::too_many_arguments)]
fn group_into_entries(
    symbols: &[Hit],
    score_by_id: &HashMap<i64, f64>,
    file_of: &HashMap<i64, String>,
    limit: usize,
    symbols_per_file: usize,
    prefix: Option<&str>,
    root: &str,
    seed_ids: &HashSet<i64>,
) -> (Vec<Value>, usize) {
    let mut order: Vec<String> = Vec::new();
    let mut by_file: HashMap<String, Bucket> = HashMap::new();
    for s in symbols {
        let stored = match s.file_str() {
            "" => file_of.get(&s.id).map_or("", String::as_str),
            f => f,
        };
        if stored.is_empty() {
            continue;
        }
        let relative = relative_of(root, stored);
        if let Some(p) = prefix
            && relative != p
            && !relative.starts_with(&format!("{p}/"))
        {
            continue;
        }
        let score = score_by_id.get(&s.id).copied().unwrap_or(s.score);
        let seed = seed_ids.contains(&s.id);
        let entry = json!({
            "name": s.name,
            "kind": s.kind,
            "line": s.line,
            "signature": s.signature,
            "seed": seed,
        });
        let sym = Scored {
            seed,
            score,
            line: s.line.clone(),
            entry,
        };
        match by_file.get_mut(&relative) {
            None => {
                order.push(relative.clone());
                by_file.insert(
                    relative.clone(),
                    Bucket {
                        file: relative,
                        best: score,
                        matched: seed,
                        symbols: vec![sym],
                    },
                );
            }
            Some(b) => {
                b.symbols.push(sym);
                if score > b.best {
                    b.best = score;
                }
                if seed {
                    b.matched = true;
                }
            }
        }
    }
    let mut buckets: Vec<Bucket> = order
        .iter()
        .map(|f| by_file.remove(f).expect("bucket"))
        .collect();
    buckets.sort_by(|a, b| f64_desc(a.best, b.best).then_with(|| locale_compare(&a.file, &b.file)));
    let total = buckets.len();
    buckets.truncate(limit);
    let max = buckets.first().map_or(0.0, |b| b.best);
    let entries = buckets
        .into_iter()
        .map(|mut b| {
            b.symbols.sort_by(|x, y| {
                (y.seed as i32)
                    .cmp(&(x.seed as i32))
                    .then_with(|| f64_desc(x.score, y.score))
                    .then_with(|| {
                        let (xl, yl) = (
                            x.line.as_f64().unwrap_or(f64::NAN),
                            y.line.as_f64().unwrap_or(f64::NAN),
                        );
                        xl.partial_cmp(&yl).unwrap_or(Ordering::Equal)
                    })
            });
            b.symbols.truncate(symbols_per_file);
            json!({
                "file": b.file,
                "relevance": num(if max > 0.0 { b.best / max } else { 0.0 }),
                "matched": b.matched,
                "symbols": b.symbols.into_iter().map(|s| s.entry).collect::<Vec<_>>(),
            })
        })
        .collect();
    (entries, total)
}

/// `Math.max(lo, Math.min(Math.trunc(n ?? default), hi))` for a JS-number arg.
fn int_option(args: &Value, key: &str, default: f64, hi: f64) -> usize {
    let raw = args
        .get(key)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
        .unwrap_or(default);
    raw.trunc().min(hi).max(1.0) as usize
}

/// `retrieveContext` as called by `contextService`.
pub fn context(r: &Reader, args: &Value) -> Result<Value, QueryError> {
    const SEED_LIMIT: f64 = 40.0;
    const NODE_HYDRATION_LIMIT: usize = 400;
    const SEMANTIC_SYMBOLS_PER_FILE: i64 = 3;
    const SEMANTIC_SEED_SHARE: f64 = 0.35;

    let c = &r.conn;
    let root = args
        .get("projectRoot")
        .and_then(Value::as_str)
        .ok_or_else(|| QueryError::InvalidArgs("projectRoot must be a string".into()))?;
    let query = js_trim(query_arg(args)?);
    let limit = int_option(args, "limit", 12.0, 50.0);
    let symbols_per_file = int_option(args, "symbolsPerFile", 4.0, 20.0);
    let prefix: Option<String> = match args.get("pathPrefix").and_then(Value::as_str) {
        Some(p) => {
            let t = posix_index_path(&js_trim(p));
            let t = t.trim_end_matches('/');
            (!t.is_empty()).then(|| t.to_string())
        }
        None => None,
    };
    let vector_files: Vec<(String, f64)> = args
        .get("vectorFiles")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| {
                    (
                        v.get("file")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        v.get("score").and_then(Value::as_f64).unwrap_or(f64::NAN),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let empty = |status: &str| {
        json!({
            "query": query,
            "entries": [],
            "seedCount": 0,
            "semanticSeedCount": 0,
            "totalCandidates": 0,
            "indexStatus": status,
        })
    };
    if query.is_empty() {
        return Ok(empty("no-matches"));
    }

    let (results, _) = search_ranked(c, &query, &Filter::default(), Some(SEED_LIMIT))?;
    if results.is_empty() && vector_files.is_empty() {
        let (files, _) = index_summary(c)?;
        return Ok(empty(if files == 0 { "no-index" } else { "no-matches" }));
    }

    let (graph, file_of) = load_graph(c)?;
    let finish = |(entries, total): (Vec<Value>, usize), status: &str| {
        json!({
            "query": query,
            "entries": entries,
            "totalCandidates": total,
            "seedCount": results.len(),
            "semanticSeedCount": vector_files.len(),
            "indexStatus": status,
        })
    };
    let unranked = || {
        finish(
            group_into_entries(
                &results,
                &HashMap::new(),
                &file_of,
                limit,
                symbols_per_file,
                prefix.as_deref(),
                root,
                &HashSet::new(),
            ),
            "unranked",
        )
    };
    if graph.size == 0 {
        return Ok(unranked());
    }

    // Seeds, in `Map` insertion order.
    let mut seeds: Vec<(usize, f64)> = Vec::new();
    let mut seed_pos: HashMap<usize, usize> = HashMap::new();
    let mut add_seed =
        |seeds: &mut Vec<(usize, f64)>, dense: usize, w: f64| match seed_pos.get(&dense) {
            Some(&i) => seeds[i].1 += w,
            None => {
                seed_pos.insert(dense, seeds.len());
                seeds.push((dense, w));
            }
        };
    let mut lexical_mass = 0f64;
    for hit in &results {
        let Some(&dense) = graph.index.get(&hit.id) else {
            continue;
        };
        let w = if hit.score > 0.0 { hit.score } else { 1.0 };
        lexical_mass += w;
        add_seed(&mut seeds, dense, w);
    }

    // Semantic seeds (no recorded fixture passes `vectorFiles`).
    let mut semantic: Vec<(usize, f64)> = Vec::new();
    let mut semantic_pos: HashMap<usize, usize> = HashMap::new();
    for (file, score) in &vector_files {
        if score.partial_cmp(&0.0) != Some(Ordering::Greater) {
            continue;
        }
        let mut stmt = c.prepare(&format!(
            "SELECT id, name, kind, line, end_line, is_exported, signature FROM symbols s
     WHERE file = ? AND NOT (name = '{MODULE_OWNER_NAME}' AND EXISTS (
       SELECT 1 FROM symbols o WHERE o.file = s.file AND o.name <> '{MODULE_OWNER_NAME}'))
     ORDER BY line ASC, name ASC LIMIT ?"
        ))?;
        let ids: Vec<i64> = stmt
            .query_map(rusqlite::params![file, SEMANTIC_SYMBOLS_PER_FILE], |r| {
                col_i64(r, 0)
            })?
            .collect::<rusqlite::Result<_>>()?;
        for id in ids {
            let Some(&dense) = graph.index.get(&id) else {
                continue;
            };
            match semantic_pos.get(&dense) {
                Some(&i) => semantic[i].1 += score,
                None => {
                    semantic_pos.insert(dense, semantic.len());
                    semantic.push((dense, *score));
                }
            }
        }
    }
    if !semantic.is_empty() {
        let mut semantic_mass = 0f64;
        for &(_, w) in &semantic {
            semantic_mass += w;
        }
        let scale = if semantic_mass > 0.0 && lexical_mass > 0.0 {
            (lexical_mass * SEMANTIC_SEED_SHARE) / semantic_mass
        } else {
            1.0
        };
        for &(dense, w) in &semantic {
            add_seed(&mut seeds, dense, w * scale);
        }
    }
    if seeds.is_empty() {
        return Ok(unranked());
    }

    let scores = page_rank(&graph, &seeds);
    let mut ordered: Vec<(usize, f64)> = scores
        .iter()
        .enumerate()
        .filter(|(_, s)| **s > 0.0)
        .map(|(u, s)| (u, *s))
        .collect();
    ordered.sort_by(|a, b| f64_desc(a.1, b.1));
    ordered.truncate(NODE_HYDRATION_LIMIT);

    let mut score_by_id: HashMap<i64, f64> = HashMap::new();
    let mut key_order: Vec<i64> = Vec::new();
    for &(u, s) in &ordered {
        let id = graph.ids[u];
        if score_by_id.insert(id, s).is_none() {
            key_order.push(id);
        }
    }

    // getSymbolsByIdsWithStatement: one chunk (≤ 400 ids), padded to a
    // power-of-two bucket with the first id, exactly as WrongStack binds it.
    let mut walked: Vec<Hit> = Vec::new();
    if !key_order.is_empty() {
        let mut padded = key_order.clone();
        let target = padded.len().next_power_of_two();
        while padded.len() < target {
            padded.push(padded[0]);
        }
        let sql = format!(
            "SELECT id, name, kind, lang, file, line, signature, scope FROM symbols WHERE id IN ({})",
            vec!["?"; padded.len()].join(",")
        );
        let mut stmt = c.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(padded))?;
        while let Some(row) = rows.next()? {
            let id = col_i64(row, 0)?;
            walked.push(Hit {
                id,
                name: col_json(row, 1)?,
                kind: col_json(row, 2)?,
                lang: col_json(row, 3)?,
                file: col_json(row, 4)?,
                line: col_json(row, 5)?,
                col: json!(0),
                signature: col_json(row, 6)?,
                doc_comment: json!(""),
                score: score_by_id.get(&id).copied().unwrap_or(0.0),
                snippet: String::new(),
                lsp_kind: None,
            });
        }
    }

    let mut floor = f64::INFINITY;
    for &s in score_by_id.values() {
        if s < floor {
            floor = s;
        }
    }
    if !floor.is_finite() || floor <= 0.0 {
        floor = 1.0;
    }
    let unwired: Vec<&Hit> = results
        .iter()
        .filter(|h| !score_by_id.contains_key(&h.id))
        .collect();
    let mut max_lexical = 0f64;
    for h in &unwired {
        // Math.max(0, ...scores): NaN poisons the result.
        if h.score.is_nan() || max_lexical.is_nan() {
            max_lexical = f64::NAN;
        } else if h.score > max_lexical {
            max_lexical = h.score;
        }
    }
    for h in unwired {
        let lexical = if max_lexical > 0.0 && h.score > 0.0 {
            h.score / max_lexical
        } else {
            1.0
        };
        let score = floor * 0.5 * lexical;
        score_by_id.insert(h.id, score);
        let mut x = h.clone();
        x.score = score;
        walked.push(x);
    }

    let seed_ids: HashSet<i64> = results.iter().map(|h| h.id).collect();
    Ok(finish(
        group_into_entries(
            &walked,
            &score_by_id,
            &file_of,
            limit,
            symbols_per_file,
            prefix.as_deref(),
            root,
            &seed_ids,
        ),
        "ok",
    ))
}

// ─── vectorSearch (writer-vectors.ts) ───────────────────────────────────────

fn l2_norm(v: &[f32]) -> f64 {
    let mut sum = 0f64;
    for &x in v {
        sum += x as f64 * x as f64;
    }
    sum.sqrt()
}

/// `vectorSearchService`.
pub fn vector_search(r: &Reader, args: &Value) -> Result<Value, QueryError> {
    let c = &r.conn;
    let total: i64 = match c.query_row("SELECT COUNT(*) AS n FROM file_vectors", [], |r| {
        col_i64(r, 0)
    }) {
        Ok(n) => n,
        // WrongStack creates the table at open; a missing one is empty.
        Err(rusqlite::Error::SqliteFailure(_, Some(msg))) if msg.contains("no such table") => 0,
        Err(e) => return Err(e.into()),
    };
    if total == 0 {
        return Ok(json!({ "hits": [], "total": total }));
    }
    // Float32Array.from(args.vector)
    let query: Vec<f32> = args
        .get("vector")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_f64().unwrap_or(f64::NAN) as f32)
                .collect()
        })
        .unwrap_or_default();
    let limit = args
        .get("limit")
        .and_then(Value::as_f64)
        .unwrap_or(f64::NAN);
    let min_score = match args.get("minScore") {
        None | Some(Value::Null) => 0.0,
        Some(v) => v.as_f64().unwrap_or(f64::NAN),
    };
    let mut hits: Vec<(String, f64)> = Vec::new();
    let query_norm = l2_norm(&query);
    if limit > 0.0 && !query.is_empty() && query_norm > 0.0 {
        let floor = if min_score.is_finite() {
            min_score
        } else {
            f64::NEG_INFINITY
        };
        let mut stmt = c.prepare("SELECT file, vector FROM file_vectors")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let file = col_string(row, 0)?;
            let bytes: &[u8] = match row.get_ref(1)? {
                ValueRef::Blob(b) | ValueRef::Text(b) => b,
                _ => continue,
            };
            if !bytes.len().is_multiple_of(4) {
                continue;
            }
            let vector: Vec<f32> = bytes
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            if vector.len() != query.len() {
                continue;
            }
            let vector_norm = l2_norm(&vector);
            if vector_norm.partial_cmp(&0.0) != Some(Ordering::Greater) {
                continue;
            }
            let mut dot = 0f64;
            for (q, v) in query.iter().zip(&vector) {
                dot += *q as f64 * *v as f64;
            }
            let score = dot / (query_norm * vector_norm);
            if score >= floor {
                hits.push((file, score));
            }
        }
        hits.sort_by(|a, b| f64_desc(a.1, b.1).then_with(|| locale_compare(&a.0, &b.0)));
        // `slice(0, limit)`: ToIntegerOrInfinity.
        hits.truncate(limit.trunc().min(usize::MAX as f64) as usize);
    }
    Ok(json!({
        "hits": hits
            .into_iter()
            .map(|(file, score)| json!({ "file": file, "score": num(score) }))
            .collect::<Vec<_>>(),
        "total": total,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenise_matches_bm25() {
        assert_eq!(
            tokenise("  Foo-bar $x it's  "),
            vec!["foo", "bar", "$x", "it's"]
        );
        assert_eq!(tokenise("a  a"), vec!["a", "a"]);
        assert!(tokenise("()").is_empty());
        assert_eq!(tokenise("Ⅻ²x"), vec!["ⅻ²x"]);
    }

    #[test]
    fn snippet_counts_utf16_units() {
        let raw = "x".repeat(50) + "needle" + &"y".repeat(50);
        let s = extract_snippet(&raw, &["needle".into()], 40);
        assert!(s.starts_with('\u{2026}') && s.ends_with('\u{2026}'));
        assert_eq!(u16len(&s), 40 + 6 + 40 + 2);
        assert_eq!(extract_snippet("abc", &["zz".into()], 40), "abc");
    }

    #[test]
    fn win32_relative_is_case_insensitive() {
        assert_eq!(relative_of(r"C:\p\root", r"c:\P\Root\src\a.ts"), "src/a.ts");
        assert_eq!(
            relative_of(r"C:\p\root", r"C:\p\other\a.ts"),
            "C:/p/other/a.ts"
        );
        assert_eq!(relative_of(r"C:\p\root", r"D:\x\a.ts"), "D:/x/a.ts");
        assert_eq!(win32_dirname(r"C:\a\b.go"), r"C:\a");
        assert_eq!(win32_dirname(r"C:\b.go"), r"C:\");
    }

    #[test]
    fn numbers_round_trip_like_json_stringify() {
        assert_eq!(num(1.0), json!(1));
        assert_eq!(num(0.5), json!(0.5));
        assert_eq!(num(f64::NAN), Value::Null);
    }
}
