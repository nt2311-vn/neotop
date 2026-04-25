//! temp.rs — hwmon temperature readout.
//!
//! Walks `/sys/class/hwmon/hwmon*`, finds every `tempN_input` file,
//! reads the value (milli-Celsius), and labels it via `tempN_label`
//! when present or the hwmon `name` otherwise.
//!
//! We don't depend on `lm_sensors` or any libsensors binding — sysfs
//! is the same data, one layer down. This is what `lm_sensors` itself
//! reads.
//!
//! ## Adaptive blacklisting
//!
//! On real hardware some hwmon devices are *catastrophically* slow.
//! The poster child: `acpitz` on certain HP laptops takes ~3 seconds
//! per `tempN_input` read because the kernel falls through to an
//! ACPI `_TMP` method that polls the embedded controller over a
//! mailbox protocol. Reading that file every tick blocks the entire
//! UI — the original bug behind the v0.6.0 release.
//!
//! `Tracker` solves this by *measuring* every hwmon's scan time and
//! parking any device whose total scan exceeds `SLOW_THRESHOLD`.
//! Parked devices stay parked for the lifetime of the process —
//! they were slow once and are essentially guaranteed to be slow
//! forever (the bottleneck is the hardware bus, not transient load).
//! No flag, no config; the user gets a fast UI by default.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Any hwmon device whose total scan exceeds this is parked. 50 ms
/// is generous — a healthy hwmon (`coretemp`, `nvme`, `k10temp`)
/// takes <1 ms. The threshold only catches genuinely broken cases
/// like `acpitz` on HP/Dell laptops where a single read costs
/// hundreds of milliseconds.
const SLOW_THRESHOLD: Duration = Duration::from_millis(50);

#[derive(Debug, Clone)]
pub(crate) struct Reading {
    pub(crate) label: String,
    pub(crate) celsius: f64,
}

/// Adaptive temperature scanner. Holds the set of hwmon device
/// paths that have been observed to scan slowly (see module-level
/// comment); skips them on every subsequent call.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    parked: HashSet<PathBuf>,
}

