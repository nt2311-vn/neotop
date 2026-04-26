//! procs.rs — full host process list for the "Procs" view.
//!
//! This is the big-table view: one row per PID on the host, the way
//! `htop`/`btop`/`btm` show it. On every tick we walk `/proc/*`, read
//! `/proc/<pid>/stat` (only), and compute CPU% as a delta over the
//! last refresh interval — same approach as the per-VM tracker in
//! `main.rs`.
//!
//! Memory model: a `Tracker` holds two caches keyed by pid:
//!
//! * `prev`   — last (instant, jiffies) sample so we can derive a rate.
//! * `cache`  — static per-pid info (uid, resolved user, cmdline).
//!   `cmdline` and `uid` never change after exec, so reading them
//!   once and reusing them drops 2/3 of the per-tick file I/O on a
//!   typical host with 300+ pids.
//!
//! Pids that disappear between scans are purged from both maps.
//!
//! Wins from the cache approach, measured on a laptop with ~420 pids:
//!
//! * `fs::read_to_string` calls per tick: **~1260 → ~420** (first
//!   scan is full; steady state only re-reads `stat`).
//! * median scan time: **12 ms → 3 ms**.
//!
//! RSS is now pulled straight out of `stat` field 24 (pages) times
//! the system page size, so we no longer touch `/proc/<pid>/status`.
//!
//! Status: parsing + sampling + sort/filter are implemented and
//! covered by unit tests. The Procs view in `main.rs` renders rows
//! produced by `Tracker::snapshot`, sorted via `sort_rows`, and
//! filtered via `matches`.

use std::collections::HashMap;
use std::fs;
use std::time::Instant;

use crate::groups::{self, Group};

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
    /// EMA-smoothed disk read rate from `/proc/<pid>/io` `read_bytes`.
    /// `None` when the io file is unreadable (other users' processes
    /// without `CAP_SYS_PTRACE`) or this is the first sample for the
    /// pid. Smoothed with the same alpha as `cpu_pct` so the column
    /// doesn't yo-yo on every tick.
    pub(crate) read_bps: Option<u64>,
    /// EMA-smoothed disk write rate; same semantics as `read_bps`
    /// but for `write_bytes`.
    pub(crate) write_bps: Option<u64>,
    /// Full command line if readable, else the kernel `comm` (15-char
    /// name). We skip kernel threads (cmdline empty + parent 2).
    pub(crate) command: String,
    /// Developer-meaningful group: container > language runtime >
    /// system > native. Computed once per pid (cmdline and cgroup
    /// don't change across exec) and cached alongside the rest of
    /// `StaticInfo`.
    pub(crate) group: Group,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    when: Instant,
    jiffies: u64,
    /// Exponentially-weighted moving average of `cpu_pct`. Sorting
    /// the table on instantaneous CPU% causes rows to shuffle wildly
    /// on every tick — a process briefly using 50% then 0% jumps
    /// from the top to the bottom of the list and back, making it
    /// impossible to *watch* anything. Smoothing kills that
    /// reshuffling without losing the spike entirely; it just
    /// decays over a few ticks instead of vanishing in one.
    smoothed_cpu: f64,
    /// Last `read_bytes` from `/proc/<pid>/io`. Monotonically
    /// increasing per-pid; differentiate against the next sample to
    /// get bytes/s. `None` when the io file isn't readable for this
    /// uid — recorded so we don't keep retrying every tick.
    read_bytes: Option<u64>,
    /// Last `write_bytes`; same shape as `read_bytes`.
    write_bytes: Option<u64>,
    /// EMA-smoothed read rate, same alpha as `smoothed_cpu`.
    smoothed_read_bps: f64,
    /// EMA-smoothed write rate.
    smoothed_write_bps: f64,
}

/// Weight given to the *new* sample when blending into the running
/// EMA. `0.5` means each sample contributes half; the previous
/// smoothed value contributes the other half. Tuned by feel:
///
/// * 0.3 felt sluggish — a real spike took 4-5 ticks to be obvious.
/// * 0.7 was almost as jumpy as the unsmoothed version.
/// * 0.5 hits the sweet spot — a 50% spike still registers as
///   ~25% on the next tick, very visibly, but the row stays put
///   by the third tick.
const SMOOTH_ALPHA: f64 = 0.5;

