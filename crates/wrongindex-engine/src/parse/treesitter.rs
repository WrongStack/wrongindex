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

use super::py::{SourceFile, par_chunks, private_script, resolve_command, run_chunk};
use super::{Batch, Degrade, Parsed, ParsedRef, ParsedSymbol, generic, strict, with_relations};
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

/// Parse files of [`LANGS`] as `parseFilesContent` does.
///
/// Without Bun or Node.js - or when the program cannot be set up or fails on
/// a chunk - the files go to the regex extractor, which is what
/// `tree-sitter-parser.ts` does when a grammar cannot be loaded, and the run
/// says so. In strict mode each of these is the run's error instead
/// (WrongStack itself always has its runtime).
pub fn parse_all(files: &[SourceFile<'_>]) -> Result<Batch, String> {
    parse_with(runtime(), files, strict())
}

/// `parseGeneric` + `withRelations`: `None` for a file whose infra artifact
/// the port does not write (never for the tree-sitter languages today).
fn regex_fallback(f: &SourceFile<'_>) -> Option<Parsed> {
    let content = utf16(f.text);
    let parsed = generic::parse(f.file, f.lang, &content);
    parsed
        .unsupported
        .is_none()
        .then(|| with_relations(parsed, &content, f.lang))
}

pub(super) fn parse_with(
    rt: Option<&Path>,
    files: &[SourceFile<'_>],
    strict: bool,
) -> Result<Batch, String> {
    let all_regex = |note: String| Batch {
        parsed: files.iter().map(regex_fallback).collect(),
        notes: vec![note],
    };
    let Some(rt) = rt else {
        if strict {
            return Err("tree-sitter languages need Bun or Node.js on PATH".into());
        }
        return Ok(all_regex(
            "tree-sitter languages: the regex extractor ran (no Bun or Node.js on PATH)".into(),
        ));
    };
    let setup = || -> Result<_, String> {
        let (dir, script) = private_script("wi-ts-parse-", "ts_batch.mjs", PROGRAM)?;
        for (rel, bytes) in ASSETS {
            let p = dir.path().join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("tree-sitter assets: {e}"))?;
            }
            std::fs::write(&p, bytes).map_err(|e| format!("tree-sitter assets: {e}"))?;
        }
        Ok((dir, script))
    };
    let (dir, script) = match setup() {
        Ok(v) => v,
        Err(e) if strict => return Err(e),
        Err(e) => {
            return Ok(all_regex(format!(
                "tree-sitter languages: the regex extractor ran ({e})"
            )));
        }
    };
    let degrade = Degrade::new(strict);
    let unparsed = std::sync::Mutex::new(Vec::<String>::new());
    let parsed = par_chunks(files, |chunk| {
        let helper = || {
            let mut out = Vec::with_capacity(chunk.len());
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
            Ok(out)
        };
        degrade.run(helper, || {
            chunk
                .iter()
                .map(|f| {
                    regex_fallback(f).unwrap_or_else(|| {
                        unparsed
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(f.file.to_string());
                        // Marked below: no rows for this file.
                        Parsed {
                            unsupported: Some(String::new()),
                            ..Parsed::default()
                        }
                    })
                })
                .collect()
        })
    })?;
    Ok(Batch {
        parsed: parsed
            .into_iter()
            .map(|p| p.unsupported.is_none().then_some(p))
            .collect(),
        notes: degrade
            .notes("tree-sitter languages: the regex extractor ran where the parser failed"),
    })
}

