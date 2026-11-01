//! WrongIndex native engine (M5).
//!
//! Capabilities are ported from WrongStack one at a time and each is gated by
//! differential evidence against the original implementation; nothing here
//! is used for writing until its gate passes. Ported so far: the JSON and
//! YAML symbol extractors (WI-501/502 slice), the TOML regex extractor, the
//! Rust symbol parser with its regex import refs, and the post-parse pipeline
//! of a full run (module roots and package labels, module resolution for
//! every language family, import binding, graph ranks).

pub mod binding;
pub mod dbdump;
pub mod js;
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
    use crate::parse::{json, rs, yaml};

    #[test]
    fn parser_ports_match_wrongstack_known_answers() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../contracts/fixtures/parser-kat.json"
        );
        assert!(check(path) >= 70);
    }

    /// Real-world corpus produced by
    /// `bun scripts/oracle/parser-kat.ts --wrongstack <oracle> --corpus <dir> --out <file>`.
    #[test]
    #[ignore = "needs WRONGINDEX_PARSER_CORPUS (a corpus KAT file)"]
    fn parser_ports_match_a_real_world_corpus() {
        let path = std::env::var("WRONGINDEX_PARSER_CORPUS").expect("WRONGINDEX_PARSER_CORPUS");
        let n = check(&path);
        eprintln!("{n} corpus files identical");
    }

    fn check(path: &str) -> usize {
        let doc: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let cases = doc["cases"].as_array().unwrap();
        let mut failures = Vec::new();
        for c in cases {
            let content = utf16(c["content"].as_str().unwrap());
            let file = c["file"].as_str().unwrap();
            let parsed = match c["lang"].as_str().unwrap() {
                "json" => json::parse(file, &content, None),
                "yaml" => yaml::parse(&content),
                "rs" => rs::parse(&content),
                other => panic!("no port for {other}"),
            };
            // Rust cases record their refs. JSON/YAML cases record only a
            // count: YAML infra refs are not ported (the native writer refuses
            // those files instead), so the count is not compared here.
            let refs_match = match &c["refs"] {
                Value::Array(_) => serde_json::to_value(&parsed.refs).unwrap() == c["refs"],
                _ => true,
            };
            if !refs_match {
                failures.push(format!(
                    "{} refs differ:
  want {}
  got  {}",
                    c["source"],
                    c["refs"],
                    serde_json::to_value(&parsed.refs).unwrap()
                ));
            }
            let got = serde_json::to_value(&parsed.symbols).unwrap();
            if got != c["symbols"] {
                let want = c["symbols"].as_array().unwrap();
                let got = got.as_array().unwrap();
                let first = (0..want.len().max(got.len()))
                    .find(|&i| want.get(i) != got.get(i))
                    .unwrap_or(0);
                failures.push(format!(
                    "{} ({} want / {} got), first difference at #{first}:\n  want {}\n  got  {}",
                    c["source"],
                    want.len(),
                    got.len(),
                    want.get(first).map(Value::to_string).unwrap_or_default(),
                    got.get(first).map(Value::to_string).unwrap_or_default(),
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} differ:\n{}",
            failures.len(),
            cases.len(),
            failures.join("\n")
        );
        cases.len()
    }
}
