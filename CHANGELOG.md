# Changelog

All notable changes to **neotop** are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.21.1]

### macOS support

Cross-platform compilation and basic functionality on macOS. The codebase now compiles and runs on macOS with platform-specific data sources:

- **host.rs** — CPU count, memory, load averages via `sysctl`
- **proc.rs** — process info via `libproc` (`PROC_PIDTASKINFO`)
- **procs.rs** — process list via `proc_listallpids`
- **Linux-only modules** — battery, disk, net, temp, gpu, elf return empty data on macOS (full implementations deferred)

CI now includes macOS builds (allowing failures as the implementation is basic).

### Platform abstraction

- Moved `rustix` to Linux-only dependencies
- Added `libc` for macOS system calls
- Added `#[cfg(target_os = "macos")]` guards throughout
- Updated module documentation to note platform differences

## [0.20.1] — 2026-04-26

### crates.io release prep

First release published to crates.io. No source changes — only
metadata + README polish so the project surfaces correctly in the
crates.io UI and `cargo install neotop --locked` produces the
right binary.

`Cargo.toml`:

- Added `homepage` (mirrors `repository`).
- Refreshed `description` from "container groups" to the current
  scope: *Linux TUI for host metrics, processes, GPU activity,
  containers, and KVM virtual machines*.
- Swapped the `system` keyword for `kvm` (more discoverable).
- Added two crates.io categories: `os::linux-apis` and
  `visualization`.
- Added an explicit `include` allowlist so the published tarball
  ships **only** what an installer needs: `src/`, `Cargo.toml`,
  `Cargo.lock`, `LICENSE`, `README.md`, `CHANGELOG.md`. This
  cuts ~50 KB of dev-only files (`.github/`, `VMPLAN.md`,
  `deny.toml`, `justfile`) that have no meaning outside the
  GitHub repo. Tarball is now 23 files / 498 KB / 154 KB
  compressed.

`README.md`:

- Six badges at the top: crates.io version + downloads, license,
  CI, CodeQL, MSRV.
- `cargo install neotop --locked` is now the primary install
  command; `--git` and `--path` follow as alternatives.
- Fixed stale **MSRV claim** (was "1.80", actually 1.88 since
  `v0.17.1`).
- Refreshed module list — added `vm.rs`, `vcpus.rs`, `kvm.rs`,
  `passthrough.rs`, `elf.rs` (all shipped in 0.16+ but missing
  from the architecture table).
- Description now mentions Intel GPU, KVM VMs, vCPU pinning, and
  VFIO passthrough — features that landed in `v0.16` through
  `v0.19` but weren't reflected in the headline paragraph.
- Added `Documentation`, `Contributing`, refreshed `License` and
  `Roadmap` sections. Roadmap now distinguishes "still open"
  (themes, per-engine Intel GPU, SMT/NUMA, OS ports) from
  "recently shipped" with version anchors.

### Removed: Snyk from the `security` CI job

Snyk does not support Rust. Two independent failures confirmed
this: their per-language `snyk/actions/rust@master` was removed
from the Supported Actions list in 2024 (only Node, Python, Ruby,
Go, Maven, Gradle, PHP, .NET, etc. remain), and the standalone
CLI errors with `Could not detect package manager for file:
Cargo.lock` because there is no Cargo plugin.

Rather than maintain a permanently skipped step or wire up
`snyk test --unmanaged` (which fingerprints unmanaged C/C++
binaries and would only flag the binary's own NVML dlopen, not
its Cargo deps), the `security` job now drops Snyk entirely.
The remaining stack covers everything Snyk would have:

- `cargo audit` — RustSec advisory DB (the same DB Snyk reads
  for its other ecosystems).
- `cargo deny` — licenses, dup versions, banned crates, sources.
- CodeQL — first-party SAST with data-flow analysis.
- Semgrep — pattern-based SAST.
- OpenSSF Scorecard — supply-chain best-practices scoring.
- Dependabot — automated dep-bump PRs.

