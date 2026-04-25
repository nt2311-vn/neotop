//! neotop — oxker-style TUI for live neosandbox VMs.
//!
//! Polls every `state.json` under `$NEOSANDBOX_STATE/run/*/state.json`
//! at a fixed cadence and renders a table of the current fleet plus a
//! serial-tail pane for the selected VM.
//!
//! It is a pure observer: no writes, no RPC, nothing to go wrong on the
//! vmmd side. If a state file is truncated mid-read, we surface the row
//! as `?` and move on — vmmd writes atomically via rename, so this only
//! happens in the narrow window of initial `makePath`.
//!
//! Usage:
//!     neotop                      # watch `$NEOSANDBOX_STATE/run`
//!     neotop --state-dir <path>   # watch <path>/run
//!     neotop --refresh-ms 500     # faster poll (default 1000 ms)
//!
//! Controls:
//!     q or Ctrl-C   quit
//!     ?             toggle the keybindings overlay
//!     Tab           switch between Vms and Procs view
//!     j / Down      next row
//!     k / Up        prev row
//!     `PgDn` / `PgUp`   jump 10 rows
//!     r             refresh immediately
//!     + / -         halve / double the refresh interval (50 ms .. 5 s)
//!     x             (Vms)   delete state file of the selected halted vm
//!     s             (Procs) cycle sort key (CPU → MEM → PID → CMD)
//!     t             (Procs) toggle tree view (parent → children)
//!     /             (Procs) enter filter mode (Esc to clear, Enter to confirm)
//!     K             (Procs) send SIGTERM to selected pid (with confirm)
//!     Ctrl-K        (Procs) send SIGKILL to selected pid (with confirm)

mod battery;
mod disk;
mod errors;
mod host;
mod net;
mod proc;
mod procs;
mod temp;

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState,
    Sparkline, Table, TableState, Wrap,
};
use ratatui::Terminal;
use serde::Deserialize;

// -----------------------------------------------------------------------------
// CLI
// -----------------------------------------------------------------------------

struct Args {
    state_dir: PathBuf,
    refresh: Duration,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut state_dir: Option<PathBuf> = None;
        // 1 Hz default — same ballpark as `htop` / `btop` / `iotop`.
        // 250 ms updates earlier in development looked impressive but
        // turned every value into a stock-ticker that the eye can't
        // actually read. With EMA smoothing already in place, 1 Hz
        // is a calm dashboard cadence; the user can still drop to
        // 100 ms via `+` if they're chasing a specific spike.
        let mut refresh_ms: u64 = 1000;

        let mut it = std::env::args().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--state-dir" => {
                    state_dir = Some(PathBuf::from(
                        it.next().context("--state-dir requires a path")?,
                    ));
                }
                "--refresh-ms" => {
                    refresh_ms = it
                        .next()
                        .context("--refresh-ms requires a number")?
                        .parse()
                        .context("invalid --refresh-ms")?;
                }
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }

        let state_dir = state_dir
            .or_else(|| std::env::var_os("NEOSANDBOX_STATE").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(".neosandbox"));

        Ok(Self {
            state_dir,
            refresh: Duration::from_millis(refresh_ms),
        })
    }
}

fn print_help() {
    println!(
        "neotop — live view of running neosandbox VMs and host processes\n\
         \n\
         USAGE:\n    \
             neotop [--state-dir <path>] [--refresh-ms <n>]\n\
         \n\
         Defaults to $NEOSANDBOX_STATE or ./.neosandbox if unset.\n\
         \n\
         CONTROLS:\n    \
             q            quit\n    \
             ?            keybindings overlay\n    \
             Tab          toggle Vms / Procs view\n    \
             j / Down     next row\n    \
             k / Up       prev row\n    \
             PgDn / PgUp  jump 10 rows\n    \
             r            refresh immediately\n    \
             + / -        speed up / slow down refresh tick\n    \
             x  (Vms)     delete state file for selected halted vm\n    \
             s  (Procs)   cycle sort: CPU → MEM → PID → CMD\n    \
             t  (Procs)   toggle tree view\n    \
             /  (Procs)   enter filter mode\n    \
             K  (Procs)   SIGTERM selected pid (confirmed)\n    \
             Ctrl-K       SIGKILL selected pid (confirmed)"
    );
}

// -----------------------------------------------------------------------------
// Schema — mirrors engine/vmmd/src/state.zig
// -----------------------------------------------------------------------------

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)] // schema mirrors state.zig; not every field is displayed yet
struct Exits {
    io: u64,
    mmio: u64,
    hlt: u64,
    shutdown: u64,
    total: u64,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)] // schema mirrors state.zig; not every field is displayed yet
struct StateFile {
    schema: u32,
    vm_id: String,
    pid: i64,
    mode: String,
    kernel_path: Option<String>,
    phase: String,
    started_at_ns: i128,
    updated_at_ns: i128,
    exits: Exits,
    serial_tail: String,
}

#[derive(Debug, Clone)]
struct VmRow {
    path: PathBuf,
    state: StateFile,
    /// CPU% over the last refresh interval. `None` if the process is gone
    /// or we have not sampled twice yet.
    cpu_pct: Option<f64>,
    /// Live `/proc/<pid>/` snapshot, refreshed every scan.
    proc: Option<proc::Snapshot>,
}

/// Per-pid state carried across scans, used to compute CPU% as a delta
/// of cumulative jiffies over wall-clock time.
#[derive(Debug, Clone, Copy)]
struct CpuSample {
    taken_at: Instant,
    jiffies: u64,
}

/// Ring buffer of recent CPU% samples per pid, feeding the sparkline.
/// 60 samples × 1 s tick (default) = last minute, which is what a
/// human eyeball can actually parse as "what's happening right now".
/// At a tighter `+`-tuned tick the window scales accordingly.
const CPU_HISTORY_CAP: usize = 60;

/// Ring buffer for host-level CPU% / memory% / net rate histories.
/// Same window logic as `CpuHistory`: 60 samples × 1 s tick = last
/// minute. CPU and mem are stored as `0..=100` percentages; net is
/// stored as raw bytes/sec so `draw_host_history` can compute a
/// rolling max for the sparkline ceiling and label the actual rate.
#[derive(Debug, Default)]
struct HostHistory {
    cpu: VecDeque<u64>,
    mem: VecDeque<u64>,
    net_down: VecDeque<u64>,
    net_up: VecDeque<u64>,
}

impl HostHistory {
    fn push(&mut self, cpu_pct: Option<f64>, mem_pct: f64, net_down_bps: u64, net_up_bps: u64) {
        push_pct(&mut self.cpu, cpu_pct.unwrap_or(0.0));
        push_pct(&mut self.mem, mem_pct);
        push_raw(&mut self.net_down, net_down_bps);
        push_raw(&mut self.net_up, net_up_bps);
    }
}

fn push_pct(buf: &mut VecDeque<u64>, pct: f64) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let v = pct.clamp(0.0, 100.0) as u64;
    push_raw(buf, v);
}

fn push_raw(buf: &mut VecDeque<u64>, v: u64) {
    if buf.len() == CPU_HISTORY_CAP {
        buf.pop_front();
    }
    buf.push_back(v);
}

#[derive(Debug, Default)]
struct CpuHistory {
    per_pid: HashMap<i64, VecDeque<u64>>,
}

impl CpuHistory {
    fn push(&mut self, pid: i64, pct: f64) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let scaled = pct.clamp(0.0, 100.0) as u64;
        let entry = self.per_pid.entry(pid).or_default();
        if entry.len() == CPU_HISTORY_CAP {
            entry.pop_front();
        }
        entry.push_back(scaled);
    }

    fn get(&self, pid: i64) -> Option<&VecDeque<u64>> {
        self.per_pid.get(&pid)
    }

    fn retain(&mut self, pids: &[i64]) {
        self.per_pid.retain(|k, _| pids.contains(k));
    }
}

fn scan(
    run_dir: &Path,
    prev_cpu: &mut HashMap<i64, CpuSample>,
    history: &mut CpuHistory,
    clk_tck: u64,
) -> Vec<VmRow> {
    let Ok(entries) = fs::read_dir(run_dir) else {
        return Vec::new();
    };

    let now = Instant::now();
    let mut rows = Vec::new();
    let mut seen: Vec<i64> = Vec::new();

    for e in entries.flatten() {
        let state_path = e.path().join("state.json");
        let Ok(bytes) = fs::read(&state_path) else {
            continue;
        };
        // The vmmd writer is atomic (rename), but a brand-new run dir may
        // not have the file yet — that's the `Err` above. Parse errors
        // would mean corruption; drop silently and the row will reappear
        // on the next poll.
        let Ok(state) = serde_json::from_slice::<StateFile>(&bytes) else {
            continue;
        };

        let pid = state.pid;
        seen.push(pid);

        let snap = proc::snapshot(pid);
        let cpu_pct = match (&snap, prev_cpu.get(&pid)) {
            (Some(s), Some(prev)) => {
                let dt = now.duration_since(prev.taken_at).as_secs_f64();
                if dt > 0.0 {
                    let delta = s.stat.cpu_jiffies.saturating_sub(prev.jiffies);
                    #[allow(clippy::cast_precision_loss)]
                    let used_secs = delta as f64 / clk_tck as f64;
                    Some((used_secs / dt) * 100.0)
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some(s) = &snap {
            prev_cpu.insert(
                pid,
                CpuSample {
                    taken_at: now,
                    jiffies: s.stat.cpu_jiffies,
                },
            );
        }
        if let Some(p) = cpu_pct {
            history.push(pid, p);
        }

        rows.push(VmRow {
            path: state_path,
            state,
            cpu_pct,
            proc: snap,
        });
    }
    rows.sort_by(|a, b| a.state.pid.cmp(&b.state.pid));

    // Drop samples for pids that disappeared so the maps can't grow without bound.
    prev_cpu.retain(|k, _| seen.contains(k));
    history.retain(&seen);
    rows
}

/// Duration → milliseconds as `f64`. Convenience for the perf footer
/// where sub-millisecond precision matters.
fn duration_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Host memory usage as a 0..=100 percentage. Returns `0.0` when
/// `mem_total_bytes` is unknown so the sparkline degrades gracefully.
fn mem_used_pct(h: &host::HostInfo) -> f64 {
    if h.mem_total_bytes == 0 {
        return 0.0;
    }
    let used = h.mem_total_bytes.saturating_sub(h.mem_avail_bytes);
    #[allow(clippy::cast_precision_loss)]
    {
        (used as f64 / h.mem_total_bytes as f64) * 100.0
    }
}

fn now_ns() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i128::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

fn format_uptime(start_ns: i128, now: i128) -> String {
    let ns = (now - start_ns).max(0);
    let secs = ns / 1_000_000_000;
    let ms = (ns % 1_000_000_000) / 1_000_000;
    if secs >= 60 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{m}m{s:02}s")
    } else if secs >= 1 {
        format!("{secs}.{ms:03}s")
    } else {
        format!("{ms}ms")
    }
}

fn phase_style(phase: &str) -> Style {
    match phase {
        "running" => Style::default().fg(Color::Green),
        "booting" => Style::default().fg(Color::Yellow),
        "halted" => Style::default().fg(Color::Gray),
        "shutdown" => Style::default().fg(Color::Magenta),
        "error" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        _ => Style::default(),
    }
}

fn one_line(s: &str) -> String {
    // Collapse the serial tail into a single short line for the table row.
    let last = s.lines().rfind(|l| !l.is_empty()).unwrap_or("");
    if last.len() > 60 {
        format!("…{}", &last[last.len() - 59..])
    } else {
        last.to_string()
    }
}

// -----------------------------------------------------------------------------
// TUI main loop
// -----------------------------------------------------------------------------

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "neotop is Linux-only for now — every data source it reads \
         (/proc, /sys/class/hwmon, /sys/class/power_supply, cgroup v2) \
         is a Linux kernel thing.\n\
         \n\
         Porting to {} would need a separate module using the \
         platform's native APIs. See README for notes.",
        std::env::consts::OS,
    );
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() -> Result<()> {
    let args = Args::parse()?;
    let run_dir = args.state_dir.join("run");

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &run_dir, args.refresh);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

// -----------------------------------------------------------------------------
// App state — what the run loop owns
// -----------------------------------------------------------------------------

/// Which table the user is currently driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Vms,
    Procs,
}

