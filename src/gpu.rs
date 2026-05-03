//! gpu.rs — GPU detection and activity reporting.
//! Linux: `/sys/class/drm` + NVML. macOS: requires IOKit/Metal (not implemented).

use std::collections::HashMap;
use std::fs;
#[cfg(all(target_os = "linux", feature = "i915-pmu"))]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Per-engine utilisation percentages for Intel i915 GPUs.
///
/// Each engine variant is `None` when absent on this GPU model
/// (e.g. GT1 iGPUs have no video engines).  `CapDenied` is returned
/// when `perf_event_open` was refused — the caller shows a hint.
#[derive(Debug, Clone)]
pub(crate) enum IntelEngines {
    /// `perf_event_open` was denied; `CAP_PERFMON` or root is needed.
    CapDenied,
    /// Successfully-read engine utilisation percentages.
    Busy {
        rcs: Option<f64>,  // Render Command Streamer (3D / compute)
        bcs: Option<f64>,  // Blitter Command Streamer
        vcs: Option<f64>,  // Video Codec Streamer
        vecs: Option<f64>, // Video Enhancement Command Streamer
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GpuVendor {
    Amd,
    Nvidia,
    Intel,
    Other,
}

impl GpuVendor {
    /// Map a PCI vendor ID (as written in `/sys/.../device/vendor`,
    /// hex prefixed with `0x`) to a known vendor enum. Anything we
    /// don't recognise falls through to `Other` and gets shown as
    /// `gpu unknown` in the overview rather than panicking.
    pub(crate) fn from_pci_id(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "0x1002" | "0x1022" => Self::Amd, // ATI legacy + AMD GPU
            "0x10de" => Self::Nvidia,
            "0x8086" => Self::Intel,
            _ => Self::Other,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Amd => "AMD",
            Self::Nvidia => "NVIDIA",
            Self::Intel => "Intel",
            Self::Other => "GPU",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Gpu {
    /// Stored for future per-vendor render logic (e.g. NVIDIA-only
    /// VRAM ECC counters). Not yet read by the UI — the host
    /// overview routes off `name` and the metric `Option`s.
    #[allow(dead_code)]
    pub(crate) vendor: GpuVendor,
    /// Short device name. Best-effort: comes from `/sys/.../uevent`
    /// `PCI_ID=...` lookup or just falls back to the vendor label.
    pub(crate) name: String,
    /// 0..=100 utilisation percentage. `None` when:
    /// * the card's vendor backend isn't implemented yet (NVIDIA,
    ///   Intel, Other); or
    /// * the kernel didn't expose `gpu_busy_percent` (older drivers).
    pub(crate) busy_pct: Option<f64>,
    /// Bytes currently mapped into VRAM. `0` when unknown.
    pub(crate) vram_used: u64,
    /// Bytes of VRAM total. `0` when unknown — `vram_used / 0` is
    /// suppressed at the display layer rather than NaN-ing.
    pub(crate) vram_total: u64,
    /// Instantaneous draw in watts. `None` when no `power1_average`
    /// hwmon node was found under the card.
    pub(crate) power_watts: Option<f64>,
    /// Canonical PCI bus address (`00000000:01:00.0`). Used to
    /// match sysfs-discovered NVIDIA cards against the NVML
    /// `Device` list. `None` when sysfs's `device` symlink wasn't
    /// resolvable — rare; the merge step falls back to detect-only
    /// behaviour for those.
    #[allow(dead_code)]
    pub(crate) pci_addr: Option<String>,
    /// Per-engine breakdown for Intel i915 GPUs.  `None` for all
    /// non-Intel cards and when the `i915-pmu` feature is disabled.
    pub(crate) intel_engines: Option<IntelEngines>,
}

impl Gpu {
    pub(crate) fn vram_pct(&self) -> Option<f64> {
        if self.vram_total == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some((self.vram_used as f64 / self.vram_total as f64) * 100.0)
    }

    /// Returns `true` when this card has at least one piece of
    /// real telemetry to display. NVIDIA / Intel cards we *detect*
    /// but don't have backends for are still useful for the line-1
    /// overview (so the user knows the card is recognised), but we
    /// don't waste a sparkline column on them.
    pub(crate) fn has_busy_data(&self) -> bool {
        self.busy_pct.is_some()
    }
}

impl Gpu {
    #[allow(dead_code)]
    pub(crate) fn has_intel_engines(&self) -> bool {
        matches!(&self.intel_engines, Some(IntelEngines::Busy { .. }))
    }
}

/// State of the lazy NVML init. We try once on the first
/// `snapshot()` that finds an NVIDIA card; on success we keep the
/// handle for the lifetime of the process; on failure we remember
/// it failed and never retry. Re-initialising NVML costs ~50-100 ms
/// per call, so caching matters even at the slow tick.
#[cfg(feature = "nvml")]
#[derive(Default)]
enum NvmlState {
    #[default]
    Uninit,
    Failed,
    // Boxed so the enum stays compact even though `Nvml` itself is
    // a few hundred bytes of NVML symbol pointers. This is the
    // `clippy::large_enum_variant` story: without the box the enum
    // is sized to the largest variant on every `Tracker`.
    Ready(Box<nvml_wrapper::Nvml>),
}

// `nvml_wrapper::Nvml` doesn't impl Debug, so derive a manual one
// that just prints the variant name. The other paths through
// `Tracker::Debug` print "Tracker { nvml: <state> }" which is
// fine for our `--diag` output.
#[cfg(feature = "nvml")]
impl std::fmt::Debug for NvmlState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Uninit => "Uninit",
            Self::Failed => "Failed",
            Self::Ready(_) => "Ready",
        };
        f.write_str(s)
    }
}

/// Stateful tracker. Holds the (lazily-initialised) NVML handle so
/// repeated snapshots reuse the same library context — `Nvml::init`
/// is itself a 50-100 ms call we don't want on the hot path. AMD's
/// sysfs reads are stateless and don't need caching.
///
/// `intel_state` keeps the previous RC6 residency sample per
/// Intel card (keyed by canonical PCI address). Busy% is the
/// derivative of that counter against wall-clock — needs the prev
/// sample to compute, so we cache it here for the lifetime of the
/// process and purge entries for cards that have disappeared.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    #[cfg(feature = "nvml")]
    nvml: NvmlState,
    intel_state: HashMap<String, IntelSample>,
    #[cfg(all(target_os = "linux", feature = "i915-pmu"))]
    intel_pmu: IntelPmuTracker,
}

