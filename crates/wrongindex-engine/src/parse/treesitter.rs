//! The ten languages WrongStack parses with tree-sitter (`tree-sitter-parser.ts`,
//! `tree-sitter/visitor.ts`, `tree-sitter/queries.ts`), run as WrongStack
//! runs them: the same visitor and query code, the same `web-tree-sitter`
//! runtime and the same grammar WASM files, bundled by
//! `scripts/oracle/bundle-tree-sitter.ts` into `tree-sitter/` and embedded
//! here, executed by Bun or Node.js over chunks of files.
//!
//! As in `parser-dispatch.ts`, a file the grammar yields no symbols for (or
//! fails on) goes to the regex extractor (`generic-parser.ts`, including its
//! infra artifacts), and every result then gets `withRelations`.
//!
//! The JavaScript runtime only runs the bundled program from a private
//! temporary directory; grammars parse the source, nothing of the project is
//! executed.

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use serde_json::Value;

use super::py::{SourceFile, chunks, private_script, resolve_command, run_chunk};
use super::{Parsed, ParsedRef, ParsedSymbol, generic, with_relations};
use crate::js::utf16;

/// `LANG_TO_GRAMMAR` (no opt-ins).
pub const LANGS: &[&str] = &[
    "c", "cpp", "java", "csharp", "php", "ruby", "swift", "kotlin", "shell", "elixir",
];

macro_rules! asset {
    ($p:literal) => {
        (
            $p,
            include_bytes!(concat!("../../tree-sitter/", $p)).as_slice(),
        )
    };
}

/// The bundled program, runtime and grammars (see `tree-sitter/provenance.json`).
const ASSETS: &[(&str, &[u8])] = &[
    asset!("tree-sitter-runtime.wasm"),
    asset!("c/tree-sitter-c.wasm"),
    asset!("cpp/tree-sitter-cpp.wasm"),
    asset!("java/tree-sitter-java.wasm"),
    asset!("c_sharp/tree-sitter-c_sharp.wasm"),
    asset!("php/tree-sitter-php.wasm"),
    asset!("ruby/tree-sitter-ruby.wasm"),
    asset!("swift/tree-sitter-swift.wasm"),
    asset!("kotlin/tree-sitter-kotlin.wasm"),
    asset!("bash/tree-sitter-bash.wasm"),
    asset!("elixir/tree-sitter-elixir.wasm"),
];
const PROGRAM: &str = include_str!("../../tree-sitter/ts_batch.mjs");
/// WrongStack's TypeScript-compiler parser, for the TS/JS files the oxc port
/// refuses ([`super::ts`] gates).
const TS_PROGRAM: &str = include_str!("../../tree-sitter/ts_parse.mjs");

/// Bun (WrongStack's runtime), else Node.js.
pub fn runtime() -> Option<&'static Path> {
    static P: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    P.get_or_init(|| resolve_command("bun").or_else(|| resolve_command("node")))
        .as_deref()
}

/// Every symbol kind of WrongStack's `SymbolKind`.
fn static_kind(k: &str) -> Option<&'static str> {
    [
        "class",
        "interface",
        "enum",
        "type",
        "function",
        "method",
        "var",
        "const",
        "let",
        "property",
        "parameter",
        "namespace",
        "object",
        "literal",
        "schema",
        "struct",
        "trait",
        "impl",
        "static",
        "mod",
    ]
    .into_iter()
    .find(|s| *s == k)
}

/// Parse files of [`LANGS`] as `parseFilesContent` does. Without Bun or
/// Node.js the files are refused (WrongStack itself always has its runtime).
pub fn parse_all(files: &[SourceFile<'_>]) -> Result<Vec<Parsed>, String> {
    let Some(rt) = runtime() else {
        return Err("tree-sitter languages need Bun or Node.js on PATH".into());
    };
    let (dir, script) = private_script("wi-ts-parse-", "ts_batch.mjs", PROGRAM)?;
    for (rel, bytes) in ASSETS {
        let p = dir.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("tree-sitter assets: {e}"))?;
        }
        std::fs::write(&p, bytes).map_err(|e| format!("tree-sitter assets: {e}"))?;
    }
    let mut out = Vec::with_capacity(files.len());
    for chunk in chunks(files) {
        let results = run_program(rt, &script, dir.path(), chunk)?;
        for f in chunk {
            let content = utf16(f.text);
            let found = results.iter().rev().find(|(file, _, _)| file == f.file);
            let parsed = match found {
                Some((_, symbols, refs)) if !symbols.is_empty() => Parsed {
                    symbols: symbols.clone(),
                    refs: refs
                        .iter()
                        .cloned()
                        .map(|mut r| {
                            r.lang = f.lang;
                            r
                        })
                        .collect(),
                    unsupported: None,
                },
                // No symbols (or a failure): the regex extractor.
                Some(_) => generic::parse(f.file, f.lang, &content),
                None => return Err(format!("tree-sitter returned no result for {}", f.file)),
            };
            if let Some(why) = &parsed.unsupported {
                return Err(format!("{why} ({})", f.file));
            }
            out.push(with_relations(parsed, &content, f.lang));
        }
    }
    Ok(out)
}

