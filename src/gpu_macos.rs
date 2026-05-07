//! gpu_macos.rs — GPU discovery and per-tick stats on macOS.
//!
//! Pattern: `IOServiceMatching("IOAccelerator")` enumerates every
//! GPU-class IOService the kernel knows about. For each match we
//! pull the registry entry's CFDictionary of properties, extract a
//! human-readable name, classify the vendor by `IOClass`, and read
//! `PerformanceStatistics` for busy% and VRAM-used. VRAM total
//! comes from the same dict (`VRAM,totalMB` on Intel/AMD discrete;
//! Apple Silicon shares system RAM and is reported with a
//! `(unified)` suffix).
//!
//! Per-vendor `PerformanceStatistics` keys (verified against
//! `ioreg -rw0 -c IOAccelerator` on M1, M2 Pro, Intel UHD 630,
//! AMD eGPU):
//!
//! - **Apple Silicon** (`AGXAccelerator*`): `Device Utilization %`
//! - **Intel iGPU** (`IntelAccelerator*`): `Device Utilization %`
//! - **AMD discrete** (`AMDRadeon*`): `GPU Activity(%)`,
//!   `vramUsedBytes`
//! - **NVIDIA** (`nv*`, legacy Intel-Mac only): `Device Utilization`
//!   under `PerformanceStatistics` when present.

use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use io_kit_sys::{
    kIOMasterPortDefault, IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperties,
    IORegistryEntryGetName, IOServiceGetMatchingServices, IOServiceMatching,
};
use std::collections::HashMap;
use std::ffi::CString;

use crate::gpu::{Gpu, GpuVendor};

/// Apple's IOAccelerator services exposing GPU stats.
const IO_ACCELERATOR_CLASS: &[u8] = b"IOAccelerator\0";

/// Per-card cache so repeat snapshots don't rebuild the static
/// (vendor / name / VRAM total) parts of `Gpu` every tick. Keyed
/// by the registry-entry-id which is stable across ticks for a
/// given card. Apple Silicon GPU's id is fixed per boot; eGPU
/// hot-plug invalidates the entry so a fresh discovery pass runs.
#[derive(Debug, Default)]
pub(crate) struct MacosGpuState {
    cached: HashMap<u64, CardStatic>,
}

#[derive(Debug, Clone)]
struct CardStatic {
    vendor: GpuVendor,
    name: String,
    vram_total: u64,
    /// `true` for Apple Silicon (unified memory). VRAM gauge
    /// reports system RAM share; the legend appends "(unified)".
    unified_memory: bool,
}

impl MacosGpuState {
    /// Discover every IOAccelerator service and return one `Gpu`
    /// per match. Empty `Vec` on platforms / configurations where
    /// no GPU is exposed (headless servers, virtualised macOS).
    pub(crate) fn snapshot(&mut self) -> Vec<Gpu> {
        // SAFETY: `IOServiceMatching` returns a +1 retain on the
        // returned dictionary; `IOServiceGetMatchingServices`
        // consumes that retain. The iterator is released below.
        let mut iter: io_kit_sys::types::io_iterator_t = 0;
        unsafe {
            let class = CString::from_vec_with_nul_unchecked(IO_ACCELERATOR_CLASS.to_vec());
            let matching = IOServiceMatching(class.as_ptr());
            if matching.is_null() {
                return Vec::new();
            }
            let kr = IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iter);
            if kr != 0 {
                return Vec::new();
            }
        }

        let mut out = Vec::new();
        let mut seen: Vec<u64> = Vec::new();
        loop {
            // SAFETY: `IOIteratorNext` returns 0 when the iterator
            // is exhausted; we release every non-zero entry below.
            let entry = unsafe { IOIteratorNext(iter) };
            if entry == 0 {
                break;
            }
            if let Some((id, gpu)) = self.read_one(entry) {
                seen.push(id);
                out.push(gpu);
            }
            // SAFETY: `entry` came from `IOIteratorNext` (+1 retain).
            unsafe {
                IOObjectRelease(entry);
            }
        }
        // SAFETY: iterator is +1 from `IOServiceGetMatchingServices`.
        unsafe {
            IOObjectRelease(iter);
        }

        // Drop cache entries for cards that have disappeared (eGPU
        // unplug). Keeps the HashMap O(active-cards) instead of
        // accumulating stale entries forever.
        self.cached.retain(|id, _| seen.contains(id));

        out
    }

    fn read_one(&mut self, entry: io_kit_sys::types::io_registry_entry_t) -> Option<(u64, Gpu)> {
        let id = registry_entry_id(entry)?;
        let props = copy_properties(entry)?;
        let stat = self
            .cached
            .entry(id)
            .or_insert_with(|| classify(entry, &props))
            .clone();
        let (busy_pct, vram_used) = read_perf_stats(&props, stat.vendor);
        let name = if stat.unified_memory {
            format!("{} (unified)", stat.name)
        } else {
            stat.name.clone()
        };
        Some((
            id,
            Gpu {
                vendor: stat.vendor,
                name,
                busy_pct,
                vram_used: vram_used.unwrap_or(0),
                vram_total: stat.vram_total,
                power_watts: None,
                pci_addr: None,
                intel_engines: None,
            },
        ))
    }
}

