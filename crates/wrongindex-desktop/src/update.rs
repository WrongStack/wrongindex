//! Updates from the desktop (WI-701).
//!
//! The CLI installed next to this executable does the work
//! (`wrongindex self-update`): the desktop and the command line share one
//! implementation of the release lookup, download, SHA-256 verification and
//! silent install. The installer closes this window while it replaces the
//! files and `self-update` reopens it afterwards, so an update started here
//! runs detached from this process and leaves its outcome in
//! `<data root>/updates/last-update.json` (stdout) and `last-update.log`
//! (stderr) for the reopened window to show.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::bridge::UiError;

pub const RESULT_FILE: &str = "last-update.json";
pub const LOG_FILE: &str = "last-update.log";

/// Longest stderr text shown for a failed update.
const LOG_SHOWN: usize = 4000;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// The CLI installed next to this executable.
pub fn sibling_cli() -> Result<PathBuf, UiError> {
    let exe = std::env::current_exe()
        .map_err(|e| UiError::new("INTERNAL", e.to_string()))?
        .with_file_name(if cfg!(windows) {
            "wrongindex.exe"
        } else {
            "wrongindex"
        });
    if exe.is_file() {
        Ok(exe)
    } else {
        Err(UiError::new(
            "INTERNAL",
            format!(
                "the WrongIndex CLI was not found next to the desktop ({})",
                exe.display()
            ),
        ))
    }
}

/// An installed copy (the uninstaller next to it), which is what
/// `self-update` updates; a build from source checks but does not install.
pub fn installed() -> bool {
    std::env::current_exe()
        .map(|e| e.with_file_name("uninstall.exe").is_file())
        .unwrap_or(false)
}

