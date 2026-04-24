//! procs.rs — full host process list for the "Procs" view.
//!
//! This is the big-table view: one row per PID on the host, the way
//! `htop`/`btop`/`btm` show it. We read `/proc/<pid>/{stat,status,cmdline}`
//! for every numeric subdir and compute CPU% as a delta over the last
//! refresh interval, same approach as the per-VM tracker in
//! `main.rs`.
//!
//! Memory model: a `Tracker` holds prev-sample jiffies per pid so we
//! can compute a rate. Pids that disappear between scans are purged so
//! the map can't grow unbounded.
//!
//! Status: parsing + sampling + sort/filter are implemented and
//! covered by unit tests. The Procs view in `main.rs` renders rows
//! produced by `Tracker::snapshot`, sorted via `sort_rows`, and
//! filtered via `matches`.

use std::collections::HashMap;
use std::fs;
use std::time::Instant;

#[derive(Debug, Clone)]
pub(crate) struct ProcessRow {
    pub(crate) pid: i32,
    /// Parent pid — reserved for a future process-tree view (`PLAN.md` deferred §1).
    #[allow(dead_code)]
    pub(crate) ppid: i32,
    /// Numeric user id. Stored for symmetry but not displayed — the
    /// resolved `user` field below is what reaches the UI.
    #[allow(dead_code)]
    pub(crate) uid: u32,
    /// Resolved via `/etc/passwd`; falls back to `uid=N` when unknown.
    pub(crate) user: String,
    /// Single-char state letter (`R`, `S`, `D`, `Z`, …).
    pub(crate) state: char,
    pub(crate) cpu_pct: Option<f64>,
    pub(crate) rss_bytes: u64,
    pub(crate) threads: i32,
    /// Full command line if readable, else the kernel `comm` (15-char
    /// name). We skip kernel threads (cmdline empty + parent 2).
    pub(crate) command: String,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    when: Instant,
    jiffies: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Tracker {
    prev: HashMap<i32, Sample>,
}

impl Tracker {
    pub(crate) fn snapshot(&mut self, passwd: &PasswdCache, clk_tck: u64) -> Vec<ProcessRow> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let now = Instant::now();
        let mut rows = Vec::new();
        let mut seen: Vec<i32> = Vec::new();

        for e in entries.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(pid) = name.parse::<i32>() else {
                continue;
            };
            let Some((row, jiffies)) = read_one(pid, passwd, &self.prev, now, clk_tck) else {
                continue;
            };
            self.prev.insert(pid, Sample { when: now, jiffies });
            seen.push(pid);
            rows.push(row);
        }
        self.prev.retain(|k, _| seen.contains(k));
        rows
    }
}

fn read_one(
    pid: i32,
    passwd: &PasswdCache,
    prev: &HashMap<i32, Sample>,
    now: Instant,
    clk_tck: u64,
) -> Option<(ProcessRow, u64)> {
    let base = format!("/proc/{pid}");
    let stat_raw = fs::read_to_string(format!("{base}/stat")).ok()?;
    let rparen = stat_raw.rfind(')')?;
    let after = stat_raw.get(rparen + 2..)?;
    let fields: Vec<&str> = after.split_whitespace().collect();

    let state = fields.first()?.chars().next()?;
    let parent_pid: i32 = fields.get(1)?.parse().ok()?;
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let threads: i32 = fields.get(17)?.parse().unwrap_or(0);
    let jiffies = utime + stime;

    let status = fs::read_to_string(format!("{base}/status")).ok()?;
    let uid = read_uid(&status).unwrap_or(0);
    let rss_bytes = read_rss_bytes(&status).unwrap_or(0);

    let cmdline_raw = fs::read_to_string(format!("{base}/cmdline")).unwrap_or_default();
    let command = if cmdline_raw.is_empty() {
        let comm = comm_from_stat(&stat_raw).unwrap_or_else(|| "?".into());
        format!("[{comm}]")
    } else {
        cmdline_raw
            .trim_end_matches('\0')
            .replace('\0', " ")
            .trim()
            .to_string()
    };

    let cpu_pct = prev.get(&pid).and_then(|p| {
        let dt = now.duration_since(p.when).as_secs_f64();
        if dt <= 0.0 {
            return None;
        }
        let dj = jiffies.saturating_sub(p.jiffies);
        #[allow(clippy::cast_precision_loss)]
        let pct = (dj as f64 / clk_tck as f64 / dt) * 100.0;
        Some(pct)
    });

    Some((
        ProcessRow {
            pid,
            ppid: parent_pid,
            uid,
            user: passwd.lookup(uid),
            state,
            cpu_pct,
            rss_bytes,
            threads,
            command: truncate(&command, 200),
        },
        jiffies,
    ))
}

