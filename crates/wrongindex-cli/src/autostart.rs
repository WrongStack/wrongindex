//! Start the service at sign-in on Linux and macOS (`wrongindex daemon
//! autostart on|off|status`), as the desktop window's `Run\WrongIndex`
//! entry does on Windows.
//!
//! - Linux: an XDG autostart entry, `$XDG_CONFIG_HOME/autostart/wrongindex.desktop`
//!   (graphical sessions; a headless machine uses a systemd user unit instead).
//! - macOS: a LaunchAgent, `~/Library/LaunchAgents/dev.wrongstack.wrongindex.plist`,
//!   loaded at the next sign-in.
//!
//! Both run `wrongindex daemon start` from this copy of the CLI, which starts
//! the service detached and returns. Nothing else is written, and nothing is
//! loaded or started now: `daemon start` does that.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The macOS LaunchAgent label: the desktop's bundle identifier.
pub const LABEL: &str = "dev.wrongstack.wrongindex";

/// The command an entry runs: this CLI, `daemon start`, and the data root
/// when one was named.
pub fn command(cli: &Path, data_root: Option<&Path>) -> Vec<String> {
    let mut v = vec![cli.display().to_string()];
    if let Some(root) = data_root {
        v.push("--data-root".into());
        v.push(root.display().to_string());
    }
    v.extend(["daemon".into(), "start".into()]);
    v
}

