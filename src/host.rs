//! host.rs — lightweight host-wide stats for the top overview bar.
//!
//! Linux: parses `/proc`. macOS: uses `sysctl`.
//! All pure reads; snapshot is cheap enough to call on every scan tick.

use crate::errors::ErrorRing;
#[cfg(target_os = "linux")]
use std::fs;

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
    /// system has no swap configured (common on cloud servers).
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

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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
    }
}

// -----------------------------------------------------------------------------
// Pure parsers — kept separate from the fs reads so they can be tested
// against canned fixture strings without root or a Linux kernel.
// -----------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn read_kernel() -> Option<String> {
    fs::read_to_string("/proc/version")
        .ok()
        .and_then(|s| parse_kernel(&s))
}

#[cfg(target_os = "linux")]
fn read_cpu_count() -> usize {
    fs::read_to_string("/proc/cpuinfo").map_or(0, |s| parse_cpu_count(&s))
}

#[cfg(target_os = "linux")]
fn read_cpu_model() -> Option<String> {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| parse_cpu_model(&s))
}

#[cfg(target_os = "linux")]
fn read_meminfo_kb(key: &str) -> Option<u64> {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| parse_meminfo_kb(&s, key))
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "macos")]
#[allow(deprecated)] // libc deprecates `mach_host_self`/`mach_task_self` in favour of the `mach2` crate, but `mach2` doesn't expose `mach_host_self` and these symbols are stable Apple ABI.
mod macos {
    use super::*;

    const CTL_HW: i32 = 6;
    const HW_NCPU: i32 = 3;
    const HW_MEMSIZE: i32 = 24;
    const CTL_KERN: i32 = 1;
    const KERN_OSTYPE: i32 = 1;
    const KERN_OSRELEASE: i32 = 2;
    const KERN_VERSION: i32 = 4;
    const CTL_VM: i32 = 2;
    const VM_LOADAVG: i32 = 2;

    /// SAFETY: `sysctl` is a well-documented POSIX syscall. We pass a
    /// valid single-element MIB, a valid writable pointer with correct
    /// size, and null/0 for the new-value arguments (read-only query).
    unsafe fn sysctl_int(name: i32) -> i32 {
        let mut value: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::size_t;
        let mut mib = [name];
        libc::sysctl(
            mib.as_mut_ptr(),
            1,
            &mut value as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        );
        value
    }

    /// SAFETY: Same rationale as `sysctl_int`; output buffer sized for `u64`.
    unsafe fn sysctl_u64(name: i32) -> u64 {
        let mut value: u64 = 0;
        let mut len = std::mem::size_of::<u64>() as libc::size_t;
        let mut mib = [name];
        libc::sysctl(
            mib.as_mut_ptr(),
            1,
            &mut value as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        );
        value
    }

    /// SAFETY: Two-pass `sysctl`: first call with null output to get the
    /// buffer size, second call with a correctly-sized `Vec<u8>`. The
    /// MIB slice is borrowed for the call duration only.
    unsafe fn sysctl_str(mib: &[i32]) -> String {
        let mut len: libc::size_t = 0;
        let mib_ptr = mib.as_ptr().cast_mut();
        libc::sysctl(
            mib_ptr,
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        );
        if len == 0 {
            return String::new();
        }
        let mut buf = vec![0u8; len];
        libc::sysctl(
            mib_ptr,
            mib.len() as libc::c_uint,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        );
        String::from_utf8_lossy(&buf[..len.saturating_sub(1)]).to_string()
    }

    pub(crate) fn read_cpu_count_macos() -> usize {
        // SAFETY: delegates to `sysctl_int` — see its SAFETY comment.
        unsafe { sysctl_int(HW_NCPU) as usize }
    }

    pub(crate) fn read_mem_total_macos() -> u64 {
        // SAFETY: delegates to `sysctl_u64` — see its SAFETY comment.
        unsafe { sysctl_u64(HW_MEMSIZE) }
    }

