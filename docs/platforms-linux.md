# Linux platform surfaces

Every kernel-exposed file neotop reads on Linux. Nothing runs through a
daemon, a library, or `ps(1)` — it's all direct parsing of
`procfs` / `sysfs` / `debugfs`.

## /proc — per-process

| Path | Consumer | Purpose |
|------|----------|---------|
| `/proc/<pid>/stat` | [[modules\|procs.rs]] | comm, state, ppid, utime+stime jiffies, threads |
| `/proc/<pid>/status` | [[modules\|proc.rs]] | UID, VmRSS, VmSize, open-fd count |
| `/proc/<pid>/cmdline` | [[modules\|procs.rs]] | argv (NUL-separated), used by [[grouping\|classifier]] |
| `/proc/<pid>/cgroup` | [[modules\|procs.rs]] | container-runtime path (docker / podman / k8s) |
| `/proc/<pid>/limits` | [[modules\|proc.rs]] | RLIMIT values for the detail pane |
| `/proc/<pid>/io` | [[modules\|procs.rs]] | read_bytes / write_bytes — per-process disk throughput |
| `/proc/<pid>/exe` | [[modules\|elf.rs]] | ELF section scan for Go / Rust detection |
| `/proc/<pid>/task/<tid>/stat` | [[modules\|vcpus.rs]] | per-vCPU-thread CPU time (QEMU only) |

## /proc — host

| Path | Consumer | Purpose |
|------|----------|---------|
| `/proc/stat` | [[modules\|host.rs]] | aggregate + per-CPU tick counters |
| `/proc/meminfo` | [[modules\|host.rs]] | MemTotal / MemAvailable / Free / Buffers / Cached / Swap* |
| `/proc/loadavg` | [[modules\|host.rs]] | 1 / 5 / 15-minute load |
| `/proc/cpuinfo` | [[modules\|host.rs]] | model name, logical CPU count |
| `/proc/version` | [[modules\|host.rs]] | kernel string |
| `/proc/diskstats` | [[modules\|disk.rs]] | per-device sector counters (512-byte units) |
| `/proc/net/dev` | [[modules\|net.rs]] | per-interface RX/TX byte counters |

## /sys — host

| Path | Consumer | Purpose |
|------|----------|---------|
| `/sys/devices/system/cpu/cpu*/topology/{physical_package_id,core_id}` | [[modules\|topology.rs]] | SMT siblings |
| `/sys/devices/system/cpu/cpu*/node*` | [[modules\|topology.rs]] | NUMA node membership |
| `/sys/class/hwmon/hwmon*/temp*_input` | [[modules\|temp.rs]] | sensors in mC (scanned off-thread) |
| `/sys/class/power_supply/BAT*/` | [[modules\|battery.rs]] | `status`, `capacity`, `power_now`, `voltage_now` |
| `/sys/class/drm/card*/device/` | [[modules\|gpu.rs]] | AMD `gpu_busy_percent`, `mem_info_vram_{total,used}`, `vendor` |
| `/sys/class/powercap/intel-rapl:*/` | [[modules\|gpu.rs]] | iGPU package power (Intel RAPL `uncore` / `gt` domain) |

## debugfs + perf

| Path / syscall | Consumer | Purpose |
|----------------|----------|---------|
| `/sys/kernel/debug/kvm/*` | [[modules\|kvm.rs]] | KVM exit counters (requires root) |
| `perf_event_open(PERF_TYPE_HARDWARE, ...)` + i915 PMU | [[modules\|gpu.rs]] | Intel per-engine `rcs` / `bcs` / `vcs` / `vecs` busy %. Needs `CAP_PERFMON` or root. |

## VFIO / vhost / tap (VM passthrough)

| Path | Consumer | Purpose |
|------|----------|---------|
| `/sys/bus/pci/drivers/vfio-pci/` | [[modules\|passthrough.rs]] | PCI devices bound to VFIO |
| `/dev/vhost-net`, `/dev/vhost-vsock` | [[modules\|passthrough.rs]] | vhost interfaces exposed |
| `/sys/class/net/tap*`, `macvtap*` | [[modules\|passthrough.rs]] | tap interfaces used by QEMU |

## Reading discipline

- Every read uses `fs::read_to_string` or a buffered reader — no
  blocking syscalls beyond the filesystem, no `ps(1)` forks.
- Parsers are pure functions of the string contents so they can be unit
  tested against canned fixtures without a real Linux kernel. See any
  `parse_*` function in [[modules\|host.rs]] for the pattern.
- Missing / unreadable files become `None` / empty-vec; the renderer
  degrades gracefully.

## See also

- [[platforms-macos]] — the equivalent map for macOS
- [[architecture]] — which of these run on every tick vs every 4th
- [[modules]] — module index
