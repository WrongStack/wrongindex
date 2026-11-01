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
}
