//! SAGE anchor healing (WI-512): WrongStack's indexer, when the project has
//! `.wrongstack/memories`, compares the symbols an in-place run removed with
//! the ones it wrote (`detectMovedOrRenamedSymbols`) and rewrites the anchors
//! of active SAGE memories that pointed at a moved or renamed symbol
//! (`remapAnchoredSageMemories`, `ecosystem-bridge.ts`), in `sage.db` and
//! `memories.jsonl`.
//!
//! The rewritten records must be the bytes WrongStack writes, so records go
//! through a small JSON model with `JSON.parse`/`JSON.stringify` semantics:
//! property order (array-index keys first, then insertion order; a repeated
//! key keeps its first position), UTF-16 strings (a lone surrogate survives),
//! JS number formatting. Best effort, as WrongStack's: a store that cannot be
//! opened or a record that cannot be parsed is left as it is.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::js::{is_space, is_word};
use crate::paths::{index_posix, js_trim, node_is_absolute, node_relative};

// ─── JSON with JavaScript semantics ──────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Num(f64),
    Str(Vec<u16>),
    Arr(Vec<J>),
    /// Properties in creation order; see [`stringify`] for output order.
    Obj(Vec<(Vec<u16>, J)>),
}

impl J {
    fn get(&self, key: &str) -> Option<&J> {
        let k: Vec<u16> = key.encode_utf16().collect();
        match self {
            J::Obj(props) => props.iter().find(|(p, _)| *p == k).map(|(_, v)| v),
            _ => None,
        }
    }

    /// `obj[key] = value`: an existing property keeps its position.
    fn set(&mut self, key: &str, value: J) {
        let k: Vec<u16> = key.encode_utf16().collect();
        if let J::Obj(props) = self {
            match props.iter_mut().find(|(p, _)| *p == k) {
                Some((_, v)) => *v = value,
                None => props.push((k, value)),
            }
        }
    }
}

/// `JSON.parse(text)`; `None` where it would throw.
pub fn parse(text: &str) -> Option<J> {
    let s: Vec<char> = text.chars().collect();
    let mut p = Parser { s: &s, i: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    (p.i == s.len()).then_some(v)
}

struct Parser<'a> {
    s: &'a [char],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], ' ' | '\t' | '\n' | '\r') {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: char) -> bool {
        if self.s.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn lit(&mut self, word: &str) -> bool {
        let w: Vec<char> = word.chars().collect();
        if self.s[self.i..].starts_with(&w) {
            self.i += w.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Option<J> {
        // V8 parses far deeper; a record nested this deep is not a memory.
        if depth > 512 {
            return None;
        }
        match *self.s.get(self.i)? {
            '{' => {
                self.i += 1;
                let mut props: Vec<(Vec<u16>, J)> = Vec::new();
                self.ws();
                if self.eat('}') {
                    return Some(J::Obj(props));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    if !self.eat(':') {
                        return None;
                    }
                    self.ws();
                    let v = self.value(depth + 1)?;
                    match props.iter_mut().find(|(p, _)| *p == k) {
                        Some((_, old)) => *old = v,
                        None => props.push((k, v)),
                    }
                    self.ws();
                    if self.eat(',') {
                        continue;
                    }
                    if self.eat('}') {
                        return Some(J::Obj(props));
                    }
                    return None;
                }
            }
            '[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.eat(']') {
                    return Some(J::Arr(items));
                }
                loop {
                    self.ws();
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    if self.eat(',') {
                        continue;
                    }
                    if self.eat(']') {
                        return Some(J::Arr(items));
                    }
                    return None;
                }
            }
            '"' => self.string().map(J::Str),
            't' => self.lit("true").then_some(J::Bool(true)),
            'f' => self.lit("false").then_some(J::Bool(false)),
            'n' => self.lit("null").then_some(J::Null),
            _ => self.number(),
        }
    }

    fn string(&mut self) -> Option<Vec<u16>> {
        if !self.eat('"') {
            return None;
        }
        let mut out: Vec<u16> = Vec::new();
        loop {
            let c = *self.s.get(self.i)?;
            self.i += 1;
            match c {
                '"' => return Some(out),
                '\\' => {
                    let e = *self.s.get(self.i)?;
                    self.i += 1;
                    match e {
                        '"' => out.push(0x22),
                        '\\' => out.push(0x5C),
                        '/' => out.push(0x2F),
                        'b' => out.push(0x08),
                        'f' => out.push(0x0C),
                        'n' => out.push(0x0A),
                        'r' => out.push(0x0D),
                        't' => out.push(0x09),
                        'u' => {
                            let hex: String = self.s.get(self.i..self.i + 4)?.iter().collect();
                            if !hex.chars().all(|h| h.is_ascii_hexdigit()) {
                                return None;
                            }
                            out.push(u16::from_str_radix(&hex, 16).ok()?);
                            self.i += 4;
                        }
                        _ => return None,
                    }
                }
                c if (c as u32) < 0x20 => return None,
                c => {
                    let mut buf = [0u16; 2];
                    out.extend_from_slice(c.encode_utf16(&mut buf));
                }
            }
        }
    }

    fn number(&mut self) -> Option<J> {
        let start = self.i;
        self.eat('-');
        let digits = |p: &mut Self| {
            let s = p.i;
            while p.s.get(p.i).is_some_and(char::is_ascii_digit) {
                p.i += 1;
            }
            p.i - s
        };
        if self.eat('0') {
            // no further integer digits
        } else if digits(self) == 0 {
            return None;
        }
        if self.eat('.') && digits(self) == 0 {
            return None;
        }
        if self.eat('e') || self.eat('E') {
            if !self.eat('+') {
                self.eat('-');
            }
            if digits(self) == 0 {
                return None;
            }
        }
        let text: String = self.s[start..self.i].iter().collect();
        text.parse::<f64>().ok().map(J::Num)
    }
}

