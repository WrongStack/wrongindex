//! Port of WrongStack's regex symbol extractor (`generic-parser.ts`,
//! `parseGeneric`) for every language `parseFileContent` routes to it
//! (`parser-dispatch.ts`, `default` branch): scala, sql, md, toml, html,
//! css, vue, svelte, dart, lua, r, proto, graphql, zig, haskell, other.
//!
//! The patterns are the TypeScript source text, run by [`crate::jsre`]
//! (ECMAScript backtracking semantics over UTF-16). Patterns without a
//! capture group (`<script|template|style>`, zig's anonymous containers,
//! `target|PHONY`) never yield a name (`match[1] ?? match[2] ?? ''` is
//! empty) and are omitted. Infra artifacts (`infra-parser.ts`) are merged in
//! as the TypeScript does, see [`super::infra`].

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use super::{Parsed, ParsedSymbol, infra};
use crate::js::{self, U16};
use crate::jsre::Regex;

/// `GENERIC_MAX_SYMBOLS_DEFAULT`.
pub const GENERIC_MAX_SYMBOLS: usize = 500;
/// `GENERIC_MAX_FILE_CHARS` (UTF-16 units).
pub const GENERIC_MAX_FILE_CHARS: usize = 128 * 1024;

/// Languages `parseFileContent` hands to `parseGeneric`.
pub const LANGS: &[&str] = &[
    "scala", "sql", "md", "toml", "html", "css", "vue", "svelte", "dart", "lua", "r", "proto",
    "graphql", "zig", "haskell", "other",
];

