//! net_macos.rs — per-interface network rates on macOS using sysctl.
//!
//! Uses sysctl with NET_RT_IFLIST2 to query 64-bit network statistics
//! for each interface. This avoids the 4GB overflow issue with getifaddrs.
//!
//! The sysctl CTL_NET, PF_ROUTE, 0, AF_INET, NET_RT_IFLIST2 returns
//! a binary structure containing interface names and statistics.

use libc::{c_char, c_int, c_uint, c_void, size_t};
use std::time::Instant;

use crate::net::{Iface, Sample};

/// sysctl MIB for NET_RT_IFLIST2
const NET_RT_IFLIST2: c_int = 6;

pub(crate) fn snapshot_macos(tracker: &mut crate::net::Tracker, now: Instant) -> Vec<Iface> {
    tracker.snapshot_macos_impl(now)
}

impl crate::net::Tracker {
    /// Snapshot network statistics on macOS using sysctl
    fn snapshot_macos_impl(&mut self, now: Instant) -> Vec<Iface> {
        // Build MIB for sysctl: CTL_NET, PF_ROUTE, 0, AF_INET, NET_RT_IFLIST2
        let mut mib: [c_int; 6] = [
            libc::CTL_NET,
            libc::PF_ROUTE,
            0,
            libc::AF_INET,
            NET_RT_IFLIST2,
            0,
        ];

        // First call to get required buffer size
        let mut len: size_t = 0;
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                6,
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };

        if result != 0 || len == 0 {
            return Vec::new();
        }

        // Allocate buffer and fetch data
        let mut buf: Vec<u8> = vec![0; len];
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                6,
                buf.as_mut_ptr() as *mut c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };

        if result != 0 {
            return Vec::new();
        }

        // Parse the binary data
        self.parse_iflist(&buf, now)
    }

    fn parse_iflist(&mut self, buf: &[u8], now: Instant) -> Vec<Iface> {
        let mut ifaces = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        let mut offset = 0;

        while offset < buf.len() {
            // Parse if_msghdr structure
            let ifm = unsafe { *(buf.as_ptr().add(offset) as *const libc::if_msghdr) };

            if i32::from(ifm.ifm_type) != libc::RTM_IFINFO2 {
                // Skip to next message
                offset += ifm.ifm_msglen as usize;
                continue;
            }

            // Get interface name from if_msghdr2
            let ifm2 = unsafe { *(buf.as_ptr().add(offset) as *const libc::if_msghdr2) };

            let Some(name) = if_index_to_name(c_uint::from(ifm2.ifm_index)) else {
                offset += ifm.ifm_msglen as usize;
                continue;
            };

            if self.skip_iface(&name) {
                offset += ifm.ifm_msglen as usize;
                continue;
            }

            // Get statistics from if_data64
            let rx_bytes = ifm2.ifm_data.ifi_ibytes;
            let tx_bytes = ifm2.ifm_data.ifi_obytes;

            let (rx_rate, tx_rate) = match self.prev.get(&name) {
                Some(p) => {
                    let dt = now.duration_since(p.when).as_secs_f64();
                    if dt > 0.0 {
                        #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
                        #[allow(clippy::cast_possible_truncation)]
                        {
                            let r = (rx_bytes.saturating_sub(p.rx) as f64 / dt) as u64;
                            let t = (tx_bytes.saturating_sub(p.tx) as f64 / dt) as u64;
                            (Some(r), Some(t))
                        }
                    } else {
                        (None, None)
                    }
                }
                None => (None, None),
            };

            self.prev.insert(
                name.clone(),
                Sample {
                    when: now,
                    rx: rx_bytes,
                    tx: tx_bytes,
                },
            );

            if !seen.contains(&name) {
                seen.push(name.clone());
                ifaces.push(Iface {
                    name,
                    rx_rate,
                    tx_rate,
                    rx_bytes,
                    tx_bytes,
                });
            }

            offset += ifm.ifm_msglen as usize;
        }

        ifaces
    }

    fn skip_iface(&self, name: &str) -> bool {
        // Skip loopback and virtual interfaces
        name == "lo0"
            || name.starts_with("lo")
            || name.starts_with("utun")
            || name.starts_with("tun")
    }
}

/// Resolve a kernel interface index to its name via `if_indextoname(3)`.
///
/// Returns `None` when the index is unknown (interface vanished between
/// the sysctl snapshot and this call) or the kernel-returned name is not
/// valid UTF-8.
fn if_index_to_name(index: c_uint) -> Option<String> {
    // IFNAMSIZ is 16 on Darwin and already includes the trailing NUL.
    let mut buf: [c_char; libc::IFNAMSIZ] = [0; libc::IFNAMSIZ];
    // SAFETY: `buf` is a valid writable buffer of IF_NAMESIZE bytes, which
    // is the contract `if_indextoname` documents. The returned pointer
    // either equals `buf.as_mut_ptr()` on success or is null on failure.
    let ptr = unsafe { libc::if_indextoname(index, buf.as_mut_ptr()) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `ptr` points into `buf`, which is null-terminated by libc.
    let cstr = unsafe { std::ffi::CStr::from_ptr(ptr) };
    cstr.to_str().ok().map(str::to_owned)
}