/// Modal input state. `Normal` is the default; `Filter` captures
/// every printable char into `App.procs_filter`; `Confirm` shows a
/// y/N prompt for a queued kill signal; `Help` paints the centered
/// keybindings overlay.
#[derive(Debug, Clone)]
enum InputMode {
    Normal,
    Filter,
    Confirm(KillSig),
    Help,
}

#[derive(Debug, Clone, Copy)]
enum KillSig {
    Term,
    Kill,
}

impl KillSig {
    fn label(self) -> &'static str {
        match self {
            Self::Term => "SIGTERM",
            Self::Kill => "SIGKILL",
        }
    }

    fn signal(self) -> rustix::process::Signal {
        match self {
            Self::Term => rustix::process::Signal::Term,
            Self::Kill => rustix::process::Signal::Kill,
        }
    }
}

/// Self-profiling counters. Updated on every scan + draw so the
/// footer can show how much overhead neotop itself imposes. The point
/// is honesty: if these numbers ever look bad, that's a bug to chase,
/// not something to hide from the user.
#[derive(Debug, Clone, Copy, Default)]
struct Perf {
    /// Wall-clock duration of the most recent `App::refresh` call.
    scan_ms: f64,
    /// Wall-clock duration of the most recent `terminal.draw` call.
    render_ms: f64,
    /// Time between the *start* of the last two scans. Should track
    /// `args.refresh` closely; if it's much higher we're falling behind.
    refresh_actual_ms: f64,
    /// Our own `VmRSS` at the last sample. Re-read every `RSS_RETICK_EVERY`
    /// scans because parsing /proc/self/status is the most expensive bit.
    own_rss_bytes: u64,
    /// Our own CPU% — same delta math as the per-VM tracker.
    own_cpu_pct: Option<f64>,
}

/// Re-read `/proc/self/status` (for `VmRSS`) only every Nth scan tick.
/// `VmRSS` doesn't move much for an idle TUI; once a second is plenty.
const RSS_RETICK_EVERY: u32 = 4;

#[derive(Debug, Default)]
struct PerfTracker {
    perf: Perf,
    last_scan_started: Option<Instant>,
    own_prev: Option<(Instant, u64)>,
    rss_tick: u32,
}

struct App {
    view: View,
    input: InputMode,

    // Vms view
    vms: Vec<VmRow>,
    vms_table: TableState,
    prev_cpu: HashMap<i64, CpuSample>,
    history: CpuHistory,

    // Procs view
    procs_tracker: procs::Tracker,
    passwd: procs::PasswdCache,
    procs_all: Vec<procs::ProcessRow>,
    /// Rendered process rows after sort + filter (or tree expansion).
    procs_visible: Vec<ProcRender>,
    procs_table: TableState,
    procs_sort: procs::SortBy,
    procs_filter: String,
    /// When true, the procs table renders as a parent → children tree
    /// instead of the sortable flat list. Sort and filter are ignored
    /// in tree mode (a future iteration may layer them back on).
    tree_mode: bool,

    // Host overview
    prev_host_cpu: host::CpuSamples,
    host_info: host::HostInfo,
    net_tracker: net::Tracker,
    ifaces: Vec<net::Iface>,
    temp_tracker: temp::Tracker,
    temps: Vec<temp::Reading>,
    batteries: Vec<battery::Battery>,
    disk_tracker: disk::Tracker,
    disks: Vec<disk::Disk>,
    host_history: HostHistory,

    // Tunables
    clk_tck: u64,
    last_scan: Instant,
    /// Live refresh interval. Initialised from `--refresh-ms` and
    /// then mutable at runtime via `+` / `-`.
    refresh: Duration,
    /// Wraps 0..`SLOW_TICK_EVERY`. When it hits zero we re-scan
    /// hwmon temperatures, batteries, and disk I/O — three sources
    /// that change once per second at best and don't need to gate
    /// the UI tick. See `SLOW_TICK_EVERY` for the cadence math.
    slow_tick_counter: u32,
    /// When `true`, `tick()` is skipped: every snapshot is frozen
    /// where it was when the user pressed `space`. Input keeps
    /// working, you can scroll, sort, kill, etc. — useful for
    /// reading a busy table without rows shuffling underneath.
    paused: bool,
    /// Running EMA of host CPU%. The instantaneous reading from
    /// `/proc/stat` jumps wildly between consecutive 1 s windows
    /// (one short busy-burst can shift the average by 30+ points);
    /// smoothing with the same `procs::ema_blend` curve makes the
    /// line-1 number readable without lying about the underlying
    /// activity. Reset to `None` on first launch and after any pid
    /// data clears.
    smoothed_host_cpu: Option<f64>,

    // Self-profiling
    perf: PerfTracker,

    // Non-fatal parser/IO errors surfaced in the footer.
    errors: errors::ErrorRing,
}

/// Refresh-interval clamps for the `+`/`-` keys. Below ~50 ms the
/// terminal can't keep up with the redraw; above 5 s it's no longer
/// a "live" view.
const MIN_REFRESH: Duration = Duration::from_millis(50);
const MAX_REFRESH: Duration = Duration::from_millis(5000);

/// How many fast ticks pass between full slow-scanner runs. At the
/// default 1 s tick this means temps / batteries / disks refresh
/// once every 4 seconds — which is plenty: hwmon updates at ~1 Hz on
/// real hardware, batteries drift on a multi-second timescale, and
/// disk-rate spikes you care about live for whole seconds. Cuts the
/// per-tick cost on machines with lots of hwmon nodes.
const SLOW_TICK_EVERY: u32 = 4;

impl App {
    fn new(run_dir: &Path, refresh: Duration) -> Self {
        let clk_tck = proc::clk_tck();
        let mut prev_cpu: HashMap<i64, CpuSample> = HashMap::new();
        let mut history = CpuHistory::default();
        let mut net_tracker = net::Tracker::default();
        let mut procs_tracker = procs::Tracker::default();
        let passwd = procs::PasswdCache::load();
        let mut errors = errors::ErrorRing::new();
        let prev_host_cpu = host::read_cpu_samples(&mut errors);
        let host_info = host::snapshot(None, &mut errors);
        let ifaces = net_tracker.snapshot(&mut errors);
        let mut temp_tracker = temp::Tracker::default();
        let temps = temp_tracker.snapshot(&mut errors);
        let batteries = battery::snapshot();
        let mut disk_tracker = disk::Tracker::default();
        let disks = disk_tracker.snapshot(&mut errors);
        let vms = scan(run_dir, &mut prev_cpu, &mut history, clk_tck);
        let procs_all = procs_tracker.snapshot(&passwd, clk_tck);

        let mut vms_table = TableState::default();
        if !vms.is_empty() {
            vms_table.select(Some(0));
        }
        let mut procs_table = TableState::default();
        let procs_visible = compute_visible_flat(&procs_all, procs::SortBy::Cpu, "");
        if !procs_visible.is_empty() {
            procs_table.select(Some(0));
        }

        // Smart default view: if there's no neosandbox state-dir at
        // all, the Vms table will only ever show "(empty)". In that
        // case start in Procs view so neotop is immediately useful as
        // a system monitor. If the run-dir does exist (even if empty
        // right now), keep the Vms default — the user is probably
        // watching for a VM to come up.
        let view = if run_dir.is_dir() {
            View::Vms
        } else {
            View::Procs
        };

        Self {
            view,
            input: InputMode::Normal,
            vms,
            vms_table,
            prev_cpu,
            history,
            procs_tracker,
            passwd,
            procs_all,
            procs_visible,
            procs_table,
            procs_sort: procs::SortBy::Cpu,
            procs_filter: String::new(),
            tree_mode: false,
            prev_host_cpu,
            host_info,
            net_tracker,
            ifaces,
            temp_tracker,
            temps,
            batteries,
            disk_tracker,
            disks,
            host_history: HostHistory::default(),
            clk_tck,
            last_scan: Instant::now(),
            refresh,
            slow_tick_counter: 0,
            paused: false,
            smoothed_host_cpu: None,
            perf: PerfTracker::default(),
            errors,
        }
    }

    /// Re-sample everything that goes into the UI. Also updates the
    /// self-profiling counters so the perf footer always shows the
    /// most recent scan, not the previous one.
    fn tick(&mut self, run_dir: &Path) {
        let started = Instant::now();
        if let Some(prev) = self.perf.last_scan_started {
            self.perf.perf.refresh_actual_ms = duration_ms(started.duration_since(prev));
        }
        self.perf.last_scan_started = Some(started);

        // Fast-path scanners: every tick. These drive the live
        // numbers (CPU% bar, sparkline, mem%, net rates, procs).
        self.vms = scan(run_dir, &mut self.prev_cpu, &mut self.history, self.clk_tck);
        self.host_info = host::snapshot(Some(&self.prev_host_cpu), &mut self.errors);
        self.prev_host_cpu = host::read_cpu_samples(&mut self.errors);
        self.ifaces = self.net_tracker.snapshot(&mut self.errors);

        // EMA-smooth the host CPU%. Same blending curve we use for
        // per-pid CPU% in `procs::Tracker`: keeps the displayed
        // number from yo-yoing between e.g. 12% and 47% on
        // consecutive ticks, while still tracking sustained changes.
        // We overwrite `host_info.cpu_pct` so every consumer
        // (line-1 display, sparkline feed, perf footer) sees the
        // same calmed-down value.
        if let Some(new) = self.host_info.cpu_pct {
            let smoothed = match self.smoothed_host_cpu {
                Some(prev) => procs::ema_blend(prev, new),
                None => new,
            };
            self.smoothed_host_cpu = Some(smoothed);
            self.host_info.cpu_pct = Some(smoothed);
        }

        // Slow-path scanners: every `SLOW_TICK_EVERY` ticks (~4 s at
        // the default 1 s tick). The data they read updates at most
        // once per second on real hardware — re-walking
        // `/sys/class/hwmon` every tick was a pure waste of file
        // descriptors and event-loop time, and it showed up in the
        // perf footer as a fat `scan_ms`. We always run them on the
        // very first tick (counter == 0) so the UI isn't blank while
        // the user waits the first cycle after launch.
        if self.slow_tick_counter == 0 {
            self.temps = self.temp_tracker.snapshot(&mut self.errors);
            self.batteries = battery::snapshot();
            self.disks = self.disk_tracker.snapshot(&mut self.errors);
        }
        self.slow_tick_counter = (self.slow_tick_counter + 1) % SLOW_TICK_EVERY;

        // Capture which PID the cursor is on *before* re-snapshotting,
        // so we can re-anchor the row index after sort/filter changes.
        // Without this, sorting by CPU% (the default) would slide the
        // selection from process to process as load shifts — horrible
        // for trying to actually watch one PID.
        let prev_selected_pid = self.selected_proc().map(|r| r.pid);
        self.procs_all = self.procs_tracker.snapshot(&self.passwd, self.clk_tck);
        self.recompute_procs();
        self.reanchor_proc_selection(prev_selected_pid);
        self.clamp_selections();

        // Feed the host history *after* host_info has been refreshed
        // so the sparkline tracks the same numbers shown in the line-1
        // summary, not the previous tick.
        let mem_pct = mem_used_pct(&self.host_info);
        let (net_down, net_up) = total_net_rates(&self.ifaces);
        self.host_history
            .push(self.host_info.cpu_pct, mem_pct, net_down, net_up);

        self.update_self_perf(started);

        self.perf.perf.scan_ms = duration_ms(started.elapsed());
        self.last_scan = Instant::now();
    }

