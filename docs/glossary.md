# Glossary

Every project-specific term, defined once. Cross-referenced from the
other notes via `[[glossary#Term]]`.

## Band

A top-level category in the [[grouping|classifier]]: Container, VM,
Runtime, App, System, Native. Bands have a stable priority order
(Container first, Native last) and each has its own colour in the
theme. See [[grouping#Bands and visuals]].

## Bundle (macOS .app)

The outermost `*.app/` path segment of a process's executable path.
`/Applications/Google Chrome.app/Contents/...` → bundle `Google Chrome`.
Used as the clustering key for the [[grouping|App band]].

## EMA (Exponential Moving Average)

Smoothing applied to per-process and host-wide CPU %. Formula:
`smoothed = α · current + (1 − α) · previous`. Default α = 0.5 — spikes
appear on the first tick but don't thrash sort order.

## Frame vs Tick

- **Tick**: one pass through the sampling loop. Default 1 Hz.
- **Frame**: one TUI redraw. We redraw on every tick *and* whenever the
  user presses a key (so the cursor feels instant). See [[architecture]].

## Group

The classified band + qualifier for a process. Represented by the
`Group` enum in `src/groups.rs`:

```rust
enum Group {
  Container(Container),     // runtime + short id
  Vm(VmInfo),               // hypervisor + guest name
  Runtime(Lang, String),    // lang + app (jar / script / binary)
  App(String),              // macOS .app bundle name
  System,                   // aggregated, no header
  Native(String),           // argv[0] basename
}
```

Each variant's label / sort-key / band is defined via methods on
`Group`. See [[grouping]].

## i915 PMU

Intel's kernel-side performance monitoring unit for integrated GPUs.
Exposes per-engine busy counters (`rcs` render, `bcs` blitter, `vcs`
video-codec, `vecs` video-enhance) via `perf_event_open`. neotop's
`gpu.rs` samples it at 1 Hz when the binary has `CAP_PERFMON`.

## Jiffies

Unit of CPU time used by the Linux kernel: tick count since boot, where
`clk_tck = sysconf(_SC_CLK_TCK)` (usually 100 ticks/s). `/proc/<pid>/stat`
fields 14 / 15 (utime / stime) are in jiffies. neotop's cpu % is the
delta jiffies / wall-clock / `clk_tck` × 100.

## KVM

Linux kernel hypervisor subsystem. `/proc/<vm>/task/*` gives vCPU thread
mapping; `/sys/kernel/debug/kvm/*` exposes exit counters. Linux-only.

## NUMA

Non-Uniform Memory Access — multi-socket machines where each CPU package
has its own memory controller. Cross-socket accesses are slower. neotop
reads the node assignment from
`/sys/devices/system/cpu/cpu*/node*` and inserts `── NUMA N ──`
separators into the per-core spectrum.

## Orbit chart

The right-hand panel showing the top-12 busy processes as dots orbiting
the selected process. Radius scales with CPU %; angle is stable per PID
(hash-derived) so the same PID keeps the same clock position tick to
tick. New PIDs pulse once. Source: `src/orbit.rs`.

## Runtime

A language VM / interpreter. `Group::Runtime(lang, app)` distinguishes
`java:app.jar`, `node:server.js`, etc. Both scripted (Java / Node /
Python / …) and compiled (Rust / Go via [[modules#elf-rs|elf.rs]]).

## SMT (Simultaneous Multi-Threading)

Hyper-threading. Two logical CPUs share one physical core. neotop labels
SMT siblings `c0a` / `c0b` so you can tell at a glance that `c0a` at
80 % busy and `c0b` at 20 % is one hot core, not two. Grouping happens in
[[modules|topology.rs]].

## Sparkline / braille line

The inline charts `▁▂▃▄▅▆▇█` (block drawing) and `⣀⣄⣠` (braille) that
render 60 samples = 1 min of history in 8–16 cells. Braille is used when
we want higher density (GPU mini-chart inside the host overview row);
blocks are used for per-core and the main sparkline strip.

## Tick (see also [[#Frame vs Tick]])

One call to the sampling + render path. Default 1 Hz. The "slow tick"
subset (temperatures, GPU, topology) runs every 4 ticks to amortise
expensive reads. See [[architecture]].

## VFIO

Linux's userspace-facing PCI-passthrough framework. Devices bound to
`vfio-pci` are reserved for a guest VM. neotop enumerates them under
`/sys/bus/pci/drivers/vfio-pci/` for the VM detail pane. Linux-only.

## vhost / tap

Kernel-accelerated virtio transports used by QEMU. `vhost-net` pushes
packets from kernel to guest; `tap*` / `macvtap*` are host-side
interfaces exposed to the guest. Inventoried by
[[modules|passthrough.rs]].