/// Per-Intel-card sample for RC6 derivative. Tiny (24 B) so the
/// `HashMap` stays cheap even on multi-iGPU laptops.
#[derive(Debug, Clone, Copy)]
struct IntelSample {
    when: Instant,
    /// Cumulative milliseconds spent in RC6 power-save state since
    /// driver load. Read from
    /// `/sys/class/drm/card*/gt/gt0/rc6_residency_ms`.
    rc6_ms: u64,
}

impl Tracker {
    // `clippy::unused_self`: when the `nvml` feature is off, this
    // method genuinely doesn't read `self` — but with the default
    // feature on it does. Using `&mut self` unconditionally keeps
    // the call site stable across feature combinations.
    #[cfg_attr(not(feature = "nvml"), allow(clippy::unused_self))]
    pub(crate) fn snapshot(&mut self) -> Vec<Gpu> {
        #[cfg(target_os = "linux")]
        {
            let mut out = scan_sysfs_cards();
            #[cfg(feature = "nvml")]
            self.merge_nvml(&mut out);
            self.merge_intel(&mut out);
            #[cfg(feature = "i915-pmu")]
            self.intel_pmu.merge(&mut out);
            out
        }
        #[cfg(target_os = "macos")]
        {
            Vec::new()
        }
    }

    /// For every Intel sysfs card, read RC6 residency and convert
    /// the delta against the previous tick into a busy %. The
    /// first sample seeds the cache and leaves `busy_pct = None`;
    /// every subsequent tick produces a real number. Cards that
    /// disappear between ticks have their cache entry dropped.
    fn merge_intel(&mut self, gpus: &mut [Gpu]) {
        if !gpus.iter().any(|g| g.vendor == GpuVendor::Intel) {
            return;
        }
        let now = Instant::now();
        let pci_to_card = build_intel_card_map();
        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(gpus.len());
        for g in gpus.iter_mut() {
            if g.vendor != GpuVendor::Intel {
                continue;
            }
            let Some(addr) = g.pci_addr.clone() else {
                continue;
            };
            seen.insert(addr.clone());
            let Some(card_path) = pci_to_card.get(&addr) else {
                continue;
            };
            let Some(rc6_ms) = read_intel_rc6_ms(card_path) else {
                continue;
            };
            let cur = IntelSample { when: now, rc6_ms };
            if let Some(prev) = self.intel_state.get(&addr).copied() {
                g.busy_pct = compute_intel_busy_pct(prev, cur);
            }
            self.intel_state.insert(addr, cur);
        }
        self.intel_state.retain(|k, _| seen.contains(k));
    }

    /// For each NVIDIA card we found via sysfs, replace the
    /// detect-only entry with a real NVML reading when possible.
    /// Cards that are in sysfs but not in NVML's view (driver
    /// suspended, hot-disabled) are left as-is so the host
    /// overview still shows "the card exists".
    #[cfg(feature = "nvml")]
    fn merge_nvml(&mut self, gpus: &mut [Gpu]) {
        // Skip entirely when there are no NVIDIA cards — saves the
        // 50-100 ms NVML init cost on AMD-only / Intel-only boxes.
        if !gpus.iter().any(|g| g.vendor == GpuVendor::Nvidia) {
            return;
        }
        let Some(nvml) = self.ensure_nvml() else {
            return;
        };
        let pci_to_idx = build_nvml_pci_map(nvml);
        if pci_to_idx.is_empty() {
            return;
        }
        for g in gpus.iter_mut() {
            if g.vendor != GpuVendor::Nvidia {
                continue;
            }
            // We stored the canonical PCI addr alongside the card
            // when we walked sysfs. If we didn't manage to read it
            // (rare; would need a permission error on the symlink),
            // we just leave the card as detect-only.
            let Some(addr) = &g.pci_addr else { continue };
            let Some(&idx) = pci_to_idx.get(addr) else {
                continue;
            };
            let Ok(dev) = nvml.device_by_index(idx) else {
                continue;
            };
            *g = read_nvml_device(&dev, g.pci_addr.clone());
        }
    }