    /// Read `/proc/self/{stat,status}` and refresh the own-CPU%/RSS
    /// fields of the perf tracker. RSS is throttled to once per
    /// `RSS_RETICK_EVERY` scans because it's the most expensive of the
    /// two reads and barely moves between ticks.
    fn update_self_perf(&mut self, now: Instant) {
        if let Some(j) = proc::self_jiffies() {
            self.perf.perf.own_cpu_pct = self.perf.own_prev.map(|(t, prev_j)| {
                let dt = now.duration_since(t).as_secs_f64();
                if dt <= 0.0 {
                    0.0
                } else {
                    let dj = j.saturating_sub(prev_j);
                    #[allow(clippy::cast_precision_loss)]
                    let used = dj as f64 / self.clk_tck as f64;
                    (used / dt) * 100.0
                }
            });
            self.perf.own_prev = Some((now, j));
        }
        if self.perf.rss_tick == 0 {
            if let Some(rss) = proc::self_rss_bytes() {
                self.perf.perf.own_rss_bytes = rss;
            }
            self.perf.rss_tick = RSS_RETICK_EVERY;
        }
        self.perf.rss_tick = self.perf.rss_tick.saturating_sub(1);
    }

    fn recompute_procs(&mut self) {
        // Both flat and tree modes now respect the same `sort` and
        // `filter`. Before 0.7.0 the tree path ignored both; that
        // made the `t` toggle far less useful than it could be — you
        // couldn't grep for a process and still see its parent
        // chain. The tree's *shape* is preserved either way; sort
        // only reorders siblings, filter only hides leaves whose
        // entire subtree fails to match.
        self.procs_visible = if self.tree_mode {
            compute_visible_tree(&self.procs_all, self.procs_sort, &self.procs_filter)
        } else {
            compute_visible_flat(&self.procs_all, self.procs_sort, &self.procs_filter)
        };
    }

    /// After `procs_visible` has been recomputed, find the row index
    /// corresponding to `pid` (if it still exists in the visible set)
    /// and pin the cursor to it. Falls back silently when the pid is
    /// gone (process exited) or filtered out — `clamp_selections` will
    /// then put the cursor on whatever row 0 is now.
    fn reanchor_proc_selection(&mut self, pid: Option<i32>) {
        let Some(pid) = pid else { return };
        if let Some(new_idx) = self
            .procs_visible
            .iter()
            .position(|r| self.procs_all.get(r.idx).is_some_and(|row| row.pid == pid))
        {
            self.procs_table.select(Some(new_idx));
        }
    }

    fn clamp_selections(&mut self) {
        let sel = self.vms_table.selected().unwrap_or(0);
        if self.vms.is_empty() {
            self.vms_table.select(None);
        } else if sel >= self.vms.len() {
            self.vms_table.select(Some(self.vms.len() - 1));
        } else if self.vms_table.selected().is_none() {
            self.vms_table.select(Some(0));
        }

        let psel = self.procs_table.selected().unwrap_or(0);
        if self.procs_visible.is_empty() {
            self.procs_table.select(None);
        } else if psel >= self.procs_visible.len() {
            self.procs_table.select(Some(self.procs_visible.len() - 1));
        } else if self.procs_table.selected().is_none() {
            self.procs_table.select(Some(0));
        }
    }

    fn selected_proc(&self) -> Option<&procs::ProcessRow> {
        let i = self.procs_table.selected()?;
        let r = self.procs_visible.get(i)?;
        self.procs_all.get(r.idx)
    }
}

/// One rendered row in the Procs table. `idx` indexes back into
/// `App.procs_all`; `prefix` is the tree-glyph prefix (`""` in flat
/// mode, e.g. `"│ ├─"` in tree mode) that will be prepended to the
/// COMMAND cell.
#[derive(Debug, Clone, Default)]
struct ProcRender {
    idx: usize,
    prefix: String,
}

/// Comparator used by both flat and tree paths so the two stay in
/// lockstep. CPU / RSS sort descending (biggest at the top — what
/// the eye expects from htop); PID / command sort ascending.
fn cmp_rows(
    rows: &[procs::ProcessRow],
    a: usize,
    b: usize,
    by: procs::SortBy,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match by {
        procs::SortBy::Cpu => rows[b]
            .cpu_pct
            .unwrap_or(0.0)
            .partial_cmp(&rows[a].cpu_pct.unwrap_or(0.0))
            .unwrap_or(Ordering::Equal),
        procs::SortBy::Mem => rows[b].rss_bytes.cmp(&rows[a].rss_bytes),
        procs::SortBy::Pid => rows[a].pid.cmp(&rows[b].pid),
        procs::SortBy::Command => rows[a].command.cmp(&rows[b].command),
    }
}

/// Flat-list path: filter then sort, return one `ProcRender` per
/// surviving row with an empty prefix.
fn compute_visible_flat(
    rows: &[procs::ProcessRow],
    by: procs::SortBy,
    filter: &str,
) -> Vec<ProcRender> {
    let mut idxs: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| procs::matches(r, filter))
        .map(|(i, _)| i)
        .collect();
    idxs.sort_by(|&a, &b| cmp_rows(rows, a, b, by));
    idxs.into_iter()
        .map(|idx| ProcRender {
            idx,
            prefix: String::new(),
        })
        .collect()
}

/// Tree-mode path: build a parent→children adjacency map from the
/// pid / ppid columns, then DFS from the roots producing a flat
/// render list with the right `├─ │ └─` glyphs.
///
/// Filter and sort *do* apply now (they didn't in 0.6.0 and earlier):
/// * **filter** \u2014 a node is shown iff itself OR any descendant
///   matches `filter`. That keeps ancestor chains visible so the
///   matched leaf has context. Computed in a memoised post-order
///   pass before the render DFS.
/// * **sort** \u2014 siblings within each parent are ordered by the
///   chosen `SortBy` (CPU / mem / pid / cmd). The tree shape is
///   preserved; only the order inside each child list moves.
fn compute_visible_tree(
    rows: &[procs::ProcessRow],
    by: procs::SortBy,
    filter: &str,
) -> Vec<ProcRender> {
    use std::collections::HashSet;

    let mut children: HashMap<i32, Vec<usize>> = HashMap::new();
    let mut have_pid: HashSet<i32> = HashSet::new();
    for (i, r) in rows.iter().enumerate() {
        have_pid.insert(r.pid);
        children.entry(r.ppid).or_default().push(i);
    }
    // Sort siblings by the chosen key. The tree's *shape* is fixed
    // by ppid — only ordering inside each child list changes.
    for kids in children.values_mut() {
        kids.sort_by(|&a, &b| cmp_rows(rows, a, b, by));
    }

    // Compute the "alive" set: rows that match the filter or have
    // at least one descendant that does. Memoised so even very deep
    // trees stay O(N).
    let mut alive: HashSet<usize> = HashSet::new();
    let mut visiting: HashSet<usize> = HashSet::new();
    for i in 0..rows.len() {
        mark_alive_recursive(i, rows, &children, filter, &mut alive, &mut visiting);
    }

    // Roots: ppid is 0 (kernel) or refers to a pid we don't have a
    // row for (process exited mid-scan, kernel thread etc.).
    let mut roots: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.ppid <= 0 || !have_pid.contains(&r.ppid))
        .map(|(i, _)| i)
        .collect();
    roots.sort_by(|&a, &b| cmp_rows(rows, a, b, by));
    roots.retain(|i| alive.contains(i));

    let mut out = Vec::with_capacity(rows.len());
    let mut ancestor_last: Vec<bool> = Vec::new();
    let total = roots.len();
    for (n, &root_idx) in roots.iter().enumerate() {
        let last = n + 1 == total;
        dfs_tree(
            rows,
            &children,
            &alive,
            root_idx,
            &mut ancestor_last,
            last,
            0,
            &mut out,
        );
    }
    out
}

/// Post-order memoised "is this node or any descendant alive under
/// the filter?" walk. `visiting` guards against the rare case where
/// `/proc` reports a cycle (shouldn't happen, but pid wraparound +
/// races make it cheap insurance against a stack-overflow panic).
fn mark_alive_recursive(
    idx: usize,
    rows: &[procs::ProcessRow],
    children: &HashMap<i32, Vec<usize>>,
    filter: &str,
    alive: &mut std::collections::HashSet<usize>,
    visiting: &mut std::collections::HashSet<usize>,
) -> bool {
    if alive.contains(&idx) {
        return true;
    }
    if !visiting.insert(idx) {
        return false; // cycle guard
    }
    let mut ok = procs::matches(&rows[idx], filter);
    if let Some(kids) = children.get(&rows[idx].pid) {
        for &k in kids {
            if mark_alive_recursive(k, rows, children, filter, alive, visiting) {
                ok = true;
            }
        }
    }
    visiting.remove(&idx);
    if ok {
        alive.insert(idx);
    }
    ok
}

// `clippy::too_many_arguments`: 8 args is more than the default
// threshold but every one is necessary state for a *recursive*
// tree walk. Bundling them into a struct would obscure the
// recursion (we'd be threading `&mut struct` through anyway) and
// add a layer of indirection for no win.
#[allow(clippy::too_many_arguments)]
fn dfs_tree(
    rows: &[procs::ProcessRow],
    children: &HashMap<i32, Vec<usize>>,
    alive: &std::collections::HashSet<usize>,
    idx: usize,
    ancestor_last: &mut Vec<bool>,
    is_last_sibling: bool,
    depth: usize,
    out: &mut Vec<ProcRender>,
) {
    // Roots (depth 0) don't get a tree-branch prefix — they sit
    // flush-left. For deeper nodes, each *non-root* ancestor
    // contributes either '  ' (it was its parent's last child) or
    // '│ ' (more siblings follow), then this node itself gets '├─'
    // or '└─' depending on whether more siblings follow at its level.
    let mut prefix = String::new();
    if depth > 0 {
        for &al in ancestor_last.iter() {
            prefix.push_str(if al { "  " } else { "│ " });
        }
        prefix.push_str(if is_last_sibling { "└─" } else { "├─" });
    }
    out.push(ProcRender { idx, prefix });

    // Only walk into *alive* children. Because the alive set was
    // computed in advance, a filtered node still hosts visible
    // descendants if any of them passed the filter — we just don't
    // emit the dead intermediates.
    let alive_kids: Vec<usize> = children
        .get(&rows[idx].pid)
        .map(|kids| kids.iter().copied().filter(|k| alive.contains(k)).collect())
        .unwrap_or_default();

    let n = alive_kids.len();
    // Only push our own is_last_sibling onto the ancestor stack when
    // we're not the root — the root's status doesn't visually carry
    // into descendants.
    let push = depth > 0;
    if push {
        ancestor_last.push(is_last_sibling);
    }
    for (i, k) in alive_kids.into_iter().enumerate() {
        let last = i + 1 == n;
        dfs_tree(
            rows,
            children,
            alive,
            k,
            ancestor_last,
            last,
            depth + 1,
            out,
        );
    }
    if push {
        ancestor_last.pop();
    }
}

