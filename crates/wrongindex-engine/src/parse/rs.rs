//! Port of WrongStack `rs-parser.ts` (regex fallback parser: `maskRustNonCode`
//! plus nine declaration patterns), followed by `withRelations` from
//! `parser-dispatch.ts` so [`parse`] equals `parseFileContent(file, content, 'rs')`.
//!
//! Every pattern is a deterministic matcher with the semantics of the JS
//! regex (global, no `u` flag; `\w` ASCII, `\s` ECMAScript, `\b` ASCII word
//! boundary). Backtracking notes per pattern sit next to each matcher; where
//! a greedy run is followed by a token that cannot match what the run
//! consumed, giving characters back can never produce a match, so no
//! backtracking is modelled. Offsets are UTF-16 code units.

use super::imports;
use super::{Parsed, ParsedSymbol};
use crate::js::{self, U16};

const LANG: &str = "rs";

fn at(s: &U16, p: usize, lit: &str) -> bool {
    let mut i = p;
    for b in lit.bytes() {
        if s.get(i) != Some(&(b as u16)) {
            return false;
        }
        i += 1;
    }
    true
}

fn is(s: &U16, p: usize, b: u8) -> bool {
    s.get(p) == Some(&(b as u16))
}

fn word_at(s: &U16, p: usize) -> bool {
    s.get(p).is_some_and(|&c| js::is_word(c))
}

fn skip_while(s: &U16, mut p: usize, f: impl Fn(u16) -> bool) -> usize {
    while p < s.len() && f(s[p]) {
        p += 1;
    }
    p
}

fn spaces(s: &U16, p: usize) -> usize {
    skip_while(s, p, js::is_space)
}

/// `\s+`.
fn spaces1(s: &U16, p: usize) -> Option<usize> {
    let e = spaces(s, p);
    (e > p).then_some(e)
}

/// `\w+`: end of the run, if non-empty.
fn word1(s: &U16, p: usize) -> Option<usize> {
    let e = skip_while(s, p, js::is_word);
    (e > p).then_some(e)
}

/// `\b<kw>` at `p` for a keyword starting with a word character.
fn kw_boundary(s: &U16, p: usize, kw: &str) -> bool {
    (p == 0 || !js::is_word(s[p - 1])) && at(s, p, kw)
}

/// `(?<![\w'])<kw>` at `p`.
fn kw_lookbehind(s: &U16, p: usize, kw: &str) -> bool {
    (p == 0 || !(js::is_word(s[p - 1]) || s[p - 1] == b'\'' as u16)) && at(s, p, kw)
}

/// (name start, name end, match end).
type Hit = (usize, usize, usize);
/// A pattern tried at one position.
type Matcher = fn(&U16, usize) -> Option<Hit>;

/// `\bfn\s+(\w+)\s*[<(]`.
fn m_fn(s: &U16, p: usize) -> Option<Hit> {
    if !kw_boundary(s, p, "fn") {
        return None;
    }
    let n0 = spaces1(s, p + 2)?;
    let n1 = word1(s, n0)?;
    let e = spaces(s, n1);
    (is(s, e, b'<') || is(s, e, b'(')).then_some((n0, n1, e + 1))
}

/// `\b<kw>\s+(\w+)` (struct, enum, trait).
fn m_simple(s: &U16, p: usize, kw: &str) -> Option<Hit> {
    if !kw_boundary(s, p, kw) {
        return None;
    }
    let n0 = spaces1(s, p + kw.len())?;
    let n1 = word1(s, n0)?;
    Some((n0, n1, n1))
}

/// `<(?:[^<>]|<(?:[^<>]|<[^<>]*>)*>)*>` at `p`: a `<…>` group nested at most
/// three deep. Each repetition can only stop at `>`, which no alternative
/// consumes, so the end position is unique.
fn generics(s: &U16, p: usize) -> Option<usize> {
    fn group(s: &U16, p: usize, depth: usize) -> Option<usize> {
        if !is(s, p, b'<') {
            return None;
        }
        let mut i = p + 1;
        loop {
            match s.get(i).copied() {
                None => return None,
                Some(c) if c == b'>' as u16 => return Some(i + 1),
                Some(c) if c == b'<' as u16 => {
                    if depth == 0 {
                        return None;
                    }
                    i = group(s, i, depth - 1)?;
                }
                Some(_) => i += 1,
            }
        }
    }
    group(s, p, 2)
}

