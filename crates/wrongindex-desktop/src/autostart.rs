//! "Start the index service when I sign in" (plan 2.7 Startup, WI-701).
//!
//! A per-user `HKCU\...\Run` value starts this desktop executable with
//! `--start-service`: it launches the service installed next to it (if not
//! already running) and exits without opening a window. The desktop window
//! is never opened at sign-in; quitting it never affects the service.

/// The Run value name; the installer's uninstall hook removes the same name.
pub const VALUE_NAME: &str = "WrongIndex";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Argument that makes the desktop start the service and exit.
pub const START_SERVICE_ARG: &str = "--start-service";

/// The command line registered for sign-in.
pub fn command_line() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(format!("\"{}\" {START_SERVICE_ARG}", exe.display()))
}

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
    };
    use windows::core::HSTRING;

    use super::RUN_KEY;

    /// The registered command line, if any.
    pub fn get(value: &str) -> Result<Option<String>, String> {
        let (key, name) = (HSTRING::from(RUN_KEY), HSTRING::from(value));
        let mut bytes: u32 = 0;
        // SAFETY: size query with no buffer.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &key,
                &name,
                RRF_RT_REG_SZ,
                None,
                None,
                Some(&mut bytes),
            )
        };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if rc != ERROR_SUCCESS {
            return Err(format!("reading the sign-in entry failed ({rc:?})"));
        }
        let mut buf = vec![0u16; (bytes as usize).div_ceil(2) + 1];
        let mut len = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `len` bytes.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                &key,
                &name,
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut len),
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(format!("reading the sign-in entry failed ({rc:?})"));
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(Some(String::from_utf16_lossy(&buf[..end])))
    }

    pub fn set(value: &str, command: &str) -> Result<(), String> {
        let (key, name) = (HSTRING::from(RUN_KEY), HSTRING::from(value));
        let data: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `data` is a NUL-terminated UTF-16 string of the given size.
        let rc = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &key,
                &name,
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * 2) as u32,
            )
        };
        if rc == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("writing the sign-in entry failed ({rc:?})"))
        }
    }

    pub fn remove(value: &str) -> Result<(), String> {
        let (key, name) = (HSTRING::from(RUN_KEY), HSTRING::from(value));
        // SAFETY: plain registry call with owned strings.
        let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &key, &name) };
        if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!("removing the sign-in entry failed ({rc:?})"))
        }
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn get(_value: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
    pub fn set(_value: &str, _command: &str) -> Result<(), String> {
        Err("sign-in start is only implemented on Windows".into())
    }
    pub fn remove(_value: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Current state: enabled, and whether the entry points at this executable.
pub fn status() -> Result<serde_json::Value, String> {
    let current = imp::get(VALUE_NAME)?;
    let expected = command_line()?;
    Ok(serde_json::json!({
        "enabled": current.is_some(),
        "matchesThisInstall": current.as_deref() == Some(expected.as_str()),
        "command": current,
    }))
}

pub fn set_enabled(enabled: bool) -> Result<serde_json::Value, String> {
    if enabled {
        imp::set(VALUE_NAME, &command_line()?)?;
    } else {
        imp::remove(VALUE_NAME)?;
    }
    status()
}

#[cfg(all(test, windows))]
mod tests {
    use super::imp;

    /// Round trip under a throw-away value name (never the real entry).
    #[test]
    fn run_entry_round_trip() {
        let name = format!("WrongIndexTest-{}", std::process::id());
        let cmd = r#""C:\Program Files\x y\app.exe" --start-service"#;
        assert_eq!(imp::get(&name).unwrap(), None);
        imp::set(&name, cmd).unwrap();
        assert_eq!(imp::get(&name).unwrap().as_deref(), Some(cmd));
        imp::remove(&name).unwrap();
        assert_eq!(imp::get(&name).unwrap(), None);
        // Removing a missing entry is not an error.
        imp::remove(&name).unwrap();
    }
}
