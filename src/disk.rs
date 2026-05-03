//! disk.rs — per-device read/write throughput + utilisation.
//! Linux: `/proc/diskstats`. macOS: sysctl-based implementation.

use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;
use std::time::Instant;

use crate::errors::ErrorRing;

#[cfg(target_os = "macos")]
use libc::{c_int, c_void, size_t};

/// Bytes per disk sector. The kernel always reports in 512-byte units
/// regardless of physical sector size — see `Documentation/iostats.rst`.
const SECTOR_BYTES: u64 = 512;

#[derive(Debug, Clone)]
#[allow(dead_code)] // read_bytes/write_bytes reserved for "since boot" totals
pub(crate) struct Disk {
    pub(crate) name: String,
    /// Bytes/second read. `None` until two samples are available.
    pub(crate) read_bps: Option<u64>,
    pub(crate) write_bps: Option<u64>,
    /// Fraction of wall-clock time the device spent doing I/O, as a
    /// percentage 0..=100. `None` until we have two samples.
    pub(crate) util_pct: Option<f64>,
    /// Cumulative byte counts since boot.
    pub(crate) read_bytes: u64,
    pub(crate) write_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    when: Instant,
    sectors_read: u64,
    sectors_written: u64,
    time_io_ms: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Tracker {
    prev: HashMap<String, Sample>,
}

impl Tracker {
    pub(crate) fn snapshot(&mut self, errors: &mut ErrorRing) -> Vec<Disk> {
        #[cfg(target_os = "linux")]
        {
            let raw = match fs::read_to_string("/proc/diskstats") {
                Ok(r) => r,
                Err(e) => {
                    errors.push("disk", format!("/proc/diskstats: {e}"));
                    return Vec::new();
                }
            };
            self.snapshot_from_str(&raw, Instant::now())
        }
        #[cfg(target_os = "macos")]
        {
            // macOS disk I/O monitoring requires IOKit framework which is complex
            // to bind from pure Rust. For now, return empty. A future implementation
            // could use the `core-foundation-sys` and `io-kit-sys` crates to query
            // IOKit for disk statistics, or parse `iostat -d` output.
            Vec::new()
        }
    }

    /// Test seam: same logic as `snapshot` but takes the raw file
    /// contents + a clock value so callers can verify rate computation
    /// without `/proc`.
    pub(crate) fn snapshot_from_str(&mut self, raw: &str, now: Instant) -> Vec<Disk> {
        let mut out = Vec::new();
        let mut seen: Vec<String> = Vec::new();

        for row in parse_diskstats(raw) {
            if !is_physical_disk(&row.name) {
                continue;
            }
            let cur = Sample {
                when: now,
                sectors_read: row.sectors_read,
                sectors_written: row.sectors_written,
                time_io_ms: row.time_io_ms,
            };
            let (read_bps, write_bps, util_pct) = match self.prev.get(&row.name) {
                Some(p) => {
                    let wall_secs = now.duration_since(p.when).as_secs_f64();
                    if wall_secs > 0.0 {
                        #[allow(
                            clippy::cast_precision_loss,
                            clippy::cast_sign_loss,
                            clippy::cast_possible_truncation
                        )]
                        {
                            let dread = cur
                                .sectors_read
                                .saturating_sub(p.sectors_read)
                                .saturating_mul(SECTOR_BYTES);
                            let dwrite = cur
                                .sectors_written
                                .saturating_sub(p.sectors_written)
                                .saturating_mul(SECTOR_BYTES);
                            let busy_ms = cur.time_io_ms.saturating_sub(p.time_io_ms) as f64;
                            let wall_ms = wall_secs * 1000.0;
                            let util = if wall_ms > 0.0 {
                                (busy_ms / wall_ms * 100.0).clamp(0.0, 100.0)
                            } else {
                                0.0
                            };
                            (
                                Some((dread as f64 / wall_secs) as u64),
                                Some((dwrite as f64 / wall_secs) as u64),
                                Some(util),
                            )
                        }
                    } else {
                        (None, None, None)
                    }
                }
                None => (None, None, None),
            };
            self.prev.insert(row.name.clone(), cur);
            seen.push(row.name.clone());

            out.push(Disk {
                name: row.name,
                read_bps,
                write_bps,
                util_pct,
                read_bytes: row.sectors_read.saturating_mul(SECTOR_BYTES),
                write_bytes: row.sectors_written.saturating_mul(SECTOR_BYTES),
            });
        }

