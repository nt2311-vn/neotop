//! neotop — Cross-platform TUI for host metrics, processes, and GPU activity.
//!
//! Per-core CPU spectrum, NVIDIA + AMD GPU charts, container /
//! runtime / system / native process grouping. See README for
//! controls and architecture.
//!
//! Platform-specific modules are conditionally compiled:
//! - Linux: full feature set including KVM VM monitoring
//! - macOS: process monitoring (VM features disabled)

mod battery;
mod disk;
#[cfg(target_os = "linux")]
mod elf;
#[cfg(target_os = "macos")]
mod elf;
mod errors;
mod gpu;
mod groups;
mod host;
mod kvm;
mod net;
mod passthrough;
mod proc;
mod procs;
mod temp;
mod theme;
mod topology;
mod vcpus;
mod vm;

use std::collections::{HashMap, VecDeque};
use std::io;
use std::time::{Duration, Instant};

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

// -----------------------------------------------------------------------------
// CLI
// -----------------------------------------------------------------------------

struct Args {
    refresh: Duration,
    config_path: Option<std::path::PathBuf>,
}

impl Args {
    fn parse() -> Result<Self> {
        // 1 Hz default; `+` / `-` retune at runtime down to 50 ms.
        let mut refresh_ms: u64 = 1000;

        let mut config_path: Option<std::path::PathBuf> = None;

        let mut it = std::env::args().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
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
                "--config" => {
                    config_path = Some(std::path::PathBuf::from(
                        it.next().context("--config requires a path")?,
                    ));
                }
                other => anyhow::bail!("unknown arg: {other}"),
            }
        }

        Ok(Self {
            refresh: Duration::from_millis(refresh_ms),
            config_path,
        })
    }
}

fn print_help() {
    println!(
        "neotop — Linux TUI for host metrics, processes, and GPU activity\n\
         \n\
         USAGE:\n    \
             neotop [--refresh-ms <n>] [--config <path>]\n\
         \n\
         CONTROLS:\n    \
             q            quit\n    \
             ?            keybindings overlay\n    \
             j / Down     next row\n    \
             k / Up       prev row\n    \
             PgDn / PgUp  jump 10 rows\n    \
             r            refresh immediately\n    \
             + / -        speed up / slow down refresh tick\n    \
             space        pause / resume the live tick\n    \
             s            cycle sort: CPU → MEM → PID → CMD\n    \
             t            toggle tree view\n    \
             g            toggle group view\n    \
             H            toggle per-core CPU spectrum\n    \
             /            enter filter mode\n    \
             K            SIGTERM selected pid (confirmed)\n    \
             Ctrl-K       SIGKILL selected pid (confirmed)\n    \
             T            cycle theme (dark/light/monokai/tty)"
    );
}

/// 60 samples × 1 s tick = last minute of CPU / MEM / NET / GPU history.
#[cfg(target_os = "linux")]
const CPU_HISTORY_CAP: usize = 60;

/// Host-level history rings feeding the sparklines. `cpu`, `mem`,
/// `gpu`, `vram` are `0..=100`; `net_*` are raw bytes/sec
/// (auto-scaled max). Empty rings (`gpu` / `vram`) hide their column.
/// `gpu_busy_per_card` and `disk_rate` feed the inline braille
/// mini-charts in the host overview line; keyed by stable device
/// identifier (`pci_addr` or PCI slot for GPU, kernel device name
/// for disks). Devices that go away are pruned in `push_*`.
#[derive(Debug, Default)]
struct HostHistory {
    cpu: VecDeque<u64>,
    mem: VecDeque<u64>,
    net_down: VecDeque<u64>,
    net_up: VecDeque<u64>,
    gpu: VecDeque<u64>,
    vram: VecDeque<u64>,
    per_core: Vec<VecDeque<u64>>,
    gpu_busy_per_card: HashMap<String, VecDeque<u64>>,
    disk_rate: HashMap<String, VecDeque<u64>>,
}

impl HostHistory {
    fn push(
        &mut self,
        cpu_pct: Option<f64>,
        mem_pct: f64,
        net_down_bps: u64,
        net_up_bps: u64,
        gpu_pct: Option<f64>,
        vram_pct: Option<f64>,
    ) {
        push_pct(&mut self.cpu, cpu_pct.unwrap_or(0.0));
        push_pct(&mut self.mem, mem_pct);
        push_raw(&mut self.net_down, net_down_bps);
        push_raw(&mut self.net_up, net_up_bps);
        if let Some(p) = gpu_pct {
            push_pct(&mut self.gpu, p);
        }
        if let Some(p) = vram_pct {
            push_pct(&mut self.vram, p);
        }
    }

    /// Append one sample per core; resize the ring vec if the core
    /// count changed (hotplug).
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

    /// Append one busy% sample per discovered GPU. Cards without a
    /// busy reading don't get a ring (so the chart stays empty
    /// instead of misleading you with zero values). Removed cards
    /// have their rings dropped on the next tick.
    fn push_gpus(&mut self, gpus: &[gpu::Gpu]) {
        let mut keep: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(gpus.len());
        for g in gpus {
            let Some(busy) = g.busy_pct else { continue };
            let key = gpu_key(g);
            keep.insert(key.clone());
            let ring = self
                .gpu_busy_per_card
                .entry(key)
                .or_insert_with(|| VecDeque::with_capacity(CPU_HISTORY_CAP));
            push_pct(ring, busy);
        }
        self.gpu_busy_per_card.retain(|k, _| keep.contains(k));
    }

    /// Append one combined-rate sample per disk (read + write).
    fn push_disks(&mut self, disks: &[disk::Disk]) {
        let mut keep: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(disks.len());
        for d in disks {
            keep.insert(d.name.clone());
            let ring = self
                .disk_rate
                .entry(d.name.clone())
                .or_insert_with(|| VecDeque::with_capacity(CPU_HISTORY_CAP));
            let r = d.read_bps.unwrap_or(0);
            let w = d.write_bps.unwrap_or(0);
            push_raw(ring, r.saturating_add(w));
        }
        self.disk_rate.retain(|k, _| keep.contains(k));
    }
}

/// Stable identifier for a GPU history ring. Prefer the PCI
/// address (survives reorderings) and fall back to the device name
/// when sysfs didn't expose it.
fn gpu_key(g: &gpu::Gpu) -> String {
    g.pci_addr.clone().unwrap_or_else(|| g.name.clone())
}

/// 8-cell braille mini line chart (Unicode block U+2800..U+28FF).
/// Each cell encodes 2 horizontal samples × 4 vertical levels via
/// the 8-dot Braille pattern, so `cells` characters draw `cells*2`
/// samples; older on the left, newest on the right. `max` defines
/// the y-axis ceiling — caller passes 100 for percentages or
/// `slice.iter().max()` for auto-scaled rates.
fn braille_line(samples: &[u64], max: u64, cells: usize) -> String {
    const DOTS_LEFT: [u8; 4] = [0x01, 0x02, 0x04, 0x40];
    const DOTS_RIGHT: [u8; 4] = [0x08, 0x10, 0x20, 0x80];
    if cells == 0 {
        return String::new();
    }
    let want = cells * 2;
    let start = samples.len().saturating_sub(want);
    let tail = &samples[start..];
    let pad = want - tail.len();
    let level = |v: u64| -> Option<usize> {
        if max == 0 {
            return None;
        }
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )]
        let row = ((v.min(max) as f64) / (max as f64) * 3.999) as usize;
        Some(3 - row.min(3))
    };
    let mut out = String::with_capacity(cells * 3);
    for c in 0..cells {
        let li = c * 2;
        let ri = li + 1;
        let l = li.checked_sub(pad).and_then(|i| tail.get(i)).copied();
        let r = ri.checked_sub(pad).and_then(|i| tail.get(i)).copied();
        let mut bits: u32 = 0x2800;
        if let Some(v) = l {
            if let Some(row) = level(v) {
                bits |= u32::from(DOTS_LEFT[row]);
            }
        }
        if let Some(v) = r {
            if let Some(row) = level(v) {
                bits |= u32::from(DOTS_RIGHT[row]);
            }
        }
        out.push(char::from_u32(bits).unwrap_or(' '));
    }
    out
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

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, args.refresh, args.config_path.as_deref());

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

// -----------------------------------------------------------------------------
// App state — what the run loop owns
// -----------------------------------------------------------------------------

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

    #[cfg(target_os = "linux")]
    fn signal(self) -> rustix::process::Signal {
        match self {
            Self::Term => rustix::process::Signal::Term,
            Self::Kill => rustix::process::Signal::Kill,
        }
    }
}

/// Self-profiling counters surfaced in the footer.
#[derive(Debug, Clone, Copy, Default)]
struct Perf {
    scan_ms: f64,
    render_ms: f64,
    refresh_actual_ms: f64,
    own_rss_bytes: u64,
    own_cpu_pct: Option<f64>,
}

const RSS_RETICK_EVERY: u32 = 4;

#[derive(Debug, Default)]
struct PerfTracker {
    perf: Perf,
    last_scan_started: Option<Instant>,
    own_prev: Option<(Instant, u64)>,
    rss_tick: u32,
}

struct App {
    input: InputMode,

    // Process list
    procs_tracker: procs::Tracker,
    passwd: procs::PasswdCache,
    procs_all: Vec<procs::ProcessRow>,
    /// Rendered process rows after sort + filter (or tree expansion).
    procs_visible: Vec<ProcRender>,
    procs_table: TableState,
    procs_sort: procs::SortBy,
    procs_filter: String,
    /// docker / podman ps cache (off-thread).
    container_names: groups::ContainerNames,
    /// `Flat` (htop-style), `Tree` (parent → children), or `Group`
    /// (clustered by container / runtime / system / native).
    list_mode: ListMode,
    /// When true, the per-core panel renders the spectrum
    /// (sparkline + % + gauge per core); toggled with `H`.
    per_core_spectrum: bool,

    // Host overview
    prev_host_cpu: host::CpuSamples,
    host_info: host::HostInfo,
    net_tracker: net::Tracker,
    ifaces: Vec<net::Iface>,
    // Off-thread; some hwmon sensors block for seconds on first read.
    temp_worker: temp::TempWorker,
    batteries: Vec<battery::Battery>,
    disk_tracker: disk::Tracker,
    disks: Vec<disk::Disk>,
    gpu_tracker: gpu::Tracker,
    gpus: Vec<gpu::Gpu>,
    host_history: HostHistory,
    cpu_topology: topology::CpuTopology,

    // Per-vCPU tracker; only snapshot for the *selected* VM each
    // tick so a host with 50+ guests doesn't melt /proc.
    vcpu_tracker: vcpus::Tracker,
    selected_vcpus: Vec<vcpus::VcpuStat>,

    // KVM exit-counter tracker; same selection-scoped pattern as
    // `vcpu_tracker`. `selected_kvm` is `Some` only when the
    // selection is a VM *and* the tracker is enabled (root-only
    // debugfs); a `None` means "draw a `(debugfs not readable)`
    // hint instead of the kvm exits block".
    kvm_tracker: kvm::Tracker,
    selected_kvm: Option<kvm::KvmRates>,

    // VFIO + vhost + tap snapshot for the selected VM. Walked from
    // /proc/<pid>/fd; cheap (~one readlink per fd) so we refresh
    // it on every selection-tick alongside vCPUs and KVM rates.
    // None until the cursor lands on a VM row.
    selected_passthrough: Option<passthrough::Passthrough>,

    // Per-VM CPU% sparkline ring. Populated only while a VM row is
    // selected — switching to a different VM (or off VMs entirely)
    // resets it. Same `CPU_HISTORY_CAP`-sized ring as the host
    // chart it replaces. Stored as the *mean* of per-vCPU CPU%
    // (0..=100) so the sparkline scale matches the host one.
    vm_cpu_history: VecDeque<u64>,
    /// PID the ring above belongs to. `None` when the cursor isn't
    /// on a VM. Used to detect selection changes and clear the
    /// ring so we don't paint VM-A's samples onto VM-B's chart.
    vm_cpu_history_pid: Option<i32>,

