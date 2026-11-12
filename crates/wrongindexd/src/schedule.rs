//! Scheduled work, never in the way of edits.
//!
//! **Health checks** (desktop-and-operations §2.6: "health checks are real
//! jobs with timestamps"; architecture §9 "Corruption"). Every index this
//! service owns gets a `check` job once per `integrityCheckHours` (default
//! 24; 0 turns it off): the profile, SQLite's own check of every page and of
//! the full-text index (see `wrongindex_storage::integrity`), and every
//! indexed file compared with what was indexed (see
//! `wrongindex_storage::contents`; files that differ go to the watcher). A
//! damaged full-text index can still answer queries, wrongly; nothing but a
//! check finds it, and the check marks the project (see `damage`). The check
//! is read-only and takes the project's job slot like any job: about 2 s for
//! a 340 MB index. Checks wait for the first minutes after the service
//! starts (its initial reconciliations go first). An index already known
//! damaged is not checked again on a timer.
//!
//! **Periodic reconciliation** (indexing-pipeline §10). Watch events are not
//! guaranteed: a network drive, an event queue the OS dropped without
//! saying, a tool writing behind the watcher's back. Every watched project
//! gets a whole-project run (`reconcile` job, trigger `periodic`) once per
//! `reconcileMinutes` (default 15; 0 turns it off) after its last one,
//! spread by a fixed per-project jitter of up to a tenth of the period. With
//! nothing changed it is a no-op that publishes nothing (Git-clean files
//! whose stored stamp matches are not even read). It is a job of its own,
//! not a watcher batch: freshness stays as it was, since no change is known
//! until the run finds one - and a run that finds one says so
//! (`reconcile_found_changes`, `missedChanges` in the metrics).
//!
//! Both wait while background indexing is paused and while the project has
//! work pending or running. Checks are admitted one per round. Periodic
//! reconciliations are admitted longest-overdue first, as many per round as
//! it takes for every watched project to get its turn within one period
//! ([`per_round`]: one while the projects fit in the rounds of a period), so
//! many projects are spread out rather than served at once and none is
//! passed over.

use std::time::Duration;

use wrongindex_contracts::{JobKind, JobRecord, JobState};

/// How often the supervisor looks for work that is due.
pub const ROUND: Duration = Duration::from_secs(60);
/// No check runs on a timer this soon after the service starts.
pub const AFTER_START: Duration = Duration::from_secs(10 * 60);

/// A schedule in force (`None` from the constructors when it is off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    pub every_ms: i64,
    pub after_start_ms: i64,
}

impl Schedule {
    /// Health checks every `hours`.
    pub fn checks(hours: u64) -> Option<Self> {
        Self::make(
            hours.saturating_mul(3_600_000),
            AFTER_START,
            "WRONGINDEX_DEBUG_CHECK_EVERY_SECS",
        )
    }

    /// Periodic reconciliation every `minutes`.
    pub fn reconciles(minutes: u64) -> Option<Self> {
        Self::make(
            minutes.saturating_mul(60_000),
            Duration::ZERO,
            "WRONGINDEX_DEBUG_RECONCILE_EVERY_SECS",
        )
    }

    fn make(every_ms: u64, after_start: Duration, hook: &str) -> Option<Self> {
        // Test hook, debug builds only: seconds instead of the setting, no
        // delay after the start.
        #[cfg(debug_assertions)]
        if let Some(secs) = std::env::var(hook)
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
        {
            return (secs > 0).then_some(Self {
                every_ms: secs * 1000,
                after_start_ms: 0,
            });
        }
        #[cfg(not(debug_assertions))]
        let _ = hook;
        (every_ms > 0).then_some(Self {
            every_ms: every_ms.min(i64::MAX as u64) as i64,
            after_start_ms: after_start.as_millis() as i64,
        })
    }

    /// The supervisor's round, shorter under the test hook.
    pub fn round(&self) -> Duration {
        if self.every_ms < ROUND.as_millis() as i64 {
            Duration::from_secs(1)
        } else {
            ROUND
        }
    }

    /// A fixed delay of up to a tenth of the period for `key` (a project),
    /// so projects added together do not come due together. None under the
    /// test hook.
    pub fn jitter_ms(&self, key: &str) -> i64 {
        if self.every_ms < ROUND.as_millis() as i64 {
            return 0;
        }
        // FNV-1a: stable across runs and builds.
        let h = key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        (h % (self.every_ms as u64 / 10).max(1)) as i64
    }
}

/// What the supervisor knows about one project for this decision.
pub struct Candidate<'a> {
    pub owned: bool,
    pub known_damaged: bool,
    /// Work pending, admitted or running for the project.
    pub busy: bool,
    /// The last time it was done (or the attempt in progress).
    pub last: Option<&'a JobRecord>,
    /// Added to the period for this project (see [`Schedule::jitter_ms`]).
    pub jitter_ms: i64,
}