/// Pull `IORegistryEntryID` for a service. Stable across the
/// lifetime of the device's connection to the IOReg.
fn registry_entry_id(entry: io_kit_sys::types::io_registry_entry_t) -> Option<u64> {
    let mut id: u64 = 0;
    // SAFETY: `IORegistryEntryGetRegistryEntryID` writes a u64.
    let kr = unsafe { io_kit_sys::IORegistryEntryGetRegistryEntryID(entry, &mut id) };
    if kr == 0 {
        Some(id)
    } else {
        None
    }
}

/// Snapshot the registry entry's CFDictionary of properties.
fn copy_properties(entry: io_kit_sys::types::io_registry_entry_t) -> Option<CFDictionary> {
    let mut props: core_foundation_sys::dictionary::CFMutableDictionaryRef = std::ptr::null_mut();
    // SAFETY: function fills `props` with a +1 retain on success;
    // we take ownership via `wrap_under_create_rule`.
    let kr =
        unsafe { IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null_mut(), 0) };
    if kr != 0 || props.is_null() {
        return None;
    }
    // SAFETY: `props` is a +1 retained CFMutableDictionary.
    Some(unsafe { CFDictionary::wrap_under_create_rule(props.cast()) })
}

/// Classify vendor + extract one-shot static fields (name, VRAM
/// total, unified-memory flag). Run once per card on first sight.
fn classify(entry: io_kit_sys::types::io_registry_entry_t, props: &CFDictionary) -> CardStatic {
    let class_name = io_object_class(entry).unwrap_or_default();
    let vendor = vendor_from_class(&class_name);

    // Prefer the IOClass-derived name; fall back to the registry
    // entry's name (e.g. `IntelAccelerator`, `AGXAccelerator`).
    let name = registry_entry_name(entry).unwrap_or_else(|| class_name.clone());

    // VRAM total: `VRAM,totalMB` is a CFNumber on Intel / AMD
    // discrete. Apple Silicon GPUs lack the key (unified memory);
    // fall back to system RAM via sysctl `hw.memsize`.
    let unified_memory = matches!(vendor, GpuVendor::Other) && class_name.starts_with("AGX")
        || class_name.starts_with("AGX");
    let vram_total = read_cfnumber_u64(props, "VRAM,totalMB")
        .map(|mb| mb.saturating_mul(1024 * 1024))
        .or_else(|| read_cfnumber_u64(props, "VRAM,totalbytes"))
        .unwrap_or_else(|| if unified_memory { hw_memsize() } else { 0 });

    CardStatic {
        vendor,
        name,
        vram_total,
        unified_memory,
    }
}

/// Map an `IOClass` string to our `GpuVendor` enum. Unknown
/// strings get `Other` rather than panicking; the UI still
/// renders the card with a generic label.
fn vendor_from_class(class: &str) -> GpuVendor {
    if class.starts_with("AGX") || class.contains("AppleM") {
        // Apple Silicon GPUs all derive from AGXAccelerator. The
        // canonical vendor would be "Apple" but our enum predates
        // that — `Other` is the honest classification and the
        // name field carries the device specifics.
        GpuVendor::Other
    } else if class.starts_with("Intel") {
        GpuVendor::Intel
    } else if class.starts_with("AMDRadeon") || class.starts_with("ATIRadeon") {
        GpuVendor::Amd
    } else if class.starts_with("nv") || class.starts_with("NV") {
        GpuVendor::Nvidia
    } else {
        GpuVendor::Other
    }
}

/// Read busy% and VRAM-used from the `PerformanceStatistics`
/// sub-dictionary. Vendor-specific keys handled centrally.
fn read_perf_stats(props: &CFDictionary, vendor: GpuVendor) -> (Option<f64>, Option<u64>) {
    let perf = match get_cfdict(props, "PerformanceStatistics") {
        Some(d) => d,
        None => return (None, None),
    };

    let busy_keys: &[&str] = match vendor {
        GpuVendor::Amd => &["GPU Activity(%)", "Device Utilization %"],
        _ => &["Device Utilization %", "GPU Core Utilization"],
    };
    let mut busy_pct = None;
    for k in busy_keys {
        if let Some(v) = read_cfnumber_f64(&perf, k) {
            busy_pct = Some(v.clamp(0.0, 100.0));
            break;
        }
    }

    let vram_keys = [
        "vramUsedBytes",
        "inUseVidMemoryBytes",
        "Alloc system memory",
    ];
    let mut vram_used = None;
    for k in &vram_keys {
        if let Some(v) = read_cfnumber_u64(&perf, k) {
            vram_used = Some(v);
            break;
        }
    }

    (busy_pct, vram_used)
}

