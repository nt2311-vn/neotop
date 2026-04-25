# VM-support plan — neotop's standout feature

> **Status:** design doc, not yet implemented. Tracks the next-phase
> work that turns neotop from "another `htop` clone" into the
> default monitor for hosts running KVM-based virtualisation.

## Why

Every generic process monitor (`htop`, `btop`, `btm`, `procs`) treats
a `qemu-system-x86_64` PID like any other process. From the table you
see RSS, CPU%, and a long argv, but nothing about:

- which VM it actually is (name, UUID, vCPU count, RAM size);
- which host cores its vCPUs are pinned to;
- how busy each vCPU is (different from host-thread CPU%);
- what kernel the guest is running;
- which devices are passed through (VFIO, vhost-net);
- the rate of VM exits (IO / MMIO / HLT) — *the* signal for "is
  this VM thrashing?".

`virt-top` and `virsh` know some of this, but they're libvirt-only,
require XML config, and have no chart layout. `nvtop` and similar
specialised TUIs leave the rest of the host invisible.

**neotop's bet:** be the only TUI that gives you `htop`'s process
view, `btop`'s charts, and a libvirt-grade VM panel — in one screen,
with no daemons or config to set up.

## Scope of "VM kernel support"

We intentionally stay in user-space, reading kernel-exposed surfaces.
No special privileges beyond what the *running* hypervisor already
has. Sources:

| Surface                          | What it gives us                              | Cost                     |
| -------------------------------- | --------------------------------------------- | ------------------------ |
| `/proc/<pid>/cmdline`            | hypervisor flavour + raw config               | already read             |
| `/proc/<pid>/task/*`             | per-vCPU thread, name (`CPU 0/KVM`), affinity | one syscall              |
| `/proc/<pid>/status`             | `Cpus_allowed`, `Mems_allowed`                | already read             |
| `/sys/devices/system/cpu/*`      | host topology for pinning visualisation       | once per launch          |
| `/sys/kernel/debug/kvm/`         | per-VM exit counters (`exits`, `mmio_exits`,  `halt_exits`, etc) | one read of a directory tree |
| `/sys/fs/cgroup/<vm scope>/`     | guest RAM accounting (cgroup memory.current)  | one read                 |
| `/sys/class/vfio/`               | VFIO devices passed through                   | optional                 |
| `/proc/<pid>/fd/`                | open `/dev/kvm`, `/dev/vfio/*`, vhost handles | walk on first detect     |

`/sys/kernel/debug/kvm/` is the killer feature. It exists when the
KVM module is loaded with `kvm.debug=1` (default on modern distros if
debugfs is mounted) and exposes per-VM counters every `nvidia-smi`-like
tool overlooks.

## Hypervisors to detect

Argv-based detection (cheap, no syscalls beyond what we already do):

- **QEMU/KVM**: `qemu-system-x86_64`, `qemu-system-aarch64`, etc.
  Parse `-name`, `-smp`, `-m`, `-cpu`, `-machine`, `-drive`,
  `-netdev`, `-device vfio-pci`. The argv tells us almost everything.
- **Cloud Hypervisor**: `cloud-hypervisor` binary. JSON API on a
  unix socket if `--api-socket` is set; argv flags otherwise.
- **Firecracker**: `firecracker --api-sock <path>`. Pure-JSON API
  on the socket; argv only carries the socket path.
- **crosvm**: `crosvm run`. Similar argv to QEMU.
- **kvmtool / lkvm**: `lkvm run`. Older but still in test rigs.
- **VMware Workstation**: `vmware-vmx` (proprietary; argv only).
- **VirtualBox**: `VBoxHeadless` / `VirtualBox`.
- **libvirt-managed**: detected via the same hypervisor binaries
  but the cgroup path under `/sys/fs/cgroup/machine.slice/` confirms
  it. The libvirt name shows up in the cgroup path.

For the first cut, **QEMU/KVM is the priority** — it's >90% of
production virt and exposes the most information without us needing
a JSON client.

## UI: a fourth list mode

The existing `Flat` / `Tree` / `Group` toggle gains **`Vm`** mode,
bound to `v`. The list reshapes:

```text
 ▼ qemu/kvm  myapp-prod         (4 vCPU, 8 GiB)   62.4%  pinned 8-11
   PID    THR  vCPU  CPU%  MEM    EXITS/s  IO  MMIO  HLT  Q
   12473  18   c0    72.0  ─      4.2k     180 35    1.8k 0
   12474  ─    c1    65.0  ─      3.8k     —   —     —    —
   12475  ─    c2    58.0  ─      4.0k     —   —     —    —
   12476  ─    c3    54.0  ─      3.9k     —   —     —    —

 ▼ firecracker  api-runner-1   (2 vCPU, 256 MiB)   8.2%  pinned 0-1
   PID    THR  vCPU  CPU%  MEM    EXITS/s  IO  MMIO  HLT  Q
   8841   4    c0    4.1   ─      120      8   1     90   0
   8841   4    c1    4.1   ─      118      6   1     92   0

 ▼ system  (47)        2.0%   324 MiB
 ▼ native  (1843)      0.0%   nil
```

Headers carry the **VM name** (parsed from argv `-name guest=...`,
the libvirt cgroup scope, or the firecracker API ID), **vCPU
count**, **RAM allocation** (cmdline `-m`), **aggregate CPU%**, and
host-core pinning shown as a range.