    #[cfg(feature = "nvml")]
    fn ensure_nvml(&mut self) -> Option<&nvml_wrapper::Nvml> {
        if matches!(self.nvml, NvmlState::Uninit) {
            self.nvml = match nvml_wrapper::Nvml::init() {
                Ok(n) => NvmlState::Ready(Box::new(n)),
                Err(_) => NvmlState::Failed,
            };
        }
        match &self.nvml {
            NvmlState::Ready(n) => Some(n.as_ref()),
            _ => None,
        }
    }
}

/// Walk `/sys/class/drm` once and produce the baseline `Gpu` list.
/// Every detected card is in here; NVIDIA / Intel start as detect-
/// only and may be promoted by the NVML merge pass.
fn scan_sysfs_cards() -> Vec<Gpu> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        // Only `cardN` nodes — skip connector subdirs like
        // `card1-DP-1` (those have `-` in them) and the
        // `renderD128` dri-render device which doesn't carry
        // GPU stats.
        if !is_real_card_node(name) {
            continue;
        }
        let dev = e.path().join("device");
        if let Some(g) = read_one(&dev) {
            out.push(g);
        }
    }
    out
}

fn is_real_card_node(name: &str) -> bool {
    name.starts_with("card") && !name.contains('-') && !name.starts_with("cardD")
}

fn read_one(dev: &Path) -> Option<Gpu> {
    let vendor_raw = fs::read_to_string(dev.join("vendor")).ok()?;
    let vendor = GpuVendor::from_pci_id(&vendor_raw);
    let name = device_label(dev, vendor);
    let pci_addr = sysfs_pci_addr(dev);

    match vendor {
        GpuVendor::Amd => Some(read_amd(dev, name, pci_addr)),
        GpuVendor::Nvidia | GpuVendor::Intel | GpuVendor::Other => Some(Gpu {
            vendor,
            name,
            busy_pct: None,
            vram_used: 0,
            vram_total: 0,
            power_watts: None,
            pci_addr,
            intel_engines: None,
        }),
    }
}

/// Resolve the sysfs `device` symlink to its PCI bus address.
/// `/sys/class/drm/card2/device` → `…/0000:01:00.0`. We strip
/// everything but the final segment and normalise to the 8-hex-
/// digit-domain form so it can be compared against NVML directly.
fn sysfs_pci_addr(dev: &Path) -> Option<String> {
    let canonical = fs::canonicalize(dev).ok()?;
    let last = canonical.file_name()?.to_str()?;
    Some(normalize_pci_addr(last))
}

/// Build a lookup from canonical PCI bus address to NVML device
/// index. Cheap to rebuild every snapshot (handful of devices,
/// each `pci_info()` call is microseconds).
#[cfg(feature = "nvml")]
fn build_nvml_pci_map(nvml: &nvml_wrapper::Nvml) -> HashMap<String, u32> {
    let mut map = HashMap::new();
    let count = nvml.device_count().unwrap_or(0);
    for i in 0..count {
        let Ok(dev) = nvml.device_by_index(i) else {
            continue;
        };
        let Ok(pci) = dev.pci_info() else { continue };
        map.insert(normalize_pci_addr(&pci.bus_id), i);
    }
    map
}

/// Read every metric NVML exposes for a device, with each call
/// independently optional — NVIDIA's smaller cards (T1000, MX-class)
/// don't implement `power_usage()` and return `NotSupported` rather
/// than zero, so we surface that as `power_watts: None` rather than
/// claiming the card draws 0 W.
#[cfg(feature = "nvml")]
fn read_nvml_device(dev: &nvml_wrapper::Device<'_>, pci_addr: Option<String>) -> Gpu {
    // `name()` returns e.g. "NVIDIA T1000". Cap to the same 32 chars
    // every other vendor's name goes through.
    let name = dev.name().ok().map_or_else(
        || GpuVendor::Nvidia.label().to_string(),
        |s| truncate(&s, 32),
    );
    let busy_pct = dev.utilization_rates().ok().map(|u| f64::from(u.gpu));
    let mem = dev.memory_info().ok();
    let vram_used = mem.as_ref().map_or(0, |m| m.used);
    let vram_total = mem.as_ref().map_or(0, |m| m.total);
    let power_watts = dev.power_usage().ok().map(|mw| f64::from(mw) / 1000.0);
    Gpu {
        vendor: GpuVendor::Nvidia,
        name,
        busy_pct,
        vram_used,
        vram_total,
        power_watts,
        pci_addr,
        intel_engines: None,
    }
}