    pub(crate) fn read_loadavg_macos() -> (f64, f64, f64) {
        // SAFETY: `sysctl` with a fixed-size stack array; MIB and size are correct.
        unsafe {
            let mut load: [libc::c_double; 3] = [0.0; 3];
            let mut len = std::mem::size_of_val(&load) as libc::size_t;
            let mut mib = [CTL_VM, VM_LOADAVG];
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                load.as_mut_ptr() as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            );
            (load[0], load[1], load[2])
        }
    }

    pub(crate) fn read_kernel_macos() -> String {
        // SAFETY: delegates to `sysctl_str` — see its SAFETY comment.
        unsafe {
            let os_type = sysctl_str(&[CTL_KERN, KERN_OSTYPE]);
            let os_release = sysctl_str(&[CTL_KERN, KERN_OSRELEASE]);
            format!("{} {}", os_type, os_release)
        }
    }

    pub(crate) struct MemStats {
        pub(crate) free: u64,
        pub(crate) avail: u64,
        pub(crate) cached: u64,
    }

    /// Sample per-CPU and aggregate tick counters via
    /// `host_processor_info(PROCESSOR_CPU_LOAD_INFO)`. Each logical
    /// CPU yields four counters (USER, SYSTEM, IDLE, NICE); we
    /// derive `idle` and `total` from those exactly the same way
    /// the Linux `/proc/stat` path does so the shared `delta_pct`
    /// works unchanged.
    pub(crate) fn read_cpu_samples_macos(errors: &mut ErrorRing) -> CpuSamples {
        let mut cpu_count: libc::natural_t = 0;
        let mut info_array: libc::processor_info_array_t = std::ptr::null_mut();
        let mut info_count: libc::mach_msg_type_number_t = 0;

        // SAFETY: `host_processor_info` writes a Mach-allocated
        // array we own. We must `vm_deallocate` it on success.
        let kr = unsafe {
            libc::host_processor_info(
                libc::mach_host_self(),
                libc::PROCESSOR_CPU_LOAD_INFO,
                &mut cpu_count,
                &mut info_array,
                &mut info_count,
            )
        };
        if kr != 0 || info_array.is_null() || cpu_count == 0 {
            errors.push("host", "host_processor_info: kr != 0");
            return CpuSamples::default();
        }

        let states = libc::CPU_STATE_MAX as usize;
        let total_slots = info_count as usize;
        let cpus = cpu_count as usize;

        let mut per_core: Vec<CpuSample> = Vec::with_capacity(cpus);
        let mut agg_idle: u64 = 0;
        let mut agg_total: u64 = 0;

        // SAFETY: `info_array` points to `total_slots` valid
        // `integer_t` (i32) values. We read `cpus * states` of
        // them; the kernel guarantees this layout.
        unsafe {
            for i in 0..cpus {
                let base = i * states;
                if base + states > total_slots {
                    break;
                }
                let user = u64::from(*info_array.add(base + libc::CPU_STATE_USER as usize) as u32);
                let sys = u64::from(*info_array.add(base + libc::CPU_STATE_SYSTEM as usize) as u32);
                let idle = u64::from(*info_array.add(base + libc::CPU_STATE_IDLE as usize) as u32);
                let nice = u64::from(*info_array.add(base + libc::CPU_STATE_NICE as usize) as u32);
                let total = user + sys + idle + nice;
                per_core.push(CpuSample { idle, total });
                agg_idle += idle;
                agg_total += total;
            }

            // Release the Mach-allocated buffer.
            let bytes = (total_slots * std::mem::size_of::<libc::integer_t>()) as libc::vm_size_t;
            let _ = libc::vm_deallocate(
                libc::mach_task_self(),
                info_array as libc::vm_address_t,
                bytes,
            );
        }

        CpuSamples {
            aggregate: Some(CpuSample {
                idle: agg_idle,
                total: agg_total,
            }),
            per_core,
        }
    }

    /// Read VM page statistics via `host_statistics64(HOST_VM_INFO64)`
    /// and convert page counts to bytes using `vm_page_size`.
    /// Returns `None` if the call fails.
    pub(crate) fn read_vm_stats_macos() -> Option<MemStats> {
        let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
        let mut count: libc::mach_msg_type_number_t = libc::HOST_VM_INFO64_COUNT;

        // SAFETY: standard `host_statistics64` call with a
        // correctly-sized output struct.
        let kr = unsafe {
            libc::host_statistics64(
                libc::mach_host_self(),
                libc::HOST_VM_INFO64,
                std::ptr::addr_of_mut!(stats).cast(),
                &mut count,
            )
        };
        if kr != 0 {
            return None;
        }

        // SAFETY: `vm_page_size` is a process-global constant the
        // kernel sets at startup. Always valid to read.
        let page_size = unsafe { libc::vm_page_size } as u64;

        let free = u64::from(stats.free_count) * page_size;
        let inactive = u64::from(stats.inactive_count) * page_size;
        let speculative = u64::from(stats.speculative_count) * page_size;
        let purgeable = u64::from(stats.purgeable_count) * page_size;
        let external = u64::from(stats.external_page_count) * page_size;

        // Activity Monitor's "cached files" ≈ external (file-backed).
        // Reclaimable ≈ inactive + speculative + purgeable + external.
        let cached = inactive
            .saturating_add(speculative)
            .saturating_add(external);
        // Available ≈ free + reclaimable. Mirrors what `MemAvailable`
        // approximates on Linux.
        let avail = free
            .saturating_add(inactive)
            .saturating_add(speculative)
            .saturating_add(purgeable);

        Some(MemStats {
            free,
            avail,
            cached,
        })
    }

    /// Read swap totals via `sysctlbyname("vm.swapusage")`. Returns
    /// `(total_bytes, free_bytes)`. `None` if swap is unavailable.
    pub(crate) fn read_swap_macos() -> Option<(u64, u64)> {
        #[repr(C)]
        #[derive(Default, Copy, Clone)]
        struct XswUsage {
            xsu_total: u64,
            xsu_avail: u64,
            xsu_used: u64,
            xsu_pagesize: u32,
            xsu_encrypted: u8,
        }
        let mut usage = XswUsage::default();
        let mut size = std::mem::size_of::<XswUsage>() as libc::size_t;
        let name = b"vm.swapusage\0";
        // SAFETY: standard `sysctlbyname` query into a correctly
        // sized POD struct.
        let kr = unsafe {
            libc::sysctlbyname(
                name.as_ptr().cast(),
                std::ptr::addr_of_mut!(usage).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if kr != 0 {
            return None;
        }
        Some((usage.xsu_total, usage.xsu_avail))
    }

    pub(crate) fn read_cpu_model_macos() -> String {
        // SAFETY: two-pass `sysctl` with a correctly-sized `Vec<u8>` buffer.
        unsafe {
            let mut len: libc::size_t = 0;
            let mut mib = [CTL_HW, 0x10000002u32 as i32]; // HW_MACHINE
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            );
            let mut buf = vec![0u8; len];
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            );
            String::from_utf8_lossy(&buf[..len.saturating_sub(1)]).to_string()
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn read_cpu_samples(errors: &mut ErrorRing) -> CpuSamples {
    macos::read_cpu_samples_macos(errors)
}

#[cfg(target_os = "macos")]
pub(crate) fn snapshot(prev: Option<&CpuSamples>, errors: &mut ErrorRing) -> HostInfo {
    use macos::*;

    let cur = read_cpu_samples_macos(errors);

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

    let cpu_count = read_cpu_count_macos();
    let mem_total = read_mem_total_macos();
    let mem = read_vm_stats_macos().unwrap_or(MemStats {
        free: mem_total / 4,
        avail: mem_total / 2,
        cached: mem_total / 4,
    });
    let (swap_total, swap_free) = read_swap_macos().unwrap_or((0, 0));
    let loads = read_loadavg_macos();
    let kernel = read_kernel_macos();
    let cpu_model = read_cpu_model_macos();

    HostInfo {
        kernel,
        cpu_count,
        cpu_model,
        mem_total_bytes: mem_total,
        mem_avail_bytes: mem.avail,
        mem_free_bytes: mem.free,
        mem_buffers_bytes: 0,
        mem_cached_bytes: mem.cached,
        swap_total_bytes: swap_total,
        swap_free_bytes: swap_free,
        loadavg_1: loads.0,
        loadavg_5: loads.1,
        loadavg_15: loads.2,
        cpu_pct,
        per_core_pct,
    }
}
