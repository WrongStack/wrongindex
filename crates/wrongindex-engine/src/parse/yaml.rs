//! Port of WrongStack `yaml-parser.ts`. The original uses JavaScript regular
//! expressions; each one is reproduced here as a deterministic matcher with
//! the same ECMAScript semantics (multiline `^` after `\n`/`\r`/U+2028/U+2029,
//! Unicode `\s`, ASCII `\w`). Every pattern was checked for backtracking:
//! the character classes adjoining each quantifier are disjoint, so the
//! greedy match is the only match.

use std::collections::HashSet;

use super::{Parsed, ParsedSymbol};
use crate::js::{self, U16, utf16};

const HASH: u16 = b'#' as u16;
const COLON: u16 = b':' as u16;
const DASH: u16 = b'-' as u16;

fn is_blank(c: u16) -> bool {
    c == 0x20 || c == 0x09
}

fn is_block_indicator(c: u16) -> bool {
    c == b'|' as u16 || c == b'>' as u16
}

fn is_chomp(c: u16) -> bool {
    c == b'-' as u16 || c == b'+' as u16 || js::is_digit(c)
}

/// `[^:#\s]`
fn is_key_char(c: u16) -> bool {
    c != COLON && c != HASH && !js::is_space(c)
}

/// `line.replace(/\r$/, '')`
fn strip_cr(line: &U16) -> &U16 {
    match line.last() {
        Some(&0x0D) => &line[..line.len() - 1],
        _ => line,
    }
}

/// `[ \t]*[|>][-+0-9]*[ \t]*(?:#.*)?$` against the whole of `s`.
fn block_header_tail(s: &U16) -> bool {
    let mut i = 0;
    while i < s.len() && is_blank(s[i]) {
        i += 1;
    }
    if i >= s.len() || !is_block_indicator(s[i]) {
        return false;
    }
    i += 1;
    while i < s.len() && is_chomp(s[i]) {
        i += 1;
    }
    while i < s.len() && is_blank(s[i]) {
        i += 1;
    }
    if i == s.len() {
        return true;
    }
    // `#.*$`: `.` excludes line terminators, `$` is end of input (no `m`).
    s[i] == HASH && !s[i + 1..].iter().any(|&c| js::is_line_terminator(c))
}

/// `^(\s*)(?:(?:-[ \t]+)?[^#\n]*?:|-)[ \t]*[|>][-+0-9]*[ \t]*(?:#.*)?$`
/// Returns the length of group 1 when the line is a block-scalar header.
fn block_header(line: &U16) -> Option<usize> {
    let indent = line.iter().take_while(|&&c| js::is_space(c)).count();
    let rest = &line[indent..];
    // Alternative 1: some `:` with no `#` (or `\n`) before it.
    for (k, &c) in rest.iter().enumerate() {
        if c == HASH || c == 0x0A {
            break;
        }
        if c == COLON && block_header_tail(&rest[k + 1..]) {
            return Some(indent);
        }
    }
    // Alternative 2: a bare `-`.
    if rest.first() == Some(&DASH) && block_header_tail(&rest[1..]) {
        return Some(indent);
    }
    None
}

/// 0-based indices of lines that are the text of a block scalar.
fn block_scalar_body_lines(lines: &[&U16]) -> HashSet<usize> {
    let mut body = HashSet::new();
    let mut i = 0;
    while i < lines.len() {
        let line = strip_cr(lines[i]);
        let Some(base) = block_header(line) else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j < lines.len() {
            let next = strip_cr(lines[j]);
            if js::trim(next).is_empty() {
                body.insert(j);
                j += 1;
                continue;
            }
            let indent = next.len() - js::trim_start(next).len();
            if indent <= base {
                break;
            }
            body.insert(j);
            j += 1;
        }
        // `i = j - 1` then the loop's `i++`.
        i = j.max(i + 1);
    }
    body
}

/// `/^(["'])(.*)\1$/` → inner text.
fn unquote_key(key: &U16) -> &U16 {
    if key.len() >= 2 {
        let q = key[0];
        if (q == b'"' as u16 || q == b'\'' as u16) && key[key.len() - 1] == q {
            let inner = &key[1..key.len() - 1];
            if !inner.iter().any(|&c| js::is_line_terminator(c)) {
                return inner;
            }
        }
    }
    key
}

fn extract_value(c: &U16, after: usize) -> &U16 {
    let end = js::index_of(c, &[0x0A], after).unwrap_or(c.len());
    js::trim(&c[after.min(end)..end])
}

