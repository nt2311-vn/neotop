# neotop

[![ci](https://github.com/nt2311/neotop/actions/workflows/ci.yml/badge.svg)](https://github.com/nt2311/neotop/actions/workflows/ci.yml)

Live terminal observer for **neosandbox microVMs** and the host running
them.

Built for the observability gap that generic system monitors (`btm`,
`btop`, `htop`) leave open for KVM-based hypervisor projects: per-VM
phase, exit counters, serial log, cgroup accounting, plus the host
signals you actually need — CPU cores, RAM, network, temperatures,
battery, `/dev/kvm` presence.

## Platform

**Linux only**, for now. Uses `/proc`, `/sys/class/hwmon`,
`/sys/class/power_supply`, and `/sys/fs/cgroup` directly. macOS and
Windows support would need per-OS modules (mach APIs / Performance
Counters); PRs welcome — architecture is already split into one module
per data source.

## Install

```sh
cargo install --git https://github.com/nt2311/neotop
```

Or from a checkout:

```sh
git clone https://github.com/nt2311/neotop ~/src/neotop
cd ~/src/neotop
cargo install --path .
```

## Develop

The repository ships a [`justfile`](./justfile) with every common
task wrapped behind a memorable name. After cloning, run
[`just`](https://github.com/casey/just) (no args) to list every recipe
with a one-line summary:

```sh
just setup       # verify rustc / cargo / rustfmt / clippy
just run         # debug build, run
just dev         # watch & rerun on save (needs cargo-watch)
just test        # cargo test --all-targets --locked
just check       # mirror CI: fmt-check, clippy -D warnings, test
just fix         # auto-apply rustfmt + safe clippy fixes
just release     # optimized build at target/release/neotop
just bench-hwmon # show how slow each /sys/class/hwmon device is
```

If you're new to the codebase, start with `just setup` — it tells
you exactly what's missing if anything. `just check` is what you
should run before every push: if it passes, GitHub CI will pass too.

## Usage

```sh
neotop                              # watch $NEOSANDBOX_STATE or ./.neosandbox
neotop --state-dir /var/run/neo     # watch a specific directory
neotop --refresh-ms 500             # faster poll (default 1000 ms / 1 Hz)
```

### Controls

| Key | View | Action |
| --- | --- | --- |
| `q` / `Ctrl-C` | both | quit |
| `?` | both | toggle keybindings overlay |
| `Tab` | both | toggle Vms / Procs view |
| `j` / `↓` | both | next row |
| `k` / `↑` | both | previous row |
| `PgDn` / `PgUp` | both | jump 10 rows |
| `r` | both | refresh now |
| `space` | both | pause / resume the live tick |
| `+` / `-` | both | speed up / slow down the refresh tick (50 ms..5 s) |
| `x` | Vms | delete `state.json` for the selected halted VM |
| `s` | Procs | cycle sort key (CPU → MEM → PID → CMD) |
| `t` | Procs | toggle tree view (parent → children) |
| `/` | Procs | enter filter mode (Esc clears, Enter applies) |
| `K` | Procs | send SIGTERM to the selected pid (with confirm) |
| `Ctrl-K` | Procs | send SIGKILL to the selected pid (with confirm) |

## What it shows

**Host overview (3 lines):**

- `kvm:ok`/`kvm:MISSING` indicator, host CPU% (EMA-smoothed),
  memory used/total, swap used/total (only when configured;
  yellow/red when used), load 1m / 5m / 15m, battery (`%` +
  `chg/dsch/full` + watts)
- network RX/TX per interface, temperature readouts (CPU package, NVMe,
  GPU…) colored green/yellow/red by severity, with friendly short
  labels — no more raw `pch_cannonlake#1` strings
- per-disk read/write rate + utilisation% for the top three physical
  devices (partitions, loop, ram, dm-, md, zram filtered out)

Static info that used to live on a fourth row (kernel version, CPU
model) is now in the `?` overlay under "System" — it doesn't change
between ticks and didn't earn a permanent line of the header.

**Fleet table:** one row per running VM — `PID PHASE MODE UPTIME CPU%
RSS IO MMIO HLT SHDN LAST_SERIAL`.

**Bottom pane (split):** left is the serial-tail for the selected VM;
right is a resource pane with live `/proc/<pid>/` stats, a 15-second
CPU% sparkline, cgroup-v2 path + memory.current/max, and the rlimits
that actually matter for microVMs.

**Procs view (`Tab` to switch):** htop-style process table for every
PID on the host — `PID USER STATE CPU% RSS THR COMMAND`. The cursor
is pid-locked, so sorting by CPU% never slides the selection off the
process you were watching. A vertical scrollbar tracks your
position in the list. Sortable by CPU%, RSS, PID, or command (`s`).
Substring filter (`/`). SIGTERM/SIGKILL via `K` / `Ctrl-K` with a
y/N prompt. Press `t` to switch to tree view (parent → children,
standard `├─ │ └─` glyphs).

Above the procs table sits a **per-core CPU grid** — every logical
core as `c{n} {bar} {pct}%`, color-coded green / yellow / red.
Auto-flows over up to two rows depending on terminal width. Below
that, four 15-second **sparklines**: host CPU% (green), memory%
(magenta), NET↓ (cyan), NET↑ (yellow). Each sparkline title
shows the current sample, e.g. `NET↓ 1.2 MB/s`. Net charts
auto-scale to the rolling max in their window so small bursts stay
visible next to large ones.

When the terminal is at least 110 columns wide, a **detail pane**
appears on the right of the Procs table showing the selected
process's PID, PPID, user, state, CPU%, threads, RSS, VSZ, the
cgroup-v2 path with memory.current / memory.max, the curated
rlimits, and the wrapped full command line.

Press `?` for a centered keybindings overlay listing every binding
with a short description.

Use `+` / `-` (or `=` / `_` if you don't want to reach for shift)
to speed up or slow down the refresh tick at runtime; the perf
footer shows both the live and configured values, e.g.
`tick 252/250ms`.

**Default view:** when `$NEOSANDBOX_STATE/run` doesn't exist, neotop
opens directly in Procs so it's immediately useful as a system
monitor. When the state-dir exists but is empty, the Vms view shows
a hint pointing at `Tab`.

**Footer:** quick help on the left; on the right, neotop measures and
shows its own overhead — scan/render time in milliseconds, our own
VmRSS, our own CPU%, and the actual tick interval. If a `/proc` or
`/sys` read fails non-fatally, the latest entry from the error ring
appears between the help text and perf metrics for 5 seconds.

## State contract

`neotop` is a pure observer. It reads atomically-written JSON files at
`$NEOSANDBOX_STATE/run/<pid>/state.json`. See
[`docs/state.json`](./docs/state.json.md) for the schema, currently
`v1`. The producer (`neosandbox`/`vmmd`) writes via
`tmp + rename(2)`; neotop never sees a half-written file.

## Data sources

| Widget | Source |
| --- | --- |
| VM fleet | `$NEOSANDBOX_STATE/run/*/state.json` |
| Per-VM `/proc` stats | `/proc/<pid>/{stat,status,limits,cgroup}` |
| Per-VM cgroup memory | `/sys/fs/cgroup/<path>/memory.{current,max}` |
| Host CPU | `/proc/stat` (aggregate + per-core) |
| Memory | `/proc/meminfo` (`MemTotal`, `MemAvailable`) |
| Kernel | `/proc/version` |
| Load avg | `/proc/loadavg` |
| Network | `/proc/net/dev` |
| Disks | `/proc/diskstats` |
| Temperatures | `/sys/class/hwmon/hwmon*/temp*_input` + `_label` |
| Battery | `/sys/class/power_supply/BAT*/{capacity,status,power_now}` |
| Host processes | `/proc/<pid>/{stat,status,cmdline}` |
| `/dev/kvm` | `Path::new("/dev/kvm").exists()` |

No privileged syscalls. No `unsafe`. Two sampling passes per scan, no
background threads.

## Roadmap

Things `btm`/`btop` have that neotop does not yet:

- [x] Process tree — shipped in 0.3.0 (`t` toggle in Procs view)
- [x] Per-device disk I/O (`/proc/diskstats`) — shipped in 0.2.0
- [x] Memory history chart — shipped in 0.2.0
- [x] Per-core CPU panel — shipped in 0.4.0
- [x] Network history chart — shipped in 0.4.0
- [x] Scrollbars on long tables — shipped in 0.4.0
- [x] Cached per-pid snapshots (3× faster scan) — shipped in 0.5.0
- [x] Friendly sensor labels (no more `pch_cannonlake#1`) — shipped in 0.5.0
- [x] Adaptive blacklist for slow hwmon sensors (acpitz fix) — shipped in 0.6.0
- [x] EMA-smoothed CPU% so rows don't jump around — shipped in 0.6.0
- [x] Pause toggle (`space`) — shipped in 0.6.0
- [x] Calmer 1 Hz default refresh (was 4 Hz, felt like a stock-ticker) — shipped in 0.7.0
- [x] Swap + 5m / 15m load averages in host overview — shipped in 0.7.0
- [x] Sort + filter inside tree mode — shipped in 0.7.0
- [x] AMD GPU metrics (busy %, VRAM, watts) — shipped in 0.8.0
- [x] NVIDIA / Intel GPU detection (driver-pending) — shipped in 0.8.0
- [x] Memory composition bar (used / buffers / cached / free) — shipped in 0.8.0
- [x] NVIDIA GPU metrics via `nvml-wrapper` — shipped in 0.9.0
- [x] Per-core CPU heatmap (cores × time grid; `H` to toggle) — shipped in 0.10.0
- [ ] Intel via `intel_gpu_top`-style perf counters
- [ ] Themes / layout config
- [ ] macOS / Windows ports

## License

Apache-2.0.
