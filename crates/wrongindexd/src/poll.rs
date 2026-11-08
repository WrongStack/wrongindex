//! Polling, for a checkout whose file system reports no changes
//! (indexing-pipeline §2 and §9: polling as the fallback, every 5 s, the
//! mode visible).
//!
//! Windows does not report changes everywhere. A WSL share
//! (`\\wsl.localhost\...`), WebDAV and many user-mode file systems refuse
//! `ReadDirectoryChangesW` (`ERROR_INVALID_FUNCTION`), and the `notify`
//! crate then sets its watch up without a word and never reports anything:
//! such a project looked current while every edit was missed (found on a
//! WSL share, 2026-11-04). [`refused`] asks the file system before a
//! watcher is trusted. Some file systems (SMB servers among them) accept
//! the request and then never answer it; that shows only later, when a
//! periodic reconciliation finds changes while the watcher has reported
//! nothing at all since it was attached, and the project is polled from then
//! on (`watch::Supervisor::note_missed`). Where it says no, a [`Poller`] walks the checkout
//! every `pollIntervalSeconds` (default 5; 0 = no polling, and the project
//! is reported `failed`) and compares each indexable file's size and
//! modification time with the pass before. What differs goes to the
//! supervisor as if its event had come: the same filters, debounce and
//! targeted runs. The walk skips what the watcher ignores anyway (the
//! always-ignored directories, the root `.gitignore`, the index directory)
//! and never follows links.
//!
//! The first pass only records what is there; the attach reconciliation
//! runs after it, so an edit made in between is seen by the next pass. What
//! polling cannot see: a rewrite that keeps both size and modification time
//! (the scheduled check compares contents), and edits within one interval
//! arrive together. A pass that takes long spaces the next one out (at least
//! twice its own duration), and a project whose passes stop coming is not
//! reported current ([`PollingStatus::overdue`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::mpsc;
use wrongindex_contracts::PollingStatus;
use wrongindex_discovery::{DEFAULT_IGNORE_DIRS, DEFAULT_IGNORE_FILES, Gitignore, lang};

use crate::watch::WatchMsg;

/// Slack before a missing pass makes the project overdue.
const OVERDUE_SLACK: Duration = Duration::from_secs(10);
/// How long the first pass may take before the project counts as overdue.
const FIRST_PASS: Duration = Duration::from_secs(120);
/// The poller looks at its stop flag this often while it waits.
const STEP: Duration = Duration::from_millis(100);

/// Why the file system's change notifications cannot be used for `dir`, or
/// `None` when they can (or when it cannot be told: the watcher then
/// reports the problem itself).
pub fn refused(dir: &Path) -> Option<String> {
    // Test hook, debug builds only: as if the file system refused.
    #[cfg(debug_assertions)]
    if std::env::var_os("WRONGINDEX_DEBUG_FORCE_POLLING").is_some() {
        return Some("change notifications turned off by the test hook".into());
    }
    native_refusal(dir).map(|code| {
        format!(
            "the file system does not report changes (ReadDirectoryChangesW: Windows error {code})"
        )
    })
}

