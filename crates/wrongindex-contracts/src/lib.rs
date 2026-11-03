//! Wire contracts shared by the WrongIndex daemon, CLI, desktop bridge, and
//! the TypeScript client adapter.
//!
//! Framing: every frame is a 32-bit unsigned little-endian byte length followed
//! by that many bytes of UTF-8 JSON. The first frame a client sends on a
//! connection is an [`AuthRequest`]; nothing else is accepted before it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_MAJOR: u32 = 1;
pub const PROTOCOL_MINOR: u32 = 0;

/// Initial maximum request/response frame size (8 MiB).
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Storage + semantic profile this build can inspect. Resolves to
/// `contracts/profiles/wrongstack-index-v5-baseline/manifest.json`.
pub const BASELINE_PROFILE_ID: &str = "wrongstack-index-v5-baseline";

/// Exit codes shared by the CLI (see desktop-and-operations §4).
pub mod exit_code {
    pub const SUCCESS: i32 = 0;
    pub const INVALID_INPUT: i32 = 2;
    pub const SERVICE_UNAVAILABLE: i32 = 3;
    pub const INCOMPATIBLE_PROFILE: i32 = 4;
    pub const JOB_FAILED: i32 = 5;
    pub const TIMEOUT: i32 = 6;
    pub const INTERNAL: i32 = 70;
}

/// Stable typed error codes. Serialized as SCREAMING_SNAKE_CASE strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    Unauthenticated,
    Forbidden,
    ProtocolUnsupported,
    ProfileUnsupported,
    IndexIdentityConflict,
    OwnerConflict,
    ProjectUnavailable,
    ProjectNotFound,
    JobNotFound,
    InvalidRequest,
    MethodNotFound,
    NotImplemented,
    QueueSaturated,
    IndexDegraded,
    DatabaseBusy,
    DatabaseCorrupt,
    ReadOnly,
    DiskFull,
    ShuttingDown,
    Canceled,
    DeadlineExceeded,
    Internal,
}

impl ErrorCode {
    /// Whether a client may retry the same request unchanged.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            ErrorCode::QueueSaturated
                | ErrorCode::DatabaseBusy
                | ErrorCode::DeadlineExceeded
                | ErrorCode::ShuttingDown
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: code.retryable(),
        }
    }
}

impl std::fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let code = serde_json::to_value(self.code)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(|| format!("{:?}", self.code));
        write!(f, "{code}: {}", self.message)
    }
}

impl std::error::Error for ErrorBody {}

// ─── Handshake ──────────────────────────────────────────────────────────────

/// First frame on every connection. The token is read from the owner-private
/// runtime directory; it never appears in arguments, logs, or responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthRequest {
    pub protocol_major: u32,
    pub protocol_minor: u32,
    pub token: String,
    pub client_name: String,
    pub client_version: String,
    #[serde(default)]
    pub supported_profile_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    pub max_frame_bytes: usize,
    pub max_page_size: usize,
    pub max_inflight: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    pub daemon_instance_id: String,
    pub daemon_version: String,
    pub protocol_major: u32,
    pub protocol_minor: u32,
    pub supported_profile_ids: Vec<String>,
    pub selected_profile_id: Option<String>,
    /// SHA-256 over the canonical storage + semantic profile (WI-007).
    #[serde(default)]
    pub profile_fingerprint: Option<String>,
    pub capabilities: Vec<String>,
    pub limits: Limits,
}

/// Response to [`AuthRequest`]: either a hello or a typed refusal, after which
/// the daemon closes the connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum AuthResponse {
    Ok { hello: Hello },
    Error { error: ErrorBody },
}

// ─── Request / response envelope ────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub protocol: u32,
    pub request_id: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default)]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

impl Response {
    pub fn ok(request_id: impl Into<String>, result: Value) -> Self {
        Self {
            request_id: request_id.into(),
            result: Some(result),
            error: None,
        }
    }

    pub fn err(request_id: impl Into<String>, error: ErrorBody) -> Self {
        Self {
            request_id: request_id.into(),
            result: None,
            error: Some(error),
        }
    }
}

