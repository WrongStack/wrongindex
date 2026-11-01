//! Port of WrongStack `import-extractor.ts` (`extractImports`,
//! `hasImportPatterns`), Rust branch only.
//!
//! The two `rs` patterns are reimplemented as deterministic matchers with the
//! exact semantics of the JS regexes (`gm` flags, no `u`): multiline `^`
//! breaks at `\n`, `\r`, U+2028 and U+2029, `\s` is the ECMAScript set, `\w`
//! is ASCII. Offsets are UTF-16 code units.
//!
//! ```text
//! use: ^[ \t]*(?:pub(?:\s*\([^)]*\))?\s+)?use\s+(?:::)?([\w:]+?)(?:::\{|::\*|\s*;|\s+as\b)
//! mod: ^[ \t]*(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+(\w+)\s*;
//! ```
//!
//! Backtracking: every optional group is tried "taken" first and "skipped"
//! second, in regex order; the greedy `\s`/`[ \t]`/`\w` runs never need to
//! give back characters because the token that follows each of them cannot
//! match the characters they consumed. The lazy `[\w:]+?` is expanded one
//! unit at a time, testing the terminator alternatives in order.

use super::ParsedRef;
use crate::js::{self, U16};

/// (capture start, capture end, match end).
type Hit = (usize, usize, usize);
/// A `^`-anchored pattern tried at one position.
type Matcher = fn(&U16, usize) -> Option<Hit>;

/// Only the leading window is scanned (`IMPORT_MAX_FILE_CHARS`).
pub const IMPORT_MAX_FILE_CHARS: usize = 512 * 1024;
/// Upper bound on imports recorded per file (`IMPORT_MAX_PER_FILE`).
pub const IMPORT_MAX_PER_FILE: usize = 400;

/// `hasImportPatterns(lang)`, restricted to the ported languages.
pub fn has_import_patterns(lang: &str) -> bool {
    lang == "rs"
}

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

fn skip_while(s: &U16, mut p: usize, f: impl Fn(u16) -> bool) -> usize {
    while p < s.len() && f(s[p]) {
        p += 1;
    }
    p
}

/// `\s+` at `p`: end position, if at least one unit matched.
fn spaces1(s: &U16, p: usize) -> Option<usize> {
    let e = skip_while(s, p, js::is_space);
    (e > p).then_some(e)
}

/// Candidate end positions of `(?:pub(?:\s*\([^)]*\))?\s+)?` at `p`, in the
/// order the regex engine tries them.
fn pub_prefix(s: &U16, p: usize) -> Vec<usize> {
    let mut out = Vec::new();
    if at(s, p, "pub") {
        let after = p + 3;
        // Inner group taken.
        let q = skip_while(s, after, js::is_space);
        if s.get(q) == Some(&(b'(' as u16)) {
            let close = skip_while(s, q + 1, |c| c != b')' as u16);
            if close < s.len()
                && let Some(e) = spaces1(s, close + 1)
            {
                out.push(e);
            }
        }
        // Inner group skipped.
        if let Some(e) = spaces1(s, after) {
            out.push(e);
        }
    }
    out.push(p);
    out
}

fn is_path_unit(c: u16) -> bool {
    js::is_word(c) || c == b':' as u16
}

/// `(?:::\{|::\*|\s*;|\s+as\b)` at `t`: match end.
fn use_terminator(s: &U16, t: usize) -> Option<usize> {
    if at(s, t, "::{") || at(s, t, "::*") {
        return Some(t + 3);
    }
    let e = skip_while(s, t, js::is_space);
    if s.get(e) == Some(&(b';' as u16)) {
        return Some(e + 1);
    }
    if e > t && at(s, e, "as") && !s.get(e + 2).is_some_and(|&c| js::is_word(c)) {
        return Some(e + 2);
    }
    None
}

/// The `use` pattern at line start `p`: (capture start, capture end, match end).
fn match_use(s: &U16, p: usize) -> Option<Hit> {
    let p = skip_while(s, p, |c| c == 0x20 || c == 0x09);
    for q in pub_prefix(s, p) {
        if !at(s, q, "use") {
            continue;
        }
        let Some(r) = spaces1(s, q + 3) else {
            continue;
        };
        let starts: &[usize] = if at(s, r, "::") { &[r + 2, r] } else { &[r] };
        for &c0 in starts {
            let mut t = c0;
            while t < s.len() && is_path_unit(s[t]) {
                t += 1;
                if let Some(end) = use_terminator(s, t) {
                    return Some((c0, t, end));
                }
            }
        }
    }
    None
}

/// The `mod` pattern at line start `p`.
fn match_mod(s: &U16, p: usize) -> Option<Hit> {
    let p = skip_while(s, p, |c| c == 0x20 || c == 0x09);
    for q in pub_prefix(s, p) {
        if !at(s, q, "mod") {
            continue;
        }
        let Some(n0) = spaces1(s, q + 3) else {
            continue;
        };
        let n1 = skip_while(s, n0, js::is_word);
        if n1 == n0 {
            continue;
        }
        let e = skip_while(s, n1, js::is_space);
        if s.get(e) == Some(&(b';' as u16)) {
            return Some((n0, n1, e + 1));
        }
    }
    None
}