/// Ask for change notifications the way the watcher will (an overlapped
/// `ReadDirectoryChangesW` over the subtree, `notify`'s buffer size and
/// filter), then cancel the request: the error code when it is refused.
#[cfg(windows)]
fn native_refusal(dir: &Path) -> Option<u32> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, GetLastError,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
        FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY,
        FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING, ReadDirectoryChangesW,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    // notify's ReadDirectoryChangesWatcher uses 16 KiB (a network
    // redirector refuses more than 64 KiB).
    const BUF: u32 = 16_384;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: plain Win32 calls on handles this function owns and closes;
    // the buffer and OVERLAPPED outlive the request, which is cancelled and
    // waited for (GetOverlappedResult with bWait) before they are dropped.
    unsafe {
        let h = CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        );
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let ev = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
        if ev.is_null() {
            CloseHandle(h);
            return None;
        }
        let mut ov: OVERLAPPED = std::mem::zeroed();
        ov.hEvent = ev;
        let mut buf = vec![0u32; (BUF / 4) as usize];
        let ok = ReadDirectoryChangesW(
            h,
            buf.as_mut_ptr().cast(),
            BUF,
            1,
            FILE_NOTIFY_CHANGE_FILE_NAME
                | FILE_NOTIFY_CHANGE_DIR_NAME
                | FILE_NOTIFY_CHANGE_ATTRIBUTES
                | FILE_NOTIFY_CHANGE_SIZE
                | FILE_NOTIFY_CHANGE_LAST_WRITE
                | FILE_NOTIFY_CHANGE_CREATION
                | FILE_NOTIFY_CHANGE_SECURITY,
            std::ptr::null_mut(),
            &mut ov,
            None,
        );
        let refusal = if ok == 0 {
            Some(GetLastError())
        } else {
            // Queued (or already answered): take it back and wait until the
            // system is done with the buffer.
            if WaitForSingleObject(ev, 0) != WAIT_OBJECT_0 {
                CancelIoEx(h, &ov);
            }
            let mut n = 0u32;
            if GetOverlappedResult(h, &ov, &mut n, 1) == 0 {
                let e = GetLastError();
                (e != ERROR_OPERATION_ABORTED && e != ERROR_NOTIFY_ENUM_DIR).then_some(e)
            } else {
                None
            }
        };
        drop(buf);
        CloseHandle(ev);
        CloseHandle(h);
        refusal
    }
}

#[cfg(not(windows))]
fn native_refusal(_dir: &Path) -> Option<u32> {
    None // inotify and FSEvents report their failures from `watch` itself
}

/// What one pass knows about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Seen {
    len: u64,
    modified: Option<SystemTime>,
}

/// One pass over the checkout.
#[derive(Debug, Default)]
struct Pass {
    files: HashMap<PathBuf, Seen>,
    /// Directories that could not be read: what was under them is kept from
    /// the pass before, neither changed nor gone.
    unreadable: Vec<PathBuf>,
}

fn key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    if cfg!(windows) { s.to_lowercase() } else { s }
}

/// Walk `root` as the watcher would see it.
fn scan(root: &Path, index_dir: &Path) -> Pass {
    let gi = Gitignore::load(root);
    let skip = key(index_dir);
    let mut pass = Pass::default();
    walk(root, root, &gi, &skip, &mut pass);
    pass
}

fn walk(root: &Path, dir: &Path, gi: &Gitignore, index_dir: &str, pass: &mut Pass) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        pass.unreadable.push(dir.to_path_buf());
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            pass.unreadable.push(dir.to_path_buf());
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if DEFAULT_IGNORE_DIRS.contains(&name.as_str()) {
            continue;
        }
        let full = dir.join(&name);
        let rel = full
            .strip_prefix(root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        // Links (and junctions) are not followed, as discovery does not.
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if gi.is_ignored(&rel, true) || key(&full) == index_dir {
                continue;
            }
            walk(root, &full, gi, index_dir, pass);
        } else if ft.is_file() {
            let wanted = rel == ".gitignore"
                || (!DEFAULT_IGNORE_FILES.contains(&name.as_str())
                    && !gi.is_ignored(&rel, false)
                    && lang::is_indexable(&full.to_string_lossy()));
            if !wanted {
                continue;
            }
            let Ok(md) = entry.metadata() else {
                pass.unreadable.push(full);
                continue;
            };
            pass.files.insert(
                full,
                Seen {
                    len: md.len(),
                    modified: md.modified().ok(),
                },
            );
        }
    }
}

/// The paths that differ between `before` and `now` (new, changed or gone).
/// What sits under a path `now` could not read is carried over from
/// `before` instead.
fn changes(before: &Pass, now: &mut Pass) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = now
        .files
        .iter()
        .filter(|(p, s)| before.files.get(*p) != Some(*s))
        .map(|(p, _)| p.clone())
        .collect();
    for (p, s) in &before.files {
        if now.files.contains_key(p) {
            continue;
        }
        if now.unreadable.iter().any(|d| p.starts_with(d)) {
            now.files.insert(p.clone(), *s);
        } else {
            out.push(p.clone());
        }
    }
    out.sort();
    out
}

