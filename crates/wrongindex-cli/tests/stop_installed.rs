//! `wrongindex setup stop-installed <dir>` as the installer runs it: every
//! other process running from the install directory is ended and waited
//! for, but not the processes that started it - `wrongindex self-update`
//! waits in the install directory for the installer that runs this.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn system32(name: &str) -> PathBuf {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    Path::new(&root).join("System32").join(name)
}

/// The starter: this test program, copied into the install directory, runs
/// the command and writes what it printed to a file. A normal test run does
/// nothing here.
#[test]
fn starter() {
    let (Ok(app), Ok(out)) = (
        std::env::var("WI_TEST_STARTER_DIR"),
        std::env::var("WI_TEST_STARTER_OUT"),
    ) else {
        return;
    };
    let cli = std::env::var("WI_TEST_STARTER_CLI").unwrap();
    let ran = Command::new(cli)
        .args(["--json", "setup", "stop-installed", &app])
        .output()
        .unwrap();
    assert!(ran.status.success(), "{ran:?}");
    std::fs::write(out, &ran.stdout).unwrap();
}

#[test]
fn ends_what_runs_from_the_directory_but_not_its_starters() {
    let tmp = tempfile::tempdir().unwrap();
    let app = tmp.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    // A starter in the directory, as `self-update` is (`wrongindex.exe.old`).
    let starter = app.join("starter.exe");
    std::fs::copy(std::env::current_exe().unwrap(), &starter).unwrap();
    // And something else running from there, as a service would be.
    let other = app.join("other.exe");
    std::fs::copy(system32("PING.EXE"), &other).unwrap();
    let mut running = Command::new(&other)
        .args(["-n", "120", "127.0.0.1"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let out_file = tmp.path().join("out.json");
    let status = Command::new(&starter)
        .args(["--exact", "starter", "--test-threads=1"])
        .env("WI_TEST_STARTER_DIR", &app)
        .env("WI_TEST_STARTER_OUT", &out_file)
        .env("WI_TEST_STARTER_CLI", env!("CARGO_BIN_EXE_wrongindex"))
        .stdout(Stdio::null())
        .status()
        .unwrap();
    // The starter lived to see the command finish (ended, it exits 1).
    assert!(status.success(), "{status:?}");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&out_file).unwrap()).unwrap();
    let stopped = v["stopped"].as_array().unwrap();
    assert_eq!(stopped.len(), 1, "{v}");
    assert_eq!(stopped[0]["pid"], running.id(), "{v}");
    assert!(
        stopped[0]["program"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .ends_with(r"\app\other.exe"),
        "{v}"
    );
    // Ended and waited for: gone, and its program can be removed.
    assert!(running.try_wait().unwrap().is_some());
    std::fs::remove_file(&other).unwrap();
}
