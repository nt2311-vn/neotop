//! vcpus.rs — per-vCPU CPU% tracker for selected VM processes.
//!
//! Phase 2 of VM support. For a given VM pid, walks
//! `/proc/<pid>/task/*`, identifies threads whose `comm` matches a
//! known hypervisor's vCPU pattern, and computes per-thread CPU%
//! using the same delta-of-jiffies math `procs::Tracker` does for
//! whole processes.
//!
//! Only run for the *selected* VM each tick; scanning every VM
//! every tick would balloon I/O on hosts running 50+ guests.

#[cfg(target_os = "linux")]
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::time::Instant;

use crate::vm::Hypervisor;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VcpuStat {
    pub(crate) tid: i32,
    pub(crate) index: u32,
    pub(crate) cpu_pct: Option<f64>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    prev: HashMap<i32, Sample>,
    clk_tck: u64,
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug, Default)]
pub(crate) struct Tracker;

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct Sample {
    when: Instant,
    jiffies: u64,
}

#[cfg(target_os = "linux")]
impl Tracker {
    pub(crate) fn new(clk_tck: u64) -> Self {
        Self {
            prev: HashMap::new(),
            clk_tck: clk_tck.max(1),
        }
    }

    /// Per-vCPU stats for the given VM pid. Empty when the pid is
    /// gone, the hypervisor doesn't expose vCPU threads via `comm`,
    /// or `/proc/<pid>/task` isn't readable. Caller decides what to
    /// render — typically the detail pane when a VM is selected.
    pub(crate) fn snapshot(&mut self, pid: i32, hv: Hypervisor) -> Vec<VcpuStat> {
        let now = Instant::now();
        let task_dir = format!("/proc/{pid}/task");
        let Ok(entries) = fs::read_dir(&task_dir) else {
            return Vec::new();
        };

        let mut seen: Vec<i32> = Vec::new();
        let mut out: Vec<VcpuStat> = Vec::new();

        for entry in entries.flatten() {
            let Some(tid_str) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(tid) = tid_str.parse::<i32>() else {
                continue;
            };
            let Some(comm) = read_comm(pid, tid) else {
                continue;
            };
            let Some(index) = parse_vcpu_index(&comm, hv) else {
                continue;
            };
            let Some(j) = read_thread_jiffies(pid, tid) else {
                continue;
            };

            seen.push(tid);
            let cpu_pct = self.prev.get(&tid).and_then(|p| {
                let dt = now.duration_since(p.when).as_secs_f64();
                if dt <= 0.0 {
                    return None;
                }
                let dj = j.saturating_sub(p.jiffies);
                #[allow(clippy::cast_precision_loss)]
                let pct = (dj as f64) / (self.clk_tck as f64) / dt * 100.0;
                Some(pct)
            });
            self.prev.insert(
                tid,
                Sample {
                    when: now,
                    jiffies: j,
                },
            );
            out.push(VcpuStat {
                tid,
                index,
                cpu_pct,
            });
        }

        // Drop dead-thread entries so the cache doesn't grow forever
        // across guest reboots.
        self.prev.retain(|tid, _| seen.contains(tid));

        out.sort_by_key(|v| v.index);
        out
    }
}

#[cfg(not(target_os = "linux"))]
impl Tracker {
    pub(crate) fn new(_clk_tck: u64) -> Self {
        Self
    }
    pub(crate) fn snapshot(&mut self, _pid: i32, _hv: Hypervisor) -> Vec<VcpuStat> {
        Vec::new()
    }
}

/// Read `/proc/<pid>/task/<tid>/comm`. Trims trailing newline.
#[cfg(target_os = "linux")]
fn read_comm(pid: i32, tid: i32) -> Option<String> {
    let raw = fs::read_to_string(format!("/proc/{pid}/task/{tid}/comm")).ok()?;
    Some(raw.trim_end_matches('\n').to_string())
}