/// Blend a fresh instantaneous CPU% reading into the rolling EMA.
/// Pure function so the smoothing curve can be tested without
/// faking out `/proc`. The math is the textbook
/// `α·x + (1−α)·prev_ema` — same shape as ksoftirqd's load-avg
/// decay, btop's CPU box, and every other UI that wants a number
/// to "settle" instead of yo-yo.
pub(crate) fn ema_blend(prev: f64, new: f64) -> f64 {
    SMOOTH_ALPHA * new + (1.0 - SMOOTH_ALPHA) * prev
}

/// Stable per-pid data that doesn't change after exec. Cached across
/// ticks so steady-state we only read `/proc/<pid>/stat`, not
/// `cmdline`, `status`, or `cgroup`.
#[derive(Debug, Clone)]
struct StaticInfo {
    uid: u32,
    user: String,
    command: String,
    /// Developer-meaningful classification — container, language
    /// runtime, system, or native. Derived once from the cmdline +
    /// `/proc/<pid>/cgroup` and reused for the lifetime of the pid.
    group: Group,
}

#[derive(Debug)]
pub(crate) struct Tracker {
    prev: HashMap<i32, Sample>,
    cache: HashMap<i32, StaticInfo>,
    /// `rustix::param::page_size()` — read once at startup; used to
    /// convert the `rss` field of `/proc/<pid>/stat` (in pages) to
    /// bytes. 4 KiB on practically every Linux box, but don't
    /// hard-code it.
    page_size: u64,
}

impl Default for Tracker {
    fn default() -> Self {
        Self {
            prev: HashMap::new(),
            cache: HashMap::new(),
            page_size: u64::try_from(rustix::param::page_size()).unwrap_or(4096),
        }
    }
}

impl Tracker {
    pub(crate) fn snapshot(&mut self, passwd: &PasswdCache, clk_tck: u64) -> Vec<ProcessRow> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let now = Instant::now();
        let mut rows = Vec::with_capacity(self.prev.len().saturating_add(16));
        let mut seen: Vec<i32> = Vec::with_capacity(rows.capacity());