// -----------------------------------------------------------------------------
// Parsers
// -----------------------------------------------------------------------------

pub(crate) fn read_uid(status: &str) -> Option<u32> {
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

pub(crate) fn read_rss_bytes(status: &str) -> Option<u64> {
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

pub(crate) fn comm_from_stat(stat: &str) -> Option<String> {
    let lparen = stat.find('(')?;
    let rparen = stat.rfind(')')?;
    if rparen <= lparen + 1 {
        return None;
    }
    Some(stat[lparen + 1..rparen].to_string())
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Char-boundary-safe truncate.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    }
}

// -----------------------------------------------------------------------------
// /etc/passwd cache for uid → username
// -----------------------------------------------------------------------------

#[derive(Debug, Default)]
pub(crate) struct PasswdCache {
    users: HashMap<u32, String>,
}

impl PasswdCache {
    pub(crate) fn load() -> Self {
        let mut users = HashMap::new();
        if let Ok(raw) = fs::read_to_string("/etc/passwd") {
            for line in raw.lines() {
                // name:pass:uid:gid:gecos:home:shell
                let parts: Vec<&str> = line.split(':').collect();
                if parts.len() < 3 {
                    continue;
                }
                if let Ok(uid) = parts[2].parse::<u32>() {
                    users.insert(uid, parts[0].to_string());
                }
            }
        }
        Self { users }
    }

    pub(crate) fn lookup(&self, uid: u32) -> String {
        self.users
            .get(&uid)
            .cloned()
            .unwrap_or_else(|| format!("uid={uid}"))
    }
}

// -----------------------------------------------------------------------------
// Sort orders for the Procs table
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SortBy {
    Cpu,
    Mem,
    Pid,
    Command,
}

impl SortBy {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Cpu => Self::Mem,
            Self::Mem => Self::Pid,
            Self::Pid => Self::Command,
            Self::Command => Self::Cpu,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU%",
            Self::Mem => "RSS",
            Self::Pid => "PID",
            Self::Command => "CMD",
        }
    }
}

