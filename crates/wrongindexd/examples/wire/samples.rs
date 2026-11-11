//! Canonical samples of every value that crosses the WrongIndex wire or sits
//! in a file a client reads (WI-102). Serialized by the Rust contracts, they
//! are the fixtures the TypeScript adapter is checked against. Shared by the
//! `wire_fixtures` example (the generator) and `tests/wire_fixtures.rs` (the
//! drift and round-trip check).

use serde_json::{Value, json};
use wrongindex_contracts::*;
use wrongindex_storage::owner_lock::OwnerInfo;

// Every enum variant, listed once. Each `exhaustive_*` match stops compiling
// when a variant is added, so the list cannot fall behind silently.

const ERROR_CODES: [ErrorCode; 22] = [
    ErrorCode::Unauthenticated,
    ErrorCode::Forbidden,
    ErrorCode::ProtocolUnsupported,
    ErrorCode::ProfileUnsupported,
    ErrorCode::IndexIdentityConflict,
    ErrorCode::OwnerConflict,
    ErrorCode::ProjectUnavailable,
    ErrorCode::ProjectNotFound,
    ErrorCode::JobNotFound,
    ErrorCode::InvalidRequest,
    ErrorCode::MethodNotFound,
    ErrorCode::NotImplemented,
    ErrorCode::QueueSaturated,
    ErrorCode::IndexDegraded,
    ErrorCode::DatabaseBusy,
    ErrorCode::DatabaseCorrupt,
    ErrorCode::ReadOnly,
    ErrorCode::DiskFull,
    ErrorCode::ShuttingDown,
    ErrorCode::Canceled,
    ErrorCode::DeadlineExceeded,
    ErrorCode::Internal,
];

fn exhaustive_error_code(c: ErrorCode) {
    match c {
        ErrorCode::Unauthenticated
        | ErrorCode::Forbidden
        | ErrorCode::ProtocolUnsupported
        | ErrorCode::ProfileUnsupported
        | ErrorCode::IndexIdentityConflict
        | ErrorCode::OwnerConflict
        | ErrorCode::ProjectUnavailable
        | ErrorCode::ProjectNotFound
        | ErrorCode::JobNotFound
        | ErrorCode::InvalidRequest
        | ErrorCode::MethodNotFound
        | ErrorCode::NotImplemented
        | ErrorCode::QueueSaturated
        | ErrorCode::IndexDegraded
        | ErrorCode::DatabaseBusy
        | ErrorCode::DatabaseCorrupt
        | ErrorCode::ReadOnly
        | ErrorCode::DiskFull
        | ErrorCode::ShuttingDown
        | ErrorCode::Canceled
        | ErrorCode::DeadlineExceeded
        | ErrorCode::Internal => {}
    }
}

const JOB_STATES: [JobState; 5] = [
    JobState::Queued,
    JobState::Running,
    JobState::Succeeded,
    JobState::Failed,
    JobState::Canceled,
];

fn exhaustive_job_state(s: JobState) {
    match s {
        JobState::Queued
        | JobState::Running
        | JobState::Succeeded
        | JobState::Failed
        | JobState::Canceled => {}
    }
}

const JOB_KINDS: [JobKind; 8] = [
    JobKind::Check,
    JobKind::Incremental,
    JobKind::Full,
    JobKind::Reconcile,
    JobKind::Rebuild,
    JobKind::Restore,
    JobKind::Backup,
    JobKind::Checkpoint,
];

fn exhaustive_job_kind(k: JobKind) {
    match k {
        JobKind::Check
        | JobKind::Incremental
        | JobKind::Full
        | JobKind::Reconcile
        | JobKind::Rebuild
        | JobKind::Restore
        | JobKind::Backup
        | JobKind::Checkpoint => {}
    }
}

const FRESHNESS: [Freshness; 5] = [
    Freshness::Unknown,
    Freshness::Current,
    Freshness::Pending,
    Freshness::Degraded,
    Freshness::Unavailable,
];

fn exhaustive_freshness(f: Freshness) {
    match f {
        Freshness::Unknown
        | Freshness::Current
        | Freshness::Pending
        | Freshness::Degraded
        | Freshness::Unavailable => {}
    }
}

