//! Per-project dirty set with revisions, debounce, and overflow collapse
//! (indexing-pipeline §4).
//!
//! * Each observed path gets the latest global revision.
//! * A batch is ready after a quiet period (`debounce`) since the last event,
//!   or once the oldest pending event has waited `max_wait` (continuous saves
//!   cannot postpone indexing forever).
//! * Beyond `cap` distinct paths the detail is dropped and one reconcile
//!   marker remains (bounded memory).
//! * Taking a batch does not clear it. Settling a successful batch removes
//!   only entries whose revision the batch covered; edits that arrived during
//!   the run stay dirty. A failed batch leaves everything dirty and backs
//!   off: the next attempt waits 2 s, doubling per consecutive failure up to
//!   60 s, so a project that keeps failing cannot occupy the worker in a
//!   tight loop. A success resets the backoff.
//! * Circuit (WI-208): after `park_after` consecutive failures the project is
//!   parked - one probe attempt every 15 minutes instead of every minute -
//!   until a batch or an explicit job succeeds (`clear_failures`). Parking
//!   is reported (`watchHealth: parked`, freshness `degraded`), never
//!   silent.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct DebounceConfig {
    pub debounce: Duration,
    pub max_wait: Duration,
    pub cap: usize,
    /// Consecutive failed batches after which the project is parked.
    pub park_after: u32,
}

impl Default for DebounceConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(400),
            max_wait: Duration::from_secs(2),
            cap: 10_000,
            park_after: 8,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// Paths to re-index (empty when `reconcile` is set).
    pub paths: Vec<String>,
    /// A full reconciliation replaces path detail.
    pub reconcile: Option<String>,
    /// Highest revision covered by this batch.
    pub revision: u64,
}

#[derive(Debug, Default)]
pub struct DirtySet {
    paths: BTreeMap<String, u64>,
    reconcile: Option<(String, u64)>,
    revision: u64,
    first_at: Option<Instant>,
    last_at: Option<Instant>,
    overflows: u64,
    /// Consecutive failed batches.
    failures: u32,
    /// No batch is ready before this instant (after a failure).
    retry_at: Option<Instant>,
}

/// First retry delay after a failed batch; doubles per consecutive failure.
pub const RETRY_BASE: Duration = Duration::from_secs(2);
/// Longest retry delay.
pub const RETRY_MAX: Duration = Duration::from_secs(60);
/// While parked: one probe attempt this often.
pub const PARK_PROBE: Duration = Duration::from_secs(15 * 60);

