//! Minimal structured logging: one JSON object per line on stderr, plus a
//! bounded file under `<data-root>/logs`. Never logs tokens or source text.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::OnceLock;

use serde_json::{Value, json};

/// The log file is rotated (single `.1` backup, replaced each time) when it
/// reaches this size: at startup and while the service runs, so a service
/// that runs for weeks keeps at most about twice this on disk.
pub const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

static SINK: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();

struct Sink {
    path: PathBuf,
    file: Option<File>,
    size: u64,
    max: u64,
    /// Size at which the next rotation is tried (later after one failed:
    /// another process may hold the file without sharing).
    rotate_at: u64,
}

fn open_append(path: &Path) -> Option<File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}

impl Sink {
    fn open(path: PathBuf, max: u64) -> Sink {
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut sink = Sink {
            file: None,
            size,
            max,
            rotate_at: max,
            path,
        };
        if sink.size >= sink.max {
            sink.rotate();
        } else {
            sink.file = open_append(&sink.path);
        }
        sink
    }

    fn rotate(&mut self) {
        // Closed first: Windows does not rename a file this process holds
        // open without delete sharing.
        self.file = None;
        let rotated = std::fs::rename(&self.path, self.path.with_extension("log.1")).is_ok();
        self.file = open_append(&self.path);
        self.size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        self.rotate_at = if rotated {
            self.max
        } else {
            self.size + self.max / 16
        };
    }

    fn write_line(&mut self, line: &str) {
        if self.size >= self.rotate_at {
            self.rotate();
        }
        if let Some(f) = self.file.as_mut()
            && writeln!(f, "{line}").is_ok()
        {
            self.size += line.len() as u64 + 1;
        }
    }
}

pub fn init(logs_dir: &Path) {
    let sink = Sink::open(logs_dir.join("wrongindexd.log"), MAX_LOG_BYTES);
    let _ = SINK.set(Mutex::new(Some(sink)));
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
        && let Some(s) = guard.as_mut()
    {
        s.write_line(&line);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_is_rotated_while_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrongindexd.log");
        let backup = dir.path().join("wrongindexd.log.1");
        let line = "x".repeat(99); // 100 bytes with the newline
        let mut sink = Sink::open(path.clone(), 1000);
        for _ in 0..25 {
            sink.write_line(&line);
        }
        // 25 lines: rotated at 1000 bytes, then again at 2000.
        let len = |p: &Path| std::fs::metadata(p).unwrap().len();
        assert_eq!(len(&backup), 1000);
        assert_eq!(len(&path), 500);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            2,
            "one backup only"
        );
        // A file found over the limit at startup is rotated at once.
        drop(sink);
        std::fs::write(&path, vec![b'y'; 1500]).unwrap();
        let mut sink = Sink::open(path.clone(), 1000);
        assert_eq!(len(&backup), 1500);
        sink.write_line(&line);
        assert_eq!(len(&path), 100);
    }

    #[cfg(windows)]
    #[test]
    fn a_file_held_without_delete_sharing_is_tried_again_later() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrongindexd.log");
        let backup = dir.path().join("wrongindexd.log.1");
        let line = "x".repeat(9); // 10 bytes with the newline
        let mut sink = Sink::open(path.clone(), 1000);
        for _ in 0..100 {
            sink.write_line(&line);
        }
        // Another reader without FILE_SHARE_DELETE: the rename fails, and
        // the line is still written.
        let held = OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2) // read | write
            .open(&path)
            .unwrap();
        sink.write_line(&line);
        assert!(!backup.exists());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1010);
        // Not tried again on every line, but after max/16 more bytes.
        drop(held);
        sink.write_line(&line);
        assert!(!backup.exists());
        for _ in 0..10 {
            sink.write_line(&line);
        }
        assert!(backup.exists());
    }
}
