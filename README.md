# neotop

[![crates.io](https://img.shields.io/crates/v/neotop.svg)](https://crates.io/crates/neotop)
[![downloads](https://img.shields.io/crates/d/neotop.svg)](https://crates.io/crates/neotop)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![CI](https://github.com/nt2311-vn/neotop/actions/workflows/ci.yml/badge.svg)](https://github.com/nt2311-vn/neotop/actions/workflows/ci.yml)
[![CodeQL](https://github.com/nt2311-vn/neotop/actions/workflows/codeql.yml/badge.svg)](https://github.com/nt2311-vn/neotop/actions/workflows/codeql.yml)
![MSRV](https://img.shields.io/badge/MSRV-1.88-orange.svg)

A cross-platform terminal observer for **host metrics, processes, GPU
activity, containers, and KVM virtual machines**. Linux fully supported;
macOS support actively in development. Designed to
fill the gaps generic monitors (`htop`, `btop`, `btm`) leave open:
per-core CPU spectrum charts, NVIDIA + AMD + Intel GPU activity
(busy %, VRAM, watts), per-VM CPU sparklines with vCPU pinning
and VFIO passthrough discovery, and developer-meaningful process
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

**Linux fully supported; macOS support in progress.** The Linux implementation reads `/proc`, `/sys/class/hwmon`, `/sys/class/power_supply`, `/sys/class/drm`, and `/sys/fs/cgroup` directly — the same files `lm_sensors`, `nvidia-smi`, etc. read. macOS uses `sysctl` and `mach` APIs; the abstraction layer compiles on both platforms, with data source modules providing platform-specific implementations.

## Install

From [crates.io](https://crates.io/crates/neotop) (recommended):

```sh
cargo install neotop --locked
```

From source:

```sh
cargo install --git https://github.com/nt2311-vn/neotop --locked
cargo install --path .                                  # from a checkout
```

The binary is **single-file**, ~1.3 MB, no runtime deps. NVIDIA
support is on by default but dynamically loads `libnvidia-ml.so`
at launch — boxes without the driver fall back gracefully to
detect-only. For a smaller, NVIDIA-free build:

```sh
cargo install neotop --locked --no-default-features
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

Single binary, no `unsafe`, MSRV 1.88. One module per data source:

```text
src/
  main.rs        App, run loop, all UI rendering
  proc.rs        /proc/<pid>/{stat,status,limits,cgroup} parsers
  procs.rs       host-wide process tracker + EMA cpu_pct
  host.rs        /proc/{stat,meminfo,loadavg,cpuinfo,version}
  net.rs         /proc/net/dev tracker
  disk.rs        /proc/diskstats tracker
  temp.rs        /sys/class/hwmon walker (off-thread worker)
  battery.rs     /sys/class/power_supply
  gpu.rs         /sys/class/drm + Intel RC6 + NVML (feature-gated)
  groups.rs      process classification + docker/podman ps cache
  vm.rs          QEMU/KVM VM discovery + per-VM CPU history
  vcpus.rs       /proc/<qemu>/task vCPU → host-CPU pinning
  kvm.rs         KVM exit counters via /sys/kernel/debug/kvm
  passthrough.rs VFIO + vhost + tap discovery for VM detail pane
  elf.rs         ELF section probe (Go .gopclntab, Rust panic strs)
  errors.rs      bounded ring of non-fatal events (Info + Warn tiers)
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

## Documentation

- [`CHANGELOG.md`](CHANGELOG.md) — release history with per-version rationale.
- [`SECURITY.md`](.github/SECURITY.md) — disclosure policy + threat model.
- [`VMPLAN.md`](VMPLAN.md) — the design doc behind the VM-support feature set (historical reference).

## Contributing

PRs welcome. The `main` branch is protected: every change goes
through a feature branch + PR + CI (the `check`, `security`, and
`codeql` workflows must pass). See
[`.github/pull_request_template.md`](.github/pull_request_template.md)
for the incoming-PR checklist. Security-sensitive issues go
through a private advisory — see [`SECURITY.md`](.github/SECURITY.md),
not a public PR.

## License

Apache-2.0. See [`LICENSE`](LICENSE) for the full text.

## Roadmap

Items in progress:

- [x] macOS support — platform abstraction layer implemented, data sources in progress

Items still open:

- [ ] Per-engine Intel GPU breakdown (rcs / bcs / vcs / vecs) — needs `CAP_PERFMON` and `i915_pmu` perf events
- [ ] SMT / NUMA grouping in the spectrum view

Recently shipped (see [`CHANGELOG.md`](CHANGELOG.md) for the full
history):

- [x] Themes / TOML config — Catppuccin Mocha default, `T` to cycle, `--config` to override (`v0.23.0`)
- [x] Intel iGPU busy% via RC6 residency (`v0.19.0`)
- [x] KVM exit counters + per-VM CPU sparkline (`v0.16.0` / `v0.18.0`)
- [x] VFIO + vhost + tap passthrough discovery (`v0.18.0`)
- [x] Go / Rust runtime detection via ELF section scan (`v0.16.0`)
- [x] Per-app sub-buckets inside the runtime band (`v0.17.0`)
