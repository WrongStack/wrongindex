//! Desktop bridge: the only path from the WebView to the service.
//!
//! The WebView calls narrow Tauri commands; each maps to a fixed daemon
//! method through the shared Rust IPC client. The daemon token never leaves
//! this process, the WebView gets no general filesystem access, and every
//! result shown as live comes from the daemon at the time of the call.

use std::path::Path;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use wrongindex_contracts::{RegisterProjectParams, WatchPolicy, method};
use wrongindex_ipc::{CallError, Client, DataRoot};

pub const CLIENT_NAME: &str = "wrongindex-desktop";

/// Error shown to the user: a stable code plus a plain message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UiError {
    pub code: String,
    pub message: String,
}

impl UiError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

impl From<CallError> for UiError {
    fn from(e: CallError) -> Self {
        match &e {
            CallError::Remote(body) => UiError::new(
                serde_json::to_value(body.code)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .as_deref()
                    .unwrap_or("INTERNAL"),
                body.message.clone(),
            ),
            CallError::Unavailable(_) => UiError::new("SERVICE_UNAVAILABLE", e.to_string()),
            CallError::Transport(_) => UiError::new("SERVICE_UNAVAILABLE", e.to_string()),
            CallError::Protocol(_) => UiError::new("PROTOCOL", e.to_string()),
        }
    }
}

/// Project actions the UI may request. Anything else is rejected here, before
/// it reaches the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectAction {
    Claim,
    Release,
    Check,
    Reconcile,
    Remove,
    Policy(WatchPolicy),
    /// Maintenance: claim (admitting an index the regular claim refuses) and
    /// queue a `rebuild` job that keeps the old file as a backup.
    Rebuild,
}

impl ProjectAction {
    pub fn parse(action: &str, arg: Option<&str>) -> Result<Self, UiError> {
        Ok(match action {
            "claim" => Self::Claim,
            "release" => Self::Release,
            "check" => Self::Check,
            "reconcile" => Self::Reconcile,
            "remove" => Self::Remove,
            "rebuild" => Self::Rebuild,
            "policy" => Self::Policy(arg.and_then(WatchPolicy::parse).ok_or_else(|| {
                UiError::new(
                    "INVALID_REQUEST",
                    "policy must be session, pinned, manual, or disabled",
                )
            })?),
            other => {
                return Err(UiError::new(
                    "INVALID_REQUEST",
                    format!("unknown action '{other}'"),
                ));
            }
        })
    }

    /// The daemon method and parameters for this action.
    pub fn request(&self) -> (&'static str, Value) {
        match self {
            Self::Claim => (method::OWNERSHIP_ACQUIRE, json!({})),
            Self::Release => (method::OWNERSHIP_RELEASE, json!({})),
            Self::Check => (method::INDEX_CHECK, json!({})),
            Self::Reconcile => (method::INDEX_REQUEST, json!({"kind": "reconcile"})),
            Self::Remove => (method::PROJECT_DETACH, json!({})),
            Self::Rebuild => (method::INDEX_REQUEST, json!({"kind": "rebuild"})),
            Self::Policy(p) => (
                method::PROJECT_CONFIGURE,
                json!({"watchPolicy": p.as_str()}),
            ),
        }
    }
}

pub struct Bridge {
    root: DataRoot,
}

impl Bridge {
    pub fn new(root: DataRoot) -> Self {
        Self { root }
    }

    async fn client(&self) -> Result<Client, UiError> {
        Ok(Client::connect(&self.root, CLIENT_NAME).await?)
    }

    /// Service health plus a snapshot of every project. A service that cannot
    /// be reached is reported as such, never with cached values.
    pub async fn overview(&self) -> Value {
        let mut c = match self.client().await {
            Ok(c) => c,
            Err(e) => {
                return json!({
                    "connected": false,
                    "reason": e.message,
                    "dataRoot": self.root.path().display().to_string(),
                });
            }
        };
        let health = c
            .call_raw(method::DAEMON_HEALTH, None, json!({}), None)
            .await;
        let snapshot = c
            .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
            .await;
        match (health, snapshot) {
            (Ok(health), Ok(snapshot)) => json!({
                "connected": true,
                "health": health,
                "snapshot": snapshot,
                "dataRoot": self.root.path().display().to_string(),
            }),
            (Err(e), _) | (_, Err(e)) => json!({
                "connected": false,
                "reason": UiError::from(e).message,
                "dataRoot": self.root.path().display().to_string(),
            }),
        }
    }