/// `content.matchAll(re)` for a `^`-anchored global multiline regex.
fn match_all_lines(s: &U16, matcher: Matcher) -> Vec<Hit> {
    let mut out = Vec::new();
    let mut p = 0;
    while p <= s.len() {
        if js::is_line_start(s, p)
            && let Some(m) = matcher(s, p)
        {
            out.push((p, m.0, m.1));
            p = m.2.max(p + 1);
            continue;
        }
        p += 1;
    }
    out
}

/// `lastSegment(specifier)`.
fn last_segment(spec: &str) -> String {
    let path_like = spec.contains(['/', '\\']) || spec.contains("::");
    let segments: Vec<&str> = split_path(spec)
        .into_iter()
        .filter(|x| !x.is_empty())
        .collect();
    let mut last = segments.last().copied().unwrap_or(spec);
    if last == "*" || last == "_" {
        last = if segments.len() >= 2 {
            segments[segments.len() - 2]
        } else {
            spec
        };
    }
    if path_like {
        let stripped = strip_extension(last);
        return if stripped.is_empty() { last } else { stripped }.to_string();
    }
    let dotted: Vec<&str> = last
        .split('.')
        .filter(|x| !x.is_empty() && *x != "*" && *x != "_")
        .collect();
    dotted.last().copied().unwrap_or(last).to_string()
}

/// `specifier.split(/[/\\]|::/)`.
fn split_path(spec: &str) -> Vec<&str> {
    let b = spec.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < b.len() {
        let len = match b[i] {
            b'/' | b'\\' => 1,
            b':' if b.get(i + 1) == Some(&b':') => 2,
            _ => 0,
        };
        if len > 0 {
            out.push(&spec[start..i]);
            i += len;
            start = i;
        } else {
            i += 1;
        }
    }
    out.push(&spec[start..]);
    out
}

/// `last.replace(/\.[A-Za-z0-9]{1,8}$/, '')`.
fn strip_extension(last: &str) -> &str {
    if let Some(dot) = last.rfind('.') {
        let ext = &last[dot + 1..];
        if (1..=8).contains(&ext.len()) && ext.bytes().all(|c| c.is_ascii_alphanumeric()) {
            return &last[..dot];
        }
    }
    last
}

/// `extractImports({ content, lang })` for the ported languages; refs are
/// returned already stamped with `lang`, as `withRelations` leaves them.
pub fn extract_imports(content: &U16, lang: &'static str) -> Vec<ParsedRef> {
    if lang != "rs" || content.is_empty() {
        return Vec::new();
    }
    let content = &content[..content.len().min(IMPORT_MAX_FILE_CHARS)];
    let newlines: Vec<usize> = content
        .iter()
        .enumerate()
        .filter(|&(_, &c)| c == 0x0A)
        .map(|(i, _)| i)
        .collect();
    let mut refs: Vec<ParsedRef> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let patterns: [Matcher; 2] = [match_use, match_mod];
    for pattern in patterns {
        for (index, c0, c1) in match_all_lines(content, pattern) {
            if refs.len() >= IMPORT_MAX_PER_FILE {
                return refs;
            }
            // The captures hold only `[\w:]`, so `trim()` is the identity.
            let module = js::string(&content[c0..c1]);
            if module.is_empty() {
                continue;
            }
            let to_name = last_segment(&module);
            if to_name.is_empty() {
                continue;
            }
            if !seen.insert((module.clone(), to_name.clone())) {
                continue;
            }
            // `lineAt`: count of newlines strictly before the match index.
            let line = newlines.partition_point(|&o| o < index) + 1;
            refs.push(ParsedRef {
                to_name,
                call_type: "import",
                line,
                lang,
                module: Some(module),
            });
        }
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js::utf16;

    fn mods(src: &str) -> Vec<(String, String, usize)> {
        extract_imports(&utf16(src), "rs")
            .into_iter()
            .map(|r| (r.module.unwrap(), r.to_name, r.line))
            .collect()
    }

    #[test]
    fn use_trees_take_the_path_before_the_brace() {
        assert_eq!(
            mods("use a::{b, c::{d, e}};\npub(crate) use x::Y as Z;\nmod m;\n"),
            vec![
                ("a".into(), "a".into(), 1),
                ("x::Y".into(), "Y".into(), 2),
                ("m".into(), "m".into(), 3),
            ]
        );
    }

    #[test]
    fn last_segment_matches_the_typescript() {
        assert_eq!(last_segment("a::b::_"), "b");
        assert_eq!(last_segment("_"), "_");
        assert_eq!(last_segment("a:::b"), ":b");
        assert_eq!(last_segment("::"), "::");
    }
}
