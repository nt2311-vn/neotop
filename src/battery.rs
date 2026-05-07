//! battery.rs — battery status from `/sys/class/power_supply`.
//!
//! macOS: uses `system_profiler SPPowerDataType` for battery info.

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "macos")]
use std::process::Command;

#[derive(Debug, Clone)]
#[allow(dead_code)] // `name` shown in future multi-battery variants
pub(crate) struct Battery {
    /// Display name, e.g. `"BAT0"`.
    pub(crate) name: String,
    /// 0..=100. The kernel already clamps this.
    pub(crate) percent: u8,
    /// `"Charging"`, `"Discharging"`, `"Full"`, `"Not charging"`,
    /// `"Unknown"`. We pass through whatever sysfs reports.
    pub(crate) status: String,
    /// Instantaneous draw/charge rate in watts (positive = draw on
    /// discharge, negative when charging on most laptops). `None` if
    /// the driver doesn't expose `power_now`.
    pub(crate) watts: Option<f64>,
}

pub(crate) fn snapshot() -> Vec<Battery> {
    #[cfg(target_os = "linux")]
    {
        let Ok(entries) = fs::read_dir("/sys/class/power_supply") else {
            return Vec::new();
        };
        let mut out = Vec::new();

        for e in entries.flatten() {
            let path = e.path();
            let kind = fs::read_to_string(path.join("type"))
                .ok()
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            if kind != "Battery" {
                continue;
            }

            let Some(name) = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let Some(percent) = fs::read_to_string(path.join("capacity"))
                .ok()
                .and_then(|s| s.trim().parse::<u8>().ok())
            else {
                continue;
            };
            let status = fs::read_to_string(path.join("status"))
                .ok()
                .map_or_else(|| "Unknown".into(), |s| s.trim().to_string());
            #[allow(clippy::cast_precision_loss)]
            let watts = fs::read_to_string(path.join("power_now"))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(|uw| uw as f64 / 1_000_000.0);

            out.push(Battery {
                name,
                percent,
                status,
                watts,
            });
        }
        out
    }
    #[cfg(target_os = "macos")]
    {
        snapshot_macos()
    }
}

#[cfg(target_os = "macos")]
fn snapshot_macos() -> Vec<Battery> {
    // Use system_profiler to get battery info on macOS
    let output = match Command::new("system_profiler")
        .args(["SPPowerDataType", "-json"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    let json_str = match String::from_utf8(output.stdout) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    // Parse JSON to extract battery info
    // system_profiler returns complex nested JSON, so we do a simple text search
    // for the key fields we need
    let percent = extract_json_value(&json_str, "\"ChargeRemaining\"")
        .and_then(|s| s.trim().parse::<u8>().ok())
        .unwrap_or(0);

    let status_raw =
        extract_json_value(&json_str, "\"BatteryPower\"").unwrap_or_else(|| "Unknown".to_string());

    let status = if status_raw.contains("AC Power") {
        "Charging".to_string()
    } else if status_raw.contains("Battery Power") {
        "Discharging".to_string()
    } else {
        status_raw
    };

    if percent == 0 {
        return Vec::new();
    }

    vec![Battery {
        name: "Battery".to_string(),
        percent,
        status,
        watts: None, // system_profiler doesn't provide wattage easily
    }]
}

#[cfg(target_os = "macos")]
fn extract_json_value(json: &str, key: &str) -> Option<String> {
    // Simple JSON value extraction for known keys
    // This is a pragmatic approach to avoid adding a JSON dependency
    let key_pattern = format!("{}:", key);
    if let Some(idx) = json.find(&key_pattern) {
        let after_key = &json[idx + key_pattern.len()..];
        if let Some(start) = after_key.find(|c: char| c.is_ascii_digit()) {
            let value_part = &after_key[start..];
            let end = value_part.find([',', '}']).unwrap_or(value_part.len());
            return Some(value_part[..end].trim().to_string());
        }
    }
    None
}

/// `capacity` sysfs file holds an integer 0..=100. We trim whitespace
/// because the file ends in a newline.
#[allow(dead_code)]
pub(crate) fn parse_capacity(raw: &str) -> Option<u8> {
    raw.trim().parse::<u8>().ok()
}

/// `power_now` is in microwatts; some drivers expose it as negative
/// while charging, some as positive. We keep the sign so the UI can
/// distinguish draw vs charge.
#[allow(dead_code)]
pub(crate) fn parse_power_now_watts(raw: &str) -> Option<f64> {
    let uw: i64 = raw.trim().parse().ok()?;
    #[allow(clippy::cast_precision_loss)]
    Some(uw as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_capacity_trims_newline() {
        assert_eq!(parse_capacity("87\n"), Some(87));
        assert_eq!(parse_capacity("0\n"), Some(0));
        assert_eq!(parse_capacity("100\n"), Some(100));
    }

    #[test]
    fn parse_capacity_rejects_garbage() {
        assert_eq!(parse_capacity(""), None);
        assert_eq!(parse_capacity("nope\n"), None);
        // 256 doesn't fit in u8 — kernel never writes this but be safe.
        assert_eq!(parse_capacity("256\n"), None);
    }

    #[test]
    fn parse_power_now_converts_uw_to_watts() {
        assert!((parse_power_now_watts("12000000\n").unwrap() - 12.0).abs() < 1e-9);
        assert!((parse_power_now_watts("-3500000\n").unwrap() - (-3.5)).abs() < 1e-9);
        assert_eq!(parse_power_now_watts("0\n"), Some(0.0));
    }

    #[test]
    fn parse_power_now_rejects_garbage() {
        assert_eq!(parse_power_now_watts(""), None);
        assert_eq!(parse_power_now_watts("???"), None);
    }
}