const OWNERSHIP: [Ownership; 6] = [
    Ownership::Unowned,
    Ownership::Negotiating,
    Ownership::Owned,
    Ownership::Conflict,
    Ownership::Releasing,
    Ownership::RecoveryRequired,
];

fn exhaustive_ownership(o: Ownership) {
    match o {
        Ownership::Unowned
        | Ownership::Negotiating
        | Ownership::Owned
        | Ownership::Conflict
        | Ownership::Releasing
        | Ownership::RecoveryRequired => {}
    }
}

const WATCH_POLICIES: [WatchPolicy; 4] = [
    WatchPolicy::Session,
    WatchPolicy::Pinned,
    WatchPolicy::Manual,
    WatchPolicy::Disabled,
];

fn exhaustive_watch_policy(p: WatchPolicy) {
    match p {
        WatchPolicy::Session
        | WatchPolicy::Pinned
        | WatchPolicy::Manual
        | WatchPolicy::Disabled => {}
    }
}

const BACKENDS: [Backend; 3] = [Backend::Legacy, Backend::Native, Backend::Unsupported];

fn exhaustive_backend(b: Backend) {
    match b {
        Backend::Legacy | Backend::Native | Backend::Unsupported => {}
    }
}

const BACKEND_PREFERENCES: [BackendPreference; 3] = [
    BackendPreference::Auto,
    BackendPreference::Native,
    BackendPreference::Legacy,
];

fn exhaustive_backend_preference(b: BackendPreference) {
    match b {
        BackendPreference::Auto | BackendPreference::Native | BackendPreference::Legacy => {}
    }
}

const WATCH_MODES: [WatchMode; 2] = [WatchMode::Auto, WatchMode::Poll];

fn exhaustive_watch_mode(m: WatchMode) {
    match m {
        WatchMode::Auto | WatchMode::Poll => {}
    }
}

fn to<T: serde::Serialize>(v: T) -> Value {
    serde_json::to_value(v).expect("contract types serialize")
}

fn project() -> ProjectRecord {
    ProjectRecord {
        project_id: "p_0123456789abcdef0123456789abcdef".into(),
        display_name: "demo".into(),
        checkout_root: "C:\\work\\demo".into(),
        canonical_checkout_key: "c:\\work\\demo".into(),
        index_dir: "C:\\Users\\dev\\.wrongstack\\projects\\demo-1a2b3c4d".into(),
        canonical_index_key: "c:\\users\\dev\\.wrongstack\\projects\\demo-1a2b3c4d".into(),
        repository_identity: Some("git:4b825dc642cb6eb9a060e54bf8d69288fbee4904".into()),
        compatibility_profile: BASELINE_PROFILE_ID.into(),
        watch_policy: WatchPolicy::Session,
        backend: Backend::Native,
        configuration_revision: 3,
        registered_at_ms: 1_793_600_000_000,
        backend_preference: BackendPreference::Auto,
        watch_mode: WatchMode::Auto,
    }
}

/// A targeted native run's job result detail, built the way the service
/// builds it: the engine's `Report` as `rebuild_reply` serializes an
/// `Updated` run, plus what the worker adds (`durationMs`, `targets`).
fn native_detail() -> Value {
    let report = wrongindex_engine::native::Report {
        files_indexed: 1,
        files_empty: 0,
        symbols_indexed: 4,
        graph_stamp: "1793664963814:1".into(),
        oversized_files: Vec::new(),
        sage_healed: None,
    };
    let mut r = to(&report);
    r["outcome"] = json!("updated");
    r["changedFiles"] = json!(1);
    r["durationMs"] = json!(40);
    r["targets"] = json!(1);
    r
}

fn job(state: JobState) -> JobRecord {
    let (result, error) = match state {
        JobState::Succeeded => (
            Some(json!({
                "backend": "native",
                "native": native_detail(),
                "graphStamp": "1793664963814:1",
                "generation": 7
            })),
            None,
        ),
        JobState::Failed => (
            None,
            Some(ErrorBody::new(
                ErrorCode::ProjectUnavailable,
                "checkout root is unavailable",
            )),
        ),
        _ => (None, None),
    };
    JobRecord {
        job_id: "j_00000000000000000000000000000001".into(),
        project_id: project().project_id,
        kind: JobKind::Incremental,
        state,
        attempt: 1,
        admitted_at_ms: 1_793_600_001_000,
        started_at_ms: (state != JobState::Queued).then_some(1_793_600_001_010),
        completed_at_ms: matches!(
            state,
            JobState::Succeeded | JobState::Failed | JobState::Canceled
        )
        .then_some(1_793_600_001_050),
        result,
        error,
        params: Some(json!({
            "kind": "incremental",
            "paths": ["C:\\work\\demo\\src\\greet.ts"],
            "fence": OwnerFence { owner_epoch: 2, configuration_revision: 3 }
        })),
    }
}

