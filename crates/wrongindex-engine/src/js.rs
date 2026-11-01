//! JavaScript string semantics over UTF-16 code units.
//!
//! WrongStack's parsers run on JS strings: offsets, columns, `length`, and
//! `slice` count UTF-16 code units; `trim()` and regex `\s` use the ECMAScript
//! WhiteSpace + LineTerminator sets; `^`/`$` in multiline regexes break at
//! `\n`, `\r`, U+2028 and U+2029. Ports operate on `&[u16]` with these helpers
//! so positions match byte-for-byte what the TypeScript code records.

pub type U16 = [u16];

pub fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Lossy UTF-16 -> String (lone surrogates become U+FFFD, as when a JS string
/// is encoded to UTF-8 for SQLite/JSON).
pub fn string(s: &U16) -> String {
    String::from_utf16_lossy(s)
}

/// ECMAScript `LineTerminator`.
pub fn is_line_terminator(c: u16) -> bool {
    matches!(c, 0x0A | 0x0D | 0x2028 | 0x2029)
}

/// ECMAScript `WhiteSpace` ∪ `LineTerminator` — regex `\s` and `trim()`.
pub fn is_space(c: u16) -> bool {
    matches!(
        c,
        0x09 | 0x0A | 0x0B | 0x0C | 0x0D | 0x20 | 0xA0 | 0x1680 | 0x2000
            ..=0x200A | 0x2028 | 0x2029 | 0x202F | 0x205F | 0x3000 | 0xFEFF
    )
}

/// Regex `\w` without the `u`/`i` flags: `[A-Za-z0-9_]`.
pub fn is_word(c: u16) -> bool {
    matches!(c, 0x30..=0x39 | 0x41..=0x5A | 0x61..=0x7A | 0x5F)
}

pub fn is_digit(c: u16) -> bool {
    (0x30..=0x39).contains(&c)
}

pub fn trim(s: &U16) -> &U16 {
    trim_end(trim_start(s))
}

pub fn trim_start(s: &U16) -> &U16 {
    let start = s.iter().position(|&c| !is_space(c)).unwrap_or(s.len());
    &s[start..]
}

pub fn trim_end(s: &U16) -> &U16 {
    let end = s.iter().rposition(|&c| !is_space(c)).map_or(0, |i| i + 1);
    &s[..end]
}

/// `s.indexOf(needle, from)`.
pub fn index_of(s: &U16, needle: &U16, from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from.min(s.len()));
    }
    if from >= s.len() {
        return None;
    }
    s[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

pub fn includes(s: &U16, needle: &str) -> bool {
    index_of(s, &utf16(needle), 0).is_some()
}

/// `s.split('\n')`.
pub fn split_lines(s: &U16) -> Vec<&U16> {
    s.split(|&c| c == 0x0A).collect()
}

/// Line-start offsets as computed by the parsers: `[0]` then cumulative
/// `line.length + 1` for every `split('\n')` line.
pub fn line_offsets(lines: &[&U16]) -> Vec<usize> {
    let mut out = Vec::with_capacity(lines.len() + 1);
    out.push(0);
    let mut acc = 0;
    for l in lines {
        acc += l.len() + 1;
        out.push(acc);
    }
    out
}

/// The parsers' binary search: 1-based line of `offset`.
pub fn line_from_offset(offsets: &[usize], offset: usize) -> usize {
    let (mut lo, mut hi) = (0usize, offsets.len() - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if offsets[mid] <= offset {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo + 1
}

/// Multiline `^`: start of input or just after a line terminator.
pub fn is_line_start(s: &U16, pos: usize) -> bool {
    pos == 0 || (pos <= s.len() && is_line_terminator(s[pos - 1]))
}

/// `truncate` from `@wrongstack/primitives` (UTF-16 lengths, keeps a
/// surrogate pair whole).
pub fn truncate(s: &U16, max: usize) -> Vec<u16> {
    if max == 0 {
        return Vec::new();
    }
    if s.len() <= max {
        return s.to_vec();
    }
    if max == 1 {
        return vec![0x2026];
    }
    let mut end = max - 1;
    let last = s[end - 1];
    if (0xD800..=0xDBFF).contains(&last) {
        end -= 1;
    }
    let mut out = s[..end].to_vec();
    out.push(0x2026);
    out
}

/// Node's `path.win32.basename` / posix basename for the separators used by
/// the indexer (`\` and `/`), without a suffix argument.
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['\\', '/']);
    match trimmed.rfind(['\\', '/']) {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_and_trim_follow_ecmascript() {
        let s = utf16("\u{FEFF}\u{00A0} a\u{2028}b \u{3000}");
        assert_eq!(string(trim(&s)), "a\u{2028}b");
        assert!(!is_space(0x85), "NEL is not JS whitespace");
        assert!(!is_space(0x180E), "U+180E left Zs in Unicode 6.3");
    }

    #[test]
    fn truncate_counts_utf16_and_keeps_pairs() {
        assert_eq!(string(&truncate(&utf16("abcdef"), 4)), "abc…");
        assert_eq!(string(&truncate(&utf16("abc"), 3)), "abc");
        // "ab😀cd": the cut would split the pair, so it backs off one unit.
        assert_eq!(string(&truncate(&utf16("ab😀cd"), 4)), "ab…");
        assert_eq!(string(&truncate(&utf16("abc"), 1)), "…");
    }

    #[test]
    fn line_search_matches_the_parsers() {
        let s = utf16("a\nbb\n\nc");
        let lines = split_lines(&s);
        let off = line_offsets(&lines);
        assert_eq!(off, vec![0, 2, 5, 6, 8]);
        assert_eq!(line_from_offset(&off, 0), 1);
        assert_eq!(line_from_offset(&off, 3), 2);
        assert_eq!(line_from_offset(&off, 5), 3);
        assert_eq!(line_from_offset(&off, 6), 4);
    }

    #[test]
    fn basename_handles_both_separators() {
        assert_eq!(basename(r"C:\a\b\package.json"), "package.json");
        assert_eq!(basename("/x/y/tsconfig.json"), "tsconfig.json");
        assert_eq!(basename("plain.json"), "plain.json");
    }
}
