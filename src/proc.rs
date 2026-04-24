//! proc.rs — tiny slice of `/proc/<pid>/` for the neotop UI.
//!
//! We intentionally avoid the `procfs` crate: the fields we need are
//! small, stable, and zero-dep. If a file is unreadable (process gone,
//! permission issue) every function returns `None` so the caller can
//! render "—" without ceremony.
//!
//! Fields and their sources:
//!
//! | Field          | Source                | Unit                    |
//! | -------------- | --------------------- | ----------------------- |
//! | `utime+stime`  | /proc/<pid>/stat (14,15) | clock ticks (jiffies) |
//! | `num_threads`  | /proc/<pid>/stat (20) | count                   |
//! | `state`        | /proc/<pid>/stat (3)  | R/S/D/Z/T/I letter      |
//! | `VmRSS`/`VmSize` | /proc/<pid>/status  | kB → bytes              |
//! | `limits`       | /proc/<pid>/limits    | per-rlimit column text  |

use std::fs;

/// Clock ticks per second on this host. Typical values: 100 (most
/// distros), 250, 300 (Arch stock), 1000 (`CachyOS`, low-latency
/// kernels). CPU-time maths depends on this, so we read it correctly
/// via `sysconf` (wrapped safely by `rustix`).
pub(crate) fn clk_tck() -> u64 {
    rustix::param::clock_ticks_per_second()
}

#[derive(Debug, Clone)]
pub(crate) struct Stat {
    /// Human-readable process state: "R (running)", "S (sleeping)", ...
    pub(crate) state: String,
    pub(crate) num_threads: i64,
    /// `utime + stime` in clock ticks — monotonically increasing.
    pub(crate) cpu_jiffies: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Mem {
    pub(crate) rss_bytes: u64,
    pub(crate) vsz_bytes: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct LimitRow {
    pub(crate) name: String,
    pub(crate) soft: String,
    pub(crate) hard: String,
    pub(crate) unit: String,
}

/// Cgroup v2 membership + accounting for a process.
///
/// `path` is the per-process cgroup read from `/proc/<pid>/cgroup`. On
/// a cgroup-v2-only host (systemd unified hierarchy — the default on
/// modern distros) this is a single line of the form
/// `0::/user.slice/user-1000.slice/session-2.scope`.
#[derive(Debug, Clone, Default)]
pub(crate) struct Cgroup {
    pub(crate) path: String,
    /// `memory.current` in bytes — what the kernel is accounting to
    /// this cgroup right now. Zero if unavailable.
    pub(crate) memory_current: u64,
    /// `memory.max` — the hard limit. `u64::MAX` means "max" (no limit).
    pub(crate) memory_max: u64,
}

/// Full process snapshot. Each call re-reads the files — they are
/// virtual and cheap.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub(crate) stat: Stat,
    pub(crate) mem: Mem,
    pub(crate) limits: Vec<LimitRow>,
    pub(crate) cgroup: Option<Cgroup>,
}

pub(crate) fn snapshot(pid: i64) -> Option<Snapshot> {
    let stat = read_stat(pid)?;
    let mem = read_mem(pid).unwrap_or_default();
    let limits = read_limits(pid).unwrap_or_default();
    let cgroup = read_cgroup(pid);
    Some(Snapshot {
        stat,
        mem,
        limits,
        cgroup,
    })
}

// -----------------------------------------------------------------------------
// /proc/<pid>/cgroup  +  /sys/fs/cgroup/<path>/memory.{current,max}
// -----------------------------------------------------------------------------

fn read_cgroup(pid: i64) -> Option<Cgroup> {
    let raw = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    // cgroup v2 line: `0::<path>`. v1 lines look like `<id>:<ctrl>:<path>`.
    // We prefer v2; fall back to any line if the host is v1-only.
    let path = raw
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .or_else(|| {
            raw.lines()
                .next()
                .and_then(|l| l.rsplit_once(':').map(|(_, p)| p))
        })?
        .to_string();

    let base = format!("/sys/fs/cgroup{path}");
    let memory_current = fs::read_to_string(format!("{base}/memory.current"))
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let memory_max = fs::read_to_string(format!("{base}/memory.max"))
        .ok()
        .map_or(u64::MAX, |s| {
            let t = s.trim();
            if t == "max" {
                u64::MAX
            } else {
                t.parse().unwrap_or(u64::MAX)
            }
        });

    Some(Cgroup {
        path,
        memory_current,
        memory_max,
    })
}

// -----------------------------------------------------------------------------
// /proc/<pid>/stat
// -----------------------------------------------------------------------------

fn read_stat(pid: i64) -> Option<Stat> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;

    // Format: `pid (comm) state ppid pgrp ... utime stime ...`
    //
    // `comm` is wrapped in parens and may itself contain spaces or
    // parens — so we split *after* the last `)`.
    let rparen = raw.rfind(')')?;
    let after = raw.get(rparen + 2..)?; // skip ") "
    let fields: Vec<&str> = after.split_whitespace().collect();

    // Index into `fields` (0-based, i.e. `state` is fields[0]):
    //   0  state              (R/S/D/Z/T/I)
    //   1  ppid
    //   2  pgrp
    //   3  session
    //   4  tty_nr
    //   5  tpgid
    //   6  flags
    //   7  minflt
    //   8  cminflt
    //   9  majflt
    //   10 cmajflt
    //   11 utime      ← jiffies user time
    //   12 stime      ← jiffies kernel time
    //   13 cutime
    //   14 cstime
    //   15 priority
    //   16 nice
    //   17 num_threads
    let state_char = fields.first()?.chars().next()?;
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let num_threads: i64 = fields.get(17)?.parse().ok()?;

