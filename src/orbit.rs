//! orbit.rs — process orbit chart data + glyph layout.
//!
//! Renders the top-N processes as dots on an ellipse:
//!   * Angular slot is a stable hash of the PID — same PID lands at
//!     the same clock position every tick, so a busy daemon is easy
//!     to track between frames.
//!   * Radius scales with CPU% — quiet processes huddle near the
//!     centre, hot ones touch the rim.
//!   * Glyph density picks `·` / `•` / `●` from CPU%; a `BOLD`
//!     style flag pulses for one tick when the PID is new.
//!
//! Pure helpers + `compute_glyphs`; the renderer in `main.rs` walks
//! the returned `Vec<Cell>` and paints each instruction.

use std::collections::HashSet;

use crate::procs::ProcessRow;

/// One process represented on the orbit ring.
#[derive(Debug, Clone)]
pub(crate) struct OrbitProc {
    pub(crate) pid: i32,
    /// Truncated command name (≤ 8 chars) for the legend.
    pub(crate) name: String,
    /// 0..=100. Drives radius and glyph density.
    pub(crate) cpu_pct: f64,
    /// Process state char as `/proc/<pid>/stat` reports it
    /// (`R` running, `S` sleeping, `D` disk-wait, `T` stopped,
    /// `Z` zombie). Used by the renderer for colour.
    pub(crate) state: char,
}

/// One tick's worth of orbit state. Built fresh each slow tick
/// from the top-N processes by CPU%; `new_pids` contains the PIDs
/// that weren't present last tick (drives the bold pulse).
#[derive(Debug, Default, Clone)]
pub(crate) struct OrbitFrame {
    pub(crate) processes: Vec<OrbitProc>,
    pub(crate) new_pids: HashSet<i32>,
}

/// How many processes to project onto the ring. More than this
/// crowds the chart at any reasonable terminal size.
pub(crate) const ORBIT_TOP_N: usize = 12;

/// One placement instruction emitted by `compute_glyphs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cell {
    pub(crate) row: u16,
    pub(crate) col: u16,
    pub(crate) ch: char,
    /// Index into the input `OrbitFrame::processes` Vec, so the
    /// renderer can look up the colour / pulse flag without
    /// duplicating that logic here.
    pub(crate) proc_idx: usize,
}

impl OrbitFrame {
    /// Build the orbit frame from a freshly snapshotted process
    /// table. Picks the top `ORBIT_TOP_N` rows by CPU% (excluding
    /// kernel threads which have no command line — those would
    /// crowd the centre with idle workers).
    ///
    /// `prev_pids` is the PID set from the previous tick; PIDs in
    /// the new top-N but not in `prev_pids` go into `new_pids`.
    pub(crate) fn build(rows: &[ProcessRow], prev_pids: &HashSet<i32>) -> Self {
        let mut ranked: Vec<(&ProcessRow, f64)> = rows
            .iter()
            .filter(|r| !r.command.is_empty())
            .map(|r| (r, r.cpu_pct.unwrap_or(0.0)))
            .collect();
        // Stable sort on cpu desc; ties broken by pid asc so the
        // chart doesn't jitter across ticks when multiple procs
        // sit at 0%.
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.pid.cmp(&b.0.pid))
        });
        ranked.truncate(ORBIT_TOP_N);

        let processes: Vec<OrbitProc> = ranked
            .into_iter()
            .map(|(r, cpu)| OrbitProc {
                pid: r.pid,
                name: display_name(&r.command),
                cpu_pct: cpu.clamp(0.0, 100.0),
                state: r.state,
            })
            .collect();

        let new_pids: HashSet<i32> = processes
            .iter()
            .map(|p| p.pid)
            .filter(|pid| !prev_pids.contains(pid))
            .collect();

        Self {
            processes,
            new_pids,
        }
    }

    /// Snapshot of the PIDs in the current frame, suitable for use
    /// as `prev_pids` on the next tick.
    pub(crate) fn pid_set(&self) -> HashSet<i32> {
        self.processes.iter().map(|p| p.pid).collect()
    }
}