/// Whether the work is due for this project at `now_ms`.
pub fn due(s: Schedule, started_ms: i64, now_ms: i64, c: &Candidate) -> bool {
    if !c.owned || c.known_damaged || c.busy || now_ms - started_ms < s.after_start_ms {
        return false;
    }
    match c.last {
        None => true,
        Some(j) if matches!(j.state, JobState::Queued | JobState::Running) => false,
        Some(j) => now_ms - done_at(j) >= s.every_ms + c.jitter_ms,
    }
}

/// When a job ended (or was admitted, if it never did).
pub fn done_at(j: &JobRecord) -> i64 {
    j.completed_at_ms.unwrap_or(j.admitted_at_ms)
}

/// How many periodic jobs one round admits so that `projects` of them all
/// get their turn within one period: one while there are no more projects
/// than rounds in a period, and proportionally more beyond that.
pub fn per_round(s: Schedule, projects: usize) -> usize {
    let rounds = (s.every_ms / (s.round().as_millis() as i64).max(1)).max(1) as usize;
    projects.div_ceil(rounds).max(1)
}

/// The due projects to admit this round: longest overdue first (never done
/// before any), then by id so the order is stable; at most `limit`.
/// `due` holds each due project with when its work was last done.
pub fn pick_overdue(mut due: Vec<(String, Option<i64>)>, limit: usize) -> Vec<String> {
    due.sort_by(|a, b| (a.1.unwrap_or(i64::MIN), &a.0).cmp(&(b.1.unwrap_or(i64::MIN), &b.0)));
    due.into_iter().take(limit).map(|(id, _)| id).collect()
}

/// The later of two jobs (by [`done_at`]).
pub fn later<'a>(a: Option<&'a JobRecord>, b: Option<&'a JobRecord>) -> Option<&'a JobRecord> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if done_at(y) > done_at(x) { y } else { x }),
        (x, y) => x.or(y),
    }
}

/// The parameters of a scheduled check job.
pub fn check_params() -> serde_json::Value {
    serde_json::json!({"kind": JobKind::Check.as_str(), "paths": [], "scheduled": true})
}

/// The parameters of a periodic reconciliation.
pub fn reconcile_params(fence: &wrongindex_contracts::OwnerFence) -> serde_json::Value {
    serde_json::json!({
        "kind": JobKind::Reconcile.as_str(),
        "paths": [],
        "fence": fence,
        "periodic": true,
    })
}

