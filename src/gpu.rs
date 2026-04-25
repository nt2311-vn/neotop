//! gpu.rs — discrete + integrated GPU stats from `/sys/class/drm`.
//!
//! Probes every `/sys/class/drm/card*` node, reads its PCI vendor
//! ID, and dispatches to a per-vendor reader. Today AMD has a real
//! backend (sysfs is generous: `gpu_busy_percent`, `mem_info_vram_*`,
//! plus `power1_average` under hwmon). NVIDIA and Intel are
//! *detected* — we record the card and a friendly label — but their
//! metrics path is gated behind future work:
//!
//! * **NVIDIA** needs the NVML library via the `nvml-wrapper` crate.
//!   That's a non-trivial dependency and a separate v0.9.0 release.
//! * **Intel** Xe / i915 only exposes utilisation through perf
//!   counters that require root or a dedicated `intel_gpu_top`-style
//!   Linux capability. Same v0.9.0+ track.
//!
//! Detecting them here means the user can see "your card is
//! recognised" instead of silently nothing — a small but real
//! ergonomic win on hybrid laptops.
//!
//! All sysfs reads are best-effort: a card that disappears mid-scan
//! (eGPU unplug, runtime PM) is silently skipped, never panics.

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

/// Stateful tracker. Today there's nothing to cache between scans
/// (no slow probes like the temp module's blacklist), but using a
/// tracker shape from day one means adding NVML / per-engine
/// caching later doesn't change every call site.
#[derive(Debug, Default)]
pub(crate) struct Tracker {}

impl Tracker {
    // `clippy::unused_self`: the empty `&mut self` here is
    // deliberate — future backends (NVML handle reuse, slow-sensor
    // blacklisting like `temp::Tracker`) will need it. Forcing the
    // call site to migrate later is the larger evil.
    #[allow(clippy::unused_self)]
    pub(crate) fn snapshot(&mut self) -> Vec<Gpu> {
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
}

fn is_real_card_node(name: &str) -> bool {
    name.starts_with("card") && !name.contains('-') && !name.starts_with("cardD")
}

fn read_one(dev: &Path) -> Option<Gpu> {
    let vendor_raw = fs::read_to_string(dev.join("vendor")).ok()?;
    let vendor = GpuVendor::from_pci_id(&vendor_raw);
    let name = device_label(dev, vendor);

    match vendor {
        GpuVendor::Amd => Some(read_amd(dev, name)),
        GpuVendor::Nvidia | GpuVendor::Intel | GpuVendor::Other => Some(Gpu {
            vendor,
            name,
            busy_pct: None,
            vram_used: 0,
            vram_total: 0,
            power_watts: None,
        }),
    }
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
fn read_amd(dev: &Path, name: String) -> Gpu {
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
            },
            // No backend — no busy data — must not skew the average.
            Gpu {
                vendor: GpuVendor::Nvidia,
                name: "card1".into(),
                busy_pct: None,
                vram_used: 0,
                vram_total: 0,
                power_watts: None,
            },
            Gpu {
                vendor: GpuVendor::Amd,
                name: "card2".into(),
                busy_pct: Some(60.0),
                vram_used: 0,
                vram_total: 0,
                power_watts: None,
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
        }];
        assert_eq!(aggregate_busy_pct(&gpus), None);
    }
}
