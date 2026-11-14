//! What the window does outside the service: the sign-in entry, updates and
//! diagnostics through the CLI installed next to it, local times and the
//! window's own preferences.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

// ─── local time ─────────────────────────────────────────────────────────────

/// `YYYY-MM-DD HH:MM:SS` in the user's time zone.
pub fn local_time(ms: i64) -> String {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let ticks = (ms as u64).saturating_add(11_644_473_600_000) * 10_000;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    // SAFETY: plain conversions between stack values this function owns.
    let ok = unsafe {
        FileTimeToSystemTime(&ft, &mut utc) != 0
            && SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) != 0
    };
    if !ok {
        return format!("{ms} ms");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond
    )
}

// ─── sign-in start ──────────────────────────────────────────────────────────

/// The `HKCU\...\Run` value name; the installer's hooks use the same name.
pub const AUTOSTART_VALUE: &str = "WrongIndex";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Argument that makes this program start the service and exit, without a
/// window.
pub const START_SERVICE_ARG: &str = "--start-service";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

mod registry {
    use super::{RUN_KEY, wide};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };

    /// The registered command line, if any.
    pub fn get(value: &str) -> Result<Option<String>, String> {
        let (key, name) = (wide(RUN_KEY), wide(value));
        let mut bytes: u32 = 0;
        // SAFETY: size query with no buffer; strings are NUL-terminated.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut bytes,
            )
        };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if rc != ERROR_SUCCESS {
            return Err(format!("reading the sign-in entry failed ({rc})"));
        }
        let mut buf = vec![0u16; (bytes as usize).div_ceil(2) + 1];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `len` bytes.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("reading the sign-in entry failed ({rc})"));
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(Some(String::from_utf16_lossy(&buf[..end])))
    }

    pub fn set(value: &str, command: &str) -> Result<(), String> {
        let (key, name, data) = (wide(RUN_KEY), wide(value), wide(command));
        // SAFETY: `data` is a NUL-terminated UTF-16 string of the given size.
        let rc = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        };
        if rc == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("writing the sign-in entry failed ({rc})"))
        }
    }

    pub fn remove(value: &str) -> Result<(), String> {
        let (key, name) = (wide(RUN_KEY), wide(value));
        // SAFETY: plain registry call with NUL-terminated strings.
        let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
        if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!("removing the sign-in entry failed ({rc})"))
        }
    }
}

/// The command line registered for sign-in: this program, no window.
pub fn autostart_command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(format!("\"{}\" {START_SERVICE_ARG}", exe.display()))
}

/// Enabled, and whether the entry points at this copy.
pub fn autostart_status() -> Result<Value, String> {
    let current = registry::get(AUTOSTART_VALUE)?;
    let expected = autostart_command()?;
    Ok(json!({
        "enabled": current.is_some(),
        "matchesThisInstall": current.as_deref() == Some(expected.as_str()),
        "command": current,
    }))
}

pub fn set_autostart(enabled: bool) -> Result<Value, String> {
    if enabled {
        registry::set(AUTOSTART_VALUE, &autostart_command()?)?;
    } else {
        registry::remove(AUTOSTART_VALUE)?;
    }
    autostart_status()
}

// ─── the CLI next to this program ───────────────────────────────────────────

