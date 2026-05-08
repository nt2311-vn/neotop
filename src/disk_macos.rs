//! disk_macos.rs — per-device read/write throughput on macOS using IOKit.
//!
//! Uses IOKit to enumerate IOMedia devices and query their statistics
//! from the I/O registry. Each disk's statistics include bytes read/written
//! and I/O time, which we use to compute throughput and utilization.

use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use io_kit_sys::{
    kIOMasterPortDefault, IOIteratorNext, IOObjectRelease, IORegistryEntryCreateCFProperties,
    IOServiceGetMatchingServices, IOServiceMatching,
};
use std::collections::HashMap;
use std::ffi::CString;
use std::time::Instant;

use crate::disk::{Disk, Sample, Tracker};

/// IOMedia class for disk devices
const IO_MEDIA_CLASS: &[u8] = b"IOMedia\0";

/// Statistics keys in IOKit's block storage driver statistics
const STATISTICS_KEY: &[u8] = b"Statistics\0";

/// Keys for individual statistics
const BYTES_READ_KEY: &str = "Bytes read";
const BYTES_WRITTEN_KEY: &str = "Bytes written";
const TOTAL_READ_TIME_KEY: &str = "Total read time";
const TOTAL_WRITE_TIME_KEY: &str = "Total write time";

impl Tracker {
    /// Snapshot disk statistics on macOS using IOKit
    pub(crate) fn snapshot_macos(&mut self, now: Instant) -> Vec<Disk> {
        let mut iter: io_kit_sys::types::io_iterator_t = 0;

        // SAFETY: `IOServiceMatching` returns a +1 retain on the
        // returned dictionary; `IOServiceGetMatchingServices`
        // consumes that retain. The iterator is released below.
        unsafe {
            let class = CString::from_vec_with_nul_unchecked(IO_MEDIA_CLASS.to_vec());
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
        let mut seen: Vec<String> = Vec::new();

        loop {
            // SAFETY: `IOIteratorNext` returns 0 when the iterator
            // is exhausted; we release every non-zero entry below.
            let entry = unsafe { IOIteratorNext(iter) };
            if entry == 0 {
                break;
            }

            if let Some(disk) = self.read_one_disk(entry, now) {
                if !seen.contains(&disk.name) {
                    seen.push(disk.name.clone());
                    out.push(disk);
                }
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

        out
    }

    fn read_one_disk(
        &mut self,
        entry: io_kit_sys::types::io_registry_entry_t,
        now: Instant,
    ) -> Option<Disk> {
        let name = self.get_disk_name(entry)?;
        let props = self.copy_properties(entry)?;
        let stats = self.get_statistics(&props)?;

        let bytes_read = self.read_cfnumber_u64(&stats, BYTES_READ_KEY).unwrap_or(0);
        let bytes_written = self
            .read_cfnumber_u64(&stats, BYTES_WRITTEN_KEY)
            .unwrap_or(0);
        let read_time_ns = self
            .read_cfnumber_u64(&stats, TOTAL_READ_TIME_KEY)
            .unwrap_or(0);
        let write_time_ns = self
            .read_cfnumber_u64(&stats, TOTAL_WRITE_TIME_KEY)
            .unwrap_or(0);

        // Convert nanoseconds to milliseconds for consistency with Linux
        let time_io_ms = (read_time_ns + write_time_ns) / 1_000_000;

        // Convert bytes to sectors for consistency with Linux (512-byte sectors)
        let sectors_read = bytes_read / 512;
        let sectors_written = bytes_written / 512;

        let cur = Sample {
            when: now,
            sectors_read,
            sectors_written,
            time_io_ms,
        };

        let (read_bps, write_bps, util_pct) = match self.prev.get(&name) {
            Some(p) => {
                let wall_secs = now.duration_since(p.when).as_secs_f64();
                if wall_secs > 0.0 {
                    let dread = cur
                        .sectors_read
                        .saturating_sub(p.sectors_read)
                        .saturating_mul(512);
                    let dwrite = cur
                        .sectors_written
                        .saturating_sub(p.sectors_written)
                        .saturating_mul(512);
                    let dt_ms = cur.time_io_ms.saturating_sub(p.time_io_ms);

                    let read_bps = if dread > 0 {
                        Some((dread as f64 / wall_secs) as u64)
                    } else {
                        None
                    };
                    let write_bps = if dwrite > 0 {
                        Some((dwrite as f64 / wall_secs) as u64)
                    } else {
                        None
                    };
                    let util_pct = if dt_ms > 0 {
                        Some((dt_ms as f64 / (wall_secs * 1000.0)) * 100.0)
                    } else {
                        None
                    };

                    (read_bps, write_bps, util_pct)
                } else {
                    (None, None, None)
                }
            }
            None => (None, None, None),
        };

        self.prev.insert(name.clone(), cur);

        Some(Disk {
            name,
            read_bps,
            write_bps,
            util_pct,
            read_bytes: bytes_read,
            write_bytes: bytes_written,
        })
    }

    fn get_disk_name(&self, entry: io_kit_sys::types::io_registry_entry_t) -> Option<String> {
        let mut buf = [0i8; 128];
        // SAFETY: `IORegistryEntryGetName` returns a NUL-terminated string
        let kr = unsafe { io_kit_sys::IORegistryEntryGetName(entry, buf.as_mut_ptr()) };
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

    fn copy_properties(
        &self,
        entry: io_kit_sys::types::io_registry_entry_t,
    ) -> Option<CFDictionary> {
        let mut props: core_foundation_sys::dictionary::CFMutableDictionaryRef =
            std::ptr::null_mut();
        // SAFETY: function fills `props` with a +1 retain on success;
        // we take ownership via `wrap_under_create_rule`.
        let kr = unsafe {
            IORegistryEntryCreateCFProperties(entry, &mut props, std::ptr::null_mut(), 0)
        };
        if kr != 0 || props.is_null() {
            return None;
        }
        // SAFETY: `props` is a +1 retained CFMutableDictionary.
        Some(unsafe { CFDictionary::wrap_under_create_rule(props.cast()) })
    }

    fn get_statistics(&self, props: &CFDictionary) -> Option<CFDictionary> {
        let key = CFString::new(std::str::from_utf8(STATISTICS_KEY).unwrap());
        let value: *const std::ffi::c_void =
            props.find(key.as_concrete_TypeRef().cast()).map(|v| *v)?;
        if value.is_null() {
            return None;
        }
        // SAFETY: `value` is a CFDictionary; borrow without retaining.
        let raw: CFDictionaryRef = value.cast();
        Some(unsafe { CFDictionary::wrap_under_get_rule(raw) })
    }

    fn read_cfnumber_u64(&self, dict: &CFDictionary, key: &str) -> Option<u64> {
        let key = CFString::new(key);
        // SAFETY: `find` returns a borrowed reference into `dict`.
        let value: *const std::ffi::c_void =
            dict.find(key.as_concrete_TypeRef().cast()).map(|v| *v)?;
        if value.is_null() {
            return None;
        }
        // SAFETY: `value` is a CFNumber; we do not retain it.
        let num = unsafe { CFNumber::wrap_under_get_rule(value.cast()) };
        num.to_i64().and_then(|n| u64::try_from(n).ok())
    }
}