fn run<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    run_dir: &Path,
    refresh: Duration,
) -> Result<()> {
    let mut app = App::new(run_dir, refresh);

    loop {
        // Drain *all* queued input first, then redraw once. Holding `j`
        // used to fire one redraw per keypress, which on slow terminals
        // turned ~33 ms key-repeat into visible chunkiness. With this
        // collapse, a burst of ten queued j's becomes one render at the
        // right final position.
        while let Some(k) = poll_key(Duration::ZERO)? {
            if handle_key(&mut app, k, run_dir) {
                return Ok(());
            }
        }

        let render_started = Instant::now();
        terminal.draw(|f| draw(f, run_dir, &mut app))?;
        app.perf.perf.render_ms = duration_ms(render_started.elapsed());

        // Block until either the next key arrives or the refresh
        // interval elapses, whichever is first. Idle = ~0 CPU.
        let elapsed = app.last_scan.elapsed();
        let wait = app.refresh.saturating_sub(elapsed);
        if let Some(k) = poll_key(wait)? {
            if handle_key(&mut app, k, run_dir) {
                return Ok(());
            }
        }

        // When paused, every snapshot is frozen but we still
        // service input — that's the whole point of pausing. We do
        // bump `last_scan` forward so that on un-pause the next
        // tick fires immediately instead of having to "catch up"
        // through a backlog of missed intervals.
        if app.paused {
            app.last_scan = Instant::now();
        } else if app.last_scan.elapsed() >= app.refresh {
            app.tick(run_dir);
        }
    }
}

/// Wait up to `timeout` for a key *press* event. `KeyEventKind::Release`
/// and `Repeat` are filtered out — kitty / Windows-style terminals emit
/// both a Press and a Release per stroke, which would otherwise double
/// every action (e.g. two `Tab`s flipping the view back to the original).
fn poll_key(timeout: Duration) -> io::Result<Option<crossterm::event::KeyEvent>> {
    if !event::poll(timeout)? {
        return Ok(None);
    }
    match event::read()? {
        Event::Key(k) if k.kind == KeyEventKind::Press => Ok(Some(k)),
        _ => Ok(None),
    }
}

/// Returns `true` if the loop should exit.
fn handle_key(app: &mut App, k: crossterm::event::KeyEvent, _run_dir: &Path) -> bool {
    // Quit shortcuts apply in every mode.
    if matches!(k.code, KeyCode::Char('q')) && matches!(app.input, InputMode::Normal) {
        return true;
    }
    if matches!(k.code, KeyCode::Char('c')) && k.modifiers.contains(KeyModifiers::CONTROL) {
        return true;
    }

    match app.input.clone() {
        InputMode::Filter => handle_filter_key(app, k),
        InputMode::Confirm(sig) => handle_confirm_key(app, k, sig),
        InputMode::Help => handle_help_key(app, k),
        InputMode::Normal => handle_normal_key(app, k),
    }
    false
}

fn handle_help_key(app: &mut App, k: crossterm::event::KeyEvent) {
    // Any of `?` / `Esc` / `q` dismisses. Other keys are swallowed so
    // they don't accidentally drive the table behind the popup.
    if matches!(
        k.code,
        KeyCode::Esc | KeyCode::Char('?' | 'q') | KeyCode::Enter
    ) {
        app.input = InputMode::Normal;
    }
}

fn handle_normal_key(app: &mut App, k: crossterm::event::KeyEvent) {
    match k.code {
        KeyCode::Tab => {
            app.view = match app.view {
                View::Vms => View::Procs,
                View::Procs => View::Vms,
            };
        }
        KeyCode::Char('?') => {
            app.input = InputMode::Help;
        }
        KeyCode::Char(' ') => {
            // Pause / resume the live tick. Useful when CPU% sort
            // is reshuffling rows faster than you can read them —
            // hit space, take your time, hit space again.
            app.paused = !app.paused;
        }
        KeyCode::Char('+' | '=') => {
            // `=` so users on US layouts don't have to chord shift.
            // Halve the tick (clamped) — `cur / 2`, not subtraction,
            // because perceived speed is logarithmic.
            app.refresh = (app.refresh / 2).max(MIN_REFRESH);
        }
        KeyCode::Char('-' | '_') => {
            app.refresh = (app.refresh.saturating_mul(2)).min(MAX_REFRESH);
        }
        KeyCode::Char('j') | KeyCode::Down => move_selection(app, 1),
        // Ctrl-k is SIGKILL in Procs view; check that *before* the bare
        // `k` nav binding, otherwise the latter eats every Char('k') event.
        KeyCode::Char('k')
            if app.view == View::Procs && k.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            if app.selected_proc().is_some() {
                app.input = InputMode::Confirm(KillSig::Kill);
            }
        }
        KeyCode::Char('k') | KeyCode::Up => move_selection(app, -1),
        KeyCode::PageDown => move_selection(app, 10),
        KeyCode::PageUp => move_selection(app, -10),
        KeyCode::Char('r') => {
            // Force an immediate scan on the next loop iteration.
            app.last_scan = Instant::now()
                .checked_sub(app.refresh)
                .unwrap_or_else(Instant::now);
        }
        KeyCode::Char('x') if app.view == View::Vms => delete_halted_state(app),
        KeyCode::Char('s') if app.view == View::Procs => {
            let pinned = app.selected_proc().map(|r| r.pid);
            app.procs_sort = app.procs_sort.next();
            app.recompute_procs();
            app.reanchor_proc_selection(pinned);
            app.clamp_selections();
        }
        KeyCode::Char('t') if app.view == View::Procs => {
            // Tree toggle. Re-anchor by pid so the cursor stays on
            // the same process after the row order changes.
            let pinned = app.selected_proc().map(|r| r.pid);
            app.tree_mode = !app.tree_mode;
            app.recompute_procs();
            app.reanchor_proc_selection(pinned);
            app.clamp_selections();
        }
        KeyCode::Char('/') if app.view == View::Procs => {
            app.input = InputMode::Filter;
        }
        KeyCode::Char('K') if app.view == View::Procs => {
            // Shift+k. Capital K is SIGTERM by default; Ctrl+k is SIGKILL (handled above).
            if app.selected_proc().is_some() {
                app.input = InputMode::Confirm(KillSig::Term);
            }
        }
        _ => {}
    }
}

fn handle_filter_key(app: &mut App, k: crossterm::event::KeyEvent) {
    match k.code {
        KeyCode::Esc => {
            let pinned = app.selected_proc().map(|r| r.pid);
            app.procs_filter.clear();
            app.input = InputMode::Normal;
            app.recompute_procs();
            app.reanchor_proc_selection(pinned);
            app.clamp_selections();
        }
        KeyCode::Enter => {
            app.input = InputMode::Normal;
        }
        KeyCode::Backspace => {
            let pinned = app.selected_proc().map(|r| r.pid);
            app.procs_filter.pop();
            app.recompute_procs();
            app.reanchor_proc_selection(pinned);
            app.clamp_selections();
        }
        KeyCode::Char(c) => {
            // Skip the modifier-bearing combos that crossterm still
            // surfaces as `Char`s — Ctrl+C was already handled upstream.
            if !k.modifiers.contains(KeyModifiers::CONTROL)
                && !k.modifiers.contains(KeyModifiers::ALT)
            {
                let pinned = app.selected_proc().map(|r| r.pid);
                app.procs_filter.push(c);
                app.recompute_procs();
                app.reanchor_proc_selection(pinned);
                app.clamp_selections();
            }
        }
        _ => {}
    }
}

fn handle_confirm_key(app: &mut App, k: crossterm::event::KeyEvent, sig: KillSig) {
    match k.code {
        KeyCode::Char('y' | 'Y') => {
            if let Some(row) = app.selected_proc() {
                let pid = row.pid;
                if let Some(p) = rustix::process::Pid::from_raw(pid) {
                    let _ = rustix::process::kill_process(p, sig.signal());
                }
            }
            app.input = InputMode::Normal;
        }
        _ => {
            app.input = InputMode::Normal;
        }
    }
}

fn move_selection(app: &mut App, delta: i32) {
    let (state, len) = match app.view {
        View::Vms => (&mut app.vms_table, app.vms.len()),
        View::Procs => (&mut app.procs_table, app.procs_visible.len()),
    };
    if len == 0 {
        return;
    }
    let cur = i64::try_from(state.selected().unwrap_or(0)).unwrap_or(0);
    let max = i64::try_from(len.saturating_sub(1)).unwrap_or(0);
    let next = (cur + i64::from(delta)).clamp(0, max);
    let next_us = usize::try_from(next).unwrap_or(0);
    state.select(Some(next_us));
}

fn delete_halted_state(app: &mut App) {
    let Some(i) = app.vms_table.selected() else {
        return;
    };
    let Some(row) = app.vms.get(i) else { return };
    if row.state.phase == "halted" || row.state.phase == "shutdown" || row.state.phase == "error" {
        let _ = fs::remove_file(&row.path);
        if let Some(parent) = row.path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
}

// -----------------------------------------------------------------------------
// Rendering
// -----------------------------------------------------------------------------

fn draw(f: &mut ratatui::Frame<'_>, run_dir: &Path, app: &mut App) {
    match app.view {
        View::Vms => draw_vms(f, run_dir, app),
        View::Procs => draw_procs(f, app),
    }
    // Modal overlays are painted *after* the per-view draw so they
    // sit on top of whichever table is current.
    if matches!(app.input, InputMode::Help) {
        draw_help_overlay(f, &app.host_info);
    }
}

/// Centered keybindings popup. Toggled by `?`; dismissed by
/// `?` / `Esc` / `q` / `Enter`. The Clear widget blanks out the
/// rectangle first so the popup isn't see-through.
///
/// Also carries the "about this machine" block (kernel + CPU model)
/// that used to live on line 2 of the host overview — that info is
/// static and doesn't earn a line of the always-visible header.
fn draw_help_overlay(f: &mut ratatui::Frame<'_>, h: &host::HostInfo) {
    let area = centered_rect(64, 28, f.area());
    f.render_widget(Clear, area);

    let dim = Style::default().fg(Color::DarkGray);
    let kb = Style::default().fg(Color::Black).bg(Color::Yellow);
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  System",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled("  kernel  ", dim),
            Span::raw(h.kernel.clone()),
        ]),
        Line::from(vec![
            Span::styled("  cpu     ", dim),
            Span::raw(h.cpu_model.clone()),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  Global",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        kv_line("  q / Ctrl-C", "quit", kb, dim),
        kv_line("  Tab", "toggle Vms / Procs view", kb, dim),
        kv_line("  ? ", "toggle this help", kb, dim),
        kv_line("  r ", "force an immediate refresh", kb, dim),
        kv_line("  + / -", "speed up / slow down the refresh tick", kb, dim),
        kv_line("  space", "pause / resume the live tick", kb, dim),
        kv_line("  j / k", "move selection (also ↓/↑)", kb, dim),
        kv_line("  PgDn / PgUp", "jump 10 rows", kb, dim),
        Line::from(""),
        Line::from(Span::styled(
            "  Vms view",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        kv_line("  x ", "delete state.json for selected halted vm", kb, dim),
        Line::from(""),
        Line::from(Span::styled(
            "  Procs view",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        kv_line("  s ", "cycle sort: CPU → MEM → PID → CMD", kb, dim),
        kv_line(
            "  t ",
            "toggle tree view (parent → children; sort + filter still apply)",
            kb,
            dim,
        ),
        kv_line("  / ", "filter by substring (Esc clears)", kb, dim),
        kv_line("  K ", "send SIGTERM to selected pid (confirm)", kb, dim),
        kv_line(
            "  Ctrl-K",
            "send SIGKILL to selected pid (confirm)",
            kb,
            dim,
        ),
        Line::from(""),
        Line::from(Span::styled(
            "  press ? / Esc / q / Enter to close",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )),
    ];

    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " neotop · keybindings ",
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn kv_line(key: &str, desc: &str, kb_style: Style, dim: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {key} "), kb_style),
        Span::raw("  "),
        Span::styled(desc.to_string(), dim),
    ])
}

/// Compute a rect that's `pct_x` % wide and `pct_y` % tall, centered
/// inside `area`. Standard ratatui popup pattern.
fn centered_rect(pct_x: u16, pct_y: u16, area: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

fn draw_vms(f: &mut ratatui::Frame<'_>, run_dir: &Path, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // title
            Constraint::Length(3),  // host overview (summary, net+temp, disk)
            Constraint::Min(5),     // fleet table
            Constraint::Length(16), // serial + resources pane
            Constraint::Length(1),  // help
        ])
        .split(area);

    // Bottom pane splits horizontally: serial tail (flex) | resources (46 cols).
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(30), Constraint::Length(46)])
        .split(chunks[3]);

    let selected = app.vms.get(app.vms_table.selected().unwrap_or(0));

    draw_title(f, chunks[0], run_dir, app.vms.len(), app.view, app.paused);
    draw_host(
        f,
        chunks[1],
        &app.host_info,
        &app.ifaces,
        &app.temps,
        &app.batteries,
        &app.disks,
    );
    if app.vms.is_empty() {
        draw_vms_empty(f, chunks[2], run_dir);
    } else {
        draw_table(f, chunks[2], &app.vms, &mut app.vms_table);
    }
    draw_serial(f, bottom[0], selected);
    draw_resources(f, bottom[1], selected, &app.history);
    draw_footer(f, chunks[4], app);
}