        // Drop disappeared devices (hot-unplugged USB, removed mounts).
        self.prev.retain(|k, _| seen.contains(k));
        out
    }
}

/// Pick up to `limit` devices most worth showing — by combined I/O
/// rate first, falling back to alphabetical order when nothing has
/// activity yet.
pub(crate) fn highlights(disks: &[Disk], limit: usize) -> Vec<&Disk> {
    let mut sorted: Vec<&Disk> = disks.iter().collect();
    sorted.sort_by(|a, b| {
        let ar = a
            .read_bps
            .unwrap_or(0)
            .saturating_add(a.write_bps.unwrap_or(0));
        let br = b
            .read_bps
            .unwrap_or(0)
            .saturating_add(b.write_bps.unwrap_or(0));
        br.cmp(&ar).then_with(|| a.name.cmp(&b.name))
    });
    sorted.into_iter().take(limit).collect()
}

#[derive(Debug, Clone)]
struct Row {
    name: String,
    sectors_read: u64,
    sectors_written: u64,
    time_io_ms: u64,
}

/// Pure parser for `/proc/diskstats`. Lines with fewer than 14
/// whitespace-separated fields (the kernel pre-4.18 format) are
/// skipped; we don't bother to support kernels older than that.
fn parse_diskstats(raw: &str) -> Vec<Row> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 14 {
            continue;
        }
        // parts[0]=major, [1]=minor, [2]=name, [3..]=stats
        let name = parts[2].to_string();
        let Some(sectors_read) = parts[5].parse::<u64>().ok() else {
            continue;
        };
        let Some(sectors_written) = parts[9].parse::<u64>().ok() else {
            continue;
        };
        let Some(time_io_ms) = parts[12].parse::<u64>().ok() else {
            continue;
        };
        out.push(Row {
            name,
            sectors_read,
            sectors_written,
            time_io_ms,
        });
    }
    out
}

/// Heuristic for "real disk worth showing in a top-level view":
/// keep whole-disk entries (`nvme0n1`, `sda`, `vda`, `mmcblk0`,
/// `xvda`, `hd*`) and reject partitions, loops, ramdisks, and
/// device-mapper / md-raid virtuals.
fn is_physical_disk(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.starts_with("loop")
        || name.starts_with("ram")
        || name.starts_with("dm-")
        || name.starts_with("md")
        || name.starts_with("zram")
    {
        return false;
    }
    // nvme0n1 = whole disk; nvme0n1p1 = partition.
    if name.starts_with("nvme") {
        // Skip if there's a `pN` suffix.
        if let Some(idx) = name.find('p') {
            // The 'p' must come *after* the 'nN' part (e.g. nvme0n1p1).
            // A bare `nvme0n1` has no 'p' after `n`.
            let after_p = &name[idx + 1..];
            if !after_p.is_empty() && after_p.chars().all(|c| c.is_ascii_digit()) {
                return false;
            }
        }
        return true;
    }
    // sda, sdb, vda, hda, xvda, etc. — whole disks have no trailing digit.
    // sda1, sda2 — partitions, end in digits.
    if name.starts_with("sd")
        || name.starts_with("vd")
        || name.starts_with("hd")
        || name.starts_with("xvd")
    {
        return !name.chars().last().is_some_and(|c| c.is_ascii_digit());
    }
    // mmcblk0 = whole disk; mmcblk0p1 = partition.
    if name.starts_with("mmcblk") {
        return !name.contains('p');
    }
    // Anything else: include by default — if a user has a weird
    // driver they probably want to see it.
    true
}