/// `^[ \t]*(?:unsafe\s+)?impl\b\s*(?:<…>)?\s*(?:[\w]+::)*(\w+)` (multiline).
///
/// `unsafe\s+` skipped would leave `impl` to match at `u`, so it is taken
/// whenever present. After `impl\s*`, a `<` must be consumed by the generic
/// group (skipping it leaves `(\w+)` facing `<`). `(?:\w+::)*` is greedy;
/// when no word follows the last `::`, the engine gives back one iteration and
/// captures that segment instead.
fn m_impl(s: &U16, p: usize) -> Option<Hit> {
    if !js::is_line_start(s, p) {
        return None;
    }
    let mut q = skip_while(s, p, |c| c == 0x20 || c == 0x09);
    if at(s, q, "unsafe")
        && let Some(e) = spaces1(s, q + 6)
        && at(s, e, "impl")
    {
        q = e;
    }
    if !at(s, q, "impl") || word_at(s, q + 4) {
        return None;
    }
    let mut r = spaces(s, q + 4);
    if is(s, r, b'<') {
        r = spaces(s, generics(s, r)?);
    }
    let mut last_segment: Option<(usize, usize)> = None;
    let mut i = r;
    while let Some(e) = word1(s, i) {
        if at(s, e, "::") {
            last_segment = Some((i, e));
            i = e + 2;
        } else {
            return Some((i, e, e));
        }
    }
    last_segment.map(|(a, b)| (a, b, b))
}

/// `\btype\s+(\w+)\s*(?:<[^=;]*>)?\s*=`.
///
/// `[^=;]*` runs to the first `=`/`;` and backs off to the last `>` before
/// it; only that `>` can be followed by `\s*=` (an earlier one has the later
/// `>` in between).
fn m_type(s: &U16, p: usize) -> Option<Hit> {
    if !kw_boundary(s, p, "type") {
        return None;
    }
    let n0 = spaces1(s, p + 4)?;
    let n1 = word1(s, n0)?;
    let q = spaces(s, n1);
    if is(s, q, b'=') {
        return Some((n0, n1, q + 1));
    }
    if !is(s, q, b'<') {
        return None;
    }
    let stop = skip_while(s, q + 1, |c| c != b'=' as u16 && c != b';' as u16);
    let gt = (q + 1..stop).rev().find(|&i| s[i] == b'>' as u16)?;
    let e = spaces(s, gt + 1);
    (e == stop && is(s, e, b'=')).then_some((n0, n1, e + 1))
}

/// `(?<![\w'])const\s+(?!(?:fn|unsafe|extern|async)\b)(\w+)\s*:`.
///
/// Backing `\s+` off would put the lookahead and `(\w+)` on a space, so only
/// the greedy end matters.
fn m_const(s: &U16, p: usize) -> Option<Hit> {
    if !kw_lookbehind(s, p, "const") {
        return None;
    }
    let n0 = spaces1(s, p + 5)?;
    for kw in ["fn", "unsafe", "extern", "async"] {
        if at(s, n0, kw) && !word_at(s, n0 + kw.len()) {
            return None;
        }
    }
    let n1 = word1(s, n0)?;
    let e = spaces(s, n1);
    is(s, e, b':').then_some((n0, n1, e + 1))
}

/// `(?<![\w'])static\s+(?:mut\s+)?(\w+)\s*:` — `mut\s+` taken first; when the
/// rest fails the engine retries with it skipped (capturing `mut` itself).
fn m_static(s: &U16, p: usize) -> Option<Hit> {
    if !kw_lookbehind(s, p, "static") {
        return None;
    }
    let r = spaces1(s, p + 6)?;
    let tail = |n0: usize| -> Option<Hit> {
        let n1 = word1(s, n0)?;
        let e = spaces(s, n1);
        is(s, e, b':').then_some((n0, n1, e + 1))
    };
    if at(s, r, "mut")
        && let Some(n0) = spaces1(s, r + 3)
        && let Some(hit) = tail(n0)
    {
        return Some(hit);
    }
    tail(r)
}

/// `\bmod\s+(\w+)\s*[;{]`.
fn m_mod(s: &U16, p: usize) -> Option<Hit> {
    if !kw_boundary(s, p, "mod") {
        return None;
    }
    let n0 = spaces1(s, p + 3)?;
    let n1 = word1(s, n0)?;
    let e = spaces(s, n1);
    (is(s, e, b';') || is(s, e, b'{')).then_some((n0, n1, e + 1))
}

