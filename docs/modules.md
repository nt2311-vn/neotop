# Modules

One-line tour of every Rust source file. Cross-refs use `[[wikilinks]]`;
open in Obsidian to follow.

## Core loop

| File | What it does |
|------|-------------|
| `main.rs` | `App` struct, `run_loop`, all ratatui drawing, keybindings, layout. Cross-module glue. See [[architecture]]. |
| `orbit.rs` | Process orbit chart — top-12 busy PIDs plotted on a stable per-PID ellipse, radius by CPU %. |

## Data sources (cross-platform facades)

| File | Produces | Linux source | macOS source |
|------|----------|-------------|--------------|
| `proc.rs` | snapshot of `/proc/<pid>` state | `/proc/<pid>/{stat,status,cgroup,limits}` | n/a (not used on macOS) |
| `procs.rs` | `Vec<ProcessRow>` + EMA cpu % | `/proc/*` + [[modules#elf-rs\|elf.rs]] | `proc_pidinfo` + `KERN_PROCARGS2` + [[modules#elf-rs\|elf.rs]] |
| `host.rs` | `HostInfo` (CPU%, mem, load, kernel) | `/proc/stat`, `/proc/meminfo`, `/proc/loadavg`, `/proc/cpuinfo` | `host_processor_info`, `host_statistics64`, `sysctl` tree |
| `disk.rs` | per-device R/W rates | `/proc/diskstats` | delegates to `disk_macos.rs` (IOKit `IOMedia`) |
| `net.rs` | per-iface RX/TX rates | `/proc/net/dev` | delegates to `net_macos.rs` (`NET_RT_IFLIST2`) |
| `temp.rs` | sensor readings, off-thread worker | `/sys/class/hwmon` | delegates to `temp_macos.rs` (SMC / IOReport stub) |
| `battery.rs` | AC + battery gauges | `/sys/class/power_supply` | not yet wired |
| `gpu.rs` | `Vec<Gpu>` + history | NVML + amdgpu sysfs + i915_pmu | delegates to `gpu_macos.rs` (IOKit `IOAccelerator`) |
| `topology.rs` | SMT + NUMA groups | `/sys/devices/system/cpu/*/topology` | delegates to `topology_macos.rs` (`hw.{logical,physical}cpu`) |
| `elf.rs` | Rust / Go language scan | ELF64 section walk + rodata scan | Mach-O + FAT slice + rodata scan |

## macOS-only helpers

`#[cfg(target_os = "macos")]` — each one is a native implementation of
the corresponding cross-platform module:

- `topology_macos.rs`
- `disk_macos.rs`
- `net_macos.rs`
- `temp_macos.rs`
- `gpu_macos.rs`
- `container_macos.rs` — proc-tree heuristic for detecting processes
  spawned by Docker Desktop / Podman. See [[status#macOS container telemetry|the caveat]].

## Classification

| File | Role |
|------|------|
| `groups.rs` | `Group` enum and `classify_process` pipeline. See [[grouping]] for the full flow. |
| `vm.rs` | Hypervisor detection — parses `qemu-system-*`, `firecracker`, `cloud-hypervisor`, `crosvm`, `lkvm` from cmdline + pulls vCPU count, memory, guest name. |
| `vcpus.rs` | Maps `qemu-system-*` worker threads to host CPUs via `/proc/<vm>/task/*/stat`. |
| `kvm.rs` | KVM exit counters via `/sys/kernel/debug/kvm/*`. Linux-only. |
| `passthrough.rs` | VFIO groups + vhost-net + tap interfaces for the VM detail pane. |

## Presentation

| File | Role |
|------|------|
| `theme.rs` | Semantic palette, TOML schema, 4 built-in presets, `T` cycling. All colour look-ups go through `Theme::*_color` helpers. |
| `errors.rs` | Two-tier bounded ring (Warn / Info). Used by every data source to report non-fatal failures without panicking. |

## Binary entry

`main.rs` declares all modules behind `#[cfg]` gates. The binary itself
contains every module — the unused platform-specific ones are simply not
invoked, and the linker strips their dead code at release-build time.

## See also

- [[architecture]] — how these modules fit into one tick
- [[platforms-linux]] / [[platforms-macos]] — concrete syscalls per OS
- [[contributing]] — adding a new data source