/// Empty-state for the Vms table. Replaces the otherwise-empty
/// `Table` widget with a paragraph that tells the user *why* there's
/// nothing to see and points them at the Procs view.
fn draw_vms_empty(f: &mut ratatui::Frame<'_>, area: Rect, run_dir: &Path) {
    let exists = run_dir.is_dir();
    let title = if exists {
        " fleet · empty "
    } else {
        " fleet · no state dir "
    };
    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                if exists {
                    "No VMs are running yet."
                } else {
                    "No neosandbox state directory found."
                },
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw("  watching "),
            Span::styled(
                run_dir.display().to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  Press "),
            Span::styled(
                " Tab ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" to view host processes (sorted by CPU%)."),
        ]),
        Line::from(vec![
            Span::raw("  Start a VM via "),
            Span::styled(
                "just demo-pvh",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::ITALIC),
            ),
            Span::raw(" and it will appear here automatically."),
        ]),
    ];
    let block = Block::default().borders(Borders::ALL).title(title);
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_procs(f: &mut ratatui::Frame<'_>, app: &mut App) {
    let area = f.area();
    let percore_h = percore_height(app.host_info.per_core_pct.len(), area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),         // title
            Constraint::Length(3),         // host overview (summary, net+temp, disk)
            Constraint::Length(percore_h), // per-core CPU grid (0..=2)
            Constraint::Length(3),         // CPU + MEM + NET\u{2193} + NET\u{2191} sparklines
            Constraint::Min(5),            // procs table + detail pane (split horiz)
            Constraint::Length(1),         // help / prompt
        ])
        .split(area);

    draw_title_procs(f, chunks[0], app);
    draw_host(
        f,
        chunks[1],
        &app.host_info,
        &app.ifaces,
        &app.temps,
        &app.batteries,
        &app.disks,
    );
    if percore_h > 0 {
        draw_per_core(f, chunks[2], &app.host_info.per_core_pct);
    }
    draw_host_history(f, chunks[3], &app.host_history);

    // Allocate the detail pane only when the terminal is wide enough.
    // Below ~110 cols the table needs every column to stay readable.
    let body = chunks[4];
    if body.width >= 110 {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(60), Constraint::Length(48)])
            .split(body);
        let selected_pid = app.selected_proc().map(|r| i64::from(r.pid));
        draw_proc_table(f, split[0], app);
        draw_proc_detail(f, split[1], selected_pid, app.selected_proc());
    } else {
        draw_proc_table(f, body, app);
    }
    draw_footer(f, chunks[5], app);
}

/// Per-core row width = `c{nn} {bar} {pct}%  ` ≈ 11 columns. The grid
/// auto-flows that many cells per row and caps at 2 rows so the
/// procs body never gets squeezed into nothing on a 24-row terminal.
const PERCORE_CELL_W: u16 = 11;
const PERCORE_MAX_ROWS: u16 = 2;

fn percore_height(num_cores: usize, width: u16) -> u16 {
    if num_cores == 0 {
        return 0;
    }
    let per_row = (width / PERCORE_CELL_W).max(1) as usize;
    let rows = num_cores.div_ceil(per_row);
    u16::try_from(rows.min(PERCORE_MAX_ROWS as usize)).unwrap_or(PERCORE_MAX_ROWS)
}

fn draw_per_core(f: &mut ratatui::Frame<'_>, area: Rect, percore: &[f64]) {
    let per_row = (area.width / PERCORE_CELL_W).max(1) as usize;
    let max_cells = per_row.saturating_mul(PERCORE_MAX_ROWS as usize);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();

    for (i, &pct) in percore.iter().take(max_cells).enumerate() {
        let bar = bar_glyph(pct);
        let color = cpu_glyph_color(pct);
        spans.push(Span::styled(
            format!(" c{i:<2} "),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(bar.to_string(), Style::default().fg(color)));
        spans.push(Span::styled(
            format!(" {pct:>3.0}% "),
            Style::default().fg(color),
        ));
        if (i + 1) % per_row == 0 {
            lines.push(Line::from(std::mem::take(&mut spans)));
        }
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Live detail view for the process under the cursor. Reuses
/// `proc::snapshot(pid)` — the same code path the VM resources pane
/// uses — so we get cgroup-v2 path + memory.current/max + rlimits
/// for free.
fn draw_proc_detail(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    pid: Option<i64>,
    row: Option<&procs::ProcessRow>,
) {
    let block = Block::default().borders(Borders::ALL).title(" detail ");
    let Some(pid) = pid else {
        f.render_widget(Paragraph::new("(no process selected)").block(block), area);
        return;
    };
    let label = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Line<'static>> = Vec::new();

    if let Some(r) = row {
        let cpu = r
            .cpu_pct
            .map_or_else(|| "—".to_string(), |p| format!("{p:.1}%"));
        lines.push(kv("PID", pid.to_string(), label));
        lines.push(kv("PPID", r.ppid.to_string(), label));
        lines.push(kv("USER", r.user.clone(), label));
        lines.push(kv("STATE", r.state.to_string(), label));
        lines.push(kv("CPU%", cpu, label));
        lines.push(kv("THREADS", r.threads.to_string(), label));
        lines.push(kv("RSS", proc::human_bytes(r.rss_bytes), label));
    }

    // Pull live cgroup + rlimits via the same snapshot used for VMs.
    if let Some(snap) = proc::snapshot(pid) {
        lines.push(kv("VSZ", proc::human_bytes(snap.mem.vsz_bytes), label));
        if let Some(cg) = &snap.cgroup {
            lines.push(section("── cgroup ──"));
            lines.push(kv("path", ellipsize(&cg.path, 38), label));
            lines.push(kv("mem.cur", proc::human_bytes(cg.memory_current), label));
            let max = if cg.memory_max == u64::MAX {
                "∞".to_string()
            } else {
                proc::human_bytes(cg.memory_max)
            };
            lines.push(kv("mem.max", max, label));
        }
        let want = ["Max open files", "Max processes", "Max address space"];
        let mut header_pushed = false;
        for w in want {
            if let Some(l) = snap.limits.iter().find(|l| l.name == w) {
                if !header_pushed {
                    lines.push(section("── rlimits ──"));
                    header_pushed = true;
                }
                let soft = proc::format_limit_value(&l.soft, &l.unit);
                let hard = proc::format_limit_value(&l.hard, &l.unit);
                let value = if soft == hard {
                    soft
                } else {
                    format!("{soft} / {hard}")
                };
                lines.push(kv(short_limit_name(&l.name), value, label));
            }
        }
    }

    if let Some(r) = row {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  command",
            label.add_modifier(Modifier::BOLD),
        )));
        // Wrap long command lines so the user can actually read them.
        for chunk in wrap_chars(&r.command, area.width.saturating_sub(4) as usize) {
            lines.push(Line::from(Span::raw(format!("  {chunk}"))));
        }
    }

    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Char-boundary-safe line wrap. We don't try to break on word
/// boundaries — process command lines are full of paths and flags
/// where word breaks are arbitrary anyway.
fn wrap_chars(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![s.to_string()];
    }
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut count = 0;
    for c in s.chars() {
        if count == width {
            out.push(std::mem::take(&mut buf));
            count = 0;
        }
        buf.push(c);
        count += 1;
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// Four side-by-side host-level sparklines — CPU%, MEM%, NET↓, NET↑.
/// 60 samples each → last minute at the default 1 s tick. CPU/MEM
/// share a 0-100 ceiling; the two net sparklines auto-scale to the
/// rolling max in their window so a 30 KB/s burst is still visible
/// next to a 200 MB/s spike on a different interface earlier.
fn draw_host_history(f: &mut ratatui::Frame<'_>, area: Rect, h: &HostHistory) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
        ])
        .split(area);

    let cpu_data: Vec<u64> = h.cpu.iter().copied().collect();
    let mem_data: Vec<u64> = h.mem.iter().copied().collect();
    let down_data: Vec<u64> = h.net_down.iter().copied().collect();
    let up_data: Vec<u64> = h.net_up.iter().copied().collect();

    // Newest sample = label value; the user thinks "what's it doing
    // *right now*", not "what's the average over the last 15 s".
    let cpu_now = cpu_data.last().copied().unwrap_or(0);
    let mem_now = mem_data.last().copied().unwrap_or(0);
    let down_now = down_data.last().copied().unwrap_or(0);
    let up_now = up_data.last().copied().unwrap_or(0);

    let cpu_title = format!(" CPU {cpu_now}% ");
    let mem_title = format!(" MEM {mem_now}% ");
    let down_title = format!(" NET\u{2193} {} ", net::human_rate(Some(down_now)));
    let up_title = format!(" NET\u{2191} {} ", net::human_rate(Some(up_now)));

    f.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(cpu_title))
            .data(&cpu_data)
            .max(100)
            .style(Style::default().fg(Color::Green)),
        cols[0],
    );
    f.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(mem_title))
            .data(&mem_data)
            .max(100)
            .style(Style::default().fg(Color::Magenta)),
        cols[1],
    );
    // Auto-scale net sparklines to the rolling max so small bursts
    // are still visible. Floor at 1 KB/s so a totally idle window
    // doesn't draw a wall of full bars.
    let down_max = down_data.iter().copied().max().unwrap_or(0).max(1024);
    let up_max = up_data.iter().copied().max().unwrap_or(0).max(1024);
    f.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(down_title))
            .data(&down_data)
            .max(down_max)
            .style(Style::default().fg(Color::Cyan)),
        cols[2],
    );
    f.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(up_title))
            .data(&up_data)
            .max(up_max)
            .style(Style::default().fg(Color::Yellow)),
        cols[3],
    );
}

/// Sum the per-interface RX/TX rates currently visible in the host
/// overview into a single (down, up) pair for the sparklines. None
/// rates (first sample after startup) count as zero — that produces
/// one frame of "no signal" on the chart, which is honest.
fn total_net_rates(ifaces: &[net::Iface]) -> (u64, u64) {
    let mut down = 0u64;
    let mut up = 0u64;
    for i in ifaces {
        down = down.saturating_add(i.rx_rate.unwrap_or(0));
        up = up.saturating_add(i.tx_rate.unwrap_or(0));
    }
    (down, up)
}

