//! Python, as WrongStack parses it: its own `ast` extraction program
//! (`toolchain-scripts.ts` `PY_BATCH_SCRIPT`, vendored verbatim in
//! `toolchain/py_batch.py` by `scripts/oracle/extract-toolchain-scripts.ts`)
//! run by the same interpreter WrongStack would pick, over chunks of files
//! (`parser-batch.ts`), decoded like `parser-output.ts`. Without an
//! interpreter both fall back to the regex extractor (`py-parser.ts`).
//!
//! The interpreter only parses: `ast.parse` never executes the project's
//! code. It runs isolated (`-I`: no script directory, user site or
//! `PYTHON*` variables on `sys.path`) from a private temporary directory.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{Parsed, ParsedRef, ParsedSymbol, generic, with_relations};
use crate::js::{self, utf16};

const PY_BATCH_SCRIPT: &str = include_str!("toolchain/py_batch.py");
/// `MAX_BATCH_FILES` / `MAX_BATCH_BYTES` of `parser-batch.ts`.
const MAX_BATCH_FILES: usize = 100;
const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;

/// One Python file to parse.
pub struct PyFile<'a> {
    pub file: &'a str,
    /// The file text as the indexer decoded it (lossy UTF-8).
    pub text: &'a str,
}

/// `resolvePythonBinary()`: `python3`, `python`, `py` on Windows (`python3`,
/// `python` elsewhere), the first that answers `--version` with exit 0
/// within 5 s. Cached for the process, like WrongStack's.
pub fn python() -> Option<&'static Path> {
    static P: OnceLock<Option<PathBuf>> = OnceLock::new();
    P.get_or_init(|| {
        let names: &[&str] = if cfg!(windows) {
            &["python3", "python", "py"]
        } else {
            &["python3", "python"]
        };
        names
            .iter()
            .filter_map(|n| resolve_command(n))
            .find(|p| answers_version(p))
    })
    .as_deref()
}

/// `resolveWin32Command` followed by what Node's `spawn` (no shell) can
/// start: the first PATH x PATHEXT match; a `.cmd`/`.bat` shim cannot be
/// spawned without a shell, so such a candidate is unavailable.
fn resolve_command(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD;.VBS;.JS;.WS;.MSC".into())
            .to_lowercase()
            .split(';')
            .map(str::to_string)
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        let d = dir.to_string_lossy();
        let d = d.trim().trim_start_matches('"').trim_end_matches('"');
        if d.is_empty() {
            continue;
        }
        for ext in &exts {
            let full = Path::new(d).join(format!("{name}{ext}"));
            if full.is_file() {
                let spawnable = !cfg!(windows) || matches!(ext.as_str(), ".exe" | ".com");
                return spawnable.then_some(full);
            }
        }
    }
    None
}

fn hidden(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd
}

