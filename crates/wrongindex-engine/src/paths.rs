//! Node `path` and JS string helpers used by WrongStack's relation code
//! (`module-roots.ts`, `module-resolver.ts`, `graph-rank.ts`).
//!
//! Ported from Node's `lib/path.js` (posix flavour, plus win32 `dirname` for
//! drive-absolute paths). Every separator these functions look at is ASCII,
//! so they work on UTF-8 byte offsets without splitting a character.

use std::cmp::Ordering;

/// `toPortablePath`: backslashes to forward slashes.
pub fn to_portable(file: &str) -> String {
    file.replace('\\', "/")
}

/// JS `string.length` (UTF-16 code units).
pub fn u16len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `String.prototype.toLowerCase` (locale-independent full mapping).
pub fn js_lower(s: &str) -> String {
    s.to_lowercase()
}

/// ECMAScript `WhiteSpace` ∪ `LineTerminator` (`\s`, `trim()`).
pub fn is_js_space(c: char) -> bool {
    u16::try_from(c as u32).is_ok_and(crate::js::is_space)
}

/// `String.prototype.trim`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// ECMAScript `LineTerminator` (what regex `.` does not match).
pub fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// `source.split(/\r?\n/)`.
pub fn split_lines_crlf(s: &str) -> Vec<&str> {
    let mut out: Vec<&str> = s.split('\n').collect();
    let last = out.len() - 1;
    for l in &mut out[..last] {
        *l = l.strip_suffix('\r').unwrap_or(l);
    }
    out
}

/// `line.replace(/<marker>.*$/, '')` without the `m` flag: the first marker
/// whose remainder holds no line terminator (`.` stops at one, and `$` is the
/// end of input).
pub fn strip_line_comment<'a>(line: &'a str, marker: &str) -> &'a str {
    let last_lt = line
        .char_indices()
        .filter(|(_, c)| is_line_terminator(*c))
        .map(|(i, c)| i + c.len_utf8())
        .next_back()
        .unwrap_or(0);
    match line[last_lt..].find(marker) {
        Some(i) => &line[..last_lt + i],
        None => line,
    }
}

fn normalize_string(path: &str, allow_above_root: bool) -> String {
    let b = path.as_bytes();
    let mut res = String::new();
    let mut last_segment_length: usize = 0;
    let mut last_slash: isize = -1;
    let mut dots: i32 = 0;
    let mut code = 0u8;
    let n = b.len();
    let mut i = 0usize;
    while i <= n {
        if i < n {
            code = b[i];
        } else if code == b'/' {
            break;
        } else {
            code = b'/';
        }
        if code == b'/' {
            if last_slash == i as isize - 1 || dots == 1 {
                // NOOP
            } else if dots == 2 {
                let rb = res.as_bytes();
                if rb.len() < 2
                    || last_segment_length != 2
                    || rb[rb.len() - 1] != b'.'
                    || rb[rb.len() - 2] != b'.'
                {
                    if rb.len() > 2 {
                        match res.rfind('/') {
                            None => {
                                res.clear();
                                last_segment_length = 0;
                            }
                            Some(idx) => {
                                res.truncate(idx);
                                last_segment_length = (res.len() as isize
                                    - 1
                                    - res.rfind('/').map_or(-1, |x| x as isize))
                                    as usize;
                            }
                        }
                        last_slash = i as isize;
                        dots = 0;
                        i += 1;
                        continue;
                    } else if !rb.is_empty() {
                        res.clear();
                        last_segment_length = 0;
                        last_slash = i as isize;
                        dots = 0;
                        i += 1;
                        continue;
                    }
                }
                if allow_above_root {
                    if res.is_empty() {
                        res.push_str("..");
                    } else {
                        res.push_str("/..");
                    }
                    last_segment_length = 2;
                }
            } else {
                let seg = &path[(last_slash + 1) as usize..i];
                if !res.is_empty() {
                    res.push('/');
                }
                res.push_str(seg);
                last_segment_length = i - (last_slash + 1) as usize;
            }
            last_slash = i as isize;
            dots = 0;
        } else if code == b'.' && dots != -1 {
            dots += 1;
        } else {
            dots = -1;
        }
        i += 1;
    }
    res
}

