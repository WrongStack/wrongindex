use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use wrongindex_contracts::WatchPolicy;

use crate::logic::ProjectAction;

/// Service events kept for Activity.
pub const ACTIVITY_CAP: usize = 200;

/// Everything the window shows, as last read from the service (or, for the
/// machine scan, update state and the sign-in entry, from this computer).
/// Never a local guess: a value is replaced only by a newer reading.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub connected: bool,
    /// Why the service cannot be reached.
    pub reason: Option<String>,
    pub health: Value,
    /// `events.snapshot`: `projects`, `resources`, `seq`.
    pub events: Value,
    pub metrics: Value,
    /// `settings.get`: `settings`, `defaults`, `revision`.
    pub settings: Value,
    /// Recent jobs of every project, newest first.
    pub jobs: Vec<Value>,
    /// The project open in the detail view.
    pub detail: Option<Detail>,
    pub preview: Option<Preview>,
    pub machine: Machine,
    pub search: Option<Search>,
    /// Category sizes of the diagnostics bundle that would be exported.
    pub diagnostics: Option<Value>,
    pub update_check: Option<Result<Value, String>>,
    pub update_last: Value,
    pub autostart: Option<Result<Value, String>>,
    /// Service events seen by this window, newest first.
    pub activity: VecDeque<Value>,
}

impl Snapshot {
    pub fn projects(&self) -> &[Value] {
        self.events["projects"]
            .as_array()
            .map_or(&[], Vec::as_slice)
    }

    pub fn resources(&self) -> &[Value] {
        self.events["resources"]
            .as_array()
            .map_or(&[], Vec::as_slice)
    }

    pub fn project(&self, id: &str) -> Option<&Value> {
        self.projects()
            .iter()
            .find(|p| p["project"]["projectId"] == id)
    }

    pub fn project_name(&self, id: &str) -> String {
        self.project(id)
            .and_then(|p| p["project"]["displayName"].as_str())
            .unwrap_or(id)
            .to_string()
    }
}

#[derive(Clone)]
pub struct Detail {
    pub project_id: String,
    /// `project.get`: a project status.
    pub status: Value,
    pub jobs: Vec<Value>,
    pub ownership: Value,
    pub backups: Vec<Value>,
    /// `index.query stats` for the published generation, when available.
    pub stats: Option<Value>,
}

/// What registering a folder would do, read without changing anything.
#[derive(Clone)]
pub struct Preview {
    pub path: PathBuf,
    /// `resolved`: `projectRoot`, `indexDir`, `linkedWorktree`.
    pub resolved: Value,
    /// The inspector's verdict for an index already there.
    pub inspection: Value,
}

#[derive(Clone, Default)]
pub struct Machine {
    pub scan: Option<Arc<Value>>,
    pub error: Option<String>,
    pub at_ms: i64,
}

#[derive(Clone)]
pub struct Search {
    pub project_id: String,
    pub query: String,
    pub result: Value,
}

/// A request from the window. Each carries a sequence number; its outcome
/// comes back as an [`Outcome`] with the same number.
#[derive(Clone, Debug)]
pub enum Command {
    Start,
    Stop,
    /// The project whose detail the window shows (`None`: the list).
    Select(Option<String>),
    Preview(PathBuf),
    DismissPreview,
    Register {
        root: String,
        index_dir: String,
        policy: WatchPolicy,
    },
    Action {
        project: String,
        action: ProjectAction,
    },
    CancelJob(String),
    OpenFolder(String),
    ApplySettings {
        patch: Value,
        revision: Option<u64>,
    },
    ScanMachine,
    Search {
        project: String,
        query: String,
    },
    DiagnosticsPreview {
        redact: bool,
    },
    ExportDiagnostics {
        redact: bool,
        path: PathBuf,
    },
    UpdateCheck {
        quiet: bool,
    },
    UpdateStart,
    UpdateLast,
    AutostartRead,
    Autostart(bool),
}

/// How a request ended: a message for the window, good or bad.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub sequence: u64,
    pub ok: bool,
    pub message: Option<String>,
    /// A project to open (a registration just made).
    pub select: Option<String>,
}