fn now_ms() -> i64 {
    wrongindex_registry::now_ms()
}

/// What the supervisor shows about a poller.
#[derive(Default)]
struct State {
    last_pass_ms: AtomicI64,
    /// Epoch ms after which a missing pass makes the project overdue.
    due_by_ms: AtomicI64,
    pass_ms: AtomicU64,
    files: AtomicU64,
}

/// A running poller; dropping it stops the walk (within a moment, or after
/// the pass under way).
pub struct Poller {
    stop: Arc<AtomicBool>,
    state: Arc<State>,
    every_ms: Arc<AtomicU64>,
    reason: String,
}

impl Drop for Poller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Poller {
    /// Start polling `root` for `project_id`. `every_ms` is the service
    /// setting, read before each wait (a change applies from the next one).
    pub fn start(
        project_id: &str,
        root: &Path,
        index_dir: &Path,
        every_ms: Arc<AtomicU64>,
        reason: String,
        tx: mpsc::Sender<WatchMsg>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(State::default());
        state
            .due_by_ms
            .store(now_ms() + FIRST_PASS.as_millis() as i64, Ordering::SeqCst);
        let (pid, root, index_dir) = (
            project_id.to_string(),
            root.to_path_buf(),
            index_dir.to_path_buf(),
        );
        let (stop2, state2, every2) = (stop.clone(), state.clone(), every_ms.clone());
        std::thread::Builder::new()
            .name("wi-poll".into())
            .spawn(move || run(&pid, &root, &index_dir, &every2, &stop2, &state2, &tx))
            .map_err(|e| format!("cannot start polling: {e}"))?;
        Ok(Self {
            stop,
            state,
            every_ms,
            reason,
        })
    }

    pub fn status(&self) -> PollingStatus {
        let last = self.state.last_pass_ms.load(Ordering::SeqCst);
        PollingStatus {
            reason: self.reason.clone(),
            every_ms: self.every_ms.load(Ordering::SeqCst),
            last_pass_ms: (last > 0).then_some(last),
            pass_ms: self.state.pass_ms.load(Ordering::SeqCst),
            files: self.state.files.load(Ordering::SeqCst),
            overdue: now_ms() > self.state.due_by_ms.load(Ordering::SeqCst),
        }
    }
}

fn run(
    project_id: &str,
    root: &Path,
    index_dir: &Path,
    every_ms: &AtomicU64,
    stop: &AtomicBool,
    state: &State,
    tx: &mpsc::Sender<WatchMsg>,
) {
    let mut before: Option<Pass> = None;
    while !stop.load(Ordering::SeqCst) {
        let t0 = Instant::now();
        let mut pass = scan(root, index_dir);
        let took = t0.elapsed();
        let msg = match &before {
            // The first pass is the baseline: now the attach reconciliation.
            None => Some(WatchMsg::Armed {
                project_id: project_id.to_string(),
            }),
            Some(b) => {
                let paths = changes(b, &mut pass);
                (!paths.is_empty()).then(|| WatchMsg::Event {
                    project_id: project_id.to_string(),
                    paths,
                    rescan: false,
                })
            }
        };
        let wait = Duration::from_millis(every_ms.load(Ordering::SeqCst).max(1_000)).max(took * 2);
        let end = now_ms();
        state.last_pass_ms.store(end, Ordering::SeqCst);
        state
            .pass_ms
            .store(took.as_millis() as u64, Ordering::SeqCst);
        state.files.store(pass.files.len() as u64, Ordering::SeqCst);
        state.due_by_ms.store(
            end + (2 * (wait + took) + OVERDUE_SLACK).as_millis() as i64,
            Ordering::SeqCst,
        );
        // A poller that was stopped during its pass reports nothing: the
        // project may already be watched anew (another poller's baseline).
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if let Some(m) = msg
            && !send(tx, m, stop)
        {
            return;
        }
        before = Some(pass);
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(STEP.min(until.saturating_duration_since(Instant::now())));
        }
    }
}