/// Read `utime + stime` from `/proc/<pid>/task/<tid>/stat`. Same
/// format as `/proc/<pid>/stat`; we split *after* the last `)`
/// because `comm` itself can contain whitespace or parens.
#[cfg(target_os = "linux")]
fn read_thread_jiffies(pid: i32, tid: i32) -> Option<u64> {
    let raw = fs::read_to_string(format!("/proc/{pid}/task/{tid}/stat")).ok()?;
    let rparen = raw.rfind(')')?;
    let after = raw.get(rparen + 2..)?;
    let fields: Vec<&str> = after.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// Map a thread `comm` to its 0-based vCPU index, per hypervisor.
///
/// QEMU writes `CPU N/KVM`, Cloud Hypervisor / lkvm `vcpuN`,
/// Firecracker `fc_vcpu N`, crosvm `crosvm_vcpuN`. `comm` is
/// truncated to 15 chars by the kernel, so very high vCPU indices
/// might tail-clip — we still match the prefix and parse what's
/// left.
pub(crate) fn parse_vcpu_index(comm: &str, hv: Hypervisor) -> Option<u32> {
    let s = comm.trim();
    match hv {
        Hypervisor::Qemu => {
            let rest = s.strip_prefix("CPU ")?;
            let (num, _) = rest.split_once('/')?;
            num.parse().ok()
        }
        Hypervisor::Firecracker => {
            let rest = s.strip_prefix("fc_vcpu")?.trim_start();
            rest.parse().ok()
        }
        Hypervisor::CloudHypervisor | Hypervisor::Lkvm => {
            // Cloud-hypervisor uses `vcpuN`; lkvm uses `kvm-vcpuN`.
            let rest = s
                .strip_prefix("vcpu")
                .or_else(|| s.strip_prefix("kvm-vcpu"))?;
            rest.parse().ok()
        }
        Hypervisor::Crosvm => {
            let rest = s.strip_prefix("crosvm_vcpu")?;
            rest.parse().ok()
        }
        Hypervisor::VMware | Hypervisor::Parallels | Hypervisor::VirtualBox => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_qemu_comm_pattern() {
        assert_eq!(parse_vcpu_index("CPU 0/KVM", Hypervisor::Qemu), Some(0));
        assert_eq!(parse_vcpu_index("CPU 7/KVM", Hypervisor::Qemu), Some(7));
        assert_eq!(parse_vcpu_index("CPU 42/KVM", Hypervisor::Qemu), Some(42));
    }

    #[test]
    fn parses_cloud_hypervisor_comm() {
        assert_eq!(
            parse_vcpu_index("vcpu0", Hypervisor::CloudHypervisor),
            Some(0)
        );
        assert_eq!(
            parse_vcpu_index("vcpu15", Hypervisor::CloudHypervisor),
            Some(15)
        );
    }

    #[test]
    fn parses_firecracker_comm() {
        assert_eq!(
            parse_vcpu_index("fc_vcpu 0", Hypervisor::Firecracker),
            Some(0)
        );
        assert_eq!(
            parse_vcpu_index("fc_vcpu 3", Hypervisor::Firecracker),
            Some(3)
        );
    }

    #[test]
    fn parses_crosvm_comm() {
        assert_eq!(
            parse_vcpu_index("crosvm_vcpu0", Hypervisor::Crosvm),
            Some(0)
        );
        assert_eq!(
            parse_vcpu_index("crosvm_vcpu5", Hypervisor::Crosvm),
            Some(5)
        );
    }

    #[test]
    fn parses_lkvm_comm() {
        assert_eq!(parse_vcpu_index("kvm-vcpu0", Hypervisor::Lkvm), Some(0));
        assert_eq!(parse_vcpu_index("kvm-vcpu7", Hypervisor::Lkvm), Some(7));
    }

    #[test]
    fn rejects_unrelated_threads() {
        // QEMU's main thread, IO threads, etc. shouldn't match.
        assert_eq!(parse_vcpu_index("qemu-system-x86", Hypervisor::Qemu), None);
        assert_eq!(parse_vcpu_index("IO mon_iothread", Hypervisor::Qemu), None);
        assert_eq!(parse_vcpu_index("", Hypervisor::Qemu), None);
    }
}
