//! `packageGraph`, `fileGraph`, `symbolGraph`, `incomingCalls`,
//! `outgoingCalls` (ports of `writer-graph-reader.ts`,
//! `writer-graph-queries.ts`, `writer-graph-projections.ts`,
//! `writer-graph-decorate.ts`, `writer-graph-helpers.ts` and the service
//! wrappers in `index-service.ts`).
//!
//! Where WrongStack depends on SQLite row order without a total `ORDER BY`,
//! the same SQL text and the same chunking (`inChunks` + power-of-two
//! padding) are issued so rows come back in the same order.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::path::Path;

use rusqlite::types::Value as Sql;
use rusqlite::{Connection, params_from_iter};
use serde_json::{Map, Value, json};

use super::{QueryError, Reader};

/// `MAX_SQL_VARS` used by WrongStack's chunked id queries.
const MAX_VARS: usize = 900;
const MODULE_SYMBOL: &str = "<module>";

// ─── small helpers ───────────────────────────────────────────────────────────

type Row = Vec<Sql>;

fn query(c: &Connection, sql: &str, params: &[Sql]) -> Result<Vec<Row>, QueryError> {
    let mut st = c.prepare(sql)?;
    let n = st.column_count();
    let rows = st.query_map(params_from_iter(params.iter()), |r| {
        (0..n).map(|i| r.get::<_, Sql>(i)).collect()
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn as_i64(v: &Sql) -> Option<i64> {
    match v {
        Sql::Integer(i) => Some(*i),
        Sql::Real(f) => Some(*f as i64),
        _ => None,
    }
}

fn as_f64(v: &Sql) -> Option<f64> {
    match v {
        Sql::Integer(i) => Some(*i as f64),
        Sql::Real(f) => Some(*f),
        _ => None,
    }
}

fn as_str(v: &Sql) -> Option<&str> {
    match v {
        Sql::Text(s) => Some(s),
        _ => None,
    }
}

/// A JS number as JSON: integral values print as integers.
fn num(v: f64) -> Value {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        json!(v as i64)
    } else if v.is_finite() {
        json!(v)
    } else {
        Value::Null
    }
}

/// A SQLite value as better-sqlite3 would hand it to `JSON.stringify`.
fn to_json(v: &Sql) -> Value {
    match v {
        Sql::Null | Sql::Blob(_) => Value::Null,
        Sql::Integer(i) => json!(i),
        Sql::Real(f) => num(*f),
        Sql::Text(s) => json!(s),
    }
}

/// JS `<` on strings: UTF-16 code-unit order.
fn js_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// Approximation of ICU `String.prototype.localeCompare` (no ICU dependency
/// is allowed): case-folded comparison first, then raw UTF-16 order as the
/// tie-breaker. The recorded fixtures do not distinguish the two.
fn locale_cmp(a: &str, b: &str) -> Ordering {
    js_cmp(&a.to_lowercase(), &b.to_lowercase()).then_with(|| js_cmp(a, b))
}

/// JS truthiness of an argument.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, QueryError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| QueryError::InvalidArgs(format!("'{key}' must be a string")))
}

/// JS `String.prototype.trim`.
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `posixIndexPath`: backslashes to slashes, leading `./` removed.
fn posix(p: &str) -> String {
    let p = p.replace('\\', "/");
    match p.strip_prefix("./") {
        Some(rest) => rest.to_string(),
        None => p,
    }
}

fn escape_like(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for ch in v.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn basename(f: &str) -> &str {
    match f.rfind(['/', '\\']) {
        Some(i) => &f[i + 1..],
        None => f,
    }
}

/// Insertion-ordered map (JS `Map` semantics).
struct OrderedMap<K, V> {
    entries: Vec<(K, V)>,
    index: HashMap<K, usize>,
}

impl<K: Eq + Hash + Clone, V> OrderedMap<K, V> {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            index: HashMap::new(),
        }
    }

    fn get(&self, k: &K) -> Option<&V> {
        self.index.get(k).map(|&i| &self.entries[i].1)
    }

    fn contains(&self, k: &K) -> bool {
        self.index.contains_key(k)
    }

    /// `map.set(k, v)`: keeps the original position of an existing key.
    fn set(&mut self, k: K, v: V) {
        match self.index.get(&k) {
            Some(&i) => self.entries[i].1 = v,
            None => {
                self.index.insert(k.clone(), self.entries.len());
                self.entries.push((k, v));
            }
        }
    }

    fn entry_or(&mut self, k: K, make: impl FnOnce() -> V) -> &mut V {
        let i = match self.index.get(&k) {
            Some(&i) => i,
            None => {
                self.index.insert(k.clone(), self.entries.len());
                self.entries.push((k, make()));
                self.entries.len() - 1
            }
        };
        &mut self.entries[i].1
    }
}

// ─── chunked id queries (writer-helpers.ts) ──────────────────────────────────

fn next_pow2(n: usize) -> usize {
    n.max(1).next_power_of_two()
}

fn prev_pow2(n: usize) -> usize {
    if n == 0 {
        1
    } else {
        1 << (usize::BITS - 1 - n.leading_zeros())
    }
}

fn in_chunks(total: usize, max: usize) -> Vec<usize> {
    if total == 0 {
        return Vec::new();
    }
    if next_pow2(total) <= max {
        return vec![total];
    }
    let cap = prev_pow2(max).max(1);
    let mut out = Vec::new();
    let mut rem = total;
    while rem > 0 {
        let take = prev_pow2(rem.min(cap)).min(rem).max(1);
        out.push(take);
        rem -= take;
    }
    out
}