/// Normalise a PCI bus identifier to the lowercase
/// `domain:bus:device.function` form with an 8-hex-digit domain.
///
/// NVML returns the 8-hex form (`00000000:01:00.0`); sysfs
/// `realpath` returns the 4-hex form (`0000:01:00.0`). Picking the
/// 8-hex form as canonical means we can `Eq`-compare them in a
/// `HashMap` without further string surgery.
fn normalize_pci_addr(raw: &str) -> String {
    let s = raw.trim().to_lowercase();
    let Some((domain, rest)) = s.split_once(':') else {
        return s;
    };
    // Pad to 8 hex chars without truncating an already-8-char
    // domain. `format!("{:0>8}", domain)` does exactly this.
    let padded = format!("{domain:0>8}");
    format!("{padded}:{rest}")
}

/// Best-effort device name. We try, in order:
///
/// 1. `device/label` (rare; some platforms set it for accessibility).
/// 2. The `device` PCI ID rendered as `0xNNNN`.
/// 3. A bare vendor label (`AMD`, `NVIDIA`, `Intel`).
///
/// Whatever we return must fit on a one-line overview, so we cap at
/// 32 chars.
fn device_label(dev: &Path, vendor: GpuVendor) -> String {
    if let Some(l) = read_trim(&dev.join("label")) {
        if !l.is_empty() {
            return truncate(&l, 32);
        }
    }
    if let Some(d) = read_trim(&dev.join("device")) {
        return format!("{} {}", vendor.label(), d.trim_start_matches("0x"));
    }
    vendor.label().to_string()
}

/// AMD-specific reader. Every node here is amdgpu-driver-private but
/// stable across kernels going back to 5.x. Each is independently
/// optional; we surface whatever we got.
fn read_amd(dev: &Path, name: String, pci_addr: Option<String>) -> Gpu {
    let busy_pct = read_trim(&dev.join("gpu_busy_percent"))
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|p| (0.0..=100.0).contains(p));
    let vram_used = read_trim(&dev.join("mem_info_vram_used"))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let vram_total = read_trim(&dev.join("mem_info_vram_total"))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let power_watts = read_amd_power(dev);
    Gpu {
        vendor: GpuVendor::Amd,
        name,
        busy_pct,
        vram_used,
        vram_total,
        power_watts,
        pci_addr,
        intel_engines: None,
    }
}

/// Build a PCI-address → card-path lookup limited to Intel cards.
/// The Intel busy% reader needs the `cardN` directory itself
/// (not the `device/` subdir we pass through `read_one`) because
/// `gt/gt0/rc6_residency_ms` lives one level up. Filtering by
/// vendor here keeps the hot path off non-Intel cards.
fn build_intel_card_map() -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    let Ok(entries) = fs::read_dir("/sys/class/drm") else {
        return map;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_real_card_node(name) {
            continue;
        }
        let card_path = e.path();
        let dev = card_path.join("device");
        let Some(vendor) = read_trim(&dev.join("vendor")) else {
            continue;
        };
        if GpuVendor::from_pci_id(&vendor) != GpuVendor::Intel {
            continue;
        }
        let Some(addr) = sysfs_pci_addr(&dev) else {
            continue;
        };
        map.insert(addr, card_path);
    }
    map
}

/// Read the cumulative RC6 residency counter (milliseconds) for
/// an Intel card. Tries the modern path first (`gt/gt0/`) and
/// falls back to the legacy `power/` location for older kernels.
/// Returns `None` on any IO / parse error so the caller can fall
/// back to detect-only mode without crashing.
fn read_intel_rc6_ms(card_path: &Path) -> Option<u64> {
    let modern = card_path.join("gt/gt0/rc6_residency_ms");
    let legacy = card_path.join("power/rc6_residency_ms");
    for p in [modern, legacy] {
        if let Some(s) = read_trim(&p) {
            if let Ok(v) = s.parse::<u64>() {
                return Some(v);
            }
        }
    }
    None
}

/// Pure busy% derivative — `(1 − ΔRC6 / Δwallclock) × 100`,
/// clamped to `0..=100`. Skips the sample when the wall-clock
/// delta is zero (back-to-back ticks) or the counter went
/// backwards (driver reload, counter reset).
fn compute_intel_busy_pct(prev: IntelSample, cur: IntelSample) -> Option<f64> {
    let dt_ms = cur.when.duration_since(prev.when).as_millis();
    if dt_ms == 0 {
        return None;
    }
    if cur.rc6_ms < prev.rc6_ms {
        return None;
    }
    let drc6 = cur.rc6_ms - prev.rc6_ms;
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    let dt = dt_ms as f64;
    #[allow(clippy::cast_precision_loss)]
    let idle_frac = (drc6 as f64 / dt).min(1.0);
    Some(((1.0 - idle_frac) * 100.0).clamp(0.0, 100.0))
}

