//! net.rs — per-interface RX/TX byte rates.
//! Linux: `/proc/net/dev`. macOS: sysctl-based implementation.

use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;
use std::time::Instant;

use crate::errors::ErrorRing;

#[cfg(target_os = "macos")]
use libc::{c_int, c_void, size_t};

#[derive(Debug, Clone)]
#[allow(dead_code)] // rx_bytes/tx_bytes reserved for a future "total since boot" column
pub(crate) struct Iface {
    pub(crate) name: String,
    /// Bytes/second received. `None` until we have two samples.
    pub(crate) rx_rate: Option<u64>,
    pub(crate) tx_rate: Option<u64>,
    /// Cumulative counts — handy for the total-since-boot badge.
    pub(crate) rx_bytes: u64,
    pub(crate) tx_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    when: Instant,
    rx: u64,
    tx: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Tracker {
    prev: HashMap<String, Sample>,
}

impl Tracker {
    pub(crate) fn snapshot(&mut self, errors: &mut ErrorRing) -> Vec<Iface> {
        #[cfg(target_os = "linux")]
        {
            let raw = match fs::read_to_string("/proc/net/dev") {
                Ok(r) => r,
                Err(e) => {
                    errors.push("net", format!("/proc/net/dev: {e}"));
                    return Vec::new();
                }
            };
            self.snapshot_from_str(&raw, Instant::now())
        }
        #[cfg(target_os = "macos")]
        {
            self.snapshot_macos(Instant::now())
        }
    }

    #[cfg(target_os = "macos")]
    fn snapshot_macos(&mut self, now: Instant) -> Vec<Iface> {
        // macOS network stats via sysctl net.iflist
        // This is complex to implement in pure Rust due to the binary format.
        // For now, return empty. A future implementation could:
        // 1. Use `getifaddrs` from libc to enumerate interfaces
        // 2. Query per-interface stats using sysctl with IF_DATA_* MIBs
        // 3. Parse `netstat -ib` output as a fallback
        Vec::new()
    }

    /// Test seam: same logic as `snapshot`, but takes the raw file
    /// content + a clock value so callers can verify rate computation
    /// without touching the real filesystem.
    #[cfg(target_os = "linux")]
    pub(crate) fn snapshot_from_str(&mut self, raw: &str, now: Instant) -> Vec<Iface> {
        let mut ifaces = Vec::new();
        let mut seen: Vec<String> = Vec::new();

        for (name, rx_bytes, tx_bytes) in parse_proc_net_dev(raw) {
            if skip_iface(&name) {
                continue;
            }
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
            seen.push(name.clone());

            ifaces.push(Iface {
                name,
                rx_rate,
                tx_rate,
                rx_bytes,
                tx_bytes,
            });
        }

        // Drop disappeared ifaces (unplugged USB adapters, etc).
        self.prev.retain(|k, _| seen.contains(k));
        ifaces
    }
}

/// Parse `/proc/net/dev` into `(name, rx_bytes, tx_bytes)` triples.
/// The first two header lines are skipped. Lines with fewer than 9
/// numeric fields are ignored — that's the kernel's RX (8 fields) +
/// TX bytes column position.
pub(crate) fn parse_proc_net_dev(raw: &str) -> Vec<(String, u64, u64)> {
    let mut out = Vec::new();
    for line in raw.lines().skip(2) {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let parts: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        // Columns: rx_bytes, rx_packets, rx_errs, rx_drop, rx_fifo,
        //          rx_frame, rx_compressed, rx_multicast,
        //          tx_bytes, tx_packets, ...
        if parts.len() < 9 {
            continue;
        }
        out.push((name.trim().to_string(), parts[0], parts[8]));
    }
    out
}

/// Interfaces we hide from the header line — they're rarely interesting
/// for a quick glance. `br-*` and `veth*` are Docker; `lo` is loopback.
fn skip_iface(name: &str) -> bool {
    name == "lo"
        || name.starts_with("br-")
        || name.starts_with("docker")
        || name.starts_with("veth")
        || name.starts_with("virbr")
}

/// Format a byte rate compactly: `"1.2 MB/s"`, `"45 KB/s"`, `"—"` when zero.
pub(crate) fn human_rate(bps: Option<u64>) -> String {
    let Some(b) = bps else {
        return "—".into();
    };
    if b == 0 {
        return "0".into();
    }
    // Base-10 for network, which is the industry convention (ISPs,
    // ethtool, iperf all report base-10).
    #[allow(clippy::cast_precision_loss)]
    let f = b as f64;
    if f >= 1e9 {
        format!("{:.1} GB/s", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1} MB/s", f / 1e6)
    } else if f >= 1e3 {
        format!("{:.0} KB/s", f / 1e3)
    } else {
        format!("{b} B/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const PROC_NET_DEV_FIXTURE: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1234567   100   0    0    0    0    0    0   1234567   100   0    0    0    0    0    0
  eth0: 1000      10    0    0    0    0    0    0   2000      20    0    0    0    0    0    0
 wlan0: 5000      50    0    0    0    0    0    0   3000      30    0    0    0    0    0    0
";

    #[test]
    fn parse_proc_net_dev_extracts_name_and_byte_columns() {
        let parsed = parse_proc_net_dev(PROC_NET_DEV_FIXTURE);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0], ("lo".into(), 1_234_567, 1_234_567));
        assert_eq!(parsed[1], ("eth0".into(), 1000, 2000));
        assert_eq!(parsed[2], ("wlan0".into(), 5000, 3000));
    }

    #[test]
    fn parse_proc_net_dev_skips_short_lines() {
        let raw = "\
Inter-| header
 face | header
broken: 1 2 3 4
ok:    100 1 0 0 0 0 0 0 200 1 0 0 0 0 0 0
";
        let parsed = parse_proc_net_dev(raw);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, "ok");
    }

    #[test]
    fn skip_iface_filters_lo_and_bridges() {
        assert!(skip_iface("lo"));
        assert!(skip_iface("docker0"));
        assert!(skip_iface("br-abc123"));
        assert!(skip_iface("veth9af"));
        assert!(skip_iface("virbr0"));
        assert!(!skip_iface("eth0"));
        assert!(!skip_iface("wlan0"));
        assert!(!skip_iface("enp0s3"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn snapshot_from_str_computes_rates_between_two_samples() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        // First sample establishes the baseline; rates must be None.
        let first = t.snapshot_from_str(PROC_NET_DEV_FIXTURE, t0);
        assert!(first.iter().all(|i| i.rx_rate.is_none()));

        // Second sample, 1s later, with eth0 having received +1000 bytes.
        let raw2 = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
  eth0: 2000      20    0    0    0    0    0    0   2000      20    0    0    0    0    0    0
 wlan0: 5000      50    0    0    0    0    0    0   3000      30    0    0    0    0    0    0
";
        let second = t.snapshot_from_str(raw2, t0 + Duration::from_secs(1));
        let eth0 = second.iter().find(|i| i.name == "eth0").expect("eth0");
        assert_eq!(eth0.rx_rate, Some(1000));
        // tx unchanged → rate 0 (not None).
        assert_eq!(eth0.tx_rate, Some(0));
    }

    #[test]
    fn human_rate_formats_compact_units() {
        assert_eq!(human_rate(None), "—");
        assert_eq!(human_rate(Some(0)), "0");
        assert_eq!(human_rate(Some(500)), "500 B/s");
        assert_eq!(human_rate(Some(2_500)), "2 KB/s");
        assert_eq!(human_rate(Some(2_500_000)), "2.5 MB/s");
        assert_eq!(human_rate(Some(2_500_000_000)), "2.5 GB/s");
    }
}