Body rows are one per **vCPU thread** (from `/proc/<pid>/task/*`),
not the whole process. Per-vCPU CPU% is the headline number. **Q**
is queue depth from `vhost-net` — non-zero means the guest is bottlenecked
on packet reception. EXITS/s is the per-second derivative of
`/sys/kernel/debug/kvm/<vm>/{exits,mmio_exits,halt_exits}`.

## Detail pane

When a VM row is selected, the right-hand pane reformats:

```text
 NAME      myapp-prod
 RUNTIME   qemu-system-x86_64 (kvm)
 VM-ID     6e1f-4b3a (libvirt UUID)
 vCPU      4 / pinned 8-11 (Cpus_allowed_list)
 RAM       8.0 GiB max / 6.2 GiB rss
 KERNEL    /var/lib/myapp/vmlinuz-6.6
 INITRD    /var/lib/myapp/initrd
 NET       vhost0 → tap0 (rx 18 MB/s · tx 412 KB/s)
 DEVICES   vfio-pci 0000:01:00.0 (NVIDIA GT 1030)
 CGROUP    machine.slice/machine-qemu-myapp.scope
 ── kvm exits (last second) ──
   exits          4 197
   mmio_exits       180
   io_exits          35
   halt_exits     1 824
   irq_injections   240
   request_irq_exits  0
```

## Phased delivery

### Phase 1 — Detection + basic VM mode (target: ~1 week)

Self-contained features, ~600 lines. No new deps.

- New `vm.rs` module:
  - `enum Hypervisor { Qemu, Firecracker, CloudHypervisor, Crosvm,
    LkvmTool, Other(&'static str) }`
  - `struct VmInfo { hypervisor, name, uuid, vcpus, mem_max_bytes,
    pinned_cpus, host_pid, vcpu_threads: Vec<i32> }`
  - `fn detect(pid: i32, snap: &proc::Snapshot) -> Option<VmInfo>` —
    pure parser over the cmdline + cgroup path + `/proc/<pid>/task`
    listing.
- `groups.rs` gains `Group::Vm(VmInfo)`. Band order:
  Container → **Vm** → Runtime → System → Native.
- `ListMode::Vm` toggled by `v`. Group headers carry the VM-aware
  `vCPU=N RAM=8 GiB` annotation.
- Unit tests with canned `qemu-system-x86_64` cmdline fixtures
  covering: pinning, `-name guest=`, libvirt-style argv, VFIO flags,
  multi-NIC, no-`-name` (auto-name from `-drive file=`).

### Phase 2 — Per-vCPU rows (target: ~3 days)

Walk `/proc/<pid>/task/*` for the VM PID, read each task's
`comm` (looks like `CPU 0/KVM`), `stat` (CPU%), and `Cpus_allowed_list`.
The body rows in Vm mode become per-vCPU. EXITS columns hidden until
Phase 3.

### Phase 3 — KVM debugfs counters (target: ~1 week)

- New `kvm.rs` module:
  - `walk_kvm_debugfs() -> Vec<KvmStats>` — reads
    `/sys/kernel/debug/kvm/<inode>-NN/`. NN matches the qemu PID.
  - Per-VM rates: snapshot at T-1 and T-0, divide.
- Detail pane gets the `── kvm exits ──` block.
- Body table grows EXITS/s columns.
- Graceful degradation: `/sys/kernel/debug/` may not be readable
  (kernel.dmesg_restrict / non-root). Show `—` and don't error.

### Phase 4 — Network + device passthrough (target: ~3 days)

- vhost-net queue depth from `/proc/<pid>/fdinfo/<fd-number>` for
  the vhost-net handles.
- VFIO device list from `/dev/vfio/<group>` open file descriptors.
- Detail pane DEVICES + NET sections.

### Phase 5 — Live histories (target: ~2 days)

VM-aware `host_history` companion: per-VM CPU% sparkline that
**replaces** the host CPU sparkline when a VM row is selected. Same
60-sample ring, painted in the same cell.

### Out of scope (deliberate)

- **libvirt API client** (would make us depend on libvirt-dev). We
  parse the cgroup path instead — same info, no link-time dep.
- **Guest-internal stats** (would require a guest agent). Host-only.
- **VM lifecycle commands** (start/stop/migrate). neotop is a
  read-only observer — that's the contract.
- **Windows VM detection** (Hyper-V is a different code path
  entirely; only relevant if we port to Windows).

## Risks / open questions

1. **`/sys/kernel/debug/kvm/` permissions.** Often root-only. Plan:
   feature-detect on launch, fall back to "EXITS column says —"
   silently. Add a one-line footer hint on first hit ("ℹ run as root
   to see KVM exit counters").
2. **VM-name parsing fragility.** QEMU argv has many forms:
   `-name myvm`, `-name guest=myvm`, `-name guest=myvm,debug-threads=on`,
   no `-name` at all. The code path in v0.7.0 (`compact_temp_label`)
   shows we already handle this style of fixture-driven parsing —
   apply the same testing discipline here.
3. **Per-vCPU thread enumeration cost.** Walking
   `/proc/<pid>/task/*` for each VM is ~5 syscalls per vCPU. On a
   host with 100 VMs × 8 vCPUs that's 4000 reads per slow tick. Cap
   at 5 ms wall and lazily stop walking if we exceed.

## What "standout" means here

After Phase 5 lands, the pitch is:

> **One TUI, one terminal. Host CPU + GPU + every running VM with
> per-vCPU CPU%, KVM exit rates, pinned cores, VFIO devices —
> updated 1 Hz. No daemons, no config, no XML. Reads only public
> kernel surfaces.**

That's a feature `htop` / `btop` / `btm` / `virt-top` / `nvtop`
collectively don't deliver.
