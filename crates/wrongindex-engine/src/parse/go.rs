//! Go, as WrongStack parses it: its own `go/ast` extraction program
//! (`GO_BATCH_SCRIPT`, vendored in `toolchain/go_batch.go`) built once per
//! run (`go run` = build + run) and run over chunks of files (`runGoBatch`), and `go-parser.ts`'s regex
//! extractor (`parseGoWithoutToolchain`) for files the program rejects and
//! for machines without Go.
//!
//! `go build` builds only the vendored program, from a private temporary
//! directory, with `GOTOOLCHAIN=local` and `GOWORK=off` (`goSpawnOptions`):
//! the indexed project's `go.mod`/`go.work` are never read and none of its
//! code is built or run.

use std::process::Command;
use std::sync::OnceLock;

use super::py::{SourceFile, chunks, decode_batch, private_script, resolve_command, run_chunk};
use super::{Parsed, ParsedSymbol, with_relations};
use crate::js::{self, U16, utf16};
use crate::jsre::Regex;

const GO_BATCH_SCRIPT: &str = include_str!("toolchain/go_batch.go");

/// `resolveWin32Command('go')`, when Node could spawn the result.
pub fn go() -> Option<&'static std::path::Path> {
    static P: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    P.get_or_init(|| resolve_command("go")).as_deref()
}

/// `parseFilesContent` for `go`: `runGoBatch` per chunk; a file the program
/// reports an error for goes through the single-file route, whose program
/// fails the same way (`extract` error -> no symbols) and so ends in the
/// regex extractor without refs. Every result then gets `withRelations`.
///
/// A chunk `go run` cannot complete is an error (WrongStack would retry per
/// file, which is not reproduced).
pub fn parse_all(files: &[SourceFile<'_>]) -> Result<Vec<Parsed>, String> {
    let finish = |parsed: Parsed, f: &SourceFile<'_>| {
        let content = utf16(f.text);
        with_relations(parsed, &content, "go")
    };
    let Some(go) = go() else {
        return Ok(files
            .iter()
            .map(|f| finish(without_toolchain(&utf16(f.text)), f))
            .collect());
    };
    let (dir, script) = private_script("wi-go-parse-", "batch.go", GO_BATCH_SCRIPT)?;
    // `go run` is build + run. Building once, with a deadline that covers a
    // cold build cache, keeps the per-chunk `batchTimeoutMs` about parsing
    // (a cold `go run` can exceed it, which made results timing-dependent);
    // the program, and so its output, is the same.
    let exe = dir
        .path()
        .join(if cfg!(windows) { "batch.exe" } else { "batch" });
    build(go, &script, &exe)?;
    let mut out = Vec::with_capacity(files.len());
    for chunk in chunks(files) {
        let cmd = Command::new(&exe);
        let stdout = run_chunk(cmd, &script, chunk)?
            .ok_or_else(|| "go parser failed on a chunk".to_string())?;
        let results = decode_batch(&stdout, "go")?;
        for f in chunk {
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
                None if results.iter().any(|r| r.file == f.file) => {
                    without_toolchain(&utf16(f.text))
                }
                None => return Err(format!("go parser returned no result for {}", f.file)),
            };
            out.push(finish(parsed, f));
        }
    }
    Ok(out)
}

/// Deadline for building the parser program (cold build cache included).
const BUILD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(600);

/// `go build -o <exe> batch.go` from the program's private directory with
/// `goSpawnOptions`' environment.
fn build(
    go: &std::path::Path,
    script: &std::path::Path,
    exe: &std::path::Path,
) -> Result<(), String> {
    let mut cmd = Command::new(go);
    cmd.arg("build")
        .arg("-o")
        .arg(exe)
        .arg(script)
        .current_dir(script.parent().unwrap_or(std::path::Path::new(".")))
        .env("GOTOOLCHAIN", "local")
        .env("GOWORK", "off")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = super::py::hidden(&mut cmd)
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", go.display()))?;
    let deadline = std::time::Instant::now() + BUILD_DEADLINE;
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => return Ok(()),
            Ok(Some(s)) => return Err(format!("go build of the parser failed ({s})")),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("go build of the parser timed out".into());
            }
        }
    }
}

fn re(cell: &'static OnceLock<Regex>, src: &str, flags: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(src, flags))
}