/// The CLI installed next to this executable.
pub fn sibling_cli() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|e| e.to_string())?
        .with_file_name("wrongindex.exe");
    if exe.is_file() {
        Ok(exe)
    } else {
        Err(format!(
            "the WrongIndex CLI was not found next to this program ({})",
            exe.display()
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

/// The CLI's own message (`wrongindex: ...`) without its prefix, bounded.
fn cli_error(stderr: &[u8], fallback: &str) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    let text = text.strip_prefix("wrongindex: ").unwrap_or(text);
    if text.is_empty() {
        fallback.to_string()
    } else {
        tail(text, 4000)
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

async fn run_cli(args: &[&std::ffi::OsStr], limit: Duration, what: &str) -> Result<Value, String> {
    let mut cmd = tokio::process::Command::new(sibling_cli()?);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .creation_flags(CREATE_NO_WINDOW);
    let out = tokio::time::timeout(limit, cmd.output())
        .await
        .map_err(|_| format!("{what} took too long"))?
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(cli_error(
            &out.stderr,
            &format!("{what} failed ({})", out.status),
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("{what} output: {e}"))
}

/// `wrongindex diagnostics export`, so the window and the CLI share one
/// redaction and bounding implementation. `output: None` is a preview.
pub async fn diagnostics(
    root: &Path,
    redact_paths: bool,
    output: Option<&Path>,
) -> Result<Value, String> {
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "--json".as_ref(),
        "--data-root".as_ref(),
        root.as_os_str(),
        "diagnostics".as_ref(),
        "export".as_ref(),
    ];
    if redact_paths {
        args.push("--redact-paths".as_ref());
    }
    match output {
        // The save dialog already confirmed any overwrite.
        Some(p) => args.extend(["--output".as_ref(), p.as_os_str(), "--force".as_ref()]),
        None => args.push("--dry-run".as_ref()),
    }
    run_cli(&args, Duration::from_secs(120), "diagnostics export").await
}

/// `wrongindex self-update --check`: the latest release and whether it is
/// newer than this copy. Nothing is downloaded.
pub async fn update_check() -> Result<Value, String> {
    let mut v = run_cli(
        &[
            "--json".as_ref(),
            "self-update".as_ref(),
            "--check".as_ref(),
        ],
        Duration::from_secs(90),
        "the update check",
    )
    .await?;
    v["installed"] = json!(installed());
    v["desktopVersion"] = json!(env!("CARGO_PKG_VERSION"));
    Ok(v)
}

pub const UPDATE_RESULT_FILE: &str = "last-update.json";
pub const UPDATE_LOG_FILE: &str = "last-update.log";

/// An update started from this window. `wrongindex self-update` does the
/// work detached: the installer closes this window while it replaces the
/// files and `self-update` reopens it afterwards. Its outcome goes to
/// `<data root>/updates/last-update.json` (stdout) and `last-update.log`
/// (stderr) for the reopened window to show.
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

    pub fn start(&self) -> Result<(), String> {
        if !installed() {
            return Err("this copy was not installed by the WrongIndex installer, so it cannot update itself".into());
        }
        if self.is_running() {
            return Err("an update is already running".into());
        }
        let cli = sibling_cli()?;
        let dir = self.dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let create = |name: &str| File::create(dir.join(name)).map_err(|e| e.to_string());
        let (out, err) = (create(UPDATE_RESULT_FILE)?, create(UPDATE_LOG_FILE)?);
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
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        }
        let child = cmd
            .spawn()
            .map_err(|e| format!("starting the update: {e}"))?;
        *self.running.lock().unwrap_or_else(|p| p.into_inner()) = Some(child);
        Ok(())
    }

    /// The last update started from a window: still running here, or its
    /// outcome (`result` on success, `error` on failure) and when it was last
    /// written (`atMs`). Neither outcome and nothing running here: still
    /// running elsewhere, or cut short (`pending`).
    pub fn last(&self) -> Value {
        let running = self.is_running();
        let dir = self.dir();
        let result_path = dir.join(UPDATE_RESULT_FILE);
        let version = env!("CARGO_PKG_VERSION");
        let Ok(meta) = std::fs::metadata(&result_path) else {
            return json!({"running": running, "desktopVersion": version});
        };
        let log_path = dir.join(UPDATE_LOG_FILE);
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
        let error = (finished && result.is_none())
            .then(|| cli_error(&log, "the update failed without a message"));
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

// ─── window preferences ─────────────────────────────────────────────────────

/// Per-user conveniences of this window (never service state). Kept in
/// `<data root>/desktop/preferences.json`; the window works the same
/// without the file.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Prefs {
    pub auto_check_updates: bool,
    pub last_check_ms: i64,
    pub seen_update_ms: i64,
    pub index_filter: String,
}

impl Prefs {
    fn path(root: &Path) -> PathBuf {
        root.join("desktop").join("preferences.json")
    }

    pub fn load(root: &Path) -> Self {
        std::fs::read(Self::path(root))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Best effort: a preference that cannot be kept is only lost.
    pub fn save(&self, root: &Path) {
        let path = Self::path(root);
        if let Some(dir) = path.parent()
            && std::fs::create_dir_all(dir).is_ok()
            && let Ok(bytes) = serde_json::to_vec_pretty(self)
        {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

/// The checkout folder of a registered project, opened in Explorer. The
/// path comes from the service's registration, never from typed input.
pub fn open_folder(root: &str) -> Result<(), String> {
    if !Path::new(root).is_dir() {
        return Err(format!("{root} is not available"));
    }
    Command::new("explorer.exe")
        .arg(root)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round trip under a throw-away value name (never the real entry).
    #[test]
    fn run_entry_round_trip() {
        let name = format!("WrongIndexTest-{}", std::process::id());
        let cmd = r#""C:\Program Files\x y\app.exe" --start-service"#;
        assert_eq!(registry::get(&name).unwrap(), None);
        registry::set(&name, cmd).unwrap();
        assert_eq!(registry::get(&name).unwrap().as_deref(), Some(cmd));
        registry::remove(&name).unwrap();
        assert_eq!(registry::get(&name).unwrap(), None);
        // Removing a missing entry is not an error.
        registry::remove(&name).unwrap();
    }

    #[test]
    fn cli_errors_lose_their_prefix_and_stay_bounded() {
        assert_eq!(
            cli_error(b"wrongindex: checksum mismatch\r\n", "x"),
            "checksum mismatch"
        );
        assert_eq!(cli_error(b"  ", "fallback"), "fallback");
        let long = format!("wrongindex: {}", "é".repeat(5000));
        let shown = cli_error(long.as_bytes(), "x");
        assert!(shown.starts_with('…'));
        assert!(shown.len() <= 4000 + '…'.len_utf8());
    }

    #[test]
    fn last_update_reports_nothing_before_any_update_and_the_outcome_after() {
        let dir = tempfile::tempdir().unwrap();
        let u = Updater::new(dir.path());
        let none = u.last();
        assert_eq!(none["running"], false);
        assert!(none.get("atMs").is_none());

        let updates = dir.path().join("updates");
        std::fs::create_dir_all(&updates).unwrap();
        std::fs::write(updates.join(UPDATE_RESULT_FILE), "").unwrap();
        std::fs::write(updates.join(UPDATE_LOG_FILE), "").unwrap();
        let pending = u.last();
        assert_eq!(pending["pending"], true);
        assert!(pending["error"].is_null() && pending["result"].is_null());

        std::fs::write(
            updates.join(UPDATE_RESULT_FILE),
            r#"{"updated":true,"from":"0.1.0","to":"0.1.1"}"#,
        )
        .unwrap();
        let ok = u.last();
        assert_eq!(ok["result"]["to"], "0.1.1");
        assert_eq!(ok["pending"], false);
        assert!(ok["atMs"].as_u64().unwrap() > 0);

        std::fs::write(updates.join(UPDATE_RESULT_FILE), "").unwrap();
        std::fs::write(
            updates.join(UPDATE_LOG_FILE),
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
        let dir = tempfile::tempdir().unwrap();
        assert!(Updater::new(dir.path()).start().is_err());
        assert!(!dir.path().join("updates").exists());
    }

    #[test]
    fn preferences_round_trip_and_tolerate_damage() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Prefs::load(dir.path()), Prefs::default());
        let p = Prefs {
            auto_check_updates: true,
            last_check_ms: 5,
            seen_update_ms: 7,
            index_filter: "day".into(),
        };
        p.save(dir.path());
        assert_eq!(Prefs::load(dir.path()), p);
        std::fs::write(dir.path().join("desktop/preferences.json"), "{oops").unwrap();
        assert_eq!(Prefs::load(dir.path()), Prefs::default());
    }

    #[test]
    fn local_times_are_formatted() {
        let t = local_time(1_700_000_000_000);
        assert_eq!(t.len(), 19, "{t}");
        assert!(t.starts_with("2023-11-1"), "{t}");
    }
}