/// `ts-parser.ts` `parseSymbols` (TypeScript compiler) for TS/JS files the
/// oxc port refuses, then `withRelations` (TS/JS: refs stamped with `lang`).
///
/// There is no other parser for these files: without Bun or Node.js, or when
/// the program fails on a chunk, they are left unparsed (`None`) - no rows
/// are written for them in this run and their previous rows stay - and the
/// run says so. In strict mode that is the run's error instead.
pub fn parse_typescript_fallback(files: &[SourceFile<'_>]) -> Result<Batch, String> {
    typescript_with(runtime(), files, strict())
}

pub(super) fn typescript_with(
    rt: Option<&Path>,
    files: &[SourceFile<'_>],
    strict: bool,
) -> Result<Batch, String> {
    let none = |note: String| Batch {
        parsed: files.iter().map(|_| None).collect(),
        notes: vec![note],
    };
    let count = |n: usize| format!("{n} TS/JS file{}", if n == 1 { "" } else { "s" });
    let Some(rt) = rt else {
        if strict {
            return Err("TS/JS files the native parser refuses need Bun or Node.js on PATH".into());
        }
        return Ok(none(format!(
            "{} the in-process parser refuses left unparsed (no Bun or Node.js on PATH)",
            count(files.len())
        )));
    };
    let (dir, script) = match private_script("wi-tsc-parse-", "ts_parse.mjs", TS_PROGRAM) {
        Ok(v) => v,
        Err(e) if strict => return Err(e),
        Err(e) => {
            return Ok(none(format!(
                "{} the in-process parser refuses left unparsed ({e})",
                count(files.len())
            )));
        }
    };
    let degrade = Degrade::new(strict);
    let left = std::sync::atomic::AtomicUsize::new(0);
    let parsed = par_chunks(files, |chunk| {
        let helper = || {
            let mut out = Vec::with_capacity(chunk.len());
            let results = run_program(rt, &script, dir.path(), chunk)?;
            for f in chunk {
                let (_, symbols, refs) = results
                    .iter()
                    .rev()
                    .find(|(file, _, _)| file == f.file)
                    .ok_or_else(|| {
                    format!("TypeScript parser returned no result for {}", f.file)
                })?;
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
            Ok(out)
        };
        degrade.run(helper, || {
            left.fetch_add(chunk.len(), std::sync::atomic::Ordering::SeqCst);
            // Marked below: no rows for these files.
            chunk
                .iter()
                .map(|_| Parsed {
                    unsupported: Some(String::new()),
                    ..Parsed::default()
                })
                .collect()
        })
    })?;
    let left = left.into_inner();
    Ok(Batch {
        parsed: parsed
            .into_iter()
            .map(|p| p.unsupported.is_none().then_some(p))
            .collect(),
        notes: degrade.notes(&format!(
            "{} the in-process parser refuses left unparsed: the TypeScript parser failed",
            count(left)
        )),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> [SourceFile<'static>; 2] {
        [
            SourceFile {
                file: "src/a.java",
                text: "public class Alpha {\n    public int run() { return 1; }\n}\n",
                lang: "java",
            },
            SourceFile {
                file: "build.sh",
                text: "#!/bin/sh\nbeta() {\n  echo hi\n}\n",
                lang: "shell",
            },
        ]
    }

    /// A program that is not a JavaScript runtime: it exits non-zero.
    fn not_a_runtime() -> std::path::PathBuf {
        std::env::current_exe().unwrap()
    }

    #[test]
    fn without_a_runtime_the_regex_extractor_takes_the_files() {
        let files = files();
        let batch = parse_with(None, &files, false).unwrap();
        assert_eq!(batch.parsed.len(), 2);
        for (f, parsed) in files.iter().zip(&batch.parsed) {
            let content = utf16(f.text);
            let expected =
                with_relations(generic::parse(f.file, f.lang, &content), &content, f.lang);
            assert_eq!(parsed.as_ref(), Some(&expected), "{}", f.file);
        }
        let names: Vec<&str> = batch
            .parsed
            .iter()
            .flatten()
            .flat_map(|p| p.symbols.iter().map(|s| s.name.as_str()))
            .collect();
        assert!(
            names.contains(&"Alpha") && names.contains(&"beta"),
            "{names:?}"
        );
        assert_eq!(batch.notes.len(), 1);
        assert!(
            batch.notes[0].contains("no Bun or Node.js"),
            "{:?}",
            batch.notes
        );
    }

    #[test]
    fn strict_runs_refuse_instead() {
        let files = files();
        let err = parse_with(None, &files, true).unwrap_err();
        assert!(err.contains("need Bun or Node.js"), "{err}");
        let err = typescript_with(None, &files, true).unwrap_err();
        assert!(err.contains("need Bun or Node.js"), "{err}");
        assert!(parse_with(Some(&not_a_runtime()), &files, true).is_err());
    }

    #[test]
    fn a_failing_runtime_degrades_like_a_missing_one() {
        let files = files();
        let regex = parse_with(None, &files, false).unwrap();
        let degraded = parse_with(Some(&not_a_runtime()), &files, false).unwrap();
        assert_eq!(degraded.parsed, regex.parsed);
        assert_eq!(degraded.notes.len(), 1, "{:?}", degraded.notes);
        assert!(degraded.notes[0].contains("where the parser failed"));
    }

    #[test]
    fn typescript_files_without_a_parser_are_left_unparsed() {
        let files = [SourceFile {
            file: "src/broken.ts",
            text: "export function (",
            lang: "ts",
        }];
        for rt in [None, Some(not_a_runtime())] {
            let batch = typescript_with(rt.as_deref(), &files, false).unwrap();
            assert_eq!(batch.parsed, vec![None]);
            assert_eq!(batch.notes.len(), 1, "{:?}", batch.notes);
            assert!(
                batch.notes[0].starts_with("1 TS/JS file "),
                "{:?}",
                batch.notes
            );
        }
    }
}
