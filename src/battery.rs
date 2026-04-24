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
            .and_then(|s| s.trim().parse::<u8>().ok())
        else {
            continue;
        };
        let status = fs::read_to_string(path.join("status"))
            .ok()
            .map_or_else(|| "Unknown".into(), |s| s.trim().to_string());
        // `power_now` is in microwatts; some drivers expose it as
        // negative while charging, some as positive. We keep the sign
        // so the UI can distinguish draw vs charge if it wants.
        let watts = fs::read_to_string(path.join("power_now"))
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .map(|uw| {
                #[allow(clippy::cast_precision_loss)]
                let w = uw as f64 / 1_000_000.0;
                w
            });

        out.push(Battery {
            name,
            percent,
            status,
            watts,
        });
    }
    out
}
