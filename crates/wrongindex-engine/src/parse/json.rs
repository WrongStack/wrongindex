//! Port of WrongStack `json-parser.ts` (root-object keys, package.json and
//! tsconfig special cases, JSON Schema / OpenAPI definitions). A scanner, not
//! a JSON parse: JSONC comments and trailing commas never drop a file.

use super::{Parsed, ParsedSymbol};
use crate::js::{self, U16, utf16};

pub const MAX_SYMBOLS_DEFAULT: usize = 1_000;

const QUOTE: u16 = b'"' as u16;
const SLASH: u16 = b'/' as u16;
const STAR: u16 = b'*' as u16;
const BACKSLASH: u16 = b'\\' as u16;

fn skip_trivia(c: &U16, from: usize) -> usize {
    let mut i = from;
    while i < c.len() {
        let ch = c[i];
        if matches!(ch, 0x20 | 0x09 | 0x0A | 0x0D | 0xFEFF) {
            i += 1;
            continue;
        }
        if ch == SLASH && c.get(i + 1) == Some(&SLASH) {
            i = js::index_of(c, &[0x0A], i).map_or(c.len(), |n| n + 1);
            continue;
        }
        if ch == SLASH && c.get(i + 1) == Some(&STAR) {
            i = js::index_of(c, &[STAR, SLASH], i + 2).map_or(c.len(), |e| e + 2);
            continue;
        }
        break;
    }
    i
}

struct ObjectKey {
    key: Vec<u16>,
    offset: usize,
    value_start: usize,
}

fn object_keys(c: &U16, open: usize) -> Vec<ObjectKey> {
    let mut keys = Vec::new();
    let mut depth: i64 = 0;
    let mut i = open;
    while i < c.len() {
        let ch = c[i];
        if ch == QUOTE {
            let mut j = i + 1;
            while j < c.len() && c[j] != QUOTE {
                if c[j] == BACKSLASH {
                    j += 1;
                }
                j += 1;
            }
            if j >= c.len() {
                break;
            }
            if depth == 1 {
                let colon = skip_trivia(c, j + 1);
                if c.get(colon) == Some(&(b':' as u16)) {
                    keys.push(ObjectKey {
                        key: c[i + 1..j].to_vec(),
                        offset: i,
                        value_start: skip_trivia(c, colon + 1),
                    });
                }
            }
            i = j + 1;
            continue;
        }
        if ch == SLASH && matches!(c.get(i + 1), Some(&SLASH) | Some(&STAR)) {
            // `i = skipTrivia(content, i) - 1; continue` then the loop's i++.
            i = skip_trivia(c, i);
            continue;
        }
        if ch == b'{' as u16 || ch == b'[' as u16 {
            depth += 1;
        } else if ch == b'}' as u16 || ch == b']' as u16 {
            depth -= 1;
            if depth == 0 {
                break;
            }
        }
        i += 1;
    }
    keys
}

const DEFINITION_CONTAINERS: [&str; 3] = ["$defs", "definitions", "schemas"];
const DEFINITION_SEARCH_DEPTH: usize = 4;

fn is_definition_container(k: &U16) -> bool {
    DEFINITION_CONTAINERS.iter().any(|d| utf16(d) == k)
}

fn is_block_container(k: &U16) -> bool {
    is_definition_container(k) || utf16("components") == k
}

fn quoted(key: &U16, suffix: &str) -> Vec<u16> {
    let mut s = vec![QUOTE];
    s.extend_from_slice(key);
    s.push(QUOTE);
    s.extend(utf16(suffix));
    s
}

struct Ctx<'a> {
    c: &'a U16,
    offsets: Vec<usize>,
    max: usize,
    symbols: Vec<ParsedSymbol>,
}

