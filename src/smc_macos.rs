//! smc_macos.rs — AppleSMC user-client temperature reader.
//!
//! macOS exposes hardware sensors through the **System Management
//! Controller** (SMC). On Intel Macs the SMC is a separate
//! microcontroller; on Apple Silicon it's a firmware service
//! presenting the same protocol. Either way we talk to it through
//! the `AppleSMC` IOKit user-client.
//!
//! Reading a sensor is three calls:
//!
//! 1. `IOServiceOpen("AppleSMC")` → `io_connect_t`.
//! 2. `IOConnectCallStructMethod(conn, KERNEL_INDEX_SMC_GETKEYINFO,
//!    in, in_size, out, &out_size)` — returns the value's type
//!    code (e.g. `b"sp78"`) and byte length.
//! 3. `IOConnectCallStructMethod(conn, KERNEL_INDEX_SMC_READBYTES,
//!    …)` — returns the raw bytes.
//!
//! Decoding depends on the type code:
//!
//! * `sp78` — signed 16-bit fixed-point (8 integer bits, 8
//!   fractional). Used by most Intel TC* / TG* / Ts* sensors.
//! * `flt ` — little-endian IEEE-754 binary32. Used by Apple
//!   Silicon Tp* and Tg* sensors.
//! * `ui8 ` / `ui16` / `ui32` — unsigned integers (rare for temp).
//!
//! Keys probed in order; the first that returns a sensible reading
//! wins. We don't enumerate all SMC keys because there are
//! hundreds; just the well-known thermal ones.

use io_kit_sys::types::io_connect_t;
use io_kit_sys::{
    kIOMasterPortDefault, IOConnectCallStructMethod, IOIteratorNext, IOObjectRelease,
    IOServiceClose, IOServiceGetMatchingServices, IOServiceMatching, IOServiceOpen,
};
use std::ffi::CString;

/// `IOConnectCallStructMethod` selector for `SMC_CMD_READ_KEYINFO`.
const KERNEL_INDEX_SMC_GETKEYINFO: u32 = 9;
/// `IOConnectCallStructMethod` selector for `SMC_CMD_READ_BYTES`.
const KERNEL_INDEX_SMC_READBYTES: u32 = 5;

/// `SMCKeyData` matches the kernel-side struct exactly. Padding /
/// ordering is fixed by Apple's SMC ABI; we must `repr(C, packed)`
/// to keep wire-compat across compiler versions.
#[repr(C, packed)]
#[derive(Default, Clone, Copy)]
struct SmcKeyData {
    key: u32,
    vers: SmcVersion,
    p_limit_data: SmcPlimitData,
    key_info: SmcKeyInfo,
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; 32],
}

#[repr(C, packed)]
#[derive(Default, Clone, Copy)]
struct SmcVersion {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C, packed)]
#[derive(Default, Clone, Copy)]
struct SmcPlimitData {
    version: u16,
    length: u16,
    cpu_plimit: u32,
    gpu_plimit: u32,
    mem_plimit: u32,
}

#[repr(C, packed)]
#[derive(Default, Clone, Copy)]
struct SmcKeyInfo {
    data_size: u32,
    data_type: u32,
    data_attributes: u8,
}

/// Persistent connection to `AppleSMC`. Opening costs one IOKit
/// round-trip; we hold the connection open for the lifetime of the
/// process so the temp scanner pays it once.
#[derive(Debug)]
pub(crate) struct SmcClient {
    conn: io_connect_t,
}

impl Drop for SmcClient {
    fn drop(&mut self) {
        if self.conn != 0 {
            // SAFETY: `conn` is a valid `io_connect_t` from a
            // successful `IOServiceOpen` and not yet released.
            unsafe {
                IOServiceClose(self.conn);
            }
        }
    }
}

