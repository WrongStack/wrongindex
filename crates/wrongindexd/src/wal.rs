//! Write-ahead log care for the indexes this service owns (WI-603 read
//! retention; indexing-pipeline §12). See `wrongindex_storage::wal` for why
//! a WAL outlives a run or grows.
//!
//! * The disk check (every 5 s) looks at each owned index that has no job
//!   running. A WAL with bytes in it is due for a truncating checkpoint.
//! * The worker runs it in the project's job slot, so it never overlaps a
//!   write or a publication of that project, and while the service holds the
//!   ownership guard.
//! * A reader's open transaction makes the checkpoint give up at once; the
//!   reader is never disturbed. It is tried again every [`RETRY`].
//! * A WAL of at least [`LARGE_MIN_BYTES`] and more than twice its index is
//!   reported as `large` (`resource.warning` with `resource: "wal"`, whether
//!   a reader blocks it and since when), and `ok` again once it is cut.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wrongindex_storage::wal::Checkpoint;

use crate::info;

/// A checkpoint a reader blocked is tried again this much later.
pub const RETRY: Duration = Duration::from_secs(15);
/// A WAL is `large` from this size on, and only when over twice its index.
pub const LARGE_MIN_BYTES: u64 = 16 * 1024 * 1024;

pub fn is_large(wal: u64, index: u64) -> bool {
    wal >= LARGE_MIN_BYTES && wal > index.saturating_mul(2)
}

#[derive(Debug, Clone)]
struct State {
    dir: PathBuf,
    wal: u64,
    index: u64,
    /// The last checkpoint was blocked by a reader, since then (epoch ms).
    blocked_since_ms: Option<i64>,
    /// Due for a checkpoint from this instant.
    next_try: Option<Instant>,
    /// A checkpoint is running.
    running: bool,
    /// Reported as large.
    large: bool,
}

/// One owned index as the disk check saw it.
pub struct Seen {
    pub project_id: String,
    pub dir: PathBuf,
    pub wal: u64,
    pub index: u64,
}

#[derive(Default)]
pub struct Care {
    state: Mutex<HashMap<String, State>>,
}

impl Care {
    /// Sizes from the disk check for the owned indexes with no job running
    /// (`others` are owned too but busy: kept as they are). Projects in
    /// neither are forgotten. Returns `(project, payload)` for every change
    /// between `large` and `ok`.
    pub fn observe(&self, seen: &[Seen], others: &[String], now: Instant) -> Vec<(String, Value)> {
        let Ok(mut g) = self.state.lock() else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        for s in seen {
            let st = g.entry(s.project_id.clone()).or_insert_with(|| State {
                dir: s.dir.clone(),
                wal: 0,
                index: 0,
                blocked_since_ms: None,
                next_try: None,
                running: false,
                large: false,
            });
            st.dir = s.dir.clone();
            st.wal = s.wal;
            st.index = s.index;
            if s.wal == 0 {
                st.next_try = None;
                st.blocked_since_ms = None;
            } else if st.next_try.is_none() && !st.running {
                st.next_try = Some(now);
            }
            let large = is_large(s.wal, s.index);
            if large != st.large {
                st.large = large;
                changes.push((s.project_id.clone(), payload(&s.project_id, st)));
            }
        }
        g.retain(|pid, _| seen.iter().any(|s| &s.project_id == pid) || others.contains(pid));
        changes
    }