fn draw_title(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    run_dir: &Path,
    count: usize,
    view: View,
    paused: bool,
) {
    let view_label = match view {
        View::Vms => " VMs ",
        View::Procs => " Procs ",
    };
    let mut spans = vec![
        Span::styled(
            " neosandbox top ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            view_label,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  watching {} — {count} VM(s)", run_dir.display())),
    ];
    if paused {
        spans.push(paused_badge());
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Inverse-video badge appended to whichever title is active when the
/// user has frozen the live tick. Bright enough that you can't miss
/// it; deliberately *not* a popup, because pausing should leave the
/// table fully readable.
fn paused_badge() -> Span<'static> {
    Span::styled(
        "  [PAUSED — space to resume] ",
        Style::default()
            .fg(Color::Black)
            .bg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    )
}

fn draw_title_procs(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let total = app.procs_all.len();
    let visible = app.procs_visible.len();
    let mut title = vec![
        Span::styled(
            " neosandbox top ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            " Procs ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {visible}/{total} processes · sort {}{}",
            app.procs_sort.label(),
            app.procs_sort.arrow(),
        )),
    ];
    if app.paused {
        title.push(paused_badge());
    }
    f.render_widget(Paragraph::new(Line::from(title)), area);
}

fn draw_host(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    h: &host::HostInfo,
    ifaces: &[net::Iface],
    temps: &[temp::Reading],
    batteries: &[battery::Battery],
    disks: &[disk::Disk],
) {
    // Three tighter lines instead of four:
    //   1. kvm + CPU% + MEM + load + battery (the "tell me now" line)
    //   2. net + temps (moving-parts line)
    //   3. disks
    // The kernel/CPU-model line was static information that didn't
    // earn its row — it now lives in the `?` overlay. The inline
    // per-core bar strip was also removed from line 1 because the
    // Procs view has a dedicated per-core panel; the Vms view gets
    // per-core detail from its right-hand resources pane anyway.
    let line1 = host_line1(h, batteries);
    let line2 = host_line_net_temp(ifaces, temps);
    let line3 = host_line4(disks);
    f.render_widget(Paragraph::new(vec![line1, line2, line3]), area);
}

fn host_line1(h: &host::HostInfo, batteries: &[battery::Battery]) -> Line<'static> {
    let kvm = if h.kvm_available {
        Span::styled(
            " kvm:ok ",
            Style::default().fg(Color::Black).bg(Color::Green),
        )
    } else {
        Span::styled(
            " kvm:MISSING ",
            Style::default().fg(Color::White).bg(Color::Red),
        )
    };
    let cpu_pct = h
        .cpu_pct
        .map_or_else(|| "—".to_string(), |p| format!("{p:>4.1}%"));
    let mem_used = h.mem_total_bytes.saturating_sub(h.mem_avail_bytes);
    let mem_pct = mem_used_pct(h);

    let mut spans: Vec<Span<'static>> = vec![
        Span::raw(" "),
        kvm,
        Span::raw("  "),
        Span::styled("CPU", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(" {cpu_pct}  ")),
        Span::styled("MEM", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(
            " {}/{} ({mem_pct:>4.1}%)  ",
            proc::human_bytes(mem_used),
            proc::human_bytes(h.mem_total_bytes),
        )),
    ];

    // Swap is only worth the screen real estate when the box has
    // some configured. Microvms and most cloud servers don't, and
    // showing "swap 0/0 (0%)" is just noise. When swap *is*
    // present, color the percentage red once it's non-trivial —
    // the system swapping out memory is one of the strongest
    // "something is wrong" signals there is.
    if h.swap_total_bytes > 0 {
        let swap_used = h.swap_total_bytes.saturating_sub(h.swap_free_bytes);
        #[allow(clippy::cast_precision_loss)]
        let swap_pct = (swap_used as f64 / h.swap_total_bytes as f64) * 100.0;
        let swap_color = if swap_pct >= 50.0 {
            Color::Red
        } else if swap_pct >= 10.0 {
            Color::Yellow
        } else {
            Color::Reset
        };
        spans.push(Span::styled("swap", Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw(format!(
            " {}/{} (",
            proc::human_bytes(swap_used),
            proc::human_bytes(h.swap_total_bytes),
        )));
        spans.push(Span::styled(
            format!("{swap_pct:>4.1}%"),
            Style::default().fg(swap_color),
        ));
        spans.push(Span::raw(")  "));
    }

    // All three load-average windows. The triplet is what tells you
    // whether you're looking at a fresh fire (1m high, 5m and 15m
    // low) or a sustained one (all three high). Showing only the
    // 1-minute number was hiding half the signal.
    spans.extend([
        Span::styled("load", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(
            " {:.2} {:.2} {:.2}",
            h.loadavg_1, h.loadavg_5, h.loadavg_15,
        )),
    ]);
    if !batteries.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled("bat", Style::default().fg(Color::DarkGray)));
        for b in batteries {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("{}%", b.percent),
                Style::default().fg(battery_color(b)),
            ));
            spans.push(Span::raw(format!(" {}", short_bat_status(&b.status))));
            if let Some(w) = b.watts {
                if w.abs() >= 0.1 {
                    spans.push(Span::raw(format!(" {:.1}W", w.abs())));
                }
            }
        }
    }
    Line::from(spans)
}

fn host_line_net_temp(ifaces: &[net::Iface], temps: &[temp::Reading]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> =
        vec![Span::styled(" net ", Style::default().fg(Color::DarkGray))];
    if ifaces.is_empty() {
        spans.push(Span::raw("—"));
    } else {
        for (i, iface) in ifaces.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                iface.name.clone(),
                Style::default().fg(Color::Cyan),
            ));
            spans.push(Span::raw(format!(
                " ↓{} ↑{}",
                net::human_rate(iface.rx_rate),
                net::human_rate(iface.tx_rate),
            )));
        }
    }
    spans.push(Span::raw("   "));
    spans.push(Span::styled("temp ", Style::default().fg(Color::DarkGray)));

    // Prefer "informative" sensors (CPU / GPU / NVMe / battery) and
    // drop noisy chipset / ACPI readings unless they're actually hot.
    // Without this filter the overview would surface labels like
    // `pch_cannonlake#1  30°C` that don't help anyone.
    let filtered: Vec<temp::Reading> = temps
        .iter()
        .filter(|r| is_informative_temp(r))
        .cloned()
        .collect();
    let picks = temp::highlights(&filtered, 3);
    if picks.is_empty() {
        spans.push(Span::raw("—"));
    } else {
        for (i, r) in picks.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            let color = match temp::severity(r.celsius) {
                temp::Severity::Cool => Color::Green,
                temp::Severity::Warm => Color::Yellow,
                temp::Severity::Hot => Color::Red,
            };
            spans.push(Span::styled(
                compact_temp_label(&r.label).to_string(),
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("{:>4.1}°C", r.celsius),
                Style::default().fg(color),
            ));
        }
    }
    Line::from(spans)
}

fn host_line4(disks: &[disk::Disk]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> =
        vec![Span::styled(" disk ", Style::default().fg(Color::DarkGray))];
    let picks = disk::highlights(disks, 3);
    if picks.is_empty() {
        spans.push(Span::raw("—"));
        return Line::from(spans);
    }
    for (i, d) in picks.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            d.name.clone(),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::raw(format!(
            " ↓{} ↑{}",
            disk::human_rate(d.read_bps),
            disk::human_rate(d.write_bps),
        )));
        if let Some(util) = d.util_pct {
            // Highlight saturated devices — same yellow/red thresholds
            // we use for CPU% to keep the eye-trained palette consistent.
            let color = if util >= 80.0 {
                Color::Red
            } else if util >= 50.0 {
                Color::Yellow
            } else {
                Color::DarkGray
            };
            spans.push(Span::styled(
                format!(" {util:>3.0}%"),
                Style::default().fg(color),
            ));
        }
    }
    Line::from(spans)
}

fn bar_glyph(pct: f64) -> char {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let idx = (pct.clamp(0.0, 100.0) / 100.0 * (BARS.len() as f64 - 0.001)) as usize;
    BARS[idx.min(BARS.len() - 1)]
}

fn cpu_glyph_color(pct: f64) -> Color {
    if pct >= 80.0 {
        Color::Red
    } else if pct >= 50.0 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn battery_color(b: &battery::Battery) -> Color {
    if b.status == "Charging" || b.status == "Full" {
        Color::Green
    } else if b.percent < 15 {
        Color::Red
    } else if b.percent < 35 {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn short_bat_status(s: &str) -> &'static str {
    match s {
        "Charging" => "chg",
        "Discharging" => "dsch",
        "Full" => "full",
        "Not charging" => "idle",
        _ => "?",
    }
}

/// Map a raw hwmon label (e.g. `coretemp Package id 0`, `nvme Composite`,
/// `pch_cannonlake#1`, `k10temp`) to a short, human-friendly tag that
/// fits in the one-line host overview. Falls back to the first word
/// with any trailing `#N` sensor-index suffix stripped so we never
/// surface raw kernel names like `pch_cannonlake#1` to the user.
fn compact_temp_label(label: &str) -> &'static str {
    // Order matters: more-specific strings like "coretemp Package"
    // are checked before their broader "coretemp" siblings.
    let lower = label.to_ascii_lowercase();
    let first = lower
        .split(['#', ' ', '\t'])
        .next()
        .unwrap_or(lower.as_str());
    match first {
        // CPU packages / cores.
        "coretemp" if lower.contains("package") => "cpu pkg",
        "coretemp" | "k10temp" | "zenpower" => "cpu",
        // Discrete + integrated GPUs.
        "amdgpu" | "nouveau" | "i915" | "xe" | "radeon" | "nvidia" => "gpu",
        // Storage.
        "nvme" => "nvme",
        "drivetemp" => "disk",
        // Wireless + ACPI + chipsets (these are usually noise, but
        // better to label them than show `pch_cannonlake#1`).
        "iwlwifi" | "iwlwifi_1" => "wifi",
        "acpitz" => "acpi",
        s if s.starts_with("pch_") => "pch",
        // Firmware-exposed thermal zones and random sensors.
        s if s.starts_with("thermal") => "zone",
        s if s.starts_with("bat") => "bat",
        s if s.starts_with("tctl") => "cpu",
        // Last resort: return a short fallback. We deliberately avoid
        // leaking the raw `first` word so the overview stays clean
        // even on exotic hardware \u2014 the user can always see the full
        // sensor name in future detailed-view work; here they just
        // get `sensor`.
        _ => "sensor",
    }
}

/// Hide temperature readings that won't help the user. PCH, ACPI,
/// wifi, and the fallback `sensor` bucket are dropped unless they're
/// actually *hot* (warm or hot severity) \u2014 at which point the user
/// probably does want to know. CPU, GPU, `NVMe`, and battery are
/// always surfaced.
fn is_informative_temp(r: &temp::Reading) -> bool {
    match compact_temp_label(&r.label) {
        "cpu pkg" | "cpu" | "gpu" | "nvme" | "disk" | "bat" => true,
        _ => !matches!(temp::severity(r.celsius), temp::Severity::Cool),
    }
}

/// How long an error stays in the footer after it was last pushed.
const ERROR_TTL: Duration = Duration::from_secs(5);

/// Bottom row: help/prompt on the left, optional error badge in the
/// middle, perf metrics right-aligned. We allocate fixed widths to
/// the right-hand widgets; the help block gets whatever's left.
/// Below ~80 cols total the perf block is dropped — the help text is
/// more important than self-stats.
fn draw_footer(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    const PERF_W: u16 = 44;
    let err_entry = app.errors.latest_within(ERROR_TTL);
    let err_text = err_entry.map(|e| {
        format!(
            " \u{26a0} {}: {} ({} err) ",
            e.source,
            e.message,
            app.errors.total()
        )
    });
    let err_w = err_text
        .as_deref()
        .map_or(0, |s| u16::try_from(s.chars().count()).unwrap_or(0));

    if area.width <= PERF_W + err_w + 8 {
        draw_help(f, area, app);
        return;
    }

    let mut constraints: Vec<Constraint> = vec![Constraint::Min(20)];
    if err_w > 0 {
        constraints.push(Constraint::Length(err_w));
    }
    constraints.push(Constraint::Length(PERF_W));

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);
    draw_help(f, chunks[0], app);
    let mut idx = 1;
    if let Some(text) = err_text {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                text,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ))),
            chunks[idx],
        );
        idx += 1;
    }
    draw_perf(f, chunks[idx], app);
}

