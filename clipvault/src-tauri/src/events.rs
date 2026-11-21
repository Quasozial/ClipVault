//! In-memory marker bus: event sources (RL goals, laugh detector, manual mark
//! hotkey) push wall-clock events; a save turns those in its window into
//! clip-relative markers.

use crate::db::clips::MarkerKind;
use crate::locks;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// Extra time kept beyond the longest possible save window.
const RETENTION_SLACK: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub struct BusEvent {
    pub at: SystemTime,
    pub kind: MarkerKind,
    pub label: String,
}

#[derive(Debug)]
pub struct EventBus {
    retention: Mutex<Duration>,
    events: Mutex<VecDeque<BusEvent>>,
}

impl EventBus {
    /// Keeps events for `max_save_length + 10 s`.
    pub fn new(max_save_length: Duration) -> Self {
        Self { retention: Mutex::new(max_save_length + RETENTION_SLACK), events: Mutex::new(VecDeque::new()) }
    }

    pub fn retention(&self) -> Duration {
        *locks::lock(&self.retention)
    }

    /// Keeps events for `max_save_length + 10 s` from the next push on.
    pub fn set_max_save_length(&self, max_save_length: Duration) {
        *locks::lock(&self.retention) = max_save_length + RETENTION_SLACK;
    }

    /// Adds an event and drops events older than the retention (relative to now).
    /// Sources may report events slightly late, so arrival order is not assumed.
    pub fn push(&self, ev: BusEvent) {
        let cutoff = SystemTime::now().checked_sub(self.retention());
        let mut q = locks::lock(&self.events);
        q.push_back(ev);
        if let Some(cutoff) = cutoff {
            q.retain(|e| e.at >= cutoff);
        }
    }

    /// Events with `from <= at <= to`, sorted by time.
    pub fn between(&self, from: SystemTime, to: SystemTime) -> Vec<BusEvent> {
        let mut out: Vec<BusEvent> = {
            let q = locks::lock(&self.events);
            q.iter().filter(|e| e.at >= from && e.at <= to).cloned().collect()
        };
        out.sort_by_key(|e| e.at);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(at: SystemTime, kind: MarkerKind, label: &str) -> BusEvent {
        BusEvent { at, kind, label: label.into() }
    }

    #[test]
    fn bus_between_filters() {
        let bus = EventBus::new(Duration::from_secs(60));
        let now = SystemTime::now();
        let s = Duration::from_secs;
        // Pushed out of order on purpose.
        bus.push(ev(now - s(5), MarkerKind::Laugh, "c"));
        bus.push(ev(now - s(30), MarkerKind::Goal, "a"));
        bus.push(ev(now - s(20), MarkerKind::Manual, "b"));
        bus.push(ev(now - s(1), MarkerKind::Goal, "d"));

        let got = bus.between(now - s(20), now - s(5));
        let labels: Vec<&str> = got.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["b", "c"], "inclusive bounds, sorted by time");
        assert!(bus.between(now - s(100), now - s(50)).is_empty());
        assert_eq!(bus.between(now - s(100), now).len(), 4);
    }

    #[test]
    fn bus_retention_can_grow_live() {
        let bus = EventBus::new(Duration::from_secs(60));
        bus.set_max_save_length(Duration::from_secs(300));
        assert_eq!(bus.retention(), Duration::from_secs(310));
        let now = SystemTime::now();
        bus.push(ev(now - Duration::from_secs(200), MarkerKind::Goal, "kept"));
        bus.push(ev(now, MarkerKind::Goal, "new"));
        assert_eq!(bus.between(now - Duration::from_secs(1000), now).len(), 2);
    }

    #[test]
    fn bus_prunes_beyond_retention() {
        let bus = EventBus::new(Duration::from_secs(60)); // retention 70 s
        let now = SystemTime::now();
        let s = Duration::from_secs;
        bus.push(ev(now - s(75), MarkerKind::Goal, "old"));
        bus.push(ev(now - s(65), MarkerKind::Goal, "kept"));
        bus.push(ev(now, MarkerKind::Manual, "new"));
        let labels: Vec<String> =
            bus.between(now - s(1000), now + s(1)).into_iter().map(|e| e.label).collect();
        assert_eq!(labels, ["kept", "new"]);
    }
}