/// An array-index property key (`"0"` .. `"4294967294"`, canonical form).
fn index_key(k: &[u16]) -> Option<u32> {
    if k.is_empty() || k.len() > 10 || !k.iter().all(|&c| (0x30..=0x39).contains(&c)) {
        return None;
    }
    if k.len() > 1 && k[0] == 0x30 {
        return None;
    }
    let s = String::from_utf16(k).ok()?;
    s.parse::<u64>()
        .ok()
        .filter(|&n| n < 4_294_967_295)
        .map(|n| n as u32)
}

/// `Number.prototype.toString()` (what `JSON.stringify` writes).
pub fn js_number_string(f: f64) -> String {
    if !f.is_finite() {
        return "null".into();
    }
    if f == 0.0 {
        return "0".into();
    }
    // Rust's `{:e}` gives the shortest round-trip digits, like JS.
    let e = format!("{:e}", f.abs());
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let digits: String = mant.chars().filter(char::is_ascii_digit).collect();
    let k = digits.len() as i32;
    let n = exp.parse::<i32>().unwrap_or(0) + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let sign = if n - 1 < 0 { '-' } else { '+' };
        let m = if k == 1 {
            digits.clone()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!("{m}e{sign}{}", (n - 1).abs())
    };
    if f < 0.0 { format!("-{body}") } else { body }
}

fn quote(s: &[u16], out: &mut String) {
    out.push('"');
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        match c {
            0x08 => out.push_str("\\b"),
            0x09 => out.push_str("\\t"),
            0x0A => out.push_str("\\n"),
            0x0C => out.push_str("\\f"),
            0x0D => out.push_str("\\r"),
            0x22 => out.push_str("\\\""),
            0x5C => out.push_str("\\\\"),
            c if c < 0x20 => out.push_str(&format!("\\u{c:04x}")),
            0xD800..=0xDBFF if s.get(i + 1).is_some_and(|l| (0xDC00..=0xDFFF).contains(l)) => {
                out.push_str(&String::from_utf16_lossy(&s[i..i + 2]));
                i += 1;
            }
            0xD800..=0xDFFF => out.push_str(&format!("\\u{c:04x}")),
            c => out.push(char::from_u32(u32::from(c)).unwrap_or('\u{fffd}')),
        }
        i += 1;
    }
    out.push('"');
}

/// `JSON.stringify(value)` (no indentation).
pub fn stringify(v: &J) -> String {
    let mut out = String::new();
    write(v, &mut out);
    out
}

