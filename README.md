# neotop

A Linux terminal observer for **host metrics, processes, and GPU
activity**. Designed to fill the gaps generic monitors (`htop`,
`btop`, `btm`) leave open: per-core CPU spectrum charts, NVIDIA + AMD
GPU activity (busy %, VRAM, watts), and developer-meaningful process
grouping by container / language runtime / system / native.

```text
 neotop   1234/2117 processes · group · sort CPU%↓
 CPU 12.4%  MEM 8.4G/15.7G (53.1%)  load 0.42 0.31 0.18
 c00 ▁▂▃▅▇▆▄▂▁ ... ▏  16% ▕████░░░░░░░░▏
 c01 ▁▁▁▂▂▁▁▁▁ ...    ▏   3% ▕█░░░░░░░░░░░▏
 ...
 ┌─ CPU 12% ─┬─ MEM 53% ─┬─ NET↓ 18 KB/s ─┬─ NET↑ 4 KB/s ─┬─ GPU 16% ─┐
 │ ▁▂▃▅▆▇▆▅▄ │ █████████ │ ▁▂▁▁▂▃▁▁▁     │ ▁▁▁▁▂▁▁       │ ▂▃▆▄▂▃    │
 └───────────┴───────────┴────────────────┴────────────────┴───────────┘
 ▼ docker:myapp        (5)        53.4%   2.1G
 ▼ docker:redis-cache  (1)         0.7%   124 MB
 ▼ java                (3)        12.0%   1.8G
 ▼ node                (8)         4.1%   612 MB
 ▼ system              (47)        2.0%   324 MB
 ▼ native              (1843)      0.0%   nil
```

**Linux only**, for now. Reads `/proc`, `/sys/class/hwmon`,
`/sys/class/power_supply`, `/sys/class/drm`, and `/sys/fs/cgroup`
directly — the same files `lm_sensors`, `nvidia-smi`, etc. read.
macOS and Windows would need per-OS modules; PRs welcome — the
codebase is split one module per data source.

## Install

```sh
cargo install --path .                # from a checkout
cargo install --git https://github.com/nt2311-vn/neotop  # remote
```

The binary is **single-file**, ~1.3 MB, no runtime deps. NVIDIA
support is on by default but dynamically loads `libnvidia-ml.so` at
launch — boxes without the driver fall back gracefully to detect-only.
For a smaller, NVIDIA-free build:

```sh
cargo install --path . --no-default-features
```

## Develop

```sh
just                  # list every recipe
just check            # cargo fmt --check + clippy -D warnings + tests
just release          # release build (~1.3 MB)
just run              # cargo run --release
```

## Controls

| Key            | Action                                              |
| -------------- | --------------------------------------------------- |
| `q` / `Ctrl-C` | quit                                                |
| `?`            | toggle the keybindings overlay                      |
| `j` / `k`      | move selection (also `↓` / `↑`)                     |
| `PgDn` / `PgUp`| jump 10 rows                                        |
| `r`            | force an immediate refresh                          |
| `+` / `-`      | speed up / slow down the refresh tick (50 ms..5 s)  |
| `space`        | pause / resume the live tick                        |
| `s`            | cycle sort: CPU → MEM → PID → CMD                   |
| `t`            | toggle tree view (parent → children)                |
| `g`            | toggle group view (container / runtime / system)    |
| `H`            | toggle per-core CPU **spectrum** view               |
| `/`            | enter filter mode (`Esc` clears, `Enter` confirms)  |
| `K`            | send `SIGTERM` to selected pid (with confirm)       |
| `Ctrl-K`       | send `SIGKILL` to selected pid (with confirm)       |

## Why

Every Linux process / host monitor I tried under-served at least one
of these:

- **Per-core CPU history.** `htop` shows the live %, `btop` shows a
  heatmap, but neither combines a 60-second sparkline + numeric % +
  proportional gauge **per core** in one row, in colour.
- **GPU.** `nvidia-smi -l 1` is a wall of text. `nvtop` is great but
  separate from your process viewer. neotop pulls both AMD (sysfs)
  and NVIDIA (NVML) into the same dashboard with a sparkline plus
  inline busy + VRAM gauges.
- **Process grouping.** A flat list of 2000 PIDs doesn't tell you
  "this box is mostly running Docker workloads + Java daemons". The
  `g` toggle clusters processes by **container** (Docker / Podman /
  Kubernetes / containerd / LXC, with human-readable names from
  `docker ps`), **language runtime** (Java / Node / Python / Bun /
  Deno / Ruby / PHP / Perl / Lua / Erlang / .NET / R), **system**
  (`systemd`, `dbus`, …), or **native** binaries.

## Architecture

Single binary, no `unsafe`, MSRV 1.80. One module per data source:

```text
src/
  main.rs      App, run loop, all UI rendering
  proc.rs      /proc/<pid>/{stat,status,limits,cgroup} parsers
  procs.rs     host-wide process tracker + EMA cpu_pct
  host.rs      /proc/{stat,meminfo,loadavg,cpuinfo,version}
  net.rs       /proc/net/dev tracker
  disk.rs      /proc/diskstats tracker
  temp.rs      /sys/class/hwmon walker (off-thread worker)
  battery.rs   /sys/class/power_supply
  gpu.rs       /sys/class/drm + NVML (feature-gated)
  groups.rs    process classification + docker/podman ps cache
  errors.rs    bounded ring of non-fatal events (Info + Warn tiers)
```

Key design choices:

- **1 Hz default tick.** Calmer than 4 Hz, still responsive. `+`/`-`
  retune; the lower bound is 50 ms.
- **EMA-smoothed CPU%.** Both per-process and host-wide. Stops the
  CPU% column from jittering between 12% and 47% on consecutive 1 s
  windows.
- **PID-locked cursor.** Sorting by CPU% reshuffles rows; the cursor
  follows the same PID instead of sliding to whatever's hottest.
- **Off-thread temp scanner.** Some hwmon sensors (`acpitz` on
  certain HP / Dell laptops) take 3 seconds per read. The worker
  thread takes that hit so the UI never freezes.
- **Two-tier error ring.** `Warn` (red ⚠) for actual failures;
  `Info` (yellow ℹ) for self-protection events like "parked slow
  sensor". Honest signal without false-alarm styling.
- **PID cache.** Per-process static info (uid, user, command, group
  classification) is computed once on first sight and cached. Steady
  state is a single `read_to_string` per PID per tick.

## License

Apache-2.0.

## Roadmap

Items still open:

- [ ] Go / Rust runtime detection via ELF section scan (`.gopclntab`)
- [ ] Themes / TOML config
- [ ] Intel GPU via i915 / Xe perf counters (needs `CAP_PERFMON`)
- [ ] SMT / NUMA grouping in the spectrum view
- [ ] macOS / Windows ports (per-OS modules)
