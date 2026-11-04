//! Scheduled health checks (desktop-and-operations §2.6: "health checks are
//! real jobs with timestamps"; architecture §9 "Corruption").
//!
//! Every index this service owns gets a `check` job - the profile and
//! SQLite's own check of every page and of the full-text index (see
//! `wrongindex_storage::integrity`) - once per `integrityCheckHours`
//! (default 24; 0 turns it off). A damaged full-text index can still answer
//! queries, wrongly; nothing but a check finds it, and the check marks the
//! project (see `damage`). The check is read-only and takes the project's job
//! slot like any job: about 2 s for a 340 MB index.
//!
//! Checks wait while background indexing is paused, while the project has
//! work pending or running, and for the first minutes after the service
//! starts (its initial reconciliations go first). At most one is admitted per
//! round, so many projects are spread out rather than checked at once. An
//! index already known damaged is not checked again on a timer.

use std::time::Duration;

use wrongindex_contracts::{JobKind, JobRecord, JobState};

/// How often the supervisor looks for checks that are due.
pub const ROUND: Duration = Duration::from_secs(60);
/// Nothing is checked on a timer this soon after the service starts.
pub const AFTER_START: Duration = Duration::from_secs(10 * 60);

/// The schedule in force: `None` when scheduled checks are off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    pub every_ms: i64,
    pub after_start_ms: i64,
}

impl Schedule {
    pub fn from_hours(hours: u64) -> Option<Self> {
        // Test hook, debug builds only: seconds instead of hours, no delay
        // after the start.
        #[cfg(debug_assertions)]
        if let Some(secs) = std::env::var("WRONGINDEX_DEBUG_CHECK_EVERY_SECS")
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
        {
            return (secs > 0).then_some(Self {
                every_ms: secs * 1000,
                after_start_ms: 0,
            });
        }
        (hours > 0).then_some(Self {
            every_ms: hours as i64 * 3_600_000,
            after_start_ms: AFTER_START.as_millis() as i64,
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
}

/// What the supervisor knows about one project for this decision.
pub struct Candidate<'a> {
    pub owned: bool,
    pub known_damaged: bool,
    /// Work pending, admitted or running for the project.
    pub busy: bool,
    pub last_check: Option<&'a JobRecord>,
}

/// Whether a check of this project is due at `now_ms`.
pub fn due(s: Schedule, started_ms: i64, now_ms: i64, c: &Candidate) -> bool {
    if !c.owned || c.known_damaged || c.busy || now_ms - started_ms < s.after_start_ms {
        return false;
    }
    match c.last_check {
        None => true,
        Some(j) if matches!(j.state, JobState::Queued | JobState::Running) => false,
        Some(j) => {
            let at = j.completed_at_ms.unwrap_or(j.admitted_at_ms);
            now_ms - at >= s.every_ms
        }
    }
}

/// The parameters of a scheduled check job.
pub fn params() -> serde_json::Value {
    serde_json::json!({"kind": JobKind::Check.as_str(), "paths": [], "scheduled": true})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(state: JobState, done: Option<i64>) -> JobRecord {
        JobRecord {
            job_id: "j".into(),
            project_id: "p".into(),
            kind: JobKind::Check,
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
            last_check: last,
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
    fn zero_hours_turns_it_off() {
        if std::env::var_os("WRONGINDEX_DEBUG_CHECK_EVERY_SECS").is_none() {
            assert_eq!(Schedule::from_hours(0), None);
            let s = Schedule::from_hours(24).unwrap();
            assert_eq!(s.every_ms, 86_400_000);
            assert_eq!(s.round(), ROUND);
        }
    }
}
