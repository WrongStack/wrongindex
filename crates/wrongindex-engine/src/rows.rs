//! Per-symbol derived columns, ported from WrongStack's writer:
//! `inferIsExportedFlag`, `computeSymbolSignatureHash`, `inferSymbolEndLines`
//! (`writer-mutations.ts`) and `buildIndexableText` (`bm25.ts`).

use crate::js::{self, U16, utf16};
use crate::parse::ParsedSymbol;

/// `xxhash64String`: xxHash64 (seed 0) of the string's UTF-8 encoding.
pub fn xxhash64_string(s: &str) -> String {
    wrongindex_storage::content_hash::xxhash64_hex(s.as_bytes())
}

/// `/\b(export|pub|public)\b/.test(signature)` (ASCII word boundaries), plus
/// the Go/Python rules, which never apply to the languages written natively.
pub fn is_exported(signature: &str) -> bool {
    let b = signature.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    for kw in ["export", "pub", "public"] {
        let k = kw.as_bytes();
        let mut from = 0;
        while let Some(i) = signature[from..].find(kw).map(|i| i + from) {
            let before = i == 0 || !word(b[i - 1]);
            let after = i + k.len() >= b.len() || !word(b[i + k.len()]);
            if before && after {
                return true;
            }
            from = i + 1;
        }
    }
    false
}

