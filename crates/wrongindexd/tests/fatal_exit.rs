//! A running service that dies says why in its own log.
//!
//! The service is started detached with stderr discarded. A panic, or an
//! error returned after `ready` (for example from the accept loop), used to
//! end it with nothing in `wrongindexd.log`: no `stopped`, no reason. The
//! one trace, the start-error file, was deleted by the next service's
//! start. Both endings are simulated with the debug-only
//! `WRONGINDEX_DEBUG_EXIT_AFTER_READY` hook.

#![cfg(windows)]

use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use wrongindex_ipc::DataRoot;

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(home: &Path, exit_after_ready: Option<&str>) -> Daemon {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_wrongindexd"));
    cmd.arg("--data-root")
        .arg(home)
        .env_remove("WRONGINDEX_ENGINE")
        .env_remove("WRONGINDEX_DEBUG_EXIT_AFTER_READY")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(how) = exit_after_ready {
        cmd.env("WRONGINDEX_DEBUG_EXIT_AFTER_READY", how);
    }
    Daemon(cmd.spawn().unwrap())
}

fn wait_exit(d: &mut Daemon) -> ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = d.0.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "the service did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn log_lines(root: &DataRoot) -> Vec<Value> {
    std::fs::read_to_string(root.logs_dir().join("wrongindexd.log"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn find(root: &DataRoot, msg: &str) -> Option<Value> {
    log_lines(root).into_iter().find(|l| l["msg"] == msg)
}

fn wait_for(root: &DataRoot, msg: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(line) = find(root, msg) {
            return line;
        }
        assert!(
            Instant::now() < deadline,
            "no `{msg}` line in the log: {:?}",
            log_lines(root)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_panic_that_ends_the_service_is_in_its_log() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let root = DataRoot::resolve(Some(&home)).unwrap();
    let _ = root.take_start_error();

    let mut d = spawn(&home, Some("panic"));
    let status = wait_exit(&mut d);
    assert_eq!(
        status.code(),
        Some(101),
        "a panic on the service's main path"
    );

    let ready = find(&root, "ready").expect("the service was ready first");
    let panic = find(&root, "panic").unwrap_or_else(|| {
        panic!("no `panic` line in the log: {:?}", log_lines(&root));
    });
    assert_eq!(panic["level"], "error");
    assert_eq!(panic["pid"], ready["pid"]);
    assert_eq!(panic["fields"]["message"], "debug exit after ready");
    let location = panic["fields"]["location"].as_str().unwrap();
    assert!(location.contains("server.rs"), "{location}");
}

#[test]
fn a_fatal_error_is_logged_and_kept_for_the_next_start() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let root = DataRoot::resolve(Some(&home)).unwrap();
    let _ = root.take_start_error();

    let mut d = spawn(&home, Some("error"));
    let status = wait_exit(&mut d);
    assert_eq!(status.code(), Some(70));
    let fatal = find(&root, "fatal_exit").unwrap_or_else(|| {
        panic!("no `fatal_exit` line in the log: {:?}", log_lines(&root));
    });
    assert_eq!(fatal["level"], "error");
    assert!(
        fatal["fields"]["error"]
            .as_str()
            .unwrap()
            .contains("debug exit after ready"),
        "{fatal}"
    );
    drop(d);

    // The next service clears the recorded reason, and logs it first.
    let _next = spawn(&home, None);
    let previous = wait_for(&root, "previous_failure");
    assert!(
        previous["fields"]["reason"]
            .as_str()
            .unwrap()
            .contains("debug exit after ready"),
        "{previous}"
    );
    assert_ne!(previous["pid"], fatal["pid"], "logged by the next service");
    assert_eq!(root.take_start_error(), None, "cleared by the next start");
}