impl SmcClient {
    /// Open a connection to `AppleSMC`. Returns `None` if the
    /// service isn't matchable (older macOS or non-Apple hardware)
    /// or `IOServiceOpen` fails (permissions, sandbox).
    pub(crate) fn open() -> Option<Self> {
        // SAFETY: every call writes through a fresh stack slot.
        // We release the iterator + service objects regardless of
        // the `IOServiceOpen` outcome to avoid leaks.
        unsafe {
            let class = CString::new("AppleSMC").ok()?;
            let matching = IOServiceMatching(class.as_ptr());
            if matching.is_null() {
                return None;
            }

            let mut iter: io_kit_sys::types::io_iterator_t = 0;
            if IOServiceGetMatchingServices(kIOMasterPortDefault, matching, &mut iter) != 0 {
                return None;
            }

            let service = IOIteratorNext(iter);
            IOObjectRelease(iter);
            if service == 0 {
                return None;
            }

            let mut conn: io_connect_t = 0;
            let kr = IOServiceOpen(service, mach2::traps::mach_task_self(), 0, &mut conn);
            IOObjectRelease(service);
            if kr != 0 || conn == 0 {
                return None;
            }
            Some(Self { conn })
        }
    }

    /// Read a key's value as Celsius. `key` is the four-byte SMC
    /// identifier (e.g. `b"TC0P"`). Returns `None` if the key is
    /// missing, the data type isn't recognised, or the kernel
    /// returned an error.
    pub(crate) fn read_temperature(&self, key: &[u8; 4]) -> Option<f64> {
        let info = self.read_key_info(key)?;
        if info.0 == 0 || info.0 > 32 {
            return None;
        }
        let bytes = self.read_key_bytes(key, info.0)?;
        decode_temp(info.1, &bytes[..info.0 as usize])
    }

    /// First step: ask the SMC for the key's data type + size.
    fn read_key_info(&self, key: &[u8; 4]) -> Option<(u32, u32)> {
        let mut input = SmcKeyData {
            key: fourcc(key),
            data8: 9, // SMC_CMD_READ_KEYINFO
            ..SmcKeyData::default()
        };
        let mut output = SmcKeyData::default();
        let mut output_size = std::mem::size_of::<SmcKeyData>();

        // SAFETY: both buffers are owned local stack slots of the
        // exact size the kernel expects.
        let kr = unsafe {
            IOConnectCallStructMethod(
                self.conn,
                KERNEL_INDEX_SMC_GETKEYINFO,
                std::ptr::addr_of!(input).cast(),
                std::mem::size_of::<SmcKeyData>(),
                std::ptr::addr_of_mut!(output).cast(),
                &mut output_size,
            )
        };
        if kr != 0 || output.result != 0 {
            return None;
        }
        let _ = std::ptr::addr_of_mut!(input);
        let size = unsafe { std::ptr::addr_of!(output.key_info.data_size).read_unaligned() };
        let dtype = unsafe { std::ptr::addr_of!(output.key_info.data_type).read_unaligned() };
        Some((size, dtype))
    }

    /// Second step: ask the SMC for the raw bytes of the key.
    fn read_key_bytes(&self, key: &[u8; 4], size: u32) -> Option<[u8; 32]> {
        let mut input = SmcKeyData {
            key: fourcc(key),
            key_info: SmcKeyInfo {
                data_size: size,
                ..SmcKeyInfo::default()
            },
            data8: 5, // SMC_CMD_READ_BYTES
            ..SmcKeyData::default()
        };
        let mut output = SmcKeyData::default();
        let mut output_size = std::mem::size_of::<SmcKeyData>();

        // SAFETY: same buffer ownership as `read_key_info`.
        let kr = unsafe {
            IOConnectCallStructMethod(
                self.conn,
                KERNEL_INDEX_SMC_READBYTES,
                std::ptr::addr_of!(input).cast(),
                std::mem::size_of::<SmcKeyData>(),
                std::ptr::addr_of_mut!(output).cast(),
                &mut output_size,
            )
        };
        if kr != 0 || output.result != 0 {
            return None;
        }
        Some(output.bytes)
    }
}

/// Pack a four-byte SMC key into the big-endian `u32` the kernel
/// expects. `b"TC0P"` → `0x5443_3050`.
fn fourcc(k: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*k)
}

/// Decode an SMC reading into Celsius based on its type code.
/// Type codes are themselves four-byte big-endian FourCCs:
///
/// * `"sp78"` — Intel-style signed 16-bit fixed-point (8.8). Two
///   bytes, big-endian. Divide by 256.
/// * `"flt "` — Apple Silicon-style IEEE-754 binary32, little
///   endian. Four bytes, value is already in Celsius.
/// * `"ui8 "` — unsigned byte; rare for thermal sensors but a few
///   ASIC keys use it. Returned as-is in Celsius.
///
/// Anything else returns `None` so the caller skips the sensor.
fn decode_temp(data_type: u32, bytes: &[u8]) -> Option<f64> {
    match &data_type.to_be_bytes() {
        b"sp78" if bytes.len() >= 2 => {
            let raw = i16::from_be_bytes([bytes[0], bytes[1]]);
            Some(f64::from(raw) / 256.0)
        }
        b"flt " if bytes.len() >= 4 => {
            let v = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            if v.is_finite() {
                Some(f64::from(v))
            } else {
                None
            }
        }
        b"ui8 " if !bytes.is_empty() => Some(f64::from(bytes[0])),
        _ => None,
    }
}

