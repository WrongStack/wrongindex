//! Event stream (architecture §8.3).
//!
//! Every event carries the daemon instance id and a per-instance monotonically
//! increasing sequence number. A bounded ring buffer serves replay from a
//! client's last seen sequence; when the requested position has been evicted
//! (or the instance changed), the client is told there is a gap and must take
//! a snapshot. Slow subscribers never grow memory: a lagging broadcast receiver
//! gets a gap marker instead of unbounded history.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

pub const RING: usize = 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub seq: u64,
    pub instance_id: String,
    pub at_ms: i64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub data: Value,
}

pub struct EventBus {
    instance_id: String,
    seq: AtomicU64,
    ring: Mutex<VecDeque<Event>>,
    tx: broadcast::Sender<Event>,
}

/// Result of positioning a new subscriber.
pub struct Subscription {
    pub replay: Vec<Event>,
    /// The requested history is not available: take a snapshot.
    pub gap: bool,
    pub rx: broadcast::Receiver<Event>,
    pub current_seq: u64,
}

impl EventBus {
    pub fn new(instance_id: String) -> Self {
        let (tx, _) = broadcast::channel(RING);
        Self {
            instance_id,
            seq: AtomicU64::new(0),
            ring: Mutex::new(VecDeque::with_capacity(RING)),
            tx,
        }
    }

    pub fn publish(&self, kind: &str, project_id: Option<&str>, data: Value) {
        // Assign the sequence and append under the ring lock so replay order
        // always equals sequence order.
        let Ok(mut ring) = self.ring.lock() else {
            return;
        };
        let ev = Event {
            seq: self.seq.fetch_add(1, Ordering::SeqCst) + 1,
            instance_id: self.instance_id.clone(),
            at_ms: wrongindex_registry::now_ms(),
            kind: kind.to_string(),
            project_id: project_id.map(str::to_string),
            data,
        };
        if ring.len() == RING {
            ring.pop_front();
        }
        ring.push_back(ev.clone());
        let _ = self.tx.send(ev);
    }

    pub fn current_seq(&self) -> u64 {
        // Taken under the ring lock so it never runs ahead of the ring.
        let _ring = self.ring.lock();
        self.seq.load(Ordering::SeqCst)
    }

    /// Subscribe after `since` (exclusive). `since_instance` must match this
    /// instance for replay; otherwise the subscriber starts with a gap.
    pub fn subscribe(&self, since: Option<u64>, since_instance: Option<&str>) -> Subscription {
        let ring = self.ring.lock().expect("event ring");
        // Subscribe while holding the ring lock: nothing can be published
        // between the replay snapshot and the live receiver.
        let rx = self.tx.subscribe();
        let current_seq = self.seq.load(Ordering::SeqCst);
        let same_instance = since_instance.is_none_or(|i| i == self.instance_id);
        let (replay, gap) = match since {
            None => (Vec::new(), false),
            Some(_) if !same_instance => (Vec::new(), true),
            Some(s) if s >= current_seq => (Vec::new(), false),
            Some(s) => {
                let oldest = ring.front().map(|e| e.seq).unwrap_or(current_seq + 1);
                if s + 1 < oldest {
                    (Vec::new(), true)
                } else {
                    (ring.iter().filter(|e| e.seq > s).cloned().collect(), false)
                }
            }
        };
        Subscription {
            replay,
            gap,
            rx,
            current_seq,
        }
    }
}

/// Publish a job lifecycle event with the fields a UI needs to update a row.
pub fn job_event(
    shared: &crate::server::Shared,
    kind: &str,
    job: &wrongindex_contracts::JobRecord,
) {
    shared.events.publish(
        kind,
        Some(&job.project_id),
        serde_json::json!({
            "jobId": job.job_id,
            "kind": job.kind.as_str(),
            "state": job.state.as_str(),
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replay_from_sequence_and_gap_detection() {
        let bus = EventBus::new("d1".into());
        for i in 0..5 {
            bus.publish("k", None, json!({"i": i}));
        }
        let s = bus.subscribe(Some(2), Some("d1"));
        assert!(!s.gap);
        assert_eq!(
            s.replay.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert_eq!(s.current_seq, 5);
        // Another instance's sequence numbers are meaningless here.
        assert!(bus.subscribe(Some(2), Some("d0")).gap);
        // Up to date: nothing to replay, no gap.
        let s = bus.subscribe(Some(5), Some("d1"));
        assert!(s.replay.is_empty() && !s.gap);
    }

    #[test]
    fn evicted_history_is_a_gap() {
        let bus = EventBus::new("d".into());
        for _ in 0..(RING + 10) {
            bus.publish("k", None, Value::Null);
        }
        assert!(bus.subscribe(Some(1), None).gap);
        let recent = bus.subscribe(Some((RING + 5) as u64), None);
        assert!(!recent.gap);
        assert_eq!(recent.replay.len(), 5);
    }

    #[tokio::test]
    async fn live_events_follow_replay_without_loss() {
        let bus = EventBus::new("d".into());
        bus.publish("a", None, Value::Null);
        let mut s = bus.subscribe(Some(0), None);
        bus.publish("b", Some("p"), Value::Null);
        assert_eq!(s.replay.len(), 1);
        let live = s.rx.recv().await.unwrap();
        assert_eq!((live.seq, live.kind.as_str()), (2, "b"));
    }
}
