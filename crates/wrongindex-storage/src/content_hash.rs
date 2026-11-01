//! `files.content_hash` compatibility.
//!
//! WrongStack reads each file with `fs.readFile(file, 'utf8')` and hashes the
//! UTF-8 re-encoding of the decoded string with xxHash64 (seed 0), stored as 16
//! lowercase hex characters. For valid UTF-8 that equals hashing the raw bytes
//! (a BOM is retained, not stripped). For invalid UTF-8 the runtime first
//! replaces each maximal ill-formed subsequence with U+FFFD, so the stored hash
//! is NOT the raw-byte hash. `String::from_utf8_lossy` applies the same
//! substitution rule; the known-answer fixtures generated from WrongStack's own
//! implementation pin that equivalence.

use std::borrow::Cow;

use xxhash_rust::xxh64::xxh64;

/// xxHash64 (seed 0) of `bytes`, formatted as 16 lowercase hex characters.
pub fn xxhash64_hex(bytes: &[u8]) -> String {
    format!("{:016x}", xxh64(bytes, 0))
}

/// The value WrongStack stores in `files.content_hash` for a file whose raw
/// on-disk bytes are `bytes`.
pub fn file_content_hash(bytes: &[u8]) -> String {
    match String::from_utf8_lossy(bytes) {
        Cow::Borrowed(valid) => xxhash64_hex(valid.as_bytes()),
        Cow::Owned(replaced) => xxhash64_hex(replaced.as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Kat {
        cases: Vec<KatCase>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct KatCase {
        name: String,
        bytes_hex: String,
        raw_hash: String,
        file_hash: String,
    }

    const KAT_JSON: &str = include_str!("../../../contracts/fixtures/content-hash-kat.json");

    #[test]
    fn matches_wrongstack_known_answers() {
        let kat: Kat = serde_json::from_str(KAT_JSON).expect("valid KAT fixture");
        assert!(kat.cases.len() >= 10, "KAT fixture unexpectedly small");
        let mut saw_invalid_utf8_divergence = false;
        for case in &kat.cases {
            let bytes = hex::decode(&case.bytes_hex).unwrap();
            assert_eq!(
                xxhash64_hex(&bytes),
                case.raw_hash,
                "raw hash: {}",
                case.name
            );
            assert_eq!(
                file_content_hash(&bytes),
                case.file_hash,
                "file hash: {}",
                case.name
            );
            saw_invalid_utf8_divergence |= case.raw_hash != case.file_hash;
        }
        // Control: the fixture must actually exercise the lossy-decode path.
        assert!(saw_invalid_utf8_divergence);
    }

    #[test]
    fn empty_input_is_reference_value() {
        assert_eq!(xxhash64_hex(b""), "ef46db3751d8e999");
    }

    /// Every `files.content_hash` the WrongStack oracle recorded must equal the
    /// Rust hash of the checked-out fixture source. This is cross-runtime parity
    /// over real files, and it fails if a checkout rewrote fixture bytes (for
    /// example CRLF -> LF normalization).
    #[test]
    fn oracle_recorded_hashes_match_fixture_sources() {
        use std::path::PathBuf;
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
        let mut checked = 0;
        let mut saw_crlf = false;
        for entry in std::fs::read_dir(base.join("oracle")).unwrap() {
            let dir = entry.unwrap().path();
            let snapshot = dir.join("snapshot.json");
            if !snapshot.is_file() {
                continue;
            }
            let snap: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&snapshot).unwrap()).unwrap();
            let fixture = snap["fixture"].as_str().unwrap();
            for file in snap["files"].as_array().unwrap() {
                let rel = file["file"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("<root>/")
                    .unwrap();
                let path = base.join("projects").join(fixture).join(rel);
                let bytes =
                    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                saw_crlf |= bytes.windows(2).any(|w| w == b"\r\n");
                assert_eq!(
                    file_content_hash(&bytes),
                    file["content_hash"].as_str().unwrap(),
                    "{fixture}/{rel}"
                );
                checked += 1;
            }
        }
        assert!(checked >= 25, "only {checked} files checked");
        // Control: the corpus must still contain a CRLF source after checkout.
        assert!(saw_crlf, "CRLF fixture was normalized by the checkout");
    }
}
