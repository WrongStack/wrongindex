//! Start the service at sign-in on Linux and macOS (`wrongindex daemon
//! autostart on|off|status`), as the desktop window's `Run\WrongIndex`
//! entry does on Windows.
//!
//! - Linux: an XDG autostart entry, `$XDG_CONFIG_HOME/autostart/wrongindex.desktop`
//!   (graphical sessions), or with `--systemd` a systemd user unit (below),
//!   also for a machine without one.
//! - macOS: a LaunchAgent, `~/Library/LaunchAgents/dev.wrongstack.wrongindex.plist`,
//!   loaded at the next sign-in.
//!
//! The entry and the LaunchAgent run `wrongindex daemon start` from this
//! copy of the CLI, which starts the service detached and returns. Nothing else is written, and nothing is
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

/// Linux without a graphical session: a systemd user unit,
/// `$XDG_CONFIG_HOME/systemd/user/wrongindex.service`, enabled for
/// `default.target` (the link `systemctl --user enable` would make). The
/// user manager runs the service itself in the foreground, so the unit
/// names `wrongindexd`, not the CLI: `daemon start` detaches and returns,
/// and systemd would then stop what it left in the unit.
pub const UNIT: &str = "wrongindex.service";

/// The service's exit code when another one already serves the data root
/// (`wrongindexd`'s `EXIT_ALREADY_RUNNING`): not a failure for the unit.
const EXIT_ALREADY_RUNNING: u8 = 3;

/// The unit file and its `default.target.wants` link.
pub fn unit_paths(home: &Path, xdg_config: Option<&Path>) -> (PathBuf, PathBuf) {
    let config = xdg_config
        .filter(|p| p.is_absolute())
        .map_or_else(|| home.join(".config"), Path::to_path_buf);
    let dir = config.join("systemd").join("user");
    (dir.join(UNIT), dir.join("default.target.wants").join(UNIT))
}

/// The unit paths for this user (`$HOME`, `$XDG_CONFIG_HOME`).
pub fn user_unit_paths() -> io::Result<(PathBuf, PathBuf)> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    Ok(unit_paths(&home, xdg.as_deref()))
}

/// One word of a unit's `ExecStart=`, as systemd reads it back: `%`
/// (specifiers) and `$` (variables) doubled; in double quotes, with `\` and
/// `"` escaped, when it holds whitespace, a quote or a backslash.
pub fn unit_arg(arg: &str) -> String {
    let arg = arg.replace('%', "%%").replace('$', "$$");
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\'))
    {
        return arg;
    }
    let mut out = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The command the unit runs: the service next to this CLI, with the data
/// root always named (the user manager does not see this shell's
/// `WRONGINDEX_HOME`).
pub fn unit_command(daemon: &Path, data_root: &Path) -> Vec<String> {
    vec![
        daemon.display().to_string(),
        "--data-root".into(),
        data_root.display().to_string(),
    ]
}

/// The unit. `path`: this shell's `PATH`, which the service needs to find
/// the toolchains (bun, Python, Go) it parses some languages with; the
/// user manager's own is shorter. `KillMode=mixed`: a stop signals the
/// service only, which ends its workers in order; whatever is left is
/// killed after it. A clean stop (`daemon stop`) is not restarted.
pub fn unit_file(command: &[String], path: Option<&str>) -> String {
    let exec: Vec<String> = command.iter().map(|a| unit_arg(a)).collect();
    let env = path
        .filter(|p| !p.is_empty())
        .map(|p| {
            let quoted = p
                .replace('%', "%%")
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            format!("Environment=\"PATH={quoted}\"\n")
        })
        .unwrap_or_default();
    format!(
        "[Unit]\nDescription=WrongIndex codebase indexing service\nDocumentation=https://github.com/WrongStack/wrongindex\n\n[Service]\nType=simple\nExecStart={}\n{env}Restart=on-failure\nSuccessExitStatus={EXIT_ALREADY_RUNNING}\nKillMode=mixed\nTimeoutStopSec=60\n\n[Install]\nWantedBy=default.target\n",
        exec.join(" ")
    )
}

/// Whether the unit is there and enabled (its link names it), and whether
/// it is the one this copy would write.
pub fn unit_status(unit: &Path, wants: &Path, contents: &str, command: &[String]) -> Value {
    let current = std::fs::read_to_string(unit).ok();
    let linked = std::fs::read_link(wants).is_ok_and(|t| t == unit);
    json!({
        "kind": "systemd",
        "enabled": current.is_some() && linked,
        "unitPresent": current.is_some(),
        "matchesThisInstall": current.as_deref() == Some(contents),
        "path": unit.display().to_string(),
        "wants": wants.display().to_string(),
        "command": command,
    })
}

/// Write the unit (atomically) and link it, or remove both. Nothing is
/// loaded or started: the user manager reads it at the next sign-in, or
/// after `systemctl --user daemon-reload`.
#[cfg(unix)]
pub fn set_unit(
    unit: &Path,
    wants: &Path,
    contents: &str,
    command: &[String],
    enabled: bool,
) -> io::Result<Value> {
    let remove = |p: &Path| match std::fs::remove_file(p) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    };
    if enabled {
        let dir = unit
            .parent()
            .ok_or_else(|| io::Error::other("the unit has no directory"))?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{UNIT}.tmp-{}", std::process::id()));
        std::fs::write(&tmp, contents)?;
        if let Err(e) = std::fs::rename(&tmp, unit) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        if let Some(d) = wants.parent() {
            std::fs::create_dir_all(d)?;
        }
        remove(wants)?;
        std::os::unix::fs::symlink(unit, wants)?;
    } else {
        remove(wants)?;
        remove(unit)?;
    }
    Ok(unit_status(unit, wants, contents, command))
}