/// `path.posix.normalize`.
pub fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let is_abs = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut p = normalize_string(path, !is_abs);
    if p.is_empty() {
        if is_abs {
            return "/".into();
        }
        return if trailing { "./".into() } else { ".".into() };
    }
    if trailing {
        p.push('/');
    }
    if is_abs { format!("/{p}") } else { p }
}

/// `path.posix.join`.
pub fn join(parts: &[&str]) -> String {
    let mut joined: Option<String> = None;
    for a in parts {
        if !a.is_empty() {
            match &mut joined {
                None => joined = Some((*a).to_string()),
                Some(j) => {
                    j.push('/');
                    j.push_str(a);
                }
            }
        }
    }
    match joined {
        None => ".".into(),
        Some(j) => normalize(&j),
    }
}

/// `path.posix.dirname`.
pub fn dirname(path: &str) -> String {
    let b = path.as_bytes();
    if b.is_empty() {
        return ".".into();
    }
    let has_root = b[0] == b'/';
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut i = b.len() - 1;
    while i >= 1 {
        if b[i] == b'/' {
            if !matched_slash {
                end = i as isize;
                break;
            }
        } else {
            matched_slash = false;
        }
        i -= 1;
    }
    if end == -1 {
        return if has_root { "/".into() } else { ".".into() };
    }
    if has_root && end == 1 {
        return "//".into();
    }
    path[..end as usize].to_string()
}

/// `path.posix.basename` (no suffix argument).
pub fn basename(path: &str) -> String {
    let b = path.as_bytes();
    let mut start = 0usize;
    let mut end: isize = -1;
    let mut matched_slash = true;
    for i in (0..b.len()).rev() {
        if b[i] == b'/' {
            if !matched_slash {
                start = i + 1;
                break;
            }
        } else if end == -1 {
            matched_slash = false;
            end = i as isize + 1;
        }
    }
    if end == -1 {
        return String::new();
    }
    path[start..end as usize].to_string()
}

