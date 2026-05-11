# Roadmap

Live list of open items. Short-term work goes in this vault; larger
epics open as GitHub issues too.

## Near-term (next few patches)

- [ ] **Apple Silicon GPU busy %** — the IOKit `PerformanceStatistics`
  dict doesn't reliably expose `Device Utilization %` on M-series.
  Switch to the private IOReport SPI (subscribe to `GPU` channel,
  read energy / utilization counters). Matches what `powermetrics` and
  `macmon` do. [[platforms-macos#Temperatures (temp_macos.rs)]] for the
  same SPI for temps.
- [ ] **macOS temperature — Intel SMC**. Implement the full `AppleSMC`
  user-client protocol: `SMC_CMD_READ_KEYINFO` → `SMC_CMD_READ_BYTES`
  for `TC0P`, `TG0P`, `Ts0P`, `TA0P`, `TB0T`. Decode per-key formats
  (`fpe2`, `sp78`).
- [ ] **macOS temperature — Apple Silicon IOReport**. Subscribe to
  `Energy Model` + `CPU Stats` channels; read `processorTemperature`
  samples.
- [ ] **macOS battery** — `IOPSCopyPowerSourcesInfo()` / `IOPowerSources`.
  Feed existing `Battery` struct; wire into `draw_host`.
- [ ] **Container false-positives on macOS** — tighten
  `container_macos::is_runtime_ancestor` so Docker Desktop UI helpers
  (`Docker Desktop Helper (Renderer)` etc.) don't get tagged as
  containers with synthesised IDs.

## Feature epics

- [ ] **Real Docker telemetry on macOS.** Connect to
  `~/.docker/run/docker.sock`, `GET /containers/json` for names + image
  metadata, stream `/containers/{id}/stats` for per-container CPU / RSS
  / net / block I/O. Render as synthetic rows (no PID / exe on the
  host) under the Container band. Opt-in via `--docker` flag (or
  socket-presence auto-detect).
- [ ] **Windows port.** ETW for per-process stats, PDH for counters.
  Separate `src/*_windows.rs` family mirroring the macOS pattern. Big
  piece of work — tracked as its own milestone.
- [ ] **Intel GPU per-engine power draw.** Blocked upstream: i915 PMU
  exposes per-engine busy %, not per-engine energy. Package-level power
  via RAPL is already shipped. Revisit when kernel exposes a per-engine
  energy counter.

## Quality-of-life

- [ ] **Config reloads on SIGUSR1.** Today the config only loads at
  startup and when cycling themes with `T`. Add a signal handler so
  iterating on the TOML doesn't need a restart.
- [ ] **Per-user recent-apps drawer.** The orbit chart picks top-12 by
  CPU now. Add a mode that remembers the last N "heavy" PIDs across
  the session so they stay visually anchored.
- [ ] **Mouse click selection.** Ratatui supports mouse events; wire
  them to select the row under the cursor.

## Cross-cutting

- [ ] **Integration test harness.** Currently tests are unit-level.
  Add a `tests/` dir that spins up fake `/proc` trees (via `tempfile`)
  and checks the Linux code paths end-to-end.
- [ ] **Benchmarks.** Put a `criterion` harness around
  `procs::Tracker::snapshot` and the classifier pipeline so we can
  track regressions.

## Shipped (moved out once tagged)

See [`../CHANGELOG.md`](../CHANGELOG.md) for the full history.
Recent highlights:

- [x] Universal grouping — `App` band + named `Native` (`v0.28`)
- [x] macOS memory bar + Rust/Go group detection (`v0.27.2`)
- [x] macOS real CPU / mem sampling + full argv (`v0.27.1`)
- [x] macOS feature parity — topology, containers, disk, net, temp (`v0.27.0`)

## See also

- [[status]] — the parity matrix this roadmap is gradually closing
- [[contributing]] — how to pick something up and ship it