    // Tunables
    clk_tck: u64,
    last_scan: Instant,
    refresh: Duration,
    /// `0..SLOW_TICK_EVERY` counter; slow scanners fire when it's 0.
    slow_tick_counter: u32,
    paused: bool,
    /// EMA of host CPU% so the line-1 number doesn't jitter.
    smoothed_host_cpu: Option<f64>,

    // Self-profiling
    perf: PerfTracker,

    // Non-fatal parser/IO errors surfaced in the footer.
    errors: errors::ErrorRing,

    // Theme
    theme: theme::Theme,
    theme_preset: theme::ThemePreset,
}

const MIN_REFRESH: Duration = Duration::from_millis(50);
const MAX_REFRESH: Duration = Duration::from_secs(5);

/// Slow scanners (temps / batteries / disks / GPU / container names)
/// fire every Nth fast tick so a 1 s UI tick → ~4 s sensor cadence.
const SLOW_TICK_EVERY: u32 = 4;

impl App {
    fn new(refresh: Duration, config_path: Option<&std::path::Path>) -> Self {
        let clk_tck = proc::clk_tck();
        let mut net_tracker = net::Tracker::default();
        let mut procs_tracker = procs::Tracker::default();
        let passwd = procs::PasswdCache::load();
        let mut errors = errors::ErrorRing::new();
        let prev_host_cpu = host::read_cpu_samples(&mut errors);
        let host_info = host::snapshot(None, &mut errors);
        let ifaces = net_tracker.snapshot(&mut errors);
        // Off-thread temp scanner: prime the first scan; results
        // arrive on later poll() calls so the UI never blocks.
        let mut temp_worker = temp::TempWorker::spawn();
        temp_worker.request();
        let batteries = battery::snapshot();
        let mut disk_tracker = disk::Tracker::default();
        let disks = disk_tracker.snapshot(&mut errors);
        let mut gpu_tracker = gpu::Tracker::default();
        let gpus = gpu_tracker.snapshot();
        let procs_all = procs_tracker.snapshot(&passwd, clk_tck);

        let (theme, theme_preset) = theme::load(config_path);

        let mut procs_table = TableState::default();
        let procs_visible = compute_visible_flat(&procs_all, procs::SortBy::Cpu, "");
        if !procs_visible.is_empty() {
            procs_table.select(Some(0));
        }

        Self {
            input: InputMode::Normal,
            procs_tracker,
            passwd,
            procs_all,
            procs_visible,
            procs_table,
            procs_sort: procs::SortBy::Cpu,
            procs_filter: String::new(),
            list_mode: ListMode::Flat,
            container_names: groups::ContainerNames::default(),
            // Spectrum on by default; `H` swaps to the compact grid.
            per_core_spectrum: true,
            prev_host_cpu,
            host_info,
            net_tracker,
            ifaces,
            temp_worker,
            batteries,
            disk_tracker,
            disks,
            gpu_tracker,
            gpus,
            host_history: HostHistory::default(),
            cpu_topology: topology::CpuTopology::read(),
            vcpu_tracker: vcpus::Tracker::new(clk_tck),
            selected_vcpus: Vec::new(),
            kvm_tracker: kvm::Tracker::new(),
            selected_passthrough: None,
            selected_kvm: None,
            vm_cpu_history: VecDeque::with_capacity(CPU_HISTORY_CAP),
            vm_cpu_history_pid: None,
            clk_tck,
            last_scan: Instant::now(),
            refresh,
            slow_tick_counter: 0,
            paused: false,
            smoothed_host_cpu: None,
            perf: PerfTracker::default(),
            errors,
            theme,
            theme_preset,
        }
    }

    /// Re-sample every data source feeding the UI.
    fn tick(&mut self) {
        let started = Instant::now();
        if let Some(prev) = self.perf.last_scan_started {
            self.perf.perf.refresh_actual_ms = duration_ms(started.duration_since(prev));
        }
        self.perf.last_scan_started = Some(started);

        // Fast path: host stats + processes every tick.
        self.host_info = host::snapshot(Some(&self.prev_host_cpu), &mut self.errors);
        self.prev_host_cpu = host::read_cpu_samples(&mut self.errors);
        self.ifaces = self.net_tracker.snapshot(&mut self.errors);

        // EMA-smooth host CPU% (same curve as per-pid) so line-1
        // doesn't jitter between consecutive 1 s windows.
        if let Some(new) = self.host_info.cpu_pct {
            let smoothed = match self.smoothed_host_cpu {
                Some(prev) => procs::ema_blend(prev, new),
                None => new,
            };
            self.smoothed_host_cpu = Some(smoothed);
            self.host_info.cpu_pct = Some(smoothed);
        }

        // Drain temp-worker results every tick (slow first scans
        // can finish mid-cycle); route Info / Warn at their tiers.
        if let Some(out) = self.temp_worker.poll() {
            for (kind, msg) in out.infos {
                self.errors.push_info(kind, msg);
            }
            for (kind, msg) in out.errors {
                self.errors.push(kind, msg);
            }
        }

        // Slow path every SLOW_TICK_EVERY ticks (and on tick #0).
        if self.slow_tick_counter == 0 {
            self.temp_worker.request();
            self.batteries = battery::snapshot();
            self.disks = self.disk_tracker.snapshot(&mut self.errors);
            self.gpus = self.gpu_tracker.snapshot();
            self.cpu_topology = topology::CpuTopology::read();
            self.container_names.refresh_if_stale(Instant::now());
            // Drop kvm-tracker entries for VMs that have exited
            // since the last slow tick. Only sees the freshly
            // re-snapshotted procs_all so it picks up reboots and
            // crashes too.
            let alive: Vec<i32> = self.procs_all.iter().map(|r| r.pid).collect();
            self.kvm_tracker.purge_dead(&alive);
        }
        self.slow_tick_counter = (self.slow_tick_counter + 1) % SLOW_TICK_EVERY;

        // Capture cursor PID before re-snapshotting so the cursor
        // sticks to the same process when sort/filter reshuffles.
        let prev_selected_pid = self.selected_proc().map(|r| r.pid);
        self.procs_all = self.procs_tracker.snapshot(&self.passwd, self.clk_tck);
        self.recompute_procs();
        self.reanchor_proc_selection(prev_selected_pid);
        self.clamp_selections();

        // Feed sparklines from the freshly refreshed host_info.
        let mem_pct = mem_used_pct(&self.host_info);
        let (net_down, net_up) = total_net_rates(&self.ifaces);
        let gpu_pct = gpu::aggregate_busy_pct(&self.gpus);
        let vram_pct = gpu::aggregate_vram_pct(&self.gpus);
        self.host_history.push(
            self.host_info.cpu_pct,
            mem_pct,
            net_down,
            net_up,
            gpu_pct,
            vram_pct,
        );
        self.host_history
            .push_per_core(&self.host_info.per_core_pct);
        self.host_history.push_gpus(&self.gpus);
        self.host_history.push_disks(&self.disks);
        self.refresh_selected_vcpus();
        self.update_self_perf(started);

        self.perf.perf.scan_ms = duration_ms(started.elapsed());
        self.last_scan = Instant::now();
    }

    /// Refresh own-CPU% / `VmRSS` for the perf footer.
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

    /// Snapshot per-vCPU stats *only* for the currently selected
    /// row when that row is a VM. Empty otherwise — keeps tracker
    /// state clean and avoids `/proc/<pid>/task` walks for non-VM
    /// selections.
    fn refresh_selected_vcpus(&mut self) {
        let Some(row) = self.selected_proc() else {
            self.selected_vcpus.clear();
            self.selected_kvm = None;
            self.selected_passthrough = None;
            self.clear_vm_cpu_history();
            return;
        };
        if let groups::Group::Vm(v) = &row.group {
            let pid = row.pid;
            let hv = v.hypervisor;
            self.selected_vcpus = self.vcpu_tracker.snapshot(pid, hv);
            // KVM rates only land on the *second* tick after a VM
            // is selected — first call seeds the prev sample. The
            // detail pane renders "—" until rates start flowing.
            self.selected_kvm = self.kvm_tracker.snapshot(pid);
            // Passthrough snapshot is one-shot per tick — no
            // smoothing needed, the device list changes only when
            // the guest reboots or hot-plugs.
            self.selected_passthrough = Some(passthrough::snapshot(pid));
            // Per-VM CPU sparkline (Phase 5): replace the host
            // sparkline with a per-guest one while a VM row is
            // selected. We push the *mean* of per-vCPU CPU% so the
            // sparkline scale stays 0..=100 and reads the same way
            // the host chart does. Switching VMs resets the ring.
            if self.vm_cpu_history_pid != Some(pid) {
                self.vm_cpu_history.clear();
                self.vm_cpu_history_pid = Some(pid);
            }
            let mean = vm_mean_cpu_pct(&self.selected_vcpus);
            push_pct(&mut self.vm_cpu_history, mean);
        } else {
            self.selected_vcpus.clear();
            self.selected_kvm = None;
            self.selected_passthrough = None;
            self.clear_vm_cpu_history();
        }
    }

    fn clear_vm_cpu_history(&mut self) {
        self.vm_cpu_history.clear();
        self.vm_cpu_history_pid = None;
    }

    fn recompute_procs(&mut self) {
        // Sort + filter apply in all three modes; tree mode keeps
        // the parent→child shape and only reorders siblings.
        self.procs_visible = match self.list_mode {
            ListMode::Tree => {
                compute_visible_tree(&self.procs_all, self.procs_sort, &self.procs_filter)
            }
            ListMode::Group => compute_visible_grouped(
                &self.procs_all,
                self.procs_sort,
                &self.procs_filter,
                &self.container_names,
                false,
            ),
            ListMode::GroupTree => compute_visible_grouped(
                &self.procs_all,
                self.procs_sort,
                &self.procs_filter,
                &self.container_names,
                true,
            ),
            ListMode::Flat => {
                compute_visible_flat(&self.procs_all, self.procs_sort, &self.procs_filter)
            }
        };
    }

    /// Pin the cursor back onto `pid` after rows reshuffle.
    fn reanchor_proc_selection(&mut self, pid: Option<i32>) {
        let Some(pid) = pid else { return };
        if let Some(new_idx) = self.procs_visible.iter().position(|r| {
            r.header.is_none() && self.procs_all.get(r.idx).is_some_and(|row| row.pid == pid)
        }) {
            self.procs_table.select(Some(new_idx));
        }
    }

    fn clamp_selections(&mut self) {
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
        // Synthetic group headers don't map to a real PID.
        if r.header.is_some() {
            return None;
        }
        self.procs_all.get(r.idx)
    }
}

/// Layout choice for the Procs body. `t` toggles tree, `g` toggles
/// grouping; the two compose, so `GroupTree` is the four-way
/// product. Group sorts by aggregate CPU/MEM; tree-within-group
/// preserves parent→child shape inside each cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListMode {
    Flat,
    Tree,
    Group,
    GroupTree,
}

impl ListMode {
    fn toggle_tree(self) -> Self {
        match self {
            Self::Flat => Self::Tree,
            Self::Tree => Self::Flat,
            Self::Group => Self::GroupTree,
            Self::GroupTree => Self::Group,
        }
    }
    fn toggle_group(self) -> Self {
        match self {
            Self::Flat => Self::Group,
            Self::Group => Self::Flat,
            Self::Tree => Self::GroupTree,
            Self::GroupTree => Self::Tree,
        }
    }
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
    names: &groups::ContainerNames,
    as_tree: bool,
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

