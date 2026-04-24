# neotop — plan to v0.1.0 (daily-driver quality)

Working document. Each section lists a concrete task, not a dream. The
target is a tool that passes the "would I uninstall btm and use this
instead?" bar on a Linux laptop running neosandbox.

## How to resume

```sh
cd ~/Projects/Rust/neotop
cat PLAN.md                 # this file
cargo test                  # baseline: 9 tests pass
cargo clippy --all-targets -- -D warnings  # baseline: clean
git log --oneline           # see where the last session stopped
```

Then pick the first **⬜ pending** item below and drive it to done.

---

## Current state (frozen at end of 2026-04-24 session)

**Shipped & usable:**

| Module | Status | What it does |
| --- | --- | --- |
| `src/main.rs` | ✅ | Event loop, VM table, host overview, serial + resources pane, CPU sparkline |
| `src/proc.rs` | ✅ | `/proc/<pid>/` parsing for the selected VM (cgroup, rlimits, CPU jiffies) |
| `src/host.rs` | ✅ | Aggregate + per-core CPU, mem, load, kernel, kvm presence |
| `src/net.rs` | ✅ | Per-iface RX/TX byte rates from `/proc/net/dev` |
| `src/temp.rs` | ✅ | `/sys/class/hwmon` readings + severity coloring |
| `src/battery.rs` | ✅ | `/sys/class/power_supply/BAT*/{capacity,status,power_now}` |
| `src/procs.rs` | 🟡 scaffold | Parsing + sampling + sort/filter + 9 unit tests. **Not yet rendered.** `#![allow(dead_code)]` will be removed when wired. |

**Platform:** Linux only. `main.rs` has a `#[cfg(not(target_os = "linux"))]` stub that prints an error and exits 2. macOS is a next-next-session concern.

**Dependencies:** `ratatui 0.29`, `crossterm 0.28`, `serde 1`, `serde_json 1`, `anyhow 1`, `rustix 0.38`. No `unsafe`. No `libc` directly. ~30 transitive crates, ~12s cold build.

**Tests:** 9 unit tests in `procs.rs`. Nothing else is tested yet — that's task §5 below.

---

## The v0.1.0 scope (next session(s))

Five tasks. Roughly ordered by ratio of value to effort. Each should be its own git commit.

### 1. ⬜ Wire `procs` into the UI (process table view)

**Goal:** pressing `Tab` switches the table between "VMs" (current) and "Procs" (all PIDs). Rendering parity with `htop`'s default view.

**File touches:**
- `src/main.rs` — add `enum View { Vms, Procs }`, carry in `run()` state, cycle on Tab; add a `draw_procs()` sibling to `draw_table()`.
- `src/procs.rs` — remove `#![allow(dead_code)]` once used.

**Design notes:**
- Reuse the existing `TableState` for the selected row; one state per view is fine.
- Columns: `PID | USER | STATE | CPU% | RSS | THREADS | COMMAND` — widths `[7, 10, 3, 6, 9, 4, Min(30)]`.
- Default sort: `SortBy::Cpu`. Cycle with `s`. Show current sort key in the table block title: `" processes · by CPU% "`.
- Default filter: empty. Enter "filter mode" with `/`, capture keypresses into a `String`, redraw on each keystroke. Escape clears it. Reuse `procs::matches()`.
- Kill action: `K` sends SIGTERM via `rustix::process::kill_process(Pid::from_raw(…), Signal::Term)`. Confirm with a `y/n` prompt rendered over the help bar. `Shift+K` for SIGKILL, also confirmed.
- Keep the `procs::Tracker` across scans; re-use the same refresh tick driven by `Args::refresh`.

**Acceptance test (manual):** launch `neotop`, press `Tab`, see hundreds of rows; press `s` twice, see sort change from CPU → MEM → PID; `/firefox`, see only matching rows; select one, press `K`, confirm `n`, verify nothing dies.

**Rough size:** ~220 lines added to `main.rs`, ~0 to `procs.rs`.

### 2. ⬜ Self-profiling footer ("performance governance")

**Goal:** neotop measures its own overhead and displays it. This is both an honest signal to the user and a safety net when we add expensive widgets.

