//! Prints the wire fixtures (WI-102) as pretty JSON. Run through
//! `node scripts/contracts/export-wire-fixtures.mjs`, which writes
//! `contracts/wire/fixtures.json`; `tests/wire_fixtures.rs` fails when the
//! committed file and the contracts disagree.

#[path = "wire/samples.rs"]
mod samples;

fn main() {
    let text = serde_json::to_string_pretty(&samples::samples()).expect("serialize");
    println!("{text}");
}