fn write(v: &J, out: &mut String) {
    match v {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Num(f) => out.push_str(&js_number_string(*f)),
        J::Str(s) => quote(s, out),
        J::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        J::Obj(props) => {
            let mut indexed: Vec<(u32, &(Vec<u16>, J))> = props
                .iter()
                .filter_map(|p| index_key(&p.0).map(|n| (n, p)))
                .collect();
            indexed.sort_by_key(|(n, _)| *n);
            let ordered = indexed
                .into_iter()
                .map(|(_, p)| p)
                .chain(props.iter().filter(|p| index_key(&p.0).is_none()));
            out.push('{');
            for (i, (k, v)) in ordered.enumerate() {
                if i > 0 {
                    out.push(',');
                }
                quote(k, out);
                out.push(':');
                write(v, out);
            }
            out.push('}');
        }
    }
}

// ─── detectMovedOrRenamedSymbols ─────────────────────────────────────────────

/// `SymbolSnapshotItem` (the fields the detection reads).
#[derive(Debug, Clone)]
pub struct Snap {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Move {
    pub old_file: String,
    pub old_symbol: String,
    pub new_file: String,
    pub new_symbol: String,
}

/// `normalizeAnonSignature`: the name as a whole word becomes `_`
/// (`\b` over `[A-Za-z0-9_]`, UTF-16 units), whitespace runs one space,
/// trimmed.
pub fn normalize_anon_signature(name: &str, signature: Option<&str>) -> String {
    let Some(sig) = signature.filter(|s| !s.is_empty()) else {
        return String::new();
    };
    let s: Vec<u16> = sig.encode_utf16().collect();
    let n: Vec<u16> = name.encode_utf16().collect();
    let word_at = |i: usize| i < s.len() && is_word(s[i]);
    let boundary = |i: usize| (i > 0 && is_word(s[i - 1])) != word_at(i);
    let mut out: Vec<u16> = Vec::with_capacity(s.len());
    let mut i = 0;
    if n.is_empty() {
        for p in 0..=s.len() {
            if boundary(p) {
                out.push(u16::from(b'_'));
            }
            if p < s.len() {
                out.push(s[p]);
            }
        }
    } else {
        while i < s.len() {
            if i + n.len() <= s.len()
                && s[i..i + n.len()] == n[..]
                && boundary(i)
                && boundary(i + n.len())
            {
                out.push(u16::from(b'_'));
                i += n.len();
            } else {
                out.push(s[i]);
                i += 1;
            }
        }
    }
    // /\s+/g -> ' '
    let mut collapsed: Vec<u16> = Vec::with_capacity(out.len());
    let mut in_run = false;
    for c in out {
        if is_space(c) {
            if !in_run {
                collapsed.push(0x20);
                in_run = true;
            }
        } else {
            collapsed.push(c);
            in_run = false;
        }
    }
    js_trim(&String::from_utf16_lossy(&collapsed)).to_string()
}

/// `detectMovedOrRenamedSymbols(prev, next)`.
pub fn detect_moves(prev: &[Snap], next: &[Snap]) -> Vec<Move> {
    use std::collections::{HashMap, HashSet};
    let key = |s: &Snap| format!("{}::{}::{}", index_posix(&s.file), s.kind, s.name);
    let next_keys: HashSet<String> = next.iter().map(key).collect();
    let prev_keys: HashSet<String> = prev.iter().map(key).collect();
    let removed: Vec<&Snap> = prev
        .iter()
        .filter(|s| !next_keys.contains(&key(s)))
        .collect();
    let added: Vec<&Snap> = next
        .iter()
        .filter(|s| !prev_keys.contains(&key(s)))
        .collect();
    if removed.is_empty() || added.is_empty() {
        return Vec::new();
    }
    let counts = |list: &[&Snap]| {
        let mut m: HashMap<String, usize> = HashMap::new();
        for s in list {
            *m.entry(format!("{}::{}", s.kind, s.name)).or_default() += 1;
        }
        m
    };
    let removed_counts = counts(&removed);
    let added_counts = counts(&added);
    let mut used: HashSet<usize> = HashSet::new();
    let mut out: Vec<Move> = Vec::new();
    // Pass 1: cross-file moves.
    for rem in &removed {
        let old_file = index_posix(&rem.file);
        let mut best: Option<usize> = None;
        let mut best_conf = 0.0;
        for (i, add) in added.iter().enumerate() {
            if used.contains(&i) {
                continue;
            }
            let new_file = index_posix(&add.file);
            if add.name != rem.name || add.kind != rem.kind || new_file == old_file {
                continue;
            }
            let old_sig = normalize_anon_signature(&rem.name, rem.signature.as_deref());
            let new_sig = normalize_anon_signature(&add.name, add.signature.as_deref());
            if !old_sig.is_empty() && !new_sig.is_empty() && old_sig != new_sig {
                continue;
            }
            let k = format!("{}::{}", rem.kind, rem.name);
            let unique = removed_counts.get(&k) == Some(&1) && added_counts.get(&k) == Some(&1);
            if (old_sig.is_empty() || new_sig.is_empty()) && !unique {
                continue;
            }
            let conf = if !old_sig.is_empty() && old_sig == new_sig {
                0.98
            } else {
                0.85
            };
            if conf > best_conf {
                best_conf = conf;
                best = Some(i);
            }
        }
        if let Some(i) = best {
            used.insert(i);
            out.push(Move {
                old_file,
                old_symbol: rem.name.clone(),
                new_file: index_posix(&added[i].file),
                new_symbol: added[i].name.clone(),
            });
        }
    }
    // Pass 2: renames within a file.
    for rem in &removed {
        let old_file = index_posix(&rem.file);
        if out
            .iter()
            .any(|c| c.old_file == old_file && c.old_symbol == rem.name)
        {
            continue;
        }
        let old_anon = normalize_anon_signature(&rem.name, rem.signature.as_deref());
        if old_anon.is_empty() || old_anon.encode_utf16().count() < 6 {
            continue;
        }
        for (i, add) in added.iter().enumerate() {
            if used.contains(&i) {
                continue;
            }
            let new_file = index_posix(&add.file);
            if new_file != old_file || add.kind != rem.kind {
                continue;
            }
            if normalize_anon_signature(&add.name, add.signature.as_deref()) == old_anon {
                used.insert(i);
                out.push(Move {
                    old_file: old_file.clone(),
                    old_symbol: rem.name.clone(),
                    new_file,
                    new_symbol: add.name.clone(),
                });
                break;
            }
        }
    }
    out
}

// ─── remapAnchoredSageMemories ───────────────────────────────────────────────

fn u16s(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// `applyMovesToRecord`; `None` where it would throw (the record is then
/// left as it is).
fn apply_moves(record: &mut J, moves: &[Move]) -> Option<bool> {
    if matches!(record, J::Null) {
        return None; // `record.anchors` of null throws
    }
    let Some(J::Arr(anchors)) = record.get("anchors").cloned() else {
        return Some(false);
    };
    if anchors.is_empty() {
        return Some(false);
    }
    let mut anchors = anchors;
    let mut mutated = false;
    for anchor in &mut anchors {
        let (path, symbol) = match anchor {
            J::Null => return None,
            J::Obj(_) => (anchor.get("path").cloned(), anchor.get("symbol").cloned()),
            _ => (None, None),
        };
        let a_path: Option<String> = match path {
            None | Some(J::Null) | Some(J::Bool(false)) => None,
            Some(J::Num(n)) if n == 0.0 || n.is_nan() => None,
            Some(J::Str(s)) if s.is_empty() => None,
            Some(J::Str(s)) => Some(index_posix(&String::from_utf16_lossy(&s))),
            Some(_) => return None, // a truthy non-string throws
        };
        let a_sym: Option<String> = match symbol {
            None | Some(J::Null) => None,
            Some(J::Str(s)) => Some(js_trim(&String::from_utf16_lossy(&s)).to_string()),
            Some(_) => return None,
        };
        for m in moves {
            if a_path.as_deref() == Some(m.old_file.as_str())
                && a_sym.as_deref() == Some(m.old_symbol.as_str())
            {
                anchor.set("path", J::Str(u16s(&m.new_file)));
                anchor.set("symbol", J::Str(u16s(&m.new_symbol)));
                mutated = true;
                break;
            }
        }
    }
    if mutated {
        record.set("anchors", J::Arr(anchors));
    }
    Some(mutated)
}

/// What a heal changed.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Remapped {
    pub updated_sqlite_count: usize,
    pub updated_jsonl_count: usize,
}

/// The link-free real path of `p` when it stays inside the checkout
/// (`project_root`, or the root with its own links resolved); `None`
/// otherwise. Links are resolved by reading their targets, never opened
/// (security review WI-604), so nothing outside the checkout is written.
fn contained(project_root: &Path, p: &Path) -> Option<std::path::PathBuf> {
    use wrongindex_storage::git_containment::resolve_links_locally;
    use wrongindex_storage::wrongstack_paths::node_resolve;
    let real = resolve_links_locally(p).ok()?;
    let key = |p: &Path| {
        let s = node_resolve(p).to_string_lossy().into_owned();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    let f = key(&real);
    let sep = std::path::MAIN_SEPARATOR;
    let roots = [
        Some(project_root.to_path_buf()),
        resolve_links_locally(project_root).ok(),
    ];
    let inside = roots.iter().flatten().any(|root| {
        let r = key(root);
        f.starts_with(&format!("{}{sep}", r.trim_end_matches(sep)))
    });
    inside.then_some(real)
}

/// `remapAnchoredSageMemories(projectRoot, moves)` for the checkout at
/// `project_root` (its `.wrongstack/memories`). Each store is vetted right
/// before it is opened: a store (or an SQLite journal beside it) that is a
/// link, or that resolves outside the checkout, is left alone.
pub fn remap(project_root: &Path, moves: &[Move]) -> Remapped {
    let mut r = Remapped::default();
    let memories = project_root.join(".wrongstack").join("memories");
    if moves.is_empty() || !memories.is_dir() {
        return r;
    }
    let is_link = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
    let db = contained(project_root, &memories.join("sage.db")).filter(|db| {
        ["-wal", "-shm", "-journal"].iter().all(|suffix| {
            let mut s = db.as_os_str().to_owned();
            s.push(suffix);
            !is_link(Path::new(&s))
        })
    });
    if let Some(db) = db.filter(|db| db.is_file()) {
        let _ = (|| -> rusqlite::Result<()> {
            let conn = Connection::open_with_flags(
                &db,
                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            conn.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
            conn.pragma_update(None, "trusted_schema", false)?;
            conn.pragma_update(None, "busy_timeout", 2000)?;
            let rows: Vec<(rusqlite::types::Value, rusqlite::types::Value)> = conn
                .prepare("SELECT id, data FROM memories WHERE status = 'active'")?
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let mut update = conn.prepare("UPDATE memories SET data = ? WHERE id = ?")?;
            for (id, data) in rows {
                let text = match &data {
                    rusqlite::types::Value::Text(t) => t.clone(),
                    // `JSON.parse(String(x))` of a number gives a number:
                    // no anchors; of NULL, `null`: throws; of a blob: invalid.
                    _ => continue,
                };
                let Some(mut parsed) = parse(&text) else {
                    continue;
                };
                if apply_moves(&mut parsed, moves) == Some(true) {
                    update.execute(rusqlite::params![stringify(&parsed), id])?;
                    r.updated_sqlite_count += 1;
                }
            }
            Ok(())
        })();
    }
    if let Some(jsonl) = contained(project_root, &memories.join("memories.jsonl"))
        && jsonl.is_file()
        && let Ok(bytes) = std::fs::read(&jsonl)
    {
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        let mut mutated = false;
        let next: Vec<String> = raw
            .split('\n')
            .map(|line| {
                let trimmed = js_trim(line);
                if trimmed.is_empty() {
                    return line.to_string();
                }
                let Some(mut parsed) = parse(trimmed) else {
                    return line.to_string();
                };
                if apply_moves(&mut parsed, moves) == Some(true) {
                    mutated = true;
                    r.updated_jsonl_count += 1;
                    return stringify(&parsed);
                }
                line.to_string()
            })
            .collect();
        if mutated {
            write_replacing(&jsonl, next.join("\n").as_bytes());
        }
    }
    r
}

/// Replace `path` (link-free, vetted) with `bytes`: written beside it and
/// renamed over it, so a link swapped in meanwhile is replaced, not followed.
/// A rename refused because another process holds the file open falls back
/// to writing the vetted path in place, as WrongStack's `writeFileSync` does.
fn write_replacing(path: &Path, bytes: &[u8]) {
    let Some(dir) = path.parent() else { return };
    let tmp = dir.join(format!(
        ".memories.jsonl.wrongindex-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_ok() {
        return;
    }
    let _ = std::fs::remove_file(&tmp);
    let unchanged_target = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file());
    if unchanged_target {
        let _ = std::fs::write(path, bytes);
    }
}

/// The `toRel` of `finalizeIndexRun` for snapshot files.
pub fn snapshot_rel(project_root: &str, file: &str) -> String {
    if node_is_absolute(file) {
        node_relative(project_root, file).replace('\\', "/")
    } else {
        file.replace('\\', "/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt(s: &str) -> String {
        stringify(&parse(s).unwrap())
    }

    #[test]
    fn json_round_trips_like_javascript() {
        assert_eq!(
            rt(r#"{"b":1,"a":2,"2":3,"1":4,"01":5}"#),
            r#"{"1":4,"2":3,"b":1,"a":2,"01":5}"#
        );
        assert_eq!(rt(r#"{"a":1,"b":2,"a":3}"#), r#"{"a":3,"b":2}"#);
        assert_eq!(
            rt(r#"[1.0, -0, 1e21, 1e-7, 0.1, 123456789012345678901, 5e-324]"#),
            "[1,0,1e+21,1e-7,0.1,123456789012345680000,5e-324]"
        );
        assert_eq!(rt(r#""\ud800x\u0001 /""#), "\"\\ud800x\\u0001\u{2028}/\"");
        assert_eq!(rt(" { \"x\" : [ true , null ] } "), r#"{"x":[true,null]}"#);
        assert!(parse("{\"a\":01}").is_none());
        assert!(parse("[1,]").is_none());
        assert!(parse("\u{feff}{}").is_none());
        assert!(parse("\"a\tb\"").is_none());
    }

    #[test]
    fn anonymous_signatures() {
        assert_eq!(
            normalize_anon_signature("foo", Some("function foo(a)  {")),
            "function _(a) {"
        );
        assert_eq!(
            normalize_anon_signature("foo", Some("foobar(foo)")),
            "foobar(_)"
        );
        assert_eq!(normalize_anon_signature("x", None), "");
        assert_eq!(
            normalize_anon_signature("$a", Some("let $a = 1")),
            "let $a = 1"
        );
        assert_eq!(normalize_anon_signature("", Some("ab cd")), "_ab_ _cd_");
    }

    #[test]
    fn moves_are_detected_as_wrongstack_does() {
        let s = |name: &str, file: &str, sig: Option<&str>| Snap {
            name: name.into(),
            kind: "function".into(),
            file: file.into(),
            signature: sig.map(str::to_string),
        };
        let prev = vec![
            s("greet", "src/a.ts", Some("function greet(n: string)")),
            s(
                "helperOne",
                "src/a.ts",
                Some("function helperOne(x: number): number"),
            ),
        ];
        let next = vec![
            s("greet", "src/b.ts", Some("function greet(n: string)")),
            s(
                "helperTwo",
                "src/a.ts",
                Some("function helperTwo(x: number): number"),
            ),
        ];
        let moves = detect_moves(&prev, &next);
        assert_eq!(moves.len(), 2, "{moves:?}");
        assert_eq!(
            (moves[0].new_file.as_str(), moves[0].new_symbol.as_str()),
            ("src/b.ts", "greet")
        );
        assert_eq!(
            (moves[1].old_symbol.as_str(), moves[1].new_symbol.as_str()),
            ("helperOne", "helperTwo")
        );
        let mut rec = parse(r#"{"text":"t","anchors":[{"path":"src\\a.ts","symbol":" greet "},{"path":"x"}],"z":1}"#).unwrap();
        assert_eq!(apply_moves(&mut rec, &moves), Some(true));
        assert_eq!(
            stringify(&rec),
            r#"{"text":"t","anchors":[{"path":"src/b.ts","symbol":"greet"},{"path":"x"}],"z":1}"#
        );
        let mut bad = parse(r#"{"anchors":[{"path":"src/a.ts","symbol":"greet"},null]}"#).unwrap();
        assert_eq!(apply_moves(&mut bad, &moves), None);
    }
}