/// `/^[|>][-+0-9]*(\s|#|$)/`
fn is_block_scalar_value(v: &U16) -> bool {
    if v.is_empty() || !is_block_indicator(v[0]) {
        return false;
    }
    let mut i = 1;
    while i < v.len() && is_chomp(v[i]) {
        i += 1;
    }
    i == v.len() || js::is_space(v[i]) || v[i] == HASH
}

/// `/^-?\d+(\.\d+)?([eE][+-]?\d+)?$/`
fn is_number(v: &U16) -> bool {
    let mut i = 0;
    if v.first() == Some(&DASH) {
        i += 1;
    }
    let digits = |i: &mut usize| {
        let s = *i;
        while *i < v.len() && js::is_digit(v[*i]) {
            *i += 1;
        }
        *i > s
    };
    if !digits(&mut i) {
        return false;
    }
    if v.get(i) == Some(&(b'.' as u16)) {
        let mut k = i + 1;
        if digits(&mut k) {
            i = k;
        }
    }
    if matches!(v.get(i), Some(&0x65) | Some(&0x45)) {
        let mut k = i + 1;
        if matches!(v.get(k), Some(&0x2B) | Some(&0x2D)) {
            k += 1;
        }
        if digits(&mut k) {
            i = k;
        }
    }
    i == v.len()
}

fn is_scalar(v: &U16) -> bool {
    if v.is_empty() {
        return false;
    }
    if is_number(v) {
        return true;
    }
    let lower: String = js::string(v).to_ascii_lowercase();
    if matches!(lower.as_str(), "true" | "false" | "null" | "undefined")
        && v.iter().all(|&c| c < 0x80)
    {
        return true;
    }
    for q in [b'\'' as u16, b'"' as u16] {
        if v.len() >= 2 && v[0] == q && v[v.len() - 1] == q && !v[1..v.len() - 1].contains(&q) {
            return true;
        }
    }
    false
}

/// `(?:^|(?<=[\s:\-[{,]))SIGIL(\w[\w-]*)` — every occurrence, in order.
fn sigil_names(c: &U16, sigil: u16) -> Vec<(usize, Vec<u16>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] != sigil {
            i += 1;
            continue;
        }
        let ok_before = js::is_line_start(c, i)
            || matches!(c[i - 1], 0x3A | 0x2D | 0x5B | 0x7B | 0x2C)
            || js::is_space(c[i - 1]);
        if ok_before && i + 1 < c.len() && js::is_word(c[i + 1]) {
            let mut j = i + 2;
            while j < c.len() && (js::is_word(c[j]) || c[j] == DASH) {
                j += 1;
            }
            out.push((i, c[i + 1..j].to_vec()));
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

/// Positions where a `/…/gm` regex anchored with `^` may start, honouring
/// `lastIndex` advancing to the end of the previous match.
struct LineStarts<'a> {
    c: &'a U16,
    pos: usize,
}

impl LineStarts<'_> {
    fn next_from(&mut self) -> Option<usize> {
        while self.pos <= self.c.len() {
            let p = self.pos;
            if js::is_line_start(self.c, p) {
                return Some(p);
            }
            self.pos += 1;
        }
        None
    }
}

/// Match `([ \t]*)([^:#\s][^:#\s]*)\s*:` at `p`. Returns
/// (indent, key range, end of match).
fn key_colon(c: &U16, p: usize) -> Option<(usize, (usize, usize), usize)> {
    let mut i = p;
    while i < c.len() && is_blank(c[i]) {
        i += 1;
    }
    let indent = i - p;
    let ks = i;
    while i < c.len() && is_key_char(c[i]) {
        i += 1;
    }
    if i == ks {
        return None;
    }
    let ke = i;
    while i < c.len() && js::is_space(c[i]) {
        i += 1;
    }
    if c.get(i) != Some(&COLON) {
        return None;
    }
    Some((indent, (ks, ke), i + 1))
}

