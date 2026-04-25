//! host.rs — lightweight host-wide stats for the top overview bar.
//!
//! Everything here is parsed from `/proc` + a couple of stats on
//! `/dev/kvm`. All pure reads; snapshot is cheap enough to call on
//! every scan tick (sub-millisecond on a modern `NVMe`).

use std::fs;
use std::path::Path;

use crate::errors::ErrorRing;

#[derive(Debug, Clone)]
pub(crate) struct HostInfo {
    pub(crate) kernel: String,
    #[allow(dead_code)] // shown in title bar of a future per-core view
    pub(crate) cpu_count: usize,
    pub(crate) cpu_model: String,
    pub(crate) mem_total_bytes: u64,
    pub(crate) mem_avail_bytes: u64,
    /// `MemFree` from `/proc/meminfo`, in bytes. The *truly* free
    /// memory (not held by the page cache) \u2014 used as the rightmost
    /// segment of the memory composition bar.
    pub(crate) mem_free_bytes: u64,
    /// `Buffers` from `/proc/meminfo`, in bytes. Memory the kernel
    /// is using to back block-I/O queues. Reclaimable.
    pub(crate) mem_buffers_bytes: u64,
    /// `Cached` from `/proc/meminfo`, in bytes. The page cache;
    /// memory holding recent file-system reads. Reclaimable. Shown
    /// as the third segment of the composition bar so the user can
    /// see at a glance how much "memory pressure" is real and how
    /// much is just page cache that will evaporate the moment
    /// anything needs it.
    pub(crate) mem_cached_bytes: u64,
    /// `SwapTotal` from `/proc/meminfo`, in bytes. `0` when the
    /// system has no swap configured (common on servers, microVMs).
    pub(crate) swap_total_bytes: u64,
    /// `SwapFree` from `/proc/meminfo`, in bytes.
    pub(crate) swap_free_bytes: u64,
    /// 1-minute load average, e.g. `0.42`. The 5- and 15-minute
    /// figures contextualise it: `0.42 0.30 0.25` says "low load
    /// trending down"; `5.0 0.5 0.2` says "a fresh fire".
    pub(crate) loadavg_1: f64,
    pub(crate) loadavg_5: f64,
    pub(crate) loadavg_15: f64,
    /// Host CPU% across all cores, computed from two `/proc/stat`
    /// samples. `None` until we have two data points.
    pub(crate) cpu_pct: Option<f64>,
    /// Per-core CPU% in physical core order. Same `None` semantics as
    /// `cpu_pct`. Length may be empty on first call.
    pub(crate) per_core_pct: Vec<f64>,
    /// Is `/dev/kvm` present and accessible? This drives a red/green
    /// indicator — if it's gone, nothing in neosandbox works.
    pub(crate) kvm_available: bool,
}

/// Monotonically accumulating CPU time, read from a `cpu`/`cpuN` line
/// of `/proc/stat`. We keep previous samples (both aggregate and
/// per-core) to compute %CPU as a delta.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CpuSample {
    pub(crate) idle: u64,
    pub(crate) total: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CpuSamples {
    pub(crate) aggregate: Option<CpuSample>,
    pub(crate) per_core: Vec<CpuSample>,
}

pub(crate) fn read_cpu_samples(errors: &mut ErrorRing) -> CpuSamples {
    match fs::read_to_string("/proc/stat") {
        Ok(r) => parse_cpu_samples(&r),
        Err(e) => {
            errors.push("host", format!("/proc/stat: {e}"));
            CpuSamples::default()
        }
    }
}

/// Pure parser for `/proc/stat`. Splits the aggregate `cpu  ...` line
/// from per-core `cpuN ...` lines and stops as soon as we leave the
/// CPU section (kernel guarantees those are first). Lines with fewer
/// than 5 numeric fields are ignored — same shape kernels older than
/// 2.6.x had, which we don't bother to support.
pub(crate) fn parse_cpu_samples(raw: &str) -> CpuSamples {
    let mut agg: Option<CpuSample> = None;
    let mut per_core: Vec<CpuSample> = Vec::new();

    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("cpu") {
            // Two cases: the aggregate line `cpu  ...` (extra space),
            // and per-core lines `cpu0 ...`, `cpu1 ...`, etc.
            let (is_aggregate, nums) = if let Some(nums) = rest.strip_prefix(' ') {
                (true, nums.trim_start())
            } else if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                // Peel off the digits (core index) to reach the fields.
                let fields = rest.trim_start_matches(|c: char| c.is_ascii_digit());
                (false, fields.trim_start())
            } else {
                continue;
            };

            let parts: Vec<u64> = nums
                .split_whitespace()
                .filter_map(|s| s.parse().ok())
                .collect();
            if parts.len() < 5 {
                continue;
            }
            let idle = parts[3] + parts[4];
            let total: u64 = parts.iter().sum();
            let sample = CpuSample { idle, total };
            if is_aggregate {
                agg = Some(sample);
            } else {
                per_core.push(sample);
            }
        } else if agg.is_some() {
            // Once we're past the cpu lines we can stop — they're
            // always first in `/proc/stat`.
            break;
        }
    }
    CpuSamples {
        aggregate: agg,
        per_core,
    }
}