    // CPU / MEM: float the busiest group to the top regardless of
    // band. PID / Command: keep band priority (container > vm >
    // runtime > system > native) and fall back to alphabetic so
    // the layout stays stable when the sort key isn't an aggregate.
    let mut group_keys: Vec<String> = buckets.keys().cloned().collect();
    group_keys.sort_by(|a, b| {
        let ag = &buckets[a];
        let bg = &buckets[b];
        match by {
            procs::SortBy::Cpu => sum_cpu(rows, bg)
                .partial_cmp(&sum_cpu(rows, ag))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cmp(b)),
            procs::SortBy::Mem => sum_rss(rows, bg)
                .cmp(&sum_rss(rows, ag))
                .then_with(|| a.cmp(b)),
            procs::SortBy::Pid | procs::SortBy::Command => a.cmp(b),
        }
    });

    let mut out: Vec<ProcRender> =
        Vec::with_capacity(buckets.values().map(Vec::len).sum::<usize>() + buckets.len());
    for key in group_keys {
        let mut members = buckets.remove(&key).unwrap_or_default();
        members.sort_by(|&a, &b| cmp_rows(rows, a, b, by));
        let group = group_for.remove(&key).unwrap_or(groups::Group::Native);
        // Skip the synthetic banner for the catch-all `system` and
        // `native` bands. Their aggregate CPU / RSS totals tend to
        // dwarf every other group (every kernel / daemon / static
        // binary on the host lands in one of them) and the user
        // reads that sum as a "real" workload — but it isn't a
        // meaningful one. Render their members as flat rows in the
        // same slot the header would have occupied; band ordering
        // is preserved.
        let suppress_header = matches!(
            group.band(),
            groups::GroupBand::System | groups::GroupBand::Native
        );
        if !suppress_header {
            let header = GroupHeader {
                // Prefer the human-readable name when the docker/podman
                // ps cache has resolved it; falls back to the 12-char
                // hash when the daemon isn't reachable or the container
                // hasn't been polled yet.
                label: group.label_with_names(names),
                band: group.band(),
                count: members.len(),
                total_cpu: sum_cpu(rows, &members),
                total_rss: sum_rss(rows, &members),
            };
            out.push(ProcRender {
                idx: usize::MAX,
                prefix: String::new(),
                header: Some(header),
            });
        }
        let prefix = if suppress_header { "" } else { "  " };
        if as_tree {
            emit_group_as_tree(rows, &members, by, &mut out, prefix);
        } else {
            for idx in members {
                out.push(ProcRender {
                    idx,
                    prefix: prefix.to_string(),
                    header: None,
                });
            }
        }
    }
    out
}

/// Build a forest restricted to one group's members. Roots are
/// pids whose parent isn't part of the same bucket — that lets a
/// container's `nginx: master` show its `nginx: worker` children
/// nested below it without dragging in unrelated host processes.
/// Each row is prefixed with `"  "` so the tree sits clearly under
/// the group header.
fn emit_group_as_tree(
    rows: &[procs::ProcessRow],
    members: &[usize],
    by: procs::SortBy,
    out: &mut Vec<ProcRender>,
    prefix: &str,
) {
    use std::collections::HashSet;

    let pid_set: HashSet<i32> = members.iter().map(|&i| rows[i].pid).collect();
    let mut children: HashMap<i32, Vec<usize>> = HashMap::new();
    for &i in members {
        children.entry(rows[i].ppid).or_default().push(i);
    }
    for kids in children.values_mut() {
        kids.sort_by(|&a, &b| cmp_rows(rows, a, b, by));
    }
    let alive: HashSet<usize> = members.iter().copied().collect();

    // A member is a root *within this group* when its parent isn't
    // in the same group bucket.
    let mut roots: Vec<usize> = members
        .iter()
        .copied()
        .filter(|&i| !pid_set.contains(&rows[i].ppid))
        .collect();
    roots.sort_by(|&a, &b| cmp_rows(rows, a, b, by));

    let mut ancestor_last: Vec<bool> = Vec::new();
    let total = roots.len();
    let mut local: Vec<ProcRender> = Vec::with_capacity(members.len());
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
            &mut local,
        );
    }
    // Indent the whole sub-forest under the group header (when one
    // exists). For headerless bands (system/native) `prefix` is
    // empty so members render flush-left like flat mode.
    for mut r in local {
        r.prefix = format!("{prefix}{}", r.prefix);
        out.push(r);
    }
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
    refresh: Duration,
    config_path: Option<&std::path::Path>,
) -> Result<()> {
    let mut app = App::new(refresh, config_path);

    loop {
        // Drain queued input then redraw once — collapses a burst
        // of held-key repeats into a single redraw at the final pos.
        while let Some(k) = poll_key(Duration::ZERO)? {
            if handle_key(&mut app, k) {
                return Ok(());
            }
        }

        let render_started = Instant::now();
        terminal.draw(|f| draw(f, &mut app))?;
        app.perf.perf.render_ms = duration_ms(render_started.elapsed());

        // Block until next key OR refresh interval elapses. Idle ~0 CPU.
        let elapsed = app.last_scan.elapsed();
        let wait = app.refresh.saturating_sub(elapsed);
        if let Some(k) = poll_key(wait)? {
            if handle_key(&mut app, k) {
                return Ok(());
            }
        }

        // Paused: input still flows, but tick() is skipped. Bump
        // last_scan so unpause fires the next tick immediately.
        if app.paused {
            app.last_scan = Instant::now();
        } else if app.last_scan.elapsed() >= app.refresh {
            app.tick();
        }
    }
}

/// Wait up to `timeout` for a key *press*. Filters out Repeat /
/// Release so kitty-protocol terminals don't double-fire actions.
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
fn handle_key(app: &mut App, k: crossterm::event::KeyEvent) -> bool {
    // `q` quits everywhere except Filter (where it's typed text).
    if matches!(k.code, KeyCode::Char('q')) && !matches!(app.input, InputMode::Filter) {
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
    // `?` / `Esc` / `Enter` dismiss; other keys swallowed.
    if matches!(k.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Enter) {
        app.input = InputMode::Normal;
    }
}

fn handle_normal_key(app: &mut App, k: crossterm::event::KeyEvent) {
    match k.code {
        KeyCode::Char('?') => {
            app.input = InputMode::Help;
        }
        KeyCode::Char(' ') => {
            app.paused = !app.paused;
        }
        KeyCode::Char('+' | '=') => {
            app.refresh = (app.refresh / 2).max(MIN_REFRESH);
        }
        KeyCode::Char('-' | '_') => {
            app.refresh = (app.refresh.saturating_mul(2)).min(MAX_REFRESH);
        }
        KeyCode::Char('j') | KeyCode::Down => move_selection(app, 1),
        // Ctrl-k is SIGKILL; must match before the bare `k` nav arm.
        // The selection check rides as a guard so the arm doesn't
        // swallow the keypress when nothing's selected — the `_ =>`
        // fallthrough still ignores it but at least the intent
        // reads on one line.
        KeyCode::Char('k')
            if k.modifiers.contains(KeyModifiers::CONTROL) && app.selected_proc().is_some() =>
        {
            app.input = InputMode::Confirm(KillSig::Kill);
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
        // Sort cycle.
        // (s, t, g, H, /, K bodies follow.)
        KeyCode::Char('s') => {
            let pinned = app.selected_proc().map(|r| r.pid);
            app.procs_sort = app.procs_sort.next();
            app.recompute_procs();
            app.reanchor_proc_selection(pinned);
            app.clamp_selections();
        }
        KeyCode::Char('t') => {
            // Toggle the tree axis; preserves grouping when on.
            let pinned = app.selected_proc().map(|r| r.pid);
            app.list_mode = app.list_mode.toggle_tree();
            app.recompute_procs();
            app.reanchor_proc_selection(pinned);
            app.clamp_selections();
        }
        KeyCode::Char('g') => {
            // Toggle the group axis; preserves tree when on.
            let pinned = app.selected_proc().map(|r| r.pid);
            app.list_mode = app.list_mode.toggle_group();
            app.recompute_procs();
            app.reanchor_proc_selection(pinned);
            app.clamp_selections();
        }
        KeyCode::Char('H') => {
            // Spectrum ↔ compact grid. Capital H to avoid shadowing
            // a future vim-style left-motion key.
            app.per_core_spectrum = !app.per_core_spectrum;
        }
        KeyCode::Char('/') => {
            app.input = InputMode::Filter;
        }
        // Shift+K = SIGTERM (Ctrl+K = SIGKILL handled above).
        KeyCode::Char('K') if app.selected_proc().is_some() => {
            app.input = InputMode::Confirm(KillSig::Term);
        }
        // T cycles theme: dark → light → monokai → tty → dark.
        KeyCode::Char('T') => {
            let next = app.theme_preset.next();
            app.theme = next.colors();
            app.theme_preset = next;
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
        // Skip Ctrl/Alt-modified Chars (already routed upstream) by
        // riding the modifier check as a match guard — clippy's
        // collapsible_match was complaining about the nested `if`.
        KeyCode::Char(c)
            if !k.modifiers.contains(KeyModifiers::CONTROL)
                && !k.modifiers.contains(KeyModifiers::ALT) =>
        {
            let pinned = app.selected_proc().map(|r| r.pid);
            app.procs_filter.push(c);
            app.recompute_procs();
            app.reanchor_proc_selection(pinned);
            app.clamp_selections();
        }
        _ => {}
    }
}

#[cfg(not(target_os = "linux"))]
fn handle_confirm_key(app: &mut App, _k: crossterm::event::KeyEvent, _sig: KillSig) {
    app.input = InputMode::Normal;
}

#[cfg(target_os = "linux")]
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
    let len = app.procs_visible.len();
    if len == 0 {
        return;
    }
    let cur = i64::try_from(app.procs_table.selected().unwrap_or(0)).unwrap_or(0);
    let max = i64::try_from(len.saturating_sub(1)).unwrap_or(0);
    let next = (cur + i64::from(delta)).clamp(0, max);
    let next_us = usize::try_from(next).unwrap_or(0);
    app.procs_table.select(Some(next_us));
}

// -----------------------------------------------------------------------------
// Rendering
// -----------------------------------------------------------------------------

fn draw(f: &mut ratatui::Frame<'_>, app: &mut App) {
    draw_main(f, app);
    // Help overlay paints last so it sits on top of the table.
    if matches!(app.input, InputMode::Help) {
        draw_help_overlay(f, &app.host_info, &app.theme);
    }
}

/// Centered keybindings popup. Carries the static "about this
/// machine" block (kernel + CPU model).
fn draw_help_overlay(f: &mut ratatui::Frame<'_>, h: &host::HostInfo, theme: &theme::Theme) {
    let area = centered_rect(64, 28, f.area());
    f.render_widget(Clear, area);

    let dim = Style::default().fg(theme.label);
    let kb = Style::default().fg(theme.badge_fg).bg(theme.filter_bg);
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
        kv_line("  ? ", "toggle this help", kb, dim),
        kv_line("  r ", "force an immediate refresh", kb, dim),
        kv_line("  + / -", "speed up / slow down the refresh tick", kb, dim),
        kv_line("  space", "pause / resume the live tick", kb, dim),
        kv_line("  j / k", "move selection (also ↓/↑)", kb, dim),
        kv_line("  PgDn / PgUp", "jump 10 rows", kb, dim),
        Line::from(""),
        Line::from(Span::styled(
            "  Process list",
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
                .fg(theme.label)
                .add_modifier(Modifier::ITALIC),
        )),
    ];

    let block = Block::default().borders(Borders::ALL).title(Span::styled(
        " neotop · keybindings ",
        Style::default()
            .fg(theme.badge_fg)
            .bg(theme.badge_bg)
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

/// Standard ratatui centered-rect popup helper.
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

fn draw_main(f: &mut ratatui::Frame<'_>, app: &mut App) {
    let area = f.area();
    let percore_h = percore_height(
        app.host_info.per_core_pct.len(),
        area.width,
        area.height,
        app.per_core_spectrum,
    );
    let host_h = host_overview_rows(&app.gpus);
    // Memory bar needs 3 rows; drop on terminals < 22 rows.
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
        app.temp_worker.readings(),
        &app.batteries,
        &app.disks,
        &app.gpus,
        &app.host_history,
        &app.theme,
    );
    if percore_h > 0 {
        if app.per_core_spectrum {
            draw_per_core_spectrum(
                f,
                chunks[2],
                &app.host_history.per_core,
                &app.host_info.per_core_pct,
                &app.theme,
                &app.cpu_topology,
            );
        } else {
            draw_per_core(f, chunks[2], &app.host_info.per_core_pct, &app.theme);
        }
    }
    if mem_bar_h > 0 {
        draw_mem_bar(f, chunks[3], &app.host_info, &app.theme);
    }
    // Per-VM CPU sparkline preempts the host one when a VM is
    // selected (Phase 5). `vm_cpu_history` is empty otherwise, so
    // the renderer just keeps its host-mode default.
    let vm_overlay = if app.vm_cpu_history.is_empty() {
        None
    } else {
        let name = app
            .selected_proc()
            .and_then(|r| match &r.group {
                groups::Group::Vm(v) => Some(v.label()),
                _ => None,
            })
            .unwrap_or_else(|| "VM".to_string());
        Some((name, &app.vm_cpu_history))
    };
    draw_host_history(
        f,
        chunks[4],
        &app.host_history,
        &app.ifaces,
        &app.gpus,
        &app.theme,
        vm_overlay,
    );

    // Detail pane only when wide enough; below ~110 cols the
    // table needs every column to stay readable.
    let body = chunks[5];
    if body.width >= 110 {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(60), Constraint::Length(48)])
            .split(body);
        let selected_pid = app.selected_proc().map(|r| i64::from(r.pid));
        draw_proc_table(f, split[0], app);
        draw_proc_detail(
            f,
            split[1],
            selected_pid,
            app.selected_proc(),
            &app.container_names,
            &app.selected_vcpus,
            app.selected_kvm,
            app.kvm_tracker.is_available(),
            app.selected_passthrough.as_ref(),
            &app.ifaces,
            &app.theme,
        );
    } else {
        draw_proc_table(f, body, app);
    }
    draw_footer(f, chunks[6], app);
}

/// Per-core row ≈ 11 cols; grid caps at 2 rows so the body keeps space.
const PERCORE_CELL_W: u16 = 11;
const PERCORE_MAX_ROWS: u16 = 2;

/// 3 rows base, 4 when at least one GPU is detected.
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
        let cores_per_row = spectrum_cores_per_row(width, num_cores);
        let rows = num_cores.div_ceil(cores_per_row.max(1));
        let want = u16::try_from(rows.saturating_add(1)).unwrap_or(u16::MAX);
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
    theme: &theme::Theme,
    topology: &topology::CpuTopology,
) {
    if rings.is_empty() || area.height == 0 || area.width <= SPECTRUM_FIXED_W {
        return;
    }
    let cols = spectrum_cores_per_row(area.width, rings.len());
    let cols_u16 = u16::try_from(cols).unwrap_or(1).max(1);
    let col_w = area.width / cols_u16;
    if col_w <= SPECTRUM_FIXED_W {
        return;
    }
    let spark_w = (col_w - SPECTRUM_FIXED_W) as usize;
    if spark_w == 0 {
        return;
    }

    // Build an ordered list of logical CPU indices to render.
    // When topology is available, use NUMA-ordered SMT groups so
    // siblings are adjacent and NUMA boundaries are marked.
    // Fall back to linear order when topology has no data or the
    // ring count doesn't match (e.g., hotplug, first tick).
    let numa_groups = topology.numa_groups();
    let use_topology = !numa_groups.is_empty() && topology.len() == rings.len();

    let row_budget = (area.height as usize).saturating_sub(1);
    let mut lines: Vec<Line<'static>> = Vec::new();

    if use_topology {
        let is_numa = topology.is_numa();
        let mut rows_used = 0usize;
        'outer: for (node_idx, (node_id, smt_groups)) in numa_groups.iter().enumerate() {
            // NUMA separator before every node except the first.
            if is_numa && node_idx > 0 && rows_used < row_budget {
                let sep = format!(" \u{2500}\u{2500} NUMA {node_id} \u{2500}\u{2500}");
                lines.push(Line::from(Span::styled(
                    sep,
                    Style::default().fg(theme.label),
                )));
                rows_used += 1;
            }
            // Flatten SMT groups: each group may have 1 (no HT) or 2+ logical CPUs.
            let flat: Vec<(usize, usize)> = smt_groups
                .iter()
                .flat_map(|group| {
                    group
                        .iter()
                        .enumerate()
                        .map(|(sib_idx, &cpu)| (cpu, sib_idx))
                })
                .collect();
            let rows_needed = flat.len().div_ceil(cols);
            let rows_here = row_budget.saturating_sub(rows_used).min(rows_needed);
            for r in 0..rows_here {
                if rows_used >= row_budget {
                    break 'outer;
                }
                let mut spans: Vec<Span<'static>> = Vec::new();
                for c in 0..cols {
                    let idx = c * rows_needed + r;
                    let Some(&(cpu, _sib)) = flat.get(idx) else {
                        break;
                    };
                    if cpu >= rings.len() {
                        break;
                    }
                    if c > 0 {
                        spans.push(Span::raw(" "));
                    }
                    spans.extend(spectrum_row_spans(
                        cpu,
                        &rings[cpu],
                        live.get(cpu).copied(),
                        spark_w,
                        theme,
                    ));
                }
                lines.push(Line::from(spans));
                rows_used += 1;
            }
        }
    } else {
        // Linear fallback (same as before topology was added).
        let rows_needed = rings.len().div_ceil(cols);
        let rows = row_budget.min(rows_needed);
        for r in 0..rows {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for c in 0..cols {
                let i = c * rows_needed + r;
                if i >= rings.len() {
                    break;
                }
                if c > 0 {
                    spans.push(Span::raw(" "));
                }
                spans.extend(spectrum_row_spans(
                    i,
                    &rings[i],
                    live.get(i).copied(),
                    spark_w,
                    theme,
                ));
            }
            lines.push(Line::from(spans));
        }
    }

    lines.push(spectrum_axis_row(spark_w, theme));
    f.render_widget(Paragraph::new(lines), area);
}