fn draw_perf(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let p = &app.perf.perf;
    let scan_color = ms_color(p.scan_ms);
    let render_color = ms_color(p.render_ms);
    let cpu = p
        .own_cpu_pct
        .map_or_else(|| "—".to_string(), |v| format!("{v:.1}%"));
    let line = Line::from(vec![
        Span::styled("scan ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{:.1}ms", p.scan_ms),
            Style::default().fg(scan_color),
        ),
        Span::raw(" "),
        Span::styled("render ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{:.1}ms", p.render_ms),
            Style::default().fg(render_color),
        ),
        Span::raw(" "),
        Span::styled("own ", Style::default().fg(Color::DarkGray)),
        Span::raw(proc::human_bytes(p.own_rss_bytes)),
        Span::raw(" "),
        Span::raw(cpu),
        Span::raw(" "),
        Span::styled("tick ", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(
            "{:.0}/{}ms",
            p.refresh_actual_ms,
            app.refresh.as_millis()
        )),
    ]);
    f.render_widget(Paragraph::new(line).alignment(Alignment::Right), area);
}

fn ms_color(ms: f64) -> Color {
    if ms >= 100.0 {
        Color::Red
    } else if ms >= 20.0 {
        Color::Yellow
    } else {
        Color::DarkGray
    }
}

