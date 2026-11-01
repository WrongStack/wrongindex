//! WrongIndex native engine (M5).
//!
//! Capabilities are ported from WrongStack one at a time and each is gated by
//! differential evidence against the original implementation; nothing here
//! is used for writing until its gate passes. Ported so far: the JSON and
//! YAML symbol extractors (WI-501/502 slice), the regex extractor with its
//! infra artifacts and import refs for every language WrongStack routes to
//! it ([`jsre`] runs the original regexes), the Rust symbol parser with its
//! regex import refs, and the post-parse pipeline
//! of a full run (module roots and package labels, module resolution for
//! every language family, import binding, graph ranks).

pub mod binding;
pub mod dbdump;
pub mod js;
pub mod jsre;
pub mod native;
pub mod parse;
pub mod paths;
pub mod query;
pub mod rank;
pub mod relations;
pub mod resolver;
pub mod rows;
pub mod unicode;

#[cfg(test)]
mod kat {
    use serde_json::Value;

    use crate::js::utf16;
    use crate::parse;

    #[test]
    fn parser_ports_match_wrongstack_known_answers() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/fixtures/parser-kat.json"
        );
        let doc: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let cases = doc["cases"].as_array().unwrap();
        let failures: Vec<String> = cases.iter().filter_map(check_case).collect();
        assert!(
            failures.is_empty(),
            "{} of {} differ:
{}",
            failures.len(),
            cases.len(),
            failures.join(
                "
"
            )
        );
        let generic = cases
            .iter()
            .filter(|c| parse::generic::LANGS.contains(&c["lang"].as_str().unwrap()))
            .count();
        assert!(cases.len() >= 120 && generic >= 50, "{} cases", cases.len());
    }

    /// Real-world corpus produced by
    /// `bun scripts/oracle/parser-kat.ts --wrongstack <oracle> --corpus <dir> --out <file>`:
    /// a `.json` document with contents, or a `.jsonl` stream (one case per
    /// line, contents re-read from the absolute path).
    #[test]
    #[ignore = "needs WRONGINDEX_PARSER_CORPUS (a corpus KAT file)"]
    fn parser_ports_match_a_real_world_corpus() {
        use std::io::BufRead;
        let path = std::env::var("WRONGINDEX_PARSER_CORPUS").expect("WRONGINDEX_PARSER_CORPUS");
        let (mut n, mut failures) = (0usize, Vec::new());
        let mut by_lang = std::collections::BTreeMap::<String, (usize, usize, usize)>::new();
        let mut tally = |c: &Value| {
            let e = by_lang
                .entry(c["lang"].as_str().unwrap().to_string())
                .or_default();
            e.0 += 1;
            e.1 += c["symbols"].as_array().map_or(0, Vec::len);
            e.2 += c["refs"].as_array().map_or(0, Vec::len);
        };
        if path.ends_with(".jsonl") {
            let f = std::io::BufReader::new(std::fs::File::open(&path).unwrap());
            for line in f.lines() {
                let line = line.unwrap();
                if line.is_empty() {
                    continue;
                }
                let mut c: Value = serde_json::from_str(&line).unwrap();
                let file = c["file"].as_str().unwrap().to_string();
                let bytes = std::fs::read(&file).unwrap();
                c["content"] = Value::String(String::from_utf8_lossy(&bytes).into_owned());
                tally(&c);
                failures.extend(check_case(&c));
                n += 1;
            }
        } else {
            let doc: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            for c in doc["cases"].as_array().unwrap() {
                tally(c);
                failures.extend(check_case(c));
                n += 1;
            }
        }
        for (lang, (files, syms, refs)) in &by_lang {
            eprintln!("  {lang}: {files} files, {syms} symbols, {refs} refs");
        }
        assert!(
            failures.is_empty(),
            "{} of {n} differ:
{}",
            failures.len(),
            failures.iter().take(20).cloned().collect::<Vec<_>>().join(
                "
"
            )
        );
        eprintln!("{n} corpus files identical");
    }

    fn static_lang(lang: &str) -> &'static str {
        ["json", "yaml", "rs"]
            .iter()
            .chain(parse::generic::LANGS)
            .find(|l| **l == lang)
            .unwrap_or_else(|| panic!("no port for {lang}"))
    }

    /// One case: `None` when identical, else a description of the first
    /// difference.
    fn check_case(c: &Value) -> Option<String> {
        let content = utf16(c["content"].as_str().unwrap());
        let file = c["file"].as_str().unwrap();
        let parsed = parse::parse_file(file, static_lang(c["lang"].as_str().unwrap()), &content);
        let mut out = Vec::new();
        if let Some(why) = &parsed.unsupported {
            out.push(format!("{} unsupported: {why}", c["source"]));
        }
        // JSON/YAML cases record only a ref count: YAML infra refs are not
        // ported (the native writer refuses those files instead), so the
        // count is not compared.
        if c["refs"].is_array() && serde_json::to_value(&parsed.refs).unwrap() != c["refs"] {
            let want = c["refs"].as_array().unwrap();
            let got = serde_json::to_value(&parsed.refs).unwrap();
            let got = got.as_array().unwrap();
            let first = (0..want.len().max(got.len()))
                .find(|&i| want.get(i) != got.get(i))
                .unwrap_or(0);
            out.push(format!(
                "{} refs ({} want / {} got), first difference at #{first}:
  want {}
  got  {}",
                c["source"],
                want.len(),
                got.len(),
                want.get(first).map(Value::to_string).unwrap_or_default(),
                got.get(first).map(Value::to_string).unwrap_or_default(),
            ));
        }
        let got = serde_json::to_value(&parsed.symbols).unwrap();
        if got != c["symbols"] {
            let want = c["symbols"].as_array().unwrap();
            let got = got.as_array().unwrap();
            let first = (0..want.len().max(got.len()))
                .find(|&i| want.get(i) != got.get(i))
                .unwrap_or(0);
            out.push(format!(
                "{} ({} want / {} got), first difference at #{first}:
  want {}
  got  {}",
                c["source"],
                want.len(),
                got.len(),
                want.get(first).map(Value::to_string).unwrap_or_default(),
                got.get(first).map(Value::to_string).unwrap_or_default(),
            ));
        }
        (!out.is_empty()).then(|| {
            out.join(
                "
",
            )
        })
    }
}
