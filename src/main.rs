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
//!     neotop --refresh-ms 500     # slower poll (default 250 ms)
//!
//! Controls:
//!     q or Ctrl-C   quit
//!     j / Down      next vm
//!     k / Up        prev vm
//!     r             refresh immediately
//!     x             delete the state file of the selected halted vm

mod battery;
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
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table, TableState, Wrap};
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
        let mut refresh_ms: u64 = 250;

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
        "neotop — live view of running neosandbox VMs\n\
         \n\
         USAGE:\n    \
             neotop [--state-dir <path>] [--refresh-ms <n>]\n\
         \n\
         Defaults to $NEOSANDBOX_STATE or ./.neosandbox if unset.\n\
         \n\
         CONTROLS:\n    \
             q            quit\n    \
             j / Down     next vm\n    \
             k / Up       prev vm\n    \
             r            refresh immediately\n    \
             x            delete state file for selected halted vm"
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
/// Capacity is deliberately small — at a 250 ms scan rate, 60 samples
/// is the last 15 seconds, which is what a human eyeball can actually
/// parse as "what's happening right now".
const CPU_HISTORY_CAP: usize = 60;

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

fn run<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    run_dir: &Path,
    refresh: Duration,
) -> Result<()> {
    let clk_tck = proc::clk_tck();
    let mut prev_cpu: HashMap<i64, CpuSample> = HashMap::new();
    let mut history = CpuHistory::default();
    let mut prev_host_cpu: host::CpuSamples = host::read_cpu_samples();
    let mut host_info = host::snapshot(None);
    let mut net_tracker = net::Tracker::default();
    let mut ifaces = net_tracker.snapshot();
    let mut temps = temp::snapshot();
    let mut batteries = battery::snapshot();
    let mut rows = scan(run_dir, &mut prev_cpu, &mut history, clk_tck);
    let mut table_state = TableState::default();
    if !rows.is_empty() {
        table_state.select(Some(0));
    }
    let mut last_scan = Instant::now();

    loop {
        terminal.draw(|f| {
            draw(
                f,
                run_dir,
                &host_info,
                &ifaces,
                &temps,
                &batteries,
                &rows,
                &history,
                &mut table_state,
            );
        })?;

        // Wait for either keyboard input or the refresh interval, whichever
        // comes first. This keeps CPU at ~0 when idle.
        let elapsed = last_scan.elapsed();
        let wait = refresh.saturating_sub(elapsed);
        if event::poll(wait)? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(())
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        if let Some(i) = table_state.selected() {
                            let next = (i + 1).min(rows.len().saturating_sub(1));
                            table_state.select(Some(next));
                        } else if !rows.is_empty() {
                            table_state.select(Some(0));
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if let Some(i) = table_state.selected() {
                            table_state.select(Some(i.saturating_sub(1)));
                        }
                    }
                    KeyCode::Char('r') => {
                        // Force immediate rescan below.
                        last_scan = Instant::now()
                            .checked_sub(refresh)
                            .unwrap_or_else(Instant::now);
                    }
                    KeyCode::Char('x') => {
                        if let Some(i) = table_state.selected() {
                            if let Some(row) = rows.get(i) {
                                if row.state.phase == "halted"
                                    || row.state.phase == "shutdown"
                                    || row.state.phase == "error"
                                {
                                    let _ = fs::remove_file(&row.path);
                                    if let Some(parent) = row.path.parent() {
                                        let _ = fs::remove_dir(parent);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if last_scan.elapsed() >= refresh {
            rows = scan(run_dir, &mut prev_cpu, &mut history, clk_tck);
            host_info = host::snapshot(Some(&prev_host_cpu));
            prev_host_cpu = host::read_cpu_samples();
            ifaces = net_tracker.snapshot();
            temps = temp::snapshot();
            batteries = battery::snapshot();
            last_scan = Instant::now();
            // Keep selection in bounds after fleet changes.
            let sel = table_state.selected().unwrap_or(0);
            if rows.is_empty() {
                table_state.select(None);
            } else if sel >= rows.len() {
                table_state.select(Some(rows.len() - 1));
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Rendering
// -----------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn draw(
    f: &mut ratatui::Frame<'_>,
    run_dir: &Path,
    host_info: &host::HostInfo,
    ifaces: &[net::Iface],
    temps: &[temp::Reading],
    batteries: &[battery::Battery],
    rows: &[VmRow],
    history: &CpuHistory,
    table_state: &mut TableState,
) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // title
            Constraint::Length(3),  // host overview (3 lines: summary, hw, net+temp)
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

    let selected = rows.get(table_state.selected().unwrap_or(0));

    draw_title(f, chunks[0], run_dir, rows.len());
    draw_host(f, chunks[1], host_info, ifaces, temps, batteries);
    draw_table(f, chunks[2], rows, table_state);
    draw_serial(f, bottom[0], selected);
    draw_resources(f, bottom[1], selected, history);
    draw_help(f, chunks[4]);
}

fn draw_title(f: &mut ratatui::Frame<'_>, area: Rect, run_dir: &Path, count: usize) {
    let title = Line::from(vec![
        Span::styled(
            " neosandbox top ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  watching {} — {count} VM(s)", run_dir.display())),
    ]);
    f.render_widget(Paragraph::new(title), area);
}

fn draw_host(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    h: &host::HostInfo,
    ifaces: &[net::Iface],
    temps: &[temp::Reading],
    batteries: &[battery::Battery],
) {
    let line1 = host_line1(h);
    let line2 = host_line2(h, batteries);
    let line3 = host_line3(ifaces, temps);
    f.render_widget(Paragraph::new(vec![line1, line2, line3]), area);
}

fn host_line1(h: &host::HostInfo) -> Line<'static> {
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
    let mem_pct = if h.mem_total_bytes > 0 {
        #[allow(clippy::cast_precision_loss)]
        {
            (mem_used as f64 / h.mem_total_bytes as f64) * 100.0
        }
    } else {
        0.0
    };

    let mut spans: Vec<Span<'static>> = vec![
        Span::raw(" "),
        kvm,
        Span::raw("  "),
        Span::styled("host CPU", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(" {cpu_pct}  ")),
    ];
    if !h.per_core_pct.is_empty() {
        spans.push(Span::styled("cores ", Style::default().fg(Color::DarkGray)));
        for &pct in &h.per_core_pct {
            spans.push(Span::styled(
                bar_glyph(pct).to_string(),
                Style::default().fg(cpu_glyph_color(pct)),
            ));
        }
        spans.push(Span::raw("  "));
    }
    spans.extend([
        Span::styled("mem", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(
            " {}/{} ({mem_pct:>4.1}%)  ",
            proc::human_bytes(mem_used),
            proc::human_bytes(h.mem_total_bytes),
        )),
        Span::styled("load", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(" {:.2}", h.loadavg_1)),
    ]);
    Line::from(spans)
}

fn host_line2(h: &host::HostInfo, batteries: &[battery::Battery]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(" kernel ", Style::default().fg(Color::DarkGray)),
        Span::raw(h.kernel.clone()),
        Span::raw("   "),
        Span::styled("cpu ", Style::default().fg(Color::DarkGray)),
        Span::raw(h.cpu_model.clone()),
    ];
    if !batteries.is_empty() {
        spans.push(Span::raw("   "));
        spans.push(Span::styled("bat ", Style::default().fg(Color::DarkGray)));
        for (i, b) in batteries.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
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

fn host_line3(ifaces: &[net::Iface], temps: &[temp::Reading]) -> Line<'static> {
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

    let picks = temp::highlights(temps, 3);
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
            spans.push(Span::raw(compact_temp_label(&r.label)));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                format!("{:>4.1}°C", r.celsius),
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

fn compact_temp_label(label: &str) -> String {
    if label.starts_with("coretemp") {
        if label.contains("Package") {
            "cpu pkg".into()
        } else if label.contains("Core") {
            label
                .split_once(' ')
                .map_or_else(|| label.into(), |(_, t)| t.to_lowercase())
        } else {
            "cpu".into()
        }
    } else if label.starts_with("nvme") {
        "nvme".into()
    } else if label.starts_with("iwlwifi") {
        "wifi".into()
    } else if label.starts_with("acpitz") {
        "acpi".into()
    } else {
        label.split_whitespace().next().unwrap_or(label).to_string()
    }
}

fn draw_help(f: &mut ratatui::Frame<'_>, area: Rect) {
    let help = Line::from(vec![
        Span::styled(" q ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" quit  "),
        Span::styled(" j/k ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" navigate  "),
        Span::styled(" r ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" refresh  "),
        Span::styled(" x ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" delete halted state"),
    ]);
    f.render_widget(Paragraph::new(help), area);
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