/// AMD `power1_average` lives in microwatts under
/// `/sys/.../device/hwmon/hwmonN/power1_average`. We pick the first
/// hwmon directory under the card and read it. Returns watts.
fn read_amd_power(dev: &Path) -> Option<f64> {
    let hwmon_dir = dev.join("hwmon");
    let entries = fs::read_dir(&hwmon_dir).ok()?;
    for e in entries.flatten() {
        let p = e.path().join("power1_average");
        if let Some(uw) = read_trim(&p).and_then(|s| s.parse::<u64>().ok()) {
            #[allow(clippy::cast_precision_loss)]
            return Some(uw as f64 / 1_000_000.0);
        }
    }
    None
}

fn read_trim(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Average busy % across GPUs that report a number. Cards without
/// metrics are excluded, not zero-filled.
pub(crate) fn aggregate_busy_pct(gpus: &[Gpu]) -> Option<f64> {
    let with_data: Vec<f64> = gpus.iter().filter_map(|g| g.busy_pct).collect();
    if with_data.is_empty() {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    Some(with_data.iter().sum::<f64>() / with_data.len() as f64)
}

/// Aggregate VRAM utilisation: sum(used) / sum(total) × 100. `None`
/// when no card reports a non-zero total.
pub(crate) fn aggregate_vram_pct(gpus: &[Gpu]) -> Option<f64> {
    let (used, total) = gpus.iter().fold((0u64, 0u64), |(u, t), g| {
        (u + g.vram_used, t + g.vram_total)
    });
    if total == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    Some((used as f64 / total as f64) * 100.0)
}

/// Sum power draw across cards reporting watts. `None` when none do.
pub(crate) fn aggregate_power_watts(gpus: &[Gpu]) -> Option<f64> {
    let watts: Vec<f64> = gpus.iter().filter_map(|g| g.power_watts).collect();
    if watts.is_empty() {
        return None;
    }
    Some(watts.iter().sum())
}

/// Build the synthetic Gpu we'd produce for a given AMD card given
/// canned strings for each sysfs node. Pure function so the unit
/// tests can assert without touching `/sys`. Used only in tests;
/// production code goes through `Tracker::snapshot`.
#[cfg(test)]
fn parse_amd_for_test(
    busy_raw: Option<&str>,
    vram_used_raw: Option<&str>,
    vram_total_raw: Option<&str>,
    power_uw_raw: Option<&str>,
) -> Gpu {
    let busy_pct = busy_raw
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|p| (0.0..=100.0).contains(p));
    let vram_used = vram_used_raw
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let vram_total = vram_total_raw
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let power_watts = power_uw_raw.and_then(|s| s.trim().parse::<u64>().ok()).map(
        #[allow(clippy::cast_precision_loss)]
        |uw| uw as f64 / 1_000_000.0,
    );
    Gpu {
        vendor: GpuVendor::Amd,
        name: "AMD test".into(),
        busy_pct,
        vram_used,
        vram_total,
        power_watts,
        pci_addr: None,
        intel_engines: None,
    }
}

// -----------------------------------------------------------------------------
// Intel i915 PMU per-engine counters (Linux + i915-pmu feature)
// -----------------------------------------------------------------------------

#[cfg(all(target_os = "linux", feature = "i915-pmu"))]
struct EngineCounter {
    name: &'static str,
    fd: OwnedFd,
    prev_count: u64,
    prev_when: Instant,
}

#[cfg(all(target_os = "linux", feature = "i915-pmu"))]
impl std::fmt::Debug for EngineCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EngineCounter {{ name: {:?} }}", self.name)
    }
}

#[cfg(all(target_os = "linux", feature = "i915-pmu"))]
#[derive(Debug, Default)]
struct IntelPmuTracker {
    counters: Vec<EngineCounter>,
    cap_failed: bool,
    initialized: bool,
}

#[cfg(all(target_os = "linux", feature = "i915-pmu"))]
impl IntelPmuTracker {
    #[allow(clippy::similar_names)] // rcs/bcs/vcs/vecs are standard GPU engine acronyms
    fn merge(&mut self, gpus: &mut [Gpu]) {
        let has_intel = gpus.iter().any(|g| g.vendor == GpuVendor::Intel);
        if !has_intel {
            return;
        }
        if !self.initialized {
            self.try_init();
        }
        if self.cap_failed {
            for g in gpus.iter_mut().filter(|g| g.vendor == GpuVendor::Intel) {
                g.intel_engines = Some(IntelEngines::CapDenied);
            }
            return;
        }
        if self.counters.is_empty() {
            return;
        }
        let now = Instant::now();
        let (mut rcs, mut bcs) = (None::<f64>, None::<f64>);
        // vcs/vecs are GPU engine acronyms; similar names are intentional.
        #[allow(clippy::similar_names)]
        let (mut vcs, mut vecs) = (None::<f64>, None::<f64>);
        for counter in &mut self.counters {
            let Some(count) = read_engine_count(&counter.fd) else {
                counter.prev_when = now;
                continue;
            };
            let elapsed_ns = now.duration_since(counter.prev_when).as_nanos();
            if elapsed_ns > 0 && count >= counter.prev_count {
                let delta = count - counter.prev_count;
                #[allow(clippy::cast_precision_loss)]
                let pct = (delta as f64 / elapsed_ns as f64 * 100.0).clamp(0.0, 100.0);
                match counter.name {
                    "rcs" => rcs = Some(pct),
                    "bcs" => bcs = Some(pct),
                    "vcs" => vcs = Some(pct),
                    "vecs" => vecs = Some(pct),
                    _ => {}
                }
            }
            counter.prev_count = count;
            counter.prev_when = now;
        }
        let engines = IntelEngines::Busy {
            rcs,
            bcs,
            vcs,
            vecs,
        };
        for g in gpus.iter_mut().filter(|g| g.vendor == GpuVendor::Intel) {
            g.intel_engines = Some(engines.clone());
        }
    }

