# Changelog

All notable changes to **neotop** are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/nt2311/neotop/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/nt2311/neotop/releases/tag/v0.1.0
