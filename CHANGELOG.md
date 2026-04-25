# Changelog

All notable changes to **neotop** are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/nt2311/neotop/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/nt2311/neotop/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/nt2311/neotop/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/nt2311/neotop/releases/tag/v0.1.0