/// Stable per-PID angle in radians, 0..2π. Uses a small mixing
/// hash so consecutive PIDs (children of the same fork-bomb)
/// don't pile up next to each other on the ring.
pub(crate) fn angle_for_pid(pid: i32) -> f64 {
    // Splitmix64 finalizer on a u64 derived from the PID.
    #[allow(clippy::cast_sign_loss)]
    let mut x = i64::from(pid) as u64;
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    // Map the top 53 bits to [0, 1) for f64 precision.
    #[allow(clippy::cast_precision_loss)]
    let unit = (x >> 11) as f64 / (1u64 << 53) as f64;
    unit * std::f64::consts::TAU
}

/// 0..=1 normalised radius. Quiet processes near the centre, hot
/// ones at the rim — but never *at* the centre (that's where the
/// ring's label lives). 0.35 baseline with 0.65 of dynamic range.
pub(crate) fn radius_norm(cpu_pct: f64) -> f64 {
    let c = cpu_pct.clamp(0.0, 100.0) / 100.0;
    0.35 + 0.65 * c
}

/// Glyph density ramp. Picked to read at a glance: low CPU is a
/// pin-prick, mid is a bullet, high is a filled disc.
pub(crate) fn glyph_for_cpu(cpu_pct: f64) -> char {
    if cpu_pct < 5.0 {
        '·'
    } else if cpu_pct < 30.0 {
        '•'
    } else {
        '●'
    }
}

/// Place every process in the frame as one `Cell`. The chart fills
/// the supplied area (`rows × cols`); the renderer is responsible
/// for any surrounding block / border.
///
/// Aspect compensation: monospace cells are roughly twice as tall
/// as they are wide, so the ellipse uses a wider horizontal
/// half-axis to render visually circular.
pub(crate) fn compute_glyphs(rows: u16, cols: u16, frame: &OrbitFrame) -> Vec<Cell> {
    if rows < 3 || cols < 5 || frame.processes.is_empty() {
        return Vec::new();
    }
    let cx = (f64::from(cols) - 1.0) / 2.0;
    let cy = (f64::from(rows) - 1.0) / 2.0;
    // Half-axes: leave 1 cell of margin so the rim doesn't touch
    // the border. Wider on x to compensate for cell aspect.
    let half_w = (cx - 1.0).max(1.0);
    let half_h = (cy - 0.5).max(1.0);

    let mut out = Vec::with_capacity(frame.processes.len());
    for (idx, p) in frame.processes.iter().enumerate() {
        let theta = angle_for_pid(p.pid);
        let r = radius_norm(p.cpu_pct);
        let x = cx + r * half_w * theta.cos();
        let y = cy + r * half_h * theta.sin();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let col = x.round().clamp(0.0, f64::from(cols - 1)) as u16;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let row = y.round().clamp(0.0, f64::from(rows - 1)) as u16;
        out.push(Cell {
            row,
            col,
            ch: glyph_for_cpu(p.cpu_pct),
            proc_idx: idx,
        });
    }
    out
}

/// Extract a short, readable display name from a process command
/// line. The legend has limited horizontal space, so showing the
/// full path (`/usr/lib/firefox/firefox --new-window …`) collapses
/// dozens of distinct processes into the same prefix once the
/// string is truncated. We instead want what the user would type
/// to refer to the process: `firefox`, `chrome`, `bash`.
///
/// Algorithm:
///
/// 1. Drop everything after the first whitespace (the args).
/// 2. Take the basename of the remaining path.
/// 3. If the result is empty (e.g. command was pure whitespace,
///    or kernel thread `[kworker/0:1]` started with `[`), keep
///    the original first token so we don't lose the kernel-thread
///    bracket convention.
/// 4. Truncate to `max` chars on a UTF-8 boundary.
pub(crate) fn display_name(command: &str) -> String {
    display_name_with_max(command, 16)
}

