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
//!     g             (Procs) toggle group view (cluster by container / language / system / native)
//!     H             (Procs) toggle per-core CPU spectrum (sparkline + % + gauge per core)
//!     /             (Procs) enter filter mode (Esc to clear, Enter to confirm)
//!     K             (Procs) send SIGTERM to selected pid (with confirm)
//!     Ctrl-K        (Procs) send SIGKILL to selected pid (with confirm)

mod battery;
mod disk;
mod errors;
mod gpu;
mod groups;
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

/// Ring buffer for host-level CPU% / memory% / net rate / GPU%
/// histories. Same window logic as `CpuHistory`: 60 samples × 1 s
/// tick = last minute. CPU / mem / GPU are stored as `0..=100`
/// percentages; net is stored as raw bytes/sec so `draw_host_history`
/// can compute a rolling max for the sparkline ceiling and label the
/// actual rate. GPU is `None`-tolerant: machines without a card-with-
/// metrics keep an empty deque and the sparkline column hides itself.
///
/// `per_core` carries one ring per CPU core, feeding the optional
/// per-core heatmap (toggled with `H` in the Procs view). The Vec
/// is lazily resized to the core count on the first push, so a
/// machine where `cpuinfo` reports a different topology after a
/// hotplug just resets cleanly rather than indexing OOB.
#[derive(Debug, Default)]
struct HostHistory {
    cpu: VecDeque<u64>,
    mem: VecDeque<u64>,
    net_down: VecDeque<u64>,
    net_up: VecDeque<u64>,
    gpu: VecDeque<u64>,
    per_core: Vec<VecDeque<u64>>,
}

impl HostHistory {
    fn push(
        &mut self,
        cpu_pct: Option<f64>,
        mem_pct: f64,
        net_down_bps: u64,
        net_up_bps: u64,
        gpu_pct: Option<f64>,
    ) {
        push_pct(&mut self.cpu, cpu_pct.unwrap_or(0.0));
        push_pct(&mut self.mem, mem_pct);
        push_raw(&mut self.net_down, net_down_bps);
        push_raw(&mut self.net_up, net_up_bps);
        // Only record GPU samples when a backend gave us a real
        // number. Otherwise the deque stays empty and
        // `draw_host_history` knows to hide the column.
        if let Some(p) = gpu_pct {
            push_pct(&mut self.gpu, p);
        }
    }

    /// Append one sample per core. The first call (or any call where
    /// the core count changed since last tick — CPU hotplug, vCPU
    /// rebalance) resets the rings, which is fine: the heatmap
    /// simply starts fresh for the new topology.
    fn push_per_core(&mut self, samples: &[f64]) {
        if self.per_core.len() != samples.len() {
            self.per_core = (0..samples.len())
                .map(|_| VecDeque::with_capacity(CPU_HISTORY_CAP))
                .collect();
        }
        for (ring, &pct) in self.per_core.iter_mut().zip(samples) {
            push_pct(ring, pct);
        }
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
    /// How the procs body is laid out: `Flat` is the sortable
    /// htop-style list (default), `Tree` shows the parent → children
    /// hierarchy, and `Group` clusters processes by container /
    /// language runtime / system / native with aggregated header
    /// rows. Toggled with `t` (Tree ↔ Flat) and `g` (Group ↔ Flat).
    list_mode: ListMode,
    /// When true, the per-core CPU panel is replaced by the
    /// "spectrum" view — one row per core combining a 60-second
    /// sparkline, the live %, and a proportional gauge. Toggled
    /// with `H`. Default off because it's a power-user view that
    /// costs vertical real estate proportional to the core count
    /// — fine on a 14-core laptop with a tall terminal, less so
    /// on a 4-row VT in a recovery shell.
    per_core_spectrum: bool,

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
    gpu_tracker: gpu::Tracker,
    /// All GPUs found under `/sys/class/drm/card*`. AMD cards have
    /// real metrics (`busy_pct`, `vram_*`); NVIDIA / Intel are
    /// detected-only until we ship NVML / i915 backends in a later
    /// release. The host overview surfaces *all* cards (so the user
    /// knows the hardware was recognised); the sparkline only
    /// covers cards that report `busy_pct`.
    gpus: Vec<gpu::Gpu>,
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
        let mut gpu_tracker = gpu::Tracker::default();
        let gpus = gpu_tracker.snapshot();
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
            list_mode: ListMode::Flat,
            per_core_spectrum: false,
            prev_host_cpu,
            host_info,
            net_tracker,
            ifaces,
            temp_tracker,
            temps,
            batteries,
            disk_tracker,
            disks,
            gpu_tracker,
            gpus,
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
            // GPU sysfs reads are cheap (single-digit microseconds
            // on AMD), so they could go in the fast path. Keeping
            // them here means the GPU number ticks at the same
            // human-paced cadence as temps and disk I/O — fine for
            // a 1 Hz UI, and saves a few hundred microseconds per
            // tick on machines with multiple cards.
            self.gpus = self.gpu_tracker.snapshot();
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
        let gpu_pct = gpu::aggregate_busy_pct(&self.gpus);
        self.host_history
            .push(self.host_info.cpu_pct, mem_pct, net_down, net_up, gpu_pct);
        self.host_history
            .push_per_core(&self.host_info.per_core_pct);
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
        self.procs_visible = match self.list_mode {
            ListMode::Tree => {
                compute_visible_tree(&self.procs_all, self.procs_sort, &self.procs_filter)
            }
            ListMode::Group => {
                compute_visible_grouped(&self.procs_all, self.procs_sort, &self.procs_filter)
            }
            ListMode::Flat => {
                compute_visible_flat(&self.procs_all, self.procs_sort, &self.procs_filter)
            }
        };
    }