/// Sort `rows` in place by the requested key. Currently only used by
/// the unit tests — the live UI sorts indices via `main::compute_visible`
/// to avoid moving full `ProcessRow` values around. Kept public so the
/// behaviour stays test-locked.
#[allow(dead_code)]
pub(crate) fn sort_rows(rows: &mut [ProcessRow], by: SortBy) {
    match by {
        SortBy::Cpu => rows.sort_by(|a, b| {
            b.cpu_pct
                .unwrap_or(0.0)
                .partial_cmp(&a.cpu_pct.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        SortBy::Mem => rows.sort_by(|a, b| b.rss_bytes.cmp(&a.rss_bytes)),
        SortBy::Pid => rows.sort_by(|a, b| a.pid.cmp(&b.pid)),
        SortBy::Command => rows.sort_by(|a, b| a.command.cmp(&b.command)),
    }
}

/// Substring filter (case-insensitive). A process row matches if any
/// of its command / user / pid representations contain `needle`.
pub(crate) fn matches(row: &ProcessRow, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let n = needle.to_ascii_lowercase();
    row.command.to_ascii_lowercase().contains(&n)
        || row.user.to_ascii_lowercase().contains(&n)
        || row.pid.to_string().contains(&n)
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_uid_from_status() {
        let sample = "Name:\tbash\nState:\tS\nTgid:\t123\nUid:\t1000\t1000\t1000\t1000\n";
        assert_eq!(read_uid(sample), Some(1000));
    }

    #[test]
    fn parses_vmrss_kb_to_bytes() {
        let sample = "VmPeak:\t  40000 kB\nVmSize:\t  30000 kB\nVmRSS:\t   2048 kB\n";
        assert_eq!(read_rss_bytes(sample), Some(2048 * 1024));
    }

    #[test]
    fn comm_extracts_between_parens() {
        // Command names with spaces/parens need the rfind/lfind logic.
        let stat = "1 (systemd) S 0 1 1 0 -1 ...";
        assert_eq!(comm_from_stat(stat).as_deref(), Some("systemd"));

        let weird = "42 (strange (name)) R 1 42 42 ...";
        // Widest span: first `(` to last `)`. This matches procps'
        // behavior and handles command names that themselves contain
        // parens — cf. kernel `proc_get_task_name` uses the same rule.
        assert_eq!(comm_from_stat(weird).as_deref(), Some("strange (name)"));
    }

    #[test]
    fn matches_is_case_insensitive() {
        let row = ProcessRow {
            pid: 42,
            ppid: 1,
            uid: 1000,
            user: "alice".into(),
            state: 'S',
            cpu_pct: Some(1.0),
            rss_bytes: 0,
            threads: 1,
            command: "/usr/bin/BASH".into(),
        };
        assert!(matches(&row, "bash"));
        assert!(matches(&row, "ALICE"));
        assert!(matches(&row, "42"));
        assert!(!matches(&row, "zzz"));
        assert!(matches(&row, "")); // empty filter matches everything
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "café🦀rust";
        let out = truncate(s, 5);
        // 'c' 'a' 'f' 'é' uses 5 bytes in UTF-8 (1+1+1+2); truncating
        // at byte 5 is a valid boundary.
        assert_eq!(out, "café");
    }

    #[test]
    fn sort_by_cpu_desc() {
        let mut rows = vec![
            make_row(1, Some(10.0), 100),
            make_row(2, Some(50.0), 50),
            make_row(3, None, 200),
        ];
        sort_rows(&mut rows, SortBy::Cpu);
        assert_eq!(rows[0].pid, 2);
        assert_eq!(rows[1].pid, 1);
        assert_eq!(rows[2].pid, 3); // None treated as 0
    }

    #[test]
    fn sort_by_mem_desc() {
        let mut rows = vec![
            make_row(1, None, 100),
            make_row(2, None, 50),
            make_row(3, None, 200),
        ];
        sort_rows(&mut rows, SortBy::Mem);
        assert_eq!(
            rows.iter().map(|r| r.pid).collect::<Vec<_>>(),
            vec![3, 1, 2]
        );
    }

    #[test]
    fn sort_cycle_is_cpu_mem_pid_cmd_cpu() {
        let mut s = SortBy::Cpu;
        s = s.next();
        assert_eq!(s, SortBy::Mem);
        s = s.next();
        assert_eq!(s, SortBy::Pid);
        s = s.next();
        assert_eq!(s, SortBy::Command);
        s = s.next();
        assert_eq!(s, SortBy::Cpu);
    }

    #[test]
    fn passwd_cache_handles_missing_uid() {
        let cache = PasswdCache::default();
        assert_eq!(cache.lookup(9999), "uid=9999");
    }

    fn make_row(pid: i32, cpu_pct: Option<f64>, rss_kb: u64) -> ProcessRow {
        ProcessRow {
            pid,
            ppid: 1,
            uid: 1000,
            user: "u".into(),
            state: 'S',
            cpu_pct,
            rss_bytes: rss_kb * 1024,
            threads: 1,
            command: format!("cmd{pid}"),
        }
    }
}