macro_rules! regex {
    ($src:expr, $flags:expr) => {{
        static CELL: OnceLock<Regex> = OnceLock::new();
        re(&CELL, $src, $flags)
    }};
}

/// `maskGoNonCode`: comments and string/rune/raw-string bodies blanked
/// (every UTF-16 unit but `\n` becomes a space), offsets kept.
pub fn mask_non_code(src: &U16) -> Vec<u16> {
    const NL: u16 = b'\n' as u16;
    let blank = |out: &mut Vec<u16>, s: &[u16]| {
        out.extend(s.iter().map(|&c| if c == NL { NL } else { 0x20 }));
    };
    let n = src.len();
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let c = src[i];
        let next = src.get(i + 1).copied();
        if c == b'/' as u16 && next == Some(b'/' as u16) {
            let stop = src[i..].iter().position(|&x| x == NL).map_or(n, |p| i + p);
            blank(&mut out, &src[i..stop]);
            i = stop;
            continue;
        }
        if c == b'/' as u16 && next == Some(b'*' as u16) {
            let end = js::index_of(src, &utf16("*/"), i + 2);
            let stop = end.map_or(n, |e| e + 2);
            blank(&mut out, &src[i..stop]);
            i = stop;
            continue;
        }
        if c == b'"' as u16 || c == b'\'' as u16 || c == b'`' as u16 {
            let raw = c == b'`' as u16;
            let mut j = i + 1;
            while j < n && src[j] != c && (raw || src[j] != NL) {
                j += if !raw && src[j] == b'\\' as u16 { 2 } else { 1 };
            }
            if j < n && src[j] == c {
                out.push(c);
                blank(&mut out, &src[i + 1..j]);
                out.push(c);
                i = j + 1;
            } else {
                let stop = j.min(n);
                out.push(c);
                blank(&mut out, &src[i + 1..stop]);
                i = stop;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `hasUnbalancedDelimiters`.
fn unbalanced(code: &U16) -> bool {
    let mut stack: Vec<u16> = Vec::new();
    for &ch in code {
        let close = match ch {
            0x28 => Some(0x29),
            0x5B => Some(0x5D),
            0x7B => Some(0x7D),
            _ => None,
        };
        if let Some(c) = close {
            stack.push(c);
        } else if matches!(ch, 0x29 | 0x5D | 0x7D) && stack.pop() != Some(ch) {
            return true;
        }
    }
    !stack.is_empty()
}

/// `parseGoWithoutToolchain(file, content)` (symbols only, no refs).
pub fn without_toolchain(content: &U16) -> Parsed {
    let code = mask_non_code(content);
    let pkg_re = regex!(r"^\s*package\s+([A-Za-z_]\w*)", "m");
    if !pkg_re.test(&code) || unbalanced(&code) {
        return Parsed::default();
    }
    let package: Vec<u16> = pkg_re
        .exec(&code)
        .and_then(|m| m.text(&code, 1).map(<[u16]>::to_vec))
        .unwrap_or_default();
    let qualify = |parts: &[&U16]| -> Vec<u16> {
        let mut all: Vec<&U16> = vec![&package];
        all.extend_from_slice(parts);
        let kept: Vec<&U16> = all.into_iter().filter(|p| !p.is_empty()).collect();
        kept.join(&(b'.' as u16))
    };
    let func_re = regex!(
        r"^func\s+(?:\(\s*(?:[A-Za-z_]\w*\s+)?\*?\s*([A-Za-z_]\w*)(?:\[[^\]]*\])?\s*\)\s*)?([A-Za-z_]\w*)\s*[[(]",
        ""
    );
    let names_re = regex!(r"^([A-Za-z_]\w*(?:\s*,\s*[A-Za-z_]\w*)*)", "");
    let code_lines = js::split_lines(&code);
    let src_lines = js::split_lines(content);
    let mut symbols = Vec::new();
    let (mut braces, mut parens) = (0i64, 0i64);
    let mut group: Option<&'static str> = None;
    for (idx, line) in code_lines.iter().enumerate() {
        let trimmed = js::trim_start(line);
        let mut add = |kind: &'static str, name: &U16, scope: &U16| {
            if name == utf16("_").as_slice() {
                return;
            }
            let sig = js::trim(src_lines.get(idx).copied().unwrap_or(&[]));
            let mut sym =
                ParsedSymbol::simple(kind, name, idx + 1, line.len() - trimmed.len(), sig);
            sym.scope = js::string(scope);
            symbols.push(sym);
        };
        if braces == 0 && parens == 0 {
            group = None;
            let opener = regex!(r"^(const|var|type)\s*\(", "");
            if let Some(m) = func_re.exec(trimmed)
                && let Some(name) = m.text(trimmed, 2).filter(|t| !t.is_empty())
            {
                match m.text(trimmed, 1).filter(|t| !t.is_empty()) {
                    Some(recv) => add("method", name, &qualify(&[recv, name])),
                    None => add("function", name, &qualify(&[name])),
                }
            } else if let Some(g) = opener.exec(trimmed).and_then(|m| m.text(trimmed, 1)) {
                group = Some(static_group(g));
            } else {
                let type_re = regex!(r"^type\s+([A-Za-z_]\w*)", "");
                if let Some(t) = type_re.exec(trimmed).and_then(|m| m.text(trimmed, 1))
                    && !t.is_empty()
                {
                    add("type", t, &package);
                }
                let value_re = regex!(r"^(const|var)\s+", "");
                if let Some(m) = value_re.exec(trimmed) {
                    let kind = static_group(m.text(trimmed, 1).unwrap_or(&[]));
                    let rest = &trimmed[m.end()..];
                    if let Some(list) = names_re
                        .exec(rest)
                        .and_then(|n| n.text(rest, 1))
                        .filter(|t| !t.is_empty())
                    {
                        for name in list.split(|&c| c == b',' as u16) {
                            add(kind, js::trim(name), &package);
                        }
                    }
                }
            }
        } else if let Some(g) = group
            && braces == 0
            && parens == 1
            && let Some(list) = names_re
                .exec(trimmed)
                .and_then(|n| n.text(trimmed, 1))
                .filter(|t| !t.is_empty())
        {
            let mut members = list.split(|&c| c == b',' as u16);
            if g == "type" {
                add(g, js::trim(members.next().unwrap_or(&[])), &package);
            } else {
                for name in members {
                    add(g, js::trim(name), &package);
                }
            }
        }
        for &ch in *line {
            match ch {
                0x7B => braces += 1,
                0x7D => braces -= 1,
                0x28 => parens += 1,
                0x29 => parens -= 1,
                _ => {}
            }
        }
    }
    Parsed {
        symbols,
        ..Parsed::default()
    }
}

fn static_group(g: &U16) -> &'static str {
    match js::string(g).as_str() {
        "const" => "const",
        "var" => "var",
        _ => "type",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(src: &str) -> Vec<(String, &'static str, usize, usize, String)> {
        without_toolchain(&utf16(src))
            .symbols
            .into_iter()
            .map(|s| (s.name, s.kind, s.line, s.col, s.scope))
            .collect()
    }

    #[test]
    fn fallback_extracts_declarations_like_go_parser_ts() {
        let src = "// c\npackage util\n\nfunc Add(a, b int) int { return a + b }\nfunc (s *Set[T]) Has(x T) bool {\n\treturn false\n}\nconst (\n\tA, B = 1, 2\n\t_ = 3\n)\ntype (\n\tX struct{}\n)\nvar v, w int\ntype T int\nvar s = \"func Fake() {\"\n";
        assert_eq!(
            names(src),
            vec![
                ("Add".into(), "function", 4, 0, "util.Add".into()),
                ("Has".into(), "method", 5, 0, "util.Set.Has".into()),
                ("A".into(), "const", 9, 1, "util".into()),
                ("B".into(), "const", 9, 1, "util".into()),
                ("X".into(), "type", 13, 1, "util".into()),
                ("v".into(), "var", 15, 0, "util".into()),
                ("w".into(), "var", 15, 0, "util".into()),
                ("T".into(), "type", 16, 0, "util".into()),
                ("s".into(), "var", 17, 0, "util".into()),
            ]
        );
    }

    #[test]
    fn fallback_refuses_unbalanced_or_package_less_code() {
        assert!(names("package x\nfunc F( {\n").is_empty());
        assert!(names("func F() {}\n").is_empty());
    }
}