/// Minimum sparkline width per column when rendering side-by-side.
const SPECTRUM_MIN_SPARK: u16 = 12;

/// How many cores fit side-by-side at this width (1 or 2).
fn spectrum_cores_per_row(width: u16, num_cores: usize) -> usize {
    if num_cores <= 1 {
        return 1;
    }
    let one = u32::from(SPECTRUM_FIXED_W) + u32::from(SPECTRUM_MIN_SPARK);
    let fits = u32::from(width) / one.max(1);
    fits.clamp(1, 2) as usize
}

#[cfg(test)]
fn spectrum_row(
    core_idx: usize,
    ring: &VecDeque<u64>,
    live_pct: Option<f64>,
    spark_w: usize,
    theme: &theme::Theme,
) -> Line<'static> {
    Line::from(spectrum_row_spans(core_idx, ring, live_pct, spark_w, theme))
}

fn spectrum_row_spans(
    core_idx: usize,
    ring: &VecDeque<u64>,
    live_pct: Option<f64>,
    spark_w: usize,
    theme: &theme::Theme,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(spark_w + 8);
    spans.push(Span::styled(
        format!(" c{core_idx:<2} "),
        Style::default().fg(theme.label),
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
            Style::default().fg(cpu_load_color(pct, theme)),
        ));
    }

    // Numeric %. Prefer the smoothed live value the rest of the UI
    // shows; fall back to the latest ring sample if the topology
    // ring just got resized and `live` lags by a tick.
    #[allow(clippy::cast_precision_loss)]
    let cur = live_pct.unwrap_or_else(|| ring.back().copied().unwrap_or(0) as f64);
    let cur_color = cpu_load_color(cur, theme);
    spans.push(Span::styled(
        format!("  {cur:>3.0}% "),
        Style::default().fg(cur_color),
    ));

    // Gauge.
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        "▕".to_string(),
        Style::default().fg(theme.label),
    ));
    spans.extend(gauge_cells(
        cur,
        SPECTRUM_GAUGE_CELLS as usize,
        cur_color,
        theme.gauge_empty,
    ));
    spans.push(Span::styled(
        "▏".to_string(),
        Style::default().fg(theme.label),
    ));
    spans
}

/// Bottom tick row: `-Ns ──────────────── now`, where `N` is the
/// sparkline width in samples (= seconds, since we tick at 1 Hz).
/// Under the label column we leave whitespace so the axis sits
/// flush with the start of the sparkline.
fn spectrum_axis_row(spark_w: usize, theme: &theme::Theme) -> Line<'static> {
    let style = Style::default().fg(theme.label);
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
fn gauge_cells(
    pct: f64,
    cells: usize,
    fill_color: Color,
    empty_color: Color,
) -> Vec<Span<'static>> {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * cells as f64).round() as usize;
    let filled = filled.min(cells);
    vec![
        Span::styled("█".repeat(filled), Style::default().fg(fill_color)),
        Span::styled("░".repeat(cells - filled), Style::default().fg(empty_color)),
    ]
}

/// Four-stop colour ramp shared by the spectrum sparkline cells,
/// the live % readout, and the gauge fill. Idle (≤19 %) is dark
/// grey rather than green so a quiet core fades into the
/// background and the eye is drawn to active cores. Same upper
/// breakpoints as `cpu_glyph_color` so the rest of the UI keeps
/// one mental model.
fn cpu_load_color(pct: f64, theme: &theme::Theme) -> Color {
    theme.cpu_load_color(pct)
}