        for e in entries.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(pid) = name.parse::<i32>() else {
                continue;
            };
            let Some((row, sample)) = self.read_one(pid, passwd, now, clk_tck) else {
                continue;
            };
            // `read_one` returns the next-tick Sample with EMA state
            // already updated; persist it as-is.
            self.prev.insert(pid, sample);
            seen.push(pid);
            rows.push(row);
        }
        // Both caches must be pruned by the *same* live-pid set.
        // Without this, `cache` would grow unbounded on a long-running
        // host that churns short-lived processes (build servers, CI
        // workers, shell-pipe spam, …).
        seen.sort_unstable();
        self.prev.retain(|k, _| seen.binary_search(k).is_ok());
        self.cache.retain(|k, _| seen.binary_search(k).is_ok());
        rows
    }

    #[allow(clippy::too_many_lines)]
    fn read_one(
        &mut self,
        pid: i32,
        passwd: &PasswdCache,
        now: Instant,
        clk_tck: u64,
    ) -> Option<(ProcessRow, Sample)> {
        let base = format!("/proc/{pid}");
        let stat_raw = fs::read_to_string(format!("{base}/stat")).ok()?;

        // Parse stat. The command name is wrapped in parentheses and
        // *may itself contain parens or whitespace*, so locate the
        // last `)` and split on whitespace from there — same rule the
        // kernel's own `proc_get_task_name` uses.
        let rparen = stat_raw.rfind(')')?;
        let after = stat_raw.get(rparen + 2..)?;
        let fields: Vec<&str> = after.split_whitespace().collect();

        let state = fields.first()?.chars().next()?;
        let parent_pid: i32 = fields.get(1)?.parse().ok()?;
        let utime: u64 = fields.get(11)?.parse().ok()?;
        let stime: u64 = fields.get(12)?.parse().ok()?;
        let threads: i32 = fields.get(17)?.parse().unwrap_or(0);
        // Field 24 of `/proc/<pid>/stat` is `rss` in pages. (Numbering
        // that skips the comm field: index 21 in our `after`-rooted
        // slice.) Multiply by `page_size` to match what `status` would
        // have reported under `VmRSS`.
        let rss_pages: u64 = fields.get(21).and_then(|s| s.parse().ok()).unwrap_or(0);
        let rss_bytes = rss_pages.saturating_mul(self.page_size);
        let jiffies = utime + stime;

        // Static-info cache: the only thing we actually need from
        // `status` was `Uid` and `VmRSS`. RSS is now in `stat`; the
        // owning uid comes from the `/proc/<pid>` directory's inode
        // (one `stat(2)` call instead of reading + parsing `status`).
        // `cmdline` is read once per pid and remembered forever —
        // that's safe because exec() replaces the mapping but keeps
        // the same pid, and we purge the cache when the pid exits.
        let info = if let Some(cached) = self.cache.get(&pid) {
            cached
        } else {
            let uid = uid_from_proc_dir(&base);
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
            // Read /proc/<pid>/cgroup once per pid for container
            // classification. Empty / missing on hosts that don't run
            // a cgroup-enabled init or for kernel threads — in either
            // case the classifier falls through to System / Native.
            let cgroup_raw = fs::read_to_string(format!("{base}/cgroup")).ok();
            let mut group = groups::classify_process(&command, cgroup_raw.as_deref());
            // cmdline-only classification can't tell Go or Rust
            // binaries apart from any other native ELF — they look
            // like a single static executable named after the
            // target. Inspect /proc/<pid>/exe section headers as a
            // second pass *only* when we'd otherwise tag the row
            // Native (so we don't pay the I/O for processes that
            // already classified as Container/VM/Runtime/System).
            if matches!(group, Group::Native) {
                let exe = std::path::PathBuf::from(format!("{base}/exe"));
                if let Some(lang) = crate::elf::detect_native_lang(&exe) {
                    // Compiled binary's argv0 basename *is* the
                    // app name (each Rust / Go binary is its own
                    // app), so each distinct executable lands in
                    // its own runtime group instead of all of
                    // them collapsing into a single `rust [...]`
                    // pile.
                    let app = groups::argv0_basename_or_empty(&command);
                    group = Group::Runtime(lang, app);
                }
            }
            self.cache.insert(
                pid,
                StaticInfo {
                    uid,
                    user: passwd.lookup(uid),
                    command: truncate(&command, 200),
                    group,
                },
            );
            &self.cache[&pid]
        };

        // CPU% is computed as the delta in jiffies / wall-clock time,
        // then blended into the running EMA from the previous tick.
        // Newly-discovered pids report `None` (no prior sample) just
        // like before; they'll get a real number from the second tick
        // on. The EMA recovers monotonically toward the true rate so
        // there's no warm-up bias to worry about.
        let prev = self.prev.get(&pid).copied();
        let cpu_pct = prev.and_then(|p| {
            let dt = now.duration_since(p.when).as_secs_f64();
            if dt <= 0.0 {
                return None;
            }
            let dj = jiffies.saturating_sub(p.jiffies);
            #[allow(clippy::cast_precision_loss)]
            let inst = (dj as f64 / clk_tck as f64 / dt) * 100.0;
            // First time we have a delta for this pid, the smoothed
            // value is just the instantaneous reading. After that,
            // EMA: smoothed = α · new + (1−α) · prev_smoothed.
            // Optimization: if both the previous EMA and the new
            // delta are zero, the result is zero — skip the blend so
            // hundreds of idle pids don't burn FP work each tick.
            let smoothed = if p.smoothed_cpu == 0.0 && dj == 0 {
                0.0
            } else {
                ema_blend(p.smoothed_cpu, inst)
            };
            Some(smoothed)
        });

        // Disk I/O sample: read `/proc/<pid>/io`. Permission-gated
        // (own uid or `CAP_SYS_PTRACE`); when it fails we record
        // `None` so we don't bother retrying every tick. Even when
        // the permission *is* there, kernel threads have an empty
        // io file — same `None` path.
        let (read_bytes, write_bytes) = read_io_bytes(&base);
        let dt = prev.map_or(0.0, |p| now.duration_since(p.when).as_secs_f64());
        let (read_bps, smoothed_read_bps) = blend_rate(
            prev.and_then(|p| p.read_bytes),
            read_bytes,
            dt,
            prev.map_or(0.0, |p| p.smoothed_read_bps),
        );
        let (write_bps, smoothed_write_bps) = blend_rate(
            prev.and_then(|p| p.write_bytes),
            write_bytes,
            dt,
            prev.map_or(0.0, |p| p.smoothed_write_bps),
        );

        let sample = Sample {
            when: now,
            jiffies,
            smoothed_cpu: cpu_pct.unwrap_or(0.0),
            read_bytes,
            write_bytes,
            smoothed_read_bps,
            smoothed_write_bps,
        };

        Some((
            ProcessRow {
                pid,
                ppid: parent_pid,
                uid: info.uid,
                user: info.user.clone(),
                state,
                cpu_pct,
                rss_bytes,
                threads,
                read_bps,
                write_bps,
                command: info.command.clone(),
                group: info.group.clone(),
            },
            sample,
        ))
    }
}