/// `path.posix.extname`.
pub fn extname(path: &str) -> String {
    let b = path.as_bytes();
    let mut start_dot: isize = -1;
    let mut start_part: isize = 0;
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut pre_dot_state = 0;
    for i in (0..b.len()).rev() {
        let c = b[i];
        if c == b'/' {
            if !matched_slash {
                start_part = i as isize + 1;
                break;
            }
            continue;
        }
        if end == -1 {
            matched_slash = false;
            end = i as isize + 1;
        }
        if c == b'.' {
            if start_dot == -1 {
                start_dot = i as isize;
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot != -1 {
            pre_dot_state = -1;
        }
    }
    if start_dot == -1
        || end == -1
        || pre_dot_state == 0
        || (pre_dot_state == 1 && start_dot == end - 1 && start_dot == start_part + 1)
    {
        return String::new();
    }
    path[start_dot as usize..end as usize].to_string()
}

/// `path.posix.relative` for two paths sharing the same base. Node resolves
/// both against the working directory first; for the drive-absolute
/// (`C:/...`) paths used here both get the same prefix, which cancels out.
pub fn relative(from: &str, to: &str) -> String {
    if from == to {
        return String::new();
    }
    let f = normalize(&format!("/{from}"));
    let t = normalize(&format!("/{to}"));
    let fa: Vec<&str> = f.split('/').filter(|s| !s.is_empty()).collect();
    let ta: Vec<&str> = t.split('/').filter(|s| !s.is_empty()).collect();
    let mut i = 0;
    while i < fa.len() && i < ta.len() && fa[i] == ta[i] {
        i += 1;
    }
    let mut out: Vec<&str> = vec![".."; fa.len() - i];
    out.extend_from_slice(&ta[i..]);
    out.join("/")
}

/// `path.win32.dirname` for drive-absolute and relative paths (UNC roots are
/// not modelled).
pub fn win32_dirname(path: &str) -> String {
    let b = path.as_bytes();
    let sep = |c: u8| c == b'\\' || c == b'/';
    let mut root_end = 0usize;
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        root_end = if b.len() > 2 && sep(b[2]) { 3 } else { 2 };
    } else if !b.is_empty() && sep(b[0]) {
        root_end = 1;
    }
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut i = b.len();
    while i > root_end {
        i -= 1;
        if sep(b[i]) {
            if !matched_slash {
                end = i as isize;
                break;
            }
        } else {
            matched_slash = false;
        }
    }
    if end == -1 {
        if root_end == 0 {
            return ".".into();
        }
        end = root_end as isize;
    }
    path[..end as usize].to_string()
}

// ─── localeCompare ──────────────────────────────────────────────────────────

/// Primary weight of an ASCII character in the ICU root collation
/// (`String.prototype.localeCompare` without a locale): whitespace, then
/// punctuation and symbols in DUCET order, then digits, then letters with
/// case folded. Non-ASCII characters sort after ASCII by lowercased code
/// point - an approximation (see [`locale_compare`]).
fn primary(c: char) -> (u8, u32) {
    const PUNCT: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    match c {
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' => (0, c as u32),
        _ if PUNCT.contains(c) => (1, PUNCT.find(c).unwrap_or(0) as u32),
        '0'..='9' => (2, c as u32),
        'a'..='z' | 'A'..='Z' => (3, c.to_ascii_lowercase() as u32),
        _ if (c as u32) < 0x80 => (0, c as u32),
        _ => (4, c.to_lowercase().next().unwrap_or(c) as u32),
    }
}

/// Approximation of `a.localeCompare(b)` (ICU root collation, tertiary
/// strength): primary weights over the whole string, then lowercase before
/// uppercase at the first case difference, then code points. Exact for
/// ASCII paths made of letters, digits and the usual punctuation; other
/// scripts, contractions and ignorable characters are not modelled.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let pa: Vec<(u8, u32)> = a.chars().map(primary).collect();
    let pb: Vec<(u8, u32)> = b.chars().map(primary).collect();
    match pa.cmp(&pb) {
        Ordering::Equal => {}
        o => return o,
    }
    for (x, y) in a.chars().zip(b.chars()) {
        if x != y {
            let (xl, yl) = (x.is_lowercase(), y.is_lowercase());
            if xl && !yl {
                return Ordering::Less;
            }
            if yl && !xl {
                return Ordering::Greater;
            }
        }
    }
    a.cmp(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posix_path_functions_match_node() {
        assert_eq!(normalize("C:/a/./b/../c//d/"), "C:/a/c/d/");
        assert_eq!(normalize("../a/.."), "..");
        assert_eq!(normalize("/../a"), "/a");
        assert_eq!(join(&["C:/r/src", "a", "", "b.rs"]), "C:/r/src/a/b.rs");
        assert_eq!(join(&["C:/r/src/x", "..", "y"]), "C:/r/src/y");
        assert_eq!(join(&[]), ".");
        assert_eq!(dirname("C:/a/b.rs"), "C:/a");
        assert_eq!(dirname("C:"), ".");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(dirname("a/b/"), "a");
        assert_eq!(basename("C:/a/b.rs"), "b.rs");
        assert_eq!(basename("C:/a/b/"), "b");
        assert_eq!(extname("a/b.test.ts"), ".ts");
        assert_eq!(extname("a/.env"), "");
        assert_eq!(extname("a/b."), ".");
        assert_eq!(extname("a/b"), "");
        assert_eq!(relative("C:/r", "C:/r/x/y"), "x/y");
        assert_eq!(relative("C:/r/a", "C:/r/b"), "../b");
        assert_eq!(relative("C:/r", "C:/r"), "");
        assert_eq!(win32_dirname(r"C:\a\b.go"), r"C:\a");
        assert_eq!(win32_dirname(r"C:\b.go"), r"C:\");
        assert_eq!(win32_dirname("C:/a/b/c.go"), "C:/a/b");
    }

    #[test]
    fn comments_strip_like_the_js_regex() {
        assert_eq!(strip_line_comment("name = 'x' # c", "#"), "name = 'x' ");
        assert_eq!(strip_line_comment("a # b\rc", "#"), "a # b\rc");
        assert_eq!(strip_line_comment("a # b\rc # d", "#"), "a # b\rc ");
        assert_eq!(split_lines_crlf("a\r\nb\nc\r"), vec!["a", "b", "c\r"]);
    }

    #[test]
    fn locale_compare_follows_icu_root_order() {
        assert_eq!(locale_compare("a", "B"), Ordering::Less);
        assert_eq!(locale_compare("a", "A"), Ordering::Less);
        assert_eq!(locale_compare("a_b", "a-b"), Ordering::Less);
        assert_eq!(locale_compare("a-b", "a/b"), Ordering::Less);
        assert_eq!(locale_compare("a1", "aa"), Ordering::Less);
        assert_eq!(locale_compare("x/b", "x/a"), Ordering::Greater);
    }
}