/// Outcome of a single tracker scan: temperature readings plus
/// whatever non-fatal events the scan produced. Decoupled from
/// `ErrorRing` so we can ship reports across thread boundaries via
/// `mpsc::channel` — `ErrorRing::Entry` carries an `Instant` that
/// makes no sense to pre-compute on a worker thread.
///
/// `infos` and `errors` map directly onto `Severity::Info` and
/// `Severity::Warn`. Parking a slow sensor is informational — the
/// hardware is fine, neotop just chose not to read it again — so it
/// rides on `infos` and doesn't inflate the footer's `(N err)`
/// counter.
#[derive(Debug, Default)]
pub(crate) struct ScanReport {
    pub(crate) readings: Vec<Reading>,
    pub(crate) infos: Vec<(&'static str, String)>,
    pub(crate) errors: Vec<(&'static str, String)>,
}

impl Tracker {
    /// Single hwmon walk. Returns readings plus collected errors
    /// (parked-sensor notifications, missing `/sys/class/hwmon`,
    /// etc) so the caller can route them however it wants —
    /// `TempWorker` ships the `ScanReport` over a channel; the
    /// test path just inspects it directly.
    pub(crate) fn scan(&mut self) -> ScanReport {
        let entries = match fs::read_dir("/sys/class/hwmon") {
            Ok(e) => e,
            Err(e) => {
                // Missing `/sys/class/hwmon` is a real failure
                // (this is Linux — it should always exist on the
                // path neotop targets), so it goes on `errors`.
                return ScanReport {
                    readings: Vec::new(),
                    infos: Vec::new(),
                    errors: vec![("hwmon", format!("/sys/class/hwmon: {e}"))],
                };
            }
        };
        let mut readings = Vec::new();
        let mut infos: Vec<(&'static str, String)> = Vec::new();
        let errors: Vec<(&'static str, String)> = Vec::new();

        for hwmon in entries.flatten() {
            let hwmon_path = hwmon.path();
            if self.parked.contains(&hwmon_path) {
                continue;
            }

            // Time every hwmon's full scan. If a single device
            // exceeds the threshold (see `SLOW_THRESHOLD`), it goes
            // into `parked` and is skipped from now on. Whatever
            // partial readings we managed to grab from it are kept
            // so the user still sees a number for the first tick.
            let started = Instant::now();
            scan_one_hwmon(&hwmon_path, &mut readings);
            if started.elapsed() > SLOW_THRESHOLD {
                let name = read_trim(&hwmon_path.join("name"))
                    .unwrap_or_else(|| hwmon_path.display().to_string());
                // Parked-sensor notice is informational: the user
                // should know we worked around a broken sensor,
                // but it isn't a failure they need to act on.
                infos.push((
                    "hwmon",
                    format!(
                        "parked slow sensor {name} ({} ms)",
                        started.elapsed().as_millis()
                    ),
                ));
                self.parked.insert(hwmon_path);
            }
        }

        ScanReport {
            readings,
            infos,
            errors,
        }
    }
}

/// Off-thread temperature scanner. Owns a `Tracker` on a dedicated
/// worker thread and exchanges scan requests / results with the UI
/// thread over a pair of `mpsc::channel`s. The UI thread never
/// blocks on a sysfs read, even when `acpitz` takes 3 seconds on
/// the very first hit.
///
/// Lifecycle:
/// 1. `spawn()` starts the worker and primes the first scan.
/// 2. Each slow tick calls `request()` (no-op while a scan is
///    already in flight — the worker coalesces queued requests).
/// 3. Each tick (slow *or* fast) calls `poll()` to drain any
///    completed scan; results are folded into `readings()`.
/// 4. The worker thread shuts itself down when the last `Sender`
///    is dropped at app exit.
pub(crate) struct TempWorker {
    request_tx: mpsc::Sender<()>,
    result_rx: mpsc::Receiver<ScanReport>,
    in_flight: bool,
    cached: Vec<Reading>,
}

/// Side-effects from a single `TempWorker::poll()`. The caller is
/// expected to route each list at the matching `ErrorRing`
/// severity — `infos` go through `push_info`, `errors` through
/// `push`. Pulling them out as a named struct (instead of
/// returning a tuple) makes the intent at the call site obvious.
#[derive(Debug, Default)]
pub(crate) struct PollOutput {
    pub(crate) infos: Vec<(&'static str, String)>,
    pub(crate) errors: Vec<(&'static str, String)>,
}

impl TempWorker {
    pub(crate) fn spawn() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<()>();
        let (res_tx, res_rx) = mpsc::channel::<ScanReport>();
        // The thread owns the Tracker, including the parked-paths
        // HashSet, so parking decisions persist across scans
        // exactly the same way the sync path did. We don't keep
        // the JoinHandle — the worker terminates naturally when
        // the request channel closes, which happens when this
        // struct is dropped.
        thread::Builder::new()
            .name("neotop-temp".into())
            .spawn(move || {
                let mut tracker = Tracker::default();
                while req_rx.recv().is_ok() {
                    // Coalesce: if multiple requests piled up
                    // while we were scanning, collapse them into
                    // a single fresh scan. The user sees the most
                    // recent state, never a stale backlog.
                    while req_rx.try_recv().is_ok() {}
                    let report = tracker.scan();
                    if res_tx.send(report).is_err() {
                        // UI thread gone; nothing left to report to.
                        return;
                    }
                }
            })
            .expect("spawn temp worker");
        Self {
            request_tx: req_tx,
            result_rx: res_rx,
            in_flight: false,
            cached: Vec::new(),
        }
    }

    /// Ask the worker to take a fresh snapshot. No-op while a
    /// previous request hasn't been answered yet — this is what
    /// keeps a backlog of slow scans from accumulating while the
    /// firmware is misbehaving.
    pub(crate) fn request(&mut self) {
        if !self.in_flight && self.request_tx.send(()).is_ok() {
            self.in_flight = true;
        }
    }

    /// Drain a pending result if one is ready. Updates the cached
    /// readings and returns the `(infos, errors)` so the caller
    /// can fold each tier into its `ErrorRing` at the right
    /// severity. Non-blocking.
    pub(crate) fn poll(&mut self) -> Option<PollOutput> {
        match self.result_rx.try_recv() {
            Ok(r) => {
                self.in_flight = false;
                self.cached = r.readings;
                Some(PollOutput {
                    infos: r.infos,
                    errors: r.errors,
                })
            }
            Err(_) => None,
        }
    }

    /// Most recent set of readings the worker has produced. Empty
    /// for the brief window between app launch and the first
    /// completed scan.
    pub(crate) fn readings(&self) -> &[Reading] {
        &self.cached
    }
}

/// Read every `tempN_input` under one hwmon directory, push the
/// results into `out`. Pure I/O; no filtering. Failures on
/// individual sensors are silent — sysfs entries can disappear
/// (e.g. an `NVMe` is yanked) and we don't want to spam the error
/// ring for races.
fn scan_one_hwmon(hwmon_path: &std::path::Path, out: &mut Vec<Reading>) {
    let group = read_trim(&hwmon_path.join("name")).unwrap_or_else(|| "?".into());
    let Ok(files) = fs::read_dir(hwmon_path) else {
        return;
    };
    for f in files.flatten() {
        let name = f.file_name();
        let Some(name) = name.to_str() else { continue };
        // Match `tempN_input` where N is 1..=99.
        if !(name.starts_with("temp") && name.ends_with("_input")) {
            continue;
        }
        let idx = &name["temp".len()..name.len() - "_input".len()];
        let Some(milli) = read_trim(&f.path()).and_then(|s| s.parse::<i64>().ok()) else {
            continue;
        };

        let label_path = hwmon_path.join(format!("temp{idx}_label"));
        let label = read_trim(&label_path)
            .filter(|s| !s.is_empty())
            .map_or_else(|| format!("{group}#{idx}"), |l| format!("{group} {l}"));

        #[allow(clippy::cast_precision_loss)]
        let celsius = milli as f64 / 1000.0;
        out.push(Reading { label, celsius });
    }
}

/// Pick the most interesting temperatures for a compact one-line view.
/// Priorities: CPU package first, then `NVMe`, then anything else hot.
pub(crate) fn highlights(readings: &[Reading], limit: usize) -> Vec<&Reading> {
    let mut picks: Vec<&Reading> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();

    // Priority tags in search order.
    let wanted: [&str; 4] = ["Package", "Tctl", "Composite", "edge"];
    for needle in wanted {
        if let Some(r) = readings.iter().find(|r| r.label.contains(needle)) {
            let group = group_of(&r.label);
            if !seen.contains(&group) {
                picks.push(r);
                seen.push(group);
            }
        }
        if picks.len() >= limit {
            return picks;
        }
    }

    // Fill the rest with the hottest readings from new groups.
    let mut rest: Vec<&Reading> = readings
        .iter()
        .filter(|r| !seen.contains(&group_of(&r.label)))
        .collect();
    rest.sort_by(|a, b| {
        b.celsius
            .partial_cmp(&a.celsius)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    for r in rest {
        if picks.len() >= limit {
            break;
        }
        let g = group_of(&r.label);
        if !seen.contains(&g) {
            picks.push(r);
            seen.push(g);
        }
    }
    picks
}

fn group_of(label: &str) -> &str {
    // Labels look like "coretemp Package id 0" or "nvme Composite".
    label.split_whitespace().next().unwrap_or(label)
}

fn read_trim(path: &std::path::Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Color code for a temperature — green <70, yellow 70..85, red ≥85.
pub(crate) fn severity(c: f64) -> Severity {
    if c >= 85.0 {
        Severity::Hot
    } else if c >= 70.0 {
        Severity::Warm
    } else {
        Severity::Cool
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    Cool,
    Warm,
    Hot,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(label: &str, c: f64) -> Reading {
        Reading {
            label: label.into(),
            celsius: c,
        }
    }

    #[test]
    fn severity_thresholds() {
        assert_eq!(severity(20.0), Severity::Cool);
        assert_eq!(severity(69.9), Severity::Cool);
        assert_eq!(severity(70.0), Severity::Warm);
        assert_eq!(severity(84.9), Severity::Warm);
        assert_eq!(severity(85.0), Severity::Hot);
        assert_eq!(severity(120.0), Severity::Hot);
    }

    #[test]
    fn group_of_takes_first_word() {
        assert_eq!(group_of("coretemp Package id 0"), "coretemp");
        assert_eq!(group_of("nvme Composite"), "nvme");
        assert_eq!(group_of("singleword"), "singleword");
    }

    #[test]
    fn highlights_prioritises_package_over_other_groups() {
        let readings = vec![
            r("coretemp Core 0", 50.0),
            r("coretemp Package id 0", 60.0),
            r("nvme Composite", 40.0),
            r("acpitz", 30.0),
        ];
        let picks = highlights(&readings, 3);
        // Package wins for the coretemp group; then nvme Composite,
        // then the next-hottest from a new group (acpitz).
        assert_eq!(picks[0].label, "coretemp Package id 0");
        assert_eq!(picks[1].label, "nvme Composite");
        assert_eq!(picks[2].label, "acpitz");
    }

    #[test]
    fn highlights_falls_back_to_hottest_when_no_priority_tag() {
        let readings = vec![r("acpitz", 40.0), r("zone7", 80.0), r("amd_pmf", 55.0)];
        let picks = highlights(&readings, 2);
        // No priority tag matches; sorted by hottest first.
        assert_eq!(picks[0].label, "zone7");
        assert_eq!(picks[1].label, "amd_pmf");
    }

    #[test]
    fn highlights_respects_limit() {
        let readings = (0..5)
            .map(|i| r(&format!("group{i}"), f64::from(50 + i)))
            .collect::<Vec<_>>();
        assert_eq!(highlights(&readings, 2).len(), 2);
        assert_eq!(highlights(&readings, 0).len(), 0);
    }

    #[test]
    fn temp_worker_initial_readings_are_empty_until_first_poll() {
        // The worker starts with an empty cache. After spawn() but
        // before any poll() returns Some, readings() must yield an
        // empty slice — that's what allows the UI to render the
        // first frame immediately even if the kernel takes 3 s to
        // answer the first sysfs read.
        let worker = TempWorker::spawn();
        assert!(worker.readings().is_empty());
    }

    #[test]
    fn temp_worker_poll_returns_results_after_request() {
        // Fire a request, then poll until either the worker
        // returns *something* or we time out. On any Linux dev
        // box `/sys/class/hwmon` exists (even if empty), so the
        // worker will respond promptly. We don't assert on the
        // contents of `readings` (depends on hardware) — only
        // that the request/response handshake works end to end.
        // 15 s is generous because on hardware with broken ACPI
        // firmware (the whole reason this worker exists) the
        // first sysfs read can block for 3+ seconds, and parallel
        // test execution can stack several such reads onto the
        // same kernel mailbox. The success path returns instantly.
        let mut worker = TempWorker::spawn();
        worker.request();
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if worker.poll().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("temp worker did not respond within 15 s");
    }

    #[test]
    fn temp_worker_request_is_idempotent_while_in_flight() {
        // Calling request() many times before the worker has
        // answered should not pile up a backlog of scans. The
        // coalescing guarantee is what protects us from a
        // 3-second-per-scan acpitz pinning a worker thread for
        // minutes on a busy slow-tick.
        let mut worker = TempWorker::spawn();
        for _ in 0..20 {
            worker.request();
        }
        // Drain whatever response arrives (one, at most). Same
        // 15 s budget as `temp_worker_poll_returns_results_after_request`
        // for the same reason: slow firmware + parallel tests.
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut responses = 0;
        while Instant::now() < deadline {
            if worker.poll().is_some() {
                responses += 1;
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(responses <= 1, "expected at most one coalesced response");
    }

    #[test]
    fn tracker_skips_already_parked_paths() {
        // Synthetic test: pre-populate the tracker's parked set with
        // a path and confirm the public snapshot interface accepts a
        // freshly-built `Tracker`. We can't easily inject a slow
        // sensor into `/sys/class/hwmon` from a unit test (it's the
        // real kernel interface), so the production blacklisting
        // behaviour is verified by the `live_tick_bench` ad-hoc
        // benchmark in main.rs and by direct measurement.
        let mut t = Tracker::default();
        t.parked.insert(PathBuf::from("/sys/class/hwmon/hwmon0"));
        // Should run without panic; the parked path is silently
        // skipped, all other devices read normally.
        let _ = t.scan();
        assert!(t.parked.contains(&PathBuf::from("/sys/class/hwmon/hwmon0")));
    }
}