fn draw_per_core(f: &mut ratatui::Frame<'_>, area: Rect, percore: &[f64], theme: &theme::Theme) {
    let per_row = (area.width / PERCORE_CELL_W).max(1) as usize;
    let max_cells = per_row.saturating_mul(PERCORE_MAX_ROWS as usize);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();

    for (i, &pct) in percore.iter().take(max_cells).enumerate() {
        let bar = bar_glyph(pct);
        let color = cpu_glyph_color(pct, theme);
        spans.push(Span::styled(
            format!(" c{i:<2} "),
            Style::default().fg(theme.label),
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
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn draw_proc_detail(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    pid: Option<i64>,
    row: Option<&procs::ProcessRow>,
    names: &groups::ContainerNames,
    vcpus: &[vcpus::VcpuStat],
    kvm_rates: Option<kvm::KvmRates>,
    kvm_available: bool,
    passthrough: Option<&passthrough::Passthrough>,
    ifaces: &[net::Iface],
    theme: &theme::Theme,
) {
    let block = Block::default().borders(Borders::ALL).title(" detail ");
    let Some(pid) = pid else {
        f.render_widget(Paragraph::new("(no process selected)").block(block), area);
        return;
    };
    let label = Style::default().fg(theme.label);
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
        // THREADS reads as a bare integer for Container / VM /
        // System / Native rows. For the language-runtime band we
        // append the concurrency signature so the user can map
        // "this process has 18 OS threads" → "the Go runtime is
        // multiplexing N goroutines onto 18 threads", or "this
        // JVM has 24 platform threads carrying virtual threads".
        // Tiny addition that turns the static [signature] tag
        // from Phase 2 into something live.
        let threads_value = match &r.group {
            groups::Group::Runtime(lang, _) => {
                format!("{} ({})", r.threads, lang.signature())
            }
            _ => r.threads.to_string(),
        };
        lines.push(kv("THREADS", threads_value, label));
        lines.push(kv("RSS", proc::human_bytes(r.rss_bytes), label));
        // Disk I/O rates from /proc/<pid>/io. `—` when the file
        // isn't readable (foreign uid without CAP_SYS_PTRACE) or
        // we haven't sampled twice yet; blank-ish "0 B/s" when the
        // process simply hasn't touched disk this tick.
        lines.push(kv("DISK R", io_rate_detail(r.read_bps), label));
        lines.push(kv("DISK W", io_rate_detail(r.write_bps), label));
        // GROUP / CONTAINER: developer-meaningful classification.
        // For container processes show a more explicit label so the
        // user knows they can run `docker ps` / `podman ps` to
        // recover the human name; otherwise just the runtime name.
        match &r.group {
            groups::Group::Container(c) => {
                // When `docker ps` / `podman ps` has surfaced a
                // human-readable name, render `docker myapp
                // (abc12345)` so the user gets both the friendly
                // identifier and the hash they need for `docker
                // logs <id>`. Without a resolved name, fall back to
                // the bare hash form.
                let value = match names.lookup(&c.id) {
                    Some(name) => format!("{} {} ({})", c.runtime.label(), name, c.id),
                    None => format!("{}:{}", c.runtime.label(), c.id),
                };
                lines.push(kv("CONTAINER", value, label));
            }
            groups::Group::Vm(v) => {
                lines.push(kv("VM", v.label(), label));
            }
            groups::Group::Runtime(..) | groups::Group::System | groups::Group::Native => {
                lines.push(kv("GROUP", r.group.label(), label));
            }
        }
    }

    // Pull live cgroup + rlimits via the same snapshot used for VMs.
    if let Some(snap) = proc::snapshot(pid) {
        lines.push(kv("VSZ", proc::human_bytes(snap.mem.vsz_bytes), label));
        if let Some(cg) = &snap.cgroup {
            lines.push(section("── cgroup ──", label));
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
                    lines.push(section("── rlimits ──", label));
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

    if !vcpus.is_empty() {
        push_vcpu_lines(&mut lines, vcpus, label, theme);
    }

    // KVM exit-rate block: only relevant when the selected row is
    // a VM. The vcpus list is empty for non-VM rows, so we piggy-
    // back on it as the "is this a VM?" gate without re-checking
    // the group enum.
    if !vcpus.is_empty() {
        push_kvm_lines(&mut lines, kvm_rates, kvm_available, label);
    }

    // VFIO + vhost + tap blocks (Phase 4). Same VM-only gate as
    // the kvm block above: passthrough is `Some` only for VM rows
    // and `is_empty()` skips the section entirely when the guest
    // isn't using any pass-through.
    if let Some(p) = passthrough {
        if !p.is_empty() {
            push_passthrough_lines(&mut lines, p, ifaces, label);
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

/// Mean CPU% across all vCPUs of a VM. Used as the per-VM
/// sparkline sample (Phase 5) so the chart's 0..=100 scale matches
/// the host one. Empty / all-`None` input maps to 0.0 — first-tick
/// samples land in the ring but read as flatlined until the
/// tracker's prev-sample is established.
fn vm_mean_cpu_pct(vcpus: &[vcpus::VcpuStat]) -> f64 {
    let mut sum = 0.0;
    let mut n = 0u32;
    for v in vcpus {
        if let Some(p) = v.cpu_pct {
            sum += p;
            n += 1;
        }
    }
    if n == 0 {
        0.0
    } else {
        sum / f64::from(n)
    }
}

/// `── vcpus ──` block in the detail pane. Each row carries vCPU
/// index, host tid, CPU% (color-ramped), and an inline 8-cell
/// gauge that mirrors the per-core spectrum so guest hot vCPUs
/// jump out the same way host hot cores do.
fn push_vcpu_lines(
    lines: &mut Vec<Line<'static>>,
    vcpus: &[vcpus::VcpuStat],
    label: Style,
    theme: &theme::Theme,
) {
    lines.push(section("── vcpus ──", label));
    for v in vcpus {
        let pct = v
            .cpu_pct
            .map_or_else(|| "—".to_string(), |p| format!("{p:>5.1}%"));
        let color = v.cpu_pct.map_or(theme.label, |p| cpu_load_color(p, theme));
        let key = format!("cpu{}", v.index);
        let mut spans = vec![
            Span::styled(format!("  {key:<8}"), label),
            Span::styled(pct, Style::default().fg(color)),
            Span::styled(format!("  tid {}", v.tid), label),
        ];
        if let Some(p) = v.cpu_pct {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                "▕".to_string(),
                Style::default().fg(theme.label),
            ));
            spans.extend(gauge_cells(p, 8, color, theme.gauge_empty));
            spans.push(Span::styled(
                "▏".to_string(),
                Style::default().fg(theme.label),
            ));
        }
        lines.push(Line::from(spans));
    }
}

/// `── kvm exits ──` block in the detail pane. Each row reports a
/// per-second rate for one VM-exit class. When debugfs isn't
/// readable (the common case for non-root users) the block
/// collapses to a single hint line so the user knows the data
/// exists but they need elevated privileges to see it.
fn push_kvm_lines(
    lines: &mut Vec<Line<'static>>,
    rates: Option<kvm::KvmRates>,
    available: bool,
    label: Style,
) {
    lines.push(section("── kvm exits ──", label));
    if !available {
        lines.push(Line::from(Span::styled(
            "  (run as root for /sys/kernel/debug/kvm)",
            label,
        )));
        return;
    }
    let Some(r) = rates else {
        // First tick after VM selection — tracker has the prev
        // sample now; rates land on the next tick.
        lines.push(Line::from(Span::styled("  (sampling…)", label)));
        return;
    };
    lines.push(kv("exits", per_sec(r.exits), label));
    lines.push(kv("mmio_exits", per_sec(r.mmio_exits), label));
    lines.push(kv("io_exits", per_sec(r.io_exits), label));
    lines.push(kv("halt_exits", per_sec(r.halt_exits), label));
    lines.push(kv("irq_inj", per_sec(r.irq_injections), label));
}

/// `── devices ──` and `── network ──` blocks for VM rows that
/// have any passthrough surface in use. Each VFIO group renders one
/// header line plus one indented row per PCI function. Vhost
/// flavours are listed inline. Tap interfaces are cross-referenced
/// against `ifaces` so the rows carry live rx / tx rates.
fn push_passthrough_lines(
    lines: &mut Vec<Line<'static>>,
    p: &passthrough::Passthrough,
    ifaces: &[net::Iface],
    label: Style,
) {
    if !p.vfio_groups.is_empty() || !p.vhost.is_empty() {
        lines.push(section("── devices ──", label));
        for group in &p.vfio_groups {
            // Empty device list (rare — group existed in /dev/vfio
            // but iommu_groups was unreadable) still earns a line so
            // the user knows the group is open.
            if group.devices.is_empty() {
                lines.push(kv(
                    "vfio",
                    format!("group {} (devices unreadable)", group.group_id),
                    label,
                ));
                continue;
            }
            for (i, dev) in group.devices.iter().enumerate() {
                // Only the first row carries the "vfio:N" key — the
                // rest indent under it for grouped readability when
                // a single IOMMU group spans several functions
                // (e.g. GPU + its HDMI audio sibling).
                let key = if i == 0 {
                    format!("vfio:{}", group.group_id)
                } else {
                    String::new()
                };
                lines.push(kv(&short_left(&key, 10), dev.label(), label));
            }
        }
        for v in &p.vhost {
            lines.push(kv("vhost", v.label().to_string(), label));
        }
    }

    if !p.taps.is_empty() {
        lines.push(section("── network ──", label));
        for tap in &p.taps {
            // Pull live rx / tx from the existing `net::Tracker`
            // snapshot — no extra syscalls. Foreign or freshly-up
            // interfaces show "—" until a second sample lands.
            let rates = ifaces
                .iter()
                .find(|i| i.name == *tap)
                .map(|i| (i.rx_rate, i.tx_rate));
            let value = match rates {
                Some((rx, tx)) => {
                    format!("rx {} · tx {}", net::human_rate(rx), net::human_rate(tx))
                }
                None => "(no counters)".to_string(),
            };
            lines.push(kv(&format!("tap:{tap}"), value, label));
        }
    }
}

/// Left-justify-and-truncate a key for the detail-pane KV column —
/// the layout assumes ≤ 10 chars. Pure presentation helper kept
/// near `push_passthrough_lines` because that's where the rule
/// matters most (BDFs are long).
fn short_left(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        s.chars().take(width).collect()
    }
}

/// Render a per-second rate in the same compact style as the rest
/// of the detail block — `1.2k/s`, `4.7M/s`, `38/s`. Anything below
/// 0.1/s collapses to `—` so flatlined counters don't look like
/// noise.
fn per_sec(v: f64) -> String {
    if v < 0.1 {
        return "—".to_string();
    }
    if v >= 1_000_000.0 {
        format!("{:.1}M/s", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("{:.1}k/s", v / 1_000.0)
    } else {
        format!("{v:.0}/s")
    }
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
fn draw_mem_bar(f: &mut ratatui::Frame<'_>, area: Rect, h: &host::HostInfo, theme: &theme::Theme) {
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

    // Each segment is a solid-fill background span. The byte count
    // sits *inside* its own segment so the bar is self-explanatory
    // — no readout above it. Narrow segments degrade to label-only
    // then to bare fill; the title carries only the totals.
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(8);
    spans.push(seg("used", used, theme.mem_used, theme.badge_fg, used_w));
    spans.push(seg(
        "buf",
        buffers,
        theme.mem_buffers,
        theme.badge_fg,
        buffers_w,
    ));
    spans.push(seg(
        "cache",
        cached,
        theme.mem_cached,
        theme.badge_fg,
        cached_w,
    ));
    spans.push(seg("free", free, theme.mem_free, theme.badge_fg, free_w));

    let title = format!(" memory · {} total ", proc::human_bytes(total));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(title, Style::default().fg(theme.label)));
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

/// One segment of the memory bar. Picks the richest readout that
/// fits: `" used 14.0 GiB "` → `" used "` → solid fill. The byte
/// count lives *inside* the segment so the bar replaces the
/// title-row readout entirely.
fn seg(label: &str, bytes: u64, bg: Color, fg: Color, width: u64) -> Span<'static> {
    let w = usize::try_from(width).unwrap_or(0);
    if w == 0 {
        return Span::raw("");
    }
    let with_bytes = format!("{label} {}", proc::human_bytes(bytes));
    let content = if w >= with_bytes.len() + 2 {
        center(&with_bytes, w)
    } else if w >= label.len() + 2 {
        center(label, w)
    } else {
        " ".repeat(w)
    };
    Span::styled(content, Style::default().fg(fg).bg(bg))
}

/// Pad `s` with spaces to fill `w` cells, keeping `s` centered.
fn center(s: &str, w: usize) -> String {
    let pad = w.saturating_sub(s.len());
    let left = pad / 2;
    let right = pad - left;
    format!("{}{s}{}", " ".repeat(left), " ".repeat(right))
}

/// Side-by-side host-level sparklines: CPU%, MEM%, NET↓, NET↑, GPU%,
/// VRAM%. GPU + VRAM cells appear only when a backend reports them.
/// 60 samples each → last minute at the default 1 s tick.
// `vm_cpu_overlay` is `Some((vm_name, &ring))` when a VM row is
// selected; the first sparkline cell switches from host CPU to
// per-guest CPU%. Same 0..=100 scale, same width, just a different
// data source + title.
#[allow(clippy::too_many_lines)]
fn draw_host_history(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    h: &HostHistory,
    ifaces: &[net::Iface],
    gpus: &[gpu::Gpu],
    theme: &theme::Theme,
    vm_cpu_overlay: Option<(String, &VecDeque<u64>)>,
) {
    let show_gpu = !h.gpu.is_empty();
    let show_vram = !h.vram.is_empty();
    let n_cells = 4 + usize::from(show_gpu) + usize::from(show_vram);
    #[allow(clippy::cast_possible_truncation)]
    let denom = n_cells as u32;
    let constraints: Vec<Constraint> = (0..n_cells).map(|_| Constraint::Ratio(1, denom)).collect();
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    // CPU sparkline cell defaults to host samples but is preempted
    // by a per-VM ring when the user has a VM row selected. Same
    // shape — both rings are 0..=100 percent — so the layout doesn't
    // shift and the eye reads the chart the same way.
    let (cpu_data, cpu_title) = if let Some((vm_name, ring)) = vm_cpu_overlay {
        let data: Vec<u64> = ring.iter().copied().collect();
        let now = data.last().copied().unwrap_or(0);
        // Truncate long VM labels so the chart title doesn't bleed
        // past the cell border.
        let label = if vm_name.chars().count() > 20 {
            vm_name.chars().take(20).collect::<String>()
        } else {
            vm_name
        };
        (data, format!(" {label} {now}% "))
    } else {
        let data: Vec<u64> = h.cpu.iter().copied().collect();
        let now = data.last().copied().unwrap_or(0);
        (data, format!(" CPU {now}% "))
    };
    let mem_data: Vec<u64> = h.mem.iter().copied().collect();
    let down_data: Vec<u64> = h.net_down.iter().copied().collect();
    let up_data: Vec<u64> = h.net_up.iter().copied().collect();

    let mem_now = mem_data.last().copied().unwrap_or(0);
    let down_now = down_data.last().copied().unwrap_or(0);
    let up_now = up_data.last().copied().unwrap_or(0);
    let mem_title = format!(" MEM {mem_now}% ");
    let (top_rx, top_tx) = top_iface_names(ifaces);
    let down_title = format!(
        " NET\u{2193} {}{} ",
        net::human_rate(Some(down_now)),
        top_rx.map_or(String::new(), |n| format!(" {n}"))
    );
    let up_title = format!(
        " NET\u{2191} {}{} ",
        net::human_rate(Some(up_now)),
        top_tx.map_or(String::new(), |n| format!(" {n}"))
    );

    f.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(cpu_title))
            .data(&cpu_data)
            .max(100)
            .style(Style::default().fg(theme.spark_cpu)),
        cols[0],
    );
    f.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(mem_title))
            .data(&mem_data)
            .max(100)
            .style(Style::default().fg(theme.spark_mem)),
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
            .style(Style::default().fg(theme.spark_net_down)),
        cols[2],
    );
    f.render_widget(
        Sparkline::default()
            .block(Block::default().borders(Borders::ALL).title(up_title))
            .data(&up_data)
            .max(up_max)
            .style(Style::default().fg(theme.spark_net_up)),
        cols[3],
    );

    let mut next = 4;
    if show_gpu {
        let gpu_data: Vec<u64> = h.gpu.iter().copied().collect();
        let gpu_now = gpu_data.last().copied().unwrap_or(0);
        let watts = gpu::aggregate_power_watts(gpus)
            .map(|w| format!(" {w:.0}W"))
            .unwrap_or_default();
        let gpu_title = format!(" GPU {gpu_now}%{watts} ");
        f.render_widget(
            Sparkline::default()
                .block(Block::default().borders(Borders::ALL).title(gpu_title))
                .data(&gpu_data)
                .max(100)
                .style(Style::default().fg(theme.spark_gpu)),
            cols[next],
        );
        next += 1;
    }
    if show_vram {
        let vram_data: Vec<u64> = h.vram.iter().copied().collect();
        let vram_now = vram_data.last().copied().unwrap_or(0);
        let vram_title = format!(" VRAM {vram_now}% ");
        f.render_widget(
            Sparkline::default()
                .block(Block::default().borders(Borders::ALL).title(vram_title))
                .data(&vram_data)
                .max(100)
                .style(Style::default().fg(theme.spark_vram)),
            cols[next],
        );
    }
}

/// Iface with the highest RX rate, then the highest TX rate.
/// Returned for the NET sparkline titles so the user sees *which*
/// link is responsible for the rate.
fn top_iface_names(ifaces: &[net::Iface]) -> (Option<String>, Option<String>) {
    let top_rx = ifaces
        .iter()
        .filter(|i| i.rx_rate.unwrap_or(0) > 0)
        .max_by_key(|i| i.rx_rate.unwrap_or(0))
        .map(|i| i.name.clone());
    let top_tx = ifaces
        .iter()
        .filter(|i| i.tx_rate.unwrap_or(0) > 0)
        .max_by_key(|i| i.tx_rate.unwrap_or(0))
        .map(|i| i.name.clone());
    (top_rx, top_tx)
}

/// Sum the per-iface RX/TX rates into one (down, up) pair.
fn total_net_rates(ifaces: &[net::Iface]) -> (u64, u64) {
    let mut down = 0u64;
    let mut up = 0u64;
    for i in ifaces {
        down = down.saturating_add(i.rx_rate.unwrap_or(0));
        up = up.saturating_add(i.tx_rate.unwrap_or(0));
    }
    (down, up)
}

/// Inverse-video badge appended to the title when the user has
/// frozen the live tick. Bright enough that you can't miss it;
/// deliberately *not* a popup, because pausing should leave the
/// table fully readable.
fn paused_badge(theme: &theme::Theme) -> Span<'static> {
    Span::styled(
        "  [PAUSED — space to resume] ",
        Style::default()
            .fg(theme.badge_fg)
            .bg(theme.filter_bg)
            .add_modifier(Modifier::BOLD),
    )
}

fn draw_title_procs(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let total = app.procs_all.len();
    let visible = app.procs_visible.len();
    let mode_label = match app.list_mode {
        ListMode::Flat => " flat",
        ListMode::Tree => " tree",
        ListMode::Group => " group",
        ListMode::GroupTree => " group+tree",
    };
    let mut title = vec![
        Span::styled(
            " neotop ",
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.badge_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  {visible}/{total} processes ·{mode_label} · sort {}{}",
            app.procs_sort.label(),
            app.procs_sort.arrow(),
        )),
    ];
    if app.paused {
        title.push(paused_badge(&app.theme));
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
    history: &HostHistory,
    theme: &theme::Theme,
) {
    // Three tight lines by default. A fourth GPU line appears only
    // when at least one card was discovered under `/sys/class/drm`,
    // so machines without a discrete GPU don't pay a row of screen
    // real estate for nothing.
    let mut lines = vec![
        host_line1(h, batteries, theme),
        host_line_net_temp(ifaces, temps, theme),
        host_line4(disks, history, theme),
    ];
    if !gpus.is_empty() {
        lines.push(host_line_gpu(gpus, history, theme));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Cells reserved for the inline braille chart on the GPU and
/// disk lines. 8 chars × 2 samples-per-char = 16 s of history at
/// the default 1 Hz tick — narrow enough to fit on a 100-col
/// terminal next to the numeric readout.
const GPU_BRAILLE_CELLS: usize = 8;
const DISK_BRAILLE_CELLS: usize = 8;

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
#[allow(clippy::too_many_lines)]
fn host_line_gpu(gpus: &[gpu::Gpu], history: &HostHistory, theme: &theme::Theme) -> Line<'static> {
    let mut spans: Vec<Span<'static>> =
        vec![Span::styled(" gpu ", Style::default().fg(theme.label))];
    for (i, g) in gpus.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled(
            g.name.clone(),
            Style::default().fg(theme.gpu_name),
        ));
        if let Some(busy) = g.busy_pct {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let busy_int = busy.round() as i64;
            let busy_color = gpu_busy_color(busy, theme);
            // Inline braille mini-chart: last ~16 s of busy% so the
            // panel reads as a chart, not a single instant snapshot.
            if let Some(ring) = history.gpu_busy_per_card.get(&gpu_key(g)) {
                let series: Vec<u64> = ring.iter().copied().collect();
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    braille_line(&series, 100, GPU_BRAILLE_CELLS),
                    Style::default().fg(busy_color),
                ));
            }
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
                Style::default().fg(theme.label),
            ));
            spans.extend(gauge_cells(
                busy,
                GPU_GAUGE_CELLS,
                busy_color,
                theme.gauge_empty,
            ));
            spans.push(Span::styled(
                "▏".to_string(),
                Style::default().fg(theme.label),
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
                let vram_color = cpu_load_color(vram_pct, theme);
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    "▕".to_string(),
                    Style::default().fg(theme.label),
                ));
                spans.extend(gauge_cells(
                    vram_pct,
                    GPU_GAUGE_CELLS,
                    vram_color,
                    theme.gauge_empty,
                ));
                spans.push(Span::styled(
                    "▏".to_string(),
                    Style::default().fg(theme.label),
                ));
            }
        }
        if let Some(w) = g.power_watts {
            spans.push(Span::raw(format!(" {w:.1}W")));
        }
        // Per-engine breakdown (Intel i915 only, requires CAP_PERFMON).
        match &g.intel_engines {
            Some(gpu::IntelEngines::Busy {
                rcs,
                bcs,
                vcs,
                vecs,
            }) => {
                spans.push(Span::styled(" [", Style::default().fg(theme.label)));
                for (label, val) in [("rcs", rcs), ("bcs", bcs), ("vcs", vcs), ("vecs", vecs)] {
                    if let Some(p) = val {
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let pi = p.round() as i64;
                        spans.push(Span::styled(
                            format!(" {label}:{pi:>2}%"),
                            Style::default().fg(cpu_load_color(*p, theme)),
                        ));
                    }
                }
                spans.push(Span::styled(" ]", Style::default().fg(theme.label)));
            }
            Some(gpu::IntelEngines::CapDenied) => {
                spans.push(Span::styled(
                    " (+CAP_PERFMON for engines)",
                    Style::default().fg(theme.label),
                ));
            }
            None => {}
        }
        if !g.has_busy_data() && g.vram_total == 0 && g.intel_engines.is_none() {
            // No backend wired up yet for this vendor.
            spans.push(Span::styled(
                " (driver pending)",
                Style::default().fg(theme.label),
            ));
        }
    }
    Line::from(spans)
}