/// `wrongindex self-update --check`: the latest release and whether it is
/// newer than this copy. Nothing is downloaded.
pub async fn check() -> Result<Value, UiError> {
    let mut cmd = tokio::process::Command::new(sibling_cli()?);
    cmd.args(["--json", "self-update", "--check"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(Duration::from_secs(90), cmd.output())
        .await
        .map_err(|_| UiError::new("DEADLINE_EXCEEDED", "the update check took too long"))?
        .map_err(|e| UiError::new("INTERNAL", e.to_string()))?;
    if !out.status.success() {
        return Err(UiError::new("UPDATE_FAILED", cli_error(&out.stderr)));
    }
    let mut v: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| UiError::new("PROTOCOL", format!("update check output: {e}")))?;
    v["installed"] = json!(installed());
    v["desktopVersion"] = json!(env!("CARGO_PKG_VERSION"));
    Ok(v)
}

/// The CLI's own message (`wrongindex: ...`) without its prefix.
fn cli_error(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    let text = text.strip_prefix("wrongindex: ").unwrap_or(text);
    if text.is_empty() {
        "the update failed without a message".to_string()
    } else {
        tail(text, LOG_SHOWN)
    }
}

fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

/// An update started from this window.
pub struct Updater {
    root: PathBuf,
    running: Mutex<Option<Child>>,
}

impl Updater {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            running: Mutex::new(None),
        }
    }

    fn dir(&self) -> PathBuf {
        self.root.join("updates")
    }

    fn is_running(&self) -> bool {
        let mut guard = self.running.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_mut().map(|c| c.try_wait()) {
            Some(Ok(None)) => true,
            Some(_) => {
                *guard = None;
                false
            }
            None => false,
        }
    }

    /// Start `wrongindex self-update` (latest release) detached. It outlives
    /// this window, which the installer closes; the outcome goes to the two
    /// files that `last` reads.
    pub fn start(&self) -> Result<Value, UiError> {
        if !installed() {
            return Err(UiError::new(
                "INVALID_REQUEST",
                "this copy was not installed by the WrongIndex installer, so it cannot update itself",
            ));
        }
        if self.is_running() {
            return Err(UiError::new(
                "INVALID_REQUEST",
                "an update is already running",
            ));
        }
        let cli = sibling_cli()?;
        let dir = self.dir();
        std::fs::create_dir_all(&dir).map_err(|e| UiError::new("INTERNAL", e.to_string()))?;
        let create = |name: &str| {
            File::create(dir.join(name)).map_err(|e| UiError::new("INTERNAL", e.to_string()))
        };
        let (out, err) = (create(RESULT_FILE)?, create(LOG_FILE)?);
        let mut cmd = Command::new(cli);
        cmd.arg("--json")
            .arg("--data-root")
            .arg(&self.root)
            .arg("self-update")
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        // A hidden console: the CLI's own console children (`--version`,
        // `daemon start`) share it instead of opening windows.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        }
        let child = cmd
            .spawn()
            .map_err(|e| UiError::new("INTERNAL", format!("starting the update: {e}")))?;
        *self.running.lock().unwrap_or_else(|p| p.into_inner()) = Some(child);
        Ok(json!({"started": true}))
    }

    /// The last update started from a desktop: still running here, or its
    /// outcome (`result` on success, `error` on failure) and when it was last
    /// written (`atMs`: the start, or the end once it finished). Neither
    /// outcome and nothing running here: still running elsewhere, or cut
    /// short (`pending`). No `atMs` when there was none.
    pub fn last(&self) -> Value {
        let running = self.is_running();
        let dir = self.dir();
        let result_path = dir.join(RESULT_FILE);
        let version = env!("CARGO_PKG_VERSION");
        let Ok(meta) = std::fs::metadata(&result_path) else {
            return json!({"running": running, "desktopVersion": version});
        };
        let log_path = dir.join(LOG_FILE);
        // Creation times survive a re-create on Windows: use the last write.
        let at = [Some(meta), std::fs::metadata(&log_path).ok()]
            .into_iter()
            .flatten()
            .filter_map(|m| m.modified().ok())
            .filter_map(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .max();
        let result = std::fs::read(&result_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
        let log = std::fs::read(&log_path).unwrap_or_default();
        let finished = !running && (result.is_some() || !log.trim_ascii().is_empty());
        let error = (finished && result.is_none()).then(|| cli_error(&log));
        json!({
            "running": running,
            "pending": !running && !finished,
            "atMs": at,
            "result": result,
            "error": error,
            "desktopVersion": version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_errors_lose_their_prefix_and_stay_bounded() {
        assert_eq!(
            cli_error(b"wrongindex: checksum mismatch\r\n"),
            "checksum mismatch"
        );
        assert_eq!(cli_error(b"  "), "the update failed without a message");
        let long = format!("wrongindex: {}", "é".repeat(5000));
        let shown = cli_error(long.as_bytes());
        assert!(shown.starts_with('…'));
        assert!(shown.len() <= LOG_SHOWN + '…'.len_utf8());
    }

    #[test]
    fn last_reports_nothing_before_any_update_and_the_outcome_after() {
        let dir = tempfile::tempdir().unwrap();
        let u = Updater::new(dir.path());
        let none = u.last();
        assert_eq!(none["running"], false);
        assert!(none.get("atMs").is_none());

        // Started elsewhere and not finished (or cut short): no verdict.
        let updates = dir.path().join("updates");
        std::fs::create_dir_all(&updates).unwrap();
        std::fs::write(updates.join(RESULT_FILE), "").unwrap();
        std::fs::write(updates.join(LOG_FILE), "").unwrap();
        let pending = u.last();
        assert_eq!(pending["pending"], true);
        assert!(pending["error"].is_null() && pending["result"].is_null());

        std::fs::write(
            updates.join(RESULT_FILE),
            r#"{"updated":true,"from":"0.1.0","to":"0.1.1"}"#,
        )
        .unwrap();
        let ok = u.last();
        assert_eq!(ok["result"]["to"], "0.1.1");
        assert_eq!(ok["pending"], false);
        assert!(ok["error"].is_null());
        assert!(ok["atMs"].as_u64().unwrap() > 0);

        // A failed run leaves no JSON, only the message.
        std::fs::write(updates.join(RESULT_FILE), "").unwrap();
        std::fs::write(
            updates.join(LOG_FILE),
            "wrongindex: checksum mismatch for WrongIndex_0.1.1_x64-setup.exe\n",
        )
        .unwrap();
        let failed = u.last();
        assert!(failed["result"].is_null());
        assert_eq!(
            failed["error"],
            "checksum mismatch for WrongIndex_0.1.1_x64-setup.exe"
        );
    }

    #[test]
    fn a_copy_without_an_uninstaller_does_not_start_an_update() {
        // The test binary has no uninstall.exe next to it.
        let dir = tempfile::tempdir().unwrap();
        let e = Updater::new(dir.path()).start().unwrap_err();
        assert_eq!(e.code, "INVALID_REQUEST");
        assert!(!dir.path().join("updates").exists());
    }
}
