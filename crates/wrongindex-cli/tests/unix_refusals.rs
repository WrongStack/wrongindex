//! Off Windows, the commands that exist for the Windows installer refuse
//! with exit 2 before doing anything: `self-update` makes no request (a
//! release carries a Windows installer only) and `setup` changes nothing.

#![cfg(not(windows))]

use std::net::TcpListener;
use std::process::Command;

fn cli(home: &std::path::Path, update_url: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wrongindex"))
        .args(args)
        .env("HOME", home)
        .env("WRONGINDEX_HOME", home.join("service"))
        .env("WRONGINDEX_UPDATE_URL", update_url)
        .output()
        .unwrap()
}

#[test]
fn self_update_refuses_without_a_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/releases", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let mut asked = false;
    let home = tempfile::tempdir().unwrap();
    for args in [
        &["self-update", "--check"][..],
        &["self-update", "--download-only"],
        &["self-update"],
        &["--json", "self-update", "--version", "v0.3.0"],
    ] {
        let out = cli(home.path(), &url, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("replace the unpacked archive"), "{err}");
        asked |= listener.accept().is_ok();
    }
    assert!(!asked, "a request reached the release server");
    assert!(!home.path().join("service").join("updates").exists());
}

#[test]
fn setup_refuses() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("app");
    for sub in ["add-to-path", "remove-from-path", "stop-installed"] {
        let out = cli(
            home.path(),
            "http://127.0.0.1:9/",
            &["setup", sub, dir.to_str().unwrap()],
        );
        assert_eq!(out.status.code(), Some(2), "{sub}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Windows installer"),
            "{sub}: {out:?}"
        );
    }
}