/// `signature.trim().replace(/\s+/g, ' ')` with ECMAScript whitespace.
fn normalize_signature(signature: &str) -> String {
    let s = utf16(signature);
    let t = js::trim(&s);
    let mut out: Vec<u16> = Vec::with_capacity(t.len());
    let mut in_space = false;
    for &c in t {
        if js::is_space(c) {
            if !in_space {
                out.push(0x20);
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    js::string(&out)
}

pub fn signature_hash(s: &ParsedSymbol) -> String {
    xxhash64_string(&format!(
        "{}\u{0}{}\u{0}{}\u{0}{}",
        s.kind,
        s.scope,
        s.name,
        normalize_signature(&s.signature)
    ))
}

/// End lines for one file's symbols: stable sort by line, then only the
/// immediate successor is consulted.
pub fn end_lines(symbols: &[ParsedSymbol]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..symbols.len()).collect();
    order.sort_by_key(|&i| symbols[i].line); // stable
    let mut out = vec![0; symbols.len()];
    for (k, &i) in order.iter().enumerate() {
        let s = &symbols[i];
        out[i] = match order.get(k + 1).map(|&n| &symbols[n]) {
            Some(next) if next.line > s.line => s.line.max(next.line - 1),
            _ => s.line,
        };
    }
    out
}

// ─── FTS text (bm25.ts) ─────────────────────────────────────────────────────

pub const FTS_SIGNATURE_MAX_CHARS: usize = 300;
pub const MODULE_OWNER_NAME: &str = "<module>";

fn is_upper(c: char) -> bool {
    c.is_ascii_uppercase()
}

fn is_lower(c: char) -> bool {
    c.is_ascii_lowercase()
}

/// `\p{L}` (Unicode General Category Letter).
fn is_letter(c: char) -> bool {
    crate::unicode::is_letter(c)
}

/// `/([A-Z]+)([A-Z][a-z])/g` → `$1 $2`
fn split_acronyms(s: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(s.len() + 4);
    let mut p = 0;
    while p < s.len() {
        if is_upper(s[p]) {
            let mut r = p;
            while r < s.len() && is_upper(s[r]) {
                r += 1;
            }
            if r - p >= 2 && r < s.len() && is_lower(s[r]) {
                out.extend_from_slice(&s[p..r - 1]);
                out.push(' ');
                out.extend_from_slice(&s[r - 1..=r]);
                p = r + 1;
                continue;
            }
        }
        out.push(s[p]);
        p += 1;
    }
    out
}

/// Replace every non-overlapping `(a)(b)` pair with `a b`.
fn split_pairs(
    s: &[char],
    first: impl Fn(char) -> bool,
    second: impl Fn(char) -> bool,
) -> Vec<char> {
    let mut out = Vec::with_capacity(s.len() + 4);
    let mut p = 0;
    while p < s.len() {
        if p + 1 < s.len() && first(s[p]) && second(s[p + 1]) {
            out.push(s[p]);
            out.push(' ');
            out.push(s[p + 1]);
            p += 2;
            continue;
        }
        out.push(s[p]);
        p += 1;
    }
    out
}

pub fn split_name(name: &str) -> String {
    if name.is_empty()
        || !name
            .chars()
            .any(|c| is_upper(c) || c == '_' || c == '-' || c.is_ascii_digit())
    {
        return name.to_string();
    }
    let chars: Vec<char> = name.chars().collect();
    let s = split_acronyms(&chars);
    let s = split_pairs(&s, |c| is_lower(c) || c.is_ascii_digit(), is_upper);
    let s = split_pairs(&s, is_letter, |c| c.is_ascii_digit());
    let s = split_pairs(&s, |c| c.is_ascii_digit(), is_letter);
    // `/[_-]+/g` → ' '
    let mut out = String::with_capacity(s.len());
    let mut run = false;
    for c in s {
        if c == '_' || c == '-' {
            if !run {
                out.push(' ');
            }
            run = true;
        } else {
            out.push(c);
            run = false;
        }
    }
    js::string(js::trim(&utf16(&out)))
}

/// `declarationBodyStart` over UTF-16 code units.
fn declaration_body_start(s: &U16) -> Option<usize> {
    let (mut paren, mut angle, mut closed) = (0i64, 0i64, false);
    let mut i = 0;
    while i < s.len() {
        let ch = s[i];
        if !closed {
            if ch == b'(' as u16 {
                paren += 1;
            } else if ch == b')' as u16 {
                paren = (paren - 1).max(0);
                if paren == 0 {
                    closed = true;
                }
            }
            i += 1;
            continue;
        }
        if ch == b'<' as u16 {
            angle += 1;
            i += 1;
            continue;
        }
        if ch == b'>' as u16 && (i == 0 || s[i - 1] != b'=' as u16) {
            angle = (angle - 1).max(0);
            i += 1;
            continue;
        }
        if ch == b'{' as u16 {
            let mut j = i as isize - 1;
            while j >= 0 && js::is_space(s[j as usize]) {
                j -= 1;
            }
            let prev = if j >= 0 { Some(s[j as usize]) } else { None };
            if angle > 0 || matches!(prev, Some(0x3A) | Some(0x7C) | Some(0x26)) {
                let mut depth = 1;
                i += 1;
                while i < s.len() && depth > 0 {
                    if s[i] == b'{' as u16 {
                        depth += 1;
                    } else if s[i] == b'}' as u16 {
                        depth -= 1;
                    }
                    i += 1;
                }
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

fn fts_signature(signature: &str) -> String {
    if signature.is_empty() {
        return String::new();
    }
    let s = utf16(signature);
    let header = match declaration_body_start(&s) {
        Some(b) => &s[..b],
        None => &s[..],
    };
    js::string(&header[..header.len().min(FTS_SIGNATURE_MAX_CHARS)])
}

pub fn indexable_text(name: &str, signature: &str, doc: &str) -> String {
    if name == MODULE_OWNER_NAME && signature.is_empty() && doc.is_empty() {
        return String::new();
    }
    [
        split_name(name),
        name.to_string(),
        fts_signature(signature),
        doc.to_string(),
    ]
    .into_iter()
    .filter(|p| !p.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_name_matches_bm25() {
        assert_eq!(split_name("HTTPServer"), "HTTP Server");
        assert_eq!(split_name("complexOperation"), "complex Operation");
        assert_eq!(split_name("foo_bar-baz"), "foo bar baz");
        assert_eq!(split_name("sha256Hash"), "sha 256 Hash");
        assert_eq!(split_name("server"), "server");
        assert_eq!(split_name("__x__"), "x");
        assert_eq!(split_name("v2beta1"), "v 2 beta 1");
    }

    #[test]
    fn fts_text_keeps_declaration_header_only() {
        assert_eq!(
            indexable_text("server", "server: ", ""),
            "server server server: "
        );
        assert_eq!(indexable_text("f", "f(a) { body }", ""), "f f f(a) ");
        assert_eq!(indexable_text("<module>", "", ""), "");
    }

    #[test]
    fn exported_uses_ascii_word_boundaries() {
        assert!(is_exported("public: true"));
        assert!(is_exported("x: pub"));
        assert!(!is_exported("publication: 1"));
        assert!(!is_exported("exported: 1"));
    }

    #[test]
    fn end_lines_look_only_at_the_next_symbol() {
        let s = |line| ParsedSymbol::simple("property", &utf16("n"), line, 0, &utf16("n"));
        let syms = vec![s(1), s(3), s(6), s(4), s(4)];
        assert_eq!(end_lines(&syms), vec![2, 3, 6, 4, 5]);
    }
}
