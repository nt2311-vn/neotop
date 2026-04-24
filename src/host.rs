//! host.rs — lightweight host-wide stats for the top overview bar.
//!
//! Everything here is parsed from `/proc` + a couple of stats on
//! `/dev/kvm`. All pure reads; snapshot is cheap enough to call on
//! every scan tick (sub-millisecond on a modern `NVMe`).

use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub(crate) struct HostInfo {
    pub(crate) kernel: String,
    #[allow(dead_code)] // shown in title bar of a future per-core view
    pub(crate) cpu_count: usize,
    pub(crate) cpu_model: String,
    pub(crate) mem_total_bytes: u64,
    pub(crate) mem_avail_bytes: u64,
    /// 1-minute load average, e.g. `0.42`.
    pub(crate) loadavg_1: f64,
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

pub(crate) fn read_cpu_samples() -> CpuSamples {
    let Ok(raw) = fs::read_to_string("/proc/stat") else {
        return CpuSamples::default();
    };
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
        } else {
            // Once we're past the cpu lines, we can stop — they're
            // always first in `/proc/stat`.
            if agg.is_some() {
                break;
            }
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

pub(crate) fn snapshot(prev: Option<&CpuSamples>) -> HostInfo {
    let cur = read_cpu_samples();

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

    HostInfo {
        kernel: read_kernel().unwrap_or_else(|| "unknown".into()),
        cpu_count: read_cpu_count(),
        cpu_model: read_cpu_model().unwrap_or_else(|| "unknown".into()),
        mem_total_bytes: read_meminfo_kb("MemTotal:").map_or(0, |kb| kb * 1024),
        mem_avail_bytes: read_meminfo_kb("MemAvailable:").map_or(0, |kb| kb * 1024),
        loadavg_1: read_loadavg().unwrap_or(0.0),
        cpu_pct,
        per_core_pct,
        kvm_available: Path::new("/dev/kvm").exists(),
    }
}

// -----------------------------------------------------------------------------

fn read_kernel() -> Option<String> {
    // /proc/version is e.g. "Linux version 6.11.2-arch1-1 (...) (gcc ...) ..."
    let raw = fs::read_to_string("/proc/version").ok()?;
    // Just the version field for compactness.
    raw.split_whitespace().nth(2).map(str::to_string)
}

fn read_cpu_count() -> usize {
    fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.lines().filter(|l| l.starts_with("processor")).count())
        .unwrap_or(0)
}

fn read_cpu_model() -> Option<String> {
    let raw = fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("model name") {
            return rest.split_once(':').map(|(_, v)| {
                let v = v.trim();
                // Collapse "(R)" / "(TM)" / repeated spaces to a compact
                // label. These strings are long; the overview bar has
                // maybe 60 cols for the model name.
                let trimmed = v.replace("(R)", "").replace("(TM)", "");
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

fn read_meminfo_kb(key: &str) -> Option<u64> {
    let raw = fs::read_to_string("/proc/meminfo").ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

fn read_loadavg() -> Option<f64> {
    let raw = fs::read_to_string("/proc/loadavg").ok()?;
    raw.split_whitespace().next()?.parse().ok()
}
