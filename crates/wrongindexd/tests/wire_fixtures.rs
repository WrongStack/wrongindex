//! WI-102: the wire fixtures the TypeScript adapter is checked against are
//! exactly what the Rust contracts produce today (drift fails here, and the
//! fix is to regenerate with `node scripts/contracts/export-wire-fixtures.mjs`),
//! and every fixture round-trips through its Rust type without loss.

#[path = "../examples/wire/samples.rs"]
mod samples;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use wrongindex_contracts::*;
use wrongindex_storage::owner_lock::OwnerInfo;

fn committed() -> Value {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/wire/fixtures.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

#[test]
fn committed_fixtures_match_the_contracts() {
    assert!(
        committed() == samples::samples(),
        "contracts/wire/fixtures.json is stale: regenerate it with \
         `node scripts/contracts/export-wire-fixtures.mjs` and update the adapter's copy"
    );
}

fn round_trip<T: Serialize + DeserializeOwned>(name: &str, v: &Value) {
    let typed: T = serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("{name}: {e}"));
    let back = serde_json::to_value(&typed).unwrap();
    assert_eq!(&back, v, "{name} changed in a round trip");
}

#[test]
fn every_fixture_round_trips_through_its_rust_type() {
    let f = committed();
    let t = &f["types"];
    let names: Vec<&String> = t.as_object().unwrap().keys().collect();
    let mut seen = 0;
    let mut check = |name: &str, run: &dyn Fn(&Value)| {
        run(&t[name]);
        seen += 1;
    };
    check("AuthRequest", &|v| {
        round_trip::<AuthRequest>("AuthRequest", v)
    });
    check("AuthResponseOk", &|v| {
        round_trip::<AuthResponse>("AuthResponseOk", v)
    });
    check("AuthResponseError", &|v| {
        round_trip::<AuthResponse>("AuthResponseError", v)
    });
    check("Request", &|v| round_trip::<Request>("Request", v));
    check("ResponseOk", &|v| round_trip::<Response>("ResponseOk", v));
    check("ResponseError", &|v| {
        round_trip::<Response>("ResponseError", v)
    });
    check("ErrorBody", &|v| round_trip::<ErrorBody>("ErrorBody", v));
    check("Hello", &|v| round_trip::<Hello>("Hello", v));
    check("Health", &|v| round_trip::<Health>("Health", v));
    check("EndpointInfo", &|v| {
        round_trip::<EndpointInfo>("EndpointInfo", v)
    });
    check("RegisterProjectParams", &|v| {
        round_trip::<RegisterProjectParams>("RegisterProjectParams", v)
    });
    check("RegisterProjectResult", &|v| {
        round_trip::<RegisterProjectResult>("RegisterProjectResult", v)
    });
    check("ProjectRecord", &|v| {
        round_trip::<ProjectRecord>("ProjectRecord", v)
    });
    check("ProjectStatus", &|v| {
        round_trip::<ProjectStatus>("ProjectStatus", v)
    });
    check("OwnershipStatus", &|v| {
        round_trip::<OwnershipStatus>("OwnershipStatus", v)
    });
    check("IndexRequestParams", &|v| {
        round_trip::<IndexRequestParams>("IndexRequestParams", v)
    });
    check("SessionLease", &|v| {
        round_trip::<SessionLease>("SessionLease", v)
    });
    check("JobAdmission", &|v| {
        round_trip::<JobAdmission>("JobAdmission", v)
    });
    check("JobRecordSucceeded", &|v| {
        round_trip::<JobRecord>("JobRecordSucceeded", v)
    });
    check("JobRecordFailed", &|v| {
        round_trip::<JobRecord>("JobRecordFailed", v)
    });
    check("DaemonSettings", &|v| {
        round_trip::<DaemonSettings>("DaemonSettings", v)
    });
    check("OwnerInfo", &|v| round_trip::<OwnerInfo>("OwnerInfo", v));
    assert_eq!(
        seen,
        names.len(),
        "a fixture type has no round-trip check: {names:?}"
    );

    for (name, values) in f["enums"].as_object().unwrap() {
        for v in values.as_array().unwrap() {
            let v = if name == "ErrorCode" { &v["code"] } else { v };
            match name.as_str() {
                "ErrorCode" => round_trip::<ErrorCode>(name, v),
                "JobState" => round_trip::<JobState>(name, v),
                "JobKind" => round_trip::<JobKind>(name, v),
                "Freshness" => round_trip::<Freshness>(name, v),
                "Ownership" => round_trip::<Ownership>(name, v),
                "WatchPolicy" => round_trip::<WatchPolicy>(name, v),
                "Backend" => round_trip::<Backend>(name, v),
                "BackendPreference" => round_trip::<BackendPreference>(name, v),
                other => panic!("enum {other} has no round-trip check"),
            }
        }
    }
}