/// `LANG_PATTERNS` (source, flags, kind) for [`LANGS`].
/// (source, flags, kind).
type Pattern = (&'static str, &'static str, &'static str);

const PATTERNS: &[(&str, &[Pattern])] = &[
    (
        "scala",
        &[
            (r"\b(?:def)\s+([A-Za-z_]\w*)", "g", "function"),
            (
                r"\b(?:class|object|trait|enum)\s+([A-Za-z_]\w*)",
                "g",
                "class",
            ),
        ],
    ),
    (
        "sql",
        &[(
            r#"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:TABLE|VIEW|INDEX|FUNCTION|PROCEDURE|TRIGGER)\s+(?:IF\s+NOT\s+EXISTS\s+)?([A-Za-z_"][\w."]*)"#,
            "gi",
            "type",
        )],
    ),
    ("md", &[(r"^(#{1,6})\s+(.+)$", "gm", "namespace")]),
    (
        "toml",
        &[(r"^\[\[?\s*([^[\]\s][^[\]]*?)\s*\]\]?", "gm", "namespace")],
    ),
    (
        "html",
        &[(r#"\bid\s*=\s*["']([^"']+)["']"#, "gi", "property")],
    ),
    (
        "css",
        &[
            (r"^[ \t]*([.#]?[A-Za-z_][\w-]*)\s*\{", "gm", "type"),
            (
                r"@(?:keyframes|media|supports)\s+([^{\s]+)",
                "g",
                "namespace",
            ),
        ],
    ),
    (
        "vue",
        &[(
            r"\b(?:function|const|let|var|class|export\s+(?:default\s+)?(?:function|class|const))\s+([A-Za-z_]\w*)",
            "g",
            "function",
        )],
    ),
    (
        "svelte",
        &[(
            r"\b(?:function|const|let|var|class|export\s+(?:default\s+)?(?:function|class|const))\s+([A-Za-z_]\w*)",
            "g",
            "function",
        )],
    ),
    (
        "dart",
        &[
            (
                r"\b(?:class|enum|mixin|extension)\s+([A-Za-z_]\w*)",
                "g",
                "class",
            ),
            (
                r"\b([A-Za-z_]\w*)\s*\([^;]*\)\s*(?:async\s*)?\{",
                "g",
                "function",
            ),
        ],
    ),
    (
        "lua",
        &[
            (r"\bfunction\s+([A-Za-z_.:]\w*)", "g", "function"),
            (r"\blocal\s+function\s+([A-Za-z_]\w*)", "g", "function"),
        ],
    ),
    (
        "r",
        &[
            (r"([A-Za-z_.][\w.]*)\s*<-\s*function\s*\(", "g", "function"),
            (r"([A-Za-z_.][\w.]*)\s*=\s*function\s*\(", "g", "function"),
        ],
    ),
    (
        "proto",
        &[
            (r"\b(?:message|service|enum)\s+([A-Za-z_]\w*)", "g", "type"),
            (r"\brpc\s+([A-Za-z_]\w*)", "g", "function"),
        ],
    ),
    (
        "graphql",
        &[
            (
                r"\b(?:type|interface|enum|input|union|scalar)\s+([A-Za-z_]\w*)",
                "g",
                "type",
            ),
            (
                r"\b(?:query|mutation|subscription)\s+([A-Za-z_]\w*)",
                "g",
                "function",
            ),
        ],
    ),
    (
        "zig",
        &[
            (r"\b(?:fn|pub\s+fn)\s+([A-Za-z_]\w*)", "g", "function"),
            (r"\b(?:const|var)\s+([A-Za-z_]\w*)", "g", "const"),
        ],
    ),
    (
        "haskell",
        &[
            (r"^module\s+([A-Z][\w.]*)", "gm", "namespace"),
            (r"^([A-Za-z_]\w*)\s*::", "gm", "function"),
            (r"\bdata\s+([A-Za-z_]\w*)", "g", "type"),
            (r"\btype\s+(?:family\s+)?([A-Za-z_]\w*)", "g", "type"),
            (r"\bclass\s+([A-Za-z_]\w*)", "g", "class"),
        ],
    ),
    (
        "other",
        &[
            (
                r"^(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_]\w*)",
                "gm",
                "function",
            ),
            (r"^(?:export\s+)?class\s+([A-Za-z_]\w*)", "gm", "class"),
            (
                r"^(?:export\s+)?(?:const|let|var)\s+([A-Za-z_]\w*)",
                "gm",
                "const",
            ),
            (r"^(?:async\s+)?def\s+([A-Za-z_]\w*)", "gm", "function"),
            (r"^func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)", "gm", "function"),
            (r"\bfn\s+([A-Za-z_]\w*)", "g", "function"),
        ],
    ),
];

const KEYWORDS: &[&str] = &[
    "if",
    "else",
    "for",
    "while",
    "switch",
    "case",
    "return",
    "break",
    "continue",
    "new",
    "delete",
    "typeof",
    "instanceof",
    "void",
    "null",
    "true",
    "false",
    "this",
    "super",
    "import",
    "export",
    "from",
    "as",
    "default",
    "public",
    "private",
    "protected",
    "static",
    "final",
    "class",
    "struct",
    "enum",
    "interface",
    "function",
    "def",
    "fn",
    "func",
    "var",
    "let",
    "const",
    "package",
    "namespace",
    "module",
    "using",
    "include",
    "require",
    "select",
    "where",
    "and",
    "or",
    "not",
    "in",
    "is",
    "try",
    "catch",
    "finally",
    "throw",
    "async",
    "await",
    "yield",
];

type Compiled = HashMap<&'static str, Vec<(Regex, &'static str)>>;

fn compiled() -> &'static Compiled {
    static C: OnceLock<Compiled> = OnceLock::new();
    C.get_or_init(|| {
        PATTERNS
            .iter()
            .map(|(lang, ps)| {
                (
                    *lang,
                    ps.iter()
                        .map(|(src, flags, kind)| (Regex::new(src, flags), *kind))
                        .collect(),
                )
            })
            .collect()
    })
}

fn name_filter() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[A-Za-z_#.@/\w][\w.\-:/#!?]*$", ""))
}

fn fence_marker() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\s{0,3}(`{3,}|~{3,})", ""))
}

/// `looksBinary`.
pub fn looks_binary(c: &U16) -> bool {
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

/// `maskMarkdownFences`: blank fenced code block bodies (every unit but
/// `\r`), keeping offsets and newlines.
pub fn mask_markdown_fences(content: &U16) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(content.len());
    let mut fence: Option<Vec<u16>> = None;
    for (i, line) in content.split(|&c| c == 0x0A).enumerate() {
        if i > 0 {
            out.push(0x0A);
        }
        let marker = fence_marker()
            .exec(line)
            .and_then(|m| m.text(line, 1).map(<[u16]>::to_vec));
        match &fence {
            None => {
                if marker.is_some() {
                    fence = marker;
                }
                out.extend_from_slice(line);
            }
            Some(f) => {
                if let Some(m) = &marker
                    && m[0] == f[0]
                    && m.len() >= f.len()
                {
                    fence = None;
                    out.extend_from_slice(line);
                    continue;
                }
                out.extend(line.iter().map(|&c| if c == 0x0D { c } else { 0x20 }));
            }
        }
    }
    out
}

/// `parseGeneric({ file, content, lang })` (before `withRelations`): the
/// language's regex symbols, then the infra artifact's symbols and refs.
pub fn parse(file: &str, lang: &'static str, content: &U16) -> Parsed {
    let mut out = Parsed::default();
    if content.is_empty() || looks_binary(content) {
        return out;
    }
    let bounded = &content[..content.len().min(GENERIC_MAX_FILE_CHARS)];
    let masked;
    let s: &U16 = if lang == "md" {
        masked = mask_markdown_fences(bounded);
        &masked
    } else {
        bounded
    };
    let is_md = lang == "md";
    let prose = is_md || lang == "toml";
    let newlines: Vec<usize> = s
        .iter()
        .enumerate()
        .filter(|(_, c)| **c == 10)
        .map(|(i, _)| i)
        .collect();
    let mut seen: HashSet<(Vec<u16>, usize, &'static str)> = HashSet::new();
    let symbols = &mut out.symbols;
    let patterns = compiled()
        .get(lang)
        .unwrap_or_else(|| panic!("no generic patterns for {lang}"));
    'patterns: for (re, kind) in patterns {
        for m in re.match_all(s) {
            if symbols.len() >= GENERIC_MAX_SYMBOLS {
                break 'patterns;
            }
            // `(match[1] ?? match[2] ?? '').trim()`, then markdown's group 2.
            let g1 = m.text(s, 1);
            let g2 = m.text(s, 2);
            let mut name: &U16 = js::trim(g1.or(g2).unwrap_or(&[]));
            if is_md && let Some(t) = g2.filter(|t| !t.is_empty()) {
                name = js::trim(t);
            }
            if name.is_empty() || name.len() > 200 {
                continue;
            }
            // `.replace(/^#+\s*/, '').replace(/["'`]/g, '')`
            let mut n = name;
            if n.first() == Some(&(b'#' as u16)) {
                let mut i = n.iter().take_while(|&&c| c == b'#' as u16).count();
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
            if !prose {
                let text = js::string(&name);
                if KEYWORDS.contains(&text.as_str()) || !name_filter().test(&name) {
                    continue;
                }
            }
            let index = m.start();
            // `lineColAt`: newlines strictly before `index`.
            let low = newlines.partition_point(|&o| o < index);
            let line = low + 1;
            let col = index - if low > 0 { newlines[low - 1] + 1 } else { 0 };
            if !seen.insert((name.clone(), line, kind)) {
                continue;
            }
            let nl = js::index_of(s, &[10], index).unwrap_or(s.len());
            let line_text = &s[index..nl];
            let sig = js::trim(if line_text.is_empty() {
                &name
            } else {
                line_text
            });
            let signature = &sig[..sig.len().min(500)];
            let mut text = name.clone();
            text.push(0x20);
            text.extend_from_slice(signature);
            let text = js::trim(&text);
            symbols.push(ParsedSymbol {
                kind: if is_md { "namespace" } else { kind },
                name: js::string(&name[..name.len().min(200)]),
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
    }

    let infra = infra::parse(file, bounded, lang);
    for sym in infra.symbols {
        if symbols.len() >= GENERIC_MAX_SYMBOLS {
            break;
        }
        if !seen.insert((js::utf16(&sym.name), sym.line, sym.kind)) {
            continue;
        }
        symbols.push(sym);
    }
    out.refs = infra.refs;
    out.unsupported = infra.unsupported;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js::utf16;

    fn names(file: &str, lang: &'static str, src: &str) -> Vec<(String, usize, usize, String)> {
        parse(file, lang, &utf16(src))
            .symbols
            .into_iter()
            .map(|s| (s.name, s.line, s.col, s.signature))
            .collect()
    }

    #[test]
    fn toml_tables_and_arrays_of_tables() {
        assert_eq!(
            names(
                "C:/p/Cargo.toml",
                "toml",
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
        assert!(names("a.toml", "toml", "x[a]\n  [b]\n").is_empty());
        assert!(names("a.toml", "toml", "[a\0]").is_empty());
    }

    #[test]
    fn markdown_headings_skip_fences() {
        assert_eq!(
            names(
                "C:/p/README.md",
                "md",
                "# Title\n```bash\n# install\n```\n## \"Quoted\" `x`\n####### seven\n"
            ),
            vec![
                ("Title".into(), 1, 0, "# Title".into()),
                ("Quoted x".into(), 5, 0, "## \"Quoted\" `x`".into()),
            ]
        );
    }

    #[test]
    fn keywords_and_punctuation_are_rejected() {
        assert_eq!(
            names("a.css", "css", ".btn {\n@media (x) {\nfor {\n"),
            vec![(".btn".into(), 1, 0, ".btn {".into())]
        );
    }
}