    /// After `procs_visible` has been recomputed, find the row index
    /// corresponding to `pid` (if it still exists in the visible set)
    /// and pin the cursor to it. Falls back silently when the pid is
    /// gone (process exited) or filtered out — `clamp_selections` will
    /// then put the cursor on whatever row 0 is now.
    fn reanchor_proc_selection(&mut self, pid: Option<i32>) {
        let Some(pid) = pid else { return };
        if let Some(new_idx) = self.procs_visible.iter().position(|r| {
            r.header.is_none() && self.procs_all.get(r.idx).is_some_and(|row| row.pid == pid)
        }) {
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
        // Group mode emits synthetic header rows that don't map to a
        // real PID. Kill / detail-pane callers must see `None` for
        // those so we don't try to SIGTERM a non-process.
        if r.header.is_some() {
            return None;
        }
        self.procs_all.get(r.idx)
    }
}

/// Layout choice for the Procs body. Mutually exclusive — pressing
/// `t` while in `Group` switches back to `Flat`, and vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListMode {
    /// Sortable htop-style flat list. Default.
    Flat,
    /// Parent → children hierarchy with branch glyphs in the prefix.
    Tree,
    /// Clustered by container / language runtime / system / native,
    /// each cluster preceded by a synthetic header row that
    /// aggregates count, total CPU%, and total RSS.
    Group,
}

/// One row of the rendered Procs table. `idx` indexes into
/// `procs_all`; `prefix` is whatever decoration the layout mode
/// wants (e.g. `"│ ├─"` in tree mode). When `header` is `Some`,
/// this is a *synthetic* group-header row — `idx` is meaningless
/// and the renderer paints the header info instead.
#[derive(Debug, Clone, Default)]
struct ProcRender {
    idx: usize,
    prefix: String,
    /// `Some` → synthetic group header (skipped by selection /
    /// kill keys); `None` → a real process row.
    header: Option<GroupHeader>,
}

/// Aggregated info painted on a `Group` mode header row.
#[derive(Debug, Clone, Default)]
struct GroupHeader {
    /// e.g. `docker:abc12`, `java`, `system`, `native`.
    label: String,
    /// `Container`, `Runtime`, `System`, or `Native` — chosen by
    /// the renderer to colour the header consistently.
    band: groups::GroupBand,
    /// Number of member processes in this group.
    count: usize,
    /// Sum of `cpu_pct` across members (0..N·100).
    total_cpu: f64,
    /// Sum of `rss_bytes` across members.
    total_rss: u64,
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
            header: None,
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
    out.push(ProcRender {
        idx,
        prefix,
        header: None,
    });

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

/// Group-mode path: cluster surviving rows by `Group` (container >
/// runtime > system > native), emit a synthetic header row for
/// each cluster, then the cluster's members. Sort and filter both
/// apply: filter prunes rows before grouping, and the chosen
/// `SortBy` orders members within each group **and** orders the
/// groups themselves (groups with the highest aggregate of the
/// sort key bubble up first within their band).
///
/// Why the band ordering matters: a developer skimming for "what
/// is my laptop *actually* running right now" wants Docker /
/// Podman containers at the top because those are the workloads
/// they explicitly started, then language runtimes (the daemons
/// the developer actively launched), then system, then native.
fn compute_visible_grouped(
    rows: &[procs::ProcessRow],
    by: procs::SortBy,
    filter: &str,
) -> Vec<ProcRender> {
    // Bucket surviving indices by group key.
    let mut buckets: HashMap<String, Vec<usize>> = HashMap::new();
    let mut group_for: HashMap<String, groups::Group> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if !procs::matches(r, filter) {
            continue;
        }
        let key = r.group.sort_key();
        group_for
            .entry(key.clone())
            .or_insert_with(|| r.group.clone());
        buckets.entry(key).or_default().push(i);
    }

    // Sort groups by their *band* primarily (the prefix in
    // `sort_key` already encodes that), then by aggregate of the
    // chosen sort key descending (CPU / RSS) or ascending (PID /
    // command — for those, fall back to alphabetic group name to
    // keep the layout stable).
    let mut group_keys: Vec<String> = buckets.keys().cloned().collect();
    group_keys.sort_by(|a, b| {
        let ag = &buckets[a];
        let bg = &buckets[b];
        // Same band? Order by the chosen sort key's aggregate.
        let a_band = a.chars().next();
        let b_band = b.chars().next();
        if a_band != b_band {
            return a.cmp(b);
        }
        match by {
            procs::SortBy::Cpu => sum_cpu(rows, bg)
                .partial_cmp(&sum_cpu(rows, ag))
                .unwrap_or(std::cmp::Ordering::Equal),
            procs::SortBy::Mem => sum_rss(rows, bg).cmp(&sum_rss(rows, ag)),
            procs::SortBy::Pid | procs::SortBy::Command => a.cmp(b),
        }
    });

    let mut out: Vec<ProcRender> =
        Vec::with_capacity(buckets.values().map(Vec::len).sum::<usize>() + buckets.len());
    for key in group_keys {
        let mut members = buckets.remove(&key).unwrap_or_default();
        members.sort_by(|&a, &b| cmp_rows(rows, a, b, by));
        let group = group_for.remove(&key).unwrap_or(groups::Group::Native);
        let header = GroupHeader {
            label: group.label(),
            band: group.band(),
            count: members.len(),
            total_cpu: sum_cpu(rows, &members),
            total_rss: sum_rss(rows, &members),
        };
        // Synthetic header row.
        out.push(ProcRender {
            idx: usize::MAX,
            prefix: String::new(),
            header: Some(header),
        });
        // Members indented under the header so the visual grouping
        // is unambiguous even before colour.
        for idx in members {
            out.push(ProcRender {
                idx,
                prefix: "  ".to_string(),
                header: None,
            });
        }
    }
    out
}

fn sum_cpu(rows: &[procs::ProcessRow], idxs: &[usize]) -> f64 {
    idxs.iter()
        .filter_map(|&i| rows.get(i).and_then(|r| r.cpu_pct))
        .sum()
}