    Some(Stat {
        state: describe_state(state_char),
        num_threads,
        cpu_jiffies: utime + stime,
    })
}

fn describe_state(c: char) -> String {
    let name = match c {
        'R' => "running",
        'S' => "sleeping",
        'D' => "disk-wait",
        'Z' => "zombie",
        'T' => "stopped",
        't' => "traced",
        'I' => "idle",
        'X' => "dead",
        _ => "unknown",
    };
    format!("{c} ({name})")
}

// -----------------------------------------------------------------------------
// /proc/<pid>/status  (for VmRSS / VmSize)
// -----------------------------------------------------------------------------

fn read_mem(pid: i64) -> Option<Mem> {
    let raw = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    Some(Mem {
        rss_bytes: kb_line(&raw, "VmRSS:").unwrap_or(0),
        vsz_bytes: kb_line(&raw, "VmSize:").unwrap_or(0),
    })
}

fn kb_line(status: &str, key: &str) -> Option<u64> {
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

// -----------------------------------------------------------------------------
// /proc/<pid>/limits  (fixed-column table)
// -----------------------------------------------------------------------------

fn read_limits(pid: i64) -> Option<Vec<LimitRow>> {
    let raw = fs::read_to_string(format!("/proc/{pid}/limits")).ok()?;
    let mut lines = raw.lines();

    // First line is the header. We use it to find where each column starts,
    // because the `Limit` column contains multi-word names like
    // "Max address space" — a naive whitespace split is wrong.
    let header = lines.next()?;
    let soft_col = header.find("Soft Limit")?;
    let hard_col = header.find("Hard Limit")?;
    let unit_col = header.find("Units")?;

    let slice = |line: &str, start: usize, end: Option<usize>| -> String {
        let end = end.unwrap_or(line.len()).min(line.len());
        let start = start.min(line.len());
        line.get(start..end).unwrap_or("").trim().to_string()
    };

    let mut rows = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        rows.push(LimitRow {
            name: slice(line, 0, Some(soft_col)),
            soft: slice(line, soft_col, Some(hard_col)),
            hard: slice(line, hard_col, Some(unit_col)),
            unit: slice(line, unit_col, None),
        });
    }
    Some(rows)
}

// -----------------------------------------------------------------------------
// Human formatting helpers
// -----------------------------------------------------------------------------

pub(crate) fn human_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    // We only format for display; f64 mantissa precision (~15 decimal
    // digits) is vastly more than a human needs for "MiB" / "GiB".
    #[allow(clippy::cast_precision_loss)]
    {
        if n >= GIB {
            format!("{:.1} GiB", n as f64 / GIB as f64)
        } else if n >= MIB {
            format!("{:.0} MiB", n as f64 / MIB as f64)
        } else if n >= KIB {
            format!("{:.0} KiB", n as f64 / KIB as f64)
        } else {
            format!("{n} B")
        }
    }
}

// -----------------------------------------------------------------------------
// /proc/self/* — used by the self-profiling perf footer
// -----------------------------------------------------------------------------

/// Cumulative `utime + stime` in clock ticks for *this* process. Used
/// by the perf footer to compute its own CPU%.
pub(crate) fn self_jiffies() -> Option<u64> {
    let raw = fs::read_to_string("/proc/self/stat").ok()?;
    let rparen = raw.rfind(')')?;
    let after = raw.get(rparen + 2..)?;
    let fields: Vec<&str> = after.split_whitespace().collect();
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

/// `VmRSS` for the current process, in bytes. Returns `None` if the
/// field is missing — should never happen on Linux but the parser is
/// robust to it.
pub(crate) fn self_rss_bytes() -> Option<u64> {
    let raw = fs::read_to_string("/proc/self/status").ok()?;
    kb_line(&raw, "VmRSS:")
}

/// Compact limit value: "unlimited" stays short; pure numbers get byte
/// formatting if the unit looks like bytes.
pub(crate) fn format_limit_value(raw: &str, unit: &str) -> String {
    if raw == "unlimited" {
        return "∞".into();
    }
    if unit == "bytes" {
        if let Ok(n) = raw.parse::<u64>() {
            return human_bytes(n);
        }
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_thresholds() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1 MiB");
        // 1.5 GiB → "1.5 GiB"
        assert_eq!(human_bytes(1024 * 1024 * 1024 * 3 / 2), "1.5 GiB");
    }

    #[test]
    fn format_limit_value_handles_unlimited() {
        assert_eq!(format_limit_value("unlimited", "bytes"), "∞");
        assert_eq!(format_limit_value("unlimited", "us"), "∞");
    }

    #[test]
    fn format_limit_value_humanises_byte_units() {
        assert_eq!(
            format_limit_value(&(1024 * 1024).to_string(), "bytes"),
            "1 MiB"
        );
    }

    #[test]
    fn format_limit_value_passes_through_non_bytes() {
        // Non-byte units are kept verbatim — caller decides formatting.
        assert_eq!(format_limit_value("4096", "files"), "4096");
        assert_eq!(format_limit_value("1024", "us"), "1024");
    }
}