/// Method names. Kept as constants so the TS adapter can be generated from one list.
pub mod method {
    pub const DAEMON_HELLO: &str = "daemon.hello";
    pub const DAEMON_HEALTH: &str = "daemon.health";
    pub const DAEMON_SHUTDOWN: &str = "daemon.shutdown";
    pub const PROJECT_REGISTER: &str = "project.register";
    pub const PROJECT_LIST: &str = "project.list";
    pub const PROJECT_GET: &str = "project.get";
    pub const PROJECT_DETACH: &str = "project.detach";
    pub const INDEX_REQUEST: &str = "index.request";
    pub const INDEX_CHECK: &str = "index.check";
    pub const INDEX_QUERY: &str = "index.query";
    pub const INDEX_BACKUPS: &str = "index.backups";
    pub const SETTINGS_GET: &str = "settings.get";
    pub const SETTINGS_SET: &str = "settings.set";
    pub const DAEMON_METRICS: &str = "daemon.metrics";
    pub const JOB_GET: &str = "job.get";
    pub const JOB_LIST: &str = "job.list";
    pub const JOB_CANCEL: &str = "job.cancel";
    pub const OWNERSHIP_ACQUIRE: &str = "ownership.acquire";
    pub const OWNERSHIP_RELEASE: &str = "ownership.release";
    pub const OWNERSHIP_STATUS: &str = "ownership.status";
    pub const PROJECT_CONFIGURE: &str = "project.configure";
    pub const SESSION_ACQUIRE: &str = "session.acquire";
    pub const SESSION_HEARTBEAT: &str = "session.heartbeat";
    pub const SESSION_RELEASE: &str = "session.release";
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    pub const EVENTS_SNAPSHOT: &str = "events.snapshot";

    pub const ALL: &[&str] = &[
        DAEMON_HELLO,
        DAEMON_HEALTH,
        DAEMON_SHUTDOWN,
        PROJECT_REGISTER,
        PROJECT_LIST,
        PROJECT_GET,
        PROJECT_DETACH,
        INDEX_REQUEST,
        INDEX_CHECK,
        INDEX_QUERY,
        INDEX_BACKUPS,
        SETTINGS_GET,
        SETTINGS_SET,
        DAEMON_METRICS,
        JOB_GET,
        JOB_LIST,
        JOB_CANCEL,
        OWNERSHIP_ACQUIRE,
        OWNERSHIP_RELEASE,
        OWNERSHIP_STATUS,
        PROJECT_CONFIGURE,
        SESSION_ACQUIRE,
        SESSION_HEARTBEAT,
        SESSION_RELEASE,
        EVENTS_SUBSCRIBE,
        EVENTS_SNAPSHOT,
    ];
}

// ─── Domain DTOs ────────────────────────────────────────────────────────────

/// Which engine writes a project's index (WI-600). `auto` follows the
/// service's `--backend`; `native` never falls back to the compatibility
/// worker; `legacy` always uses it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendPreference {
    #[default]
    Auto,
    Native,
    Legacy,
}

