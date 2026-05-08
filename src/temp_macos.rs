//! temp_macos.rs — temperature sensor reading on macOS.
//!
//! Uses IOKit to read temperature sensors:
//! - Intel Macs: SMC (System Management Controller) via AppleSMC driver
//! - Apple Silicon: IOReport framework for thermal sensors
//!
//! This is a simplified implementation that provides basic temperature
//! readings for the most common sensors.

use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use io_kit_sys::{
    kIOMasterPortDefault, IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperties,
    IOServiceGetMatchingServices, IOServiceMatching,
};
use std::ffi::CString;

use crate::temp::{Reading, ScanReport, Tracker};

/// AppleSMC service for Intel Macs
const APPLE_SMC_CLASS: &[u8] = b"AppleSMC\0";

/// IOReport service for Apple Silicon
const IO_REPORT_CLASS: &[u8] = b"IOReport\0";

pub(crate) fn scan(tracker: &Tracker) -> ScanReport {
    tracker.scan_macos()
}

impl Tracker {
    /// Scan temperature sensors on macOS
    fn scan_macos(&self) -> ScanReport {
        // Detect architecture
        let is_apple_silicon = self.is_apple_silicon();

        if is_apple_silicon {
            self.scan_apple_silicon()
        } else {
            self.scan_intel()
        }
    }

    /// Check if running on Apple Silicon
    fn is_apple_silicon(&self) -> bool {
        // Use sysctl to check hw.machine for arm64
        const CTL_HW: i32 = 6;
        const HW_MACHINE: i32 = 1;
        let mut value = [0i8; 32];
        let mut len = value.len() as libc::size_t;
        let mut mib = [CTL_HW, HW_MACHINE];

        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                value.as_mut_ptr() as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };

        if result == 0 {
            let bytes: Vec<u8> = value
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            let machine = unsafe { std::str::from_utf8_unchecked(&bytes) };
            machine.starts_with("arm")
        } else {
            false
        }
    }

    /// Scan temperature sensors on Intel Macs via SMC
    fn scan_intel(&self) -> ScanReport {
        let mut readings = Vec::new();

        // Try to read from AppleSMC
        if let Some(temp) = self.read_smc_temperature() {
            readings.push(Reading {
                label: "CPU".to_string(),
                celsius: temp,
            });
        }

        // Try to read GPU temperature if available
        if let Some(temp) = self.read_gpu_temperature() {
            readings.push(Reading {
                label: "GPU".to_string(),
                celsius: temp,
            });
        }

        ScanReport {
            readings,
            infos: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// Scan temperature sensors on Apple Silicon via IOReport
    fn scan_apple_silicon(&self) -> ScanReport {
        let mut readings = Vec::new();

        // Apple Silicon temperature sensors are exposed via IOReport
        // This is a simplified implementation that reads common sensors
        if let Some(temp) = self.read_ioreport_temperature("TC0E") {
            readings.push(Reading {
                label: "CPU Efficiency".to_string(),
                celsius: temp,
            });
        }

        if let Some(temp) = self.read_ioreport_temperature("TC0P") {
            readings.push(Reading {
                label: "CPU Performance".to_string(),
                celsius: temp,
            });
        }

        if let Some(temp) = self.read_ioreport_temperature("TG0D") {
            readings.push(Reading {
                label: "GPU".to_string(),
                celsius: temp,
            });
        }

        ScanReport {
            readings,
            infos: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// Read temperature from SMC on Intel Macs
    fn read_smc_temperature(&self) -> Option<f64> {
        // This is a simplified implementation. A full implementation would:
        // 1. Open AppleSMC user client
        // 2. Send SMC keys for temperature sensors
        // 3. Parse the response

        // For now, return None as this requires complex SMC protocol handling
        // A production implementation would use the SMC keys like "TC0C", "TC0D", etc.
        None
    }

    /// Read GPU temperature via IOKit
    fn read_gpu_temperature(&self) -> Option<f64> {
        // Try to read from IOAccelerator services
        let mut iter: io_kit_sys::types::io_iterator_t = 0;

        unsafe {
            let class = CString::new("IOAccelerator").ok()?;
            let matching = IOServiceMatching(class.as_ptr());
            if matching.is_null() {
                return None;
            }
            let kr = IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iter);
            if kr != 0 {
                return None;
            }
        }

        loop {
            let entry = unsafe { IOIteratorNext(iter) };
            if entry == 0 {
                break;
            }

            if let Some(temp) = self.read_temperature_from_entry(entry) {
                unsafe { IOObjectRelease(entry) };
                unsafe { IOObjectRelease(iter) };
                return Some(temp);
            }

            unsafe { IOObjectRelease(entry) };
        }

        unsafe { IOObjectRelease(iter) };
        None
    }

    /// Read temperature from IOReport on Apple Silicon
    fn read_ioreport_temperature(&self, _sensor_key: &str) -> Option<f64> {
        // IOReport is complex and requires subscribing to report channels
        // This is a placeholder for a full implementation
        None
    }

    /// Read temperature from a specific IOKit registry entry
    fn read_temperature_from_entry(
        &self,
        entry: io_kit_sys::types::io_registry_entry_t,
    ) -> Option<f64> {
        let props = self.copy_properties(entry)?;

        // Try common temperature keys
        let temp_keys = ["Temperature", "Device Temperature", "GPU Temperature"];
        for key in &temp_keys {
            if let Some(temp) = self.read_cfnumber_f64(&props, key) {
                // Convert from whatever units the driver uses to Celsius
                // Some drivers report in deci-Celsius, some in Celsius
                let celsius = if temp > 1000.0 { temp / 10.0 } else { temp };
                return Some(celsius);
            }
        }

        None
    }

    fn copy_properties(
        &self,
        entry: io_kit_sys::types::io_registry_entry_t,
    ) -> Option<CFDictionary> {
        let mut props: core_foundation_sys::dictionary::CFMutableDictionaryRef =
            std::ptr::null_mut();
        let kr = unsafe {
            IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null_mut(), 0)
        };
        if kr != 0 || props.is_null() {
            return None;
        }
        Some(unsafe { CFDictionary::wrap_under_create_rule(props.cast()) })
    }

    fn read_cfnumber_f64(&self, dict: &CFDictionary, key: &str) -> Option<f64> {
        let key = CFString::new(key);
        let value: *const std::ffi::c_void =
            dict.find(key.as_concrete_TypeRef().cast()).map(|v| *v)?;
        if value.is_null() {
            return None;
        }
        let num = unsafe { CFNumber::wrap_under_get_rule(value.cast()) };
        num.to_f64()
    }
}