fn answers_version(p: &Path) -> bool {
    let Ok(mut child) = hidden(
        Command::new(p)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
    .spawn() else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return s.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Parse every file as `parseFilesContent` does for `py`: batches through the
/// interpreter, each result through `withRelations`. Without an interpreter,
/// `parseGeneric` + `withRelations`. A chunk the interpreter could not
/// complete (crash, timeout) is an error: WrongStack would then re-parse its
/// files one by one, which is not reproduced here.
pub fn parse_all(files: &[PyFile<'_>]) -> Result<Vec<Parsed>, String> {
    let Some(py) = python() else {
        return Ok(files
            .iter()
            .map(|f| {
                let content = utf16(f.text);
                with_relations(generic::parse(f.file, "py", &content), &content, "py")
            })
            .collect());
    };
    let dir = tempfile::Builder::new()
        .prefix("wi-py-parse-")
        .tempdir()
        .map_err(|e| format!("python parser directory: {e}"))?;
    let script = dir.path().join("batch.py");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&script)
        .and_then(|mut f| f.write_all(PY_BATCH_SCRIPT.as_bytes()))
        .map_err(|e| format!("python parser script: {e}"))?;
    let mut out = Vec::with_capacity(files.len());
    for chunk in chunks(files) {
        let results = run_chunk(py, &script, chunk)?;
        for f in chunk {
            let content = utf16(f.text);
            // `collectBatch` keeps the last entry without an error.
            let ok = results
                .iter()
                .rev()
                .find(|r| r.file == f.file && r.error.is_none());
            let parsed = match ok {
                Some(r) => Parsed {
                    symbols: r.symbols.clone(),
                    refs: r.refs.clone(),
                    unsupported: None,
                },
                // A per-file failure inside the batch (a syntax error): the
                // single-file program fails the same way and yields no
                // symbols and no refs.
                None if results.iter().any(|r| r.file == f.file) => Parsed::default(),
                None => return Err(format!("python parser returned no result for {}", f.file)),
            };
            out.push(with_relations(parsed, &content, "py"));
        }
    }
    Ok(out)
}

/// `chunkBatchFiles`.
fn chunks<'a, 'b>(files: &'b [PyFile<'a>]) -> Vec<&'b [PyFile<'a>]> {
    let mut out = Vec::new();
    let (mut start, mut bytes) = (0usize, 0usize);
    for (i, f) in files.iter().enumerate() {
        let size = f.text.len();
        if i > start && (i - start >= MAX_BATCH_FILES || bytes + size > MAX_BATCH_BYTES) {
            out.push(&files[start..i]);
            start = i;
            bytes = 0;
        }
        bytes += size;
    }
    if start < files.len() {
        out.push(&files[start..]);
    }
    out
}

struct FileResult {
    file: String,
    error: Option<String>,
    symbols: Vec<ParsedSymbol>,
    refs: Vec<ParsedRef>,
}

fn run_chunk(py: &Path, script: &Path, chunk: &[PyFile<'_>]) -> Result<Vec<FileResult>, String> {
    let payload: Vec<Value> = chunk
        .iter()
        .map(|f| serde_json::json!({"file": f.file, "content": f.text}))
        .collect();
    let payload = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    let mut child = hidden(
        Command::new(py)
            .arg("-I")
            .arg(script)
            .current_dir(script.parent().unwrap_or(Path::new(".")))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
    )
    .spawn()
    .map_err(|e| format!("cannot start {}: {e}", py.display()))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let writer = std::thread::spawn(move || {
        // A child that exits early closes the pipe; that is not an error here.
        let _ = stdin.write_all(&payload);
    });
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    // `batchTimeoutMs`.
    let timeout = Duration::from_millis((15_000 + chunk.len() as u64 * 1_500).min(120_000));
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("python parser timed out".into());
            }
        }
    };
    let _ = writer.join();
    let stdout = reader.join().unwrap_or_default();
    let text = String::from_utf8_lossy(&stdout);
    if !status.success() || text.trim().is_empty() {
        return Err(format!("python parser failed ({status})"));
    }
    decode_batch(text.trim())
}

/// `parseParserBatchOutput` + `collectBatch` + `toIndexSymbols`.
fn decode_batch(stdout: &str) -> Result<Vec<FileResult>, String> {
    let v: Value =
        serde_json::from_str(stdout).map_err(|e| format!("python parser output: {e}"))?;
    let Some(results) = v.get("results").and_then(Value::as_array) else {
        return Err("python parser output has no results".into());
    };
    let mut out = Vec::new();
    for entry in results {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let Some(file) = obj
            .get("file")
            .and_then(Value::as_str)
            .filter(|f| !f.is_empty())
        else {
            continue;
        };
        let error = obj
            .get("error")
            .and_then(Value::as_str)
            .filter(|e| !e.is_empty())
            .map(str::to_string);
        out.push(FileResult {
            file: file.to_string(),
            error,
            symbols: symbols(obj.get("symbols"))?,
            refs: dedupe(refs(obj.get("refs"))),
        });
    }
    Ok(out)
}

fn num(v: Option<&Value>, default: usize) -> usize {
    v.and_then(Value::as_u64).map_or(default, |n| n as usize)
}

fn string(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

/// Kinds the extraction program emits.
fn static_kind(k: &str) -> Option<&'static str> {
    [
        "class", "function", "method", "property", "var", "const", "type",
    ]
    .into_iter()
    .find(|s| *s == k)
}