impl BackendPreference {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendPreference::Auto => "auto",
            BackendPreference::Native => "native",
            BackendPreference::Legacy => "legacy",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "auto" => BackendPreference::Auto,
            "native" => BackendPreference::Native,
            "legacy" => BackendPreference::Legacy,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchPolicy {
    Session,
    Pinned,
    Manual,
    Disabled,
}

impl WatchPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            WatchPolicy::Session => "session",
            WatchPolicy::Pinned => "pinned",
            WatchPolicy::Manual => "manual",
            WatchPolicy::Disabled => "disabled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "session" => WatchPolicy::Session,
            "pinned" => WatchPolicy::Pinned,
            "manual" => WatchPolicy::Manual,
            "disabled" => WatchPolicy::Disabled,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Legacy,
    Native,
    Unsupported,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Legacy => "legacy",
            Backend::Native => "native",
            Backend::Unsupported => "unsupported",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "legacy" => Backend::Legacy,
            "native" => Backend::Native,
            "unsupported" => Backend::Unsupported,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Unknown,
    Current,
    Pending,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ownership {
    Unowned,
    Negotiating,
    Owned,
    Conflict,
    Releasing,
    /// The previous owner stopped without a recorded release; must be
    /// re-negotiated before any write.
    RecoveryRequired,
}

impl Ownership {
    pub fn as_str(self) -> &'static str {
        match self {
            Ownership::Unowned => "unowned",
            Ownership::Negotiating => "negotiating",
            Ownership::Owned => "owned",
            Ownership::Conflict => "conflict",
            Ownership::Releasing => "releasing",
            Ownership::RecoveryRequired => "recovery_required",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "unowned" => Ownership::Unowned,
            "negotiating" => Ownership::Negotiating,
            "owned" => Ownership::Owned,
            "conflict" => Ownership::Conflict,
            "releasing" => Ownership::Releasing,
            "recovery_required" => Ownership::RecoveryRequired,
            _ => return None,
        })
    }
}

/// Write fence (INV-14): a mutation commits only if the project's owner epoch
/// and configuration revision are unchanged since the work was admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnerFence {
    pub owner_epoch: i64,
    pub configuration_revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnershipStatus {
    pub state: Ownership,
    pub owner_epoch: i64,
    pub desired: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterProjectParams {
    pub checkout_root: String,
    pub index_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default = "default_watch_policy")]
    pub watch_policy: WatchPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_identity: Option<String>,
}

fn default_watch_policy() -> WatchPolicy {
    WatchPolicy::Manual
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRecord {
    pub project_id: String,
    pub display_name: String,
    pub checkout_root: String,
    pub canonical_checkout_key: String,
    pub index_dir: String,
    pub canonical_index_key: String,
    pub repository_identity: Option<String>,
    pub compatibility_profile: String,
    pub watch_policy: WatchPolicy,
    pub backend: Backend,
    pub configuration_revision: i64,
    pub registered_at_ms: i64,
    /// Engine choice for this project (WI-600).
    #[serde(default)]
    pub backend_preference: BackendPreference,
}

/// Separate state dimensions (architecture §4). M1 only populates the subset it
/// can observe truthfully; everything else stays `unknown`/`unowned`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStatus {
    pub project: ProjectRecord,
    pub freshness: Freshness,
    pub ownership: Ownership,
    pub activity: String,
    pub watch_health: String,
    pub last_job: Option<JobRecord>,
    #[serde(default)]
    pub pending_paths: usize,
    #[serde(default)]
    pub reconcile_reason: Option<String>,
    #[serde(default)]
    pub sessions: usize,
    #[serde(default)]
    pub generation: i64,
    #[serde(default)]
    pub last_indexed_ms: Option<i64>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// The running job's current phase, while one runs on the native
    /// engine (`{jobId, phase, done?, ms?, files?, atMs}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Value>,
    /// Consecutive failed watcher batches (WI-208); 0 when healthy.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Epoch ms of the next automatic attempt, while failures back it off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_ms: Option<i64>,
    /// Parked by the circuit after `watchParkAfterFailures` failures: one
    /// probe per 15 minutes until a batch or an explicit job succeeds.
    #[serde(default)]
    pub parked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterProjectResult {
    pub project: ProjectRecord,
    /// `false` when an identical registration already existed (idempotent).
    pub created: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Read-only profile/schema inspection of the project's index.
    Check,
    Incremental,
    Full,
    Reconcile,
    /// Maintenance: replace an index the regular writer refuses (unreadable,
    /// another schema, extra data) with a fresh one, keeping the old file as
    /// a backup next to it. Native engine only; needs maintenance ownership
    /// or ordinary ownership.
    Rebuild,
    /// Replace the index with one of its backups (taken at each ownership
    /// transfer); the current file is backed up first. Needs ownership.
    Restore,
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::Check => "check",
            JobKind::Incremental => "incremental",
            JobKind::Full => "full",
            JobKind::Reconcile => "reconcile",
            JobKind::Rebuild => "rebuild",
            JobKind::Restore => "restore",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "check" => JobKind::Check,
            "incremental" => JobKind::Incremental,
            "full" => JobKind::Full,
            "reconcile" => JobKind::Reconcile,
            "rebuild" => JobKind::Rebuild,
            "restore" => JobKind::Restore,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Canceled,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Canceled => "canceled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => JobState::Queued,
            "running" => JobState::Running,
            "succeeded" => JobState::Succeeded,
            "failed" => JobState::Failed,
            "canceled" => JobState::Canceled,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Succeeded | JobState::Failed | JobState::Canceled
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub job_id: String,
    pub project_id: String,
    pub kind: JobKind,
    pub state: JobState,
    pub attempt: i64,
    pub admitted_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub result: Option<Value>,
    pub error: Option<ErrorBody>,
    /// Admission parameters (paths, write fence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexRequestParams {
    pub kind: JobKind,
    #[serde(default)]
    pub paths: Vec<String>,
    /// `restore` only: the backup file name (from `index.backups`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

/// `session.acquire` result: a watch lease the client keeps alive with
/// `session.heartbeat` every `heartbeatMs`; it lapses after `leaseMs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLease {
    pub session_id: String,
    pub project_id: String,
    pub lease_ms: u64,
    pub heartbeat_ms: u64,
}

/// Durable admission acknowledgement. Completion is observed via `job.get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobAdmission {
    pub job: JobRecord,
    /// `true` when an idempotency key matched an earlier admission.
    pub deduplicated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub daemon_instance_id: String,
    pub daemon_version: String,
    pub pid: u32,
    pub started_at_ms: i64,
    pub uptime_ms: i64,
    pub draining: bool,
    pub project_count: usize,
    pub queued_jobs: usize,
    pub running_jobs: usize,
    pub sqlite_version: String,
    pub backend_inventory: Vec<String>,
    /// Background indexing paused by the `paused` setting.
    #[serde(default)]
    pub paused: bool,
    /// Revision of the applied settings.
    #[serde(default)]
    pub settings_revision: u64,
}

