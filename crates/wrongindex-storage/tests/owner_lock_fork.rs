//! A released ownership lock is free at once while another thread forks
//! (WI-M8-07). Its own test binary: the children it forks hold copies of the
//! process's descriptors until they exec, which in the library's test binary
//! made other tests see their files as held by another process.

#![cfg(unix)]

use wrongindex_storage::owner_lock::{AcquireError, OwnerGuard, OwnerInfo, is_held};

fn info(epoch: i64) -> OwnerInfo {
    OwnerInfo {
        owner: "wrongindexd".into(),
        instance_id: format!("d_test_{epoch}"),
        pid: std::process::id(),
        epoch,
        profile_id: "wrongstack-index-v5-baseline".into(),
        acquired_at_ms: 0,
    }
}

/// A child forked by another thread holds a copy of every descriptor until
/// it execs. A released guard (and the probe of `is_held`) must not stay
/// locked through such a copy. Here children are forked without pause and
/// wait 2 ms before they exec.
#[test]
fn a_released_lock_is_free_while_other_threads_fork() {
    use std::os::unix::process::CommandExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let forker = std::thread::spawn(move || {
        while !s.load(Ordering::Relaxed) {
            let mut c = std::process::Command::new("/bin/sh");
            c.args(["-c", ":"]);
            // SAFETY: the closure only sleeps.
            unsafe {
                c.pre_exec(|| {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    Ok(())
                });
            }
            let _ = c.status();
        }
    });
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("idx");
    let mut late = 0;
    for _ in 0..500 {
        // Held here: the last release (or probe) has not let go.
        match OwnerGuard::acquire(&dir, info(1)) {
            Ok(g) => drop(g),
            Err(AcquireError::Held { .. }) => late += 1,
            Err(e) => panic!("{e}"),
        }
        if is_held(&dir) {
            late += 1;
        }
    }
    stop.store(true, Ordering::Relaxed);
    forker.join().unwrap();
    assert_eq!(
        late, 0,
        "the lock stayed held after release in {late} of 1000 checks"
    );
}