/// `coerceSymbols` + `toIndexSymbols`.
fn symbols(v: Option<&Value>) -> Result<Vec<ParsedSymbol>, String> {
    let Some(list) = v.and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for s in list {
        let (Some(name), Some(kind)) = (
            s.get("name").and_then(Value::as_str),
            s.get("kind").and_then(Value::as_str),
        ) else {
            continue;
        };
        let kind = static_kind(kind).ok_or_else(|| format!("python symbol kind {kind}"))?;
        let sig16 = utf16(&string(s.get("signature")));
        let signature = js::string(&sig16[..sig16.len().min(500)]);
        let doc = string(s.get("doc"));
        let text = [name, signature.as_str(), doc.as_str()]
            .into_iter()
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        out.push(ParsedSymbol {
            kind,
            name: name.to_string(),
            line: num(s.get("line"), 1),
            col: num(s.get("col"), 0),
            signature,
            doc_comment: doc,
            scope: string(s.get("scope")),
            text,
            is_exported: None,
            end_line: None,
        });
    }
    Ok(out)
}

/// `coerceRefs`.
fn refs(v: Option<&Value>) -> Vec<ParsedRef> {
    let Some(list) = v.and_then(Value::as_array) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|r| {
            let to_name = r.get("toName")?.as_str().filter(|s| !s.is_empty())?;
            let call_type = ["call", "type_ref", "inherit", "implement", "import"]
                .into_iter()
                .find(|c| Some(*c) == r.get("callType").and_then(Value::as_str))?;
            Some(ParsedRef {
                to_name: to_name.to_string(),
                call_type,
                line: num(r.get("line"), 1),
                lang: "py",
                module: r
                    .get("module")
                    .and_then(Value::as_str)
                    .filter(|m| !m.is_empty())
                    .map(str::to_string),
            })
        })
        .collect()
}

/// `dedupeRefs`.
fn dedupe(refs: Vec<ParsedRef>) -> Vec<ParsedRef> {
    let mut seen = std::collections::HashSet::new();
    refs.into_iter()
        .filter(|r| {
            seen.insert((
                r.to_name.clone(),
                r.call_type,
                r.line,
                r.module.clone().unwrap_or_default(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_respect_file_and_byte_limits() {
        let big = "x".repeat(5 * 1024 * 1024);
        let small: Vec<PyFile<'_>> = (0..250)
            .map(|_| PyFile {
                file: "a.py",
                text: "",
            })
            .collect();
        assert_eq!(
            chunks(&small).iter().map(|c| c.len()).collect::<Vec<_>>(),
            [100, 100, 50]
        );
        let two = [
            PyFile {
                file: "a.py",
                text: &big,
            },
            PyFile {
                file: "b.py",
                text: &big,
            },
        ];
        assert_eq!(chunks(&two).len(), 2);
        let one = [PyFile {
            file: "a.py",
            text: &big,
        }];
        assert_eq!(chunks(&one).len(), 1);
    }

    #[test]
    fn decoding_coerces_like_parser_output() {
        let out = r#"{"version":1,"results":[
            {"file":"a.py","symbols":[{"name":"f","kind":"function","line":2,"col":4,"signature":"def f()","scope":"a.f","doc":"Doc."},{"kind":"class"}],
             "refs":[{"toName":"g","callType":"call","line":3},{"toName":"g","callType":"call","line":3},{"toName":"","callType":"call"},{"toName":"x","callType":"weird"},{"toName":"os","callType":"import","line":1,"module":""}]},
            {"file":"b.py","error":"invalid syntax","symbols":[],"refs":[]},
            null, {"file":""}]}"#;
        let r = decode_batch(out).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].symbols.len(), 1);
        assert_eq!(r[0].symbols[0].text, "f def f() Doc.");
        assert_eq!(r[0].refs.len(), 2);
        assert_eq!(r[0].refs[1].module, None);
        assert_eq!(r[1].error.as_deref(), Some("invalid syntax"));
    }
}
