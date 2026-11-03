//! The install directory on the user's PATH (`HKCU\Environment\Path`), added
//! by the installer and removed by the uninstaller through the hidden
//! `wrongindex setup add-to-path|remove-from-path` commands. Other entries
//! are kept exactly as written, `%VAR%` references and the value's
//! `REG_EXPAND_SZ` type included.

use std::io;
use std::path::Path;

/// Normalized form for comparing PATH entries: case-insensitive, without
/// surrounding quotes or a trailing separator.
fn same_dir(entry: &str, dir: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .trim_matches('"')
            .trim_end_matches(['\\', '/'])
            .replace('/', "\\")
            .to_lowercase()
    };
    !entry.trim().is_empty() && norm(entry) == norm(dir)
}

/// The edited value, or `None` when nothing changes. Adding then removing
/// gives back the value byte for byte: a trailing separator (Windows writes
/// `...\WindowsApps;` on a new account) stays after the added entry, and
/// removing drops only the matching entries (empty ones stay).
pub fn edit(value: &str, dir: &str, add: bool) -> Option<String> {
    let entries: Vec<&str> = value.split(';').collect();
    let present = entries.iter().any(|e| same_dir(e, dir));
    if add {
        if present {
            return None;
        }
        return Some(if value.is_empty() {
            dir.to_string()
        } else if value.ends_with(';') {
            format!("{value}{dir};")
        } else {
            format!("{value};{dir}")
        });
    }
    if !present {
        return None;
    }
    let kept: Vec<&str> = entries.into_iter().filter(|e| !same_dir(e, dir)).collect();
    Some(kept.join(";"))
}

#[cfg(windows)]
pub fn update(dir: &Path, add: bool) -> io::Result<bool> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let dir = dir.to_string_lossy().into_owned();
    let subkey = wide("Environment");
    let name = wide("Path");
    let mut key: HKEY = null_mut();
    // SAFETY: valid NUL-terminated subkey; `key` receives the handle.
    let rc = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            &mut key,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: a key this function opened, closed once.
            unsafe { RegCloseKey(self.0) };
        }
    }
    let key = Key(key);

    // Read the raw value (unexpanded) and its type.
    let mut kind: REG_VALUE_TYPE = 0;
    let mut size: u32 = 0;
    // SAFETY: size query with a null buffer.
    let rc = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            null(),
            &mut kind,
            null_mut(),
            &mut size,
        )
    };
    let (current, kind) = if rc == ERROR_FILE_NOT_FOUND {
        (String::new(), REG_EXPAND_SZ)
    } else if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    } else {
        let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `bytes` bytes.
        let rc = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                null(),
                &mut kind,
                buf.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        buf.truncate(bytes as usize / 2);
        while buf.last() == Some(&0) {
            buf.pop();
        }
        // Keep REG_SZ when that is what the user has; default to REG_EXPAND_SZ.
        let kind = if kind == REG_SZ {
            REG_SZ
        } else {
            REG_EXPAND_SZ
        };
        (String::from_utf16_lossy(&buf), kind)
    };
    let Some(next) = edit(&current, &dir, add) else {
        return Ok(false);
    };
    let data = wide(&next);
    // SAFETY: `data` is a NUL-terminated UTF-16 string of the given byte size.
    let rc = unsafe {
        RegSetValueExW(
            key.0,
            name.as_ptr(),
            0,
            kind,
            data.as_ptr().cast(),
            (data.len() * 2) as u32,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    // Tell running programs (Explorer, new terminals) that the environment
    // changed.
    let env = wide("Environment");
    let mut result: usize = 0;
    // SAFETY: broadcast with a NUL-terminated string parameter and a timeout.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            env.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5_000,
            &mut result,
        )
    };
    Ok(true)
}

#[cfg(not(windows))]
pub fn update(_dir: &Path, _add: bool) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the user PATH is managed on Windows only",
    ))
}

#[cfg(test)]
mod tests {
    use super::edit;

    #[test]
    fn adds_once_and_keeps_every_other_entry_as_written() {
        let v = r"%USERPROFILE%\.cargo\bin;C:\Tools";
        let d = r"C:\Users\a\AppData\Local\WrongIndex";
        let added = edit(v, d, true).unwrap();
        assert_eq!(added, format!(r"%USERPROFILE%\.cargo\bin;C:\Tools;{d}"));
        assert_eq!(edit(&added, d, true), None);
        // Same directory spelled differently counts as present.
        assert_eq!(
            edit(&added, r"c:\users\A\appdata\local\wrongindex\", true),
            None
        );
        assert_eq!(edit("", d, true).unwrap(), d);
        assert_eq!(edit("C:\\X;", d, true).unwrap(), format!("C:\\X;{d};"));
    }

    #[test]
    fn adding_then_removing_gives_the_value_back_byte_for_byte() {
        let d = r"C:\Users\a\AppData\Local\WrongIndex";
        for v in [
            // A new Windows account (M05 in Windows Sandbox).
            r"%USERPROFILE%\AppData\Local\Microsoft\WindowsApps;",
            r"%USERPROFILE%\AppData\Local\Microsoft\WindowsApps",
            r"C:\A;;C:\B",
            r"C:\A;;",
            ";",
            "",
        ] {
            let added = edit(v, d, true).unwrap();
            assert_eq!(
                edit(&added, d, false).as_deref(),
                Some(v),
                "{v:?} via {added:?}"
            );
        }
    }

    #[test]
    fn removes_every_spelling_and_nothing_else() {
        let d = r"C:\Apps\WrongIndex";
        let v = r#"C:\A;"C:\Apps\WrongIndex\";%LOCALAPPDATA%\B;c:/apps/wrongindex"#;
        assert_eq!(edit(v, d, false).unwrap(), r"C:\A;%LOCALAPPDATA%\B");
        assert_eq!(edit(r"C:\A;C:\B", d, false), None);
        assert_eq!(edit(d, d, false).unwrap(), "");
        // Empty entries are someone else's: kept.
        assert_eq!(
            edit(r"C:\A;;C:\Apps\WrongIndex;C:\B;", d, false).unwrap(),
            r"C:\A;;C:\B;"
        );
    }
}
