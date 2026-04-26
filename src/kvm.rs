//! kvm.rs — per-VM KVM exit counters from `/sys/kernel/debug/kvm`.
//!
//! Phase 3 of VM support. The kernel exposes one directory per
//! active VM under `/sys/kernel/debug/kvm/<pid>-<inode>/` (the `pid`
//! matches the hypervisor process pid; `inode` is the dev/ino pair
//! of the per-VM file descriptor — uniquifies a single hypervisor
//! that runs more than one guest, which `qemu-system-*` doesn't
//! but `cloud-hypervisor` and others can). Each directory holds
//! single-integer files for every VM-exit class:
//!
//! * `exits` — total VM exits
//! * `mmio_exits` — MMIO-trapped exits (device emulation cost)
//! * `io_exits` — port-IO exits (legacy device emulation)
//! * `halt_exits` — guest-halt exits (idle / waiting on IRQ)
//! * `irq_injections` — host→guest interrupt injections
//!
//! All counters are monotonic-per-VM; we sample, store, and divide
//! by the wall-clock delta to get per-second rates.
//!
//! Why this matters: VM-exit rates are the single best signal for
//! "this guest is thrashing" that a host-side observer can see
//! without a guest agent. A web app running in a VM with 50k
//! `mmio_exits/s` is bottlenecked on virtual-device emulation; one
//! with 100k `halt_exits/s` is mostly idle. `htop` shows neither.
//!
//! Permissions: `/sys/kernel/debug/` is root-only on most distros.
//! The tracker feature-detects on construction; if the directory
//! isn't readable, every subsequent `snapshot` returns `None` and
//! the UI falls back to "—". No errors are logged — this is the
//! expected case for non-root users.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Root of the KVM debugfs tree. Compile-time constant so the
/// tracker can refuse to do anything when feature-detect fails.
const KVM_ROOT: &str = "/sys/kernel/debug/kvm";

/// Counter files we care about. Each is a tiny single-integer file
/// in the per-VM directory. Order matches `KvmCounts` field order
/// so we can index into a fixed-size sample array.
const COUNTER_FILES: [&str; 5] = [
    "exits",
    "mmio_exits",
    "io_exits",
    "halt_exits",
    "irq_injections",
];

/// Raw monotonic counters scraped out of one VM's debugfs dir.
/// All are absolute values since the VM was created — useless on
/// their own. The `Tracker` differentiates them against the
/// previous sample to produce `KvmRates`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct KvmCounts {
    pub(crate) exits: u64,
    pub(crate) mmio_exits: u64,
    pub(crate) io_exits: u64,
    pub(crate) halt_exits: u64,
    pub(crate) irq_injections: u64,
}

/// Per-second rates derived from two consecutive `KvmCounts`
/// samples. The first call to `snapshot` for a pid returns `None`
/// (no prior sample); from the second call on, every field is a
/// real `events / s`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct KvmRates {
    pub(crate) exits: f64,
    pub(crate) mmio_exits: f64,
    pub(crate) io_exits: f64,
    pub(crate) halt_exits: f64,
    pub(crate) irq_injections: f64,
}

#[derive(Debug, Default)]
pub(crate) struct Tracker {
    /// Previous (sample time, counters) per VM pid. Cleared lazily
    /// when a snapshot can't find the pid's debugfs dir anymore
    /// (guest rebooted, qemu exited).
    prev: HashMap<i32, (Instant, KvmCounts)>,
    /// `false` when `/sys/kernel/debug/kvm` isn't readable. Set
    /// once at construction so the per-tick path is a single bool
    /// check, not a syscall, on the common non-root case.
    available: bool,
}

impl Tracker {
    /// Probe for debugfs availability. The check is a single
    /// `read_dir` on `KVM_ROOT` — succeeds when the kernel has
    /// `kvm_intel`/`kvm_amd` loaded with debugfs mounted *and* the
    /// running uid can traverse the directory. Failure is silent.
    pub(crate) fn new() -> Self {
        let available = fs::read_dir(KVM_ROOT).is_ok();
        Self {
            prev: HashMap::new(),
            available,
        }
    }

    /// `true` when the tracker has a chance of returning rates. The
    /// UI uses this to decide whether to show the `── kvm exits ──`
    /// block or a one-line "(debugfs not readable)" hint.
    pub(crate) fn is_available(&self) -> bool {
        self.available
    }

    /// Compute per-second exit rates for the given hypervisor pid.
    /// Returns `None` until we have two samples in a row — same
    /// semantics as `procs::Tracker` reporting `cpu_pct = None` on
    /// first sight of a pid.
    pub(crate) fn snapshot(&mut self, pid: i32) -> Option<KvmRates> {
        if !self.available {
            return None;
        }
        let dir = find_vm_dir(pid)?;
        let counts = read_counts(&dir)?;
        let now = Instant::now();
        let rates = self.prev.get(&pid).and_then(|(prev_when, prev)| {
            let dt = now.duration_since(*prev_when).as_secs_f64();
            if dt <= 0.0 {
                return None;
            }
            Some(rates_between(*prev, counts, dt))
        });
        self.prev.insert(pid, (now, counts));
        rates
    }

    /// Drop cached samples for pids that no longer have a debugfs
    /// dir. Call once per slow tick so a host that churns
    /// short-lived microVMs doesn't keep a sample-per-dead-pid in
    /// memory forever.
    pub(crate) fn purge_dead(&mut self, alive_pids: &[i32]) {
        self.prev.retain(|pid, _| alive_pids.contains(pid));
    }
}