impl Ctx<'_> {
    fn push(&mut self, name: &U16, kind: &'static str, offset: usize, signature: &U16) {
        if self.symbols.len() >= self.max {
            return;
        }
        let line = js::line_from_offset(&self.offsets, offset);
        let col = offset - self.offsets[line - 1];
        self.symbols
            .push(ParsedSymbol::simple(kind, name, line, col, signature));
    }

    fn visit(&mut self, open: usize, depth: usize) {
        for entry in object_keys(self.c, open) {
            if self.symbols.len() >= self.max {
                return;
            }
            if self.c.get(entry.value_start) != Some(&(b'{' as u16)) {
                continue;
            }
            if is_block_container(&entry.key) {
                self.push(
                    &entry.key,
                    "property",
                    entry.offset,
                    &quoted(&entry.key, ": { ... }"),
                );
            }
            if is_definition_container(&entry.key) {
                for d in object_keys(self.c, entry.value_start) {
                    self.push(&d.key, "schema", d.offset, &quoted(&d.key, ": { ... }"));
                }
            }
            if depth < DEFINITION_SEARCH_DEPTH {
                self.visit(entry.value_start, depth + 1);
            }
        }
    }
}

pub fn parse(file: &str, content: &U16, max_symbols: Option<usize>) -> Parsed {
    let c = content;
    let base = js::basename(file);
    let lower = base.to_lowercase();
    let is_package = lower == "package.json";
    let is_tsconfig = lower == "tsconfig.json" || lower == "tsconfig.build.json";
    let is_schema = js::includes(c, "$schema")
        || js::includes(c, "$id")
        || js::includes(c, "$ref")
        || js::includes(c, "$defs");
    let is_openapi = js::includes(c, "openapi") || js::includes(c, "swagger");

    let lines = js::split_lines(c);
    let mut ctx = Ctx {
        c,
        offsets: js::line_offsets(&lines),
        max: max_symbols.unwrap_or(MAX_SYMBOLS_DEFAULT),
        symbols: Vec::new(),
    };

    let root_open = skip_trivia(c, 0);
    if c.get(root_open) != Some(&(b'{' as u16)) {
        return Parsed::default();
    }
    let root_line = js::line_from_offset(&ctx.offsets, root_open);
    let base16 = utf16(base);
    ctx.symbols.push(ParsedSymbol::simple(
        "object",
        &base16,
        root_line,
        0,
        &quoted(&base16, " = { ... }"),
    ));

    for k in object_keys(c, root_open) {
        if ctx.symbols.len() >= ctx.max {
            break;
        }
        let key = k.key.as_slice();
        let ks = js::string(key);
        let mut kind = "property";
        let mut signature = quoted(key, ": ...");
        if is_package {
            if matches!(
                ks.as_str(),
                "scripts"
                    | "dependencies"
                    | "devDependencies"
                    | "peerDependencies"
                    | "optionalDependencies"
            ) {
                kind = "const";
                signature = quoted(key, ": { ... }");
            }
        } else if is_tsconfig && ks == "compilerOptions" {
            signature = quoted(key, ": { ... }");
        }
        if (is_schema || is_openapi) && matches!(ks.as_str(), "$schema" | "$id" | "$ref") {
            kind = "schema";
            signature = quoted(key, ": \"...\"");
        }
        ctx.push(key, kind, k.offset, &signature);

        let object_value = c.get(k.value_start) == Some(&(b'{' as u16));
        if is_package && ks == "scripts" && object_value {
            for s in object_keys(c, k.value_start) {
                ctx.push(&s.key, "function", s.offset, &quoted(&s.key, ": \"...\""));
            }
        }
        if is_tsconfig && ks == "compilerOptions" && object_value {
            for o in object_keys(c, k.value_start) {
                ctx.push(&o.key, "property", o.offset, &quoted(&o.key, ": ..."));
            }
        }
    }

    if is_schema || is_openapi {
        ctx.visit(root_open, 0);
    }

    Parsed {
        symbols: ctx.symbols,
        unsupported: None,
    }
}