fn display_name_with_max(command: &str, max: usize) -> String {
    let first_token = command.split_whitespace().next().unwrap_or("");
    if first_token.is_empty() {
        return String::new();
    }
    // Kernel threads come in as `[kworker/0:1]` — keep the
    // brackets so they're recognisable at a glance.
    let candidate = if first_token.starts_with('[') {
        first_token
    } else {
        // Basename: text after the final `/`.
        first_token.rsplit('/').next().unwrap_or(first_token)
    };
    let stem = if candidate.is_empty() {
        first_token
    } else {
        candidate
    };
    truncate_chars(stem, max)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_proc(pid: i32, cpu: f64, state: char, cmd: &str) -> ProcessRow {
        ProcessRow {
            pid,
            ppid: 0,
            uid: 0,
            user: "u".into(),
            state,
            cpu_pct: Some(cpu),
            rss_bytes: 0,
            threads: 1,
            read_bps: None,
            write_bps: None,
            command: cmd.into(),
            group: crate::groups::Group::Native,
        }
    }

    #[test]
    fn pid_hashes_to_stable_angle() {
        // Same PID → same angle on every call.
        let a = angle_for_pid(1234);
        let b = angle_for_pid(1234);
        assert!((a - b).abs() < f64::EPSILON);
        // Result is in [0, 2π).
        assert!((0.0..std::f64::consts::TAU).contains(&a));
        // Sequential PIDs should *not* land near each other —
        // splitmix scatters them across the circle.
        let close: Vec<f64> = (1000..1010).map(angle_for_pid).collect();
        let mut spread = 0.0_f64;
        for w in close.windows(2) {
            spread += (w[1] - w[0]).abs();
        }
        assert!(spread > 1.0, "consecutive PIDs clumped: {close:?}");
    }

    #[test]
    fn radius_scales_monotonically_with_cpu_pct() {
        assert!(radius_norm(0.0) < radius_norm(50.0));
        assert!(radius_norm(50.0) < radius_norm(100.0));
        // Idle processes still sit *off* the centre — the centre
        // is reserved for the ring's label.
        assert!(radius_norm(0.0) > 0.3);
        // Saturated processes never escape the unit ellipse.
        assert!(radius_norm(100.0) <= 1.0);
    }

    #[test]
    fn glyph_picks_density_by_cpu() {
        assert_eq!(glyph_for_cpu(0.0), '·');
        assert_eq!(glyph_for_cpu(1.0), '·');
        assert_eq!(glyph_for_cpu(15.0), '•');
        assert_eq!(glyph_for_cpu(80.0), '●');
        // Boundaries match the docstring exactly.
        assert_eq!(glyph_for_cpu(5.0), '•');
        assert_eq!(glyph_for_cpu(30.0), '●');
    }

    #[test]
    fn compute_glyphs_stays_within_bounds() {
        let frame = OrbitFrame {
            processes: (0..ORBIT_TOP_N)
                .map(|i| OrbitProc {
                    pid: i32::try_from(i).unwrap_or(0),
                    name: format!("p{i}"),
                    #[allow(clippy::cast_precision_loss)]
                    cpu_pct: (i * 8) as f64,
                    state: 'R',
                })
                .collect(),
            new_pids: HashSet::new(),
        };
        let cells = compute_glyphs(12, 30, &frame);
        assert_eq!(cells.len(), ORBIT_TOP_N);
        for c in &cells {
            assert!(c.row < 12, "row {} out of bounds", c.row);
            assert!(c.col < 30, "col {} out of bounds", c.col);
        }
    }

    #[test]
    fn compute_glyphs_returns_empty_on_tiny_or_empty() {
        let frame = OrbitFrame::default();
        assert!(compute_glyphs(20, 40, &frame).is_empty());
        let frame_with_one = OrbitFrame {
            processes: vec![OrbitProc {
                pid: 1,
                name: "init".into(),
                cpu_pct: 1.0,
                state: 'S',
            }],
            new_pids: HashSet::new(),
        };
        // 2-row area is too small for an ellipse; return nothing
        // rather than collapse onto a line.
        assert!(compute_glyphs(2, 40, &frame_with_one).is_empty());
        assert!(compute_glyphs(20, 4, &frame_with_one).is_empty());
    }

    #[test]
    fn build_picks_top_n_by_cpu_and_diffs_pids() {
        let rows = vec![
            mk_proc(10, 90.0, 'R', "hot"),
            mk_proc(20, 50.0, 'R', "warm"),
            mk_proc(30, 1.0, 'S', "cool"),
            mk_proc(40, 0.0, 'S', "idle"),
        ];
        let mut prev = HashSet::new();
        prev.insert(20); // pid 20 was here last tick — not new
        prev.insert(99); // pid 99 has gone away — irrelevant
        let frame = OrbitFrame::build(&rows, &prev);
        // Sorted by cpu desc.
        assert_eq!(frame.processes.len(), 4);
        assert_eq!(frame.processes[0].pid, 10);
        assert_eq!(frame.processes[1].pid, 20);
        // pid 10/30/40 are new; pid 20 is not.
        assert!(frame.new_pids.contains(&10));
        assert!(!frame.new_pids.contains(&20));
        assert!(frame.new_pids.contains(&30));
    }

    #[test]
    fn build_skips_kernel_threads_with_empty_command() {
        // Kernel threads have empty `command` strings — those would
        // crowd the centre at 0% CPU. The orbit is for userspace.
        let rows = vec![mk_proc(1, 5.0, 'S', "init"), mk_proc(2, 10.0, 'S', "")];
        let frame = OrbitFrame::build(&rows, &HashSet::new());
        assert_eq!(frame.processes.len(), 1);
        assert_eq!(frame.processes[0].pid, 1);
    }

    #[test]
    fn truncate_chars_respects_utf8_boundaries() {
        // 8-byte cap that would land mid-char must back off.
        let s = "✨magicwand"; // ✨ is 3 bytes
        let t = truncate_chars(s, 8);
        assert!(s.starts_with(&t));
        assert!(t.is_char_boundary(t.len()));
    }

    #[test]
    fn display_name_strips_path_and_args() {
        // Canonical case: full binary path with args. We want
        // just the binary basename, not the path prefix that
        // would collapse with every other process under that dir.
        assert_eq!(
            display_name("/usr/lib/firefox/firefox --new-window https://example.com"),
            "firefox"
        );
        assert_eq!(display_name("/usr/bin/bash -l"), "bash");
        // Bare command (no path) — passes through.
        assert_eq!(display_name("vim"), "vim");
    }

    #[test]
    fn display_name_keeps_kernel_thread_brackets() {
        // /proc cmdline is empty for kernel threads; the procs
        // module substitutes `[kworker/0:1]` etc. We keep the
        // brackets so the user can spot kernel-thread CPU.
        assert_eq!(display_name("[kworker/0:1]"), "[kworker/0:1]");
        assert_eq!(display_name("[ksoftirqd/2]"), "[ksoftirqd/2]");
    }

    #[test]
    fn display_name_disambiguates_collapsed_paths() {
        // The original bug: two distinct processes both under
        // `/usr/lib/...` no longer collapse to "usr/lib".
        let firefox = display_name("/usr/lib/firefox/firefox -P default");
        let chromium = display_name("/usr/lib/chromium/chromium --type=renderer");
        assert_ne!(firefox, chromium);
        assert_eq!(firefox, "firefox");
        assert_eq!(chromium, "chromium");
    }

    #[test]
    fn display_name_handles_empty_and_whitespace() {
        assert_eq!(display_name(""), "");
        assert_eq!(display_name("   "), "");
        // Single-slash edge case: "/" should not panic; basename
        // of "/" is empty, so we keep the original token "/".
        assert_eq!(display_name("/"), "/");
    }
}