/// Pad a chunk to the next power of two by repeating its first value, so
/// prepared statements are reused across sizes.
fn pad(v: &[Sql]) -> Vec<Sql> {
    let mut out = v.to_vec();
    if out.len() <= 1 {
        return out;
    }
    let target = next_pow2(out.len());
    while out.len() < target {
        out.push(out[0].clone());
    }
    out
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

fn chunked(
    c: &Connection,
    ids: &[Sql],
    build: impl Fn(&str) -> String,
    extra: &[Sql],
) -> Result<Vec<Row>, QueryError> {
    let mut out = Vec::new();
    let mut cur = 0;
    for take in in_chunks(ids.len(), MAX_VARS) {
        let mut params = pad(&ids[cur..cur + take]);
        cur += take;
        let sql = build(&placeholders(params.len()));
        params.extend_from_slice(extra);
        out.extend(query(c, &sql, &params)?);
    }
    Ok(out)
}

fn ints(ids: &[i64]) -> Vec<Sql> {
    ids.iter().map(|&i| Sql::Integer(i)).collect()
}

fn texts(v: &[String]) -> Vec<Sql> {
    v.iter().map(|s| Sql::Text(s.clone())).collect()
}

fn resolve_indexed_files(c: &Connection, file: &str) -> Result<Vec<String>, QueryError> {
    let pz = posix(js_trim(file));
    let like = format!("%/{}", escape_like(&pz));
    let rows = query(
        c,
        r"SELECT DISTINCT file FROM symbols WHERE (file = ? OR replace(file, '\', '/') = ? OR replace(file, '\', '/') LIKE ? ESCAPE '\') ORDER BY length(file), file",
        &[Sql::Text(file.to_string()), Sql::Text(pz), Sql::Text(like)],
    )?;
    Ok(rows
        .iter()
        .filter_map(|r| as_str(&r[0]).map(str::to_string))
        .collect())
}

// ─── packages ────────────────────────────────────────────────────────────────

/// `derivePackageFromLayout` (module-roots.ts).
fn derive_package_from_layout(file: &str) -> Option<String> {
    let p = file.replace('\\', "/");
    for (marker, prefix) in [("/packages/", "@wrongstack/"), ("/apps/", "app:")] {
        if let Some(i) = p.find(marker) {
            let seg = p[i + marker.len()..].split('/').next().unwrap_or("");
            if !seg.is_empty() {
                return Some(format!("{prefix}{seg}"));
            }
        }
    }
    None
}

struct Labeller {
    stored: HashMap<String, String>,
}

impl Labeller {
    fn load(c: &Connection) -> Result<Self, QueryError> {
        let mut stored = HashMap::new();
        for r in query(
            c,
            "SELECT file, package FROM files WHERE package != ''",
            &[],
        )? {
            if let (Some(f), Some(p)) = (as_str(&r[0]), as_str(&r[1])) {
                stored.insert(f.to_string(), p.to_string());
            }
        }
        Ok(Self { stored })
    }

    fn label(&self, file: &str) -> String {
        if let Some(p) = self.stored.get(file) {
            return p.clone();
        }
        derive_package_from_layout(file).unwrap_or_else(|| "(root)".to_string())
    }
}

// ─── edges ───────────────────────────────────────────────────────────────────

struct EdgeAcc {
    weight: i64,
    types: Vec<(String, i64)>,
}

type EdgeMap = OrderedMap<(String, String), EdgeAcc>;

fn add_edge(em: &mut EdgeMap, s: String, t: String, call_type: &str, w: i64) {
    let e = em.entry_or((s, t), || EdgeAcc {
        weight: 0,
        types: Vec::new(),
    });
    e.weight += w;
    match e.types.iter_mut().find(|(k, _)| k == call_type) {
        Some((_, n)) => *n += w,
        None => e.types.push((call_type.to_string(), w)),
    }
}

fn materialize(em: EdgeMap, prefix: &str) -> Vec<Value> {
    em.entries
        .into_iter()
        .map(|((s, t), e)| {
            let mut best = ("call", 0i64);
            for (ty, n) in &e.types {
                if *n > best.1 {
                    best = (ty.as_str(), *n);
                }
            }
            json!({
                "source": format!("{prefix}:{s}"),
                "target": format!("{prefix}:{t}"),
                "weight": e.weight,
                "refType": best.0,
            })
        })
        .collect()
}

/// `(from_id, to_id, call_type, n)` for refs leaving or entering `ids`.
fn ref_counts_touching(c: &Connection, ids: &[i64]) -> Result<Vec<Row>, QueryError> {
    let local: HashSet<i64> = ids.iter().copied().collect();
    let params = ints(ids);
    let mut out = chunked(
        c,
        &params,
        |p| {
            format!(
                "SELECT from_id, to_id, call_type, COUNT(*) AS n FROM refs
        WHERE from_id IN ({p}) AND to_id IS NOT NULL
        GROUP BY from_id, to_id, call_type"
            )
        },
        &[],
    )?;
    for r in chunked(
        c,
        &params,
        |p| {
            format!(
                "SELECT from_id, to_id, call_type, COUNT(*) AS n FROM refs
        WHERE to_id IN ({p})
        GROUP BY from_id, to_id, call_type"
            )
        },
        &[],
    )? {
        if !as_i64(&r[0]).is_some_and(|f| local.contains(&f)) {
            out.push(r);
        }
    }
    Ok(out)
}

fn id_key(v: &Sql) -> String {
    match v {
        Sql::Integer(i) => i.to_string(),
        other => to_json(other).to_string(),
    }
}

// ─── decoration (writer-graph-decorate.ts) ───────────────────────────────────

fn decorate(
    c: &Connection,
    nodes: &mut [Map<String, Value>],
    pkg: &Labeller,
) -> Result<(), QueryError> {
    if nodes.is_empty() {
        return Ok(());
    }
    let mut file_rank: OrderedMap<String, f64> = OrderedMap::new();
    for r in query(c, "SELECT file, rank FROM file_rank", &[])? {
        if let (Some(f), Some(rk)) = (as_str(&r[0]), as_f64(&r[1])) {
            file_rank.set(f.to_string(), rk);
        }
    }
    let mut mtime: HashMap<String, Sql> = HashMap::new();
    for r in query(c, "SELECT file, mtime_ms FROM files", &[])? {
        if let Some(f) = as_str(&r[0]) {
            mtime.insert(f.to_string(), r[1].clone());
        }
    }
    let mut concepts: HashMap<String, Row> = HashMap::new();
    for r in query(
        c,
        "SELECT file, summary, crux_start, crux_end FROM file_concepts WHERE state = 'ready' AND summary != ''",
        &[],
    )? {
        if let Some(f) = as_str(&r[0]) {
            concepts.insert(f.to_string(), r);
        }
    }
    let mut subsystems: HashMap<String, Row> = HashMap::new();
    for r in query(c, "SELECT id, name, summary FROM subsystems", &[])? {
        if let Some(id) = as_str(&r[0]) {
            subsystems.insert(id.to_string(), r);
        }
    }
    let symbol_ids: Vec<Sql> = nodes
        .iter()
        .filter_map(|n| n.get("symbolId").and_then(Value::as_i64).map(Sql::Integer))
        .collect();
    let mut symbol_rank: HashMap<i64, f64> = HashMap::new();
    for ch in symbol_ids.chunks(MAX_VARS) {
        let sql = format!(
            "SELECT symbol_id, rank FROM symbol_rank WHERE symbol_id IN ({})",
            placeholders(ch.len())
        );
        for r in query(c, &sql, ch)? {
            if let (Some(id), Some(rk)) = (as_i64(&r[0]), as_f64(&r[1])) {
                symbol_rank.insert(id, rk);
            }
        }
    }
    let mut package_rank: HashMap<String, f64> = HashMap::new();
    if nodes.iter().any(|n| n["kind"] == "package") {
        for (f, rk) in &file_rank.entries {
            *package_rank.entry(pkg.label(f)).or_insert(0.0) += rk;
        }
    }

    let mut ranks: Vec<Option<f64>> = vec![None; nodes.len()];
    for (n, rank) in nodes.iter_mut().zip(ranks.iter_mut()) {
        if n["kind"] == "package" {
            let p = n["package"].as_str().unwrap_or("").to_string();
            *rank = package_rank.get(&p).copied();
            if let Some(s) = subsystems.get(&p) {
                n.insert("subsystem".into(), to_json(&s[1]));
                if as_str(&s[2]) != Some("") {
                    n.insert("concept".into(), to_json(&s[2]));
                }
            }
            continue;
        }
        let Some(f) = n.get("file").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        if let Some(m) = mtime.get(&f) {
            n.insert("lastModifiedMs".into(), to_json(m));
        }
        if let Some(s) = subsystems.get(&pkg.label(&f)) {
            n.insert("subsystem".into(), to_json(&s[1]));
        }
        if n["kind"] == "file" {
            *rank = file_rank.get(&f).copied();
            if let Some(cc) = concepts.get(&f) {
                n.insert("concept".into(), to_json(&cc[1]));
                if !matches!(cc[2], Sql::Null) && !matches!(cc[3], Sql::Null) {
                    n.insert(
                        "crux".into(),
                        json!({ "start": to_json(&cc[2]), "end": to_json(&cc[3]) }),
                    );
                }
            }
            continue;
        }
        if let Some(id) = n.get("symbolId").and_then(Value::as_i64) {
            *rank = symbol_rank.get(&id).copied();
        }
    }

    // normaliseInPlace: divide by the local maximum (externals excluded
    // unless every ranked node is external).
    let (mut local_max, mut all_max) = (0.0f64, 0.0f64);
    for (n, r) in nodes.iter().zip(&ranks) {
        let Some(r) = *r else { continue };
        if r > all_max {
            all_max = r;
        }
        if n.get("external") != Some(&Value::Bool(true)) && r > local_max {
            local_max = r;
        }
    }
    let max = if local_max > 0.0 { local_max } else { all_max };
    for (n, r) in nodes.iter_mut().zip(ranks) {
        if let Some(r) = r {
            let v = if max > 0.0 { (r / max).min(1.0) } else { r };
            n.insert("rank".into(), num(v));
        }
    }
    Ok(())
}

/// `decorateGraphNodesWithMemories` reads SAGE memories and WrongTrace
/// health from `<projectRoot>/.wrongstack`. That is not ported: when those
/// sources exist the result could differ, so the op is reported as
/// unsupported; when they are absent WrongStack's decoration is a no-op.
fn check_memory_sources(args: &Value, nodes: &[Map<String, Value>]) -> Result<(), QueryError> {
    if nodes.is_empty() {
        return Ok(());
    }
    let Some(root) = args.get("projectRoot").and_then(Value::as_str) else {
        return Ok(());
    };
    if root.is_empty() {
        return Ok(());
    }
    let ws = Path::new(root).join(".wrongstack");
    let sources = [
        ws.join("memories").join("sage.db"),
        ws.join("memories").join("memories.jsonl"),
        ws.join("wrongtrace-health.json"),
    ];
    if sources.iter().any(|p| p.exists()) {
        return Err(QueryError::Unsupported(
            "graph memory/health decoration (.wrongstack sources present)".into(),
        ));
    }
    Ok(())
}

fn graph(nodes: Vec<Map<String, Value>>, edges: Vec<Value>) -> Value {
    json!({
        "nodes": nodes.into_iter().map(Value::Object).collect::<Vec<_>>(),
        "edges": edges,
    })
}

fn empty_graph() -> Value {
    json!({ "nodes": [], "edges": [] })
}

// ─── packageGraph ────────────────────────────────────────────────────────────

pub fn package_graph(r: &Reader, _args: &Value) -> Result<Value, QueryError> {
    let c = &r.conn;
    let mut file_of: HashMap<i64, String> = HashMap::new();
    let mut per_file: HashMap<String, i64> = HashMap::new();
    for row in query(c, "SELECT id, file FROM symbols", &[])? {
        if let (Some(id), Some(f)) = (as_i64(&row[0]), as_str(&row[1])) {
            file_of.insert(id, f.to_string());
            *per_file.entry(f.to_string()).or_insert(0) += 1;
        }
    }
    let mut file_counts: Vec<(String, i64)> = per_file.into_iter().collect();
    file_counts.sort_by(|a, b| js_cmp(&a.0, &b.0));
    let files: Vec<String> = query(c, "SELECT DISTINCT file FROM files", &[])?
        .iter()
        .filter_map(|r| as_str(&r[0]).map(str::to_string))
        .collect();
    let pkg = Labeller::load(c)?;
    let mut f2p: HashMap<String, String> = HashMap::new();
    let mut gp = |f: &str| -> String {
        f2p.entry(f.to_string())
            .or_insert_with(|| pkg.label(f))
            .clone()
    };

    // package -> (symbolCount, fileCount)
    let mut nodes: OrderedMap<String, (i64, i64)> = OrderedMap::new();
    for (f, n) in &file_counts {
        nodes.entry_or(gp(f), || (0, 0)).0 += n;
    }
    for f in &files {
        nodes.entry_or(gp(f), || (0, 0)).1 += 1;
    }

    let mut pairs: HashMap<(String, String), HashMap<String, i64>> = HashMap::new();
    for row in query(
        c,
        "SELECT from_id, to_id, to_file, call_type FROM refs",
        &[],
    )? {
        let Some(from_file) = as_i64(&row[0]).and_then(|i| file_of.get(&i)).cloned() else {
            continue;
        };
        let to_sym_file = as_i64(&row[1]).and_then(|i| file_of.get(&i)).cloned();
        let ct = as_str(&row[3]).unwrap_or("").to_string();
        let (a, b) = if ct == "import" {
            let to = match as_str(&row[2]) {
                Some(t) => Some(t.to_string()),
                None => to_sym_file,
            };
            let Some(to) = to else { continue };
            let (a, b) = (gp(&from_file), gp(&to));
            if a == b || !nodes.contains(&b) {
                continue;
            }
            (a, b)
        } else {
            let Some(tf) = to_sym_file else { continue };
            let (a, b) = (gp(&from_file), gp(&tf));
            if a == b {
                continue;
            }
            (a, b)
        };
        *pairs.entry((a, b)).or_default().entry(ct).or_insert(0) += 1;
    }
    let mut pairs: Vec<_> = pairs.into_iter().collect();
    pairs.sort_by(|x, y| js_cmp(&x.0.0, &y.0.0).then_with(|| js_cmp(&x.0.1, &y.0.1)));
    let mut em = EdgeMap::new();
    for ((from, to), types) in pairs {
        let mut types: Vec<_> = types.into_iter().collect();
        types.sort_by(|x, y| {
            (x.0 == "import")
                .cmp(&(y.0 == "import"))
                .then_with(|| js_cmp(&x.0, &y.0))
        });
        for (t, n) in types {
            add_edge(&mut em, from.clone(), to.clone(), &t, n);
        }
    }
    let edges = materialize(em, "pkg");
    let mut out: Vec<Map<String, Value>> = nodes
        .entries
        .into_iter()
        .map(|(p, (sc, fc))| {
            let Value::Object(m) = json!({
                "id": format!("pkg:{p}"),
                "label": p,
                "kind": "package",
                "package": p,
                "symbolCount": sc,
                "fileCount": fc,
            }) else {
                unreachable!()
            };
            m
        })
        .collect();
    decorate(c, &mut out, &pkg)?;
    Ok(graph(out, edges))
}

// ─── fileGraph ───────────────────────────────────────────────────────────────

fn matches_package_filter(stored: &str, label: &str, filter: &str) -> bool {
    if label == filter {
        return true;
    }
    let pf = posix(stored);
    let fl = posix(js_trim(filter));
    if fl.is_empty() {
        return false;
    }
    pf == fl || pf.ends_with(&format!("/{fl}")) || pf.contains(&format!("/{fl}/"))
}

struct FileStat {
    count: i64,
    lang: Option<String>,
}

pub fn file_graph(r: &Reader, args: &Value) -> Result<Value, QueryError> {
    let filter = required_str(args, "packageFilter")?;
    let c = &r.conn;
    let all_files: Vec<String> = query(c, "SELECT DISTINCT file FROM symbols", &[])?
        .iter()
        .filter_map(|r| as_str(&r[0]).map(str::to_string))
        .collect();
    let pkg = Labeller::load(c)?;
    let lang_of = |f: &str| {
        wrongindex_discovery::lang::detect_lang(f)
            .unwrap_or("other")
            .to_string()
    };
    let mut local: Vec<String> = Vec::new();
    let mut local_set: HashSet<String> = HashSet::new();
    for f in all_files {
        if matches_package_filter(&f, &pkg.label(&f), filter) && local_set.insert(f.clone()) {
            local.push(f);
        }
    }
    if local.is_empty() {
        return Ok(empty_graph());
    }

    let mut syms = chunked(
        c,
        &texts(&local),
        |p| format!("SELECT file, id, name, kind, lang, line FROM symbols WHERE file IN ({p})"),
        &[],
    )?;
    syms.sort_by_key(|s| as_i64(&s[1]).unwrap_or(0));
    let mut sym_file: HashMap<i64, String> = HashMap::new();
    let mut stats: HashMap<String, FileStat> = HashMap::new();
    for s in &syms {
        let (Some(id), Some(f)) = (as_i64(&s[1]), as_str(&s[0])) else {
            continue;
        };
        sym_file.insert(id, f.to_string());
        let st = stats.entry(f.to_string()).or_insert_with(|| FileStat {
            count: 0,
            lang: as_str(&s[4]).map(str::to_string),
        });
        st.count += 1;
    }

    let mut nodes: OrderedMap<String, Map<String, Value>> = OrderedMap::new();
    let ensure = |nodes: &mut OrderedMap<String, Map<String, Value>>,
                  stats: &HashMap<String, FileStat>,
                  f: &str| {
        if nodes.contains(&f.to_string()) {
            return;
        }
        let st = stats.get(f);
        let mut n = Map::new();
        n.insert("id".into(), json!(format!("file:{f}")));
        n.insert("label".into(), json!(basename(f)));
        n.insert("kind".into(), json!("file"));
        n.insert("package".into(), json!(pkg.label(f)));
        n.insert("file".into(), json!(f));
        n.insert("symbolCount".into(), json!(st.map_or(0, |s| s.count)));
        n.insert("external".into(), json!(!local_set.contains(f)));
        if let Some(l) = st.and_then(|s| s.lang.as_ref()) {
            n.insert("lang".into(), json!(l));
        }
        nodes.set(f.to_string(), n);
    };
    for f in &local {
        ensure(&mut nodes, &stats, f);
    }

    let ids: Vec<i64> = syms.iter().filter_map(|s| as_i64(&s[1])).collect();
    let refs = ref_counts_touching(c, &ids)?;
    let known: HashSet<i64> = ids.iter().copied().collect();
    let mut cross: Vec<i64> = Vec::new();
    let mut cross_set: HashSet<i64> = HashSet::new();
    for row in &refs {
        for v in [&row[0], &row[1]] {
            if let Some(id) = as_i64(v)
                && !known.contains(&id)
                && cross_set.insert(id)
            {
                cross.push(id);
            }
        }
    }
    if !cross.is_empty() {
        for x in chunked(
            c,
            &ints(&cross),
            |p| format!("SELECT id, file FROM symbols WHERE id IN ({p})"),
            &[],
        )? {
            let (Some(id), Some(f)) = (as_i64(&x[0]), as_str(&x[1])) else {
                continue;
            };
            sym_file.insert(id, f.to_string());
            stats.entry(f.to_string()).or_insert_with(|| FileStat {
                count: 0,
                lang: Some(lang_of(f)),
            });
        }
    }

    let mut em = EdgeMap::new();
    for row in &refs {
        let ct = as_str(&row[2]).unwrap_or("");
        if ct == "import" {
            continue;
        }
        let a = as_i64(&row[0]).and_then(|i| sym_file.get(&i)).cloned();
        let b = as_i64(&row[1]).and_then(|i| sym_file.get(&i)).cloned();
        let (Some(a), Some(b)) = (a, b) else { continue };
        if a.is_empty() || b.is_empty() || a == b {
            continue;
        }
        if !local_set.contains(&a) && !local_set.contains(&b) {
            continue;
        }
        ensure(&mut nodes, &stats, &a);
        ensure(&mut nodes, &stats, &b);
        add_edge(&mut em, a, b, ct, as_i64(&row[3]).unwrap_or(0));
    }
    for row in chunked(
        c,
        &ints(&ids),
        |p| {
            format!(
                "SELECT r.from_id, COALESCE(r.to_file, st.file) AS to_file, COUNT(*) AS n
         FROM refs r
         LEFT JOIN symbols st ON st.id = r.to_id
         WHERE +r.call_type = 'import'
           AND COALESCE(r.to_file, st.file) IS NOT NULL
           AND r.from_id IN ({p})
         GROUP BY r.from_id, COALESCE(r.to_file, st.file)"
            )
        },
        &[],
    )? {
        let Some(a) = as_i64(&row[0]).and_then(|i| sym_file.get(&i)).cloned() else {
            continue;
        };
        if a.is_empty() || !local_set.contains(&a) {
            continue;
        }
        let b = as_str(&row[1]).unwrap_or("").to_string();
        if b.is_empty() || a == b {
            continue;
        }
        if !stats.contains_key(&b) {
            let lang = lang_of(&b);
            stats.insert(
                b.clone(),
                FileStat {
                    count: 0,
                    lang: Some(lang),
                },
            );
        }
        ensure(&mut nodes, &stats, &a);
        ensure(&mut nodes, &stats, &b);
        add_edge(&mut em, a, b, "import", as_i64(&row[2]).unwrap_or(0));
    }
    let edges = materialize(em, "file");
    let mut out: Vec<Map<String, Value>> = nodes.entries.into_iter().map(|(_, n)| n).collect();
    decorate(c, &mut out, &pkg)?;
    check_memory_sources(args, &out)?;
    Ok(graph(out, edges))
}

// ─── symbolGraph ─────────────────────────────────────────────────────────────

/// Columns: id, name, kind, lang, file, line, signature, scope.
const SYMBOL_COLS: &str = "id, name, kind, lang, file, line, signature, scope";

pub fn symbol_graph(r: &Reader, args: &Value) -> Result<Value, QueryError> {
    let filter = required_str(args, "fileFilter")?;
    let c = &r.conn;
    let files = resolve_indexed_files(c, filter)?;
    if files.is_empty() {
        return Ok(empty_graph());
    }
    let mut syms = chunked(
        c,
        &texts(&files),
        |p| format!("SELECT {SYMBOL_COLS} FROM symbols WHERE file IN ({p})"),
        &[],
    )?;
    let line_of = |s: &Row| as_i64(&s[5]).unwrap_or(0);
    let id_of = |s: &Row| as_i64(&s[0]).unwrap_or(0);
    syms.sort_by(|a, b| line_of(a).cmp(&line_of(b)).then(id_of(a).cmp(&id_of(b))));
    if syms.is_empty() {
        return Ok(empty_graph());
    }
    let mut by_id: HashMap<i64, Row> = HashMap::new();
    let mut related: Vec<i64> = Vec::new();
    let mut related_set: HashSet<i64> = HashSet::new();
    for s in &syms {
        by_id.insert(id_of(s), s.clone());
        if related_set.insert(id_of(s)) {
            related.push(id_of(s));
        }
    }
    let ids: Vec<i64> = syms.iter().map(id_of).collect();
    let mut em = EdgeMap::new();
    for row in ref_counts_touching(c, &ids)? {
        let (Some(from), Some(to)) = (as_i64(&row[0]), as_i64(&row[1])) else {
            continue;
        };
        for id in [from, to] {
            if related_set.insert(id) {
                related.push(id);
            }
        }
        add_edge(
            &mut em,
            id_key(&row[0]),
            id_key(&row[1]),
            as_str(&row[2]).unwrap_or(""),
            as_i64(&row[3]).unwrap_or(0),
        );
    }
    let edges = materialize(em, "sym");
    let loaded: HashSet<i64> = ids.iter().copied().collect();
    let missing: Vec<i64> = related
        .iter()
        .copied()
        .filter(|i| !loaded.contains(i))
        .collect();
    if !missing.is_empty() {
        for s in chunked(
            c,
            &ints(&missing),
            |p| format!("SELECT {SYMBOL_COLS} FROM symbols WHERE id IN ({p})"),
            &[],
        )? {
            by_id.insert(id_of(&s), s);
        }
    }
    let pkg = Labeller::load(c)?;
    let file_of = |s: &Row| as_str(&s[4]).unwrap_or("").to_string();
    let local_files: HashSet<String> = syms.iter().map(|s| file_of(s).replace('\\', "/")).collect();
    let mut matched: Vec<(&Row, bool)> = related
        .iter()
        .filter_map(|i| by_id.get(i))
        .map(|s| (s, !local_files.contains(&file_of(s).replace('\\', "/"))))
        .collect();
    matched.sort_by(|(a, ae), (b, be)| {
        ae.cmp(be)
            .then_with(|| locale_cmp(&file_of(a), &file_of(b)))
            .then(line_of(a).cmp(&line_of(b)))
            .then(id_of(a).cmp(&id_of(b)))
    });
    let mut nodes: Vec<Map<String, Value>> = matched
        .into_iter()
        .map(|(s, ext)| {
            let Value::Object(m) = json!({
                "id": format!("sym:{}", id_key(&s[0])),
                "label": to_json(&s[1]),
                "kind": "symbol",
                "symbolId": to_json(&s[0]),
                "symbolKind": to_json(&s[2]),
                "file": to_json(&s[4]),
                "package": pkg.label(&file_of(s)),
                "lang": to_json(&s[3]),
                "line": to_json(&s[5]),
                "signature": to_json(&s[6]),
                "scope": to_json(&s[7]),
                "external": ext,
            }) else {
                unreachable!()
            };
            m
        })
        .collect();
    decorate(c, &mut nodes, &pkg)?;
    check_memory_sources(args, &nodes)?;
    Ok(graph(nodes, edges))
}

// ─── incomingCalls / outgoingCalls ───────────────────────────────────────────

const SEL: &str = "s.id   AS sym_id,
         s.name AS sym_name,
         s.kind AS sym_kind,
         s.lang AS sym_lang,
         s.file AS sym_file,
         s.line AS sym_line,
         s.signature AS sym_signature";

const NOT_BOUND: &str = "(r.to_file IS NULL OR r.to_file <> '')
         AND NOT (r.call_type = 'import' AND r.module IS NOT NULL
                  AND r.lang IN (SELECT lang FROM lang_family WHERE family = 'js'))";

/// A `CallSiteRow` (SEL columns, `call_type`, `ref_line`, optional `depth`).
struct CallRow {
    v: Row,
    depth: i64,
}

impl CallRow {
    fn from_row(mut v: Row) -> Self {
        let depth = if v.len() > 9 {
            as_i64(&v[9]).filter(|d| *d != 0).unwrap_or(1)
        } else {
            1
        };
        v.truncate(9);
        Self { v, depth }
    }
    fn id(&self) -> i64 {
        as_i64(&self.v[0]).unwrap_or(0)
    }
    fn name(&self) -> &str {
        as_str(&self.v[1]).unwrap_or("")
    }
    fn file(&self) -> &str {
        as_str(&self.v[4]).unwrap_or("")
    }
    fn line(&self) -> i64 {
        as_i64(&self.v[5]).unwrap_or(0)
    }
    fn ref_line(&self) -> i64 {
        as_i64(&self.v[8]).unwrap_or(0)
    }
    fn to_call(&self) -> Value {
        json!({
            "symbol": {
                "id": to_json(&self.v[0]),
                "name": to_json(&self.v[1]),
                "kind": to_json(&self.v[2]),
                "lang": to_json(&self.v[3]),
                "file": to_json(&self.v[4]),
                "line": to_json(&self.v[5]),
                "signature": to_json(&self.v[6]),
            },
            "callType": to_json(&self.v[7]),
            "line": to_json(&self.v[8]),
        })
    }
}

type RowCmp = fn(&CallRow, &CallRow) -> Ordering;

/// `compareCallSiteRows`.
fn cs_cmp(a: &CallRow, b: &CallRow) -> Ordering {
    a.ref_line()
        .cmp(&b.ref_line())
        .then_with(|| js_cmp(a.file(), b.file()))
        .then(a.line().cmp(&b.line()))
        .then(a.id().cmp(&b.id()))
}

fn tree_cmp(a: &CallRow, b: &CallRow) -> Ordering {
    a.depth.cmp(&b.depth).then_with(|| cs_cmp(a, b))
}

fn call_rows(rows: Vec<Row>) -> Vec<CallRow> {
    rows.into_iter().map(CallRow::from_row).collect()
}

/// Service-layer limit: finite number (else 100), truncated, at least 1.
fn service_limit(args: &Value) -> usize {
    let raw = args
        .get("limit")
        .and_then(Value::as_f64)
        .filter(|f| f.is_finite())
        .unwrap_or(100.0);
    let t = raw.trunc().max(1.0);
    if t >= usize::MAX as f64 {
        usize::MAX
    } else {
        t as usize
    }
}

fn resolve_symbol_ids(
    c: &Connection,
    name: &str,
    file: Option<&str>,
) -> Result<Vec<i64>, QueryError> {
    let file = file.filter(|f| !f.is_empty());
    let Some(file) = file else {
        return Ok(query(
            c,
            "SELECT id FROM symbols WHERE name = ? ORDER BY id",
            &[Sql::Text(name.to_string())],
        )?
        .iter()
        .filter_map(|r| as_i64(&r[0]))
        .collect());
    };
    let files = resolve_indexed_files(c, file)?;
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let files = texts(&files);
    let mut ids = Vec::new();
    let mut cur = 0;
    for take in in_chunks(files.len(), MAX_VARS) {
        let fs = pad(&files[cur..cur + take]);
        cur += take;
        let sql = format!(
            "SELECT id FROM symbols WHERE name = ? AND file IN ({}) ORDER BY id",
            placeholders(fs.len())
        );
        let mut params = vec![Sql::Text(name.to_string())];
        params.extend(fs);
        ids.extend(
            query(c, &sql, &params)?
                .iter()
                .filter_map(|r| as_i64(&r[0])),
        );
    }
    Ok(ids)
}

fn direct_incoming(
    c: &Connection,
    match_ids: &[i64],
    scoped: Option<&[i64]>,
) -> Result<Vec<CallRow>, QueryError> {
    let target_files: Vec<Sql> = match scoped {
        None => Vec::new(),
        Some(s) => chunked(
            c,
            &ints(s),
            |p| format!("SELECT DISTINCT file FROM symbols WHERE id IN ({p})"),
            &[],
        )?
        .into_iter()
        .map(|mut r| r.swap_remove(0))
        .collect(),
    };
    let refine = !target_files.is_empty() && target_files.len() <= 64;
    let evidence = if refine {
        let fph = placeholders(target_files.len());
        format!(
            "AND (r.to_file IS NULL OR r.to_file IN ({fph}))
       AND NOT EXISTS (
         SELECT 1 FROM refs ri JOIN symbols si ON si.id = ri.from_id
         WHERE si.file = s.file AND ri.call_type = 'import' AND ri.to_name = r.to_name
           AND ri.to_file IS NOT NULL AND ri.to_file NOT IN ({fph})
       )"
        )
    } else {
        String::new()
    };
    let extra: Vec<Sql> = if refine {
        target_files
            .iter()
            .chain(target_files.iter())
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let rows = chunked(
        c,
        &ints(match_ids),
        |p| {
            format!(
                "SELECT
         {SEL},
         r.call_type,
         r.line AS ref_line
       FROM refs r
       JOIN symbols s ON s.id = r.from_id
       WHERE r.to_id IN ({p})
       {evidence}
       ORDER BY r.line, r.id"
            )
        },
        &extra,
    )?;
    Ok(call_rows(rows))
}

/// Drop `<module>` import rows for files that also contribute a real caller.
fn drop_module_rows(rows: Vec<CallRow>) -> Vec<CallRow> {
    let is_module = |r: &CallRow| {
        r.name() == MODULE_SYMBOL && matches!(as_str(&r.v[7]), Some("import") | Some(""))
    };
    let used: HashSet<String> = rows
        .iter()
        .filter(|r| !is_module(r))
        .map(|r| r.file().to_string())
        .collect();
    rows.into_iter()
        .filter(|r| !is_module(r) || !used.contains(r.file()))
        .collect()
}

fn unbound_fallback(c: &Connection, name: &str) -> Result<Vec<CallRow>, QueryError> {
    let sql = format!(
        "SELECT
         {SEL},
         r.call_type,
         r.line AS ref_line
       FROM refs r
       JOIN symbols s ON s.id = r.from_id
       WHERE r.to_id IS NULL AND r.to_name = ?
         AND {NOT_BOUND}
       ORDER BY r.line, r.id"
    );
    Ok(call_rows(query(c, &sql, &[Sql::Text(name.to_string())])?))
}

fn unresolved_count(c: &Connection, ids: &[i64]) -> Result<i64, QueryError> {
    let ids = ints(ids);
    let mut total = 0;
    let mut cur = 0;
    for take in in_chunks(ids.len(), MAX_VARS) {
        let ch = pad(&ids[cur..cur + take]);
        cur += take;
        let sql = format!(
            "SELECT COUNT(*) AS n FROM refs WHERE from_id IN ({}) AND to_id IS NULL",
            placeholders(ch.len())
        );
        total += query(c, &sql, &ch)?
            .first()
            .and_then(|r| as_i64(&r[0]))
            .unwrap_or(0);
    }
    Ok(total)
}

/// `annotateDirectEdges`: first-hop rows take the best direct edge (a real
/// call beats an import, then the earliest line).
fn annotate_direct_edges(
    c: &Connection,
    rows: &mut [CallRow],
    seeds: &[i64],
    seed_col: &str,
    other_col: &str,
) -> Result<(), QueryError> {
    if rows.is_empty() {
        return Ok(());
    }
    let edges = chunked(
        c,
        &ints(seeds),
        |p| {
            format!(
                "SELECT {other_col} AS sym_id, call_type, line
       FROM refs
       WHERE {seed_col} IN ({p}) AND {other_col} IS NOT NULL"
            )
        },
        &[],
    )?;
    let mut best: HashMap<i64, (Sql, Sql)> = HashMap::new();
    for e in edges {
        let Some(id) = as_i64(&e[0]) else { continue };
        let e_import = as_str(&e[1]) == Some("import");
        let replace = match best.get(&id) {
            None => true,
            Some((ct, line)) => {
                let c_import = as_str(ct) == Some("import");
                (c_import && !e_import)
                    || (c_import == e_import
                        && as_i64(&e[2]).unwrap_or(0) < as_i64(line).unwrap_or(0))
            }
        };
        if replace {
            best.insert(id, (e[1].clone(), e[2].clone()));
        }
    }
    for r in rows.iter_mut() {
        if let Some((ct, line)) = best.get(&r.id()) {
            r.v[7] = ct.clone();
            r.v[8] = line.clone();
        }
    }
    Ok(())
}

/// `runCteWithSeeds`. Above 900 seeds WrongStack loads a temp table; the
/// reader is query-only, so the same seed set is fed through `json_each`
/// instead. Result rows are unique per symbol and re-sorted by a total
/// order afterwards, so the seed source does not affect the output.
fn run_cte(
    c: &Connection,
    seeds: &[i64],
    build: impl Fn(&str) -> String,
) -> Result<Vec<CallRow>, QueryError> {
    let rows = if seeds.len() <= MAX_VARS {
        query(c, &build(&placeholders(seeds.len())), &ints(seeds))?
    } else {
        let list = serde_json::to_string(seeds).unwrap_or_else(|_| "[]".into());
        query(
            c,
            &build("SELECT value FROM json_each(?)"),
            &[Sql::Text(list)],
        )?
    };
    Ok(call_rows(rows))
}

fn incoming_not_found() -> Value {
    json!({ "calls": [], "symbolFound": false, "ambiguous": false, "totalMatches": 0 })
}

fn outgoing_not_found() -> Value {
    json!({ "calls": [], "symbolFound": false, "unresolvedCount": 0, "totalMatches": 0 })
}

fn file_arg(args: &Value) -> Option<&str> {
    args.get("file").and_then(Value::as_str)
}

fn finish(mut rows: Vec<CallRow>, cmp: RowCmp, limit: usize) -> (Vec<Value>, usize) {
    rows.sort_by(cmp);
    let total = rows.len();
    let calls = rows.iter().take(limit).map(CallRow::to_call).collect();
    (calls, total)
}

pub fn incoming_calls(r: &Reader, args: &Value) -> Result<Value, QueryError> {
    let name = required_str(args, "symbol")?;
    let file = file_arg(args);
    let limit = service_limit(args);
    let transitive = truthy(args.get("transitive"));
    let c = &r.conn;

    let targets = resolve_symbol_ids(c, name, file)?;
    if targets.is_empty() {
        return Ok(incoming_not_found());
    }
    let mut match_ids = targets.clone();
    let mut ambiguous = false;
    if file.is_some() {
        let all = resolve_symbol_ids(c, name, None)?;
        if all.len() > targets.len() {
            match_ids = all;
            ambiguous = true;
        }
    }
    let no_file = file.is_none_or(str::is_empty);

    let (rows, cmp): (Vec<CallRow>, RowCmp) = if !transitive {
        let mut rows = direct_incoming(c, &match_ids, ambiguous.then_some(&targets[..]))?;
        if no_file {
            rows.extend(unbound_fallback(c, name)?);
        }
        (rows, cs_cmp)
    } else {
        let anchors: Option<Vec<i64>> = if ambiguous {
            let mut seen = HashSet::new();
            Some(
                direct_incoming(c, &match_ids, Some(&targets))?
                    .iter()
                    .map(CallRow::id)
                    .filter(|id| seen.insert(*id))
                    .collect(),
            )
        } else {
            None
        };
        let anchored = anchors.is_some();
        let build = |src: &str| {
            let anchor = if anchored {
                format!("SELECT s0.id, 1 FROM symbols s0 WHERE s0.id IN ({src})")
            } else {
                format!(
                    "SELECT r.from_id, 1
       FROM refs r
       WHERE r.to_id IN ({src})"
                )
            };
            format!(
                "WITH RECURSIVE incoming_tree(from_id, depth) AS (
       {anchor}

       UNION

       SELECT r.from_id, it.depth + 1
       FROM refs r
       JOIN incoming_tree it ON r.to_id = it.from_id
       WHERE it.depth < 3
     )
     SELECT
       {SEL},
       '' AS call_type,
       0 AS ref_line,
       MIN(it.depth) AS depth
     FROM incoming_tree it
     JOIN symbols s ON s.id = it.from_id
     GROUP BY s.id
     ORDER BY s.file, s.line"
            )
        };
        let mut rows = match &anchors {
            Some(a) if a.is_empty() => Vec::new(),
            Some(a) => run_cte(c, a, build)?,
            None => run_cte(c, &match_ids, build)?,
        };
        annotate_direct_edges(c, &mut rows, &match_ids, "to_id", "from_id")?;
        if no_file {
            rows.extend(unbound_fallback(c, name)?);
        }
        (rows, tree_cmp)
    };
    let (calls, total) = finish(drop_module_rows(rows), cmp, limit);
    Ok(json!({
        "calls": calls,
        "symbolFound": true,
        "ambiguous": ambiguous,
        "totalMatches": total,
    }))
}

pub fn outgoing_calls(r: &Reader, args: &Value) -> Result<Value, QueryError> {
    let name = required_str(args, "symbol")?;
    let file = file_arg(args);
    let limit = service_limit(args);
    let transitive = truthy(args.get("transitive"));
    let c = &r.conn;

    let sources = resolve_symbol_ids(c, name, file)?;
    if sources.is_empty() {
        return Ok(outgoing_not_found());
    }
    let unresolved = unresolved_count(c, &sources)?;
    let (rows, cmp): (Vec<CallRow>, RowCmp) = if !transitive {
        let rows = chunked(
            c,
            &ints(&sources),
            |p| {
                format!(
                    "SELECT
         {SEL},
         r.call_type,
         r.line AS ref_line
       FROM refs r
       JOIN symbols s ON s.id = r.to_id
       WHERE r.from_id IN ({p})
         AND r.to_id IS NOT NULL
       ORDER BY r.line, r.id"
                )
            },
            &[],
        )?;
        (call_rows(rows), cs_cmp)
    } else {
        let mut rows = run_cte(c, &sources, |src| {
            format!(
                "WITH RECURSIVE outgoing_tree(to_id, depth) AS (
       SELECT r.to_id, 1
       FROM refs r
       WHERE r.from_id IN ({src}) AND r.to_id IS NOT NULL

       UNION

       SELECT r.to_id, ot.depth + 1
       FROM refs r
       JOIN outgoing_tree ot ON r.from_id = ot.to_id
       WHERE r.to_id IS NOT NULL AND ot.depth < 3
     )
     SELECT
       {SEL},
       '' AS call_type,
       0 AS ref_line,
       MIN(ot.depth) AS depth
     FROM outgoing_tree ot
     JOIN symbols s ON s.id = ot.to_id
     GROUP BY s.id
     ORDER BY s.file, s.line"
            )
        })?;
        annotate_direct_edges(c, &mut rows, &sources, "from_id", "to_id")?;
        (rows, tree_cmp)
    };
    let (calls, total) = finish(rows, cmp, limit);
    Ok(json!({
        "calls": calls,
        "symbolFound": true,
        "unresolvedCount": unresolved,
        "totalMatches": total,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_ladder_matches_wrongstack() {
        assert_eq!(in_chunks(0, 900), Vec::<usize>::new());
        assert_eq!(in_chunks(3, 900), vec![3]);
        assert_eq!(in_chunks(512, 900), vec![512]);
        assert_eq!(in_chunks(513, 900), vec![512, 1]);
        assert_eq!(in_chunks(1300, 900), vec![512, 512, 256, 16, 4]);
        assert_eq!(pad(&ints(&[1, 2, 3])).len(), 4);
    }

    #[test]
    fn js_string_order_is_utf16() {
        // U+FF5E (BMP) sorts after U+1F600 (surrogate pair) in UTF-16.
        assert_eq!(js_cmp("\u{ff5e}", "\u{1f600}"), Ordering::Greater);
        assert_eq!(js_cmp("a", "b"), Ordering::Less);
    }

    #[test]
    fn layout_packages() {
        assert_eq!(
            derive_package_from_layout(r"C:\r\packages\tools\src\a.ts").as_deref(),
            Some("@wrongstack/tools")
        );
        assert_eq!(
            derive_package_from_layout("/r/apps/web/x.ts").as_deref(),
            Some("app:web")
        );
        assert_eq!(derive_package_from_layout("/r/src/x.ts"), None);
    }

    #[test]
    fn service_limit_like_js() {
        assert_eq!(service_limit(&json!({})), 100);
        assert_eq!(service_limit(&json!({"limit": 2.9})), 2);
        assert_eq!(service_limit(&json!({"limit": -5})), 1);
        assert_eq!(service_limit(&json!({"limit": "3"})), 100);
    }
}