fn sum_rss(rows: &[procs::ProcessRow], idxs: &[usize]) -> u64 {
    idxs.iter()
        .filter_map(|&i| rows.get(i).map(|r| r.rss_bytes))
        .sum()
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
            // Tree toggle (Tree ↔ Flat). Pressing `t` from Group
            // mode also lands on Tree — the user is asking for a
            // hierarchical view either way. Re-anchor by pid so the
            // cursor stays on the same process after rows shuffle.
            let pinned = app.selected_proc().map(|r| r.pid);
            app.list_mode = match app.list_mode {
                ListMode::Tree => ListMode::Flat,
                _ => ListMode::Tree,
            };
            app.recompute_procs();
            app.reanchor_proc_selection(pinned);
            app.clamp_selections();
        }
        KeyCode::Char('g') if app.view == View::Procs => {
            // Group toggle (Group ↔ Flat). Same re-anchor logic as
            // the tree toggle so the cursor follows the pid through
            // the layout change.
            let pinned = app.selected_proc().map(|r| r.pid);
            app.list_mode = match app.list_mode {
                ListMode::Group => ListMode::Flat,
                _ => ListMode::Group,
            };
            app.recompute_procs();
            app.reanchor_proc_selection(pinned);
            app.clamp_selections();
        }
        KeyCode::Char('H') if app.view == View::Procs => {
            // Per-core spectrum-view toggle. Capital H so it
            // doesn't shadow any future vim-style left-motion key.
            // The first toggle "on" is essentially free at frame
            // time because `host_history.per_core` has been
            // filling since launch — the user instantly sees the
            // last 60 s of per-core activity (sparkline + numeric
            // % + gauge) without waiting for a sample window to
            // accumulate.
            app.per_core_spectrum = !app.per_core_spectrum;
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
        kv_line(
            "  g ",
            "toggle group view (container / runtime / system / native, with totals)",
            kb,
            dim,
        ),
        kv_line(
            "  H ",
            "toggle per-core CPU spectrum (60 s history + live % + gauge)",
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
    // Host overview is 3 rows by default and 4 when at least one
    // GPU is detected. Computing it once and reusing the value
    // keeps `draw_host`'s own "add a line iff GPUs is non-empty"
    // behaviour honest — the layout reserves exactly the space the
    // paragraph will consume.
    let host_h = host_overview_rows(&app.gpus);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),      // title
            Constraint::Length(host_h), // host overview (3 or 4 lines)
            Constraint::Min(5),         // fleet table
            Constraint::Length(16),     // serial + resources pane
            Constraint::Length(1),      // help
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
        &app.gpus,
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
    let percore_h = percore_height(
        app.host_info.per_core_pct.len(),
        area.width,
        area.height,
        app.per_core_spectrum,
    );
    let host_h = host_overview_rows(&app.gpus);
    // The memory composition bar needs 3 rows (top border, content,
    // bottom border). Hide it on terminals shorter than ~24 rows
    // so the procs body still gets at least 5 rows of useful list.
    let mem_bar_h: u16 = if area.height >= 22 { 3 } else { 0 };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),         // title
            Constraint::Length(host_h),    // host overview (3 or 4 lines)
            Constraint::Length(percore_h), // per-core CPU grid (0..=2)
            Constraint::Length(mem_bar_h), // memory composition bar (0 or 3)
            Constraint::Length(3),         // CPU + MEM + NET + GPU sparklines
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
        &app.gpus,
    );
    if percore_h > 0 {
        if app.per_core_spectrum {
            draw_per_core_spectrum(
                f,
                chunks[2],
                &app.host_history.per_core,
                &app.host_info.per_core_pct,
            );
        } else {
            draw_per_core(f, chunks[2], &app.host_info.per_core_pct);
        }
    }
    if mem_bar_h > 0 {
        draw_mem_bar(f, chunks[3], &app.host_info);
    }
    draw_host_history(f, chunks[4], &app.host_history);

    // Allocate the detail pane only when the terminal is wide enough.
    // Below ~110 cols the table needs every column to stay readable.
    let body = chunks[5];
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
    draw_footer(f, chunks[6], app);
}

/// Per-core row width = `c{nn} {bar} {pct}%  ` ≈ 11 columns. The grid
/// auto-flows that many cells per row and caps at 2 rows so the
/// procs body never gets squeezed into nothing on a 24-row terminal.
const PERCORE_CELL_W: u16 = 11;
const PERCORE_MAX_ROWS: u16 = 2;

/// 3 by default; 4 once a GPU is detected. The layout keeps the
/// host-overview height in lockstep with what `draw_host` will
/// actually paint so we never reserve a blank row.
fn host_overview_rows(gpus: &[gpu::Gpu]) -> u16 {
    if gpus.is_empty() {
        3
    } else {
        4
    }
}

fn percore_height(num_cores: usize, width: u16, term_h: u16, spectrum: bool) -> u16 {
    if num_cores == 0 {
        return 0;
    }
    if spectrum {
        // One row per core + 1 row for the time-axis tick label,
        // capped at a third of the terminal so the procs body
        // still gets ~two-thirds of the screen. Floor at 4 so
        // even a 6-row terminal gets 3 cores + axis rather than
        // collapsing into nubs.
        let want = u16::try_from(num_cores.saturating_add(1)).unwrap_or(u16::MAX);
        let dyn_cap = (term_h / 3).max(4);
        return want.min(dyn_cap);
    }
    let per_row = (width / PERCORE_CELL_W).max(1) as usize;
    let rows = num_cores.div_ceil(per_row);
    u16::try_from(rows.min(PERCORE_MAX_ROWS as usize)).unwrap_or(PERCORE_MAX_ROWS)
}

/// Reserve 5 cells for the row label `" c{:<2} "` (4 visible chars
/// plus a trailing space). Lifted to module scope so
/// `clippy::items_after_statements` stays happy.
const SPECTRUM_LABEL_W: u16 = 5;
/// Width of the trailing `"  99% "` numeric percent column.
const SPECTRUM_PCT_W: u16 = 6;
/// Width of the trailing gauge ` ▕XXXXXXXXXXXX▏` — 1 leading space,
/// 1 cell `▕`, `SPECTRUM_GAUGE_CELLS` bar slots, 1 cell `▏`. Tuned so
/// the gauge reads at a glance without dominating the row.
const SPECTRUM_GAUGE_CELLS: u16 = 12;
const SPECTRUM_GAUGE_W: u16 = 1 + 1 + SPECTRUM_GAUGE_CELLS + 1;
const SPECTRUM_FIXED_W: u16 = SPECTRUM_LABEL_W + SPECTRUM_PCT_W + SPECTRUM_GAUGE_W;

