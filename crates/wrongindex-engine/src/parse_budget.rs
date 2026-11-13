//! Shared admission for in-process parsers and all helper chunk families.
use std::sync::{Condvar, Mutex, OnceLock};

pub(crate) fn slots() -> usize {
    std::env::var("WRONGINDEX_NATIVE_PARSER_SLOTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()))
        .clamp(1, 16)
}
struct Gate {
    available: Mutex<usize>,
    wake: Condvar,
}
static GATE: OnceLock<Gate> = OnceLock::new();
pub(crate) struct Permit(&'static Gate);
pub(crate) fn acquire() -> Permit {
    let gate = GATE.get_or_init(|| Gate {
        available: Mutex::new(slots()),
        wake: Condvar::new(),
    });
    let mut available = gate.available.lock().unwrap_or_else(|e| e.into_inner());
    while *available == 0 {
        available = gate.wake.wait(available).unwrap_or_else(|e| e.into_inner());
    }
    *available -= 1;
    Permit(gate)
}
impl Drop for Permit {
    fn drop(&mut self) {
        *self.0.available.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.0.wake.notify_one();
    }
}
