//! temp.rs — hwmon temperature readout.
//!
//! Walks `/sys/class/hwmon/hwmon*`, finds every `tempN_input` file,
//! reads the value (milli-Celsius), and labels it via `tempN_label`
//! when present or the hwmon `name` otherwise.
//!
//! We don't depend on `lm_sensors` or any libsensors binding — sysfs
//! is the same data, one layer down. This is what `lm_sensors` itself
//! reads.

use std::fs;

#[derive(Debug, Clone)]
pub(crate) struct Reading {
    pub(crate) label: String,
    pub(crate) celsius: f64,
}

pub(crate) fn snapshot() -> Vec<Reading> {
    let Ok(entries) = fs::read_dir("/sys/class/hwmon") else {
        return Vec::new();
    };
    let mut readings = Vec::new();

    for hwmon in entries.flatten() {
        let hwmon_path = hwmon.path();
        let group = read_trim(&hwmon_path.join("name")).unwrap_or_else(|| "?".into());

        let Ok(files) = fs::read_dir(&hwmon_path) else {
            continue;
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
            readings.push(Reading { label, celsius });
        }
    }

    readings
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
