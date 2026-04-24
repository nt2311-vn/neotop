//! battery.rs — laptop battery readout from `/sys/class/power_supply/`.
//!
//! We scan `/sys/class/power_supply/*` looking for entries whose
//! `type` is `Battery`. Name varies by vendor (`BAT0`, `BAT1`, `CMB0`,
//! etc), so we iterate rather than hard-coding. Missing files / empty
//! directory → `None`, rendered as a blank cell in the UI.
//!
//! Kernel docs: <https://www.kernel.org/doc/html/latest/power/power_supply_class.html>

use std::fs;

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
    let Ok(entries) = fs::read_dir("/sys/class/power_supply") else {
        return Vec::new();
    };
    let mut out = Vec::new();

    for e in entries.flatten() {
        let path = e.path();
        // Only power supplies of type "Battery". USB-C chargers, main
        // AC, etc all live here too — skip them.
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
            .and_then(|s| parse_capacity(&s))
        else {
            continue;
        };
        let status = fs::read_to_string(path.join("status"))
            .ok()
            .map_or_else(|| "Unknown".into(), |s| s.trim().to_string());
        let watts = fs::read_to_string(path.join("power_now"))
            .ok()
            .and_then(|s| parse_power_now_watts(&s));

        out.push(Battery {
            name,
            percent,
            status,
            watts,
        });
    }
    out
}

/// `capacity` sysfs file holds an integer 0..=100. We trim whitespace
/// because the file ends in a newline.
pub(crate) fn parse_capacity(raw: &str) -> Option<u8> {
    raw.trim().parse::<u8>().ok()
}

/// `power_now` is in microwatts; some drivers expose it as negative
/// while charging, some as positive. We keep the sign so the UI can
/// distinguish draw vs charge.
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
