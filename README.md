# neotop

[![crates.io](https://img.shields.io/crates/v/neotop.svg)](https://crates.io/crates/neotop)
[![downloads](https://img.shields.io/crates/d/neotop.svg)](https://crates.io/crates/neotop)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![CI](https://github.com/nt2311-vn/neotop/actions/workflows/ci.yml/badge.svg)](https://github.com/nt2311-vn/neotop/actions/workflows/ci.yml)
[![CodeQL](https://github.com/nt2311-vn/neotop/actions/workflows/codeql.yml/badge.svg)](https://github.com/nt2311-vn/neotop/actions/workflows/codeql.yml)
![MSRV](https://img.shields.io/badge/MSRV-1.88-orange.svg)

**A Linux-first terminal system monitor that shows what generic
tools hide. macOS support achieved feature parity with Linux in v0.27.0.**

Per-core CPU spectrum with SMT/NUMA grouping (HT siblings show as
`c0a` / `c0b`, `lscpu`-style), multi-vendor GPU dashboards
(NVIDIA / AMD / Intel with per-engine `i915_pmu` breakdown and RAPL
package power), a process **orbit chart** that visualizes the top-12
busy processes as dots on a stable per-PID ellipse, KVM hypervisor
insight (vCPU pinning, exit counters, VFIO passthrough), developer-
aware process grouping by container and language runtime, and a
fully themeable UI (Catppuccin Mocha default, TOML overrides).
Single binary, no daemons, no config required to start.

```text
 CPU  8.3%  MEM 9.1G/15.7G (58%)  load 0.31 0.28 0.22  kernel 6.9.3  Ryzen 7 7840HS
 ── NUMA 0 ─────────────────────────────────────────────────────────────────────────
 c0a ▁▂▃▄▅▄▃▂▁▁  8% ▕██░░▏  c0b ▁▁▁▁▂▁▁▁  2% ▕░░░░▏  c1a ▇▆▅▄▃▂▁▁  3% ▕█░░░▏
 c1b ▁▁▁▁▁▁▁▁    1% ▕░░░░▏  c2a ▂▃▄▅▆▅▄▃ 18% ▕███░▏  c2b ▁▁▂▁▁▁▁▁  4% ▕█░░░▏
 ┌─ CPU  8% ─┬─ MEM 58% ─┬─ NET↓ 2.1 MB/s ─┬─ NET↑ 84 KB/s ─┬─ GPU 41% ─┬─VRAM 31%─┐
 │ ▁▂▃▅▆▅▄▃▂ │ ██████▌░░ │ ▁▂▃▁▁▅▆▄▂▁▁▁▁  │ ▁▁▁▁▁▂▁▁▁▁▁▁▁  │ ▂▄▆▅▃▄▅▄▃ │ ▂▂▂▂▂▂▂▂▂ │
 └───────────┴───────────┴─────────────────┴────────────────┴───────────┴──────────┘
 gpu  AMD Radeon 780M ⣾⣷⣶⣤ 41%  ▕█████░░░▏  vram 1.9G/8.5G (22.4%)  ▕██░░░░░░▏
 ┌── procs ──────────────────────────┐ ┌── process orbit · busy = bigger radius ─┐
 │ ▼ docker:caddy        (2) 72.4%   │ │            ·          ●                │
 │ ▼ docker:postgres     (3)  4.1%   │ │      ·          12p           •        │
 │ ▼ rust:neotop [async] (1)  0.3%   │ │            •          ·                │
 │ ▼ system              (51) 1.2%   │ │ 12345 firefox       45.2% S            │
 │ ▼ native             (1821) 0.0%  │ │ 67890 chromium      22.1% R            │
 └───────────────────────────────────┘ └────────────────────────────────────────┘
```

Linux fully supported; macOS fully supported (feature parity achieved in v0.27.0).

## Install

From [crates.io](https://crates.io/crates/neotop) (recommended):

```sh
cargo install neotop --locked
```

From source:

```sh
cargo install --git https://github.com/nt2311-vn/neotop --locked
cargo install --path .                                  # from a local checkout
```

The binary is **single-file**, ~1.5 MB, no runtime deps.

**Feature flags** (all default-on, can be disabled to reduce binary size):

| Flag | What it adds | Disable with |
|------|-------------|--------------|
| `nvml` | NVIDIA GPU metrics via dynamic `libnvidia-ml.so` | `--no-default-features` |
| `i915-pmu` | Intel GPU per-engine breakdown via `perf_event_open` | `--no-default-features` |

```sh
# Smallest build — no NVIDIA, no i915 perf events
cargo install neotop --locked --no-default-features
```

## Develop

```sh
just                  # list every recipe
just check            # cargo fmt --check + clippy -D warnings + tests
just release          # release build
just run              # cargo run --release
```

## Controls

| Key             | Action                                                  |
| --------------- | ------------------------------------------------------- |
| `q` / `Ctrl-C`  | quit                                                    |
| `?`             | toggle the keybindings overlay                          |
| `j` / `k`       | move selection (also `↓` / `↑`)                         |
| `PgDn` / `PgUp` | jump 10 rows                                            |
| `r`             | force an immediate refresh                              |
| `+` / `-`       | speed up / slow down the refresh tick (50 ms … 5 s)     |
| `space`         | pause / resume the live tick                            |
| `s`             | cycle sort: CPU → MEM → PID → CMD                       |
| `t`             | toggle tree view (parent → children)                    |
| `g`             | toggle group view (container / runtime / system)        |
| `H`             | toggle per-core CPU **spectrum** view                   |
| `T`             | cycle theme: Dark → Light → Monokai → Tty → Dark        |
| `/`             | enter filter mode (`Esc` clears, `Enter` confirms)      |
| `K`             | send `SIGTERM` to selected pid (with confirm)           |
| `Ctrl-K`        | send `SIGKILL` to selected pid (with confirm)           |

## Configuration

Theme and colour overrides live in `~/.config/neotop/config.toml`.
All fields are optional; missing ones use the preset default.

```toml
theme = "dark"   # dark | light | monokai | tty

[colors]
cpu_high      = "#f38ba8"   # hex RGB
spark_mem     = "203,166,247" # decimal RGB
label         = "i244"      # 256-colour index
border        = "DarkGray"  # ratatui named colour
```

Override the config path at the command line:

```sh
neotop --config ~/dotfiles/neotop.toml
```

The default theme is **Catppuccin Mocha** — a high-contrast dark palette
designed to read well on true-colour terminals. Press `T` to cycle through
the four built-in presets without restarting.

## Why

Every Linux process / host monitor I tried under-served at least one
of these:

- **Per-core CPU history with topology.** `htop` shows the live %, `btop`
  shows a heatmap. neotop combines a 60-second sparkline + numeric % +
  proportional gauge per logical CPU, with SMT siblings placed adjacent and
  `── NUMA N ──` separators on multi-socket machines.
- **GPU — all three vendors.** `nvidia-smi -l 1` is a wall of text.
  `nvtop` is great but separate. neotop shows AMD (sysfs), NVIDIA (NVML),
  and Intel (RC6 overall busy% + per-engine `rcs`/`bcs`/`vcs`/`vecs`
  breakdown via `i915_pmu` when `CAP_PERFMON` is available) side-by-side
  with sparklines, VRAM gauges, and wattage.
- **KVM hypervisors.** No other host TUI shows a `qemu-system-x86_64` PID
  as a first-class VM with vCPU thread mapping, KVM exit counter rates, and
  VFIO / vhost / tap passthrough inventory — neotop does all of it from
  public kernel surfaces without a guest agent.
- **Process grouping.** A flat list of 2 000 PIDs doesn't tell you "this
  box is mostly Docker + Java daemons". The `g` toggle clusters processes
  by **container** (Docker / Podman / Kubernetes / containerd / LXC, with
  human-readable names), **language runtime** (Go / Rust / Java /
  Node / Python / Bun / Deno / Ruby / PHP / Perl / Lua / Erlang / .NET / R,
  detected via ELF section probe — no heuristics), **system** daemons, and
  **native** binaries.
- **Themes.** Most TUIs are hardcoded ANSI colours. neotop ships Catppuccin
  Mocha by default and supports per-field TOML overrides so the dashboard
  matches your terminal theme.

## Architecture

MSRV 1.88. One module per data source; only minimal `unsafe` for
`perf_event_open` (i915 engine counters) and macOS FFI
(`sysctl`, `libproc`) — each block is annotated with a `SAFETY` comment.

```text
src/
  main.rs        App struct, run loop, all ratatui UI rendering
  proc.rs        /proc/<pid>/{stat,status,limits,cgroup} parsers
  procs.rs       process tracker, EMA cpu_pct, disk I/O, ELF detection
  host.rs        /proc/{stat,meminfo,loadavg,cpuinfo,version}
  net.rs         /proc/net/dev rate tracker
  disk.rs        /proc/diskstats rate tracker
  temp.rs        /sys/class/hwmon walker (off-thread worker)
  battery.rs     /sys/class/power_supply
  gpu.rs         /sys/class/drm + Intel RC6 + i915_pmu + NVML
  topology.rs    /sys/devices/system/cpu/*/topology — SMT/NUMA groups
  theme.rs       semantic colour palette, TOML config, preset cycling
  groups.rs      container/runtime classification + docker/podman cache
  vm.rs          QEMU/KVM/Firecracker/crosvm discovery + per-VM history
  vcpus.rs       /proc/<vm>/task vCPU thread → host-CPU mapping
  kvm.rs         KVM exit counters via /sys/kernel/debug/kvm
  passthrough.rs VFIO + vhost + tap discovery for VM detail pane
  elf.rs         ELF64 section probe (Go .gopclntab, Rust panic strings)
  errors.rs      bounded ring of non-fatal events (Info + Warn tiers)
```

Key design choices:

- **1 Hz default tick.** Calmer than 4 Hz, still responsive. `+`/`-`
  retune from 50 ms to 5 s.
- **EMA-smoothed CPU%.** α = 0.5 for both per-process and host-wide.
  Spikes register visibly on the first tick but don't thrash sort order.
- **PID-locked cursor.** CPU% sorting reshuffles rows every tick; the
  cursor follows the same PID rather than chasing the hottest process.
- **Off-thread temp scanner.** Some `acpitz` sensors block for seconds.
  The worker thread absorbs that so the UI never stalls.
- **Two-tier error ring.** `Warn` (⚠) for real failures; `Info` (ℹ) for
  self-protection events. Honest signal without false-alarm styling.
- **Slow tick for expensive sources.** Temperatures, batteries, disks,
  GPUs, and CPU topology refresh every 4 ticks (4 s at default speed)
  instead of every tick, so steady-state is cheap.

## Documentation

- [`CHANGELOG.md`](CHANGELOG.md) — full release history with rationale.
- [`SECURITY.md`](.github/SECURITY.md) — disclosure policy and threat model.
- [`VMPLAN.md`](VMPLAN.md) — design doc for the VM feature set (historical reference).

## Contributing

PRs welcome. `main` is protected: every change goes through a feature
branch + PR + CI (`check`, `security`, `codeql` must all pass). See
[`.github/pull_request_template.md`](.github/pull_request_template.md)
for the checklist. Security issues go through a private advisory — see
[`SECURITY.md`](.github/SECURITY.md), not a public issue.

## License

Apache-2.0. See [`LICENSE`](LICENSE) for the full text.

## Roadmap

Items in progress:

- macOS support — bring-up + GPU shipped in `v0.26.0`. Disk I/O,
  per-interface network rates, and temperature sensors still
  fall back to empty rows; tracked for `v0.27.0`.

Items still open:

- [ ] macOS: per-disk I/O via IOKit (`IOServiceMatching("IOMedia")`)
- [ ] macOS: per-iface network rates via `getifaddrs` + `IFDATA`
- [ ] macOS: temperatures — SMC keys on Intel, IOReport on Apple
  Silicon
- [ ] Windows: ETW + PDH counters (separate epic)
- [ ] Intel GPU **true per-engine** power draw (blocked upstream —
  i915 PMU only exposes per-engine busy counters, not energy.
  Package-level power via RAPL is already shipped in `v0.25.0`.)

Recently shipped (see [`CHANGELOG.md`](CHANGELOG.md) for the full history):

- [x] macOS TUI bring-up + GPU enumeration via IOKit `IOAccelerator` (`v0.26.0`)
- [x] Process orbit chart in the detail pane (top-12 by CPU%, stable per-PID angle, bold-pulse on spawn) (`v0.25.0`)
- [x] Per-core spectrum: 4 cols max (was 2) — halves the chart's vertical footprint (`v0.25.0`)
- [x] SMT-aware spectrum labels (`c0a`/`c0b` for HT siblings) (`v0.25.0`)
- [x] Intel iGPU package-level power draw via RAPL `uncore`/`gt` domain (`v0.25.0`)
- [x] Intel GPU per-engine breakdown (`rcs`/`bcs`/`vcs`/`vecs`) via `i915_pmu` (`v0.24.0`)
- [x] SMT / NUMA grouping in the CPU spectrum (`v0.24.0`)
- [x] Catppuccin Mocha default theme, TOML config, `T` preset cycling (`v0.23.0`)
- [x] Intel iGPU overall busy% via RC6 residency — no root required (`v0.19.0`)
- [x] KVM exit counters + per-VM CPU sparkline (`v0.16.0` / `v0.18.0`)
- [x] VFIO + vhost + tap passthrough discovery (`v0.18.0`)
- [x] Go / Rust runtime detection via ELF section scan (`v0.16.0`)
- [x] Per-app sub-buckets inside runtime groups (`v0.17.0`)