fn draw_help(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    // Modal prompts take over the help bar entirely.
    match &app.input {
        InputMode::Filter => {
            let line = Line::from(vec![
                Span::styled(
                    " filter ",
                    Style::default().fg(Color::Black).bg(Color::Yellow),
                ),
                Span::raw(" "),
                Span::styled(
                    app.procs_filter.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
                Span::raw("   "),
                Span::styled(" Enter ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw(" apply   "),
                Span::styled(" Esc ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw(" clear"),
            ]);
            f.render_widget(Paragraph::new(line), area);
            return;
        }
        InputMode::Help => {
            // Help-mode prompt has nothing useful to add to the help
            // bar — the popup itself reminds the user how to dismiss.
            return;
        }
        InputMode::Confirm(sig) => {
            let target = app.selected_proc().map_or_else(
                || "(no selection)".to_string(),
                |r| format!("pid {} · {}", r.pid, r.command),
            );
            let line = Line::from(vec![
                Span::styled(
                    format!(" {} ", sig.label()),
                    Style::default().fg(Color::White).bg(Color::Red),
                ),
                Span::raw(format!(" {target}   ")),
                Span::styled(" y ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw(" confirm   "),
                Span::styled(" any ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw(" cancel"),
            ]);
            f.render_widget(Paragraph::new(line), area);
            return;
        }
        InputMode::Normal => {}
    }

    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(" q ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" quit  "),
        Span::styled(" ? ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" help  "),
        Span::styled(" Tab ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" view  "),
        Span::styled(" j/k ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" nav  "),
        Span::styled(" r ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" refresh  "),
    ];
    match app.view {
        View::Vms => {
            spans.extend([
                Span::styled(" x ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw(" delete halted"),
            ]);
        }
        View::Procs => {
            spans.extend([
                Span::styled(" s ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw(" sort  "),
                Span::styled(" t ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw(" tree  "),
                Span::styled(" / ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw(" filter  "),
                Span::styled(" K ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw(" SIGTERM  "),
                Span::styled(" ^K ", Style::default().fg(Color::Black).bg(Color::Gray)),
                Span::raw(" SIGKILL"),
            ]);
            if !app.procs_filter.is_empty() {
                spans.push(Span::raw("    "));
                spans.push(Span::styled(
                    "filter:",
                    Style::default().fg(Color::DarkGray),
                ));
                spans.push(Span::styled(
                    format!(" {} ", app.procs_filter),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            }
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_proc_table(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let header = Row::new(vec!["PID", "USER", "S", "CPU%", "RSS", "THR", "COMMAND"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let body: Vec<Row> = app
        .procs_visible
        .iter()
        .filter_map(|pr| app.procs_all.get(pr.idx).map(|r| (pr, r)))
        .map(|(pr, r)| {
            let cpu = r
                .cpu_pct
                .map_or_else(|| "—".to_string(), |p| format!("{p:.1}"));
            let cpu_style = Style::default().fg(cpu_glyph_color(r.cpu_pct.unwrap_or(0.0)));
            let state_style = proc_state_style(r.state);
            // In tree mode the COMMAND cell is prefixed with the
            // glyph chain ('│ ├─', '└─', etc). In flat mode `prefix`
            // is empty and we render the same as before.
            let cmd = if pr.prefix.is_empty() {
                truncate_lossy(&r.command, 200)
            } else {
                format!("{} {}", pr.prefix, truncate_lossy(&r.command, 200))
            };
            Row::new(vec![
                Cell::from(r.pid.to_string()),
                Cell::from(truncate_lossy(&r.user, 10)),
                Cell::from(Span::styled(r.state.to_string(), state_style)),
                Cell::from(Span::styled(cpu, cpu_style)),
                Cell::from(proc::human_bytes(r.rss_bytes)),
                Cell::from(r.threads.to_string()),
                Cell::from(cmd),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(7),
        Constraint::Length(10),
        Constraint::Length(2),
        Constraint::Length(6),
        Constraint::Length(9),
        Constraint::Length(4),
        Constraint::Min(30),
    ];

    let title = if app.tree_mode {
        " processes · tree (sort/filter disabled — t to leave) ".to_string()
    } else {
        format!(
            " processes · by {}{}{} ",
            app.procs_sort.label(),
            app.procs_sort.arrow(),
            if app.procs_filter.is_empty() {
                String::new()
            } else {
                format!(" · /{}", app.procs_filter)
            },
        )
    };
    let table = Table::new(body, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title))
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

    f.render_stateful_widget(table, area, &mut app.procs_table);
    draw_scrollbar(
        f,
        area,
        app.procs_visible.len(),
        app.procs_table.selected().unwrap_or(0),
    );
}

fn proc_state_style(c: char) -> Style {
    match c {
        'R' => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        'D' => Style::default().fg(Color::Red),
        'Z' => Style::default().fg(Color::Magenta),
        'T' | 't' => Style::default().fg(Color::Yellow),
        'I' => Style::default().fg(Color::DarkGray),
        _ => Style::default().fg(Color::Gray),
    }
}

fn truncate_lossy(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

fn draw_table(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    rows: &[VmRow],
    table_state: &mut TableState,
) {
    let now = now_ns();
    let header = Row::new(vec![
        "PID",
        "PHASE",
        "MODE",
        "UPTIME",
        "CPU%",
        "RSS",
        "IO",
        "MMIO",
        "HLT",
        "SHDN",
        "LAST SERIAL",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let body: Vec<Row> = rows
        .iter()
        .map(|r| {
            let s = &r.state;
            let cpu = r
                .cpu_pct
                .map_or_else(|| "—".to_string(), |p| format!("{p:.1}"));
            let rss = r
                .proc
                .as_ref()
                .map_or_else(|| "—".to_string(), |p| proc::human_bytes(p.mem.rss_bytes));
            Row::new(vec![
                Cell::from(s.vm_id.clone()),
                Cell::from(Span::styled(s.phase.clone(), phase_style(&s.phase))),
                Cell::from(s.mode.clone()),
                Cell::from(format_uptime(s.started_at_ns, now)),
                Cell::from(cpu),
                Cell::from(rss),
                Cell::from(s.exits.io.to_string()),
                Cell::from(s.exits.mmio.to_string()),
                Cell::from(s.exits.hlt.to_string()),
                Cell::from(s.exits.shutdown.to_string()),
                Cell::from(one_line(&s.serial_tail)),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(6),
        Constraint::Length(9),
        Constraint::Length(6),
        Constraint::Length(6),
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Min(20),
    ];

    let table = Table::new(body, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title("fleet"))
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

    f.render_stateful_widget(table, area, table_state);
    draw_scrollbar(f, area, rows.len(), table_state.selected().unwrap_or(0));
}

/// Vertical scrollbar painted on the right edge of a bordered table.
/// Hides when the row count is small enough that the table doesn't
/// scroll — no point in a stub thumb that fills the whole track.
///
/// Drawn *after* the table so it overlays the right border. We use the
/// border row directly as the track so the table loses no inner width.
fn draw_scrollbar(f: &mut ratatui::Frame<'_>, area: Rect, total: usize, selected: usize) {
    // Subtract 2: one for the table header row, one for the bottom
    // border. The remaining height is roughly the visible row count.
    let visible_rows = area.height.saturating_sub(2) as usize;
    if total <= visible_rows.max(1) {
        return;
    }
    let mut state = ScrollbarState::new(total).position(selected);
    let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .track_symbol(None)
        .thumb_symbol("\u{2588}")
        .style(Style::default().fg(Color::DarkGray));
    // Inset by 1 so we don't clobber the corner glyphs of the block.
    let inner = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height.saturating_sub(2),
    };
    f.render_stateful_widget(bar, inner, &mut state);
}

fn draw_serial(f: &mut ratatui::Frame<'_>, area: Rect, selected: Option<&VmRow>) {
    let (title, body) = match selected {
        Some(r) => {
            let kp = r.state.kernel_path.as_deref().unwrap_or("(no kernel)");
            (
                format!(" serial tail — pid {} / {} ", r.state.pid, kp),
                r.state.serial_tail.clone(),
            )
        }
        None => (
            " serial tail ".to_string(),
            String::from("(no VM selected — run `just demo-pvh` with NEOSANDBOX_STATE set)"),
        ),
    };

    let p = Paragraph::new(body)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

/// Right-hand pane: live `/proc/<pid>/` stats, CPU% sparkline,
/// cgroup-v2 accounting, and a curated selection of rlimits.
fn draw_resources(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    selected: Option<&VmRow>,
    history: &CpuHistory,
) {
    let block = Block::default().borders(Borders::ALL).title(" resources ");

    let Some(row) = selected else {
        f.render_widget(Paragraph::new("—").block(block), area);
        return;
    };

    // Split the pane vertically: text stats at top, sparkline at bottom.
    let inner = block.inner(area);
    f.render_widget(block, area);

    let splits = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(5), Constraint::Length(3)])
        .split(inner);

    draw_resources_text(f, splits[0], row);
    draw_cpu_sparkline(f, splits[1], row, history);
}

fn draw_resources_text(f: &mut ratatui::Frame<'_>, area: Rect, row: &VmRow) {
    let label_style = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Line<'_>> = Vec::new();

    let cpu = row
        .cpu_pct
        .map_or_else(|| "—".to_string(), |p| format!("{p:.1}%"));
    lines.push(kv("PID", row.state.pid.to_string(), label_style));
    lines.push(kv("CPU%", cpu, label_style));

    if let Some(p) = &row.proc {
        lines.push(kv("State", p.stat.state.clone(), label_style));
        lines.push(kv("Threads", p.stat.num_threads.to_string(), label_style));
        lines.push(kv("RSS", proc::human_bytes(p.mem.rss_bytes), label_style));
        lines.push(kv("VSZ", proc::human_bytes(p.mem.vsz_bytes), label_style));

        if let Some(cg) = &p.cgroup {
            lines.push(section("── cgroup ──"));
            lines.push(kv("path", ellipsize(&cg.path, 30), label_style));
            lines.push(kv(
                "mem.cur",
                proc::human_bytes(cg.memory_current),
                label_style,
            ));
            let max = if cg.memory_max == u64::MAX {
                "∞".to_string()
            } else {
                proc::human_bytes(cg.memory_max)
            };
            lines.push(kv("mem.max", max, label_style));
        }

        // A curated subset of rlimits — the ones actually relevant when
        // a vmmd process is misbehaving.
        let relevant = [
            "Max open files",
            "Max processes",
            "Max address space",
            "Max locked memory",
            "Max core file size",
            "Max cpu time",
        ];
        if !p.limits.is_empty() {
            lines.push(section("── rlimits ──"));
            for want in relevant {
                if let Some(l) = p.limits.iter().find(|l| l.name == want) {
                    let short = short_limit_name(&l.name);
                    let soft = proc::format_limit_value(&l.soft, &l.unit);
                    let hard = proc::format_limit_value(&l.hard, &l.unit);
                    let value = if soft == hard {
                        soft
                    } else {
                        format!("{soft} / {hard}")
                    };
                    lines.push(kv(short, value, label_style));
                }
            }
        }
    } else {
        lines.push(kv("State", "(gone)".to_string(), label_style));
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn draw_cpu_sparkline(f: &mut ratatui::Frame<'_>, area: Rect, row: &VmRow, history: &CpuHistory) {
    let data: Vec<u64> = history
        .get(row.state.pid)
        .map(|dq| dq.iter().copied().collect())
        .unwrap_or_default();

    let title = format!(" CPU% · last {}s ", data.len() / 4);
    let sp = Sparkline::default()
        .block(Block::default().borders(Borders::TOP).title(title))
        .data(&data)
        .max(100)
        .style(Style::default().fg(Color::Green));
    f.render_widget(sp, area);
}

fn section(label: &'static str) -> Line<'static> {
    Line::from(Span::styled(label, Style::default().fg(Color::DarkGray)))
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // Keep the tail — for cgroup paths the last segment is the useful bit.
        let tail = &s[s.len() - (max - 1)..];
        format!("…{tail}")
    }
}

fn kv(key: &str, value: String, label_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {key:>9}: "), label_style),
        Span::raw(value),
    ])
}

fn short_limit_name(full: &str) -> &'static str {
    match full {
        "Max open files" => "nofile",
        "Max processes" => "nproc",
        "Max address space" => "AS",
        "Max locked memory" => "mlock",
        "Max core file size" => "core",
        "Max cpu time" => "cpu",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(self_pid: i32, parent: i32, cmd: &str) -> procs::ProcessRow {
        procs::ProcessRow {
            pid: self_pid,
            ppid: parent,
            uid: 0,
            user: "u".into(),
            state: 'S',
            cpu_pct: None,
            rss_bytes: 0,
            threads: 1,
            command: cmd.into(),
        }
    }

    #[test]
    fn tree_orders_parents_then_children_in_pid_order() {
        // init(1) ├─ shell(10) └─ ssh(20)
        // shell(10) ├─ vim(11)   └─ rg(12)
        // ssh(20)   └─ ssh-agent(21)
        let rows = vec![
            p(1, 0, "init"),
            p(10, 1, "shell"),
            p(20, 1, "ssh"),
            p(11, 10, "vim"),
            p(12, 10, "rg"),
            p(21, 20, "ssh-agent"),
        ];
        let rendered = compute_visible_tree(&rows, procs::SortBy::Pid, "");
        let pids: Vec<i32> = rendered.iter().map(|r| rows[r.idx].pid).collect();
        assert_eq!(pids, vec![1, 10, 11, 12, 20, 21]);

        // Root has no prefix; children get '├─' or '└─'.
        assert_eq!(rendered[0].prefix, "");
        // shell(10) is the first child of init, but init's last child
        // is ssh(20), so shell gets '├─'.
        assert_eq!(rendered[1].prefix, "├─");
        // vim(11) sits under shell, which is *not* the last sibling
        // — so we expect '│ ' carried over, then '├─' for vim.
        assert_eq!(rendered[2].prefix, "│ ├─");
        assert_eq!(rendered[3].prefix, "│ └─");
        // ssh(20) is the last sibling under init.
        assert_eq!(rendered[4].prefix, "└─");
        // ssh-agent(21) under ssh; ssh is the last sibling so the
        // ancestor segment is two spaces, and ssh-agent itself is the
        // only child so it gets '└─'.
        assert_eq!(rendered[5].prefix, "  └─");
    }

    #[test]
    fn tree_handles_orphans_as_roots() {
        // Parent pid 999 doesn't exist in the row set, so child(50)
        // is treated as a root.
        let rows = vec![p(50, 999, "orphan"), p(1, 0, "init")];
        let rendered = compute_visible_tree(&rows, procs::SortBy::Pid, "");
        let pids: Vec<i32> = rendered.iter().map(|r| rows[r.idx].pid).collect();
        // Both are roots; sorted by pid → init first.
        assert_eq!(pids, vec![1, 50]);
    }

    #[test]
    fn tree_filter_keeps_ancestors_when_a_descendant_matches() {
        // init(1) -> shell(10) -> vim(11)
        //          -> rg(12)
        //  ssh(20) -> ssh-agent(21)
        //
        // Filter "vim" should keep init + shell + vim and drop the
        // rest of the tree (rg, ssh, ssh-agent). The ancestor chain
        // is what makes the match useful — without it the user just
        // sees `vim` floating with no context.
        let rows = vec![
            p(1, 0, "init"),
            p(10, 1, "shell"),
            p(20, 1, "ssh"),
            p(11, 10, "vim"),
            p(12, 10, "rg"),
            p(21, 20, "ssh-agent"),
        ];
        let v = compute_visible_tree(&rows, procs::SortBy::Pid, "vim");
        let pids: Vec<i32> = v.iter().map(|r| rows[r.idx].pid).collect();
        assert_eq!(pids, vec![1, 10, 11]);
    }

    #[test]
    fn tree_filter_drops_subtree_with_no_match() {
        // Filter "nonexistent" should produce an empty render list,
        // not panic, not partial render.
        let rows = vec![p(1, 0, "init"), p(10, 1, "shell"), p(11, 10, "vim")];
        let v = compute_visible_tree(&rows, procs::SortBy::Pid, "nonexistent");
        assert!(v.is_empty(), "got {} rows", v.len());
    }

    #[test]
    fn tree_sort_orders_siblings_by_cpu_when_requested() {
        // init has three children with different CPU%. Tree shape
        // stays init -> {a,b,c}, but the order of children must be
        // CPU-desc when SortBy::Cpu is requested.
        let rows = vec![
            p(1, 0, "init"),
            procs::ProcessRow {
                cpu_pct: Some(5.0),
                ..p(10, 1, "low")
            },
            procs::ProcessRow {
                cpu_pct: Some(80.0),
                ..p(11, 1, "hot")
            },
            procs::ProcessRow {
                cpu_pct: Some(20.0),
                ..p(12, 1, "warm")
            },
        ];
        let v = compute_visible_tree(&rows, procs::SortBy::Cpu, "");
        let names: Vec<&str> = v.iter().map(|r| rows[r.idx].command.as_str()).collect();
        // init first, then siblings hottest -> coolest.
        assert_eq!(names, vec!["init", "hot", "warm", "low"]);
    }

    #[test]
    fn flat_visible_respects_filter_and_sort() {
        let rows = vec![
            procs::ProcessRow {
                cpu_pct: Some(5.0),
                ..p(1, 0, "alpha")
            },
            procs::ProcessRow {
                cpu_pct: Some(50.0),
                ..p(2, 0, "beta")
            },
            procs::ProcessRow {
                cpu_pct: Some(15.0),
                ..p(3, 0, "alphabet")
            },
        ];
        // Filter "alpha" matches alpha + alphabet. Sort by CPU% desc.
        let v = compute_visible_flat(&rows, procs::SortBy::Cpu, "alpha");
        let pids: Vec<i32> = v.iter().map(|r| rows[r.idx].pid).collect();
        assert_eq!(pids, vec![3, 1]); // alphabet (15%) then alpha (5%)
        for r in &v {
            assert!(r.prefix.is_empty(), "flat mode should leave prefix empty");
        }
    }

    fn iface(name: &str, rx: Option<u64>, tx: Option<u64>) -> net::Iface {
        net::Iface {
            name: name.into(),
            rx_rate: rx,
            tx_rate: tx,
            rx_bytes: 0,
            tx_bytes: 0,
        }
    }

    #[test]
    fn total_net_rates_sums_with_none_as_zero() {
        // None on the first sample means "not enough data yet" — count
        // it as zero rather than dropping the iface, so the sparkline
        // shows a real, conservative trend.
        let ifs = vec![
            iface("eth0", Some(1_000), Some(500)),
            iface("wlan0", None, Some(200)),
            iface("tun0", Some(50), None),
        ];
        assert_eq!(total_net_rates(&ifs), (1_050, 700));
        assert_eq!(total_net_rates(&[]), (0, 0));
    }

    #[test]
    fn percore_height_zero_when_no_cores() {
        assert_eq!(percore_height(0, 200), 0);
    }

    #[test]
    fn percore_height_fits_in_one_row_when_wide_enough() {
        // 4 cores at 11 cols each = 44 cols → fits 1 row in 88 wide.
        assert_eq!(percore_height(4, 88), 1);
    }

    #[test]
    fn percore_height_caps_at_two_rows() {
        // 32 cores in an 80-col terminal → 7 cols per row → 5 rows
        // before capping. We cap at 2 to leave the procs body room.
        assert_eq!(percore_height(32, 80), 2);
    }

    #[test]
    fn percore_height_handles_narrow_terminal() {
        // 1 col-per-cell minimum; 2 cores in a 5-col terminal still
        // returns at least 1 row, never panics.
        assert!(percore_height(2, 5) >= 1);
    }

    #[test]
    fn compact_temp_label_maps_common_sensors() {
        // The regression we care about: the raw `pch_cannonlake#1`
        // name that confused the user before 0.4.1 must collapse to
        // something short and human-readable.
        assert_eq!(compact_temp_label("pch_cannonlake#1"), "pch");
        assert_eq!(compact_temp_label("pch_skylake#2"), "pch");
        // Intel + AMD CPUs, package and per-core.
        assert_eq!(compact_temp_label("coretemp Package id 0"), "cpu pkg");
        assert_eq!(compact_temp_label("coretemp Core 0"), "cpu");
        assert_eq!(compact_temp_label("k10temp Tctl"), "cpu");
        // GPUs + NVMe + wifi + ACPI.
        assert_eq!(compact_temp_label("amdgpu edge"), "gpu");
        assert_eq!(compact_temp_label("nvme Composite"), "nvme");
        assert_eq!(compact_temp_label("iwlwifi"), "wifi");
        assert_eq!(compact_temp_label("acpitz"), "acpi");
        // Unknown sensors collapse to a safe fallback, never the raw
        // kernel name.
        assert_eq!(compact_temp_label("some_weird_chip#3"), "sensor");
    }

    #[test]
    fn informative_temp_filter_keeps_cpu_always_and_pch_only_when_hot() {
        let cpu_cool = temp::Reading {
            label: "coretemp Package id 0".into(),
            celsius: 35.0,
        };
        let pch_cool = temp::Reading {
            label: "pch_cannonlake#1".into(),
            celsius: 30.0,
        };
        let pch_warm = temp::Reading {
            label: "pch_cannonlake#1".into(),
            celsius: 75.0,
        };
        assert!(is_informative_temp(&cpu_cool));
        assert!(!is_informative_temp(&pch_cool));
        assert!(is_informative_temp(&pch_warm));
    }
}