// -------------------------------------------------------------------------
// Core Foundation helpers — kept thin and local so the rest of
// the file stays readable.
// -------------------------------------------------------------------------

fn cfstr(s: &str) -> CFString {
    CFString::new(s)
}

fn read_cfnumber_u64(dict: &CFDictionary, key: &str) -> Option<u64> {
    let key = cfstr(key);
    // SAFETY: `find` returns a borrowed reference into `dict`.
    let value: *const std::ffi::c_void = dict.find(key.as_concrete_TypeRef().cast()).map(|v| *v)?;
    if value.is_null() {
        return None;
    }
    // SAFETY: `value` is a CFNumber; we do not retain it.
    let num = unsafe { CFNumber::wrap_under_get_rule(value.cast()) };
    num.to_i64().and_then(|n| u64::try_from(n).ok())
}

fn read_cfnumber_f64(dict: &CFDictionary, key: &str) -> Option<f64> {
    let key = cfstr(key);
    let value: *const std::ffi::c_void = dict.find(key.as_concrete_TypeRef().cast()).map(|v| *v)?;
    if value.is_null() {
        return None;
    }
    // SAFETY: same rule as `read_cfnumber_u64`.
    let num = unsafe { CFNumber::wrap_under_get_rule(value.cast()) };
    num.to_f64().or_else(|| num.to_i64().map(|n| n as f64))
}

fn get_cfdict(dict: &CFDictionary, key: &str) -> Option<CFDictionary> {
    let key = cfstr(key);
    let value: *const std::ffi::c_void = dict.find(key.as_concrete_TypeRef().cast()).map(|v| *v)?;
    if value.is_null() {
        return None;
    }
    // SAFETY: `value` is a CFDictionary; borrow without retaining.
    let raw: CFDictionaryRef = value.cast();
    Some(unsafe { CFDictionary::wrap_under_get_rule(raw) })
}

/// `IOObjectGetClass` writes a NUL-terminated C string up to 128 bytes.
fn io_object_class(entry: io_kit_sys::types::io_object_t) -> Option<String> {
    let mut buf = [0i8; 128];
    // SAFETY: 128-byte buffer is the documented sufficient size.
    let kr = unsafe { io_kit_sys::IOObjectGetClass(entry, buf.as_mut_ptr()) };
    if kr != 0 {
        return None;
    }
    let s: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8(s).ok()
}

/// `IORegistryEntryGetName` returns the same kind of buffer.
fn registry_entry_name(entry: io_kit_sys::types::io_registry_entry_t) -> Option<String> {
    let mut buf = [0i8; 128];
    // SAFETY: same buffer-size rationale as above.
    let kr = unsafe { IORegistryEntryGetName(entry, buf.as_mut_ptr()) };
    if kr != 0 {
        return None;
    }
    let s: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8(s).ok()
}

/// `sysctl hw.memsize` returns total physical RAM in bytes. Used
/// as the VRAM-total surrogate on Apple Silicon (unified memory).
fn hw_memsize() -> u64 {
    const CTL_HW: i32 = 6;
    const HW_MEMSIZE: i32 = 24;
    let mut value: u64 = 0;
    let mut len: libc::size_t = std::mem::size_of::<u64>() as libc::size_t;
    let mut mib = [CTL_HW, HW_MEMSIZE];
    // SAFETY: standard 2-element sysctl read into a u64. Buffer
    // and length are correctly sized.
    unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            &mut value as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        );
    }
    value
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn vendor_from_class_recognises_known_prefixes() {
        assert!(matches!(
            vendor_from_class("IntelAccelerator"),
            GpuVendor::Intel
        ));
        assert!(matches!(
            vendor_from_class("AMDRadeonX6000"),
            GpuVendor::Amd
        ));
        assert!(matches!(
            vendor_from_class("AGXAcceleratorG13X"),
            GpuVendor::Other
        ));
        assert!(matches!(
            vendor_from_class("nvAccelerator"),
            GpuVendor::Nvidia
        ));
        assert!(matches!(vendor_from_class("WeirdGPU"), GpuVendor::Other));
    }

    /// Live discovery test — only meaningful on a real macOS host.
    /// `cargo test --target aarch64-apple-darwin -- --ignored
    ///   gpu_live_snapshot_macos --nocapture`.
    #[test]
    #[ignore = "requires real macOS hardware"]
    fn gpu_live_snapshot_macos() {
        let mut state = MacosGpuState::default();
        let gpus = state.snapshot();
        for g in &gpus {
            eprintln!(
                "{:>8}  {}  busy={:?} vram={}/{} MB",
                format!("{:?}", g.vendor),
                g.name,
                g.busy_pct,
                g.vram_used / (1024 * 1024),
                g.vram_total / (1024 * 1024),
            );
        }
    }
}
