//! errors.rs — bounded ring of non-fatal parse / I/O errors surfaced
//! to the UI footer.
//!
//! The design goal is *honesty*: when a `/proc` or `/sys` file we
//! expected to read disappears or fails to parse, the user should see
//! a compact note rather than an empty table. Per-pid reads during the
//! `/proc` walk are deliberately *not* routed here — pids race with
//! exec/exit and reporting them would create thousands of bogus lines.
//!
//! Capacity is 16 entries; the oldest is dropped on overflow. The UI
//! shows only the latest entry, fading it out 5 s after it was pushed.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const RING_CAP: usize = 16;

/// One non-fatal error. `source` is a short, stable tag (e.g. `"hwmon"`,
/// `"net"`) so the UI can color-code it consistently across runs;
/// `message` is the human-readable bit.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) when: Instant,
    pub(crate) source: &'static str,
    pub(crate) message: String,
}

#[derive(Debug, Default)]
pub(crate) struct ErrorRing {
    entries: VecDeque<Entry>,
    /// Lifetime count of pushes — drives the "(N err)" badge in the
    /// UI. Saturates at `u64::MAX`, which happens at exactly never.
    total: u64,
}

impl ErrorRing {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push(&mut self, source: &'static str, message: impl Into<String>) {
        if self.entries.len() == RING_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry {
            when: Instant::now(),
            source,
            message: message.into(),
        });
        self.total = self.total.saturating_add(1);
    }

    /// Latest entry whose age is `<= ttl`. Older entries stay in the
    /// ring (so a future "all errors" view could browse them) but
    /// don't clutter the footer.
    pub(crate) fn latest_within(&self, ttl: Duration) -> Option<&Entry> {
        let last = self.entries.back()?;
        if last.when.elapsed() <= ttl {
            Some(last)
        } else {
            None
        }
    }

    pub(crate) fn total(&self) -> u64 {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_evicts_oldest_at_capacity() {
        let mut r = ErrorRing::new();
        for i in 0..(RING_CAP as u64 + 5) {
            r.push("test", format!("err {i}"));
        }
        assert_eq!(r.entries.len(), RING_CAP);
        assert_eq!(r.total(), RING_CAP as u64 + 5);
        // First entry should be err 5, last should be err RING_CAP+4.
        assert_eq!(r.entries.front().unwrap().message, "err 5");
        assert_eq!(
            r.entries.back().unwrap().message,
            format!("err {}", RING_CAP as u64 + 4)
        );
    }

    #[test]
    fn latest_within_filters_by_age() {
        let mut r = ErrorRing::new();
        r.push("test", "fresh");
        // ttl=5s should include the fresh entry.
        assert!(r.latest_within(Duration::from_secs(5)).is_some());
        // ttl=0ns should exclude it.
        assert!(r.latest_within(Duration::from_nanos(0)).is_none());
    }

    #[test]
    fn latest_within_empty_returns_none() {
        let r = ErrorRing::new();
        assert!(r.latest_within(Duration::from_secs(5)).is_none());
    }
}