**Metrics to track:**
- `scan_ms` — wall-clock time of the full `scan()` call (state.json reads + /proc walks).
- `render_ms` — wall-clock time of `terminal.draw(…)`.
- `own_rss_kb` — our own `/proc/self/status:VmRSS:` (re-read once per 4 scans; cheap).
- `own_cpu_pct` — our own jiffies, same delta math as VM tracker.
- `refresh_actual_ms` — time between the *start* of two consecutive scans, which should equal `args.refresh` unless we're slow.

**Where to render:**
- Status bar at the very bottom, right-aligned:
  `scan 2.1ms · render 0.4ms · own 8MiB · 0.2% · 250ms tick`
- Color `scan_ms` yellow if >20 ms, red if >100 ms. Same for `render_ms`.

**Design notes:**
- One `Perf` struct in `main.rs`, updated inline in the run loop. Resist making it a module until it's needed elsewhere.
- `own_cpu_pct` uses the same `CpuSample { when, jiffies }` shape as per-VM tracking; reuse the code if possible.

**Rough size:** ~60 lines in `main.rs`, plus 2 lines in `proc.rs` to expose a `self_jiffies()` helper.

### 3. ⬜ Error surface (stop swallowing `/proc` errors)

**Goal:** when a file we expected to read disappears or parses badly, show it in the UI instead of silently returning `None` / `Vec::new()`. At the moment a half-broken `/sys/class/hwmon` returns an empty temps vec with no explanation.

**Design:**
```rust
struct ErrorRing {
    entries: VecDeque<Entry>,  // cap 16
    count: u64,
}
struct Entry { when: Instant, source: &'static str, message: String }
```
- Every module-level parser that currently returns a `Default`/empty on error should instead take an `&mut ErrorRing` and push an entry when non-fatal data is missing. *Exception:* per-pid reads during `/proc` walk are high-volume and must stay silent (pids race; this is normal).
- Render the latest entry's source+message compactly on the bottom status bar, left of the perf metrics:
  `⚠ hwmon: can't parse temp3_input (3 err)`
- Cleared when the ring is empty or after 5 s with no new entries.

**Which paths should report errors:**
- `host::read_cpu_samples` returns `CpuSamples::default()` — change to `Result<CpuSamples>`.
- `temp::snapshot` returns `Vec::new()` on `read_dir` failure — report it.
- `net::Tracker::snapshot` — same pattern.
- `battery::snapshot` — the no-battery case is NOT an error; keep silent.

**Rough size:** ~100 lines, mostly plumbing.

### 4. ⬜ Unit tests across all parsers

**Goal:** `cargo test` covers every module's parsing path with representative fixture strings. ~25 tests total.

**Per module:**
- `host.rs`: `read_cpu_samples` given a canned `/proc/stat` string (put real content as a `const` in the test); `read_meminfo_kb` with a canned meminfo; `read_loadavg` with a canned string; `read_cpu_count` — can't really test without mocking fs but at least exercise the string parsing by refactoring to take `&str`.
- `net.rs`: feed a fake `/proc/net/dev` to `Tracker::snapshot` via a `snapshot_from_str` helper (refactor: extract the parsing from the fs read). Assert two calls compute the correct rate given a known time delta.
- `temp.rs`: test `group_of`, `highlights()` sort order, `severity()` thresholds.
- `battery.rs`: refactor `snapshot` to split parsing from fs reads (same pattern); test with canned strings.
- `proc.rs`: test `human_bytes()`, `format_limit_value()`.
- `procs.rs`: **already has 9 tests.** Add one for `matches()` edge cases (unicode), one for `PasswdCache::load()` against a constructed file in a tempdir.

**Refactor pattern:** for functions that do `fs::read_to_string(path)` + parse, extract a pure `parse_X(&str) -> Result<X>` and have the public fn be `fs::read_to_string().map(parse_X)`. Makes tests trivial.

**Rough size:** ~300 lines, mostly fixtures.

### 5. ⬜ CI via GitHub Actions

**Goal:** `.github/workflows/ci.yml` that runs on every push + PR, covering rustfmt, clippy pedantic, tests, and a release build. Matrix: stable + MSRV (1.80).

**Skeleton:**
```yaml
name: ci
on: [push, pull_request]
jobs:
  check:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        rust: [stable, "1.80"]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@v1
        with: { toolchain: "${{ matrix.rust }}", components: "rustfmt, clippy" }
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --all-targets -- -D warnings
      - run: cargo test --all-targets
      - run: cargo build --release
```
- Add a README badge: `![ci](https://github.com/nt2311/neotop/actions/workflows/ci.yml/badge.svg)`.

