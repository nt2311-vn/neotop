//! battery.rs — battery status from `/sys/class/power_supply`.
//!
//! macOS: reads `IOPSCopyPowerSourcesInfo` directly from IOKit
//! (faster than forking `system_profiler`, no JSON parsing, no shell
//! dependency, and gives us the same fields Activity Monitor shows).

#[cfg(target_os = "linux")]
use std::fs;

#[derive(Debug, Clone)]
#[allow(dead_code)] // `name` shown in future multi-battery variants
pub(crate) struct Battery {
    /// Display name, e.g. `"BAT0"`.
    pub(crate) name: String,
    /// 0..=100. The kernel already clamps this.
    pub(crate) percent: u8,
    /// `"Charging"`, `"Discharging"`, `"Full"`, `"Not charging"`,
    /// `"Unknown"`. We pass through whatever sysfs reports.
    pub(crate) status: String,
    /// Instantaneous draw/charge rate in watts (positive = draw on
    /// discharge, negative when charging on most laptops). `None` if
    /// the driver doesn't expose `power_now`.
    pub(crate) watts: Option<f64>,
}

pub(crate) fn snapshot() -> Vec<Battery> {
    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = fs::read_dir("/sys/class/power_supply") else {
            return Vec::new();
        };
        let mut out = Vec::new();

        for e in entries.flatten() {
            let path = e.path();
            let kind = fs::read_to_string(path.join("type"))
                .ok()
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if kind != "Battery" {
                continue;
            }

            let Some(name) = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let Some(percent) = fs::read_to_string(path.join("capacity"))
                .ok()
                .and_then(|s| s.trim().parse::<u8>().ok())
            else {
                continue;
            };
            let status = fs::read_to_string(path.join("status"))
                .ok()
                .map_or_else(|| "Unknown".into(), |s| s.trim().to_string());
            #[allow(clippy::cast_precision_loss)]
            let watts = fs::read_to_string(path.join("power_now"))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|uw| uw as f64 / 1_000_000.0);

            out.push(Battery {
                name,
                percent,
                status,
                watts,
            });
        }
        out
    }
    #[cfg(target_os = "macos")]
    {
        snapshot_macos()
    }
}

#[cfg(target_os = "macos")]
fn snapshot_macos() -> Vec<Battery> {
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use io_kit_sys::ps::keys::{
        kIOPSCurrentCapacityKey, kIOPSIsChargingKey, kIOPSMaxCapacityKey, kIOPSNameKey,
        kIOPSPowerSourceStateKey,
    };
    use io_kit_sys::ps::power_sources::{
        IOPSCopyPowerSourcesInfo, IOPSCopyPowerSourcesList, IOPSGetPowerSourceDescription,
    };

    // SAFETY: IOPS APIs return retained / borrowed CF objects per
    // their +1/+0 ownership rules; we wrap them with the matching
    // `wrap_under_*_rule` so Rust drops them correctly.
    unsafe {
        let blob = IOPSCopyPowerSourcesInfo();
        if blob.is_null() {
            return Vec::new();
        }
        let blob = CFType::wrap_under_create_rule(blob);

        let list = IOPSCopyPowerSourcesList(blob.as_concrete_TypeRef());
        if list.is_null() {
            return Vec::new();
        }
        let list: CFArray<CFType> = CFArray::wrap_under_create_rule(list);

        let mut out = Vec::with_capacity(list.len() as usize);
        for ps in list.iter() {
            let desc_ref =
                IOPSGetPowerSourceDescription(blob.as_concrete_TypeRef(), ps.as_concrete_TypeRef());
            if desc_ref.is_null() {
                continue;
            }
            let desc: CFDictionary = CFDictionary::wrap_under_get_rule(desc_ref);

            // SAFETY: IOPS key constants are `*const c_char` to
            // static NUL-terminated string literals; safe to wrap
            // into `&CStr` and decode as UTF-8 for `CFString`.
            let key = |k: *const std::os::raw::c_char| -> CFString {
                CFString::from(std::ffi::CStr::from_ptr(k).to_str().unwrap_or(""))
            };
            let get_num = |k: *const std::os::raw::c_char| -> Option<i64> {
                let v: *const std::ffi::c_void =
                    desc.find(key(k).as_concrete_TypeRef().cast()).map(|v| *v)?;
                if v.is_null() {
                    return None;
                }
                CFNumber::wrap_under_get_rule(v.cast()).to_i64()
            };
            let get_str = |k: *const std::os::raw::c_char| -> Option<String> {
                let v: *const std::ffi::c_void =
                    desc.find(key(k).as_concrete_TypeRef().cast()).map(|v| *v)?;
                if v.is_null() {
                    return None;
                }
                Some(CFString::wrap_under_get_rule(v.cast()).to_string())
            };
            let get_bool = |k: *const std::os::raw::c_char| -> Option<bool> {
                let v: *const std::ffi::c_void =
                    desc.find(key(k).as_concrete_TypeRef().cast()).map(|v| *v)?;
                if v.is_null() {
                    return None;
                }
                Some(CFBoolean::wrap_under_get_rule(v.cast()).into())
            };

            let cur = get_num(kIOPSCurrentCapacityKey);
            let max = get_num(kIOPSMaxCapacityKey);
            let Some(percent) = pct(cur, max) else {
                continue;
            };
            let name = get_str(kIOPSNameKey).unwrap_or_else(|| "Battery".to_string());
            // `kIOPSPowerSourceStateKey` is "AC Power" / "Battery
            // Power"; `kIOPSIsChargingKey` is a boolean. Combine
            // into the same string vocabulary the Linux side uses
            // so `Theme::battery_color` works without branching.
            let state = get_str(kIOPSPowerSourceStateKey);
            let charging = get_bool(kIOPSIsChargingKey).unwrap_or(false);
            let status = match (state.as_deref(), charging, percent) {
                (Some("AC Power"), true, _) => "Charging",
                (Some("AC Power"), false, 100) => "Full",
                (Some("AC Power"), false, _) => "Not charging",
                (Some("Battery Power"), _, _) => "Discharging",
                _ => "Unknown",
            }
            .to_string();

            out.push(Battery {
                name,
                percent,
                status,
                // IOPS doesn't expose instantaneous wattage in the
                // public dictionary; that data is private to
                // `AppleSmartBattery` IOService and needs an
                // additional IOReg walk. Leave as `None` for now —
                // the UI already handles `None` cleanly.
                watts: None,
            });
        }
        out
    }
}