fn delta_pct(prev: CpuSample, cur: CpuSample) -> Option<f64> {
    if cur.total <= prev.total {
        return None;
    }
    let dtotal = cur.total - prev.total;
    let didle = cur.idle.saturating_sub(prev.idle);
    let busy = dtotal.saturating_sub(didle);
    #[allow(clippy::cast_precision_loss)]
    let pct = (busy as f64 / dtotal as f64) * 100.0;
    Some(pct)
}

pub(crate) fn snapshot(prev: Option<&CpuSamples>, errors: &mut ErrorRing) -> HostInfo {
    let cur = read_cpu_samples(errors);

    let cpu_pct = match (prev.and_then(|p| p.aggregate), cur.aggregate) {
        (Some(p), Some(c)) => delta_pct(p, c),
        _ => None,
    };

    let per_core_pct: Vec<f64> = if let Some(prev) = prev {
        cur.per_core
            .iter()
            .zip(prev.per_core.iter())
            .filter_map(|(c, p)| delta_pct(*p, *c))
            .collect()
    } else {
        Vec::new()
    };

    let kernel = read_kernel().unwrap_or_else(|| {
        errors.push("host", "/proc/version unreadable");
        "unknown".into()
    });
    let cpu_model = read_cpu_model().unwrap_or_else(|| {
        errors.push("host", "/proc/cpuinfo: no model name");
        "unknown".into()
    });
    let mem_total_bytes = read_meminfo_kb("MemTotal:").map_or_else(
        || {
            errors.push("host", "/proc/meminfo: missing MemTotal");
            0
        },
        |kb| kb * 1024,
    );
    let mem_avail_bytes = read_meminfo_kb("MemAvailable:").map_or(0, |kb| kb * 1024);
    // The composition triple. None of these are individually fatal:
    // an old kernel without `Buffers` reports 0, the bar renderer
    // gracefully reduces to "used / cached / free" without it.
    let mem_free_bytes = read_meminfo_kb("MemFree:").map_or(0, |kb| kb * 1024);
    let mem_buffers_bytes = read_meminfo_kb("Buffers:").map_or(0, |kb| kb * 1024);
    let mem_cached_bytes = read_meminfo_kb("Cached:").map_or(0, |kb| kb * 1024);
    // Swap is optional — a missing key isn't an error, just means the
    // system has no swap configured. We don't push to the error ring.
    let swap_total_bytes = read_meminfo_kb("SwapTotal:").map_or(0, |kb| kb * 1024);
    let swap_free_bytes = read_meminfo_kb("SwapFree:").map_or(0, |kb| kb * 1024);
    let loads = read_loadavg().unwrap_or_else(|| {
        errors.push("host", "/proc/loadavg unreadable");
        (0.0, 0.0, 0.0)
    });

    HostInfo {
        kernel,
        cpu_count: read_cpu_count(),
        cpu_model,
        mem_total_bytes,
        mem_avail_bytes,
        mem_free_bytes,
        mem_buffers_bytes,
        mem_cached_bytes,
        swap_total_bytes,
        swap_free_bytes,
        loadavg_1: loads.0,
        loadavg_5: loads.1,
        loadavg_15: loads.2,
        cpu_pct,
        per_core_pct,
        kvm_available: Path::new("/dev/kvm").exists(),
    }
}

// -----------------------------------------------------------------------------
// Pure parsers — kept separate from the fs reads so they can be tested
// against canned fixture strings without root or a Linux kernel.
// -----------------------------------------------------------------------------

fn read_kernel() -> Option<String> {
    fs::read_to_string("/proc/version")
        .ok()
        .and_then(|s| parse_kernel(&s))
}

fn read_cpu_count() -> usize {
    fs::read_to_string("/proc/cpuinfo")
        .map(|s| parse_cpu_count(&s))
        .unwrap_or(0)
}

fn read_cpu_model() -> Option<String> {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| parse_cpu_model(&s))
}

fn read_meminfo_kb(key: &str) -> Option<u64> {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| parse_meminfo_kb(&s, key))
}

fn read_loadavg() -> Option<(f64, f64, f64)> {
    fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| parse_loadavg(&s))
}

/// `/proc/version` looks like `Linux version 6.11.2-arch1-1 (...) ...`;
/// we keep the bare version field.
pub(crate) fn parse_kernel(raw: &str) -> Option<String> {
    raw.split_whitespace().nth(2).map(str::to_string)
}

pub(crate) fn parse_cpu_count(raw: &str) -> usize {
    raw.lines().filter(|l| l.starts_with("processor")).count()
}

/// Pull the first `model name : ...` line out of `/proc/cpuinfo` and
/// strip the noisy "(R)"/"(TM)" trademark markers + collapse runs of
/// whitespace.
pub(crate) fn parse_cpu_model(raw: &str) -> Option<String> {
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("model name") {
            return rest.split_once(':').map(|(_, v)| {
                let trimmed = v.trim().replace("(R)", "").replace("(TM)", "");
                let mut out = String::with_capacity(trimmed.len());
                let mut prev_space = false;
                for c in trimmed.chars() {
                    let is_space = c.is_whitespace();
                    if is_space && prev_space {
                        continue;
                    }
                    out.push(c);
                    prev_space = is_space;
                }
                out.trim().to_string()
            });
        }
    }
    None
}