/// `RS_PATTERNS`, in order.
const PATTERNS: [(&str, Matcher); 9] = [
    ("function", m_fn),
    ("struct", |s, p| m_simple(s, p, "struct")),
    ("enum", |s, p| m_simple(s, p, "enum")),
    ("trait", |s, p| m_simple(s, p, "trait")),
    ("impl", m_impl),
    ("type", m_type),
    ("const", m_const),
    ("static", m_static),
    ("mod", m_mod),
];

/// `maskRustNonCode`: comments and string/char literal contents become
/// spaces (newlines kept), preserving every offset.
pub fn mask_rust_non_code(src: &U16) -> Vec<u16> {
    const SP: u16 = 0x20;
    const NL: u16 = 0x0A;
    let blank = |out: &mut Vec<u16>, text: &U16| {
        out.extend(text.iter().map(|&c| if c == NL { NL } else { SP }));
    };
    let ch = |i: usize| src.get(i).copied();
    let w = |c: Option<u16>| c.is_some_and(js::is_word);
    let n = src.len();
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let c = src[i];
        let next = ch(i + 1);
        if c == b'/' as u16 && next == Some(b'/' as u16) {
            let stop = js::index_of(src, &[NL], i).unwrap_or(n);
            blank(&mut out, &src[i..stop]);
            i = stop;
            continue;
        }
        if c == b'/' as u16 && next == Some(b'*' as u16) {
            let mut depth = 1;
            let mut j = i + 2;
            while j < n && depth > 0 {
                if src[j] == b'/' as u16 && ch(j + 1) == Some(b'*' as u16) {
                    depth += 1;
                    j += 2;
                } else if src[j] == b'*' as u16 && ch(j + 1) == Some(b'/' as u16) {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            let j = j.min(n);
            blank(&mut out, &src[i..j]);
            i = j;
            continue;
        }
        let prev = if i > 0 { ch(i - 1) } else { None };
        let prev2 = if i > 1 { ch(i - 2) } else { None };
        let raw_prefix_ok =
            !w(prev) || ((prev == Some(b'b' as u16) || prev == Some(b'c' as u16)) && !w(prev2));
        if c == b'r' as u16
            && (next == Some(b'"' as u16) || next == Some(b'#' as u16))
            && raw_prefix_ok
        {
            let mut j = i + 1;
            let mut hashes = 0;
            while ch(j) == Some(b'#' as u16) {
                hashes += 1;
                j += 1;
            }
            if ch(j) == Some(b'"' as u16) {
                let mut closer = vec![b'"' as u16];
                closer.extend(std::iter::repeat_n(b'#' as u16, hashes));
                let stop = js::index_of(src, &closer, j + 1).map_or(n, |e| e + closer.len());
                out.push(b'r' as u16);
                blank(&mut out, &src[i + 1..stop]);
                i = stop;
                continue;
            }
        }
        if c == b'"' as u16 {
            let mut j = i + 1;
            while j < n && src[j] != b'"' as u16 {
                j += if src[j] == b'\\' as u16 { 2 } else { 1 };
            }
            out.push(c);
            if j >= n {
                blank(&mut out, &src[i + 1..n]);
                i = n;
            } else {
                blank(&mut out, &src[i + 1..j]);
                out.push(c);
                i = j + 1;
            }
            continue;
        }
        if c == b'\'' as u16 {
            let mut end = None;
            if next == Some(b'\\' as u16) {
                if let Some(close) = js::index_of(src, &[b'\'' as u16], i + 2)
                    && close - i <= 12
                {
                    end = Some(close);
                }
            } else if let Some(nc) = next {
                // `codePointAt(i + 1) > 0xffff`: a well-formed surrogate pair.
                let width = if (0xD800..=0xDBFF).contains(&nc)
                    && ch(i + 2).is_some_and(|l| (0xDC00..=0xDFFF).contains(&l))
                {
                    2
                } else {
                    1
                };
                if ch(i + 1 + width) == Some(b'\'' as u16) {
                    end = Some(i + 1 + width);
                }
            }
            if let Some(end) = end {
                out.push(c);
                blank(&mut out, &src[i + 1..end]);
                out.push(c);
                i = end + 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `rustDocAbove`: first non-empty line of the `///` block directly above
/// `line_idx` (attribute lines skipped), sliced to 200 units.
fn rust_doc_above(lines: &[&U16], line_idx: usize) -> Vec<u16> {
    let mut doc: Vec<&U16> = Vec::new();
    for i in (0..line_idx).rev() {
        let trimmed = js::trim(lines[i]);
        if at(trimmed, 0, "///") {
            doc.push(js::trim(&trimmed[3..]));
        } else if at(trimmed, 0, "#[") {
            continue;
        } else {
            break;
        }
    }
    // `doc` was built bottom-up; `find` runs top-down.
    let first = doc
        .iter()
        .rev()
        .find(|l| !l.is_empty())
        .copied()
        .unwrap_or(&[]);
    first[..first.len().min(200)].to_vec()
}

/// Symbols as `regexParse` produces them.
pub fn symbols(content: &U16) -> Vec<ParsedSymbol> {
    let lines = js::split_lines(content);
    let code = mask_rust_non_code(content);
    let offsets = js::line_offsets(&lines);
    let mut out: Vec<ParsedSymbol> = Vec::new();
    for (kind, matcher) in PATTERNS {
        let mut p = 0;
        while p <= code.len() {
            let Some((n0, n1, end)) = matcher(&code, p) else {
                p += 1;
                continue;
            };
            p = end.max(p + 1);
            let name = &code[n0..n1];
            let line = js::line_from_offset(&offsets, n0);
            let col = n0 - offsets[line - 1];
            let line_idx = line - 1;
            let src_line = lines.get(line_idx).copied().unwrap_or(&[]);
            let sig = js::trim(src_line);
            let signature = &sig[..sig.len().min(500)];
            let doc = rust_doc_above(&lines, line_idx);
            let mut text: Vec<u16> = name.to_vec();
            for part in [signature, &doc[..]] {
                if !part.is_empty() {
                    text.push(0x20);
                    text.extend_from_slice(part);
                }
            }
            out.push(ParsedSymbol {
                kind,
                name: js::string(name),
                line,
                col,
                signature: js::string(signature),
                doc_comment: js::string(&doc),
                scope: String::new(),
                text: js::string(js::trim(&text)),
                is_exported: None,
                end_line: None,
            });
        }
    }
    // Deduplicate by name + line, first occurrence wins.
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert((s.name.clone(), s.line)));
    out
}

/// `parseFileContent(file, content, 'rs')`: symbols, then `withRelations`
/// (the Rust parser emits no refs, so the import extractor always supplies
/// them).
pub fn parse(content: &U16) -> Parsed {
    let symbols = symbols(content);
    let refs = if imports::has_import_patterns(LANG) {
        imports::extract_imports(content, LANG)
    } else {
        Vec::new()
    };
    Parsed {
        symbols,
        unsupported: None,
        refs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js::utf16;

    fn names(src: &str) -> Vec<(&'static str, String, usize, usize)> {
        symbols(&utf16(src))
            .into_iter()
            .map(|s| (s.kind, s.name, s.line, s.col))
            .collect()
    }

    #[test]
    fn masked_keywords_and_lifetimes_are_not_symbols() {
        let got = names(
            "// fn hidden()\nfn shown<'a>(x: &'static str) -> impl Iterator {}\nconst fn c() {}\nlet s = r#\"struct X\"#;\n",
        );
        assert_eq!(
            got,
            vec![
                ("function", "shown".to_string(), 2, 3),
                ("function", "c".to_string(), 3, 9)
            ]
        );
    }

    #[test]
    fn impl_takes_the_last_path_segment() {
        let got = names("impl<T: Into<Vec<u8>>> a::b::Foo<T> {}\nimpl a::b::<X> {}\n");
        assert_eq!(
            got,
            vec![
                ("impl", "Foo".to_string(), 1, 29),
                ("impl", "b".to_string(), 2, 8)
            ]
        );
    }

    #[test]
    fn static_mut_backtracks_to_capture_mut() {
        let got = names("static mut : u8 = 0;\nstatic mut X: u8 = 0;\n");
        assert_eq!(
            got,
            vec![
                ("static", "mut".to_string(), 1, 7),
                ("static", "X".to_string(), 2, 11)
            ]
        );
    }
}