    fn try_init(&mut self) {
        self.initialized = true;
        let Some(pmu_type) = read_i915_pmu_type() else {
            return; // no i915 driver
        };
        let engines: &[(&str, &str)] = &[
            ("rcs", "rcs0-busy"),
            ("bcs", "bcs0-busy"),
            ("vcs", "vcs0-busy"),
            ("vecs", "vecs0-busy"),
        ];
        let now = Instant::now();
        let mut opened_any = false;
        for (name, event_file) in engines {
            let Some(config) = read_i915_event_config(event_file) else {
                continue;
            };
            if let Some(fd) = open_engine_fd(pmu_type, config) {
                let count = read_engine_count(&fd).unwrap_or(0);
                self.counters.push(EngineCounter {
                    name,
                    fd,
                    prev_count: count,
                    prev_when: now,
                });
                opened_any = true;
            } else {
                // EPERM: CAP_PERFMON required; mark and stop trying.
                self.cap_failed = true;
                self.counters.clear();
                return;
            }
        }
        if !opened_any {
            // i915 type exists but no events enumerated (unusual).
            self.counters.clear();
        }
    }
}

/// Read the i915 PMU type from sysfs.
#[cfg(all(target_os = "linux", feature = "i915-pmu"))]
fn read_i915_pmu_type() -> Option<u32> {
    read_trim(Path::new("/sys/bus/event_source/devices/i915/type"))?
        .parse()
        .ok()
}

/// Parse `config=0x15` or `config=21` from an event descriptor file.
#[cfg(all(target_os = "linux", feature = "i915-pmu"))]
fn read_i915_event_config(event: &str) -> Option<u64> {
    let path = format!("/sys/bus/event_source/devices/i915/events/{event}");
    let s = read_trim(Path::new(&path))?;
    let val = s.strip_prefix("config=")?;
    if let Some(hex) = val.strip_prefix("0x").or_else(|| val.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        val.parse().ok()
    }
}

/// Minimal `perf_event_attr` layout — only the fields we use.
/// The kernel reads `size` to know how many bytes to interpret;
/// everything beyond `config` is zero and uses the kernel's defaults
/// (count, no sampling, no format flags).
/// Layout matches `struct perf_event_attr` in `<linux/perf_event.h>`.
#[cfg(all(target_os = "linux", feature = "i915-pmu"))]
#[repr(C)]
struct PerfEventAttr {
    type_: u32,
    size: u32,
    config: u64,
    /// `sample_period` / `sample_freq` union — unused, zero.
    _sample: u64,
    sample_type: u64,
    read_format: u64,
    /// Packed bitfield (`disabled`, `inherit`, …) — zero = defaults.
    _flags: u64,
    /// `wakeup_events` / `wakeup_watermark` union — unused, zero.
    _wakeup: u32,
    bp_type: u32,
    config1: u64,
    config2: u64,
    branch_sample_type: u64,
    sample_regs_user: u64,
    sample_stack_user: u32,
    clockid: i32,
    sample_regs_intr: u64,
    aux_watermark: u32,
    sample_max_stack: u16,
    _reserved2: u16,
    aux_sample_size: u32,
    _reserved3: u32,
    sig_data: u64,
    config3: u64,
}