Net change to CI surface: minus one always-failing step. The
`HAS_SNYK_TOKEN` env var is gone; you can revoke the
`SNYK_TOKEN` repo secret at your leisure (it's now unused).

### Fixed: CodeQL Rust uses buildless extractor (`build-mode: none`)

The initial `codeql.yml` configured `build-mode: manual` paired
with an explicit `cargo build --release --locked` step. CodeQL's
Rust extractor is **buildless** — it reads source files directly
without invoking cargo — and rejects `manual` and `autobuild`
with `A fatal error occurred: Rust does not support the manual
build mode.`

`build-mode: none` is the only accepted value. With that flip,
the explicit `cargo build` step plus the `dtolnay/rust-toolchain`
and `Swatinem/rust-cache` steps that fed it are all redundant
and have been removed. Net effect: ~30 seconds shaved off every
CodeQL run.

### Verification

`cargo publish --dry-run` succeeds; `cargo audit`,
`cargo deny check`, full `cargo test --all-targets --locked`
(176 tests) all clean. `actionlint` clean on the two patched
workflows.

## [0.20.0]

### Public-repo hardening — CODEOWNERS + CodeQL + Scorecard + Dependabot

Repo is now ready to flip to public on GitHub. Six new files
together encode the security posture and merge policy:

- **`.github/CODEOWNERS`** — catch-all `* @nt2311-vn`. Combined
  with branch protection's `require_code_owner_reviews`, every
  PR auto-requests owner review; no other path to merge.
- **`.github/dependabot.yml`** — weekly Monday `cargo` + weekly
  `github-actions` updates, both grouped to one PR per ecosystem
  for minor/patch bumps. Auto-assigned to `nt2311-vn`. Major
  bumps stay individual (often need code-side changes).
- **`.github/SECURITY.md`** — private disclosure policy via
  GitHub Security Advisories or maintainer email; documented
  threat model (no network, no exec, no arbitrary dlopen besides
  NVML); 48h ack / 7d severity / 30d fix targets.
- **`.github/pull_request_template.md`** — checklist for
  incoming PRs (fmt, clippy, tests, audit, deny, smoke).
- **`.github/workflows/codeql.yml`** — GitHub's first-party SAST
  with data-flow analysis; security-extended + security-and-quality
  query suites; SARIF uploaded to the Security tab; weekly
  retro-scan picks up new advisories CodeQL adds upstream.
- **`.github/workflows/scorecard.yml`** — OpenSSF Scorecard for
  supply-chain best-practice scoring; runs on push, weekly, and
  on `branch_protection_rule` events; publishes to the public
  Scorecard API for badging.
- **`.github/setup-branch-protection.sh`** — one-shot `gh api`
  bootstrap. Run once after going public to apply: required PR,
  CODEOWNERS-required review, four required status checks
  (`check (stable)`, `check (1.88)`, `security`,
  `codeql analyze (rust)`), strict up-to-date branch, linear
  history, no force-push, no deletion, conversation-resolution
  required. Idempotent — re-runnable without side-effects.

### Why this enforces "only the owner reviews and merges"

Three layers, designed so failure of any one is recoverable:

1. **Permission model.** Personal-account public repos give only
   the owner `write` access by default. Outside contributors fork
   and PR; only the owner can click *Merge*.
2. **Branch protection.** `main` rejects direct pushes, requires
   CODEOWNERS review, requires the four CI status checks above.
3. **CODEOWNERS auto-request.** Every PR routes to `@nt2311-vn`
   for review without manual triage.

`required_approving_review_count` is set to **0** (not 1+)
deliberately — GitHub forbids self-approval, so a non-zero count
on a solo project would deadlock every PR the owner writes
themselves. The permission model carries the "only I merge"
guarantee; CODEOWNERS provides the auditable trail.

### Verification

- `actionlint` clean across all four workflows.
- `bash -n` clean on `setup-branch-protection.sh`.
- `jq` parse clean on the embedded protection payload.
- `cargo audit` + `cargo deny check` still pass on the new lock.

### Rollout instructions

1. Push this release.
2. Flip the repo to public in *Settings → General → Danger
   Zone*.
3. Authenticate `gh` as the owner: `gh auth status`.
4. From the repo root: `bash .github/setup-branch-protection.sh`.
5. Apply the companion UI settings the script prints at the end
   (squash + rebase merging, dependabot alerts, secret scanning,
   private vulnerability reporting).

## [0.19.1] — 2026-04-26

### CI: supply-chain + SAST scans

New parallel `security` job in `.github/workflows/ci.yml`:

- **`cargo-audit`** (`rustsec/audit-check@v2.0.0`) — RustSec
  advisory DB scan over `Cargo.lock`. Free, no auth.
- **`cargo-deny`** (`EmbarkStudios/cargo-deny-action@v2`) —
  supply-chain policy: licenses, duplicate versions, banned
  crates, sources, advisories. Configured in the new top-level
  `deny.toml`.
- **Semgrep SAST** (`semgrep/semgrep-action@v1`) — Rust + secrets
  + general security-audit rule packs. Uploads SARIF to the
  Security tab on public repos. Optional `SEMGREP_APP_TOKEN` for
  Semgrep Cloud dashboarding.
- **Snyk** (`snyk/actions/rust@master`) — gated on `SNYK_TOKEN`
  via a job-scoped `HAS_SNYK_TOKEN` flag so token-less PRs and
  forks stay green. Hard-fails on `--severity-threshold=high` when
  the token is set.

`deny.toml` ships with two **explicit waivers** for transitive
findings via `ratatui 0.29` — both informational, not CVEs:

- `RUSTSEC-2024-0436` — `paste 1.0.15` is unmaintained
  (functionally complete; tracked for ratatui to migrate to
  `pastey`).
- `RUSTSEC-2026-0002` — `lru 0.12.5` `IterMut` Stacked Borrows
  unsoundness (we don't call `IterMut`; tracked for ratatui to
  bump to `lru ≥ 0.13`).

### Why no Valgrind

Explicit non-decision documented in the workflow. The codebase
has zero `unsafe` blocks (`rg unsafe src/` returns nothing).
Without `unsafe`, Rust's ownership model rules out the
use-after-free, double-free, and raw-pointer leak classes
Valgrind exists to catch — running it over `cargo test` mostly
yields false positives from libstd's allocator caching and
libnvml's dlopen path. The Rust-native equivalents that *do*
find things in this codebase (`cargo audit`, `cargo deny`, and
— if we ever introduce raw FFI — `cargo miri`) are wired up
above. The workflow comments capture the rationale so a future
contributor doesn't re-litigate the same decision.

### Verification

`cargo audit` and `cargo deny check` both pass locally. CI
workflow validated with `actionlint`. No source changes.

## [0.19.0] — 2026-04-26

### Intel iGPU busy% via RC6 residency

The last vendor gap closes. Intel `i915` cards now report a coarse
busy% derived from `/sys/class/drm/card*/gt/gt0/rc6_residency_ms`
— RC6 is the deepest GPU power-save state, time accrues there
when the engine is idle, so `busy% ≈ (1 − ΔRC6 / Δwallclock) ×
100`. Same fallback `intel_gpu_top` uses when it can't open
`i915_pmu` perf events; works without root, without
`CAP_PERFMON`, without ioctls.

The `Tracker` now keeps a per-card RC6 sample (24 B each, keyed
by canonical PCI address) so busy% is a proper derivative across
ticks. First sample seeds the cache; busy% lands on the second
tick. Cards that disappear between ticks have their cache entry
purged.

Falls back to the legacy `power/rc6_residency_ms` path on older
kernels. Per-engine breakdown (rcs / bcs / vcs / vecs) and Intel
power draw stay deferred — both need ioctls or perf events.

Verified live on a Comet Lake-H UHD: idle iGPU shows ~0.1% busy,
matching `intel_gpu_top -s 1`.

### Tests

3 new unit tests in `gpu.rs` cover the busy% derivative (typical
case → 80%), the over-report clamp (RC6 delta exceeds wallclock
delta → 0%), and the skip-on-counter-reset / zero-window guards.
176 tests passing total.

## [0.18.1]

### CI green again

`v0.18.0` shipped a `Cargo.toml` version bump but the matching
`Cargo.lock` refresh wasn't committed. Both CI jobs (`check
(stable)` and `check (1.88)`) run with `--locked` and refused to
update the lock file, failing with `the lock file needs to be
updated but --locked was passed to prevent this`.

This patch release re-syncs `Cargo.lock` with `Cargo.toml`. No
code changes.

## [0.18.0] — 2026-04-26

### VM Phase 4 — passthrough surface (VFIO + vhost + tap)

New module `passthrough.rs`. When a VM row is selected, walk
`/proc/<pid>/fd/` once per tick (~one `readlink` per fd, single
digits of syscalls per VM) and surface three things in the detail
pane:

- **VFIO devices.** Each open `/dev/vfio/<group>` resolves to the
  IOMMU group via `/sys/kernel/iommu_groups/<group>/devices/`,
  then each PCI BDF gets vendor / device / class IDs from
  `/sys/bus/pci/devices/<bdf>/`. Banner format:
  `vfio:24  0000:01:00.0 NVIDIA 10de:1d01 [display]`. Sibling
  functions in the same IOMMU group (e.g. GPU + its HDMI audio)
  indent under the first row.
- **vhost back-ends.** Open `/dev/vhost-{net,vsock,scsi,fs}`
  handles render as `vhost:vhost-net`, etc. We deliberately don't
  reach for queue depth — that's an `ioctl` surface and would
  break the "observe via /proc and /sys only" contract neotop
  ships with.
- **Tap interfaces.** `/dev/net/tun` fds carry the interface name
  in `/proc/<pid>/fdinfo/<fd>` as `iff:<name>`; we cross-reference
  with the existing `net::Tracker` snapshot so each tap row
  carries live `rx 18 MB/s · tx 412 KB/s` rates.

A vendor-name lookup ships in the binary for the eight vendors
worth recognising in passthrough scenarios (NVIDIA, Intel, AMD,
Mellanox, Broadcom, Realtek, Red Hat, Marvell, Samsung,
ASMedia). Everything else renders the bare `vendor:device` IDs —
the user can run `lspci -s …` for more.

### VM Phase 5 — per-VM CPU sparkline

The first sparkline cell in the host history strip now switches
from host CPU% to **per-guest CPU%** when a VM row is selected.
Same 0..=100 scale, same width, just a different data source —
the eye reads the chart the same way the host one does. Title
becomes `qemu/kvm myapp 62%` instead of `CPU 62%`.

The ring is the *mean* of per-vCPU CPU% values from the existing
`vcpu_tracker` (no extra walks of `/proc/<pid>/task`). Switching
to a different VM clears the ring; switching to a non-VM row
restores the host chart. 60 samples = the last minute at the
default 1 s tick.

### Tests

7 new unit tests in `passthrough.rs` cover PCI label rendering
(known + unknown vendors), class-code categorisation, vhost
labels, the `Default::is_empty` contract, the nonexistent-PID
fast path, and `0xABCD\n`-style hex parsing. 173 tests passing
total.

## [0.17.1] — 2026-04-26

### CI green again

Maintenance release. Fixes the `check (stable)` and `check (1.80)`
GitHub Actions jobs which had drifted on two fronts:

- **MSRV bumped from 1.80 to 1.88** — transitive deps
  (`darling 0.23`, `instability 0.3.12`, `unicode-segmentation
  1.13`) now require rustc 1.88, and the `--locked` build can't
  resolve around them. The 1.80 promise was speculative; the
  project never had a documented user blocked on it. Bumping is
  the honest fix.
- **9 new clippy lints (rustc 1.95)** addressed:
  - `host.rs`, `procs.rs`: `map(...).unwrap_or(...)` →
    `map_or(...)`
  - `procs.rs::sort_rows`: `sort_by` for Mem / Pid → `sort_by_key`
    (with `Reverse` for the descending case)
  - `main.rs`: `Duration::from_millis(5000)` →
    `Duration::from_secs(5)`
  - `main.rs`: three `collapsible_match` arms (Ctrl+K SIGKILL,
    Shift+K SIGTERM, the filter-mode char-typing arm) — moved
    the inner `if` into the match guard
  - `main.rs`: stray trailing comma in a `format!` call
  - `groups.rs::ContainerNames::refresh_if_stale`:
    `Option::map_or(true, …)` → `Option::is_none_or(…)`
    (now stable since 1.82)

No behavior change. 166 tests still pass. CI matrix updated to
test `stable + 1.88` instead of `stable + 1.80`.

## [0.17.0]

### Group view: per-app sub-buckets inside the runtime band

Splitting on language alone repeated the same "misleading totals"
problem `native` and `system` had: every Rust binary on the host
(neotop, alacritty, ripgrep, target/debug builds) collapsed into
one `rust [async/threads]` row whose aggregate CPU + RSS pushed
it to the top regardless of which specific app was actually busy.
Same for Go, same for Java when several jars run side-by-side.

`Group::Runtime(Lang)` is now `Group::Runtime(Lang, String)` where
the second field is the app identifier. Each `(lang, app)` pair
gets its own bucket, header, and aggregate, so a hot `caddy` Go
process floats above a quiet `syncthing` even though both are Go.

App-extraction strategy per language:

- **Go / Rust** (compiled): executable basename — the binary *is*
  the app. Runs on the ELF-detected upgrade path in
  `procs::Tracker`.
- **Java**: `-jar foo.jar` → `foo.jar`; otherwise the last
  non-flag token is the main class. `-cp` / `--class-path` value
  pairs are eaten so a classpath isn't mistaken for the entry
  point.
- **Python**: `-m foo.bar` → `foo.bar`, `-c '…'` → `(inline)`,
  otherwise the script's basename.
- **Node / Bun / Deno / Ruby / PHP / Perl / Lua / R / .NET**:
  basename of the first non-flag argument (the script).
- **Erlang**: empty — `beam.smp` cmdlines are too varied to
  parse reliably; processes still cluster as one `erlang
  [actors]` group.
- **Empty app**: falls back to the bare `lang [signature]` form,
  so the case where we can't identify a script (`python3` REPL,
  `node --inspect`) still produces a single useful group rather
  than a `:` bare-suffix label.

Header format: `<lang>:<app> [<signature>]` — same shape as
container labels (`docker:abc12`), so the eye reads them
consistently. E.g., `▼ rust:neotop [async/threads]  (3)`,
`▼ java:app.jar [vthreads]  (1)`, `▼ python:server.py [GIL+asyncio]  (4)`.

8 new unit tests cover Java jar / main-class / classpath, Python
`-m` and `-c` and script forms, Node first-non-flag selection,
compiled-language argv0 basename, the empty-app fallback, and a
regression test confirming two distinct Rust binaries produce
different `sort_key`s (the bug this commit fixes).

## [0.16.0]

### VM Phase 3 — KVM exit counters

The "standout" feature from `VMPLAN.md` is live. New module
`kvm.rs` reads `/sys/kernel/debug/kvm/<pid>-<inode>/` for the
selected VM and computes per-second rates of:

- `exits` (total VM exits)
- `mmio_exits` (device-emulation cost)
- `io_exits` (legacy port-IO emulation)
- `halt_exits` (guest idle)
- `irq_injections` (host→guest interrupts)

These show up in the detail pane as a `── kvm exits ──` block
when a VM row is selected. `htop`/`btop`/`btm` show none of this
— it's the single best signal for "this guest is thrashing"
visible from the host without a guest agent.

Permissions are root-only on most distros. The tracker
feature-detects on construction; non-root users see a single hint
line ("(run as root for /sys/kernel/debug/kvm)") instead of a
table of —. No errors are logged.

5 unit tests cover rate computation, counter resets (live
migration), zero-dt safety, the unavailable-tracker fast path,
and dead-pid purging.

### Per-process disk I/O (R/s, W/s)

`procs::Tracker` now reads `/proc/<pid>/io` per tick and
EMA-smooths the `read_bytes` / `write_bytes` deltas with the same
α=0.5 curve used for CPU%. Two new columns in the proc table
(R/s, W/s) plus dedicated lines in the detail pane (`DISK R`,
`DISK W`).

- Permission-aware: `/proc/<pid>/io` is owner-only without
  `CAP_SYS_PTRACE`. Foreign-uid rows render `—`; same column,
  silent fallback.
- Compact 8-char rendering (`4.2K`, `38M`, `1.2G`) keeps the row
  budget intact at 80 cols.
- Idle processes show blank rather than `0` so the eye isn't
  drawn to a wall of zeros.
- Three new tests cover EMA convergence, decay-on-loss, and the
  None-on-first-sample contract.

### Live thread context for runtime groups

The `THREADS` line in the detail pane now appends the runtime's
concurrency signature when the process is in a `Runtime` band:

- `THREADS  18 (goroutines)`
- `THREADS  24 (vthreads)`
- `THREADS  4 (event loop)`

Turns the static `[signature]` tag introduced in v0.15.0 into a
live signal — the user can map "this process has 18 OS threads"
to "the Go runtime is multiplexing N goroutines onto them" at a
glance.

### Sort key visible in tree / group titles

The proc-table title now carries the active sort tag (`CPU%↓`,
`RSS↓`, `PID↑`, `CMD↑`) in **every** mode, not just flat. Hitting
`s` in tree or group mode used to be silent — the rows reshuffled
but nothing in the chrome told you what the new key was.

### Group view: drop the misleading "native" / "system" totals

In `g`-mode the synthetic banner row used to aggregate every static
host binary into a single `▼ native (N)` line — and the same for
kernel daemons under `▼ system`. Because every runtime-less process
on the laptop landed in one of those two buckets, that row was
**always** the largest CPU + RSS in the table and read like a real
workload when it isn't. The banners are gone for both bands;
their members render as flush-left rows in the same band slot.
Container, VM, and Runtime banners still appear (those are
genuinely cohesive workloads).

### Group view: language signatures for Go, Rust, and friends

`procs::Tracker` now opens `/proc/<pid>/exe` and parses ELF section
headers when classification would otherwise fall through to
`Native`. Two new `Lang` variants:

- **Go** — detected via `.note.go.buildid` / `.gopclntab` /
  `.go.buildinfo` section names (every modern `go build` writes
  at least one of those).
- **Rust** — detected by searching `.rodata*` sections for
  `library/std/src/` or `/rustc/` (panic-location strings that
  ride along even after stripping), with a symbol-table fallback
  for unstripped binaries (`_RNv` v0 mangling, legacy `..llvm.`
  suffix).

The runtime label now carries a one-token concurrency-model tag —
`go [goroutines]`, `rust [async/threads]`, `java [vthreads]`,
`node [event loop]`, `python [GIL+asyncio]`, `bun [event loop]`,
`erlang [actors/BEAM]`, etc. — so the user can tell at a glance
*how* a runtime spends its time, not just which one is running.

New module `elf.rs` (200 LOC, safe Rust, no new crates) holds the
ELF64 LE parser. Detection is a one-shot `O(K)` read amortised
across the lifetime of the pid — already cached alongside the
`cmdline` and `cgroup` fields in `procs::StaticInfo`. Steady-state
CPU cost is zero.
8 unit tests cover the substring search, the name-table walker,
and the I/O failure paths.

### Inline braille mini-charts for GPU and disk panels

GPU and disk overview rows now carry an 8-cell Unicode braille
(U+2800..U+28FF) mini-chart inline next to the device numbers.
Each cell encodes 2 horizontal samples × 4 vertical levels, so
8 chars = ~16 s of history at the default 1 Hz tick. Zero added
vertical space — the strip reads as a continuous trend line.

- New per-device history rings in `HostHistory.gpu_busy_per_card`
  and `HostHistory.disk_rate`, keyed by stable identifier (PCI
  addr / kernel device name). Auto-pruned when devices disappear.
- `braille_line(samples, max, cells)` helper does the rendering.
  GPU uses fixed `max=100` for percentage; disk auto-scales to
  per-disk peak so a quiet SSD next to a saturated HDD both show
  readable charts.
- Color follows the same load ramp as the per-core spectrum
  (DarkGray → Green → Yellow → Red).
- 5 unit tests covering padding, blank chart, top-row, bottom-row.

### VM Phase 2 — per-vCPU CPU% in the detail pane

When a VM PID is selected, the detail pane now shows a
`── vcpus ──` block with one row per guest CPU thread. New module
`vcpus.rs`:

- Walks `/proc/<pid>/task/*` for the selected VM only (cheap on
  hosts with many guests).
- Identifies vCPU threads by matching `comm` against per-hypervisor
  patterns: QEMU `CPU N/KVM`, Firecracker `fc_vcpu N`, Cloud
  Hypervisor `vcpuN`, crosvm `crosvm_vcpuN`, lkvm `kvm-vcpuN`.
- Computes per-thread CPU% via the same delta-of-jiffies math
  `procs::Tracker` uses for whole processes (`utime+stime` from
  `/proc/<pid>/task/<tid>/stat`).
- Renders index, host tid, percentage, and inline 8-cell gauge —
  same look as the per-core CPU spectrum, so guest hot vCPUs jump
  out exactly the way host hot cores do.

6 unit tests pin the comm-pattern parser for all five hypervisors
plus a "rejects unrelated threads" case.

## [0.14.0]

### Group view sorts by aggregate CPU / MEM

In `g`-mode (group), groups are now ordered by their **aggregate
CPU% (or RSS)** when sorted by CPU / MEM, regardless of band. A
native binary pegging 80% floats above a Docker group at 5% — much
more useful than the old "containers always first" rule when you're
hunting the actual hot path. PID / Command sorts still respect band
priority (`container > vm > runtime > system > native`) since
aggregates aren't a stable key for those.

### Dashboard upgrades

- **CPU spectrum: 2-column layout.** Wide terminals now split the
  per-core spectrum into two columns side-by-side; an N-core box uses
  ⌈N/2⌉ rows instead of N. Auto-falls back to 1 column on narrow
  terminals (`spectrum_cores_per_row` decides at render time). Tests
  cover both shapes plus the term-height cap.
- **GPU: VRAM% sparkline + watts in title.** When a card reports
  VRAM, a 6th sparkline cell appears next to GPU%. Power draw shows
  in the GPU title (` GPU 17% 42W `). New
  `gpu::aggregate_vram_pct` / `aggregate_power_watts` helpers
  aggregate across multi-card setups.
- **NET: top-iface label.** The NET↓ / NET↑ titles now carry the
  name of the busiest interface (` NET↓ 18.5 MB/s wlp4s0 `) so
  you see which link is doing the talking. The summed total is
  unchanged — the math has always been right; this just attributes
  it.

### Phase 1 of VM support landed

- **New `vm.rs` module.** Pure parser over the joined cmdline:
  detects QEMU/KVM (`qemu-system-*`, `qemu-kvm`), Firecracker,
  Cloud Hypervisor, crosvm, lkvm. Pulls VM name (`-name guest=…`,
  `--id`, fallback to `-drive file=…` or `--api-sock`), vCPU count
  (`-smp N` / `-smp cpus=N` / `--cpus boot=N`), memory cap (`-m 8G`
  / `-m size=…` / `--memory size=…`). 10 unit tests cover real-world
  argv shapes including libvirt-style multi-field `-name` values.
- **`Group::Vm(VmInfo)`.** New band slotted between Container and
  Runtime in the priority order (`container > vm > runtime > system
  > native`). Headers render as `qemu/myapp-prod (4 vCPU, 8.0 GiB)`
  in `LightBlue`. `g`-mode (group) view auto-clusters VMs.
- **Detail pane shows VM info** when a VM PID is selected: `VM
  qemu/myapp-prod (4 vCPU, 8.0 GiB)`.

This is the *standout* feature — every other host TUI lumps a
`qemu-system-x86_64` PID with the rest of the process noise. neotop
now shows it as a first-class VM with the headline config you want
at a glance. Phases 2-5 (per-vCPU CPU%, KVM exit counters, vhost-net
queue depth, VFIO passthrough) are still pending; see `VMPLAN.md`.

### Critical fix: container-name resolution no longer freezes the UI

`ContainerNames::refresh_if_stale` was shelling out to `docker ps`
and `podman ps` synchronously on the slow tick. When either daemon
was unhealthy (which the user reproducibly hit) the UI thread
blocked indefinitely — `q` couldn't quit, no charts rendered,
ctrl-C was the only way out. Same root cause as the v0.13.0 hwmon
freeze, just a different blocking syscall.

- **`groups::ContainerNames`** is now a worker-thread + channel
  pair, mirroring `temp::TempWorker`. Calls into
  `refresh_if_stale` queue a request and return immediately;
  results flow back via `try_recv` on the next tick.
- **1-second hard timeout** per `docker ps` / `podman ps`
  invocation via `Command::spawn` + `try_wait` polling. A wedged
  daemon now caps at 1 s of worker-thread idle, never the UI thread.
- The TTL-based coalescing of v0.13.0 is preserved (the worker
  swallows pending requests if multiple piled up while it was
  scanning).

### Comment cleanup

Many narrative-style comments compressed to one-liners; module-level
doc, struct field docs in `App`, and verbose explanatory blocks in
`tick()` / `run()` / `handle_normal_key()` / `draw_main()` shrunk.
Net ~120 lines removed from `main.rs`. Genuinely non-obvious notes
(MSRV constraints, ordering requirements, race windows) preserved.

### VMPLAN.md

New design doc for the next phase: per-VM grouping (`v` toggle),
per-vCPU CPU%, KVM exit-counter readout from
`/sys/kernel/debug/kvm/`, vhost-net queue depth, VFIO passthrough
detection. Breaks down into 5 phases. The standout pitch: one TUI
that gives you `htop`'s process view + `btop`'s charts + libvirt-grade
VM panel, no daemons, no config, public kernel surfaces only.

### Project decoupled from neosandbox

neotop is now a standalone Linux TUI in the lineage of `htop` /
`btop` / `btm`, no longer tied to the neosandbox microVM project
that birthed it. Dropped:

- **Vms view**: `VmRow`, `Exits`, `StateFile`, `scan()` (which
  walked `$NEOSANDBOX_STATE/run/*/state.json`), `draw_vms`,
  `draw_vms_empty`, `draw_serial`, `draw_resources`,
  `draw_resources_text`, `draw_cpu_sparkline`, `draw_table`,
  `draw_title`, `phase_style`, `one_line`, `format_uptime`,
  `now_ns`, `delete_halted_state`. The `View` enum and the
  `Tab` view-cycle key are gone — there's only one view.
- **CLI**: `--state-dir` flag and `NEOSANDBOX_STATE` env var.
  `Args` is now just `--refresh-ms`.
- **Key handlers**: `Tab` (was view cycle) and `x` (was delete
  halted-VM state file). All `if app.view == View::Procs`
  guards on `s` / `t` / `g` / `/` / `K` are dropped — those
  keys now apply unconditionally.
- **Deps**: `serde` and `serde_json` (only used to parse the VM
  `state.json` schema). `Cargo.lock` re-resolved.
- **`kvm_available`**: the `/dev/kvm` presence indicator is
  gone from `host::HostInfo` and the host overview line. Not
  relevant to a generic process / host monitor.
- **`CpuSample` + `CpuHistory`**: per-VM-PID CPU history rings
  fed the now-removed `draw_cpu_sparkline`. Host-wide
  `HostHistory` (CPU / MEM / NET / GPU + per-core) stays — that
  drives the always-visible sparklines and spectrum view.
- **PLAN.md**: deleted. Historical implementation plan from the
  neosandbox-targeted era; CHANGELOG carries the project record now.
- **README.md**: rewritten from scratch as a generic Linux TUI
  description. No more "neosandbox", "fleet", "vmmd", microVM
  references.
- **Cargo.toml**: `description` and `keywords` updated. Dropped
  `kvm`, `microvm` keywords; added `process`, `system`, `gpu`.

### Tests

- 117 passing — same coverage as before the decoupling, since
  every removed function had no unit tests (they were view-layer
  rendering code) and the underlying parsers / data sources are
  unchanged.

### "Responsive on broken firmware" pass

Two reports from the real machine after v0.13.0 landed:

1. `q` didn't quit reliably. The footer advertises `q quit` but
   the global guard only fired in `Normal` mode, so any
   accidental `?` (Help) or `Ctrl+K` (kill prompt) consumed the
   first `q` as a popup-dismiss.
2. `acpitz` on the user's HP / Dell-class laptop took 3025 ms on
   the first sysfs read and the entire UI froze for 3 seconds at
   startup. To make matters worse, the parked-sensor message
   showed up as `⚠ (1 err)` in the footer for the rest of the
   session — exactly the alarm-style rendering reserved for real
   `/proc` failures.

### Added

- **`temp::TempWorker`** — dedicated worker thread that owns the
  hwmon `Tracker` and exchanges scan requests / results with the
  UI thread over a pair of `mpsc::channel`s. The UI thread never
  blocks on a sysfs read, even when the kernel's ACPI thermal
  mailbox takes 3 s to answer. Coalesces queued requests so a
  busy slow-tick can't pile up a backlog of scans.
- **`errors::Severity { Info, Warn }`** — two-tier classification
  for non-fatal events. `Warn` keeps the loud red `⚠ (N err)`
  treatment; `Info` renders as a quieter yellow `ℹ` and does NOT
  count toward the lifetime error total. Honest signal, no
  panic-by-styling.
- **`ErrorRing::push_info`** sibling to `push`. The 30-odd
  existing call sites that meant "this is a real warning" stay
  on `push` and get the same red treatment they always had.
- **`temp::PollOutput { infos, errors }`** — explicit named
  struct instead of returning a bare tuple, so the call site in
  `App::tick` reads `for (k, m) in out.infos { push_info(...) }
  for (k, m) in out.errors { push(...) }` and the routing is
  obvious.

### Changed

- **`q` now quits from any non-`Filter` mode.** Previously the
  global guard required `InputMode::Normal`; now Help / Confirm
  also exit on `q`. Only the filter prompt still treats `q` as
  literal text (because that's what the user is typing). Aligns
  with what the footer advertises.
- **Slow temp scan moved off the UI thread.** `App::new` spawns
  `TempWorker` and primes the first scan; `App::tick` polls
  results on every tick and routes them to the right severity.
  The `Tracker::snapshot` adapter (the old single-tier sync API)
  is gone — only the channel-friendly `scan` remains.
- **Footer `(N err)` counts only `Warn` events.** Parked sensors,
  throttled scanners, and other self-protection notices still
  appear in the badge area but no longer poison the cumulative
  count.
- **`Tracker::snapshot` removed.** Direct callers were already
  zero in production code; the only caller (a unit test) now
  uses `scan()` directly. `ErrorRing` import dropped from
  `temp.rs`.

### Tests

- 117 passing (was 111, +6):
  - `temp_worker_initial_readings_are_empty_until_first_poll`
  - `temp_worker_poll_returns_results_after_request`
  - `temp_worker_request_is_idempotent_while_in_flight`
  - `push_uses_warn_severity_by_default`
  - `push_info_does_not_count_toward_error_total`
  - `push_warn_and_info_count_separately`

### Fixed

- `q` not quitting from Help / Confirm modes.
- 3-second UI freeze on startup when `acpitz` (or any other slow
  hwmon sensor) was present.
- `⚠ (1 err)` badge persisting after a parked-sensor info event,
  misrepresenting a successful self-heal as an ongoing failure.
- **"Where are the CPU / GPU charts?"** — the per-core spectrum
  (sparkline + live % + gauge per core) was off by default and
  the `H` toggle was gated to Procs view, so a user defaulting
  into Vms had no path to see the impressive layout. Now
  spectrum is **on** by default, `H` works from any view, and
  the toggle is advertised in the help bar with a state-aware
  label (`H spectrum` / `H grid`).
- **Vms history-strip threshold** lowered from 28 → 22 rows so
  the CPU / MEM / NET / GPU sparklines render on typical 24-row
  SSH sessions instead of being silently suppressed.

### Repository hygiene

- **README CI badge** removed — the repo is private, so GitHub's
  badge endpoint always returns 404 to anonymous viewers. Re-add
  the line when / if the repo is flipped public.
- **GitHub slug typo** fixed across `README.md`, `Cargo.toml`,
  `CHANGELOG.md`, and `PLAN.md`: `nt2311` → `nt2311-vn`.

## [0.13.0] — 2026-04-25

The "charts everywhere + readable container names" release. Two
shipped problems from v0.12.0:

1. The CPU spectrum / GPU sparkline / per-core grid only
   rendered in the **Procs** view, but neotop opens in **Vms**
   view by default when a `run_dir` exists. Result: charts the
   user paid attention to designing were one keystroke away,
   discoverable only by accident.
2. The new group view labelled containers as `docker:abc12345`.
   That short hash is *technically* sufficient — you can paste
   it into `docker logs <id>` — but the user's mental model is
   `myapp`, not the SHA-256 prefix.

### Added

- **`ContainerNames` cache** in `src/groups.rs` — shells out to
  `docker ps --no-trunc --format '{{.ID}} {{.Names}}'` and the
  equivalent `podman ps`, parses the lines, and stores a
  `HashMap<full-sha, name>`. TTL = 5 s; refreshed lazily on the
  slow tick (every 4 ticks, ~4 s at 1 Hz). Silent no-op when
  neither runtime is installed — neotop doesn't grow a hard
  runtime dependency.
- **`Group::label_with_names(&ContainerNames)`** — preferred
  display label that consults the cache. Container groups
  surface the human-readable name (`docker:myapp` instead of
  `docker:abc12345`); non-container groups fall through to
  `label()` unchanged.
- **`ContainerNames::lookup`** — resolves either a 12-char
  short hash or a full SHA via prefix match. The `Container.id`
  field carries the short form (matched out of the cgroup
  path); the cache stores the full SHA from `--no-trunc`. The
  prefix match bridges the two.
- **Detail pane CONTAINER line** now reads `docker myapp
  (abc12345)` when the name is resolved, giving the user both
  the friendly identifier and the hash they need for `docker
  logs <id>`. Falls back to the bare `runtime:hash` form before
  the first `docker ps` poll completes.

### Changed

- **`draw_vms` layout** — added `host_history` sparklines (3
  rows) and the per-core CPU grid / spectrum to the Vms view
  layout. Both are gated by terminal height so the fleet table
  still gets a usable 5-row body on small terminals: sparklines
  appear when `area.height >= 28`, and the serial + resources
  pane (16 rows) appears when `area.height >= 32`. Below those
  thresholds the smaller terminal still gets a sensible layout
  with just the title, host overview, and fleet table.
- **`compute_visible_grouped` signature** grew a `names:
  &ContainerNames` parameter. Threaded through from `App` so
  group headers always see the latest cache.
- **Slow tick** now also refreshes the container-name cache
  alongside temps / batteries / disks / GPU. One fork+exec per
  installed runtime every ~4 s — measured at <5 ms per tool
  when the daemon is up, and skipped entirely when the binary
  is missing.

### Tests

- 111 passing (was 107). Net +4:
  - `parse_ps_lines_extracts_id_name_pairs` — fixture-driven
    parse of `docker ps` output (myapp, quirky_einstein,
    redis-cache).
  - `parse_ps_lines_skips_blank_and_malformed` — blank lines,
    single-token lines, and whitespace-only lines must not
    crash the parser.
  - `container_names_lookup_resolves_short_id_via_prefix` —
    12-char short hash and full SHA both resolve to the
    correct name.
  - `group_label_with_names_prefers_resolved_name` — container
    groups use the cached name; unresolved containers fall
    back to id; non-container groups ignore the cache.

### Fixed

- `Option::is_none_or` (stable 1.82) replaced with
  `map_or(true, ..)` — neotop's MSRV is 1.80, so the more
  natural form would have failed CI on the older toolchain.

### Out of scope (tracked for v0.14.0+)

- **Go / Rust runtime detection** via ELF section scan
  (`.gopclntab`, etc). Costs per-tick I/O.
- **Themes / TOML config**.
- **Intel via i915 / Xe perf counters** (needs `CAP_PERFMON`).
- **SMT / NUMA grouping** in the spectrum view.
- **macOS / Windows ports**.

## [0.12.0] — 2026-04-25

The "process groups" release. Every process viewer the user has
tried — `htop`, `btm`, `btop` — shows the host's PIDs as a flat
wall of text. On a developer laptop with 30 Node processes, 5
Java services, and a handful of Podman containers running their
own init trees, that wall of text *buries* the signal. This
release classifies every process into a developer-meaningful
group and adds a third list mode that clusters them with
aggregated CPU and RSS.

The taxonomy:

- **Container** (Docker / Podman / Kubernetes / Containerd / LXC)
  — derived from `/proc/<pid>/cgroup` patterns. Container ID is
  surfaced as a 12-char short hash so the user can `docker ps` /
  `podman ps` it back to a human name.
- **Runtime** (Java / Node / Bun / Deno / Python / Ruby / PHP /
  Perl / Lua / Erlang / .NET / R) — derived from `argv[0]`.
- **System** — PID 1, kernel threads, and the canonical systemd /
  dbus / udev daemons.
- **Native** — the catch-all (Go, Rust, C, C++ binaries; we'd
  need ELF symbol parsing to detect those properly and that's
  not worth the per-tick I/O).

Container detection wins over runtime detection: a `node`
process running inside `docker run myapp` is more usefully
grouped with the container than lumped in with all other Node
processes on the host.

### Added

- **`g` toggle** in the Procs view: cycles `Group` ↔ `Flat`. Same
  re-anchor-by-pid behaviour as `t`, so the cursor follows the
  process across the layout change.
- **`compute_visible_grouped`** — buckets surviving rows by
  group key, emits a synthetic header row per cluster, then the
  members indented two spaces. Group bands sort
  Container → Runtime → System → Native; within a band, groups
  with the largest aggregate of the chosen sort key bubble up
  first.
- **Group header row** — banner painted in the COMMAND column
  (`▼ docker:abc12345  (5)`), with the cluster's total CPU% and
  total RSS in the CPU and RSS columns, all coloured by band
  (Cyan = Container, Yellow = Runtime, DarkGray = System /
  Native). Selectable navigation skips headers, so `j` / `k` /
  `K` / `Ctrl-K` only ever land on real PIDs.
- **`src/groups.rs`** — new module with `Lang`,
  `ContainerRuntime`, `Container`, and `Group` types plus
  `classify_lang`, `parse_container_cgroup`, and
  `classify_process` (the layered classifier).
- **`procs::ProcessRow.group: Group`** — derived once when the
  pid is first seen and cached alongside `cmdline` in
  `StaticInfo`. Steady-state cost = 0.
- **Detail pane GROUP / CONTAINER line** — the same group label
  appears in the right-hand detail pane regardless of which list
  mode is active, so the classification is always one keystroke
  (`Tab`-skip away from being visible).
- **Help overlay** lists `g` alongside `s` / `t` / `H` / `/` / `K`.

### Changed

- `App.tree_mode: bool` → `App.list_mode: ListMode` (three-state
  enum: `Flat` | `Tree` | `Group`). `t` and `g` toggle their
  respective mode; pressing the active key returns to `Flat`.
- `procs.rs` reads `/proc/<pid>/cgroup` once per pid first-sight;
  the result is cached in `StaticInfo` and never re-read for the
  pid's lifetime. Roughly +1 file read per *new* process; zero
  steady-state cost.
- `ProcRender` grew a `header: Option<GroupHeader>` field. `None`
  → real process row (today's behaviour); `Some` → synthetic
  group banner. `selected_proc()` and `reanchor_proc_selection`
  filter `header.is_some()` rows out so kill / detail callers
  always see real PIDs.
- The Procs title bar now says `processes · grouped (g to leave)`
  in group mode and keeps its existing `· tree` / `· by CPU%↓`
  variants in the other two modes.

### Tests

- 107 passing (was 90). Net +17:
  - 15 new tests in `groups::tests` covering language detection
    (java / node / bun / deno / python / ruby / php / perl / lua
    / erlang / dotnet / R / nodejs / Rscript / php-fpm),
    null-separated cmdline handling, six container-runtime
    cgroup patterns (modern docker, legacy docker, podman,
    Kubernetes-wraps-docker, LXC, multi-line cgroup-v1), the
    container-wins-over-runtime priority, the band ordering, and
    the label format.
  - `grouped_visible_emits_header_then_members_per_band` — three
    bands rendered in the right order with header rows ahead of
    their members.
  - `grouped_visible_filter_prunes_before_grouping` — the
    substring filter drops entire groups whose members all fail
    to match.

### Out of scope (tracked for v0.13.0+)

- **Container-name resolution** via `docker ps` / `podman ps`
  output or socket. Would surface "myapp" rather than
  "docker:abc12345" in the header. Defers because it requires
  shelling out to runtime-specific tools.
- **Go / Rust runtime detection** via ELF section scan
  (`.gopclntab`, etc). Defers because it costs per-tick file I/O.
- **SMT / NUMA grouping** in the spectrum view (already deferred
  in v0.11.0).
- **Themes / TOML config**.
- **Intel via i915 / Xe perf counters** (needs `CAP_PERFMON`).
- **macOS / Windows ports**.

## [0.11.0] — 2026-04-25

The "spectrum view". v0.10.0's heatmap was a flat colour grid —
btop has the same idea, and the user pointed out it was "too
normal". v0.11.0 replaces the flat grid with a per-core row that
**triple-encodes** load: a height-coded *and* colour-coded
sparkline + the live numeric % + a proportional gauge. Same `H`
toggle. Same 60-second window. Strictly more information per
glance.

The same triple-readout idea now applies to GPUs in the host
overview — busy % and VRAM each get an inline gauge alongside
their numeric. So the rule is now:

> Anything that has a 0..100 % current value and a 60-second
> history gets **sparkline + numeric + gauge**, painted with the
> same green/yellow/red ramp.

### Added

- **Per-core CPU spectrum view** (`H`, replaces the flat
  heatmap). One row per core:
  - Label `c00` plus trailing space (4 chars wide).
  - 60-second sparkline drawn with the `▁▂▃▄▅▆▇█` block ramp.
    Each cell is *also* coloured by load — height + colour so
    a long quiet stretch with a recent spike reads
    differently from "hot all minute" without conscious work.
  - Live numeric % in the same colour.
  - Proportional gauge `▕████░░░░░░░░▏` so a busy core pops
    visually next to quieter ones.
- **Time-axis tick row.** The bottom row of the spectrum panel
  reads `-Ns ────── now` so a new user instantly sees the
  chart's reach (the smallest roadmap item, knocked out
  alongside the visual rework).
- **GPU gauges** in `host_line_gpu`. Every card with live
  metrics now shows two inline gauges — one for busy %, one
  for VRAM occupancy — alongside the existing numeric and the
  GPU sparkline up top. A T1000 at 92 % busy with 95 % VRAM
  used now *looks* alarming, not just reads alarming.
- **`cpu_load_color` (4-stop ramp)** — DarkGray idle (≤19 %),
  Green active-low (20–49 %), Yellow active-mid (50–79 %), Red
  hot (≥80 %). Idle is dark grey rather than green so quiet
  cores recede and the eye is drawn to active cores. Shared
  by sparkline cells, the live %, the spectrum gauge, and
  the GPU VRAM gauge.
- **`gauge_cells(pct, cells, color)`** helper — single source
  of truth for the proportional bar fill across CPU spectrum
  rows and GPU gauges. Out-of-range inputs clamp rather than
  panic.

### Changed

- `App.per_core_heatmap` → `App.per_core_spectrum`. Same key
  (`H`), same default (off), same passive-fill behaviour
  (`host_history.per_core` accumulates from launch so the
  first toggle "on" instantly shows 60 s of history).
- `percore_height()` in spectrum mode now returns
  `min(num_cores + 1, term_h / 3)` with a floor of 4, so the
  axis row always has space and even tiny terminals get
  3 cores + axis rather than collapsing into nubs.
- `heatmap_cell_color(u64)` removed — its solid-bg use case
  is gone. The colour ramp lives on as `cpu_load_color(f64)`.

### Tests

- 90 passing (was 87). Six replaced/added:
  - `cpu_load_color_steps` — verifies the four-stop ramp at
    the breakpoints (0/19/20/49/50/79/80/100).
  - `gauge_cells_round_to_nearest` — 0 / 50 / 100 % all give
    correct cell counts; out-of-range values clamp.
  - `spectrum_row_left_pads_short_ring` — short rings render
    left-padded, not right-justified, so newly-launched
    neotop doesn't look broken for the first minute.
  - `spectrum_axis_row_widths_match_sparkline` — the tick
    label aligns flush with the start of the sparkline at any
    width.
  - `percore_height_spectrum_one_row_per_core_plus_axis_with_room`
  - `percore_height_spectrum_caps_at_third_of_terminal`
  - `percore_height_spectrum_floor_at_four`

### Out of scope (tracked for v0.12.0+)

- **Themes / TOML config** — substantial; deserves its own
  release.
- **Intel via i915 / Xe perf counters** — needs `CAP_PERFMON`
  or root; gate behind a feature flag.
- **macOS / Windows ports** — quarter of work, separate
  arc.
- **SMT / NUMA grouping** — read
  `/sys/devices/system/cpu/cpu*/topology/core_id` and visually
  group SMT siblings. Useful on hybrid-core CPUs (Intel P/E).

## [0.10.0] — 2026-04-25

The per-core CPU heatmap. v0.8.0 shipped two "thousand words"
charts (memory composition, GPU sparkline) and called out a
third on the whiteboard — cores × time. v0.10.0 ships it.

The picture answers questions the existing "now" strip can't:

- *Did this load just appear, or has it been steady for a minute?*
- *Is one core hot, or all of them?*
- *Is the scheduler ping-ponging a single hot job between cores?*

`htop` / `btm` / `btop` all show the live per-core %, but none
show the **time axis**. That's the win.

### Added

- **Cores × time heatmap.** Toggled in the Procs view with
  `H`. Each row = one CPU core, each cell = one 1-second
  sample, painted with the same green/yellow/red ramp the
  "now" strip uses. The buffer fills passively from launch, so
  the first toggle "on" instantly shows the last 60 s of
  per-core activity — no warm-up wait.
- **`HostHistory.per_core: Vec<VecDeque<u64>>`** — one ring per
  core, capped at `CPU_HISTORY_CAP` (60). Topology changes
  (CPU hotplug, vCPU rebalance) reset the rings cleanly rather
  than indexing OOB.
- **Layout-aware sizing** — `percore_height()` now takes the
  terminal height and toggle state. In heatmap mode it returns
  one row per core, capped at `terminal_height / 3` so the
  procs body keeps two-thirds of the screen, with a floor of 3
  rows so a tiny terminal still gets a legible chart.
- **`?` overlay** lists `H` alongside `s` / `t` / `/` / `K`.
- Module-level `Controls:` doc comment in `main.rs` updated.

### Tests

- 87 passing (was 81). Six new tests:
  - `heatmap_cell_color_steps` — verifies the four-stop colour
    ramp matches the breakpoints used by `cpu_glyph_color` so
    eyes read both charts with one mental model.
  - `host_history_per_core_resets_on_topology_change` — proves
    a 4→2 core transition doesn't bleed across topologies.
  - `host_history_per_core_caps_at_history_length` — ring
    eviction works at the same cap as every other history.
  - `percore_height_heatmap_one_row_per_core_with_room` — happy
    path on a tall terminal.
  - `percore_height_heatmap_caps_at_third_of_terminal` —
    ensures the procs body keeps two-thirds.
  - `percore_height_heatmap_floor_at_three` — the chart never
    collapses below 3 rows.

### Out of scope (tracked for v0.11.0+)

- Intel via i915 / Xe perf counters (still needs `CAP_PERFMON`).
- Themes / TOML config.
- macOS / Windows ports.
- Optionally: a time-axis tick-label row at the bottom of the
  heatmap (`-60s … now`). Skipped for now to keep the chart
  compact.

## [0.9.0] — 2026-04-25

NVIDIA support lights up. v0.8.0 detected NVIDIA cards but
displayed `(driver pending)`; v0.9.0 actually reads them via
NVML (NVIDIA Management Library), so a hybrid laptop with a
T1000 dGPU now shows real busy %, VRAM, and (where supported)
power draw, all 1 Hz alongside the rest.

### Added

- **`nvml-wrapper` dependency** (gated behind a default-on `nvml`
  feature). The crate dlopens `libnvidia-ml.so` at runtime, so
  the binary still builds and runs on machines without the
  NVIDIA driver — init failure just leaves NVIDIA cards in
  detect-only mode. `cargo build --no-default-features` produces
  a smaller binary on AMD-only / Intel-only systems.
- **Lazy NVML initialisation** in `gpu::Tracker`. The 50-100 ms
  `Nvml::init()` cost is paid once on the first slow tick that
  finds an NVIDIA card; the handle is then reused for the
  lifetime of the process. AMD-only and Intel-only boxes never
  pay it at all (the merge step early-exits when no NVIDIA
  vendor is in the sysfs scan).
- **PCI bus matching.** sysfs's `device` symlink resolves to a
  4-hex-domain PCI address (`0000:01:00.0`); NVML returns the
  8-hex-domain form (`00000000:01:00.0`). New `normalize_pci_addr`
  helper canonicalises both to the 8-hex form so the lookup
  `HashMap` matches reliably. Tested for short-domain padding,
  long-domain pass-through, case + whitespace tolerance, and
  garbage-input safety (corrupted symlinks return-as-is and
  simply miss the lookup rather than crash).
- **Per-device telemetry** for NVIDIA via NVML:
  - `Device::utilization_rates().gpu` → `busy_pct`
  - `Device::memory_info()` → `vram_used` / `vram_total`
  - `Device::power_usage()` → `power_watts` (milliwatts → W)
  - `Device::name()` → friendly label (`"NVIDIA T1000"` etc.)
- **`Gpu.pci_addr` field** carries the canonical PCI address so
  the merge step doesn't have to re-resolve symlinks. `#[allow(dead_code)]`
  on the field's UI exposure since it's used only internally.

### Changed

- `gpu::Tracker` is now genuinely stateful (holds the lazy NVML
  handle in an enum: `Uninit` / `Failed` / `Ready(Box<Nvml>)`).
  `Box` keeps the variant compact for `clippy::large_enum_variant`.
- `Gpu` instances representing NVIDIA cards no longer say
  `(driver pending)` once NVML resolves them — the line shows
  the real card name and live numbers.

### Tests

- 81 passing (was 77). Four new tests around PCI normalisation:
  `normalize_pci_addr_pads_short_domain`,
  `normalize_pci_addr_passes_through_long_domain`,
  `normalize_pci_addr_lowercases_and_trims`,
  `normalize_pci_addr_handles_garbage_input`.
- New `#[ignore]`'d `gpu_live_snapshot` test prints the live
  tracker output on demand
  (`cargo test --release -- --ignored gpu_live_snapshot --nocapture`),
  for verifying NVML matching on novel hardware. Doesn't run in
  CI because nothing generic to assert.
- Verified live on the development box: T1000 dGPU populated
  with 25% busy, 779/4096 MiB VRAM (matching `nvidia-smi`'s
  view), `power_watts: None` because the T1000 firmware doesn't
  expose `power_usage()` — surfaced as `None` rather than `0 W`
  so we never lie about draw.

### Build

- Both feature combinations (`nvml` and `--no-default-features`)
  are clippy-clean and tested. Release binary 1.07 → 1.18 MiB
  (+11%, ~120 KB of NVML bindings).
- Verifying both feature combinations on every CI run is
  worthwhile; consider adding a `just check-no-default` recipe
  in v0.10.0+ if it's needed regularly.

### Out of scope (tracked for v0.10.0+)

- Intel via i915 / Xe perf counters (still needs `CAP_PERFMON`).
- Per-core CPU heatmap (cores × time grid) — the other "thousand
  words" chart left on the whiteboard since v0.8.0.
- Themes / TOML config.
- macOS / Windows ports.

## [0.8.0]

The "charts worth a thousand words" release. The user asked for
GPU metrics and for charts that surface what `htop`, `btm`, and
`btop` don't. Two big visual additions and one new module.

### Added

- **`gpu` module** with vendor-aware backends.
  - **AMD** is fully wired via sysfs (`/sys/class/drm/card*/device/`):
    `gpu_busy_percent`, `mem_info_vram_used`, `mem_info_vram_total`,
    plus `power1_average` from the card's hwmon subdirectory for
    instantaneous draw in watts. All reads are best-effort; a card
    that disappears mid-scan (eGPU unplug, runtime PM) is silently
    skipped.
  - **NVIDIA** and **Intel** cards are *detected* (vendor probe
    against `/sys/.../device/vendor`) and surfaced in the host
    overview by name with a `(driver pending)` tag, so users on
    hybrid laptops see the hardware was recognised. Real metrics
    for those backends — NVML for NVIDIA, perf counters for
    Intel — are tracked for v0.9.0.
  - Detection only adds a row to the host overview when at least
    one card is present; machines without a discrete GPU pay no
    visual cost.
- **5th sparkline column: GPU%.** Slots into the 60-second history
  strip alongside CPU / MEM / NET↓ / NET↑ when at least one card
  is reporting `busy_pct`. `LightRed` hue so the eye picks it out
  at a glance — "GPU pegged" is usually the headline number on
  machines that have one.
- **Memory composition bar** — full-width horizontal stacked bar
  on the Procs view, showing **used | buffers | cached | free**.
  Each segment is solid-color and proportionally sized to the
  underlying byte count; the title carries the exact figures
  (`memory  4.1G used │ 312M buf │ 6.8G cache │ 4.7G free │ 16G total`).
  This is the chart that `htop` shrinks to one tiny row and
  `btop` doesn't surface at all — most TUIs hide the difference
  between *real* memory pressure and instantly-reclaimable page
  cache. Hidden on terminals shorter than 22 rows so the procs
  body keeps a usable list.
- New `host::HostInfo` fields: `mem_free_bytes`, `mem_buffers_bytes`,
  `mem_cached_bytes`. `MemFree`, `Buffers`, `Cached` are all
  parsed out of `/proc/meminfo` on the same pass as the totals.
- `gpu::aggregate_busy_pct()` averages busy% only across cards
  that *report* it (NVIDIA / Intel cards we don't have backends
  for are excluded rather than zero-filled — zero would lie about
  the workstation's true load).
- `host_overview_rows()` helper keeps layout & paragraph in
  lockstep: 3 by default, 4 once a GPU shows up.

### Tests

- 77 passing (was 66). New tests:
  - `gpu::pci_vendor_id_matches_known_vendors`
  - `gpu::is_real_card_node_filters_connectors_and_render_devices`
  - `gpu::amd_parser_assembles_full_snapshot`
  - `gpu::amd_parser_tolerates_partial_data`
  - `gpu::amd_parser_rejects_out_of_range_busy`
  - `gpu::vram_pct_is_none_when_total_unknown`
  - `gpu::aggregate_busy_pct_averages_only_known_values`
  - `gpu::aggregate_busy_pct_returns_none_when_no_backend_responds`
  - `host_overview_rows_grows_with_gpu_presence`
  - `scale_clamps_to_bar_width`
  - `scale_avoids_overflow_on_terabyte_systems`

### Out of scope (tracked for v0.9.0)

- NVIDIA via `nvml-wrapper` — adds a runtime-loaded native
  dependency and deserves its own focused release.
- Intel via i915/Xe perf counters — needs root or `CAP_PERFMON`,
  same.
- Per-core CPU heatmap (cores × time grid) — the other "thousand
  words" chart left on the whiteboard.

## [0.7.0]

The "refined product" release. The user reported `neotop` "feels
like a chart bitcoin or something" — too fast to read — and asked
for more focus on meaningful metrics. Five changes pointed at
exactly that.

### Changed

- **Default refresh 250 ms → 1000 ms.** 4 Hz updates were too fast
  to track with the eye; values became stock-tickers. 1 Hz is the
  same cadence as `htop`, `btop`, and `iotop`. The user can still
  drop to 100 ms via `+` if they're chasing a specific spike.
  Sparkline window grows from 15 s to 60 s — a much more useful
  trend horizon.
- **Slow-tick cadence, recomputed.** With the new 1 s base tick,
  `SLOW_TICK_EVERY = 4` now means temps / batteries / disks scan
  once every 4 seconds. Previously it was once per second.
- **Host CPU% is now EMA-smoothed for display.** Same `α = 0.5`
  curve used for per-pid CPU%. The line-1 number stops jumping
  between 12% and 47% on consecutive ticks; the underlying
  measurement is unchanged, so sustained activity still tracks
  cleanly.
- **Tree mode (`t`) now respects sort and filter.** Before this
  release, toggling tree view silently disabled both — you couldn't
  grep for a process *and* see its parent chain. The new
  `compute_visible_tree` does a memoised post-order pass to compute
  the "alive" set (nodes that match OR have a matching descendant),
  then sorts siblings within each parent by the chosen `SortBy`.
  Tree shape is preserved.

### Added

- **Swap usage** in the host overview. `SwapTotal` / `SwapFree`
  from `/proc/meminfo`. Only rendered when swap is configured (no
  noise on microVMs / cloud servers without it). Color codes the
  percentage: yellow ≥ 10%, red ≥ 50% — swap is one of the
  strongest "something is wrong" signals there is.
- **5- and 15-minute load averages** alongside the 1-minute one.
  The triplet tells you whether you're looking at a fresh fire
  (1m high, 5m and 15m low) or a sustained one. Showing only the
  1-minute number was hiding half the signal.
- **`procs::ema_blend()`** is now used by `App::tick` too, not
  just `procs::Tracker`. Both code paths share the same smoothing
  curve so the displayed numbers stay coherent.
- **`cmp_rows()`** factored out as a single source of truth for
  the sort comparator across flat and tree modes.

### Tests

- 66 passing (was 63). New tests:
  - `tree_filter_keeps_ancestors_when_a_descendant_matches`
  - `tree_filter_drops_subtree_with_no_match`
  - `tree_sort_orders_siblings_by_cpu_when_requested`
- Existing tree tests updated to pass the new
  `(by, filter)` arguments (`SortBy::Pid` + `""` reproduces the
  old behaviour exactly).
- `parse_loadavg` test split into one for the full triplet and
  one for graceful rejection of partial inputs.

## [0.6.0] — 2026-04-25

The "actually responsive" release. Three findings, three fixes.

### The smoking gun: `acpitz` was costing 3 seconds per tick

A direct measurement on real hardware uncovered the root cause of
the persistent lag complaint:

```text
hwmon0 (acpitz):  3031 ms     ← reading this one file
hwmon1 (nvme):      15 ms
hwmon2 (pch_*):      1 ms
hwmon5 (coretemp):   9 ms
```

On certain HP/Dell laptops the kernel falls through to the ACPI
`_TMP` method when serving `/sys/class/hwmon/hwmonN/tempK_input`,
which polls the embedded controller over a mailbox protocol. A
single read takes hundreds of milliseconds; the whole device adds
up to multiple seconds. We were doing it four times a second.

### Fixes

- **`temp::Tracker` with adaptive blacklisting.** Every hwmon
  device's full scan is timed; anything exceeding `SLOW_THRESHOLD`
  (50 ms) is added to a `parked` set and skipped on every
  subsequent tick. The first tick still pays the cost so the user
  sees a number; from tick 2 onward the device is invisible to
  the scanner. Measured impact: **`temp::snapshot()` 3,042,534 µs
  → 8,822 µs (345× speedup)**. There's no flag, no config — slow
  sensors just disappear.
- **EMA-smoothed `cpu_pct` in `procs::Tracker`.** The user
  reported that "process IDs feel like they jump up and down with
  no clue". Cause: instantaneous CPU% is computed from a single
  250 ms delta, so a process that briefly busy-waits for one
  sample shows 50%, 0%, 50%, 0% across consecutive ticks and
  jumps from the top of the list to the bottom each time.
  Sorting and display now use an exponentially-weighted moving
  average (α = 0.5): a single 50% spike registers as 25%, then
  decays to ~6% by the third tick. Rows settle to their natural
  position in 3-5 ticks instead of yo-yoing every tick.
- **Slow-path scanners run once per second.** `temps`,
  `batteries`, and `disks` now refresh on every fourth tick
  (`SLOW_TICK_EVERY = 4`). hwmon and battery firmware updates at
  ~1 Hz on real hardware; reading them four times a second was
  pure waste. The fast-path scanners (host stats, net rates,
  procs) stay at full tick rate so the visible numbers don't
  feel laggy.
- **Pause toggle (`space`).** Freezes every snapshot in place
  while keeping the UI fully interactive — you can scroll, sort,
  filter, kill, switch views. Useful when CPU% sort is reshuffling
  rows faster than you can read them. A bright `[PAUSED — space
  to resume]` badge lights up the title bar so you can't forget
  it's on.

### Changed

- `temp::snapshot()` (free function) replaced by `temp::Tracker`
  with an instance method. `App` now holds a long-lived
  `temp_tracker` so the parked set persists across ticks.
- `App` gained `slow_tick_counter: u32` and `paused: bool`.
- `procs::Sample` gained `smoothed_cpu: f64`. `cpu_pct` returned
  by `Tracker::snapshot` is now the EMA, not the instantaneous
  value.
- New `procs::ema_blend()` pure helper for tests.

### Tests

- 63 passing (was 59). New tests:
  - `procs::ema_blend_at_alpha_half_is_arithmetic_mean`
  - `procs::ema_blend_decays_a_lone_spike_in_a_handful_of_ticks`
  - `procs::ema_blend_converges_toward_steady_state`
  - `temp::tracker_skips_already_parked_paths`

## [0.5.0] — 2026-04-25

The "stop feeling laggy" release. `neotop` now feels like a monitor
you can actually watch. Three user-visible complaints addressed:

1. **Laggy / "cannot monitor anything".** The per-tick procs scan
   used to read three files per pid (`stat`, `status`, `cmdline`).
   Steady-state measured on a laptop with 404 pids: **~25 ms → 8.7 ms**.
2. **"Board seems overwhelmed."** Host overview shrank from 4 lines
   to 3: the static kernel + CPU-model line that didn't earn a row
   moved to the `?` overlay, and the redundant inline per-core glyph
   strip on line 1 was removed (the Procs view has a dedicated
   per-core panel anyway).
3. **Confusing sensor names like `pch_cannonlake#1`.** Every hwmon
   label is now mapped to a short human tag (`cpu pkg`, `cpu`,
   `gpu`, `nvme`, `wifi`, `acpi`, `pch`, `bat`, `zone`, `sensor`).
   Cool chipset / ACPI / wifi readings are filtered out of the
   one-line overview entirely — only shown when warm or hot.

### Changed

- **`procs::Tracker` caches per-pid static info.** `uid`, resolved
  `user`, and `cmdline` never change after exec; we read them once
  per pid and reuse them for every subsequent scan. Purged when the
  pid exits.
- **Dropped `/proc/<pid>/status` reads entirely.** RSS now comes
  from `/proc/<pid>/stat` field 24 (pages) × `rustix::param::page_size()`.
  Owning uid comes from `stat(2)` on the `/proc/<pid>` dir inode.
  This cuts per-tick file I/O from `~3N` to `~N` reads.
- **Host overview: 4 lines → 3.** Line 1 now also carries battery
  info (folded in from old line 2). The kernel + CPU model block
  is available via the `?` overlay under "System".
- **Removed inline per-core glyph strip from host line 1.** The
  Procs view already has a dedicated per-core panel; in the Vms
  view the right-hand resources pane carries per-VM CPU detail.

### Added

- **`compact_temp_label()`** maps raw hwmon names to short tags and
  strips the `#N` sensor-index suffix.
- **`is_informative_temp()`** filter hides cool PCH / ACPI / wifi
  / unknown readings so only the sensors the user cares about
  (CPU, GPU, NVMe, battery) appear by default.
- **"System" block in the `?` overlay** showing kernel version and
  CPU model.

### Tests

- 59 passing. New tests:
  - `compact_temp_label_maps_common_sensors`
  - `informative_temp_filter_keeps_cpu_always_and_pch_only_when_hot`
- Removed the status-parser tests (`parses_uid_from_status`,
  `parses_vmrss_kb_to_bytes`) along with the helpers they covered —
  we don't read `/proc/<pid>/status` anymore.

## [0.4.0] — 2026-04-25

The "ergonomics + visibility" release. Four user-visible
improvements: scrolling is smooth, every long table has a
scrollbar, the Procs view grew a dedicated per-core CPU panel,
and the host sparklines now include net-down + net-up alongside
CPU% and MEM%.

### Added

- **Scrollbar on Vms and Procs tables.** A vertical scrollbar
  paints on the right border of each scrollable table, with the
  thumb position tracking the selected row vs total. The bar
  hides automatically when the row count fits in the visible
  area, so small fleets / short process lists don't get a
  decorative track.
- **Per-core CPU grid.** Procs view picks up a dedicated row
  (or two, if there are more cores than fit in one) showing
  every logical core with `c{n} {bar} {pct}%`, color-coded
  green/yellow/red. Auto-flows based on terminal width, capped
  at two rows so the procs body never gets squeezed.
- **NET↓ / NET↑ sparklines.** The host history strip went from
  two columns (CPU, MEM) to four (CPU, MEM, NET↓, NET↑). The
  net charts auto-scale to the rolling max in their 15 s window
  (floored at 1 KB/s so an idle window doesn't draw a wall of
  full bars), and each title carries the live human-readable
  rate, e.g. `NET↓ 1.2 MB/s`.
- **Sparkline title shows the live value.** CPU and MEM titles
  now include the current sample (e.g. `CPU 42%`), matching the
  net titles. No need to glance back at the host overview line
  to read the number off the chart.

### Changed

- **Smoother input.** The run loop now drains *all* queued key
  events before redrawing, so holding `j` collapses ten queued
  presses into a single render at the right final position.
  Previously, each key triggered its own redraw and on slower
  terminals (~10 ms render time) a 33 ms key-repeat felt
  visibly chunky.
- **`KeyEventKind::Release` and `Repeat` are filtered out.** On
  kitty / Windows-style terminals where every keystroke fires
  Press *and* Release events, neotop used to apply each binding
  twice (e.g. Tab → flip → flip-back → no-op). Now only Press
  events are honored.

### Tests

- 59 passing (was 54). New tests:
  - `total_net_rates_sums_with_none_as_zero`
  - `percore_height_zero_when_no_cores`
  - `percore_height_fits_in_one_row_when_wide_enough`
  - `percore_height_caps_at_two_rows`
  - `percore_height_handles_narrow_terminal`

## [0.3.0] — 2026-04-25

The "refined daily-driver" release. The Procs view picks up six
quality-of-life improvements that turn it from "works" into the
htop replacement neotop set out to be.

### Added

- **PID-locked cursor.** Before each refresh — and on every sort /
  filter / tree mutation — neotop captures the PID under the cursor
  and re-anchors the row index to wherever that PID ends up after
  the recompute. Sorting by CPU% no longer slides the selection
  from process to process as load shifts.
- **Keybindings overlay (`?`).** A centered popup lists every
  binding split into Global / Vms / Procs sections. Esc / ? / q /
  Enter dismiss. Status bar grew a `?  help` chip so the binding
  is discoverable.
- **Tunable refresh tick (`+` / `-`).** App now owns the refresh
  `Duration`, initialised from `--refresh-ms`. `+` (or `=`) halves
  the tick down to 50 ms; `-` (or `_`) doubles up to 5 s. The perf
  footer's tick metric grew a `actual/configured` form (e.g.
  `252/250ms`) so drift is visible.
- **Process detail pane.** When the terminal is ≥ 110 cols wide,
  the Procs body splits 60/48 with a live detail pane on the right
  showing PID, PPID, user, state, CPU%, threads, RSS, VSZ, the
  cgroup-v2 path + memory.current/max, the curated rlimits, and
  the wrapped full command line.
- **Tree view (`t`).** Toggle Procs between flat-list mode and a
  parent → children tree. Tree rendering uses the standard glyph
  set (`├─`, `└─`, `│`). Roots are pids whose ppid is 0 or whose
  ppid isn't in the row set (covers exec races + kernel threads).
  Sort and filter are disabled in tree mode for now — a future
  iteration may layer them back on.
- **Sort-direction indicator.** Procs title shows `sort CPU%↓` /
  `sort PID↑` so the user doesn't have to guess which way numbers
  flow.

### Changed

- `App::refresh` renamed to `App::tick` so the field/method names
  don't collide.
- `procs_visible: Vec<usize>` → `Vec<ProcRender>` (idx + prefix)
  so the tree-glyph chain travels with each rendered row.

### Tests

- 54 passing (was 51). Adds three tests in `main.rs`:
  - `tree_orders_parents_then_children_in_pid_order` — caught a
    real bug during dev where the root's `is_last_sibling` was
    leaking into its children's prefix; fixed by adding a depth
    parameter to `dfs_tree`.
  - `tree_handles_orphans_as_roots`
  - `flat_visible_respects_filter_and_sort`

## [0.2.0] — 2026-04-25

The "neotop is now usable as a general system monitor" release. The
v0.1.0 first impression — empty table when no VMs were running — is
replaced with sensible defaults and three new host-level signals.

### Added

- **Smart default view.** When `$NEOSANDBOX_STATE/run` doesn't exist
  at startup, neotop now opens in the Procs view (host process
  table, sorted by CPU%) instead of the empty Vms view. When the
  state-dir exists but is empty, the Vms view renders a friendly
  hint paragraph pointing at `Tab` and `just demo-pvh` instead of
  showing an empty table.
- **Disk I/O.** New `src/disk.rs` parses `/proc/diskstats` and
  shows the top three physical devices' read/write/util on a new
  4th host-overview line. Partitions, loop, ram, dm-, md, and
  zram are filtered out — same heuristic as `iostat`/`btm`. Util%
  uses the same yellow/red thresholds as CPU and temp.
- **Host history sparklines.** `HostHistory` keeps the last 60
  CPU% / mem% samples (15 s at the default tick). The Procs view
  has a new 3-row band rendering two side-by-side `Sparkline`s
  for host CPU and host memory. Vms view is unchanged.

### Changed

- The host overview block grew from 3 to 4 rows in both views to
  accommodate the disk line.
- `mem_used_pct` extracted from `host_line1` and reused by the
  history sampler so the sparkline tracks exactly what line 1
  shows.

### Tests

- 51 passing (was 43). 7 new disk-module tests cover
  `parse_diskstats`, `is_physical_disk` on every device-class
  shape, `snapshot_from_str` rate computation, `highlights`
  ordering, and `human_rate` formatting.

## [0.1.0]

The first daily-driver release. neotop now passes the bar laid out in
`PLAN.md`: a responsive quit, host signals at a glance, a process view
that can kill runaway processes, honest error surfacing, and CI that
keeps the parsers test-locked.

### Added

- **Procs view.** `Tab` toggles between the VM fleet table and an
  htop-style process table for every PID on the host. Columns:
  `PID USER S CPU% RSS THR COMMAND`. Sortable on `s` (CPU → MEM → PID
  → CMD), filterable on `/` (case-insensitive substring), with state
  letters and CPU% colored by load.
- **Process kill.** `K` queues a SIGTERM and `Ctrl-K` queues a SIGKILL
  on the selected pid. Both prompt for `y/N` on the help bar before
  the signal is delivered. Uses `rustix::process::kill_process`; no
  `unsafe`, no `libc`.
- **Self-profiling footer.** Right-aligned bottom row shows `scan
  Xms · render Yms · own ZMiB W% · tick Tms`. `scan_ms` and `render_ms`
  go yellow above 20 ms and red above 100 ms; the user can see whether
  neotop itself is the bottleneck.
- **Error ring.** Bounded VecDeque (cap 16) collects non-fatal parse
  / I/O failures from `host`, `net`, and `hwmon`. The latest entry is
  rendered as a red badge between help and perf for 5 s after each
  push, including a lifetime "(N err)" counter.
- **Test seams** for every parser. `host::parse_*`, `net::parse_proc_net_dev`,
  `net::Tracker::snapshot_from_str`, `battery::parse_capacity`,
  `battery::parse_power_now_watts`, `procs::PasswdCache::parse`. The
  test suite went from 9 to 43 unit tests across 6 modules — every
  parser now runs against a canned fixture, not just the live kernel.
- **CI** (`.github/workflows/ci.yml`): fmt-check, clippy-pedantic,
  tests, and release-build, on a `{ stable, 1.80 }` matrix with
  cargo cache. README has the badge.
- **Help / docs.** README controls table covers Tab / s / / / K /
  Ctrl-K and the Procs / footer sections; `--help` mirrors the same
  text.

### Changed

- `host::snapshot`, `host::read_cpu_samples`, `temp::snapshot`, and
  `net::Tracker::snapshot` now take `&mut errors::ErrorRing` so failures
  reach the UI instead of returning empty defaults silently.
- `procs::sort_rows` is no longer used by the live UI — replaced by
  `main::compute_visible`, which sorts an indirection vector of
  indices to keep the row payloads stable. `sort_rows` stays around
  (and tested) so the sort behaviour is regression-locked.
- Per-pid /proc reads from the `procs::Tracker` walk *do not* push to
  the error ring. Pids race with exec/exit; reporting them would
  flood the footer with false positives. (See `PLAN.md` §3 design
  note.)

### Fixed

- The help-bar `k`-vs-`Ctrl+k` ordering: in earlier drafts of the
  procs view, the bare `j/k` nav arm shadowed the Ctrl+k SIGKILL
  arm, so the latter was silently unreachable. The Ctrl-modified arm
  is now matched first.

### Acknowledgements

The five-task plan in `PLAN.md` is the basis for this release.

[Unreleased]: https://github.com/nt2311-vn/neotop/compare/v0.13.0...HEAD
[0.13.0]: https://github.com/nt2311-vn/neotop/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/nt2311-vn/neotop/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/nt2311-vn/neotop/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/nt2311-vn/neotop/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/nt2311-vn/neotop/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/nt2311-vn/neotop/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/nt2311-vn/neotop/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/nt2311-vn/neotop/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/nt2311-vn/neotop/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/nt2311-vn/neotop/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/nt2311-vn/neotop/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/nt2311-vn/neotop/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/nt2311-vn/neotop/releases/tag/v0.1.0