/// Same green/yellow/red ramp the per-core CPU grid uses, so the
/// user reads the GPU number with the same eye they read CPU.
fn gpu_busy_color(busy: f64, theme: &theme::Theme) -> Color {
    theme.gpu_busy_color(busy)
}

fn host_line1(
    h: &host::HostInfo,
    batteries: &[battery::Battery],
    theme: &theme::Theme,
) -> Line<'static> {
    let cpu_pct = h
        .cpu_pct
        .map_or_else(|| "—".to_string(), |p| format!("{p:>4.1}%"));
    let mem_used = h.mem_total_bytes.saturating_sub(h.mem_avail_bytes);
    let mem_pct = mem_used_pct(h);

    let mut spans: Vec<Span<'static>> = vec![
        Span::raw(" "),
        Span::styled("CPU", Style::default().fg(theme.label)),
        Span::raw(format!(" {cpu_pct}  ")),
        Span::styled("MEM", Style::default().fg(theme.label)),
        Span::raw(format!(
            " {}/{} ({mem_pct:>4.1}%)  ",
            proc::human_bytes(mem_used),
            proc::human_bytes(h.mem_total_bytes),
        )),
    ];

    // Swap is only worth the screen real estate when the box has
    // some configured. Most cloud servers don't, and showing
    // "swap 0/0 (0%)" is just noise. When swap *is* present,
    // color the percentage red once it's non-trivial — the
    // system swapping out memory is one of the strongest
    // "something is wrong" signals there is.
    if h.swap_total_bytes > 0 {
        let swap_used = h.swap_total_bytes.saturating_sub(h.swap_free_bytes);
        #[allow(clippy::cast_precision_loss)]
        let swap_pct = (swap_used as f64 / h.swap_total_bytes as f64) * 100.0;
        let swap_color = theme.swap_color(swap_pct);
        spans.push(Span::styled("swap", Style::default().fg(theme.label)));
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
        Span::styled("load", Style::default().fg(theme.label)),
        Span::raw(format!(
            " {:.2} {:.2} {:.2}",
            h.loadavg_1, h.loadavg_5, h.loadavg_15,
        )),
    ]);
    if !batteries.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled("bat", Style::default().fg(theme.label)));
        for b in batteries {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("{}%", b.percent),
                Style::default().fg(battery_color(b, theme)),
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

fn host_line_net_temp(
    ifaces: &[net::Iface],
    temps: &[temp::Reading],
    theme: &theme::Theme,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> =
        vec![Span::styled(" net ", Style::default().fg(theme.label))];
    if ifaces.is_empty() {
        spans.push(Span::raw("—"));
    } else {
        for (i, iface) in ifaces.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                iface.name.clone(),
                Style::default().fg(theme.spark_net_down),
            ));
            spans.push(Span::raw(format!(
                " ↓{} ↑{}",
                net::human_rate(iface.rx_rate),
                net::human_rate(iface.tx_rate),
            )));
        }
    }
    spans.push(Span::raw("   "));
    spans.push(Span::styled("temp ", Style::default().fg(theme.label)));

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
                temp::Severity::Cool => theme.battery_good,
                temp::Severity::Warm => theme.battery_mid,
                temp::Severity::Hot => theme.battery_low,
            };
            spans.push(Span::styled(
                compact_temp_label(&r.label).to_string(),
                Style::default().fg(theme.label),
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

fn host_line4(disks: &[disk::Disk], history: &HostHistory, theme: &theme::Theme) -> Line<'static> {
    let mut spans: Vec<Span<'static>> =
        vec![Span::styled(" disk ", Style::default().fg(theme.label))];
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
            Style::default().fg(theme.gpu_name),
        ));
        // Inline braille mini-chart of total throughput. y-axis
        // auto-scales to this disk's own peak in the window so a
        // quiet SSD next to a saturated HDD both show a readable
        // chart instead of one being flatlined.
        if let Some(ring) = history.disk_rate.get(&d.name) {
            let series: Vec<u64> = ring.iter().copied().collect();
            let max = series.iter().copied().max().unwrap_or(0).max(1);
            let color = match d.util_pct {
                Some(u) if u >= 80.0 => theme.cpu_high,
                Some(u) if u >= 50.0 => theme.cpu_mid,
                _ => theme.cpu_low,
            };
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                braille_line(&series, max, DISK_BRAILLE_CELLS),
                Style::default().fg(color),
            ));
        }
        spans.push(Span::raw(format!(
            " ↓{} ↑{}",
            disk::human_rate(d.read_bps),
            disk::human_rate(d.write_bps),
        )));
        if let Some(util) = d.util_pct {
            // Highlight saturated devices — same yellow/red thresholds
            // we use for CPU% to keep the eye-trained palette consistent.
            let color = if util >= 80.0 {
                theme.cpu_high
            } else if util >= 50.0 {
                theme.cpu_mid
            } else {
                theme.perf_ok
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

/// Render a bytes-per-second rate for the proc table's `R/s`/`W/s`
/// columns. Compact 8-char form: `—`, `512 B`, `4.2K`, `38M`, `1.2G`.
/// Same shape as `proc::human_bytes` but tighter so two columns fit
/// in the row budget. `None` collapses to `—` and zero collapses to
/// blank — most processes never touch disk and we don't want a wall
/// of zeros drawing the eye.
fn io_rate_cell(bps: Option<u64>) -> String {
    let Some(n) = bps else {
        return "—".to_string();
    };
    if n == 0 {
        return String::new();
    }
    #[allow(clippy::cast_precision_loss)]
    let f = n as f64;
    if n >= 1_000_000_000 {
        format!("{:.1}G", f / 1_000_000_000.0)
    } else if n >= 1_000_000 {
        format!("{:.1}M", f / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", f / 1_000.0)
    } else {
        format!("{n}B")
    }
}

/// Detail-pane variant of `io_rate_cell` — keeps the trailing `/s`
/// suffix because the detail block has plenty of room and a bare
/// `4.2K` next to other lines could be mistaken for an absolute
/// counter.
fn io_rate_detail(bps: Option<u64>) -> String {
    match bps {
        None => "—".to_string(),
        Some(0) => "0 B/s".to_string(),
        Some(n) => format!("{}/s", proc::human_bytes(n)),
    }
}

fn cpu_glyph_color(pct: f64, theme: &theme::Theme) -> Color {
    theme.cpu_load_color(pct)
}

/// Header colour by group band: Cyan for Container (the workload
/// the developer explicitly started), Yellow for language Runtime
/// (the daemon they actively launched), `DarkGray` for System and
/// Native so they sit in the visual background of the panel.
fn group_band_color(band: groups::GroupBand, theme: &theme::Theme) -> Color {
    theme.group_band_color(band)
}

fn battery_color(b: &battery::Battery, theme: &theme::Theme) -> Color {
    theme.battery_color(b)
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
    // Two-tier badge: real failures get the loud red ⚠ + cumulative
    // `(N err)`; informational events (parked sensors, throttled
    // scanners) get a quieter yellow ℹ with no count, because
    // counting them would mislead users into thinking neotop was
    // broken when it actually self-healed.
    let err_text = err_entry.map(|e| match e.severity {
        errors::Severity::Warn => format!(
            " \u{26a0} {}: {} ({} err) ",
            e.source,
            e.message,
            app.errors.total()
        ),
        errors::Severity::Info => format!(" \u{2139} {}: {} ", e.source, e.message),
    });
    let err_style = err_entry.map_or_else(
        || {
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.err_warn_bg)
        },
        |e| match e.severity {
            errors::Severity::Warn => Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.err_warn_bg)
                .add_modifier(Modifier::BOLD),
            errors::Severity::Info => Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.err_info_bg),
        },
    );
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
            Paragraph::new(Line::from(Span::styled(text, err_style))),
            chunks[idx],
        );
        idx += 1;
    }
    draw_perf(f, chunks[idx], app);
}