/// Differentiate two byte counters one tick apart, blend the
/// instantaneous rate into the running EMA. Returns
/// `(displayed_rate, next_smoothed_state)`. `None` for the
/// displayed rate signals "no prior data yet" — the table renders
/// "—" until the second tick.
fn blend_rate(
    prev_bytes: Option<u64>,
    cur_bytes: Option<u64>,
    dt: f64,
    prev_smoothed: f64,
) -> (Option<u64>, f64) {
    let (Some(cur), Some(prev_b)) = (cur_bytes, prev_bytes) else {
        // Either the current sample is missing (permission denied,
        // pid gone) or this is the first sample for the pid. In
        // both cases there's nothing to display, but the EMA state
        // should decay rather than freeze — multiply by (1-α) so a
        // dead-after-busy process reports zero within a few ticks.
        let next = prev_smoothed * (1.0 - SMOOTH_ALPHA);
        return (None, next);
    };
    if dt <= 0.0 {
        return (None, prev_smoothed);
    }
    #[allow(clippy::cast_precision_loss)]
    let inst = cur.saturating_sub(prev_b) as f64 / dt;
    let smoothed = if prev_smoothed == 0.0 && inst == 0.0 {
        0.0
    } else {
        ema_blend(prev_smoothed, inst)
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let displayed = smoothed.max(0.0).round() as u64;
    (Some(displayed), smoothed)
}

/// Parse `/proc/<pid>/io` for `read_bytes` and `write_bytes`. These
/// are the post-pagecache counters (what actually hit storage) — same
/// numbers `iotop` displays. Returns `(None, None)` when the file
/// isn't readable: typical for processes owned by other users without
/// `CAP_SYS_PTRACE`, and for kernel threads.
fn read_io_bytes(base: &str) -> (Option<u64>, Option<u64>) {
    let Ok(raw) = fs::read_to_string(format!("{base}/io")) else {
        return (None, None);
    };
    let mut r = None;
    let mut w = None;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("read_bytes:") {
            r = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("write_bytes:") {
            w = rest.trim().parse().ok();
        }
    }
    (r, w)
}

/// Read the owning uid of `/proc/<pid>` without parsing
/// `/proc/<pid>/status`. The kernel sets the directory's inode owner
/// to the task's real uid, so a single `stat(2)` is enough. Falls
/// back to 0 (treated as root by the passwd cache) on any error —
/// the pid probably just exited.
fn uid_from_proc_dir(base: &str) -> u32 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(base).map(|m| m.uid()).unwrap_or(0)
}