/// `ts-parser.ts` `parseSymbols` (TypeScript compiler) for TS/JS files the
/// oxc port refuses, then `withRelations` (TS/JS: refs stamped with `lang`).
pub fn parse_typescript_fallback(files: &[SourceFile<'_>]) -> Result<Vec<Parsed>, String> {
    let Some(rt) = runtime() else {
        return Err("TS/JS files the native parser refuses need Bun or Node.js on PATH".into());
    };
    let (dir, script) = private_script("wi-tsc-parse-", "ts_parse.mjs", TS_PROGRAM)?;
    let mut out = Vec::with_capacity(files.len());
    for chunk in chunks(files) {
        let results = run_program(rt, &script, dir.path(), chunk)?;
        for f in chunk {
            let (_, symbols, refs) = results
                .iter()
                .rev()
                .find(|(file, _, _)| file == f.file)
                .ok_or_else(|| format!("TypeScript parser returned no result for {}", f.file))?;
            out.push(Parsed {
                symbols: symbols.clone(),
                refs: refs
                    .iter()
                    .cloned()
                    .map(|mut r| {
                        r.lang = f.lang;
                        r
                    })
                    .collect(),
                unsupported: None,
            });
        }
    }
    Ok(out)
}

fn run_program(
    rt: &Path,
    script: &Path,
    dir: &Path,
    chunk: &[SourceFile<'_>],
) -> Result<Vec<FileOut>, String> {
    let mut cmd = Command::new(rt);
    cmd.arg(script).arg(dir);
    let stdout = run_chunk(cmd, script, chunk)?
        .ok_or_else(|| format!("{} failed on a chunk", script.display()))?;
    decode(&stdout)
}

type FileOut = (String, Vec<ParsedSymbol>, Vec<ParsedRef>);

fn decode(stdout: &str) -> Result<Vec<FileOut>, String> {
    let v: Value = serde_json::from_str(stdout).map_err(|e| format!("tree-sitter output: {e}"))?;
    let results = v
        .get("results")
        .and_then(Value::as_array)
        .ok_or("tree-sitter output has no results")?;
    let mut out = Vec::new();
    for r in results {
        let file = r["file"].as_str().unwrap_or_default().to_string();
        let mut symbols = Vec::new();
        for s in r["symbols"].as_array().into_iter().flatten() {
            let kind = s["kind"].as_str().unwrap_or_default();
            let kind =
                static_kind(kind).ok_or_else(|| format!("tree-sitter symbol kind {kind}"))?;
            let text = |k: &str| s[k].as_str().unwrap_or_default().to_string();
            symbols.push(ParsedSymbol {
                kind,
                name: text("name"),
                line: s["line"].as_u64().unwrap_or(1) as usize,
                col: s["col"].as_u64().unwrap_or(0) as usize,
                signature: text("signature"),
                doc_comment: text("docComment"),
                scope: text("scope"),
                text: text("text"),
                is_exported: s["isExported"].as_bool(),
                end_line: s["endLine"].as_u64().map(|n| n as usize),
            });
        }
        let mut refs = Vec::new();
        for x in r["refs"].as_array().into_iter().flatten() {
            let call_type = ["call", "type_ref", "inherit", "implement", "import"]
                .into_iter()
                .find(|c| Some(*c) == x["callType"].as_str())
                .ok_or_else(|| format!("tree-sitter ref call type {}", x["callType"]))?;
            refs.push(ParsedRef {
                to_name: x["toName"].as_str().unwrap_or_default().to_string(),
                call_type,
                line: x["line"].as_u64().unwrap_or(1) as usize,
                lang: "",
                module: x["module"].as_str().map(str::to_string),
            });
        }
        out.push((file, symbols, refs));
    }
    Ok(out)
}