/// Whether a job is a periodic reconciliation.
pub fn is_periodic(j: &JobRecord) -> bool {
    j.kind == JobKind::Reconcile && j.params.as_ref().is_some_and(|p| p["periodic"] == true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(kind: JobKind, state: JobState, done: Option<i64>) -> JobRecord {
        JobRecord {
            job_id: "j".into(),
            project_id: "p".into(),
            kind,
            state,
            attempt: 1,
            admitted_at_ms: 1_000,
            started_at_ms: None,
            completed_at_ms: done,
            result: None,
            error: None,
            params: None,
        }
    }

    fn check(state: JobState, done: Option<i64>) -> JobRecord {
        job(JobKind::Check, state, done)
    }

    /// With more projects than rounds in a period, admitting the first due
    /// project in registration order served only the first ones for ever.
    #[test]
    fn every_project_gets_its_periodic_turn() {
        let s = Schedule {
            every_ms: 15 * 60_000,
            after_start_ms: 0,
        };
        assert_eq!(per_round(s, 0), 1);
        assert_eq!(per_round(s, 15), 1);
        assert_eq!(per_round(s, 16), 2);
        assert_eq!(per_round(s, 100), 7);

        // 40 projects, all reconciled at their attach; simulate four hours
        // of one-minute rounds.
        let n = 40usize;
        let ids: Vec<String> = (0..n).map(|i| format!("p{i:02}")).collect();
        let mut last: Vec<i64> = vec![0; n];
        let mut runs = vec![0u32; n];
        let mut worst_gap = 0i64;
        for minute in 1..=240i64 {
            let now = minute * 60_000;
            let due: Vec<(String, Option<i64>)> = (0..n)
                .filter(|&i| now - last[i] >= s.every_ms + s.jitter_ms(&ids[i]))
                .map(|i| (ids[i].clone(), Some(last[i])))
                .collect();
            for id in pick_overdue(due, per_round(s, n)) {
                let i = ids.iter().position(|x| *x == id).unwrap();
                worst_gap = worst_gap.max(now - last[i]);
                last[i] = now;
                runs[i] += 1;
            }
        }
        assert!(runs.iter().all(|&r| r >= 10), "{runs:?}");
        // Projects attached together come due together and are then spread
        // over the rounds of one period: nobody waits two periods.
        assert!(worst_gap < 2 * s.every_ms + s.every_ms / 10, "{worst_gap}");

        // The old rule (first due in registration order, one per round)
        // never reaches the later projects.
        let mut last: Vec<i64> = vec![0; n];
        let mut runs = vec![0u32; n];
        for minute in 1..=240i64 {
            let now = minute * 60_000;
            if let Some(i) = (0..n).find(|&i| now - last[i] >= s.every_ms + s.jitter_ms(&ids[i])) {
                last[i] = now;
                runs[i] += 1;
            }
        }
        assert!(runs.contains(&0), "{runs:?}");

        // Longest overdue first, never-done before any, stable by id.
        let picked = pick_overdue(
            vec![
                ("b".into(), Some(50)),
                ("a".into(), Some(50)),
                ("c".into(), None),
                ("d".into(), Some(10)),
            ],
            3,
        );
        assert_eq!(picked, ["c", "d", "a"]);
    }

    #[test]
    fn checks_come_once_per_period_and_never_in_the_way() {
        // Under the test hook the period is seconds; the default is 24 h.
        let s = Schedule {
            every_ms: 24 * 3_600_000,
            after_start_ms: AFTER_START.as_millis() as i64,
        };
        let day = 24 * 3_600_000;
        let start = 0;
        let later = start + AFTER_START.as_millis() as i64;
        let cand = |last| Candidate {
            owned: true,
            known_damaged: false,
            busy: false,
            last,
            jitter_ms: 0,
        };
        // Never checked: due, once the service has run a while.
        assert!(!due(s, start, later - 1, &cand(None)));
        assert!(due(s, start, later, &cand(None)));
        // Checked: due a period after it finished.
        let done = check(JobState::Succeeded, Some(later));
        assert!(!due(s, start, later + day - 1, &cand(Some(&done))));
        assert!(due(s, start, later + day, &cand(Some(&done))));
        // A failed check counts as one (no tight retry loop).
        let failed = check(JobState::Failed, Some(later));
        assert!(!due(s, start, later + 60_000, &cand(Some(&failed))));
        // One admitted or running: never a second.
        let queued = check(JobState::Queued, None);
        assert!(!due(s, start, later + 2 * day, &cand(Some(&queued))));
        // Not owned, already known damaged, or busy: not now.
        for c in [
            Candidate {
                owned: false,
                ..cand(None)
            },
            Candidate {
                known_damaged: true,
                ..cand(None)
            },
            Candidate {
                busy: true,
                ..cand(None)
            },
        ] {
            assert!(!due(s, start, later + 2 * day, &c));
        }
    }

    #[test]
    fn reconciliations_follow_the_last_whole_project_run_with_jitter() {
        let s = Schedule::reconciles(15).unwrap_or(Schedule {
            every_ms: 15 * 60_000,
            after_start_ms: 0,
        });
        if s.every_ms != 15 * 60_000 {
            return; // the debug hook is set in this environment
        }
        assert_eq!(s.after_start_ms, 0);
        let period = 15 * 60_000;
        // Fixed per project, within a tenth of the period, and not the same
        // for every project.
        let jitters: Vec<i64> = (0..20).map(|i| s.jitter_ms(&format!("p_{i}"))).collect();
        assert!(jitters.iter().all(|&j| (0..period / 10).contains(&j)));
        assert_eq!(s.jitter_ms("p_3"), jitters[3]);
        assert!(jitters.iter().any(|&j| j != jitters[0]), "{jitters:?}");

        let jitter = s.jitter_ms("p_3");
        let cand = |last| Candidate {
            owned: true,
            known_damaged: false,
            busy: false,
            last,
            jitter_ms: jitter,
        };
        // The watcher's own full run counts as the last verification.
        let full = job(JobKind::Full, JobState::Succeeded, Some(10_000));
        assert!(!due(s, 0, 10_000 + period + jitter - 1, &cand(Some(&full))));
        assert!(due(s, 0, 10_000 + period + jitter, &cand(Some(&full))));
        // A failed periodic run is the latest attempt: no retry before the
        // next period.
        let failed = job(JobKind::Reconcile, JobState::Failed, Some(20_000));
        let last = later(Some(&full), Some(&failed));
        assert_eq!(last.unwrap().state, JobState::Failed);
        assert!(!due(s, 0, 10_000 + period + jitter, &cand(last)));
        assert!(due(s, 0, 20_000 + period + jitter, &cand(last)));
        assert_eq!(later(None, Some(&full)).unwrap().kind, JobKind::Full);
        assert_eq!(later(None, None).map(|j| j.kind), None);
    }

    #[test]
    fn zero_turns_them_off() {
        if std::env::var_os("WRONGINDEX_DEBUG_CHECK_EVERY_SECS").is_none() {
            assert_eq!(Schedule::checks(0), None);
            let s = Schedule::checks(24).unwrap();
            assert_eq!(s.every_ms, 86_400_000);
            assert_eq!(s.round(), ROUND);
        }
        if std::env::var_os("WRONGINDEX_DEBUG_RECONCILE_EVERY_SECS").is_none() {
            assert_eq!(Schedule::reconciles(0), None);
            assert_eq!(Schedule::reconciles(15).unwrap().every_ms, 900_000);
        }
    }

    #[test]
    fn periodic_jobs_are_told_apart() {
        let mut j = job(JobKind::Reconcile, JobState::Queued, None);
        assert!(!is_periodic(&j));
        j.params = Some(reconcile_params(&wrongindex_contracts::OwnerFence {
            owner_epoch: 1,
            configuration_revision: 1,
        }));
        assert!(is_periodic(&j));
        assert_eq!(j.params.as_ref().unwrap()["fence"]["ownerEpoch"], 1);
    }
}