/// Compute `0..=100` from `current / max`, clamping out-of-range
/// values. Returns `None` if either side is missing or `max` is
/// non-positive (kernel hasn't populated the dict yet).
#[cfg(target_os = "macos")]
fn pct(cur: Option<i64>, max: Option<i64>) -> Option<u8> {
    let (cur, max) = (cur?, max?);
    if max <= 0 {
        return None;
    }
    let raw = (cur.saturating_mul(100) / max).clamp(0, 100);
    u8::try_from(raw).ok()
}

/// `capacity` sysfs file holds an integer 0..=100. We trim whitespace
/// because the file ends in a newline.
#[allow(dead_code)]
pub(crate) fn parse_capacity(raw: &str) -> Option<u8> {
    raw.trim().parse::<u8>().ok()
}

/// `power_now` is in microwatts; some drivers expose it as negative
/// while charging, some as positive. We keep the sign so the UI can
/// distinguish draw vs charge.
#[allow(dead_code)]
pub(crate) fn parse_power_now_watts(raw: &str) -> Option<f64> {
    let uw: i64 = raw.trim().parse().ok()?;
    #[allow(clippy::cast_precision_loss)]
    Some(uw as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_capacity_trims_newline() {
        assert_eq!(parse_capacity("87\n"), Some(87));
        assert_eq!(parse_capacity("0\n"), Some(0));
        assert_eq!(parse_capacity("100\n"), Some(100));
    }

    #[test]
    fn parse_capacity_rejects_garbage() {
        assert_eq!(parse_capacity(""), None);
        assert_eq!(parse_capacity("nope\n"), None);
        // 256 doesn't fit in u8 — kernel never writes this but be safe.
        assert_eq!(parse_capacity("256\n"), None);
    }

    #[test]
    fn parse_power_now_converts_uw_to_watts() {
        assert!((parse_power_now_watts("12000000\n").unwrap() - 12.0).abs() < 1e-9);
        assert!((parse_power_now_watts("-3500000\n").unwrap() - (-3.5)).abs() < 1e-9);
        assert_eq!(parse_power_now_watts("0\n"), Some(0.0));
    }

    #[test]
    fn parse_power_now_rejects_garbage() {
        assert_eq!(parse_power_now_watts(""), None);
        assert_eq!(parse_power_now_watts("???"), None);
    }
}