fn draw_perf(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let p = &app.perf.perf;
    let scan_color = ms_color(p.scan_ms, &app.theme);
    let render_color = ms_color(p.render_ms, &app.theme);
    let cpu = p
        .own_cpu_pct
        .map_or_else(|| "—".to_string(), |v| format!("{v:.1}%"));
    let line = Line::from(vec![
        Span::styled("scan ", Style::default().fg(app.theme.label)),
        Span::styled(
            format!("{:.1}ms", p.scan_ms),
            Style::default().fg(scan_color),
        ),
        Span::raw(" "),
        Span::styled("render ", Style::default().fg(app.theme.label)),
        Span::styled(
            format!("{:.1}ms", p.render_ms),
            Style::default().fg(render_color),
        ),
        Span::raw(" "),
        Span::styled("own ", Style::default().fg(app.theme.label)),
        Span::raw(proc::human_bytes(p.own_rss_bytes)),
        Span::raw(" "),
        Span::raw(cpu),
        Span::raw(" "),
        Span::styled("tick ", Style::default().fg(app.theme.label)),
        Span::raw(format!(
            "{:.0}/{}ms",
            p.refresh_actual_ms,
            app.refresh.as_millis()
        )),
    ]);
    f.render_widget(Paragraph::new(line).alignment(Alignment::Right), area);
}

fn ms_color(ms: f64, theme: &theme::Theme) -> Color {
    theme.ms_color(ms)
}