/// Locate the per-VM debugfs directory for `pid`. KVM names them
/// `<pid>-<inode>`; we match the prefix because `<inode>` rotates
/// across guest reboots and we don't want to stat every entry.
fn find_vm_dir(pid: i32) -> Option<PathBuf> {
    let entries = fs::read_dir(KVM_ROOT).ok()?;
    let prefix = format!("{pid}-");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.starts_with(&prefix) {
            return Some(entry.path());
        }
    }
    None
}

/// Read every counter file in the VM dir into a `KvmCounts`. A
/// missing or unparseable file degrades to 0 for that field rather
/// than failing the whole snapshot — kernel versions vary in which
/// counters they expose, and we'd rather show 4-of-5 than nothing.
fn read_counts(dir: &Path) -> Option<KvmCounts> {
    let mut vals = [0u64; COUNTER_FILES.len()];
    let mut any_read = false;
    for (i, name) in COUNTER_FILES.iter().enumerate() {
        let path = dir.join(name);
        if let Ok(raw) = fs::read_to_string(&path) {
            if let Ok(n) = raw.trim().parse::<u64>() {
                vals[i] = n;
                any_read = true;
            }
        }
    }
    // If *every* counter failed, the dir is probably gone (guest
    // exited mid-tick) — give the caller `None` so it can fall
    // back rather than reporting a frozen "0 events/s" forever.
    if !any_read {
        return None;
    }
    Some(KvmCounts {
        exits: vals[0],
        mmio_exits: vals[1],
        io_exits: vals[2],
        halt_exits: vals[3],
        irq_injections: vals[4],
    })
}

/// Pure rate-of-change between two `KvmCounts` samples taken `dt`
/// seconds apart. Saturating subtract handles the rare case where
/// debugfs counters wrap or reset under us (e.g. live migration
/// hand-off), turning what would be a giant negative spike into a
/// clean zero.
pub(crate) fn rates_between(prev: KvmCounts, cur: KvmCounts, dt: f64) -> KvmRates {
    let inv = if dt > 0.0 { 1.0 / dt } else { 0.0 };
    #[allow(clippy::cast_precision_loss)]
    let r = |a: u64, b: u64| (b.saturating_sub(a) as f64) * inv;
    KvmRates {
        exits: r(prev.exits, cur.exits),
        mmio_exits: r(prev.mmio_exits, cur.mmio_exits),
        io_exits: r(prev.io_exits, cur.io_exits),
        halt_exits: r(prev.halt_exits, cur.halt_exits),
        irq_injections: r(prev.irq_injections, cur.irq_injections),
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_between_divides_delta_by_seconds() {
        // 1 s of wall clock, 1 000 new exits → 1 000 events/s.
        let prev = KvmCounts {
            exits: 5_000,
            mmio_exits: 100,
            io_exits: 50,
            halt_exits: 800,
            irq_injections: 30,
        };
        let cur = KvmCounts {
            exits: 6_000,
            mmio_exits: 250,
            io_exits: 75,
            halt_exits: 1_200,
            irq_injections: 60,
        };
        let r = rates_between(prev, cur, 1.0);
        assert!((r.exits - 1_000.0).abs() < 1e-9);
        assert!((r.mmio_exits - 150.0).abs() < 1e-9);
        assert!((r.io_exits - 25.0).abs() < 1e-9);
        assert!((r.halt_exits - 400.0).abs() < 1e-9);
        assert!((r.irq_injections - 30.0).abs() < 1e-9);
    }

    #[test]
    fn rates_between_handles_counter_reset() {
        // After a live-migration hand-off the counter file may go
        // back to a smaller value. Saturating-sub clamps that to
        // zero rather than wrapping into a 10^19 events/s spike.
        let prev = KvmCounts {
            exits: 1_000_000,
            ..Default::default()
        };
        let cur = KvmCounts {
            exits: 5,
            ..Default::default()
        };
        let r = rates_between(prev, cur, 1.0);
        assert!((r.exits - 0.0).abs() < 1e-9);
    }

    #[test]
    fn rates_between_zero_dt_yields_zero() {
        // A double-tick at the same Instant is theoretically possible
        // on hosts with coarse monotonic clocks; never divide by zero.
        let prev = KvmCounts {
            exits: 100,
            ..Default::default()
        };
        let cur = KvmCounts {
            exits: 200,
            ..Default::default()
        };
        let r = rates_between(prev, cur, 0.0);
        assert!((r.exits - 0.0).abs() < 1e-9);
    }

    #[test]
    fn tracker_disabled_when_kvm_root_unreadable() {
        // On any host where /sys/kernel/debug/kvm isn't readable
        // (non-root user, no debugfs, no kvm module) the tracker
        // initialises to "unavailable" and stays that way. snapshot()
        // is a single bool check on the hot path.
        let mut t = Tracker::new();
        if !t.is_available() {
            // Common case: non-root user without CAP_SYS_ADMIN.
            assert!(t.snapshot(1).is_none());
        }
    }

    #[test]
    fn purge_dead_drops_pids_no_longer_alive() {
        let mut t = Tracker {
            available: true,
            prev: HashMap::new(),
        };
        let now = Instant::now();
        t.prev.insert(101, (now, KvmCounts::default()));
        t.prev.insert(202, (now, KvmCounts::default()));
        t.prev.insert(303, (now, KvmCounts::default()));
        t.purge_dead(&[101, 303]);
        assert!(t.prev.contains_key(&101));
        assert!(!t.prev.contains_key(&202));
        assert!(t.prev.contains_key(&303));
    }
}