impl DirtySet {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.reconcile.is_none()
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn overflows(&self) -> u64 {
        self.overflows
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// The circuit is open: automatic attempts are down to one probe per
    /// `PARK_PROBE`.
    pub fn parked(&self, cfg: &DebounceConfig) -> bool {
        self.failures >= cfg.park_after
    }

    /// How long until the next automatic attempt is allowed (after failures).
    pub fn retry_in(&self, now: Instant) -> Option<Duration> {
        self.retry_at
            .filter(|t| *t > now)
            .map(|t| t.duration_since(now))
    }

    /// The cause went away (an explicit job succeeded): close the circuit and
    /// let pending work run now.
    pub fn clear_failures(&mut self) -> bool {
        let had = self.failures > 0;
        self.failures = 0;
        self.retry_at = None;
        had
    }

    pub fn reconcile_reason(&self) -> Option<&str> {
        self.reconcile.as_ref().map(|(r, _)| r.as_str())
    }

    fn touch(&mut self, now: Instant) -> u64 {
        self.revision += 1;
        self.first_at.get_or_insert(now);
        self.last_at = Some(now);
        self.revision
    }

    pub fn mark(&mut self, path: String, now: Instant, cfg: &DebounceConfig) {
        let rev = self.touch(now);
        if self.reconcile.is_some() {
            // Reconciliation already covers every path; keep it current.
            if let Some((_, r)) = &mut self.reconcile {
                *r = rev;
            }
            return;
        }
        self.paths.insert(path, rev);
        if self.paths.len() > cfg.cap {
            self.paths.clear();
            self.overflows += 1;
            self.reconcile = Some(("dirty_set_overflow".into(), rev));
        }
    }

    pub fn mark_reconcile(&mut self, reason: &str, now: Instant) {
        let rev = self.touch(now);
        self.paths.clear();
        self.reconcile = Some((reason.to_string(), rev));
    }

    pub fn ready(&self, now: Instant, cfg: &DebounceConfig) -> bool {
        if self.is_empty() || self.retry_at.is_some_and(|t| now < t) {
            return false;
        }
        let quiet = self
            .last_at
            .is_some_and(|t| now.duration_since(t) >= cfg.debounce);
        let overdue = self
            .first_at
            .is_some_and(|t| now.duration_since(t) >= cfg.max_wait);
        quiet || overdue
    }

    pub fn take(&self) -> Option<Batch> {
        if self.is_empty() {
            return None;
        }
        Some(match &self.reconcile {
            Some((reason, _)) => Batch {
                paths: Vec::new(),
                reconcile: Some(reason.clone()),
                revision: self.revision,
            },
            None => Batch {
                paths: self.paths.keys().cloned().collect(),
                reconcile: None,
                revision: self.revision,
            },
        })
    }

    /// Settle a finished batch. Success clears what it covered; anything
    /// newer survives. Failure keeps everything.
    pub fn settle(&mut self, batch: &Batch, success: bool, now: Instant, cfg: &DebounceConfig) {
        if !success {
            self.failures = self.failures.saturating_add(1);
            let delay = if self.parked(cfg) {
                PARK_PROBE
            } else {
                RETRY_BASE
                    .saturating_mul(1u32 << (self.failures - 1).min(16))
                    .min(RETRY_MAX)
            };
            self.retry_at = Some(now + delay);
            return;
        }
        self.failures = 0;
        self.retry_at = None;
        if batch.reconcile.is_some() {
            if self
                .reconcile
                .as_ref()
                .is_some_and(|(_, r)| *r <= batch.revision)
            {
                self.reconcile = None;
            }
            // Paths recorded before the reconcile run started are covered by it.
            self.paths.retain(|_, r| *r > batch.revision);
        } else {
            self.paths.retain(|_, r| *r > batch.revision);
        }
        if self.is_empty() {
            self.first_at = None;
            self.last_at = None;
        } else {
            // Remaining work restarts its wait from now.
            self.first_at = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DebounceConfig {
        DebounceConfig {
            debounce: Duration::from_millis(400),
            max_wait: Duration::from_secs(2),
            cap: 3,
            park_after: 100,
        }
    }

    #[test]
    fn debounce_waits_for_quiet_and_dedups() {
        let t0 = Instant::now();
        let mut d = DirtySet::default();
        d.mark("a.ts".into(), t0, &cfg());
        d.mark("a.ts".into(), t0 + Duration::from_millis(100), &cfg());
        assert_eq!(d.len(), 1);
        assert!(!d.ready(t0 + Duration::from_millis(300), &cfg()));
        assert!(d.ready(t0 + Duration::from_millis(500), &cfg()));
    }

    #[test]
    fn continuous_saves_cannot_postpone_forever() {
        let t0 = Instant::now();
        let mut d = DirtySet::default();
        let mut t = t0;
        while t < t0 + Duration::from_millis(1900) {
            d.mark("a.ts".into(), t, &cfg());
            t += Duration::from_millis(100);
            assert!(!d.ready(t, &cfg()) || t >= t0 + Duration::from_secs(2));
        }
        d.mark("a.ts".into(), t0 + Duration::from_millis(2050), &cfg());
        assert!(
            d.ready(t0 + Duration::from_millis(2100), &cfg()),
            "max wait"
        );
    }

    #[test]
    fn edits_during_a_run_survive_settlement() {
        let t0 = Instant::now();
        let mut d = DirtySet::default();
        d.mark("a.ts".into(), t0, &cfg());
        d.mark("b.ts".into(), t0, &cfg());
        let batch = d.take().unwrap();
        assert_eq!(batch.paths, vec!["a.ts", "b.ts"]);
        // a.ts edited again while the batch runs.
        d.mark("a.ts".into(), t0 + Duration::from_millis(10), &cfg());
        d.settle(&batch, true, t0 + Duration::from_millis(20), &cfg());
        assert_eq!(d.take().unwrap().paths, vec!["a.ts"]);
    }

    #[test]
    fn failure_keeps_everything() {
        let t0 = Instant::now();
        let mut d = DirtySet::default();
        d.mark("a.ts".into(), t0, &cfg());
        let batch = d.take().unwrap();
        d.settle(&batch, false, t0, &cfg());
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn failures_back_off_and_success_resets() {
        let t0 = Instant::now();
        let mut d = DirtySet::default();
        d.mark("a.ts".into(), t0, &cfg());
        let mut now = t0 + Duration::from_secs(1);
        let mut waits = Vec::new();
        for _ in 0..8 {
            assert!(d.ready(now, &cfg()));
            let batch = d.take().unwrap();
            d.settle(&batch, false, now, &cfg());
            let mut t = now;
            while !d.ready(t, &cfg()) {
                t += Duration::from_millis(500);
            }
            waits.push((t - now).as_secs());
            now = t;
        }
        assert_eq!(waits, vec![2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(d.failures(), 8);
        // A new edit does not bypass the backoff.
        let batch = d.take().unwrap();
        d.settle(&batch, false, now, &cfg());
        d.mark("b.ts".into(), now, &cfg());
        assert!(!d.ready(now + Duration::from_secs(1), &cfg()));
        let later = now + RETRY_MAX;
        let batch = d.take().unwrap();
        d.settle(&batch, true, later, &cfg());
        assert_eq!(d.failures(), 0);
        d.mark("c.ts".into(), later, &cfg());
        assert!(d.ready(later + Duration::from_millis(500), &cfg()));
    }

    #[test]
    fn repeated_failures_park_until_an_explicit_success() {
        let c = DebounceConfig {
            park_after: 3,
            ..cfg()
        };
        let t0 = Instant::now();
        let mut d = DirtySet::default();
        d.mark("a.ts".into(), t0, &c);
        let mut now = t0 + Duration::from_secs(1);
        for n in 1..=3 {
            assert!(d.ready(now, &c), "attempt {n}");
            assert!(!d.parked(&c));
            let batch = d.take().unwrap();
            d.settle(&batch, false, now, &c);
            now += d.retry_in(now).unwrap();
        }
        // Parked: the next attempt is a probe 15 minutes out, edits included.
        assert!(d.parked(&c));
        let probe = now;
        // The third failure (at probe - PARK_PROBE) set the probe delay.
        assert_eq!(d.retry_in(probe - PARK_PROBE), Some(PARK_PROBE));
        d.mark("b.ts".into(), probe - Duration::from_secs(60), &c);
        assert!(!d.ready(probe - Duration::from_secs(1), &c));
        assert!(d.ready(probe, &c));
        // A failed probe stays parked; an explicit success closes the circuit.
        let batch = d.take().unwrap();
        d.settle(&batch, false, probe, &c);
        assert!(d.parked(&c));
        assert_eq!(d.retry_in(probe), Some(PARK_PROBE));
        assert!(d.clear_failures());
        assert!(!d.parked(&c));
        assert!(d.ready(probe + Duration::from_secs(1), &c));
        assert!(!d.clear_failures());
    }

    #[test]
    fn overflow_collapses_to_reconcile_and_settles() {
        let t0 = Instant::now();
        let mut d = DirtySet::default();
        for p in ["a", "b", "c", "d"] {
            d.mark(p.into(), t0, &cfg());
        }
        assert_eq!(d.len(), 0, "detail released");
        assert_eq!(d.reconcile_reason(), Some("dirty_set_overflow"));
        assert_eq!(d.overflows(), 1);
        let batch = d.take().unwrap();
        assert!(batch.paths.is_empty());
        // An event after the reconcile started keeps the marker alive.
        d.mark("e".into(), t0 + Duration::from_millis(5), &cfg());
        d.settle(&batch, true, t0 + Duration::from_millis(10), &cfg());
        assert!(!d.is_empty(), "newer event must survive");
        let again = d.take().unwrap();
        d.settle(&again, true, t0 + Duration::from_millis(20), &cfg());
        assert!(d.is_empty());
    }
}