/// Hand a message to the supervisor, waiting while its queue is full;
/// `false` once the poller is stopped or the supervisor gone.
fn send(tx: &mpsc::Sender<WatchMsg>, mut msg: WatchMsg, stop: &AtomicBool) -> bool {
    loop {
        match tx.try_send(msg) {
            Ok(()) => return true,
            Err(mpsc::error::TrySendError::Closed(_)) => return false,
            Err(mpsc::error::TrySendError::Full(m)) => {
                if stop.load(Ordering::SeqCst) {
                    return false;
                }
                msg = m;
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("wi-poll-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(d.join("src")).unwrap();
        d
    }

    fn names(root: &Path, paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn a_pass_sees_what_the_watcher_would() {
        let root = tmp();
        let idx = root.join("idx");
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        std::fs::create_dir_all(root.join("gen")).unwrap();
        std::fs::create_dir_all(&idx).unwrap();
        std::fs::write(root.join(".gitignore"), "gen/\n*.tmp.ts\n").unwrap();
        std::fs::write(root.join("src/a.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(root.join("src/notes.bin"), [0u8, 1, 2]).unwrap();
        std::fs::write(root.join("src/b.tmp.ts"), "x").unwrap();
        std::fs::write(root.join("node_modules/x/i.ts"), "x").unwrap();
        std::fs::write(root.join("gen/g.ts"), "x").unwrap();
        std::fs::write(idx.join("index.ts"), "x").unwrap();
        std::fs::write(root.join("package-lock.json"), "{}").unwrap();
        let pass = scan(&root, &idx);
        let mut seen = names(&root, &pass.files.keys().cloned().collect::<Vec<_>>());
        seen.sort();
        assert_eq!(seen, [".gitignore", "src/a.ts"]);
        assert!(pass.unreadable.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn changes_are_new_changed_and_gone_files() {
        let root = tmp();
        let idx = root.join("idx");
        for f in ["a", "b", "c"] {
            std::fs::write(root.join(format!("src/{f}.ts")), "export const x = 1;\n").unwrap();
        }
        let before = scan(&root, &idx);
        // Nothing changed: nothing reported.
        let mut same = scan(&root, &idx);
        assert!(changes(&before, &mut same).is_empty());
        // Size, time, a new file, a gone one.
        std::fs::write(root.join("src/a.ts"), "export const x = 12;\n").unwrap();
        let b = std::fs::OpenOptions::new()
            .write(true)
            .open(root.join("src/b.ts"))
            .unwrap();
        b.set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
        drop(b);
        std::fs::write(root.join("src/d.ts"), "export const d = 1;\n").unwrap();
        std::fs::remove_file(root.join("src/c.ts")).unwrap();
        let mut now = scan(&root, &idx);
        assert_eq!(
            names(&root, &changes(&before, &mut now)),
            ["src/a.ts", "src/b.ts", "src/c.ts", "src/d.ts"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_directory_is_neither_changed_nor_gone() {
        let root = tmp();
        let a = root.join("src/a.ts");
        let mut before = Pass::default();
        before.files.insert(
            a.clone(),
            Seen {
                len: 1,
                modified: None,
            },
        );
        let mut now = Pass {
            files: HashMap::new(),
            unreadable: vec![root.join("src")],
        };
        assert!(changes(&before, &mut now).is_empty());
        assert!(now.files.contains_key(&a), "carried over for the next pass");
        // Readable again and the file gone: now it is reported.
        let mut later = Pass::default();
        assert_eq!(changes(&now, &mut later), [a]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(windows)]
    #[test]
    fn a_local_directory_reports_changes() {
        if std::env::var_os("WRONGINDEX_DEBUG_FORCE_POLLING").is_some() {
            return;
        }
        let root = tmp();
        assert_eq!(refused(&root), None);
        // A path that cannot be opened is not a refusal: the watcher says
        // what is wrong.
        assert_eq!(refused(&root.join("missing")), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