/// Compact display for a B/s rate. Mirrors `net::human_rate` but uses
/// binary-ish prefixes (KB / MB / GB) at base-10, which is what
/// `iostat` does. Returns `"—"` for `None`.
pub(crate) fn human_rate(bps: Option<u64>) -> String {
    let Some(b) = bps else {
        return "—".into();
    };
    if b == 0 {
        return "0".into();
    }
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

    const DISKSTATS_FIXTURE: &str = "\
   7       0 loop0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
   8       0 sda 12345 0 1000000 5000 6789 0 500000 3000 0 8000 8000 0 0 0 0
   8       1 sda1 100 0 200 50 50 0 100 25 0 75 75 0 0 0 0
 259       0 nvme0n1 99 0 9999 100 50 0 5000 80 0 200 200 0 0 0 0
 259       1 nvme0n1p1 5 0 50 10 5 0 50 8 0 18 18 0 0 0 0
 253       0 dm-0 1 0 1 0 1 0 1 0 0 0 0 0 0 0 0
";

    #[test]
    fn parse_diskstats_extracts_required_columns() {
        let rows = parse_diskstats(DISKSTATS_FIXTURE);
        // 6 lines pass the field-count gate; we don't filter here.
        assert_eq!(rows.len(), 6);
        let sda = rows.iter().find(|r| r.name == "sda").unwrap();
        assert_eq!(sda.sectors_read, 1_000_000);
        assert_eq!(sda.sectors_written, 500_000);
        assert_eq!(sda.time_io_ms, 8000);
    }

    #[test]
    fn parse_diskstats_skips_short_lines() {
        let rows = parse_diskstats("8 0 sda 1 2 3\n");
        assert!(rows.is_empty());
    }

    #[test]
    fn is_physical_disk_keeps_whole_disks() {
        assert!(is_physical_disk("sda"));
        assert!(is_physical_disk("vdb"));
        assert!(is_physical_disk("hda"));
        assert!(is_physical_disk("xvdf"));
        assert!(is_physical_disk("nvme0n1"));
        assert!(is_physical_disk("mmcblk0"));
    }

    #[test]
    fn is_physical_disk_drops_partitions() {
        assert!(!is_physical_disk("sda1"));
        assert!(!is_physical_disk("sdb12"));
        assert!(!is_physical_disk("nvme0n1p1"));
        assert!(!is_physical_disk("mmcblk0p2"));
        assert!(!is_physical_disk("xvda1"));
    }

    #[test]
    fn is_physical_disk_drops_virtuals() {
        assert!(!is_physical_disk("loop0"));
        assert!(!is_physical_disk("loop12"));
        assert!(!is_physical_disk("ram0"));
        assert!(!is_physical_disk("dm-0"));
        assert!(!is_physical_disk("md0"));
        assert!(!is_physical_disk("zram0"));
    }

    #[test]
    fn snapshot_from_str_filters_then_computes_rates() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let first = t.snapshot_from_str(DISKSTATS_FIXTURE, t0);
        // sda + nvme0n1 are the only two physical disks in the fixture.
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|d| d.read_bps.is_none()));

        // Second sample, +1s, sda did 1024 more sectors of read (= 512 KiB)
        // and spent 1000 ms on I/O during the gap.
        let raw2 = "\
   7       0 loop0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0
   8       0 sda 12345 0 1001024 5000 6789 0 500000 3000 0 9000 9000 0 0 0 0
 259       0 nvme0n1 99 0 9999 100 50 0 5000 80 0 200 200 0 0 0 0
";
        let second = t.snapshot_from_str(raw2, t0 + Duration::from_secs(1));
        let sda = second.iter().find(|d| d.name == "sda").unwrap();
        assert_eq!(sda.read_bps, Some(1024 * SECTOR_BYTES));
        assert_eq!(sda.write_bps, Some(0));
        // 1000 ms / 1000 ms wall = 100% utilisation.
        let util = sda.util_pct.unwrap();
        assert!((util - 100.0).abs() < 1e-9);
    }

    #[test]
    fn highlights_orders_by_total_bps_then_name() {
        let disks = vec![
            Disk {
                name: "sda".into(),
                read_bps: Some(100),
                write_bps: Some(0),
                util_pct: None,
                read_bytes: 0,
                write_bytes: 0,
            },
            Disk {
                name: "nvme0n1".into(),
                read_bps: Some(500),
                write_bps: Some(500),
                util_pct: None,
                read_bytes: 0,
                write_bytes: 0,
            },
            Disk {
                name: "vdb".into(),
                read_bps: None,
                write_bps: None,
                util_pct: None,
                read_bytes: 0,
                write_bytes: 0,
            },
        ];
        let picks = highlights(&disks, 2);
        assert_eq!(picks[0].name, "nvme0n1");
        assert_eq!(picks[1].name, "sda");
    }

    #[test]
    fn human_rate_compact_units() {
        assert_eq!(human_rate(None), "—");
        assert_eq!(human_rate(Some(0)), "0");
        assert_eq!(human_rate(Some(800)), "800 B/s");
        assert_eq!(human_rate(Some(1500)), "2 KB/s");
        assert_eq!(human_rate(Some(2_500_000)), "2.5 MB/s");
        assert_eq!(human_rate(Some(2_500_000_000)), "2.5 GB/s");
    }
}