/// Per-core "spectrum" view. Each row triple-encodes one core:
///
/// * **Time series** — a 60-second sparkline drawn with the
///   `▁▂▃▄▅▆▇█` block ramp, every cell *also* coloured by the
///   green/yellow/red load palette. The eye reads height **and**
///   colour, so a glance separates "long quiet stretch with a
///   recent spike" from "hot all minute" without conscious work.
/// * **Live %** — the most recent sample as a numeric percent.
/// * **Gauge** — a proportional bar `▕████░░░░░░░░▏` so a busy core
///   pops visually next to the quieter ones.
///
/// htop / btm / btop all show the live per-core %; btop's heatmap
/// shows the time axis. Combining height-coded sparkline +
/// numeric + gauge per row gives three readouts where the
/// existing tools give one or two — and groups them per-core so
/// you can scan the whole CPU like an EKG strip.
///
/// The last row of the panel is reserved for a thin time-axis
/// tick label (`-Ns ──── now`) so a new user instantly sees the
/// chart's reach.
fn draw_per_core_spectrum(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    rings: &[VecDeque<u64>],
    live: &[f64],
) {
    if rings.is_empty() || area.height == 0 || area.width <= SPECTRUM_FIXED_W {
        return;
    }
    let spark_w = (area.width - SPECTRUM_FIXED_W) as usize;
    if spark_w == 0 {
        return;
    }

    // Last visible row is the axis label; everything above is cores.
    let core_rows_budget = (area.height as usize).saturating_sub(1);
    let core_rows = core_rows_budget.min(rings.len());

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(core_rows + 1);
    for (i, ring) in rings.iter().take(core_rows).enumerate() {
        lines.push(spectrum_row(i, ring, live.get(i).copied(), spark_w));
    }
    lines.push(spectrum_axis_row(spark_w));
    f.render_widget(Paragraph::new(lines), area);
}

/// Build one core's row. Public-ish so unit tests can exercise the
/// span layout without a real `Frame`.
fn spectrum_row(
    core_idx: usize,
    ring: &VecDeque<u64>,
    live_pct: Option<f64>,
    spark_w: usize,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(spark_w + 8);
    spans.push(Span::styled(
        format!(" c{core_idx:<2} "),
        Style::default().fg(Color::DarkGray),
    ));

    // Sparkline. Take the most recent `spark_w` samples and left-pad
    // with blanks so a freshly-launched neotop doesn't render
    // right-justified before the buffer fills.
    let start = ring.len().saturating_sub(spark_w);
    let visible: Vec<u64> = ring.range(start..).copied().collect();
    let pad = spark_w.saturating_sub(visible.len());
    for _ in 0..pad {
        spans.push(Span::raw(" "));
    }
    for &v in &visible {
        #[allow(clippy::cast_precision_loss)]
        let pct = v as f64;
        spans.push(Span::styled(
            bar_glyph(pct).to_string(),
            Style::default().fg(cpu_load_color(pct)),
        ));
    }

    // Numeric %. Prefer the smoothed live value the rest of the UI
    // shows; fall back to the latest ring sample if the topology
    // ring just got resized and `live` lags by a tick.
    #[allow(clippy::cast_precision_loss)]
    let cur = live_pct.unwrap_or_else(|| ring.back().copied().unwrap_or(0) as f64);
    let cur_color = cpu_load_color(cur);
    spans.push(Span::styled(
        format!("  {cur:>3.0}% "),
        Style::default().fg(cur_color),
    ));

    // Gauge.
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        "▕".to_string(),
        Style::default().fg(Color::DarkGray),
    ));
    spans.extend(gauge_cells(cur, SPECTRUM_GAUGE_CELLS as usize, cur_color));
    spans.push(Span::styled(
        "▏".to_string(),
        Style::default().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

/// Bottom tick row: `-Ns ──────────────── now`, where `N` is the
/// sparkline width in samples (= seconds, since we tick at 1 Hz).
/// Under the label column we leave whitespace so the axis sits
/// flush with the start of the sparkline.
fn spectrum_axis_row(spark_w: usize) -> Line<'static> {
    let style = Style::default().fg(Color::DarkGray);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
    // 5 spaces under the label column.
    spans.push(Span::raw(" ".repeat(SPECTRUM_LABEL_W as usize)));
    let lhs = format!("-{spark_w}s ");
    let rhs = " now";
    let dashes = spark_w
        .saturating_sub(lhs.chars().count())
        .saturating_sub(rhs.chars().count());
    spans.push(Span::styled(lhs, style));
    spans.push(Span::styled("─".repeat(dashes), style));
    spans.push(Span::styled(rhs.to_string(), style));
    Line::from(spans)
}

/// Render a horizontal gauge of `cells` characters: filled cells in
/// `fill_color`, empties in `DarkGray`. Returns the cells as spans
/// (no surrounding brackets — those are emitted at the call site so
/// the caller can colour them independently).
fn gauge_cells(pct: f64, cells: usize, fill_color: Color) -> Vec<Span<'static>> {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * cells as f64).round() as usize;
    let filled = filled.min(cells);
    vec![
        Span::styled("█".repeat(filled), Style::default().fg(fill_color)),
        Span::styled(
            "░".repeat(cells - filled),
            Style::default().fg(Color::DarkGray),
        ),
    ]
}

