//! temp_macos.rs — temperature sensor reader for macOS.
//!
//! Talks to AppleSMC (see [`crate::smc_macos`] for the protocol) and
//! returns a `ScanReport` shaped exactly like the Linux hwmon path
//! so the cross-platform `temp::Tracker` doesn't care which OS the
//! readings came from. Both Intel and Apple Silicon Macs route
//! through the same SMC user-client; only the key vocabulary
//! differs and we probe the union (see `SENSOR_KEYS`).
//!
//! The IOKit `IOAccelerator` fallback for GPU temperature is kept
//! as a last resort — some macOS releases stop reporting `TG0*` /
//! `Tg0*` via SMC, but the GPU's own IOReg dictionary still carries
//! a `Temperature` field. Belt-and-braces.

use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use io_kit_sys::{
    kIOMasterPortDefault, IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperties,
    IOServiceGetMatchingServices, IOServiceMatching,
};
use std::ffi::CString;

use crate::smc_macos::{SmcClient, SENSOR_KEYS};
use crate::temp::{Reading, ScanReport, Tracker};

pub(crate) fn scan(tracker: &Tracker) -> ScanReport {
    tracker.scan_macos()
}

impl Tracker {
    /// Drive the platform-specific scan. The cached SMC client
    /// lives for the lifetime of the function call — opening costs
    /// one IOKit round-trip, well under 1 ms.
    fn scan_macos(&self) -> ScanReport {
        let mut readings: Vec<Reading> = Vec::new();
        let mut errors: Vec<(&'static str, String)> = Vec::new();

        match SmcClient::open() {
            Some(smc) => {
                for (key, label) in SENSOR_KEYS {
                    if let Some(c) = smc.read_temperature(key) {
                        // Filter outliers — a missing-but-not-reported
                        // sensor sometimes returns 0 K (-273) or
                        // wildly positive values. Anything outside a
                        // plausible silicon range is junk.
                        if (-40.0..=125.0).contains(&c) && c.abs() > 0.5 {
                            readings.push(Reading {
                                label: (*label).to_string(),
                                celsius: c,
                            });
                        }
                    }
                }
            }
            None => {
                errors.push(("temp", "AppleSMC: IOServiceOpen failed".into()));
            }
        }

        // Belt-and-braces: walk `IOAccelerator` for any GPU that
        // still surfaces `Temperature` in its IOReg dict. This is
        // the only way to see Apple Silicon dGPU readings on the
        // few external NVIDIA cards still supported.
        if let Some(temp) = read_ioacc_gpu_temperature() {
            if !readings.iter().any(|r| r.label.starts_with("GPU")) {
                readings.push(Reading {
                    label: "GPU".to_string(),
                    celsius: temp,
                });
            }
        }

        ScanReport {
            readings,
            infos: Vec::new(),
            errors,
        }
    }
}

/// Walk every `IOAccelerator` and return the first `Temperature`
/// (or `Device Temperature` / `GPU Temperature`) value we find.
/// Some drivers report deci-Celsius — divide by 10 if the reading
/// is implausibly large.
fn read_ioacc_gpu_temperature() -> Option<f64> {
    // SAFETY: IOKit's matching/iteration APIs are documented; we
    // release every retained object on every return path.
    unsafe {
        let class = CString::new("IOAccelerator").ok()?;
        let matching = IOServiceMatching(class.as_ptr());
        if matching.is_null() {
            return None;
        }
        let mut iter: io_kit_sys::types::io_iterator_t = 0;
        if IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iter) != 0 {
            return None;
        }

        let mut found: Option<f64> = None;
        loop {
            let entry = IOIteratorNext(iter);
            if entry == 0 {
                break;
            }
            if let Some(t) = temperature_from_entry(entry) {
                found = Some(t);
                IOObjectRelease(entry);
                break;
            }
            IOObjectRelease(entry);
        }
        IOObjectRelease(iter);
        found
    }
}

unsafe fn temperature_from_entry(entry: io_kit_sys::types::io_registry_entry_t) -> Option<f64> {
    let mut props: core_foundation_sys::dictionary::CFMutableDictionaryRef = std::ptr::null_mut();
    if IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null_mut(), 0) != 0
        || props.is_null()
    {
        return None;
    }
    let dict: CFDictionary = CFDictionary::wrap_under_create_rule(props.cast());
    for key in ["Temperature", "Device Temperature", "GPU Temperature"] {
        if let Some(raw) = read_cfnumber_f64(&dict, key) {
            let c = if raw > 1000.0 { raw / 10.0 } else { raw };
            if (-40.0..=125.0).contains(&c) && c.abs() > 0.5 {
                return Some(c);
            }
        }
    }
    None
}

unsafe fn read_cfnumber_f64(dict: &CFDictionary, key: &str) -> Option<f64> {
    let _ = dict; // borrow check
    let cf_key = CFString::new(key);
    let value: *const std::ffi::c_void = dict
        .find(cf_key.as_concrete_TypeRef().cast::<std::ffi::c_void>())
        .map(|v| *v)?;
    if value.is_null() {
        return None;
    }
    let num = CFNumber::wrap_under_get_rule(value.cast());
    num.to_f64().or_else(|| num.to_i64().map(|n| n as f64))
}