    /// Local, read-only preview of what registering `path` would do: the
    /// resolved checkout, WrongStack's index location, and the inspector's
    /// verdict for any index already there. Nothing is written.
    pub fn preview(&self, path: &str) -> Result<Value, UiError> {
        let checkout = Path::new(path.trim());
        if !checkout.is_absolute() || !checkout.is_dir() {
            return Err(UiError::new(
                "INVALID_REQUEST",
                "enter the absolute path of an existing folder",
            ));
        }
        let global = wrongindex_storage::wstack_global_root().ok_or_else(|| {
            UiError::new("INVALID_REQUEST", "cannot determine the WrongStack home")
        })?;
        let resolved = wrongindex_storage::resolve_index_dir(checkout, &global);
        let db = Path::new(&resolved.index_dir).join("index.db");
        let inspection = wrongindex_storage::inspect_index(&db);
        Ok(json!({
            "resolved": resolved,
            "inspection": inspection,
        }))
    }

    pub async fn add_project(
        &self,
        checkout_root: &str,
        index_dir: &str,
        policy: &str,
    ) -> Result<Value, UiError> {
        let watch_policy = WatchPolicy::parse(policy)
            .ok_or_else(|| UiError::new("INVALID_REQUEST", "unknown watch policy"))?;
        let params = RegisterProjectParams {
            checkout_root: checkout_root.to_string(),
            index_dir: index_dir.to_string(),
            display_name: None,
            watch_policy,
            repository_identity: None,
        };
        let v = self
            .client()
            .await?
            .call_raw(
                method::PROJECT_REGISTER,
                None,
                serde_json::to_value(params).unwrap_or_default(),
                None,
            )
            .await?;
        Ok(v)
    }

    pub async fn project_action(
        &self,
        project_id: &str,
        action: &ProjectAction,
    ) -> Result<Value, UiError> {
        let (m, params) = action.request();
        let mut c = self.client().await?;
        if *action == ProjectAction::Rebuild {
            // Idempotent when this service already owns the index.
            c.call_raw(
                method::OWNERSHIP_ACQUIRE,
                Some(project_id),
                json!({"maintenance": true}),
                None,
            )
            .await?;
        }
        Ok(c.call_raw(m, Some(project_id), params, None).await?)
    }

    pub async fn project(&self, project_id: &str) -> Result<Value, UiError> {
        Ok(self
            .client()
            .await?
            .call_raw(method::PROJECT_GET, Some(project_id), json!({}), None)
            .await?)
    }

    pub async fn jobs(&self, project_id: Option<&str>, limit: usize) -> Result<Value, UiError> {
        Ok(self
            .client()
            .await?
            .call_raw(
                method::JOB_LIST,
                None,
                json!({"projectId": project_id, "limit": limit.min(200)}),
                None,
            )
            .await?)
    }

    pub async fn cancel_job(&self, job_id: &str) -> Result<Value, UiError> {
        Ok(self
            .client()
            .await?
            .call_raw(method::JOB_CANCEL, None, json!({"jobId": job_id}), None)
            .await?)
    }

    pub async fn settings(&self) -> Result<Value, UiError> {
        Ok(self
            .client()
            .await?
            .call_raw(method::SETTINGS_GET, None, json!({}), None)
            .await?)
    }

    /// Apply a settings patch. The daemon validates, persists and applies it
    /// before answering; the reply is the applied snapshot, so the UI never
    /// shows a value the service refused.
    pub async fn set_settings(
        &self,
        patch: Value,
        expected_revision: Option<u64>,
    ) -> Result<Value, UiError> {
        if !patch.is_object() {
            return Err(UiError::new(
                "INVALID_REQUEST",
                "settings must be an object",
            ));
        }
        Ok(self
            .client()
            .await?
            .call_raw(
                method::SETTINGS_SET,
                None,
                json!({"settings": patch, "expectedRevision": expected_revision}),
                None,
            )
            .await?)
    }

