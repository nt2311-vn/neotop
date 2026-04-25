//! gpu.rs — discrete + integrated GPU stats.
//!
//! Two parallel data sources:
//!
//! * **sysfs** (`/sys/class/drm/card*`) — universal vendor probe and
//!   the AMD metrics backend. Free, no extra deps, no privileges.
//! * **NVML** (NVIDIA Management Library, via the `nvml-wrapper`
//!   crate, gated behind the default-on `nvml` feature) — real
//!   metrics for NVIDIA cards. The crate dlopens
//!   `libnvidia-ml.so` at runtime, so the binary builds and runs
//!   on machines without the NVIDIA driver; init failure just
//!   leaves NVIDIA cards in detect-only mode.
//!
//! Snapshot algorithm:
//!   1. Walk sysfs, produce a `Gpu` for every `cardN`. AMD cards
//!      get full metrics; NVIDIA / Intel start as "pending".
//!   2. If NVML is initialised, build a PCI-bus → device-index
//!      map from NVML's view, then for every sysfs NVIDIA card
//!      look up its PCI address and replace the entry with the
//!      NVML record (real busy %, VRAM, watts).
//!   3. NVIDIA cards present in sysfs but missing from NVML
//!      (proprietary driver loaded but the card was hot-disabled,
//!      runtime PM suspended) stay as detect-only.
//!
//! Intel still has no real backend; its Xe / i915 perf counters
//! need `CAP_PERFMON` or root and are deferred to a later release.
//!
//! All probes are best-effort: a card that disappears mid-scan
//! (eGPU unplug, runtime PM) is silently skipped, never panics.

#[cfg(feature = "nvml")]
use std::collections::HashMap;
use std::fs;
use std::path::Path;

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
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    #[cfg(feature = "nvml")]
    nvml: NvmlState,
}

impl Tracker {
    // `clippy::unused_self`: when the `nvml` feature is off, this
    // method genuinely doesn't read `self` — but with the default
    // feature on it does. Using `&mut self` unconditionally keeps
    // the call site stable across feature combinations.
    #[cfg_attr(not(feature = "nvml"), allow(clippy::unused_self))]
    pub(crate) fn snapshot(&mut self) -> Vec<Gpu> {
        // The `mut` is used by the `nvml` cfg branch below.
        #[allow(unused_mut)]
        let mut out = scan_sysfs_cards();
        #[cfg(feature = "nvml")]
        self.merge_nvml(&mut out);
        out
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
    }
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

/// Average busy % across every GPU that reports a real number.
/// Used by the host-history feeder so the GPU sparkline tracks
/// aggregate pressure on multi-card boxes (rare on laptops, common
/// on workstations). NVIDIA / Intel cards we don't have real
/// metrics for are excluded from the average rather than zero-
/// filled — zero would lie about the workstation's true load.
pub(crate) fn aggregate_busy_pct(gpus: &[Gpu]) -> Option<f64> {
    let with_data: Vec<f64> = gpus.iter().filter_map(|g| g.busy_pct).collect();
    if with_data.is_empty() {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    Some(with_data.iter().sum::<f64>() / with_data.len() as f64)
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
            },
            Gpu {
                vendor: GpuVendor::Amd,
                name: "card2".into(),
                busy_pct: Some(60.0),
                vram_used: 0,
                vram_total: 0,
                power_watts: None,
                pci_addr: None,
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
