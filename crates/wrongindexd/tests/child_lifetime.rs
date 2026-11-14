//! Unix: the compatibility worker runs under `wrongindexd --engine-shim`,
//! which must end, with everything it started, the moment the service is
//! gone (INV-01: nothing may go on writing an index after the ownership lock
//! died with the service). On Windows the job object does this.
//!
//! `sh` stands in for the service: it starts the shim, which starts a
//! long-running program, and is then killed.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const EXE: &str = env!("CARGO_BIN_EXE_wrongindexd");

/// Running and not a zombie (`ps` shows a zombie as `Z`).
fn alive(pid: u32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    !stat.trim().is_empty() && !stat.trim().starts_with('Z')
}

fn children(parent: u32) -> Vec<u32> {
    let out = Command::new("pgrep")
        .args(["-P", &parent.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

fn until(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_engine_shim_and_its_program_die_with_the_service() {
    let script =
        format!("WRONGINDEX_SERVICE_PID=$$ '{EXE}' --engine-shim sleep 600 & echo $!; wait");
    let mut service = Command::new("sh")
        .args(["-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(service.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let shim: u32 = line.trim().parse().unwrap();
    let mut program = Vec::new();
    until("the shim never started its program", 10, || {
        program = children(shim);
        !program.is_empty()
    });
    let program = program[0];
    assert!(alive(shim) && alive(program));

    // The service dies without a word (SIGKILL): no stdin EOF, no request.
    service.kill().unwrap();
    service.wait().unwrap();
    let started = Instant::now();
    until("the program outlived the service", 5, || !alive(program));
    until("the shim outlived the service", 5, || !alive(shim));
    eprintln!("both gone {:?} after the service", started.elapsed());
}

/// While the service lives, the shim is invisible: the program reads and
/// writes the shim's standard handles, and its exit code comes back.
#[test]
fn the_engine_shim_passes_handles_and_exit_code_through() {
    let mut shim = Command::new(EXE)
        .args(["--engine-shim", "sh", "-c", "read x; echo got:$x; exit 3"])
        .env("WRONGINDEX_SERVICE_PID", std::process::id().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    shim.stdin.take().unwrap().write_all(b"hi\n").unwrap();
    let out = shim.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "got:hi");
    assert_eq!(out.status.code(), Some(3));
}

/// A shim whose service is already gone does not start its program.
#[test]
fn the_engine_shim_refuses_when_the_service_is_gone() {
    let out = Command::new(EXE)
        .args(["--engine-shim", "sh", "-c", "echo ran"])
        // Not this test process: the shim's parent.
        .env("WRONGINDEX_SERVICE_PID", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(70));
    assert!(out.stdout.is_empty());
}