    pub async fn ownership(&self, project_id: &str) -> Result<Value, UiError> {
        Ok(self
            .client()
            .await?
            .call_raw(method::OWNERSHIP_STATUS, Some(project_id), json!({}), None)
            .await?)
    }

    /// Read-only symbol search in a project's published index (the search
    /// inspector); served by the daemon like any WrongStack client's query.
    pub async fn search(&self, project_id: &str, query: &str) -> Result<Value, UiError> {
        let query = query.trim();
        if query.is_empty() || query.chars().count() > 200 {
            return Err(UiError::new(
                "INVALID_REQUEST",
                "enter a search term (at most 200 characters)",
            ));
        }
        Ok(self
            .client()
            .await?
            .call_raw(
                method::INDEX_QUERY,
                Some(project_id),
                json!({"op": "search", "args": {"query": query, "limit": 50}}),
                None,
            )
            .await?)
    }

    /// `wrongindex diagnostics export` (the CLI installed next to this
    /// executable), so the desktop and the CLI share one redaction and
    /// bounding implementation. `output: None` is a preview (`--dry-run`).
    pub async fn diagnostics(
        &self,
        redact_paths: bool,
        output: Option<&Path>,
    ) -> Result<Value, UiError> {
        let exe = std::env::current_exe()
            .map_err(|e| UiError::new("INTERNAL", e.to_string()))?
            .with_file_name(if cfg!(windows) {
                "wrongindex.exe"
            } else {
                "wrongindex"
            });
        if !exe.is_file() {
            return Err(UiError::new(
                "INTERNAL",
                format!(
                    "the WrongIndex CLI was not found next to the desktop ({})",
                    exe.display()
                ),
            ));
        }
        let mut cmd = tokio::process::Command::new(&exe);
        cmd.arg("--json")
            .arg("--data-root")
            .arg(self.root.path())
            .args(["diagnostics", "export"]);
        if redact_paths {
            cmd.arg("--redact-paths");
        }
        match output {
            // The save dialog already confirmed any overwrite.
            Some(p) => {
                cmd.arg("--output").arg(p).arg("--force");
            }
            None => {
                cmd.arg("--dry-run");
            }
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let out = tokio::time::timeout(Duration::from_secs(120), cmd.output())
            .await
            .map_err(|_| UiError::new("DEADLINE_EXCEEDED", "diagnostics export took too long"))?
            .map_err(|e| UiError::new("INTERNAL", e.to_string()))?;
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(UiError::new(
                "INTERNAL",
                if msg.is_empty() {
                    format!("diagnostics export failed ({})", out.status)
                } else {
                    msg
                },
            ));
        }
        serde_json::from_slice(&out.stdout)
            .map_err(|e| UiError::new("PROTOCOL", format!("diagnostics output: {e}")))
    }

    /// The checkout folder of a registered project, as recorded by the
    /// daemon (never a path supplied by the WebView).
    pub async fn checkout_root(&self, project_id: &str) -> Result<String, UiError> {
        let v = self.project(project_id).await?;
        let root = v["project"]["checkoutRoot"]
            .as_str()
            .ok_or_else(|| UiError::new("PROTOCOL", "project has no checkout root"))?;
        if !Path::new(root).is_dir() {
            return Err(UiError::new(
                "PROJECT_UNAVAILABLE",
                format!("{root} is not available"),
            ));
        }
        Ok(root.to_string())
    }