pub(crate) fn parse_meminfo_kb(raw: &str, key: &str) -> Option<u64> {
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

/// Parse the three load averages out of `/proc/loadavg`. The file's
/// shape is `LOAD_1 LOAD_5 LOAD_15 RUNNING/TOTAL LATEST_PID`; we
/// only look at the first three fields. Returns `None` if any of
/// them is missing or unparseable — we don't pretend a partial
/// result is meaningful.
pub(crate) fn parse_loadavg(raw: &str) -> Option<(f64, f64, f64)> {
    let mut it = raw.split_whitespace();
    let one: f64 = it.next()?.parse().ok()?;
    let five: f64 = it.next()?.parse().ok()?;
    let fifteen: f64 = it.next()?.parse().ok()?;
    Some((one, five, fifteen))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT_FIXTURE: &str = "\
cpu  3357 0 4313 1362393 9 0 0 0 0 0
cpu0 1839 0 2090 681284 5 0 0 0 0 0
cpu1 1518 0 2223 681108 3 0 0 0 0 0
intr 1234567
ctxt 2345678
btime 1700000000
";

    #[test]
    fn parse_cpu_samples_aggregate_and_cores() {
        let s = parse_cpu_samples(STAT_FIXTURE);
        let agg = s.aggregate.expect("aggregate present");
        // total = 3357 + 4313 + 1362393 + 9 = 1370072 (the line above)
        assert_eq!(agg.total, 3357 + 4313 + 1_362_393 + 9);
        // idle = parts[3] + parts[4] = 1_362_393 + 9
        assert_eq!(agg.idle, 1_362_393 + 9);
        assert_eq!(s.per_core.len(), 2);
    }

    #[test]
    fn parse_cpu_samples_handles_empty() {
        let s = parse_cpu_samples("");
        assert!(s.aggregate.is_none());
        assert!(s.per_core.is_empty());
    }

    #[test]
    fn parse_cpu_samples_skips_short_lines() {
        // <5 numeric fields → ignored.
        let s = parse_cpu_samples("cpu 1 2 3\n");
        assert!(s.aggregate.is_none());
    }

    const MEMINFO_FIXTURE: &str = "\
MemTotal:       16374804 kB
MemFree:         3221408 kB
MemAvailable:    9876543 kB
Buffers:           45224 kB
";

    #[test]
    fn parse_meminfo_kb_finds_keys() {
        assert_eq!(
            parse_meminfo_kb(MEMINFO_FIXTURE, "MemTotal:"),
            Some(16_374_804)
        );
        assert_eq!(
            parse_meminfo_kb(MEMINFO_FIXTURE, "MemAvailable:"),
            Some(9_876_543)
        );
    }

    #[test]
    fn parse_meminfo_kb_returns_none_for_missing() {
        assert_eq!(parse_meminfo_kb(MEMINFO_FIXTURE, "NotAKey:"), None);
    }

    #[test]
    fn parse_loadavg_extracts_all_three_windows() {
        let (one, five, fifteen) = parse_loadavg("0.42 0.30 0.20 1/256 12345").unwrap();
        assert!((one - 0.42).abs() < 1e-9);
        assert!((five - 0.30).abs() < 1e-9);
        assert!((fifteen - 0.20).abs() < 1e-9);
    }

    #[test]
    fn parse_loadavg_rejects_garbage() {
        assert_eq!(parse_loadavg(""), None);
        assert_eq!(parse_loadavg("not a number"), None);
        // Two fields where three are required — reject rather than
        // silently zero-fill.
        assert_eq!(parse_loadavg("0.42 0.30"), None);
    }

    #[test]
    fn parse_kernel_extracts_version_field() {
        let raw = "Linux version 6.11.2-arch1-1 (linux@archlinux) (gcc 14) #1 SMP";
        assert_eq!(parse_kernel(raw).as_deref(), Some("6.11.2-arch1-1"));
    }

    #[test]
    fn parse_cpu_count_counts_processor_lines() {
        let raw = "\
processor       : 0
vendor_id       : GenuineIntel
processor       : 1
vendor_id       : GenuineIntel
processor       : 2
";
        assert_eq!(parse_cpu_count(raw), 3);
    }

    #[test]
    fn parse_cpu_model_strips_trademark_and_whitespace() {
        let raw = "\
processor       : 0
model name      : Intel(R) Core(TM) i7-1185G7 @ 3.00GHz
cache size      : 12288 KB
";
        assert_eq!(
            parse_cpu_model(raw).as_deref(),
            Some("Intel Core i7-1185G7 @ 3.00GHz")
        );
    }

    #[test]
    fn parse_cpu_model_returns_none_when_absent() {
        assert_eq!(parse_cpu_model("processor : 0\n"), None);
    }
}
