//! `wrongindex daemon start` returns while the service keeps running, even
//! when the caller reads the CLI's output through a pipe (scripts, the
//! installer, PowerShell pipelines). Regression: the detached service used to
//! inherit the CLI's stdout pipe, so a reader waited until the service exited.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn piped_start_returns_while_the_service_runs() {
    let cli = std::path::Path::new(env!("CARGO_BIN_EXE_wrongindex"));
    let daemon = cli.with_file_name(if cfg!(windows) {
        "wrongindexd.exe"
    } else {
        "wrongindexd"
    });
    if !daemon.is_file() {
        eprintln!("NOT RUN: {} is not built", daemon.display());
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    let mut child = Command::new(cli)
        .arg("--data-root")
        .arg(&root)
        .args(["daemon", "start"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    while !reader.is_finished() {
        assert!(
            Instant::now() < deadline,
            "the CLI's stdout stayed open: the service inherited the pipe"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = reader.join().unwrap();
    assert!(child.wait().unwrap().success(), "{out}");
    assert!(out.contains("started") || out.contains("running"), "{out}");
    let stop = Command::new(cli)
        .arg("--data-root")
        .arg(&root)
        .args(["daemon", "stop"])
        .output()
        .unwrap();
    assert!(stop.status.success(), "{stop:?}");
}