    /// Start the daemon installed next to this executable and wait until it
    /// answers. An already running service is reported, not restarted.
    pub async fn start_service(&self) -> Result<Value, UiError> {
        if let Ok(c) = self.client().await {
            return Ok(json!({"started": false, "instanceId": c.hello().daemon_instance_id}));
        }
        let exe = wrongindex_ipc::launch::sibling_daemon_exe()
            .map_err(|e| UiError::new("INTERNAL", e.to_string()))?;
        wrongindex_ipc::launch::spawn_detached(&exe, &self.root)
            .map_err(|e| UiError::new("SERVICE_UNAVAILABLE", e.to_string()))?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(c) = self.client().await {
                return Ok(json!({"started": true, "instanceId": c.hello().daemon_instance_id}));
            }
            if Instant::now() > deadline {
                return Err(UiError::new(
                    "SERVICE_UNAVAILABLE",
                    "the service was started but did not answer within 15 s",
                ));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// Ask the daemon to drain and stop. Indexing ends; projects keep their
    /// registrations and indexes.
    pub async fn stop_service(&self) -> Result<Value, UiError> {
        Ok(self
            .client()
            .await?
            .call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wrongindex_contracts::{ErrorBody, ErrorCode};

    #[test]
    fn actions_map_to_fixed_methods() {
        let cases = [
            ("claim", None, method::OWNERSHIP_ACQUIRE),
            ("release", None, method::OWNERSHIP_RELEASE),
            ("check", None, method::INDEX_CHECK),
            ("reconcile", None, method::INDEX_REQUEST),
            ("remove", None, method::PROJECT_DETACH),
            ("rebuild", None, method::INDEX_REQUEST),
            ("policy", Some("pinned"), method::PROJECT_CONFIGURE),
        ];
        for (a, arg, m) in cases {
            assert_eq!(ProjectAction::parse(a, arg).unwrap().request().0, m, "{a}");
        }
        assert_eq!(
            ProjectAction::parse("reconcile", None).unwrap().request().1,
            json!({"kind": "reconcile"})
        );
        assert_eq!(
            ProjectAction::parse("policy", Some("manual"))
                .unwrap()
                .request()
                .1,
            json!({"watchPolicy": "manual"})
        );
    }

    #[test]
    fn unknown_actions_and_policies_never_reach_the_daemon() {
        for (a, arg) in [
            ("shutdown", None),
            ("daemon.shutdown", None),
            ("policy", None),
            ("policy", Some("always")),
            ("", None),
        ] {
            assert_eq!(
                ProjectAction::parse(a, arg).unwrap_err().code,
                "INVALID_REQUEST",
                "{a}"
            );
        }
    }

    #[test]
    fn remote_errors_keep_their_stable_code() {
        let e: UiError =
            CallError::Remote(ErrorBody::new(ErrorCode::OwnerConflict, "held by x")).into();
        assert_eq!(e, UiError::new("OWNER_CONFLICT", "held by x"));
        let e: UiError = CallError::Unavailable("no discovery".into()).into();
        assert_eq!(e.code, "SERVICE_UNAVAILABLE");
    }

    #[tokio::test]
    async fn search_terms_are_validated_before_the_service_is_called() {
        let tmp = std::env::temp_dir().join(format!("wi-desktop-s-{}", std::process::id()));
        let b = Bridge::new(DataRoot::resolve(Some(&tmp)).unwrap());
        assert_eq!(
            b.search("p", "   ").await.unwrap_err().code,
            "INVALID_REQUEST"
        );
        assert_eq!(
            b.search("p", &"x".repeat(201)).await.unwrap_err().code,
            "INVALID_REQUEST"
        );
        // A valid term reaches the (absent) service.
        assert_eq!(
            b.search("p", "Runner").await.unwrap_err().code,
            "SERVICE_UNAVAILABLE"
        );
    }

    #[test]
    fn preview_rejects_relative_and_missing_paths() {
        let b = Bridge::new(DataRoot::resolve(Some(Path::new(r"C:\nonexistent-wi"))).unwrap());
        assert_eq!(
            b.preview("relative\\dir").unwrap_err().code,
            "INVALID_REQUEST"
        );
        assert_eq!(
            b.preview(r"C:\definitely\not\here\wi").unwrap_err().code,
            "INVALID_REQUEST"
        );
    }

    #[tokio::test]
    async fn unreachable_service_is_reported_not_cached() {
        let tmp = std::env::temp_dir().join(format!("wi-desktop-{}", std::process::id()));
        let b = Bridge::new(DataRoot::resolve(Some(&tmp)).unwrap());
        let v = b.overview().await;
        assert_eq!(v["connected"], false);
        assert!(v.get("health").is_none() && v.get("snapshot").is_none());
    }
}
