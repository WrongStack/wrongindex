//! Disk full on a real volume (architecture §9 "Disk full": roll back the
//! write, report the failure). Needs a small empty NTFS volume named by
//! `WRONGINDEX_TEST_SMALL_VOLUME` (CI attaches a 64 MB VHD as `W:\`), so it
//! is ignored by default; `disk_pressure.rs` covers the same behaviour with
//! the free space faked.
//!
//! 1. The native child the daemon runs indexes a project into the volume.
//! 2. The volume is filled up (256 KiB left) and the project grows: the next
//!    run fails with `diskFull`, `index.db` is byte-identical, still
//!    write-admissible, and no staging directory is left.
//! 3. With the filler gone, the run succeeds.
//! 4. The daemon refuses a job for that index before running anything,
//!    with the volume's real free space (under 256 MiB) in the message.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongindex_contracts::method;
use wrongindex_ipc::{Client, DataRoot};

const KIB: u64 = 1024;

fn small_volume() -> PathBuf {
    PathBuf::from(
        std::env::var_os("WRONGINDEX_TEST_SMALL_VOLUME")
            .expect("WRONGINDEX_TEST_SMALL_VOLUME names a small empty volume"),
    )
}

fn free_bytes(path: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    // SAFETY: NUL-terminated name; one valid out pointer, the others null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            name.as_ptr(),
            &mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_ne!(ok, 0, "GetDiskFreeSpaceExW({})", path.display());
    free
}

/// JSON files the native writer indexes (every key is a symbol).
fn add_files(root: &Path, from: usize, count: usize) {
    let dir = root.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    for i in from..from + count {
        let keys: Vec<String> = (0..20)
            .map(|k| {
                format!("\"setting_{i}_{k}_with_a_longer_name\": \"value number {k} of file {i}\"")
            })
            .collect();
        std::fs::write(
            dir.join(format!("f{i}.json")),
            format!("{{{}}}", keys.join(",")),
        )
        .unwrap();
    }
}

fn native_run(root: &Path, index: &Path) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
        .arg("--native-rebuild")
        .arg(root)
        .arg(index)
        .env_remove("WRONGINDEX_NATIVE_FAULT")
        .output()
        .unwrap();
    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "no reply (status {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn staging_dirs(index: &Path) -> Vec<String> {
    std::fs::read_dir(index)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".wrongindex-staging-"))
        .collect()
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a small empty volume in WRONGINDEX_TEST_SMALL_VOLUME (CI attaches a 64 MB VHD)"]
async fn a_write_that_runs_out_of_space_rolls_back_and_says_so() {
    let volume = small_volume();
    let work = volume.join(format!("wi-disk-full-{}", std::process::id()));
    let index = work.join("index");
    std::fs::create_dir_all(&index).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("project");
    add_files(&root, 0, 20);

    // 1. With room.
    let r = native_run(&root, &index);
    assert_eq!(r["ok"], true, "{r}");
    let db = index.join("index.db");
    let before = std::fs::read(&db).unwrap();

    // 2. The volume full, the project bigger.
    add_files(&root, 20, 600);
    let filler = work.join("filler.bin");
    let free = free_bytes(&volume);
    std::fs::File::create(&filler)
        .unwrap()
        .set_len(free.saturating_sub(256 * KIB))
        .unwrap();
    let left = free_bytes(&volume);
    assert!(left < 1024 * KIB, "{left} bytes still free");
    let r = native_run(&root, &index);
    assert_eq!(r["ok"], false, "{r}");
    assert_eq!(r["kind"], "diskFull", "{r}");
    assert_eq!(std::fs::read(&db).unwrap(), before, "index.db changed");
    assert!(
        wrongindex_storage::inspect_index(&db).write_admissible,
        "index no longer admissible"
    );
    assert_eq!(staging_dirs(&index), Vec::<String>::new());

    // 3. Room again.
    std::fs::remove_file(&filler).unwrap();
    let r = native_run(&root, &index);
    assert_eq!(r["ok"], true, "{r}");
    assert_ne!(std::fs::read(&db).unwrap(), before);

    // 4. The daemon's check, with the real free space.
    let home = tmp.path().join("home");
    let daemon = Daemon(
        Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
            .arg("--data-root")
            .arg(&home)
            .env_remove("WRONGINDEX_ENGINE")
            .env_remove("WRONGINDEX_FAULT_FREE_BYTES_FILE")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut c = loop {
        if let Ok(c) = Client::connect(&DataRoot::resolve(Some(&home)).unwrap(), "t").await {
            break c;
        }
        assert!(Instant::now() < deadline, "daemon not ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let reg = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": root, "indexDir": index, "watchPolicy": "manual"}),
            None,
        )
        .await
        .unwrap();
    let pid = reg["project"]["projectId"].as_str().unwrap().to_string();
    c.call_raw(method::OWNERSHIP_ACQUIRE, Some(&pid), json!({}), None)
        .await
        .unwrap();
    let after_claim = std::fs::read(&db).unwrap();
    let admitted = c
        .call_raw(
            method::INDEX_REQUEST,
            Some(&pid),
            json!({"kind": "full"}),
            None,
        )
        .await
        .unwrap();
    let id = admitted["job"]["jobId"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(60);
    let job = loop {
        let job = c
            .call_raw(method::JOB_GET, None, json!({"jobId": id}), None)
            .await
            .unwrap();
        if job["state"] == "failed" || job["state"] == "succeeded" {
            break job;
        }
        assert!(Instant::now() < deadline, "{job}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(job["state"], "failed", "{job}");
    assert_eq!(job["error"]["code"], "DISK_FULL", "{job}");
    let message = job["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("free on {}", volume.display())),
        "{message}"
    );
    assert_eq!(std::fs::read(&db).unwrap(), after_claim, "the job wrote");
    drop(c);
    drop(daemon);
    let _ = std::fs::remove_dir_all(&work);
}
