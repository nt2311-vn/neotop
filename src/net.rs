//! net.rs — per-interface RX/TX byte rates from `/proc/net/dev`.
//!
//! We sample cumulative byte counts on each scan and turn them into a
//! rate by dividing the delta by the wall-clock time since the last
//! sample. First scan has no delta — `rx_rate`/`tx_rate` are `None`.
//!
//! Loopback and docker bridges are filtered out by default; they
//! aren't informative for a "is anything talking to the outside"
//! glance.

use std::collections::HashMap;
use std::fs;
use std::time::Instant;

use crate::errors::ErrorRing;

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
    /// Take a fresh snapshot; `prev` is updated for the next call.
    pub(crate) fn snapshot(&mut self, errors: &mut ErrorRing) -> Vec<Iface> {
        let raw = match fs::read_to_string("/proc/net/dev") {
            Ok(r) => r,
            Err(e) => {
                errors.push("net", format!("/proc/net/dev: {e}"));
                return Vec::new();
            }
        };
        let now = Instant::now();
        let mut ifaces = Vec::new();
        let mut seen: Vec<String> = Vec::new();

        // First two lines are the header.
        for line in raw.lines().skip(2) {
            let Some((name, rest)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim().to_string();
            if skip_iface(&name) {
                continue;
            }
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
            let rx_bytes = parts[0];
            let tx_bytes = parts[8];

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
