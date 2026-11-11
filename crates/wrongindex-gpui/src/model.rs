use serde_json::Value;
use std::path::PathBuf;
use wrongindex_contracts::ProjectStatus;

#[derive(Clone, Default)]
pub struct Snapshot {
    pub connected: bool,
    pub error: Option<String>,
    pub health: Value,
    pub metrics: Value,
    pub projects: Vec<ProjectStatus>,
    pub jobs: Vec<Value>,
    pub settings: Value,
    pub stats: Value,
    pub stats_project: Option<String>,
    pub notice: Option<String>,
    pub preview: Option<Preview>,
    pub acknowledged: u64,
}

#[derive(Clone)]
pub struct Preview {
    pub root: PathBuf,
    pub index_dir: String,
    pub verdict: String,
}

#[derive(Clone)]
pub enum Command {
    Start,
    Refresh,
    Select(String),
    Preview(PathBuf),
    Register {
        root: PathBuf,
        index_dir: String,
        pinned: bool,
    },
    Index {
        project: String,
        kind: &'static str,
    },
    Policy {
        project: String,
        policy: &'static str,
    },
    Pause(bool),
    DismissPreview,
}

pub fn readable_bytes(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.0} MB", bytes as f64 / (1u64 << 20) as f64)
    }
}
