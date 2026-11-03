//! `wrongindex self-update` against a local release server (the GitHub
//! releases API shape, served over plain HTTP on 127.0.0.1 through
//! `WRONGINDEX_UPDATE_URL`): it finds the newest release, downloads and
//! verifies the installer, refuses a checksum mismatch and a missing release
//! without writing anything, and does not try to update a copy that was not
//! installed.

#![cfg(windows)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

type Routes = Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>>;

/// A tiny HTTP/1.1 server: one request per connection, `Connection: close`.
fn serve() -> (String, Routes) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes: Routes = Arc::new(Mutex::new(HashMap::new()));
    let r = routes.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                    break;
                }
            }
            let (status, body) = r
                .lock()
                .unwrap()
                .get(&path)
                .cloned()
                .unwrap_or((404, b"{\"message\":\"Not Found\"}".to_vec()));
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (base, routes)
}

fn publish(base: &str, routes: &Routes, tag: &str, installer: &[u8], sums_hash: Option<&str>) {
    let version = tag.trim_start_matches('v');
    let name = format!("WrongIndex_{version}_x64-setup.exe");
    let hash = sums_hash
        .map(str::to_string)
        .unwrap_or_else(|| hex::encode(Sha256::digest(installer)));
    let release = json!({
        "tag_name": tag,
        "draft": false,
        "html_url": format!("{base}/release/{tag}"),
        "assets": [
            {"name": "SHA256SUMS.txt", "browser_download_url": format!("{base}/dl/{tag}/SHA256SUMS.txt")},
            {"name": name, "browser_download_url": format!("{base}/dl/{tag}/{name}")},
        ],
    });
    let mut r = routes.lock().unwrap();
    r.insert(
        "/releases/latest".into(),
        (200, release.to_string().into_bytes()),
    );
    r.insert(
        format!("/releases/tags/{tag}"),
        (200, release.to_string().into_bytes()),
    );
    r.insert(
        format!("/dl/{tag}/SHA256SUMS.txt"),
        (200, format!("{hash}  {name}\n").into_bytes()),
    );
    r.insert(format!("/dl/{tag}/{name}"), (200, installer.to_vec()));
}

fn cli(base: &str, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wrongindex"))
        .arg("--data-root")
        .arg(home)
        .args(args)
        .env("WRONGINDEX_UPDATE_URL", format!("{base}/releases"))
        .output()
        .unwrap()
}

fn stdout_json(o: &Output) -> Value {
    serde_json::from_slice(&o.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    })
}

#[test]
fn finds_downloads_and_verifies_the_newest_release() {
    let (base, routes) = serve();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let installer = b"MZ fake installer bytes".repeat(1000);
    publish(&base, &routes, "v99.0.0", &installer, None);

    let o = cli(&base, &home, &["--json", "self-update", "--check"]);
    assert!(o.status.success(), "{o:?}");
    let v = stdout_json(&o);
    assert_eq!(v["latest"], "99.0.0");
    assert_eq!(v["current"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["updateAvailable"], true);

    let o = cli(&base, &home, &["--json", "self-update", "--download-only"]);
    assert!(o.status.success(), "{o:?}");
    let v = stdout_json(&o);
    assert_eq!(v["verified"], true);
    let path = Path::new(v["downloaded"].as_str().unwrap()).to_path_buf();
    assert!(
        path.starts_with(home.join("updates").join("99.0.0")),
        "{}",
        path.display()
    );
    assert_eq!(std::fs::read(&path).unwrap(), installer);
}

#[test]
fn a_checksum_mismatch_or_a_missing_release_installs_nothing() {
    let (base, routes) = serve();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    publish(&base, &routes, "v99.0.0", b"real", Some(&"0".repeat(64)));

    let o = cli(&base, &home, &["self-update", "--download-only"]);
    assert_eq!(o.status.code(), Some(5), "{o:?}");
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("checksum mismatch"),
        "{o:?}"
    );
    assert!(!home.join("updates").join("99.0.0").exists());

    let o = cli(
        &base,
        &home,
        &["self-update", "--version", "v1.2.3", "--download-only"],
    );
    assert_eq!(o.status.code(), Some(5), "{o:?}");
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("no release v1.2.3"),
        "{o:?}"
    );
}

#[test]
fn a_copy_that_was_not_installed_is_not_updated() {
    let (base, routes) = serve();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    publish(&base, &routes, "v99.0.0", b"bytes", None);
    let o = cli(&base, &home, &["self-update"]);
    assert_eq!(o.status.code(), Some(2), "{o:?}");
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("not an installed copy"),
        "{o:?}"
    );
    assert!(!home.join("updates").exists(), "downloaded before refusing");
}

#[test]
fn an_older_or_equal_latest_release_is_reported_up_to_date() {
    let (base, routes) = serve();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    publish(
        &base,
        &routes,
        &format!("v{}", env!("CARGO_PKG_VERSION")),
        b"same",
        None,
    );
    let o = cli(&base, &home, &["--json", "self-update"]);
    assert!(o.status.success(), "{o:?}");
    assert_eq!(stdout_json(&o)["updateAvailable"], false);
    assert!(!home.join("updates").exists());
}
