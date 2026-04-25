# Changelog

All notable changes to **neotop** are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

## [0.8.0] — 2026-04-25

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

## [0.7.0] — 2026-04-25

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

## [0.1.0] — 2026-04-25

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

[Unreleased]: https://github.com/nt2311/neotop/compare/v0.13.0...HEAD
[0.13.0]: https://github.com/nt2311/neotop/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/nt2311/neotop/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/nt2311/neotop/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/nt2311/neotop/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/nt2311/neotop/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/nt2311/neotop/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/nt2311/neotop/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/nt2311/neotop/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/nt2311/neotop/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/nt2311/neotop/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/nt2311/neotop/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/nt2311/neotop/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/nt2311/neotop/releases/tag/v0.1.0