/// One argument of a desktop entry's `Exec` key, quoted as the Desktop Entry
/// Specification reads it back: plain when it holds no reserved character,
/// else in double quotes with `"`, `` ` ``, `$` and `\` escaped; `%` is
/// doubled everywhere (a field code otherwise).
pub fn exec_arg(arg: &str) -> String {
    let arg = arg.replace('%', "%%");
    let reserved = |c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\''
                    | '\\'
                    | '>'
                    | '<'
                    | '~'
                    | '|'
                    | '&'
                    | ';'
                    | '$'
                    | '*'
                    | '?'
                    | '#'
                    | '('
                    | ')'
                    | '`'
            )
    };
    if !arg.is_empty() && !arg.chars().any(reserved) {
        return arg;
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The XDG autostart entry for `command`.
pub fn desktop_entry(command: &[String]) -> String {
    let exec: Vec<String> = command.iter().map(|a| exec_arg(a)).collect();
    format!(
        "[Desktop Entry]\nType=Application\nName=WrongIndex\nComment=Starts the WrongIndex service at sign-in\nExec={}\nTerminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
        exec.join(" ")
    )
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The LaunchAgent for `command`. `AbandonProcessGroup`: the service runs on
/// after `daemon start` returns (it is in a session of its own anyway).
pub fn launch_agent(command: &[String]) -> String {
    let args: String = command
        .iter()
        .map(|a| format!("\n        <string>{}</string>", xml(a)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>{args}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>AbandonProcessGroup</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
</dict>
</plist>
"#
    )
}

/// Where this platform keeps the entry, under the user's home `home` (and
/// `xdg_config`, `$XDG_CONFIG_HOME` when set and absolute, on Linux).
pub fn entry_path(home: &Path, xdg_config: Option<&Path>) -> io::Result<PathBuf> {
    if cfg!(target_os = "macos") {
        return Ok(home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist")));
    }
    if cfg!(unix) {
        let config = xdg_config
            .filter(|p| p.is_absolute())
            .map_or_else(|| home.join(".config"), Path::to_path_buf);
        return Ok(config.join("autostart").join("wrongindex.desktop"));
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "on Windows the desktop window's Settings start the service at sign-in",
    ))
}

/// The entry's contents for this platform.
pub fn contents(command: &[String]) -> String {
    if cfg!(target_os = "macos") {
        launch_agent(command)
    } else {
        desktop_entry(command)
    }
}

/// The entry for this user: `$HOME`, and `$XDG_CONFIG_HOME` on Linux.
pub fn user_entry_path() -> io::Result<PathBuf> {
    if !cfg!(unix) {
        // Says where Windows keeps it, before asking for a HOME it lacks.
        return entry_path(Path::new(""), None);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    entry_path(&home, xdg.as_deref())
}

/// Whether the entry at `path` is there, and whether it runs `command`.
pub fn status(path: &Path, command: &[String]) -> Value {
    let current = std::fs::read_to_string(path).ok();
    json!({
        "enabled": current.is_some(),
        "matchesThisInstall": current.as_deref() == Some(contents(command).as_str()),
        "path": path.display().to_string(),
        "command": command,
    })
}

/// Write (atomically: a temporary file renamed over it) or remove the entry.
pub fn set(path: &Path, command: &[String], enabled: bool) -> io::Result<Value> {
    if enabled {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("the entry has no directory"))?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(
            ".{}.tmp-{}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        std::fs::write(&tmp, contents(command))?;
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    } else {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(status(path, command))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_arguments_are_quoted_as_the_spec_reads_them() {
        assert_eq!(exec_arg("/usr/bin/wrongindex"), "/usr/bin/wrongindex");
        assert_eq!(exec_arg("/opt/Wrong Index/wi"), "\"/opt/Wrong Index/wi\"");
        assert_eq!(exec_arg("a$b"), "\"a\\$b\"");
        assert_eq!(exec_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(exec_arg("50%"), "50%%");
        assert_eq!(exec_arg(""), "\"\"");
    }

    #[test]
    fn the_entry_runs_this_cli_with_the_named_root() {
        let cmd = command(
            Path::new("/opt/wi/wrongindex"),
            Some(Path::new("/srv/wi root")),
        );
        assert_eq!(
            cmd,
            [
                "/opt/wi/wrongindex",
                "--data-root",
                "/srv/wi root",
                "daemon",
                "start"
            ]
        );
        let desktop = desktop_entry(&cmd);
        assert!(
            desktop
                .contains("\nExec=/opt/wi/wrongindex --data-root \"/srv/wi root\" daemon start\n"),
            "{desktop}"
        );
        let plist = launch_agent(&command(Path::new("/a&b/wrongindex"), None));
        assert!(
            plist.contains("<string>/a&amp;b/wrongindex</string>"),
            "{plist}"
        );
        assert!(plist.contains("<string>daemon</string>\n        <string>start</string>"));
        assert!(plist.contains(&format!("<string>{LABEL}</string>")));
    }

    #[cfg(unix)]
    #[test]
    fn on_off_and_status_touch_only_the_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let path = entry_path(home, None).unwrap();
        if cfg!(target_os = "macos") {
            assert!(path.ends_with("Library/LaunchAgents/dev.wrongstack.wrongindex.plist"));
        } else {
            assert_eq!(path, home.join(".config/autostart/wrongindex.desktop"));
            // `$XDG_CONFIG_HOME` is used when absolute, ignored otherwise.
            let xdg = home.join("xdg");
            assert_eq!(
                entry_path(home, Some(&xdg)).unwrap(),
                xdg.join("autostart/wrongindex.desktop")
            );
            assert_eq!(entry_path(home, Some(Path::new("rel"))).unwrap(), path);
        }
        let cmd = command(Path::new("/opt/wi/wrongindex"), None);
        assert_eq!(status(&path, &cmd)["enabled"], false);
        let on = set(&path, &cmd, true).unwrap();
        assert_eq!(on["enabled"], true);
        assert_eq!(on["matchesThisInstall"], true);
        // Another copy's entry is reported as not this install's.
        let other = command(Path::new("/elsewhere/wrongindex"), None);
        assert_eq!(status(&path, &other)["matchesThisInstall"], false);
        // Nothing but the entry was written.
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [path.file_name().unwrap().to_owned()]);
        let off = set(&path, &cmd, false).unwrap();
        assert_eq!(off["enabled"], false);
        assert!(!path.exists());
        // Off when already off is fine.
        assert_eq!(set(&path, &cmd, false).unwrap()["enabled"], false);
    }
}
