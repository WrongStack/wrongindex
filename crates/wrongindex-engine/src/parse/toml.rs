//! Port of WrongStack's regex fallback extractor (`generic-parser.ts`,
//! `parseGeneric`) for `toml` only: one pattern, no keyword filter, no
//! import refs (`hasImportPatterns('toml')` is false).
//!
//! ```text
//! toml: /^\[\[?\s*([^[\]\s][^[\]]*?)\s*\]\]?/gm   kind 'namespace'
//! ```
//!
//! Infra artifacts (`detectInfraArtifactKind`) never apply to a `.toml`
//! file except by base name (`dockerfile.*`, `.env.*`); callers refuse those.

use super::{Parsed, ParsedSymbol};
use crate::js::{self, U16};

pub const GENERIC_MAX_SYMBOLS: usize = 500;
pub const GENERIC_MAX_FILE_CHARS: usize = 128 * 1024;

/// `detectInfraArtifactKind(file)` is non-null for this TOML path.
pub fn is_infra_artifact(file: &str) -> bool {
    let p = file.replace('\\', "/").to_lowercase();
    let base = p.rsplit('/').next().unwrap_or("");
    base == "dockerfile"
        || base.starts_with("dockerfile.")
        || base == ".env"
        || base.starts_with(".env.")
}

fn looks_binary(c: &U16) -> bool {
    if c.contains(&0) {
        return true;
    }
    let sample = &c[..c.len().min(2048)];
    if sample.is_empty() {
        return false;
    }
    let bad = sample
        .iter()
        .filter(|&&ch| !(ch == 9 || ch == 10 || ch == 13) && (ch < 32 || ch == 127))
        .count();
    bad as f64 / sample.len() as f64 > 0.1
}

/// One `^\[\[?\s*([^[\]\s][^[\]]*?)\s*\]\]?` attempt at `p`:
/// (capture start, capture end, match end).
fn match_at(s: &U16, p: usize) -> Option<(usize, usize, usize)> {
    let (open, close) = (b'[' as u16, b']' as u16);
    if s.get(p) != Some(&open) {
        return None;
    }
    let mut q = p + 1;
    // `\[?` taken when possible; skipping it can never succeed then (the
    // capture may not start with `[`).
    if s.get(q) == Some(&open) {
        q += 1;
    }
    while q < s.len() && js::is_space(s[q]) {
        q += 1;
    }
    let c0 = *s.get(q)?;
    if c0 == open || c0 == close || js::is_space(c0) {
        return None;
    }
    let start = q;
    let mut e = q + 1;
    loop {
        // `\s*\]` at e?
        let mut k = e;
        while k < s.len() && js::is_space(s[k]) {
            k += 1;
        }
        if s.get(k) == Some(&close) {
            let mut end = k + 1;
            if s.get(end) == Some(&close) {
                end += 1;
            }
            return Some((start, e, end));
        }
        // extend the lazy `[^[\]]*?` by one unit
        match s.get(e) {
            Some(&c) if c != open && c != close => e += 1,
            _ => return None,
        }
    }
}

/// `parseGeneric({ file, content, lang: 'toml' })` without infra symbols.
pub fn parse(content: &U16) -> Parsed {
    let mut out = Parsed::default();
    if content.is_empty() || looks_binary(content) {
        return out;
    }
    let s = &content[..content.len().min(GENERIC_MAX_FILE_CHARS)];
    let newlines: Vec<usize> = s
        .iter()
        .enumerate()
        .filter(|(_, c)| **c == 10)
        .map(|(i, _)| i)
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut p = 0;
    while p < s.len() {
        if out.symbols.len() >= GENERIC_MAX_SYMBOLS {
            break;
        }
        if !js::is_line_start(s, p) {
            p += 1;
            continue;
        }
        let Some((c0, c1, end)) = match_at(s, p) else {
            p += 1;
            continue;
        };
        let index = p;
        p = end;
        let name = js::trim(&s[c0..c1]);
        if name.is_empty() || name.len() > 200 {
            continue;
        }
        // `.replace(/^#+\s*/, '').replace(/["'`]/g, '')`
        let mut n = name;
        if n.first() == Some(&(b'#' as u16)) {
            let mut i = 0;
            while i < n.len() && n[i] == b'#' as u16 {
                i += 1;
            }
            while i < n.len() && js::is_space(n[i]) {
                i += 1;
            }
            n = &n[i..];
        }
        let name: Vec<u16> = n
            .iter()
            .copied()
            .filter(|&c| c != b'"' as u16 && c != b'\'' as u16 && c != b'`' as u16)
            .collect();
        if name.is_empty() {
            continue;
        }
        // lineColAt: newlines strictly before `index`.
        let low = newlines.partition_point(|&o| o < index);
        let last_nl: isize = if low > 0 {
            newlines[low - 1] as isize
        } else {
            -1
        };
        let line = low + 1;
        let col = (index as isize - last_nl - 1) as usize;
        let key = (name.clone(), line);
        if !seen.insert(key) {
            continue;
        }
        let nl = js::index_of(s, &[10], index).unwrap_or(s.len());
        let line_text = &s[index..nl];
        let sig_src: &U16 = if line_text.is_empty() {
            &name
        } else {
            line_text
        };
        let sig = js::trim(sig_src);
        let signature = &sig[..sig.len().min(500)];
        let mut text = name.clone();
        text.push(0x20);
        text.extend_from_slice(signature);
        let text = js::trim(&text);
        let name = &name[..name.len().min(200)];
        out.symbols.push(ParsedSymbol {
            kind: "namespace",
            name: js::string(name),
            line,
            col,
            signature: js::string(signature),
            doc_comment: String::new(),
            scope: String::new(),
            text: js::string(&text[..text.len().min(1000)]),
            is_exported: None,
            end_line: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js::utf16;

    fn names(src: &str) -> Vec<(String, usize, usize, String)> {
        parse(&utf16(src))
            .symbols
            .into_iter()
            .map(|s| (s.name, s.line, s.col, s.signature))
            .collect()
    }

    #[test]
    fn tables_and_arrays_of_tables() {
        assert_eq!(
            names(
                "[package]\nname = \"x\"\n\n[[bin]]\npath = 'a'\n[ dependencies . serde ] # c\n[]\n[a\nb]\n"
            ),
            vec![
                ("package".into(), 1, 0, "[package]".into()),
                ("bin".into(), 4, 0, "[[bin]]".into()),
                (
                    "dependencies . serde".into(),
                    6,
                    0,
                    "[ dependencies . serde ] # c".into()
                ),
                ("a\nb".into(), 8, 0, "[a".into()),
            ]
        );
        assert_eq!(
            names("[\"quoted.key\"]\r\n"),
            vec![("quoted.key".into(), 1, 0, "[\"quoted.key\"]".into())]
        );
        assert!(names("x[a]\n  [b]\n").is_empty());
        assert!(names("[a\0]").is_empty());
    }
}