    /// Projects due for a checkpoint, with their index directories.
    pub fn due(&self, now: Instant) -> Vec<(String, PathBuf)> {
        self.state
            .lock()
            .map(|g| {
                g.iter()
                    .filter(|(_, s)| !s.running && s.next_try.is_some_and(|t| t <= now))
                    .map(|(pid, s)| (pid.clone(), s.dir.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn start(&self, project_id: &str) {
        if let Ok(mut g) = self.state.lock()
            && let Some(s) = g.get_mut(project_id)
        {
            s.running = true;
            s.next_try = None;
        }
    }

    /// Record a checkpoint's outcome; returns a payload when the report
    /// changes: a large WAL was cut (`ok`), or a large WAL is found blocked
    /// by a reader.
    pub fn finish(
        &self,
        project_id: &str,
        outcome: &Result<Checkpoint, String>,
        now: Instant,
    ) -> Option<Value> {
        let Ok(mut g) = self.state.lock() else {
            return None;
        };
        let s = g.get_mut(project_id)?;
        s.running = false;
        match outcome {
            Ok(Checkpoint::Truncated { bytes }) => {
                info!("wal_truncated", {"projectId": project_id, "bytes": bytes});
                s.wal = 0;
                s.blocked_since_ms = None;
                if s.large {
                    s.large = false;
                    return Some(payload(project_id, s));
                }
            }
            Ok(Checkpoint::Empty) => {
                s.wal = 0;
                s.blocked_since_ms = None;
            }
            Ok(Checkpoint::Blocked { bytes }) => {
                s.wal = *bytes;
                s.next_try = Some(now + RETRY);
                if s.blocked_since_ms.is_none() {
                    info!("wal_checkpoint_blocked", {
                        "projectId": project_id, "bytes": bytes,
                        "retrySeconds": RETRY.as_secs(),
                    });
                    s.blocked_since_ms = Some(wrongindex_registry::now_ms());
                    // Already reported large: now it is known why.
                    if s.large {
                        return Some(payload(project_id, s));
                    }
                }
            }
            Err(e) => {
                crate::warn!("wal_checkpoint_failed", {"projectId": project_id, "error": e});
                s.next_try = Some(now + RETRY * 4);
            }
        }
        None
    }

    /// Large WALs, for `events.snapshot`.
    pub fn warnings(&self) -> Vec<Value> {
        let Ok(g) = self.state.lock() else {
            return Vec::new();
        };
        let mut out: Vec<(&String, &State)> = g.iter().filter(|(_, s)| s.large).collect();
        out.sort_by(|a, b| a.0.cmp(b.0));
        out.into_iter().map(|(pid, s)| payload(pid, s)).collect()
    }
}

fn payload(project_id: &str, s: &State) -> Value {
    json!({
        "resource": "wal",
        "projectId": project_id,
        "level": if s.large { "large" } else { "ok" },
        "walBytes": s.wal,
        "indexBytes": s.index,
        "largeBytes": LARGE_MIN_BYTES,
        "blockedByReader": s.blocked_since_ms.is_some(),
        "blockedSinceMs": s.blocked_since_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn seen(wal: u64, index: u64) -> Vec<Seen> {
        vec![Seen {
            project_id: "p".into(),
            dir: PathBuf::from("d"),
            wal,
            index,
        }]
    }

    #[test]
    fn large_means_big_and_over_twice_the_index() {
        assert!(!is_large(10 * MIB, MIB), "under the floor");
        assert!(!is_large(40 * MIB, 20 * MIB), "not over twice");
        assert!(is_large(41 * MIB, 20 * MIB));
        assert!(is_large(16 * MIB, 0));
    }

    #[test]
    fn a_large_wal_found_blocked_is_reported_again() {
        let care = Care::default();
        let t0 = Instant::now();
        let ch = care.observe(&seen(40 * MIB, 10 * MIB), &[], t0);
        assert_eq!(ch[0].1["blockedByReader"], false);
        care.start("p");
        let blocked = Ok(Checkpoint::Blocked { bytes: 40 * MIB });
        let again = care.finish("p", &blocked, t0).unwrap();
        assert_eq!(again["level"], "large");
        assert_eq!(again["blockedByReader"], true);
        assert!(again["blockedSinceMs"].as_i64().unwrap() > 0);
        // Still blocked on the retry: nothing new to say.
        care.start("p");
        assert!(care.finish("p", &blocked, t0 + RETRY).is_none());
    }

    #[test]
    fn a_wal_is_due_until_cut_and_retried_while_blocked() {
        let care = Care::default();
        let t0 = Instant::now();
        assert!(care.observe(&seen(0, 10 * MIB), &[], t0).is_empty());
        assert!(care.due(t0).is_empty());
        // Bytes in the WAL: due now.
        care.observe(&seen(MIB, 10 * MIB), &[], t0);
        assert_eq!(care.due(t0).len(), 1);
        care.start("p");
        assert!(care.due(t0).is_empty(), "running");
        // Blocked: due again after RETRY, not before.
        let blocked = Ok(Checkpoint::Blocked { bytes: MIB });
        assert!(care.finish("p", &blocked, t0).is_none());
        assert!(care.due(t0 + RETRY / 2).is_empty());
        care.observe(&seen(MIB, 10 * MIB), &[], t0 + RETRY / 2);
        assert!(
            care.due(t0 + RETRY / 2).is_empty(),
            "observing does not hurry it"
        );
        assert_eq!(care.due(t0 + RETRY).len(), 1);
        // It grows past twice the index: large, and says a reader blocks it.
        let ch = care.observe(&seen(30 * MIB, 10 * MIB), &[], t0 + RETRY);
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].1["level"], "large");
        assert_eq!(ch[0].1["blockedByReader"], true);
        assert_eq!(care.warnings().len(), 1);
        assert_eq!(care.warnings()[0]["projectId"], "p");
        // Cut: ok again, nothing due.
        care.start("p");
        let ok = care
            .finish(
                "p",
                &Ok(Checkpoint::Truncated { bytes: 30 * MIB }),
                t0 + RETRY,
            )
            .unwrap();
        assert_eq!(ok["level"], "ok");
        assert_eq!(ok["blockedByReader"], false);
        assert!(care.warnings().is_empty());
        assert!(care.observe(&seen(0, 10 * MIB), &[], t0 + RETRY).is_empty());
        assert!(care.due(t0 + RETRY * 2).is_empty());
        // Busy projects are kept, the rest forgotten.
        care.observe(&seen(MIB, 10 * MIB), &[], t0);
        care.observe(&[], &["p".to_string()], t0);
        assert_eq!(care.due(t0).len(), 1);
        care.observe(&[], &[], t0);
        assert!(care.due(t0).is_empty());
    }
}