/// Canonical SMC keys + human labels we try on every host. The
/// list is the union of the well-known Intel and Apple Silicon
/// thermal vocabulary — keys that aren't present return `None` at
/// read time, so probing the union is cheap and platform-agnostic.
///
/// * `TC0*` / `TC1*` — Intel CPU sensors (proximity, die, etc.).
/// * `TG0*` — Intel GPU sensors.
/// * `Ts0*` / `Ts1*` — Intel skin / palm-rest sensors.
/// * `Tp0*` / `Tp1*` / `Tp2*` — Apple Silicon CPU P-cluster
///   sensors (M1 has 4 cores: Tp01..Tp04, M1 Pro/Max more).
/// * `Te0*` — Apple Silicon CPU E-cluster.
/// * `Tg0*` — Apple Silicon GPU clusters.
/// * `TaLP` / `TaRP` — Apple Silicon ambient L/R.
pub(crate) const SENSOR_KEYS: &[(&[u8; 4], &str)] = &[
    // Intel
    (b"TC0P", "CPU"),
    (b"TC0D", "CPU die"),
    (b"TC0E", "CPU efficient"),
    (b"TG0P", "GPU"),
    (b"TG0D", "GPU die"),
    (b"Ts0P", "skin"),
    (b"Ts1P", "skin"),
    (b"TA0P", "ambient"),
    (b"TB0T", "battery"),
    (b"TB1T", "battery"),
    // Apple Silicon (M1 / M2 / M3 P + E cores).
    (b"Tp01", "P-core 1"),
    (b"Tp05", "P-core 2"),
    (b"Tp09", "P-core 3"),
    (b"Tp0D", "P-core 4"),
    (b"Tp0b", "P-core 5"),
    (b"Tp0f", "P-core 6"),
    (b"Te05", "E-core 1"),
    (b"Te0L", "E-core 2"),
    (b"Tg05", "GPU 1"),
    (b"Tg0D", "GPU 2"),
    (b"Tg0L", "GPU 3"),
    (b"Tg0T", "GPU 4"),
    (b"TaLP", "ambient L"),
    (b"TaRP", "ambient R"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fourcc_packs_big_endian() {
        assert_eq!(fourcc(b"TC0P"), 0x5443_3050);
        assert_eq!(fourcc(b"flt "), 0x666c_7420);
        assert_eq!(fourcc(b"sp78"), 0x7370_3738);
    }

    #[test]
    fn decode_sp78_signed_fixed_point() {
        // 0x2C00 == 44.00 °C, classic CPU package idle reading.
        let bytes = [0x2C, 0x00];
        let v = decode_temp(fourcc(b"sp78"), &bytes).unwrap();
        assert!((v - 44.0).abs() < 1e-9);

        // Negative reading (Apple SMC fan sensors will never
        // report this for temps but the format is signed).
        let bytes = [0xFF, 0x00];
        let v = decode_temp(fourcc(b"sp78"), &bytes).unwrap();
        assert!((v - (-1.0)).abs() < 1e-9);
    }

    #[test]
    fn decode_flt_little_endian_f32() {
        // 32.5 °C as IEEE-754 binary32 little-endian = 0x42020000
        let bytes = 32.5_f32.to_le_bytes();
        let v = decode_temp(fourcc(b"flt "), &bytes).unwrap();
        assert!((v - 32.5).abs() < 1e-6);
    }

    #[test]
    fn decode_unknown_type_returns_none() {
        assert!(decode_temp(fourcc(b"abcd"), &[0u8; 4]).is_none());
    }

    #[test]
    fn decode_short_buffer_returns_none() {
        // sp78 needs 2 bytes, flt needs 4.
        assert!(decode_temp(fourcc(b"sp78"), &[0u8; 1]).is_none());
        assert!(decode_temp(fourcc(b"flt "), &[0u8; 3]).is_none());
    }
}