/// Service settings (WI-405), applied live by the daemon and persisted in
/// `<data-root>/settings.json`. Unknown keys are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct DaemonSettings {
    /// Quiet period after the last file event before a watcher batch runs.
    pub watch_debounce_ms: u64,
    /// Longest a pending watcher batch waits under continuous edits.
    pub watch_max_wait_ms: u64,
    /// How long a `session` project stays watched after its last client.
    pub session_grace_seconds: u64,
    /// Index jobs running at once (one per project).
    pub parallel_jobs: usize,
    /// Memory cap for one native indexing run in MiB; 0 = automatic
    /// (a quarter of physical memory, at least 2 GiB).
    pub native_memory_mb: u64,
    /// Index backups kept per project (taken at each ownership transfer).
    pub backup_keep: usize,
    /// Consecutive failed watcher batches after which a project is parked
    /// (one probe per 15 minutes until a batch or an explicit job succeeds).
    pub watch_park_after_failures: u32,
    /// Background indexing paused: watcher batches wait (freshness is not
    /// current); explicit requests and queries still run.
    pub paused: bool,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            watch_debounce_ms: 400,
            watch_max_wait_ms: 2_000,
            session_grace_seconds: 120,
            parallel_jobs: 2,
            native_memory_mb: 0,
            backup_keep: 3,
            watch_park_after_failures: 8,
            paused: false,
        }
    }
}

