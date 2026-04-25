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

/// Non-fatal events surface at one of two severities. The point of
/// the distinction is *honesty*: parking a slow `acpitz` sensor and
/// failing to read `/proc/stat` are very different events, but the
/// old single-severity ring rendered both with the same red `⚠ (N
/// err)` badge. That scared users into thinking neotop was broken
/// when in fact it had self-healed and was running fine.
///
/// - `Warn` — something we expected to read failed, the user might
///   want to investigate. Red ⚠ in the footer; counted in `(N err)`.
/// - `Info` — a deliberate self-protection action (parked sensor,
///   skipped scanner) the user should know about but doesn't need
///   to act on. Yellow ℹ in the footer; not counted as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Info,
    Warn,
}

/// One non-fatal event. `source` is a short, stable tag
/// (e.g. `"hwmon"`, `"net"`) so the UI can color-code it
/// consistently across runs; `message` is the human-readable bit.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) when: Instant,
    pub(crate) source: &'static str,
    pub(crate) message: String,
    pub(crate) severity: Severity,
}

#[derive(Debug, Default)]
pub(crate) struct ErrorRing {
    entries: VecDeque<Entry>,
    /// Lifetime count of `Warn`-severity pushes — drives the "(N
    /// err)" badge in the UI. Info-severity pushes (parked
    /// sensors etc) are tracked separately so they don't inflate
    /// the error count.
    total_warns: u64,
}

impl ErrorRing {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Push a `Warn`-severity event. This is the legacy
    /// `push` semantics — kept under the same name so the dozens
    /// of existing call sites in `host`, `net`, `disk`, etc keep
    /// working without churn.
    pub(crate) fn push(&mut self, source: &'static str, message: impl Into<String>) {
        self.push_with(source, message, Severity::Warn);
    }

    /// Push an `Info`-severity event. Use this for self-protection
    /// notifications (parked sensors, skipped scanners) that the
    /// user should see but that don't represent a failure.
    pub(crate) fn push_info(&mut self, source: &'static str, message: impl Into<String>) {
        self.push_with(source, message, Severity::Info);
    }

    fn push_with(&mut self, source: &'static str, message: impl Into<String>, severity: Severity) {
        if self.entries.len() == RING_CAP {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry {
            when: Instant::now(),
            source,
            message: message.into(),
            severity,
        });
        if severity == Severity::Warn {
            self.total_warns = self.total_warns.saturating_add(1);
        }
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

    /// Lifetime count of `Warn`-severity pushes. The footer's "(N
    /// err)" badge reads this. Info pushes are not counted.
    pub(crate) fn total(&self) -> u64 {
        self.total_warns
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

    #[test]
    fn push_uses_warn_severity_by_default() {
        // Existing call sites all expected `push` to mean "this
        // is a real warning" — the v0.13.0 footer rendered them
        // in red. Don't silently downgrade them by changing the
        // default; that would mute genuine errors.
        let mut r = ErrorRing::new();
        r.push("test", "boom");
        let last = r.entries.back().unwrap();
        assert_eq!(last.severity, Severity::Warn);
    }

    #[test]
    fn push_info_does_not_count_toward_error_total() {
        // The whole point of the Info tier: parked-sensor messages
        // and other self-protection notifications should NOT
        // inflate the "(N err)" badge in the footer.
        let mut r = ErrorRing::new();
        r.push_info("hwmon", "parked acpitz");
        r.push_info("hwmon", "parked nct6779");
        assert_eq!(r.total(), 0, "info pushes don't count as errors");
        assert_eq!(r.entries.len(), 2, "but they're still in the ring");
        assert!(r.entries.iter().all(|e| e.severity == Severity::Info));
    }

    #[test]
    fn push_warn_and_info_count_separately() {
        // Mixed sequence: warns increment the total, infos don't,
        // and both end up in the ring in insertion order.
        let mut r = ErrorRing::new();
        r.push("net", "rx parse failed");
        r.push_info("hwmon", "parked acpitz");
        r.push("disk", "no diskstats");
        assert_eq!(r.total(), 2, "two warns counted");
        let severities: Vec<_> = r.entries.iter().map(|e| e.severity).collect();
        assert_eq!(
            severities,
            vec![Severity::Warn, Severity::Info, Severity::Warn]
        );
    }
}
