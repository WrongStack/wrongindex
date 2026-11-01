//! Minimal structured logging: one JSON object per line on stderr, plus a
//! bounded file under `<data-root>/logs`. Never logs tokens or source text.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;

use serde_json::{Value, json};

/// The log file is rotated (single `.1` backup) when it exceeds this size.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

static SINK: OnceLock<Mutex<Option<File>>> = OnceLock::new();

pub fn init(logs_dir: &Path) {
    let path = logs_dir.join("wrongindexd.log");
    if std::fs::metadata(&path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&path, logs_dir.join("wrongindexd.log.1"));
    }
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    let _ = SINK.set(Mutex::new(file));
}

pub fn event(level: &str, msg: &str, fields: Value) {
    let line = json!({
        "ts": wrongindex_registry::now_ms(),
        "level": level,
        "msg": msg,
        "pid": std::process::id(),
        "fields": fields,
    })
    .to_string();
    eprintln!("{line}");
    if let Some(sink) = SINK.get()
        && let Ok(mut guard) = sink.lock()
        && let Some(f) = guard.as_mut()
    {
        let _ = writeln!(f, "{line}");
    }
}

#[macro_export]
macro_rules! info {
    ($msg:expr) => { $crate::log::event("info", $msg, serde_json::Value::Null) };
    ($msg:expr, $($fields:tt)+) => { $crate::log::event("info", $msg, serde_json::json!($($fields)+)) };
}

#[macro_export]
macro_rules! warn {
    ($msg:expr) => { $crate::log::event("warn", $msg, serde_json::Value::Null) };
    ($msg:expr, $($fields:tt)+) => { $crate::log::event("warn", $msg, serde_json::json!($($fields)+)) };
}