impl DaemonSettings {
    /// Range checks; the message names the first offending setting.
    pub fn validate(&self) -> Result<(), String> {
        let range = |name: &str, v: u64, lo: u64, hi: u64| {
            if v < lo || v > hi {
                Err(format!("{name} must be between {lo} and {hi} (got {v})"))
            } else {
                Ok(())
            }
        };
        range("watchDebounceMs", self.watch_debounce_ms, 100, 10_000)?;
        range("watchMaxWaitMs", self.watch_max_wait_ms, 500, 60_000)?;
        if self.watch_max_wait_ms < self.watch_debounce_ms {
            return Err("watchMaxWaitMs must not be shorter than watchDebounceMs".into());
        }
        range("sessionGraceSeconds", self.session_grace_seconds, 0, 3_600)?;
        range("parallelJobs", self.parallel_jobs as u64, 1, 8)?;
        if self.native_memory_mb != 0 {
            range("nativeMemoryMb", self.native_memory_mb, 512, 1_048_576)?;
        }
        range("backupKeep", self.backup_keep as u64, 1, 20)?;
        range(
            "watchParkAfterFailures",
            u64::from(self.watch_park_after_failures),
            2,
            100,
        )?;
        Ok(())
    }
}

/// Discovery metadata written atomically to `<data-root>/runtime/endpoint.json`.
/// Contains no secret; the token lives in `runtime/token`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointInfo {
    pub endpoint: String,
    pub daemon_instance_id: String,
    pub daemon_version: String,
    pub pid: u32,
    pub protocol_major: u32,
    pub protocol_minor: u32,
    pub started_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_serialize_screaming_snake() {
        let s = serde_json::to_string(&ErrorCode::IndexIdentityConflict).unwrap();
        assert_eq!(s, "\"INDEX_IDENTITY_CONFLICT\"");
        let back: ErrorCode = serde_json::from_str("\"QUEUE_SATURATED\"").unwrap();
        assert_eq!(back, ErrorCode::QueueSaturated);
        assert!(back.retryable());
        assert!(!ErrorCode::Unauthenticated.retryable());
    }

    #[test]
    fn request_envelope_matches_plan_example() {
        let raw = r#"{"protocol":1,"requestId":"r1","method":"index.request","projectId":"p1",
            "params":{"kind":"incremental","paths":["src/main.ts"]},"idempotencyKey":"k"}"#;
        let req: Request = serde_json::from_str(raw).unwrap();
        assert_eq!(req.method, method::INDEX_REQUEST);
        let params: IndexRequestParams = serde_json::from_value(req.params).unwrap();
        assert_eq!(params.kind, JobKind::Incremental);
        assert_eq!(params.paths, vec!["src/main.ts"]);
    }

    #[test]
    fn auth_response_is_tagged() {
        let r = AuthResponse::Error {
            error: ErrorBody::new(ErrorCode::Unauthenticated, "bad token"),
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["status"], "error");
        assert_eq!(v["error"]["code"], "UNAUTHENTICATED");
    }

    /// `method::ALL` is the list clients are generated from: it must name
    /// every method constant (read from this file, so a new constant that is
    /// not listed fails here).
    #[test]
    fn method_all_lists_every_method() {
        let src = include_str!("lib.rs");
        let module = &src[src.find("pub mod method {").unwrap()..];
        let module = &module[..module.find("\n}\n").unwrap()];
        let declared: Vec<&str> = module
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pub const "))
            .filter(|l| !l.starts_with("ALL"))
            .filter_map(|l| l.split('"').nth(1))
            .collect();
        assert_eq!(declared.len(), 26, "{declared:?}");
        for m in &declared {
            assert!(method::ALL.contains(m), "method::ALL lacks {m}");
        }
        assert_eq!(method::ALL.len(), declared.len());
    }

    #[test]
    fn enum_string_round_trips() {
        for p in [
            WatchPolicy::Session,
            WatchPolicy::Pinned,
            WatchPolicy::Manual,
            WatchPolicy::Disabled,
        ] {
            assert_eq!(WatchPolicy::parse(p.as_str()), Some(p));
        }
        for k in [
            JobKind::Check,
            JobKind::Incremental,
            JobKind::Full,
            JobKind::Reconcile,
            JobKind::Rebuild,
        ] {
            assert_eq!(JobKind::parse(k.as_str()), Some(k));
        }
        for s in [
            JobState::Queued,
            JobState::Running,
            JobState::Succeeded,
            JobState::Failed,
            JobState::Canceled,
        ] {
            assert_eq!(JobState::parse(s.as_str()), Some(s));
        }
    }
}