/// Open a perf event counter for an `i915_pmu` engine.
/// Returns `None` on permission denial or any other failure.
#[cfg(all(target_os = "linux", feature = "i915-pmu"))]
#[allow(unsafe_code)]
fn open_engine_fd(pmu_type: u32, config: u64) -> Option<OwnedFd> {
    // SAFETY: `PerfEventAttr` is a `#[repr(C)]` struct with no padding
    // invariants; zeroing it is safe and produces the "use all defaults"
    // state the kernel accepts.  We only set the fields we need.
    let mut attr: PerfEventAttr = unsafe { std::mem::zeroed() };
    attr.type_ = pmu_type;
    attr.size = u32::try_from(std::mem::size_of::<PerfEventAttr>()).unwrap_or(136);
    attr.config = config;

    // SAFETY: `perf_event_open(2)` is a well-known Linux syscall.
    // `pid = -1` + `cpu = 0` selects system-wide counting on CPU 0;
    // for device PMUs (i915_pmu) `cpu` must be >= 0 even for system-wide
    // mode.  `group_fd = -1` means standalone counter.  `flags = 0`.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_perf_event_open,
            std::ptr::addr_of!(attr) as libc::c_long,
            -1_i64, // pid: system-wide
            0_i64,  // cpu: must be \u2265 0 for device PMUs
            -1_i64, // group_fd
            0_i64,  // flags
        )
    };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` is a valid non-negative file descriptor just returned
    // by `perf_event_open`.  We exclusively own it from this point.
    // fd is the return value of a syscall — it's guaranteed to fit in i32
    // when >= 0 (Linux file descriptors are signed 32-bit integers).
    #[allow(clippy::cast_possible_truncation)]
    Some(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// Read the current 64-bit counter value from a perf fd.
#[cfg(all(target_os = "linux", feature = "i915-pmu"))]
#[allow(unsafe_code)]
fn read_engine_count(fd: &OwnedFd) -> Option<u64> {
    let mut buf = [0u8; 8];
    // SAFETY: `fd` is a valid perf file descriptor.  A bare `read(2)` with
    // no `PERF_FORMAT_*` flags in the attr returns exactly 8 bytes: the
    // 64-bit accumulated event count (nanoseconds for i915_pmu busy events).
    let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), 8) };
    if n == 8 {
        Some(u64::from_ne_bytes(buf))
    } else {
        None
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_vendor_id_matches_known_vendors() {
        assert_eq!(GpuVendor::from_pci_id("0x1002"), GpuVendor::Amd);
        assert_eq!(GpuVendor::from_pci_id("0x10de"), GpuVendor::Nvidia);
        assert_eq!(GpuVendor::from_pci_id("0x8086"), GpuVendor::Intel);
        assert_eq!(GpuVendor::from_pci_id("0xdead"), GpuVendor::Other);
        // Whitespace + capitalisation tolerated (sysfs files have a
        // trailing newline; some kernels uppercase the hex).
        assert_eq!(GpuVendor::from_pci_id(" 0X10DE \n"), GpuVendor::Nvidia);
    }

    #[test]
    fn intel_busy_pct_inverts_rc6_fraction() {
        // Card spent 200 ms in RC6 over a 1000 ms wall-clock
        // window → 80% busy.
        let now = Instant::now();
        let prev = IntelSample {
            when: now,
            rc6_ms: 1000,
        };
        let cur = IntelSample {
            when: now + std::time::Duration::from_secs(1),
            rc6_ms: 1200,
        };
        let busy = compute_intel_busy_pct(prev, cur).unwrap();
        assert!((busy - 80.0).abs() < 0.01, "got {busy}");
    }

    #[test]
    fn intel_busy_pct_clamps_full_idle_window() {
        // RC6 caught up with wall-clock (counter is monotonic but
        // the driver's accounting can momentarily over-report).
        // Result must clamp to 0.
        let now = Instant::now();
        let prev = IntelSample {
            when: now,
            rc6_ms: 1000,
        };
        let cur = IntelSample {
            when: now + std::time::Duration::from_millis(500),
            rc6_ms: 2000, // delta(1000) > dt(500) → idle_frac caps at 1.0
        };
        let busy = compute_intel_busy_pct(prev, cur).unwrap();
        assert!(busy.abs() < 0.01, "got {busy}");
    }

    #[test]
    fn intel_busy_pct_skips_zero_window_and_counter_reset() {
        let now = Instant::now();
        // Same instant → dt = 0; no derivative possible.
        let same = IntelSample {
            when: now,
            rc6_ms: 1000,
        };
        assert_eq!(compute_intel_busy_pct(same, same), None);

        // Counter went backwards (driver reload). Skip rather
        // than report bogus -inf%.
        let prev = IntelSample {
            when: now,
            rc6_ms: 5000,
        };
        let cur = IntelSample {
            when: now + std::time::Duration::from_secs(1),
            rc6_ms: 100,
        };
        assert_eq!(compute_intel_busy_pct(prev, cur), None);
    }

    #[test]
    fn is_real_card_node_filters_connectors_and_render_devices() {
        assert!(is_real_card_node("card0"));
        assert!(is_real_card_node("card1"));
        assert!(is_real_card_node("card12"));
        // Connector subnodes have a `-` in them.
        assert!(!is_real_card_node("card1-DP-1"));
        assert!(!is_real_card_node("card1-HDMI-A-2"));
        // dri-render devices (`renderD128`) and version files.
        assert!(!is_real_card_node("renderD128"));
        assert!(!is_real_card_node("version"));
    }

    #[test]
    fn amd_parser_assembles_full_snapshot() {
        // 8 GiB total, 4.2 GiB used, 35% busy, 75 W draw.
        let g = parse_amd_for_test(
            Some("35\n"),
            Some("4509715456\n"),
            Some("8589934592\n"),
            Some("75000000\n"),
        );
        assert_eq!(g.vendor, GpuVendor::Amd);
        assert_eq!(g.busy_pct, Some(35.0));
        assert_eq!(g.vram_used, 4_509_715_456);
        assert_eq!(g.vram_total, 8_589_934_592);
        assert!((g.power_watts.unwrap() - 75.0).abs() < 1e-6);
        // ~52.5%
        assert!((g.vram_pct().unwrap() - 52.5).abs() < 0.5);
    }

    #[test]
    fn amd_parser_tolerates_partial_data() {
        // Only busy% and total VRAM. Used should default to 0,
        // power should be None, vram_pct should still compute (0%).
        let g = parse_amd_for_test(Some("42"), None, Some("8589934592"), None);
        assert_eq!(g.busy_pct, Some(42.0));
        assert_eq!(g.vram_used, 0);
        assert_eq!(g.vram_total, 8_589_934_592);
        assert_eq!(g.power_watts, None);
        assert!((g.vram_pct().unwrap() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn amd_parser_rejects_out_of_range_busy() {
        // Some buggy kernels return e.g. 256 — that's clearly bogus
        // and we'd rather show "—" than mislead.
        let g = parse_amd_for_test(Some("256"), Some("0"), Some("0"), None);
        assert_eq!(g.busy_pct, None);
    }

    #[test]
    fn vram_pct_is_none_when_total_unknown() {
        let g = parse_amd_for_test(None, Some("100"), Some("0"), None);
        assert_eq!(g.vram_pct(), None);
    }

    #[test]
    fn aggregate_busy_pct_averages_only_known_values() {
        let gpus = vec![
            Gpu {
                vendor: GpuVendor::Amd,
                name: "card0".into(),
                busy_pct: Some(30.0),
                vram_used: 0,
                vram_total: 0,
                power_watts: None,
                pci_addr: None,
                intel_engines: None,
            },
            // No backend — no busy data — must not skew the average.
            Gpu {
                vendor: GpuVendor::Nvidia,
                name: "card1".into(),
                busy_pct: None,
                vram_used: 0,
                vram_total: 0,
                power_watts: None,
                pci_addr: None,
                intel_engines: None,
            },
            Gpu {
                vendor: GpuVendor::Amd,
                name: "card2".into(),
                busy_pct: Some(60.0),
                vram_used: 0,
                vram_total: 0,
                power_watts: None,
                pci_addr: None,
                intel_engines: None,
            },
        ];
        // Average over the two AMD cards = 45%.
        assert!((aggregate_busy_pct(&gpus).unwrap() - 45.0).abs() < 1e-9);
    }

    #[test]
    fn aggregate_busy_pct_returns_none_when_no_backend_responds() {
        let gpus = vec![Gpu {
            vendor: GpuVendor::Nvidia,
            name: "card0".into(),
            busy_pct: None,
            vram_used: 0,
            vram_total: 0,
            power_watts: None,
            pci_addr: None,
            intel_engines: None,
        }];
        assert_eq!(aggregate_busy_pct(&gpus), None);
    }

    #[test]
    fn normalize_pci_addr_pads_short_domain() {
        // Sysfs's `realpath /sys/.../device` form: 4-hex domain.
        assert_eq!(normalize_pci_addr("0000:01:00.0"), "00000000:01:00.0");
    }

    #[test]
    fn normalize_pci_addr_passes_through_long_domain() {
        // NVML's `pci_info().bus_id` form: already 8-hex domain.
        assert_eq!(normalize_pci_addr("00000000:01:00.0"), "00000000:01:00.0");
    }

    #[test]
    fn normalize_pci_addr_lowercases_and_trims() {
        // Some sources uppercase the hex; whitespace can sneak in
        // from sysfs file reads. Both must collapse to the same
        // canonical form so the HashMap lookup hits.
        assert_eq!(normalize_pci_addr(" 0000:01:00.0 \n"), "00000000:01:00.0");
        assert_eq!(normalize_pci_addr("0000:01:00.A"), "00000000:01:00.a");
    }

    #[test]
    fn normalize_pci_addr_handles_garbage_input() {
        // Anything without a colon at all gets returned untouched
        // (lowercased + trimmed). Means a corrupted sysfs symlink
        // can't crash us, just won't match anything in the NVML map.
        assert_eq!(normalize_pci_addr("not-a-pci-addr"), "not-a-pci-addr");
    }

    /// Live smoke: prints the tracker's view of the host's GPUs.
    /// Ignored by default because the result is hardware-specific
    /// — nothing to assert generically. Run on demand with
    /// `cargo test --features nvml -- --ignored gpu_live_snapshot
    /// --nocapture`.
    #[test]
    #[ignore = "live: prints actual hardware state"]
    fn gpu_live_snapshot() {
        let mut t = Tracker::default();
        let gpus = t.snapshot();
        println!("--- live GPUs ({}) ---", gpus.len());
        for g in &gpus {
            println!(
                "{:?}  name={:?}  busy={:?}  vram={}/{}  power={:?}  pci={:?}",
                g.vendor, g.name, g.busy_pct, g.vram_used, g.vram_total, g.power_watts, g.pci_addr,
            );
        }
    }
}