#[cfg(not(unix))]
pub fn set_unit(
    _unit: &Path,
    _wants: &Path,
    _contents: &str,
    _command: &[String],
    _enabled: bool,
) -> io::Result<Value> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "systemd user units are a Linux option",
    ))
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

    #[test]
    fn unit_arguments_are_quoted_as_systemd_reads_them() {
        assert_eq!(unit_arg("/opt/wi/wrongindexd"), "/opt/wi/wrongindexd");
        assert_eq!(unit_arg("/srv/wi root"), "\"/srv/wi root\"");
        assert_eq!(unit_arg("50%"), "50%%");
        assert_eq!(unit_arg("a$b"), "a$$b");
        assert_eq!(unit_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(unit_arg(r"a\b"), r#""a\\b""#);
        assert_eq!(unit_arg(""), "\"\"");
    }

    #[test]
    fn the_unit_runs_the_service_in_the_foreground_with_the_root_and_path() {
        let cmd = unit_command(Path::new("/opt/wi/wrongindexd"), Path::new("/srv/wi root"));
        let unit = unit_file(&cmd, Some("/home/u/.bun/bin:/usr/bin"));
        assert!(
            unit.contains("\nExecStart=/opt/wi/wrongindexd --data-root \"/srv/wi root\"\n"),
            "{unit}"
        );
        assert!(unit.contains("\nType=simple\n"), "{unit}");
        assert!(unit.contains("\nKillMode=mixed\n"), "{unit}");
        assert!(unit.contains("\nRestart=on-failure\n"), "{unit}");
        assert!(unit.contains("\nSuccessExitStatus=3\n"), "{unit}");
        assert!(unit.contains("\nWantedBy=default.target\n"), "{unit}");
        assert!(
            unit.contains("\nEnvironment=\"PATH=/home/u/.bun/bin:/usr/bin\"\n"),
            "{unit}"
        );
        assert!(!unit_file(&cmd, None).contains("Environment="));
        assert!(!unit_file(&cmd, Some("")).contains("Environment="));
    }

    #[cfg(unix)]
    #[test]
    fn unit_paths_follow_xdg_config_home() {
        let home = Path::new("/home/u");
        let (unit, wants) = unit_paths(home, None);
        assert_eq!(unit, home.join(".config/systemd/user/wrongindex.service"));
        assert_eq!(
            wants,
            home.join(".config/systemd/user/default.target.wants/wrongindex.service")
        );
        let (unit, _) = unit_paths(home, Some(Path::new("/x/cfg")));
        assert_eq!(unit, Path::new("/x/cfg/systemd/user/wrongindex.service"));
        assert_eq!(
            unit_paths(home, Some(Path::new("rel"))).0,
            home.join(".config/systemd/user/wrongindex.service")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_unit_is_written_linked_and_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let (unit, wants) = unit_paths(tmp.path(), None);
        let cmd = unit_command(Path::new("/opt/wi/wrongindexd"), Path::new("/srv/wi"));
        let contents = unit_file(&cmd, Some("/usr/bin"));
        assert_eq!(
            unit_status(&unit, &wants, &contents, &cmd)["enabled"],
            false
        );
        let on = set_unit(&unit, &wants, &contents, &cmd, true).unwrap();
        assert_eq!(on["enabled"], true);
        assert_eq!(on["matchesThisInstall"], true);
        assert_eq!(std::fs::read_link(&wants).unwrap(), unit);
        assert_eq!(std::fs::read_to_string(&unit).unwrap(), contents);
        // On again replaces the link and the unit; nothing else is written.
        set_unit(&unit, &wants, &contents, &cmd, true).unwrap();
        let names: Vec<_> = std::fs::read_dir(unit.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(names, ["default.target.wants", "wrongindex.service"]);
        // Another PATH or another copy is not this install's unit.
        let other = unit_file(&cmd, Some("/other"));
        assert_eq!(
            unit_status(&unit, &wants, &other, &cmd)["matchesThisInstall"],
            false
        );
        // A unit without its link is present but not enabled.
        std::fs::remove_file(&wants).unwrap();
        let v = unit_status(&unit, &wants, &contents, &cmd);
        assert_eq!(
            (v["enabled"].clone(), v["unitPresent"].clone()),
            (json!(false), json!(true))
        );
        let off = set_unit(&unit, &wants, &contents, &cmd, false).unwrap();
        assert_eq!(off["enabled"], false);
        assert!(!unit.exists() && std::fs::symlink_metadata(&wants).is_err());
        assert_eq!(
            set_unit(&unit, &wants, &contents, &cmd, false).unwrap()["unitPresent"],
            false
        );
    }
}