// Per-mode footer prompts plus the Normal-mode shortcut line is
// long enough to nudge clippy's `too_many_lines` over its 100-line
// default threshold by a couple lines. Splitting it would just
// move the same shortcut spans into a sibling helper without
// adding clarity, so we accept the lint here.
#[allow(clippy::too_many_lines)]
fn draw_help(f: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    // Modal prompts take over the help bar entirely.
    match &app.input {
        InputMode::Filter => {
            let line = Line::from(vec![
                Span::styled(
                    " filter ",
                    Style::default()
                        .fg(app.theme.badge_fg)
                        .bg(app.theme.filter_bg),
                ),
                Span::raw(" "),
                Span::styled(
                    app.procs_filter.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
                Span::raw("   "),
                Span::styled(
                    " Enter ",
                    Style::default()
                        .fg(app.theme.badge_fg)
                        .bg(app.theme.highlight_bg),
                ),
                Span::raw(" apply   "),
                Span::styled(
                    " Esc ",
                    Style::default()
                        .fg(app.theme.badge_fg)
                        .bg(app.theme.highlight_bg),
                ),
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
                    Style::default()
                        .fg(app.theme.badge_fg)
                        .bg(app.theme.err_warn_bg),
                ),
                Span::raw(format!(" {target}   ")),
                Span::styled(
                    " y ",
                    Style::default()
                        .fg(app.theme.badge_fg)
                        .bg(app.theme.highlight_bg),
                ),
                Span::raw(" confirm   "),
                Span::styled(
                    " any ",
                    Style::default()
                        .fg(app.theme.badge_fg)
                        .bg(app.theme.highlight_bg),
                ),
                Span::raw(" cancel"),
            ]);
            f.render_widget(Paragraph::new(line), area);
            return;
        }
        InputMode::Normal => {}
    }

    // `H` toggles the per-core spectrum chart in either view, so
    // it goes on the shared prefix instead of the view-specific
    // tail. The label reflects current state so the user knows
    // what pressing `H` will do *next*.
    let h_label = if app.per_core_spectrum {
        " grid"
    } else {
        " spectrum"
    };
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(
            " q ",
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.highlight_bg),
        ),
        Span::raw(" quit  "),
        Span::styled(
            " ? ",
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.highlight_bg),
        ),
        Span::raw(" help  "),
        Span::styled(
            " j/k ",
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.highlight_bg),
        ),
        Span::raw(" nav  "),
        Span::styled(
            " r ",
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.highlight_bg),
        ),
        Span::raw(" refresh  "),
        Span::styled(
            " H ",
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.highlight_bg),
        ),
        Span::raw(format!("{h_label}  ")),
    ];
    spans.extend([
        Span::styled(
            " s ",
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.highlight_bg),
        ),
        Span::raw(" sort  "),
        Span::styled(
            " t ",
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.highlight_bg),
        ),
        Span::raw(" tree  "),
        Span::styled(
            " g ",
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.highlight_bg),
        ),
        Span::raw(" group  "),
        Span::styled(
            " / ",
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.highlight_bg),
        ),
        Span::raw(" filter  "),
        Span::styled(
            " K ",
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.highlight_bg),
        ),
        Span::raw(" SIGTERM  "),
        Span::styled(
            " ^K ",
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.highlight_bg),
        ),
        Span::raw(" SIGKILL"),
    ]);
    if !app.procs_filter.is_empty() {
        spans.push(Span::raw("    "));
        spans.push(Span::styled(
            "filter:",
            Style::default().fg(app.theme.label),
        ));
        spans.push(Span::styled(
            format!(" {} ", app.procs_filter),
            Style::default()
                .fg(app.theme.badge_fg)
                .bg(app.theme.filter_bg)
                .add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// `draw_proc_table` is tabular and the body reads top-to-bottom: the
// header-row branch above the process-row branch. Splitting it would
// turn that into two functions called from a wrapper, which costs
// clarity for no real win.
#[allow(clippy::too_many_lines)]
fn draw_proc_table(f: &mut ratatui::Frame<'_>, area: Rect, app: &mut App) {
    let header = Row::new(vec![
        "PID", "USER", "S", "CPU%", "RSS", "THR", "R/s", "W/s", "COMMAND",
    ])
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
                let band_color = group_band_color(h.band, &app.theme);
                let cpu_text = format!("{:>5.1}", h.total_cpu);
                let rss_text = proc::human_bytes(h.total_rss);
                let banner = format!("▼ {label}  ({n})", label = h.label, n = h.count);
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
                    Cell::from(""),
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
            let cpu_style =
                Style::default().fg(cpu_glyph_color(r.cpu_pct.unwrap_or(0.0), &app.theme));
            let state_style = proc_state_style(r.state, &app.theme);
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
                Cell::from(io_rate_cell(r.read_bps)),
                Cell::from(io_rate_cell(r.write_bps)),
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
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Min(30),
    ];

    // Sort key applies to *every* mode (siblings inside a tree,
    // members inside a group, the whole flat list), so surface it
    // in every title — used to be flat-only and made sort changes
    // invisible in the other modes.
    let sort_tag = format!("{}{}", app.procs_sort.label(), app.procs_sort.arrow());
    let filter_tag = if app.procs_filter.is_empty() {
        String::new()
    } else {
        format!(" · /{}", app.procs_filter)
    };
    let title = match app.list_mode {
        ListMode::Tree => format!(" processes · tree · {sort_tag} (t to leave){filter_tag} "),
        ListMode::Group => {
            format!(" processes · grouped · {sort_tag} (g to leave){filter_tag} ")
        }
        ListMode::GroupTree => {
            format!(" processes · group+tree · {sort_tag} (g/t to peel off){filter_tag} ")
        }
        ListMode::Flat => format!(" processes · by {sort_tag}{filter_tag} "),
    };
    let table = Table::new(body, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(title))
        .row_highlight_style(
            Style::default()
                .bg(app.theme.highlight_bg)
                .add_modifier(Modifier::BOLD),
        );

    f.render_stateful_widget(table, area, &mut app.procs_table);
    draw_scrollbar(
        f,
        area,
        app.procs_visible.len(),
        app.procs_table.selected().unwrap_or(0),
        &app.theme,
    );
}

fn proc_state_style(c: char, theme: &theme::Theme) -> Style {
    let color = theme.proc_state_color(c);
    match c {
        'R' => Style::default().fg(color).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(color),
    }
}

fn truncate_lossy(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Vertical scrollbar painted on the right edge of a bordered table.
/// Hides when the row count is small enough that the table doesn't
/// scroll — no point in a stub thumb that fills the whole track.
///
/// Drawn *after* the table so it overlays the right border. We use the
/// border row directly as the track so the table loses no inner width.
fn draw_scrollbar(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    total: usize,
    selected: usize,
    theme: &theme::Theme,
) {
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
        .style(Style::default().fg(theme.label));
    // Inset by 1 so we don't clobber the corner glyphs of the block.
    let inner = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: area.height.saturating_sub(2),
    };
    f.render_stateful_widget(bar, inner, &mut state);
}

fn section(label: &'static str, style: Style) -> Line<'static> {
    Line::from(Span::styled(label, style))
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
            read_bps: None,
            write_bps: None,
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
        // header row sits ahead of its members for Container and
        // Runtime; Native (and System) skip the header to avoid the
        // misleading "sum of every static binary on the host" line.
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
                groups::Group::Runtime(groups::Lang::Java, "app.jar".into()),
            ),
            p_with_group(300, "myapp", 1.0, 100_000, groups::Group::Native),
        ];
        let names = groups::ContainerNames::default();
        let v = compute_visible_grouped(&rows, procs::SortBy::Cpu, "", &names, false);
        // Pattern: header(docker) m m header(java [vthreads]) m m_native.
        assert_eq!(v.len(), 6, "no header for the native band");
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
        // Then the java header (carrying its concurrency signature)
        // and its single member.
        assert!(v[3].header.is_some());
        assert_eq!(
            v[3].header.as_ref().unwrap().label,
            "java:app.jar [vthreads]"
        );
        assert_eq!(rows[v[4].idx].pid, 200);
        // Native member is emitted directly with no banner ahead.
        assert!(v[5].header.is_none());
        assert_eq!(rows[v[5].idx].pid, 300);
        assert!(
            v[5].prefix.is_empty(),
            "headerless members render flush-left like flat mode"
        );
    }

    #[test]
    fn grouped_visible_sort_cpu_floats_busy_group_above_band_priority() {
        // A native binary pegging 80% CPU should appear *above* a
        // Docker group at 5% CPU when sorted by CPU. The native
        // group renders without a banner, so the row at v[0] is the
        // member itself; the docker header lands at v[1] with its
        // member at v[2]. Band priority only kicks back in when the
        // sort key isn't an aggregate (PID / Command).
        let rows = vec![
            p_with_group(
                10,
                "nginx",
                5.0,
                0,
                groups::Group::Container(groups::Container {
                    runtime: groups::ContainerRuntime::Docker,
                    id: "abc12".into(),
                }),
            ),
            p_with_group(20, "myapp", 80.0, 0, groups::Group::Native),
        ];
        let names = groups::ContainerNames::default();
        let v = compute_visible_grouped(&rows, procs::SortBy::Cpu, "", &names, false);
        assert_eq!(v.len(), 3, "native member + docker header + docker member");
        assert!(v[0].header.is_none(), "native band has no banner");
        assert_eq!(rows[v[0].idx].pid, 20);
        assert_eq!(v[1].header.as_ref().unwrap().label, "docker:abc12");
        assert_eq!(rows[v[2].idx].pid, 10);

        // Sort by PID: band priority restored (docker first, then
        // the bannerless native member).
        let v = compute_visible_grouped(&rows, procs::SortBy::Pid, "", &names, false);
        assert_eq!(v[0].header.as_ref().unwrap().label, "docker:abc12");
        assert!(v[2].header.is_none());
        assert_eq!(rows[v[2].idx].pid, 20);
    }

    #[test]
    fn grouped_visible_skips_native_and_system_headers() {
        // Regression for the "misleading total" bug: Native and
        // System used to emit a banner that aggregated every static
        // binary / kernel daemon on the host into a single huge
        // CPU + RSS line. The row was always the largest in the
        // table and gave the user a false signal. Confirm both
        // bands now render members only, no banner.
        let rows = vec![
            p_with_group(1, "/lib/systemd/systemd", 0.0, 0, groups::Group::System),
            p_with_group(2, "/usr/local/bin/myapp", 0.0, 0, groups::Group::Native),
        ];
        let names = groups::ContainerNames::default();
        let v = compute_visible_grouped(&rows, procs::SortBy::Cpu, "", &names, false);
        assert_eq!(v.len(), 2, "two members, no banners");
        assert!(v.iter().all(|r| r.header.is_none()));
    }

    #[test]
    fn grouped_tree_renders_parent_child_inside_each_group() {
        // Two java rows where pid=200 is the parent and pid=201 is
        // its child. In group+tree mode the child should sit *under*
        // the parent inside the java group, with a `└─` glyph.
        let parent = procs::ProcessRow {
            ppid: 1,
            ..p_with_group(
                200,
                "java -jar app",
                10.0,
                0,
                groups::Group::Runtime(groups::Lang::Java, "app.jar".into()),
            )
        };
        let child = procs::ProcessRow {
            ppid: 200,
            ..p_with_group(
                201,
                "java worker",
                40.0,
                0,
                groups::Group::Runtime(groups::Lang::Java, "app.jar".into()),
            )
        };
        let rows = vec![parent, child];
        let names = groups::ContainerNames::default();
        let v = compute_visible_grouped(&rows, procs::SortBy::Cpu, "", &names, true);
        assert_eq!(v.len(), 3, "header + parent + child");
        assert!(v[0].header.is_some());
        assert_eq!(rows[v[1].idx].pid, 200);
        assert_eq!(rows[v[2].idx].pid, 201);
        // Child row carries a tree branch glyph in its prefix.
        assert!(v[2].prefix.contains("─"), "child should have ─ glyph");
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
                groups::Group::Runtime(groups::Lang::Java, "app.jar".into()),
            ),
            p_with_group(
                2,
                "node server",
                5.0,
                0,
                groups::Group::Runtime(groups::Lang::Node, "server.js".into()),
            ),
        ];
        let names = groups::ContainerNames::default();
        let v = compute_visible_grouped(&rows, procs::SortBy::Cpu, "java", &names, false);
        assert_eq!(v.len(), 2, "one header + one member");
        assert_eq!(
            v[0].header.as_ref().unwrap().label,
            "java:app.jar [vthreads]"
        );
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
    fn percore_height_spectrum_two_cols_when_wide() {
        // Wide terminal (200 cols): cores split across 2 columns.
        // 8 cores → 4 rows + 1 axis = 5 rows.
        assert_eq!(percore_height(8, 200, 60, true), 5);
    }

    #[test]
    fn percore_height_spectrum_one_col_when_narrow() {
        // Narrow terminal: only one column fits, one row per core.
        // 4 cores @ 30 cols (only 1 fits) → 4 rows + axis = 5.
        assert_eq!(percore_height(4, 30, 60, true), 5);
    }

    #[test]
    fn percore_height_spectrum_caps_at_third_of_terminal() {
        // 32 cores @ 24 rows: 2-col → 16+1 wanted, capped at 24/3=8.
        assert_eq!(percore_height(32, 200, 24, true), 8);
    }

    #[test]
    fn spectrum_cores_per_row_picks_one_or_two() {
        assert_eq!(spectrum_cores_per_row(80, 8), 2);
        assert_eq!(spectrum_cores_per_row(40, 8), 1);
        assert_eq!(spectrum_cores_per_row(200, 1), 1);
    }

    #[test]
    fn braille_line_renders_zero_cells_as_empty() {
        assert_eq!(braille_line(&[10, 20, 30], 100, 0), "");
    }

    #[test]
    fn braille_line_pads_left_when_buffer_is_short() {
        // 3 samples in a 4-cell (= 8-slot) chart: first 5 slots are
        // blank braille (U+2800), last 3 carry the data.
        let s = braille_line(&[50, 50, 50], 100, 4);
        let chars: Vec<char> = s.chars().collect();
        assert_eq!(chars.len(), 4);
        // Cell 0 = slots 0,1 (both pad) → blank pattern.
        assert_eq!(chars[0], '\u{2800}');
        // Cell 1 = slots 2,3 (both pad) → blank.
        assert_eq!(chars[1], '\u{2800}');
        // Cell 2 = slots 4 (pad) + 5 (data) → only right dot lit.
        assert_ne!(chars[2], '\u{2800}');
        // Cell 3 = slots 6 (data) + 7 (data) → both lit.
        assert_ne!(chars[3], '\u{2800}');
    }

    #[test]
    fn braille_line_max_zero_yields_blank_chart() {
        let s = braille_line(&[1, 2, 3, 4], 0, 4);
        for c in s.chars() {
            assert_eq!(c, '\u{2800}');
        }
    }

    #[test]
    fn braille_line_high_value_uses_top_row() {
        // Two samples at peak → both columns, top row only
        // (left=0x01, right=0x08). Top-row dots only → 0x09.
        let s = braille_line(&[100, 100], 100, 1);
        let bits = u32::from(s.chars().next().unwrap()) - 0x2800;
        assert_eq!(bits, 0x09);
    }

    #[test]
    fn braille_line_zero_value_uses_bottom_row() {
        // Two zero samples → both columns, bottom row only
        // (left=0x40, right=0x80). Bottom-row dots only → 0xC0.
        let s = braille_line(&[0, 0], 100, 1);
        let bits = u32::from(s.chars().next().unwrap()) - 0x2800;
        assert_eq!(bits, 0xC0);
    }

    #[test]
    fn cpu_load_color_steps() {
        // Four-stop ramp: idle → low → mid → high.
        // Colour values come from the Dark (Catppuccin Mocha) preset.
        let t = theme::ThemePreset::Dark.colors();
        let idle = cpu_load_color(0.0, &t);
        let low = cpu_load_color(20.0, &t);
        let mid = cpu_load_color(50.0, &t);
        let high = cpu_load_color(80.0, &t);
        assert_eq!(idle, t.cpu_idle);
        assert_eq!(cpu_load_color(19.0, &t), t.cpu_idle);
        assert_eq!(low, t.cpu_low);
        assert_eq!(cpu_load_color(49.0, &t), t.cpu_low);
        assert_eq!(mid, t.cpu_mid);
        assert_eq!(cpu_load_color(79.0, &t), t.cpu_mid);
        assert_eq!(high, t.cpu_high);
        assert_eq!(cpu_load_color(100.0, &t), t.cpu_high);
    }

    #[test]
    fn gauge_cells_round_to_nearest() {
        // 0% empty.
        let s = gauge_cells(0.0, 10, Color::Green, Color::DarkGray);
        let total: usize = s.iter().map(|sp| sp.content.chars().count()).sum();
        assert_eq!(total, 10);
        assert_eq!(s[0].content.as_ref(), "");
        // 50% gives exactly 5 filled out of 10.
        let s = gauge_cells(50.0, 10, Color::Green, Color::DarkGray);
        assert_eq!(s[0].content.chars().count(), 5);
        assert_eq!(s[1].content.chars().count(), 5);
        // 100% fully filled, no empties.
        let s = gauge_cells(100.0, 10, Color::Red, Color::DarkGray);
        assert_eq!(s[0].content.chars().count(), 10);
        assert_eq!(s[1].content.as_ref(), "");
        // Out-of-range values clamp rather than panic.
        let s = gauge_cells(-50.0, 8, Color::Green, Color::DarkGray);
        assert_eq!(s[0].content.as_ref(), "");
        let s = gauge_cells(150.0, 8, Color::Red, Color::DarkGray);
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
        let t = theme::ThemePreset::Dark.colors();
        let line = spectrum_row(0, &ring, Some(50.0), 10, &t);
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
        let t = theme::ThemePreset::Dark.colors();
        let line = spectrum_axis_row(40, &t);
        let visible_after_label: usize = line
            .spans
            .iter()
            .skip(1)
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(visible_after_label, 40);
        // Empty sparkline still renders without panicking.
        let line = spectrum_axis_row(0, &t);
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
            intel_engines: None,
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