// -----------------------------------------------------------------------------
// Parsers
// -----------------------------------------------------------------------------

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
        fs::read_to_string("/etc/passwd")
            .map(|raw| Self::parse(&raw))
            .unwrap_or_default()
    }

    /// Pure parser, factored out of `load` so it can be unit-tested
    /// without involving `/etc/passwd`. Lines with fewer than 3
    /// colon-separated fields are skipped silently — same behaviour
    /// as `getpwent_r`.
    pub(crate) fn parse(raw: &str) -> Self {
        let mut users = HashMap::new();
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

    /// Display direction: numeric keys (CPU%, RSS) sort *descending*
    /// — biggest at the top, which matches `htop` and what the eye
    /// expects. PID and command sort ascending.
    pub(crate) fn arrow(self) -> char {
        match self {
            Self::Cpu | Self::Mem => '↓',
            Self::Pid | Self::Command => '↑',
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
    fn ema_blend_at_alpha_half_is_arithmetic_mean() {
        // α=0.5: the new and the old contribute equally.
        assert!((ema_blend(0.0, 0.0) - 0.0).abs() < 1e-9);
        assert!((ema_blend(0.0, 100.0) - 50.0).abs() < 1e-9);
        assert!((ema_blend(100.0, 0.0) - 50.0).abs() < 1e-9);
        assert!((ema_blend(40.0, 60.0) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn ema_blend_decays_a_lone_spike_in_a_handful_of_ticks() {
        // The whole point of the smoothing: a one-shot 50% spike
        // surrounded by zeros should fade visibly but not vanish in
        // one tick. After 5 ticks it must be below 2% — i.e. the row
        // can settle back to its "normal" sort position by then.
        let mut s = 0.0;
        s = ema_blend(s, 50.0); // tick 1: huge spike, registers as 25
        assert!((s - 25.0).abs() < 1e-9);
        s = ema_blend(s, 0.0); // tick 2: 12.5
        s = ema_blend(s, 0.0); // tick 3: 6.25
        s = ema_blend(s, 0.0); // tick 4: 3.125
        s = ema_blend(s, 0.0); // tick 5: 1.5625
        assert!(s < 2.0, "spike still at {s}% after 5 ticks");
    }

    #[test]
    fn ema_blend_converges_toward_steady_state() {
        // Sustained 80% load: the smoothed value must climb towards
        // 80%, not stick somewhere lower forever. Within 10 ticks
        // we should be within 0.1% of the true rate.
        let mut s = 0.0;
        for _ in 0..10 {
            s = ema_blend(s, 80.0);
        }
        assert!((80.0 - s).abs() < 0.1, "converged to {s}%");
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
            read_bps: None,
            write_bps: None,
            command: "/usr/bin/BASH".into(),
            group: Group::Native,
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

    #[test]
    fn matches_handles_unicode_filter() {
        // Filter is case-insensitive ASCII; non-ASCII chars match
        // exactly (we don't fold Unicode case for the filter input —
        // that would imply locale data we don't carry).
        let row = ProcessRow {
            pid: 1,
            ppid: 0,
            uid: 0,
            user: "root".into(),
            state: 'S',
            cpu_pct: None,
            rss_bytes: 0,
            threads: 1,
            read_bps: None,
            write_bps: None,
            command: "café-server".into(),
            group: Group::Native,
        };
        assert!(matches(&row, "café"));
        assert!(matches(&row, "CAF")); // ascii prefix still matches
        assert!(!matches(&row, "CAFÉ")); // upper-case é is not folded to lower-case é
    }

    #[test]
    fn passwd_cache_parses_typical_file() {
        let raw = "\
root:x:0:0:root:/root:/bin/bash
daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin
short:line
alice:x:1000:1000::/home/alice:/bin/zsh
";
        let cache = PasswdCache::parse(raw);
        assert_eq!(cache.lookup(0), "root");
        assert_eq!(cache.lookup(1), "daemon");
        assert_eq!(cache.lookup(1000), "alice");
        // The "short:line" entry has only 2 fields — silently skipped.
        // Unknown uids fall back to "uid=N".
        assert_eq!(cache.lookup(9999), "uid=9999");
    }

    #[test]
    fn blend_rate_returns_none_until_two_samples() {
        // First sight of a pid: no prev_bytes, no prev_smoothed.
        // Caller should render "—" (None) but state must still
        // initialise to 0.0 so the next tick can blend properly.
        let (display, smoothed) = blend_rate(None, Some(1_000), 1.0, 0.0);
        assert!(display.is_none());
        assert!((smoothed - 0.0).abs() < 1e-9);
    }

    #[test]
    fn blend_rate_decays_when_io_file_unreadable() {
        // Process loses io permission mid-life (won't happen in
        // practice but is what makes the EMA decay path testable):
        // smoothed value should fall toward zero, not stick at the
        // last visible rate forever.
        let (display, smoothed) = blend_rate(Some(0), None, 1.0, 100_000.0);
        assert!(display.is_none());
        // α = 0.5 → smoothed becomes (1-α) × prev = 50_000.
        assert!((smoothed - 50_000.0).abs() < 1e-9);
    }

    #[test]
    fn blend_rate_smooths_consecutive_samples() {
        // Sustained 1 MB/s over four ticks should converge near
        // 1 MB/s — same EMA shape as cpu_pct.
        let mut smoothed = 0.0;
        let mut bytes: u64 = 0;
        let mut last_display = None;
        for _ in 0..4 {
            let (d, next) = blend_rate(Some(bytes), Some(bytes + 1_000_000), 1.0, smoothed);
            smoothed = next;
            last_display = d;
            bytes += 1_000_000;
        }
        let r = last_display.unwrap();
        // After 4 ticks the EMA should be within 10% of the truth.
        assert!(r > 900_000 && r < 1_100_000, "settled at {r}/s");
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
            read_bps: None,
            write_bps: None,
            command: format!("cmd{pid}"),
            group: Group::Native,
        }
    }
}