pub fn parse(content: &U16) -> Parsed {
    let c = content;
    let lines = js::split_lines(c);
    let offsets = js::line_offsets(&lines);
    let body = block_scalar_body_lines(&lines);
    let in_body = |offset: usize| body.contains(&(js::line_from_offset(&offsets, offset) - 1));
    let pos = |offset: usize| {
        let line = js::line_from_offset(&offsets, offset);
        (line, offset - offsets[line - 1])
    };
    let mut symbols = Vec::new();

    // 1. Anchors, 2. aliases.
    for (sigil, ch) in [(b'&' as u16, "&"), (b'*' as u16, "*")] {
        for (offset, name) in sigil_names(c, sigil) {
            if in_body(offset) {
                continue;
            }
            let (line, col) = pos(offset);
            let mut sig = utf16(ch);
            sig.extend_from_slice(&name);
            symbols.push(ParsedSymbol::simple("const", &name, line, col, &sig));
        }
    }

    // 3. `key: value` pairs.
    let mut starts = LineStarts { c, pos: 0 };
    while let Some(p) = starts.next_from() {
        let Some((indent, (ks, ke), end)) = key_colon(c, p) else {
            starts.pos = p + 1;
            continue;
        };
        starts.pos = end;
        let key = unquote_key(&c[ks..ke]);
        if key.is_empty() || in_body(p) {
            continue;
        }
        let (line, _) = pos(p);
        let col = p + indent - offsets[line - 1];
        let line_text = lines.get(line - 1).copied().unwrap_or(&[]);
        if js::trim(line_text)
            .first()
            .is_some_and(|&f| f == b'|' as u16 || f == b'&' as u16 || f == b'>' as u16)
        {
            continue;
        }
        if key == utf16("---").as_slice() || key == utf16("...").as_slice() {
            continue;
        }
        if indent > 12 {
            continue;
        }
        let value = extract_value(c, end);
        if is_block_scalar_value(value) {
            continue;
        }
        let kind = if is_scalar(value) {
            "literal"
        } else {
            "property"
        };
        let mut sig = key.to_vec();
        sig.extend(utf16(": "));
        sig.extend(js::truncate(value, 60));
        symbols.push(ParsedSymbol::simple(kind, key, line, col, &sig));
    }

    // 4. `- key: value` list items.
    let mut starts = LineStarts { c, pos: 0 };
    while let Some(p) = starts.next_from() {
        let m = (|| {
            let mut i = p;
            while i < c.len() && is_blank(c[i]) {
                i += 1;
            }
            let g1 = i - p;
            if c.get(i) != Some(&DASH) {
                return None;
            }
            i += 1;
            let s2 = i;
            while i < c.len() && js::is_space(c[i]) {
                i += 1;
            }
            let g2 = i - s2;
            if g2 == 0 {
                return None;
            }
            let ks = i;
            while i < c.len() && is_key_char(c[i]) {
                i += 1;
            }
            if i == ks {
                return None;
            }
            let ke = i;
            while i < c.len() && js::is_space(c[i]) {
                i += 1;
            }
            if c.get(i) != Some(&COLON) {
                return None;
            }
            Some((g1, g2, ks, ke, i + 1))
        })();
        let Some((g1, g2, ks, ke, end)) = m else {
            starts.pos = p + 1;
            continue;
        };
        starts.pos = end;
        let key = unquote_key(&c[ks..ke]);
        if in_body(p) {
            continue;
        }
        let (line, _) = pos(p);
        let col = p + g1 + 1 + g2 - offsets[line - 1];
        let value = extract_value(c, end);
        let kind = if is_scalar(value) {
            "literal"
        } else {
            "property"
        };
        let mut sig = utf16("- ");
        sig.extend_from_slice(key);
        sig.extend(utf16(": "));
        sig.extend(js::truncate(value, 60));
        symbols.push(ParsedSymbol::simple(kind, key, line, col, &sig));
    }

    // 5. Block scalar headers `key: |` / `key: >-`.
    let mut starts = LineStarts { c, pos: 0 };
    while let Some(p) = starts.next_from() {
        let m = key_colon(c, p).and_then(|(indent, (ks, ke), mut i)| {
            while i < c.len() && is_blank(c[i]) {
                i += 1;
            }
            if i >= c.len() || !is_block_indicator(c[i]) {
                return None;
            }
            i += 1;
            while i < c.len() && is_chomp(c[i]) {
                i += 1;
            }
            // `(?=[ \t]|$)` with the `m` flag.
            let ok = i == c.len() || is_blank(c[i]) || js::is_line_terminator(c[i]);
            ok.then_some((indent, ks, ke, i))
        });
        let Some((indent, ks, ke, end)) = m else {
            starts.pos = p + 1;
            continue;
        };
        starts.pos = end;
        let key = unquote_key(&c[ks..ke]);
        if in_body(p) {
            continue;
        }
        let (line, _) = pos(p);
        let col = p + indent - offsets[line - 1];
        let mut sig = key.to_vec();
        sig.extend(utf16(": | ..."));
        symbols.push(ParsedSymbol::simple("property", key, line, col, &sig));
    }

    Parsed {
        symbols,
        unsupported: None,
    }
}