/// Four-stop colour ramp shared by the spectrum sparkline cells,
/// the live % readout, and the gauge fill. Idle (≤19 %) is dark
/// grey rather than green so a quiet core fades into the
/// background and the eye is drawn to active cores. Same upper
/// breakpoints as `cpu_glyph_color` so the rest of the UI keeps
/// one mental model.
fn cpu_load_color(pct: f64) -> Color {
    if pct >= 80.0 {
        Color::Red
    } else if pct >= 50.0 {
        Color::Yellow
    } else if pct >= 20.0 {
        Color::Green
    } else {
        Color::DarkGray
    }
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
        // GROUP / CONTAINER: developer-meaningful classification.
        // For container processes show a more explicit label so the
        // user knows they can run `docker ps` / `podman ps` to
        // recover the human name; otherwise just the runtime name.
        match &r.group {
            groups::Group::Container(c) => {
                lines.push(kv(
                    "CONTAINER",
                    format!("{}:{}", c.runtime.label(), c.id),
                    label,
                ));
            }
            groups::Group::Runtime(_) | groups::Group::System | groups::Group::Native => {
                lines.push(kv("GROUP", r.group.label(), label));
            }
        }
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

/// Wide horizontal stacked bar showing the four classes of memory
/// usage at full screen width: **used | buffers | cached | free**.
/// The reason this beats the line-1 `MEM 5G/16G (32%)` summary is
/// that "32%" hides what kind of usage you're looking at — page
/// cache (cyan, instantly reclaimable) vs. real allocations (red,
/// the number that actually matters when you're chasing OOMs).
///
/// `htop` shows this *tiny* and only one row tall; `btop` doesn't
/// surface buffers + cached at all. Giving it a dedicated full-width
/// row turns memory composition from an afterthought into a chart
/// you can read in one glance.
fn draw_mem_bar(f: &mut ratatui::Frame<'_>, area: Rect, h: &host::HostInfo) {
    if h.mem_total_bytes == 0 || area.width < 20 {
        return;
    }
    // Compute segment widths in cells. We render the bar on the
    // *content* row inside a bordered block, hence area.width - 2
    // for left/right borders.
    let total = h.mem_total_bytes;
    let cached = h.mem_cached_bytes;
    let buffers = h.mem_buffers_bytes;
    // Used = total - free - buffers - cached. This is the real
    // "memory I can't get back without paging" number, which
    // matches what `free -h`'s `used` column shows.
    let free = h.mem_free_bytes;
    let used = total
        .saturating_sub(free)
        .saturating_sub(buffers)
        .saturating_sub(cached);

    let bar_w = u64::from(area.width.saturating_sub(2));
    let used_w = scale(used, total, bar_w);
    let buffers_w = scale(buffers, total, bar_w);
    let cached_w = scale(cached, total, bar_w);
    // Free fills whatever pixels the rounding left over; this avoids
    // the bar being one cell short on the right edge.
    let free_w = bar_w
        .saturating_sub(used_w)
        .saturating_sub(buffers_w)
        .saturating_sub(cached_w);

    // Build the bar by appending colored space-spans. Each cell is
    // a single ASCII space painted with a background color — that
    // gives a solid-fill block on every terminal without needing
    // Unicode block glyphs.
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(8);
    spans.push(seg(" used ", Color::Red, used_w, used));
    spans.push(seg(" buf ", Color::Blue, buffers_w, buffers));
    spans.push(seg(" cache ", Color::Cyan, cached_w, cached));
    spans.push(seg(" free ", Color::DarkGray, free_w, free));

    let title = format!(
        " memory  {} used \u{2502} {} buf \u{2502} {} cache \u{2502} {} free \u{2502} {} total ",
        proc::human_bytes(used),
        proc::human_bytes(buffers),
        proc::human_bytes(cached),
        proc::human_bytes(free),
        proc::human_bytes(total),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(title, Style::default().fg(Color::DarkGray)));
    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

/// Scale `n` out of `total` to a cell count, never returning more
/// than `bar_w`. `total == 0` short-circuits to 0 — the meminfo
/// loop guards against that anyway.
fn scale(n: u64, total: u64, bar_w: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    // Use u128 to avoid overflow on multi-TB systems.
    u64::try_from(u128::from(n) * u128::from(bar_w) / u128::from(total))
        .unwrap_or(bar_w)
        .min(bar_w)
}

/// Build one segment of the memory bar. Each segment is a single
/// span of `width` spaces, painted with a solid background. We
/// inline a tiny ASCII label (e.g. `" used "`) when the segment is
/// wide enough; very narrow segments get a single dot to avoid
/// truncated label fragments. Bytes are passed through so the
/// caller can compose a tooltip-style summary in the title.
fn seg(label: &str, bg: Color, width: u64, _bytes: u64) -> Span<'static> {
    let w = usize::try_from(width).unwrap_or(0);
    if w == 0 {
        return Span::raw("");
    }
    // Show the label only when there's room for it plus padding.
    let content = if w >= label.len() + 2 {
        let pad_total = w - label.len();
        let left = pad_total / 2;
        let right = pad_total - left;
        format!("{}{label}{}", " ".repeat(left), " ".repeat(right))
    } else {
        " ".repeat(w)
    };
    Span::styled(content, Style::default().fg(Color::Black).bg(bg))
}

/// Side-by-side host-level sparklines — CPU%, MEM%, NET↓, NET↑,
/// and (when an AMD-class card is reporting) GPU%. 60 samples each
/// → last minute at the default 1 s tick. CPU/MEM/GPU share a
/// 0-100 ceiling; the two net sparklines auto-scale to the rolling
/// max in their window so a 30 KB/s burst is still visible next to
/// a 200 MB/s spike from earlier.
fn draw_host_history(f: &mut ratatui::Frame<'_>, area: Rect, h: &HostHistory) {
    // The GPU column only appears once we have at least one sample
    // — otherwise we'd reserve 1/5 of the row for a blank
    // sparkline on every box that has no AMD card. This keeps
    // workstations and laptops visually identical until the dGPU
    // actually reports something.
    let show_gpu = !h.gpu.is_empty();
    let constraints: Vec<Constraint> = if show_gpu {
        vec![Constraint::Ratio(1, 5); 5]
    } else {
        vec![Constraint::Ratio(1, 4); 4]
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    let cpu_data: Vec<u64> = h.cpu.iter().copied().collect();
    let mem_data: Vec<u64> = h.mem.iter().copied().collect();
    let down_data: Vec<u64> = h.net_down.iter().copied().collect();
    let up_data: Vec<u64> = h.net_up.iter().copied().collect();

    // Newest sample = label value; the user thinks "what's it doing
    // *right now*", not "what's the average over the last minute".
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

    if show_gpu {
        let gpu_data: Vec<u64> = h.gpu.iter().copied().collect();
        let gpu_now = gpu_data.last().copied().unwrap_or(0);
        let gpu_title = format!(" GPU {gpu_now}% ");
        f.render_widget(
            Sparkline::default()
                .block(Block::default().borders(Borders::ALL).title(gpu_title))
                .data(&gpu_data)
                .max(100)
                // Distinct hue from the four neighbours so the eye
                // can pick it out at a glance: red/orange-leaning
                // since "GPU pegged" is usually the headline number
                // on machines that have one.
                .style(Style::default().fg(Color::LightRed)),
            cols[4],
        );
    }
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

// `clippy::too_many_arguments`: the eight slices are exactly the
// host-snapshot data sources rendered as a paragraph. Wrapping
// them in a struct just to please the lint would mean either (a)
// borrowing eight fields out of `App` into a temporary view
// struct on every draw or (b) adding lifetimes for `App`'s field
// borrows to a public type. Neither is worth the cost; the
// parameter list is tabular and clear at the call site.
#[allow(clippy::too_many_arguments)]
fn draw_host(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    h: &host::HostInfo,
    ifaces: &[net::Iface],
    temps: &[temp::Reading],
    batteries: &[battery::Battery],
    disks: &[disk::Disk],
    gpus: &[gpu::Gpu],
) {
    // Three tight lines by default. A fourth GPU line appears only
    // when at least one card was discovered under `/sys/class/drm`,
    // so machines without a discrete GPU don't pay a row of screen
    // real estate for nothing.
    let mut lines = vec![
        host_line1(h, batteries),
        host_line_net_temp(ifaces, temps),
        host_line4(disks),
    ];
    if !gpus.is_empty() {
        lines.push(host_line_gpu(gpus));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Width of each inline GPU gauge `▕XXXXXXXX▏`. Eight cells is
/// enough to read the fill at a glance without crowding the line —
/// busy % and VRAM each get their own gauge so the overview shows
/// the same triple readout (numeric + gauge + history-via-sparkline-up-top)
/// that the per-core spectrum view shows for CPU.
const GPU_GAUGE_CELLS: usize = 8;

/// One-line summary covering every detected GPU. AMD + NVIDIA
/// cards report real numbers (busy %, VRAM, watts) plus inline
/// gauges visualising current load and VRAM occupancy. Intel
/// cards are shown by name with a `(driver pending)` tag so the
/// user knows the hardware *is* recognised — the metrics just
/// aren't wired up in this neotop yet.
fn host_line_gpu(gpus: &[gpu::Gpu]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> =
        vec![Span::styled(" gpu ", Style::default().fg(Color::DarkGray))];
    for (i, g) in gpus.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(
            g.name.clone(),
            Style::default().fg(Color::Cyan),
        ));
        if let Some(busy) = g.busy_pct {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let busy_int = busy.round() as i64;
            let busy_color = gpu_busy_color(busy);
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("{busy_int:>3}%"),
                Style::default().fg(busy_color),
            ));
            // Inline busy gauge so a 92 %-pegged card shouts at you
            // visually, not just numerically.
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                "▕".to_string(),
                Style::default().fg(Color::DarkGray),
            ));
            spans.extend(gauge_cells(busy, GPU_GAUGE_CELLS, busy_color));
            spans.push(Span::styled(
                "▏".to_string(),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if g.vram_total > 0 {
            let pct_label = g
                .vram_pct()
                .map(|p| format!(" ({p:>4.1}%)"))
                .unwrap_or_default();
            spans.push(Span::raw(format!(
                " vram {}/{}{pct_label}",
                proc::human_bytes(g.vram_used),
                proc::human_bytes(g.vram_total),
            )));
            // VRAM gauge — the "current use / capacity" readout
            // in chart form. A card 95 % full of VRAM is a
            // pre-OOM signal you can spot from across the room.
            if let Some(vram_pct) = g.vram_pct() {
                let vram_color = cpu_load_color(vram_pct);
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    "▕".to_string(),
                    Style::default().fg(Color::DarkGray),
                ));
                spans.extend(gauge_cells(vram_pct, GPU_GAUGE_CELLS, vram_color));
                spans.push(Span::styled(
                    "▏".to_string(),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
        if let Some(w) = g.power_watts {
            spans.push(Span::raw(format!(" {w:.1}W")));
        }
        if !g.has_busy_data() && g.vram_total == 0 {
            // No backend wired up yet for this vendor.
            spans.push(Span::styled(
                " (driver pending)",
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    Line::from(spans)
}

/// Same green/yellow/red ramp the per-core CPU grid uses, so the
/// user reads the GPU number with the same eye they read CPU.
fn gpu_busy_color(busy: f64) -> Color {
    if busy >= 80.0 {
        Color::Red
    } else if busy >= 50.0 {
        Color::Yellow
    } else {
        Color::Green
    }
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

/// Header colour by group band: Cyan for Container (the workload
/// the developer explicitly started), Yellow for language Runtime
/// (the daemon they actively launched), `DarkGray` for System and
/// Native so they sit in the visual background of the panel.
fn group_band_color(band: groups::GroupBand) -> Color {
    match band {
        groups::GroupBand::Container => Color::Cyan,
        groups::GroupBand::Runtime => Color::Yellow,
        groups::GroupBand::System | groups::GroupBand::Native => Color::DarkGray,
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

// `draw_proc_table` is tabular and the body reads top-to-bottom: the
// header-row branch above the process-row branch. Splitting it would
// turn that into two functions called from a wrapper, which costs
// clarity for no real win.
#[allow(clippy::too_many_lines)]
fn draw_proc_table(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let header = Row::new(vec!["PID", "USER", "S", "CPU%", "RSS", "THR", "COMMAND"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let body: Vec<Row> = app
        .procs_visible
        .iter()
        .map(|pr| {
            // Group-mode synthetic header row: render as a single
            // banner spanning the COMMAND column with aggregated
            // count / CPU / RSS. The leading PID-shaped columns are
            // left blank so the eye instantly separates them from
            // process rows.
            if let Some(h) = &pr.header {
                let band_color = group_band_color(h.band);
                let cpu_text = format!("{:>5.1}", h.total_cpu);
                let rss_text = proc::human_bytes(h.total_rss);
                let banner = format!("▼ {label}  ({n})", label = h.label, n = h.count,);
                return Row::new(vec![
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(""),
                    Cell::from(Span::styled(
                        cpu_text,
                        Style::default().fg(band_color).add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(Span::styled(
                        rss_text,
                        Style::default().fg(band_color).add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(""),
                    Cell::from(Span::styled(
                        banner,
                        Style::default().fg(band_color).add_modifier(Modifier::BOLD),
                    )),
                ]);
            }

            // Real process row.
            let Some(r) = app.procs_all.get(pr.idx) else {
                return Row::new(vec![Cell::from("")]);
            };
            let cpu = r
                .cpu_pct
                .map_or_else(|| "—".to_string(), |p| format!("{p:.1}"));
            let cpu_style = Style::default().fg(cpu_glyph_color(r.cpu_pct.unwrap_or(0.0)));
            let state_style = proc_state_style(r.state);
            // In tree mode the COMMAND cell is prefixed with the
            // glyph chain ('│ ├─', '└─', etc); group mode uses two
            // leading spaces; flat mode prefix is empty.
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

    let title = match app.list_mode {
        ListMode::Tree => " processes · tree (t to leave) ".to_string(),
        ListMode::Group => format!(
            " processes · grouped (g to leave){} ",
            if app.procs_filter.is_empty() {
                String::new()
            } else {
                format!(" · /{}", app.procs_filter)
            },
        ),
        ListMode::Flat => format!(
            " processes · by {}{}{} ",
            app.procs_sort.label(),
            app.procs_sort.arrow(),
            if app.procs_filter.is_empty() {
                String::new()
            } else {
                format!(" · /{}", app.procs_filter)
            },
        ),
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
            group: groups::Group::Native,
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

    fn p_with_group(
        pid: i32,
        cmd: &str,
        cpu: f64,
        rss: u64,
        group: groups::Group,
    ) -> procs::ProcessRow {
        procs::ProcessRow {
            cpu_pct: Some(cpu),
            rss_bytes: rss,
            group,
            ..p(pid, 1, cmd)
        }
    }

    #[test]
    fn grouped_visible_emits_header_then_members_per_band() {
        // Three bands: a Container, a Runtime, and a Native. The
        // header row sits ahead of its members; the layout order is
        // Container → Runtime → System → Native.
        let rows = vec![
            p_with_group(
                100,
                "node server.js",
                40.0,
                1_000_000,
                groups::Group::Container(groups::Container {
                    runtime: groups::ContainerRuntime::Docker,
                    id: "abc12".into(),
                }),
            ),
            p_with_group(
                101,
                "postgres",
                10.0,
                500_000,
                groups::Group::Container(groups::Container {
                    runtime: groups::ContainerRuntime::Docker,
                    id: "abc12".into(),
                }),
            ),
            p_with_group(
                200,
                "java -jar",
                25.0,
                2_000_000,
                groups::Group::Runtime(groups::Lang::Java),
            ),
            p_with_group(300, "myapp", 1.0, 100_000, groups::Group::Native),
        ];
        let v = compute_visible_grouped(&rows, procs::SortBy::Cpu, "");
        // Pattern: header(docker) m m header(java) m header(native) m.
        assert!(v[0].header.is_some(), "row 0 should be a header");
        let h0 = v[0].header.as_ref().unwrap();
        assert_eq!(h0.label, "docker:abc12");
        assert_eq!(h0.count, 2);
        assert!((h0.total_cpu - 50.0).abs() < 1e-9);
        assert_eq!(h0.total_rss, 1_500_000);
        // Members under the docker header come next, sorted by CPU desc.
        assert!(v[1].header.is_none() && v[2].header.is_none());
        assert_eq!(rows[v[1].idx].pid, 100);
        assert_eq!(rows[v[2].idx].pid, 101);
        // Then the java header, then its single member.
        assert!(v[3].header.is_some());
        assert_eq!(v[3].header.as_ref().unwrap().label, "java");
        assert_eq!(rows[v[4].idx].pid, 200);
        // Native band last.
        assert!(v[5].header.is_some());
        assert_eq!(v[5].header.as_ref().unwrap().label, "native");
    }

    #[test]
    fn grouped_visible_filter_prunes_before_grouping() {
        // Filter that matches only one group's processes drops the
        // other group entirely (header + members).
        let rows = vec![
            p_with_group(
                1,
                "java -jar",
                20.0,
                0,
                groups::Group::Runtime(groups::Lang::Java),
            ),
            p_with_group(
                2,
                "node server",
                5.0,
                0,
                groups::Group::Runtime(groups::Lang::Node),
            ),
        ];
        let v = compute_visible_grouped(&rows, procs::SortBy::Cpu, "java");
        assert_eq!(v.len(), 2, "one header + one member");
        assert_eq!(v[0].header.as_ref().unwrap().label, "java");
        assert_eq!(rows[v[1].idx].pid, 1);
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
        assert_eq!(percore_height(0, 200, 40, false), 0);
        // Spectrum mode also collapses to 0 with no cores.
        assert_eq!(percore_height(0, 200, 40, true), 0);
    }

    #[test]
    fn percore_height_fits_in_one_row_when_wide_enough() {
        // 4 cores at 11 cols each = 44 cols → fits 1 row in 88 wide.
        assert_eq!(percore_height(4, 88, 40, false), 1);
    }

    #[test]
    fn percore_height_caps_at_two_rows() {
        // 32 cores in an 80-col terminal → 7 cols per row → 5 rows
        // before capping. We cap at 2 to leave the procs body room.
        assert_eq!(percore_height(32, 80, 40, false), 2);
    }

    #[test]
    fn percore_height_handles_narrow_terminal() {
        // 1 col-per-cell minimum; 2 cores in a 5-col terminal still
        // returns at least 1 row, never panics.
        assert!(percore_height(2, 5, 40, false) >= 1);
    }

    #[test]
    fn percore_height_spectrum_one_row_per_core_plus_axis_with_room() {
        // 8 cores in a tall terminal: every core gets a row + 1
        // for the time-axis tick label = 9 rows.
        assert_eq!(percore_height(8, 200, 60, true), 9);
    }

    #[test]
    fn percore_height_spectrum_caps_at_third_of_terminal() {
        // 32 cores in a 24-row terminal: cap at 24/3 = 8 rows so
        // the procs body keeps two-thirds of the screen.
        assert_eq!(percore_height(32, 200, 24, true), 8);
    }

    #[test]
    fn cpu_load_color_steps() {
        // Four-stop ramp shared by sparkline cells, live %, and
        // gauge fill. Idle (≤19 %) is dark grey so quiet cores
        // recede; the upper breakpoints match cpu_glyph_color.
        assert!(matches!(cpu_load_color(0.0), Color::DarkGray));
        assert!(matches!(cpu_load_color(19.0), Color::DarkGray));
        assert!(matches!(cpu_load_color(20.0), Color::Green));
        assert!(matches!(cpu_load_color(49.0), Color::Green));
        assert!(matches!(cpu_load_color(50.0), Color::Yellow));
        assert!(matches!(cpu_load_color(79.0), Color::Yellow));
        assert!(matches!(cpu_load_color(80.0), Color::Red));
        assert!(matches!(cpu_load_color(100.0), Color::Red));
    }

    #[test]
    fn gauge_cells_round_to_nearest() {
        // 0% empty.
        let s = gauge_cells(0.0, 10, Color::Green);
        let total: usize = s.iter().map(|sp| sp.content.chars().count()).sum();
        assert_eq!(total, 10);
        assert_eq!(s[0].content.as_ref(), "");
        // 50% gives exactly 5 filled out of 10.
        let s = gauge_cells(50.0, 10, Color::Green);
        assert_eq!(s[0].content.chars().count(), 5);
        assert_eq!(s[1].content.chars().count(), 5);
        // 100% fully filled, no empties.
        let s = gauge_cells(100.0, 10, Color::Red);
        assert_eq!(s[0].content.chars().count(), 10);
        assert_eq!(s[1].content.as_ref(), "");
        // Out-of-range values clamp rather than panic.
        let s = gauge_cells(-50.0, 8, Color::Green);
        assert_eq!(s[0].content.as_ref(), "");
        let s = gauge_cells(150.0, 8, Color::Red);
        assert_eq!(s[0].content.chars().count(), 8);
    }

    #[test]
    fn spectrum_row_left_pads_short_ring() {
        // A ring with 5 samples drawn in 10 spark cells should be
        // left-padded with 5 spaces so newly-launched neotop
        // doesn't render right-justified.
        let mut ring: VecDeque<u64> = VecDeque::new();
        for v in [10, 20, 30, 40, 50] {
            ring.push_back(v);
        }
        let line = spectrum_row(0, &ring, Some(50.0), 10);
        // First span is the label; spans 1..=5 should be the
        // five blank pad cells before the bar glyphs start.
        let pads: usize = line
            .spans
            .iter()
            .skip(1)
            .take(5)
            .filter(|s| s.content.as_ref() == " ")
            .count();
        assert_eq!(pads, 5);
    }

    #[test]
    fn spectrum_axis_row_widths_match_sparkline() {
        // Axis row layout: 5 spaces (label column) + "-Ns " + dashes
        // + " now". The total visible width past the label must
        // equal the sparkline width so the tick lines up.
        let line = spectrum_axis_row(40);
        let visible_after_label: usize = line
            .spans
            .iter()
            .skip(1)
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(visible_after_label, 40);
        // Empty sparkline still renders without panicking.
        let line = spectrum_axis_row(0);
        assert!(!line.spans.is_empty());
    }

    #[test]
    fn host_history_per_core_resets_on_topology_change() {
        // First push: 4 cores. All four rings get one sample.
        let mut h = HostHistory::default();
        h.push_per_core(&[10.0, 20.0, 30.0, 40.0]);
        assert_eq!(h.per_core.len(), 4);
        assert_eq!(h.per_core[0].len(), 1);

        // Topology changes (simulated CPU hotplug from 4 → 2).
        // The Vec resets; we don't keep stale rings around that
        // would index OOB or bleed across topologies.
        h.push_per_core(&[50.0, 60.0]);
        assert_eq!(h.per_core.len(), 2);
        assert_eq!(h.per_core[0].len(), 1);
        assert_eq!(h.per_core[0].back().copied(), Some(50));
    }

    #[test]
    fn host_history_per_core_caps_at_history_length() {
        let mut h = HostHistory::default();
        // Push CPU_HISTORY_CAP + 5 samples for a single core.
        for i in 0..(CPU_HISTORY_CAP + 5) {
            #[allow(clippy::cast_precision_loss)]
            let v = (i % 100) as f64;
            h.push_per_core(&[v]);
        }
        // Ring buffer evicts old samples; length stays at the cap.
        assert_eq!(h.per_core[0].len(), CPU_HISTORY_CAP);
    }

    #[test]
    fn percore_height_spectrum_floor_at_four() {
        // Even on a tiny 6-row terminal, we still try to give the
        // user 4 rows of spectrum (3 cores + axis) rather than
        // collapsing into nubs. A one-core "spectrum" is just a
        // sparkline you can already see in the strip below.
        assert_eq!(percore_height(8, 200, 6, true), 4);
    }

    #[test]
    fn host_overview_rows_grows_with_gpu_presence() {
        let no_gpu: Vec<gpu::Gpu> = Vec::new();
        assert_eq!(host_overview_rows(&no_gpu), 3);

        let one_gpu = vec![gpu::Gpu {
            vendor: gpu::GpuVendor::Amd,
            name: "RX 7900 XTX".into(),
            busy_pct: Some(40.0),
            vram_used: 0,
            vram_total: 0,
            power_watts: None,
            pci_addr: None,
        }];
        assert_eq!(host_overview_rows(&one_gpu), 4);
    }

    #[test]
    fn scale_clamps_to_bar_width() {
        // 50% of a 100-wide bar = 50 cells.
        assert_eq!(scale(500, 1000, 100), 50);
        // Total 0 → never panics, returns 0.
        assert_eq!(scale(500, 0, 100), 0);
        // Saturation: n exceeding total is clamped to bar_w.
        assert_eq!(scale(2000, 1000, 100), 100);
    }

    #[test]
    fn scale_avoids_overflow_on_terabyte_systems() {
        // 16 TiB / 32 TiB on a 200-cell bar should give 100 cells
        // without overflowing the multiplication.
        let half_tib = 16_u64 * 1024 * 1024 * 1024 * 1024;
        let full_tib = 32_u64 * 1024 * 1024 * 1024 * 1024;
        assert_eq!(scale(half_tib, full_tib, 200), 100);
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