**Rough size:** 40 lines of yaml + README update.

### 6. ⬜ CHANGELOG.md + v0.1.0 tag

After the five above land:

```sh
# populate CHANGELOG.md with a "0.1.0" section listing the real work
git add CHANGELOG.md
git commit -m "chore: prep 0.1.0 release notes"
git tag -a v0.1.0 -m "neotop 0.1.0 — first daily-driver release"
```

Keep Keep-a-Changelog format. Sections: Added / Fixed / Changed.

---

## Deferred (explicit non-goals for v0.1.0)

These are real things we considered and consciously skipped. Each gets
its own milestone later:

- **Process tree view** — `htop`'s `t` mode. Structurally different code (recursive render, collapsed state per pid). ~400 lines.
- **Memory history chart** — like the CPU sparkline but for RAM. ~60 lines, low priority because RSS rarely changes fast enough to need a chart.
- **Per-device disk I/O** — `/proc/diskstats` parsing + a network-style table. ~150 lines.
- **GPU** — needs vendor detection (AMD sysfs / NVIDIA NVML / Intel perf counters). Start with AMD since it's pure sysfs. ~100 lines for AMD.
- **Themes / color palettes** — TOML config, palette resolver, live reload. Real work, low value for a single-user tool.
- **macOS support** — `#[cfg(target_os = "macos")]` parallel modules using mach APIs. Another big project.
- **Windows support** — don't bother unless someone specifically asks.
- **eBPF overlay** (off-CPU flame, syscall latency) — exciting, but needs `libbpf-rs` or similar and CAP_BPF. Post-1.0.

---

## Known design decisions to lock in before starting

Write the answer next to each one before you code. These are the
questions I stopped on this session:

1. **Tab cycling:** should `Tab` cycle `Vms → Procs → Vms` or `Vms → Procs → Host → Vms`? The Host view would be a dedicated screen with all the widgets the current overview has, larger. Decision: _______
2. **Kill confirmation UX:** inline prompt (a single line overlayed) or modal (Centered popup with Y/N)? htop does a modal, btop inlines. Decision: _______
3. **When to flush `ErrorRing`:** after 5s of no new entries, or never (let the user scroll through all)? Decision: _______
4. **Clock source for perf metrics:** `Instant` (monotonic) is obvious for deltas. But for comparing against `args.refresh`, we want wall-clock stable. Decision: _______ (probably `Instant` throughout; wall-clock only for `updated_at_ns` in state.json).
5. **Should `procs::Tracker` walk `/proc/<pid>/task/*` for per-thread stats?** Current answer is **no, too expensive** — one file read per thread × 1000 threads × 4 Hz = 16k reads/s. Reconsider only if `htop`-style thread view becomes a requirement.

---

## File map (for quick navigation)

```
~/Projects/Rust/neotop/
├── Cargo.toml           # 0.1.0 manifest, lints
├── LICENSE              # Apache-2.0
├── PLAN.md              # ← you are here
├── README.md            # user-facing
├── .gitignore
└── src/
    ├── main.rs          # event loop, views, rendering
    ├── host.rs          # /proc/stat, /proc/meminfo, /proc/loadavg
    ├── net.rs           # /proc/net/dev (rates)
    ├── temp.rs          # /sys/class/hwmon
    ├── battery.rs       # /sys/class/power_supply
    ├── proc.rs          # /proc/<pid>/* for a single PID (VM case)
    └── procs.rs         # /proc/* walk (host process case) — WIP
```

---

## Ambition check

What a daily driver looks like in concrete terms:

- [ ] I press `q` and get a responsive quit, even mid-scan
- [ ] I can see my laptop's CPU, RAM, temps, battery, network at a glance
- [ ] I can switch to a process view and kill a runaway process without leaving the TUI
- [ ] If it goes wrong, I see *why* — not a blank table
- [ ] It uses <1% of one core at idle
- [ ] CI passes, tests pass, and I trust the parsers because they have fixtures
- [ ] It runs on my neosandbox host and shows VMs when I boot one with `just demo-linux`

The five-task v0.1.0 above checks every box. Everything else is polish.