/// The fixture document.
pub fn samples() -> Value {
    ERROR_CODES.into_iter().for_each(exhaustive_error_code);
    JOB_STATES.into_iter().for_each(exhaustive_job_state);
    JOB_KINDS.into_iter().for_each(exhaustive_job_kind);
    FRESHNESS.into_iter().for_each(exhaustive_freshness);
    OWNERSHIP.into_iter().for_each(exhaustive_ownership);
    WATCH_POLICIES.into_iter().for_each(exhaustive_watch_policy);
    BACKENDS.into_iter().for_each(exhaustive_backend);
    BACKEND_PREFERENCES
        .into_iter()
        .for_each(exhaustive_backend_preference);
    WATCH_MODES.into_iter().for_each(exhaustive_watch_mode);

    let hello = Hello {
        daemon_instance_id: "d_0123456789abcdef".into(),
        daemon_version: "0.1.0".into(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        supported_profile_ids: vec![BASELINE_PROFILE_ID.into()],
        selected_profile_id: Some(BASELINE_PROFILE_ID.into()),
        profile_fingerprint: Some(
            "0000000000000000000000000000000000000000000000000000000000000000".into(),
        ),
        capabilities: vec!["index.native".into(), "watch".into()],
        limits: Limits {
            max_frame_bytes: MAX_FRAME_BYTES,
            max_page_size: 1000,
            max_inflight: 16,
        },
    };
    let status = ProjectStatus {
        project: project(),
        freshness: Freshness::Degraded,
        ownership: Ownership::Owned,
        activity: "dormant".into(),
        watch_health: "parked".into(),
        last_job: Some(job(JobState::Failed)),
        pending_paths: 2,
        reconcile_reason: None,
        sessions: 1,
        generation: 7,
        last_indexed_ms: Some(1_793_600_000_500),
        last_error: Some("PROJECT_UNAVAILABLE: checkout root is unavailable".into()),
        progress: None,
        consecutive_failures: 8,
        next_retry_ms: Some(1_793_600_900_000),
        parked: true,
        last_check: None,
        last_verified_ms: None,
        polling: Some(wrongindex_contracts::PollingStatus {
            reason:
                "the file system does not report changes (ReadDirectoryChangesW: Windows error 1)"
                    .into(),
            every_ms: 5_000,
            last_pass_ms: Some(1_793_600_000_400),
            pass_ms: 38,
            files: 412,
            overdue: false,
        }),
    };
    let enums = json!({
        "ErrorCode": ERROR_CODES
            .iter()
            .map(|c| json!({"code": to(c), "retryable": c.retryable()}))
            .collect::<Vec<_>>(),
        "JobState": JOB_STATES.iter().map(to).collect::<Vec<_>>(),
        "JobKind": JOB_KINDS.iter().map(to).collect::<Vec<_>>(),
        "Freshness": FRESHNESS.iter().map(to).collect::<Vec<_>>(),
        "Ownership": OWNERSHIP.iter().map(to).collect::<Vec<_>>(),
        "WatchPolicy": WATCH_POLICIES.iter().map(to).collect::<Vec<_>>(),
        "Backend": BACKENDS.iter().map(to).collect::<Vec<_>>(),
        "BackendPreference": BACKEND_PREFERENCES.iter().map(to).collect::<Vec<_>>(),
        "WatchMode": WATCH_MODES.iter().map(to).collect::<Vec<_>>(),
    });
    let types = json!({
        "AuthRequest": to(AuthRequest {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            token: "<runtime/token contents; never logged>".into(),
            client_name: "wrongstack-tools".into(),
            client_version: "0.0.0".into(),
            supported_profile_ids: vec![BASELINE_PROFILE_ID.into()],
        }),
        "AuthResponseOk": to(AuthResponse::Ok { hello: hello.clone() }),
        "AuthResponseError": to(AuthResponse::Error {
            error: ErrorBody::new(ErrorCode::Unauthenticated, "bad token"),
        }),
        "Request": to(Request {
            protocol: PROTOCOL_MAJOR,
            request_id: "r1".into(),
            method: method::INDEX_REQUEST.into(),
            project_id: Some(project().project_id),
            params: to(IndexRequestParams {
                kind: JobKind::Incremental,
                paths: vec!["src/greet.ts".into()],
                backup: None,
            }),
            idempotency_key: Some("k1".into()),
        }),
        "ResponseOk": to(Response::ok("r1", json!({"ok": true}))),
        "ResponseError": to(Response::err(
            "r2",
            ErrorBody::new(ErrorCode::QueueSaturated, "job queue is full"),
        )),
        "ErrorBody": to(ErrorBody::new(ErrorCode::OwnerConflict, "owned by another process")),
        "Hello": to(&hello),
        "Health": to(Health {
            daemon_instance_id: hello.daemon_instance_id.clone(),
            daemon_version: hello.daemon_version.clone(),
            pid: 4242,
            started_at_ms: 1_793_600_000_000,
            uptime_ms: 60_000,
            draining: false,
            project_count: 1,
            queued_jobs: 0,
            running_jobs: 1,
            sqlite_version: "3.54.1".into(),
            backend_inventory: vec!["native".into()],
            paused: false,
            settings_revision: 2,
        }),
        "EndpointInfo": to(EndpointInfo {
            endpoint: "\\\\.\\pipe\\wrongindex-0123456789abcdef".into(),
            client_bridge: Some("C:\\Program Files\\WrongIndex\\wrongindexd.exe".into()),
            daemon_instance_id: hello.daemon_instance_id.clone(),
            daemon_version: hello.daemon_version.clone(),
            pid: 4242,
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            started_at_ms: 1_793_600_000_000,
        }),
        "RegisterProjectParams": to(RegisterProjectParams {
            checkout_root: "C:\\work\\demo".into(),
            index_dir: "C:\\Users\\dev\\.wrongstack\\projects\\demo-1a2b3c4d".into(),
            display_name: None,
            watch_policy: WatchPolicy::Session,
            repository_identity: None,
        }),
        "RegisterProjectResult": to(RegisterProjectResult { project: project(), created: true }),
        "ProjectRecord": to(project()),
        "ProjectStatus": to(&status),
        "OwnershipStatus": to(OwnershipStatus { state: Ownership::Owned, owner_epoch: 2, desired: true }),
        "IndexRequestParams": to(IndexRequestParams {
            kind: JobKind::Full,
            paths: Vec::new(),
            backup: None,
        }),
        "SessionLease": to(SessionLease {
            session_id: "s_00000000000000000000000000000001".into(),
            project_id: project().project_id,
            lease_ms: 60_000,
            heartbeat_ms: 15_000,
        }),
        "JobAdmission": to(JobAdmission { job: job(JobState::Queued), deduplicated: false }),
        "JobRecordSucceeded": to(job(JobState::Succeeded)),
        "JobRecordFailed": to(job(JobState::Failed)),
        "DaemonSettings": to(DaemonSettings::default()),
        "OwnerInfo": to(OwnerInfo {
            owner: "wrongindexd".into(),
            instance_id: hello.daemon_instance_id.clone(),
            pid: 4242,
            epoch: 2,
            profile_id: BASELINE_PROFILE_ID.into(),
            acquired_at_ms: 1_793_600_000_100,
        }),
    });
    json!({
        "schema": "wrongindex.wire-fixtures/1",
        "generator": "node scripts/contracts/export-wire-fixtures.mjs (cargo run -p wrongindexd --example wire_fixtures)",
        "protocol": {
            "major": PROTOCOL_MAJOR,
            "minor": PROTOCOL_MINOR,
            "maxFrameBytes": MAX_FRAME_BYTES,
            "baselineProfileId": BASELINE_PROFILE_ID,
        },
        "methods": method::ALL,
        "enums": enums,
        "types": types,
    })
}
