//! In-memory service metrics (WI-209): job outcomes and durations per kind,
//! native phase durations, and the bounds the service enforces. Counters
//! reset when the service restarts; no paths or project content are kept.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{Value, json};

/// Durations kept per series for percentiles.
const WINDOW: usize = 256;

#[derive(Default)]
struct Series {
    count: u64,
    sum_ms: u64,
    max_ms: u64,
    recent: std::collections::VecDeque<u64>,
}

impl Series {
    fn add(&mut self, ms: u64) {
        self.count += 1;
        self.sum_ms += ms;
        self.max_ms = self.max_ms.max(ms);
        if self.recent.len() == WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(ms);
    }

    fn summary(&self) -> Value {
        let mut v: Vec<u64> = self.recent.iter().copied().collect();
        v.sort_unstable();
        let pct = |p: f64| -> Option<u64> {
            if v.is_empty() {
                None
            } else {
                Some(v[((v.len() - 1) as f64 * p).round() as usize])
            }
        };
        json!({
            "count": self.count,
            "meanMs": self.sum_ms.checked_div(self.count),
            "maxMs": self.max_ms,
            "p50Ms": pct(0.5),
            "p95Ms": pct(0.95),
        })
    }
}

#[derive(Default)]
struct Inner {
    /// `kind` -> `state` -> count.
    jobs: BTreeMap<String, BTreeMap<String, u64>>,
    /// `kind` durations of finished jobs.
    durations: BTreeMap<String, Series>,
    /// Native phase durations.
    phases: BTreeMap<String, Series>,
}

#[derive(Default)]
pub struct Metrics {
    inner: Mutex<Inner>,
}

impl Metrics {
    pub fn job_finished(&self, kind: &str, state: &str, ms: u64) {
        if let Ok(mut g) = self.inner.lock() {
            *g.jobs
                .entry(kind.to_string())
                .or_default()
                .entry(state.to_string())
                .or_default() += 1;
            g.durations.entry(kind.to_string()).or_default().add(ms);
        }
    }

    pub fn phase(&self, phase: &str, ms: u64) {
        // Phase names come from the engine (a fixed set); bound them anyway.
        if phase.len() > 64 {
            return;
        }
        if let Ok(mut g) = self.inner.lock()
            && (g.phases.len() < 64 || g.phases.contains_key(phase))
        {
            g.phases.entry(phase.to_string()).or_default().add(ms);
        }
    }

    pub fn snapshot(&self) -> Value {
        let Ok(g) = self.inner.lock() else {
            return json!({});
        };
        json!({
            "jobs": g.jobs,
            "jobDurations": g.durations.iter().map(|(k, s)| (k.clone(), s.summary())).collect::<BTreeMap<_, _>>(),
            "nativePhases": g.phases.iter().map(|(k, s)| (k.clone(), s.summary())).collect::<BTreeMap<_, _>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_summaries() {
        let m = Metrics::default();
        for ms in [10, 20, 30, 40] {
            m.job_finished("full", "succeeded", ms);
        }
        m.job_finished("full", "failed", 5);
        m.phase("discover", 100);
        m.phase(&"x".repeat(65), 1);
        let s = m.snapshot();
        assert_eq!(s["jobs"]["full"]["succeeded"], 4);
        assert_eq!(s["jobs"]["full"]["failed"], 1);
        assert_eq!(s["jobDurations"]["full"]["count"], 5);
        assert_eq!(s["jobDurations"]["full"]["maxMs"], 40);
        assert_eq!(s["jobDurations"]["full"]["p50Ms"], 20);
        assert_eq!(s["nativePhases"]["discover"]["meanMs"], 100);
        assert_eq!(s["nativePhases"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn the_window_is_bounded() {
        let m = Metrics::default();
        for i in 0..1000 {
            m.phase("parse", i);
        }
        let g = m.inner.lock().unwrap();
        assert_eq!(g.phases["parse"].recent.len(), WINDOW);
        assert_eq!(g.phases["parse"].count, 1000);
    }
}
