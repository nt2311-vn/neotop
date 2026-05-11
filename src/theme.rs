//! theme.rs — semantic colour palette with built-in presets and TOML overrides.
//!
//! Every colour in the UI is accessed through a `Theme` field so the
//! entire look can be swapped at runtime (key `T`) or via
//! `~/.config/neotop/config.toml`.  The default "dark" preset matches
//! the original hardcoded palette — a high-contrast dark theme that
//! reads well on opaque-black terminals.

use ratatui::style::Color;
use std::path::Path;
// ---------------------------------------------------------------------------
// Theme struct — one field per semantic colour role
// ---------------------------------------------------------------------------

/// Semantic colour palette.  All draw functions read colours from here
/// instead of hardcoding `Color::` variants.
#[derive(Debug, Clone)]
pub(crate) struct Theme {
    // CPU load ramp (shared by per-core, sparklines, gauges, vCPU)
    pub cpu_idle: Color,
    pub cpu_low: Color,
    pub cpu_mid: Color,
    pub cpu_high: Color,

    // Labels / axis / border
    pub label: Color,
    pub border: Color,

    // Badges (title bar, key hints, paused banner)
    pub badge_fg: Color,
    pub badge_bg: Color,

    // Sparkline colours (host-history row)
    pub spark_cpu: Color,
    pub spark_mem: Color,
    pub spark_net_down: Color,
    pub spark_net_up: Color,
    pub spark_gpu: Color,
    pub spark_vram: Color,

    // Memory composition bar segments
    pub mem_used: Color,
    pub mem_buffers: Color,
    pub mem_cached: Color,
    pub mem_free: Color,

    // Process state colours
    pub proc_r: Color,
    pub proc_d: Color,
    pub proc_z: Color,
    pub proc_t: Color,
    pub proc_i: Color,
    pub proc_other: Color,

    // Group band colours
    pub group_container: Color,
    pub group_vm: Color,
    pub group_runtime: Color,
    pub group_app: Color,
    pub group_system: Color,
    pub group_native: Color,

    // Battery
    pub battery_good: Color,
    pub battery_mid: Color,
    pub battery_low: Color,

    // Gauge empty fill
    pub gauge_empty: Color,

    // Swap colour ramp
    pub swap_high: Color,
    pub swap_mid: Color,
    pub swap_low: Color,

    // Perf ms colour ramp
    pub perf_slow: Color,
    pub perf_mid: Color,
    pub perf_ok: Color,

    // Error badge
    pub err_warn_bg: Color,
    pub err_info_bg: Color,

    // Selection highlight
    pub highlight_bg: Color,

    // GPU name
    pub gpu_name: Color,

    // Filter badge bg
    pub filter_bg: Color,
}

// ---------------------------------------------------------------------------
// Built-in presets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThemePreset {
    Dark,
    Light,
    Monokai,
    Tty,
}

const PRESETS: [ThemePreset; 4] = [
    ThemePreset::Dark,
    ThemePreset::Light,
    ThemePreset::Monokai,
    ThemePreset::Tty,
];

impl ThemePreset {
    #[allow(dead_code)]
    fn label(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Monokai => "monokai",
            Self::Tty => "tty",
        }
    }

    /// Cycle to the next preset.
    pub(crate) fn next(self) -> ThemePreset {
        let idx = PRESETS
            .iter()
            .position(|&v| v == self)
            .map_or(0, |i| (i + 1) % PRESETS.len());
        PRESETS[idx]
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn colors(self) -> Theme {
        match self {
            Self::Dark => Theme {
                // Catppuccin Mocha — high-contrast dark palette.
                // https://github.com/catppuccin/catppuccin#-palette
                cpu_idle: Color::Rgb(108, 112, 134), // Overlay0
                cpu_low: Color::Rgb(166, 227, 161),  // Green
                cpu_mid: Color::Rgb(250, 179, 135),  // Peach
                cpu_high: Color::Rgb(243, 139, 168), // Red

                label: Color::Rgb(108, 112, 134), // Overlay0
                border: Color::Rgb(88, 91, 112),  // Surface2

                badge_fg: Color::Rgb(30, 30, 46),    // Base
                badge_bg: Color::Rgb(137, 180, 250), // Blue

                spark_cpu: Color::Rgb(166, 227, 161), // Green
                spark_mem: Color::Rgb(203, 166, 247), // Mauve
                spark_net_down: Color::Rgb(148, 226, 213), // Teal
                spark_net_up: Color::Rgb(137, 220, 235), // Sky
                spark_gpu: Color::Rgb(243, 139, 168), // Red
                spark_vram: Color::Rgb(245, 194, 231), // Pink

                mem_used: Color::Rgb(243, 139, 168),    // Red
                mem_buffers: Color::Rgb(137, 180, 250), // Blue
                mem_cached: Color::Rgb(148, 226, 213),  // Teal
                mem_free: Color::Rgb(88, 91, 112),      // Surface2

                proc_r: Color::Rgb(166, 227, 161),     // Green
                proc_d: Color::Rgb(243, 139, 168),     // Red
                proc_z: Color::Rgb(249, 226, 175),     // Yellow
                proc_t: Color::Rgb(137, 220, 235),     // Sky
                proc_i: Color::Rgb(108, 112, 134),     // Overlay0
                proc_other: Color::Rgb(186, 194, 222), // Subtext1

                group_container: Color::Rgb(137, 180, 250), // Blue
                group_vm: Color::Rgb(203, 166, 247),        // Mauve
                group_runtime: Color::Rgb(148, 226, 213),   // Teal
                group_app: Color::Rgb(249, 226, 175),       // Yellow
                group_system: Color::Rgb(147, 153, 178),    // Overlay2
                group_native: Color::Rgb(108, 112, 134),    // Overlay0

                battery_good: Color::Rgb(166, 227, 161), // Green
                battery_mid: Color::Rgb(250, 179, 135),  // Peach
                battery_low: Color::Rgb(243, 139, 168),  // Red

                gauge_empty: Color::Rgb(49, 50, 68), // Surface0

                swap_high: Color::Rgb(243, 139, 168), // Red
                swap_mid: Color::Rgb(250, 179, 135),  // Peach
                swap_low: Color::Reset,

                perf_slow: Color::Rgb(243, 139, 168), // Red
                perf_mid: Color::Rgb(250, 179, 135),  // Peach
                perf_ok: Color::Rgb(108, 112, 134),   // Overlay0

                err_warn_bg: Color::Rgb(243, 139, 168), // Red
                err_info_bg: Color::Rgb(249, 226, 175), // Yellow

                highlight_bg: Color::Rgb(69, 71, 90), // Surface1

                gpu_name: Color::Rgb(137, 220, 235), // Sky

                filter_bg: Color::Rgb(249, 226, 175), // Yellow
            },
            Self::Light => Theme {
                // On a light terminal the ANSI "standard" colours
                // (Red, Green, …) are the *dark* variants — that's
                // what we want for readable contrast on a white bg.
                cpu_idle: Color::Gray,
                cpu_low: Color::Green,
                cpu_mid: Color::Yellow,
                cpu_high: Color::Red,

                label: Color::Gray,
                border: Color::Gray,

                badge_fg: Color::White,
                badge_bg: Color::Cyan,

                spark_cpu: Color::Green,
                spark_mem: Color::Magenta,
                spark_net_down: Color::Cyan,
                spark_net_up: Color::Yellow,
                spark_gpu: Color::Red,
                spark_vram: Color::Magenta,

                mem_used: Color::Red,
                mem_buffers: Color::Blue,
                mem_cached: Color::Cyan,
                mem_free: Color::Gray,

                proc_r: Color::Green,
                proc_d: Color::Red,
                proc_z: Color::Magenta,
                proc_t: Color::Yellow,
                proc_i: Color::Gray,
                proc_other: Color::DarkGray,

                group_container: Color::Cyan,
                group_vm: Color::Blue,
                group_runtime: Color::Yellow,
                group_app: Color::Magenta,
                group_system: Color::Gray,
                group_native: Color::Gray,

                battery_good: Color::Green,
                battery_mid: Color::Yellow,
                battery_low: Color::Red,

                gauge_empty: Color::Gray,

                swap_high: Color::Red,
                swap_mid: Color::Yellow,
                swap_low: Color::Reset,

                perf_slow: Color::Red,
                perf_mid: Color::Yellow,
                perf_ok: Color::Gray,

                err_warn_bg: Color::Red,
                err_info_bg: Color::Yellow,

                highlight_bg: Color::Gray,

                gpu_name: Color::Cyan,

                filter_bg: Color::Yellow,
            },
            Self::Monokai => Theme {
                cpu_idle: Color::Indexed(244),
                cpu_low: Color::Indexed(186),
                cpu_mid: Color::Indexed(228),
                cpu_high: Color::Indexed(196),

                label: Color::Indexed(244),
                border: Color::Indexed(244),

                badge_fg: Color::Indexed(232),
                badge_bg: Color::Indexed(37),

                spark_cpu: Color::Indexed(186),
                spark_mem: Color::Indexed(176),
                spark_net_down: Color::Indexed(81),
                spark_net_up: Color::Indexed(228),
                spark_gpu: Color::Indexed(196),
                spark_vram: Color::Indexed(176),

                mem_used: Color::Indexed(196),
                mem_buffers: Color::Indexed(67),
                mem_cached: Color::Indexed(81),
                mem_free: Color::Indexed(244),

                proc_r: Color::Indexed(186),
                proc_d: Color::Indexed(196),
                proc_z: Color::Indexed(176),
                proc_t: Color::Indexed(228),
                proc_i: Color::Indexed(244),
                proc_other: Color::Indexed(250),

                group_container: Color::Indexed(81),
                group_vm: Color::Indexed(67),
                group_runtime: Color::Indexed(228),
                group_app: Color::Indexed(214),
                group_system: Color::Indexed(244),
                group_native: Color::Indexed(244),

                battery_good: Color::Indexed(186),
                battery_mid: Color::Indexed(228),
                battery_low: Color::Indexed(196),

                gauge_empty: Color::Indexed(244),

                swap_high: Color::Indexed(196),
                swap_mid: Color::Indexed(228),
                swap_low: Color::Reset,

                perf_slow: Color::Indexed(196),
                perf_mid: Color::Indexed(228),
                perf_ok: Color::Indexed(244),

                err_warn_bg: Color::Indexed(196),
                err_info_bg: Color::Indexed(228),

                highlight_bg: Color::Indexed(244),

                gpu_name: Color::Indexed(81),

                filter_bg: Color::Indexed(228),
            },
            Self::Tty => Theme {
                // Monochrome — safe for 16-colour VTs
                cpu_idle: Color::White,
                cpu_low: Color::White,
                cpu_mid: Color::White,
                cpu_high: Color::White,

                label: Color::White,
                border: Color::White,

                badge_fg: Color::Black,
                badge_bg: Color::White,

                spark_cpu: Color::White,
                spark_mem: Color::White,
                spark_net_down: Color::White,
                spark_net_up: Color::White,
                spark_gpu: Color::White,
                spark_vram: Color::White,

                mem_used: Color::White,
                mem_buffers: Color::White,
                mem_cached: Color::White,
                mem_free: Color::White,

                proc_r: Color::White,
                proc_d: Color::White,
                proc_z: Color::White,
                proc_t: Color::White,
                proc_i: Color::White,
                proc_other: Color::White,

                group_container: Color::White,
                group_vm: Color::White,
                group_runtime: Color::White,
                group_app: Color::White,
                group_system: Color::White,
                group_native: Color::White,

                battery_good: Color::White,
                battery_mid: Color::White,
                battery_low: Color::White,

                gauge_empty: Color::White,

                swap_high: Color::White,
                swap_mid: Color::White,
                swap_low: Color::White,

                perf_slow: Color::White,
                perf_mid: Color::White,
                perf_ok: Color::White,

                err_warn_bg: Color::White,
                err_info_bg: Color::White,

                highlight_bg: Color::White,

                gpu_name: Color::White,

                filter_bg: Color::White,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: resolve a TOML colour string → ratatui Color
// ---------------------------------------------------------------------------

fn parse_color(s: &str) -> Option<Color> {
    // Named ratatui colours (match ratatui::style::Color enum names)
    let named = match s {
        "Black" => Color::Black,
        "Red" => Color::Red,
        "Green" => Color::Green,
        "Yellow" => Color::Yellow,
        "Blue" => Color::Blue,
        "Magenta" => Color::Magenta,
        "Cyan" => Color::Cyan,
        "Gray" | "Grey" => Color::Gray,
        "DarkGray" | "DarkGrey" => Color::DarkGray,
        "LightRed" => Color::LightRed,
        "LightGreen" => Color::LightGreen,
        "LightYellow" => Color::LightYellow,
        "LightBlue" => Color::LightBlue,
        "LightMagenta" => Color::LightMagenta,
        "LightCyan" => Color::LightCyan,
        "White" => Color::White,
        "Reset" => Color::Reset,
        _ => {
            // Indexed: "256" or "i256" → Color::Indexed(256)
            if let Some(n) = s
                .strip_prefix('i')
                .or(Some(s))
                .and_then(|v| v.parse::<u8>().ok())
            {
                return Some(Color::Indexed(n));
            }
            // RGB: "r,g,b" or "#rrggbb"
            if let Some((r, g, b)) = parse_rgb(s) {
                return Some(Color::Rgb(r, g, b));
            }
            return None;
        }
    };
    Some(named)
}

fn parse_rgb(s: &str) -> Option<(u8, u8, u8)> {
    // "#rrggbb"
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some((r, g, b));
        }
    }
    // "r,g,b"
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() == 3 {
        let r = parts[0].parse().ok()?;
        let g = parts[1].parse().ok()?;
        let b = parts[2].parse().ok()?;
        return Some((r, g, b));
    }
    None
}

// ---------------------------------------------------------------------------
// Config file parsing
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
#[serde(default)]
struct ConfigFile {
    theme: String,
    colors: ConfigColors,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            colors: ConfigColors::default(),
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default)]
struct ConfigColors {
    cpu_idle: Option<String>,
    cpu_low: Option<String>,
    cpu_mid: Option<String>,
    cpu_high: Option<String>,
    label: Option<String>,
    border: Option<String>,
    badge_fg: Option<String>,
    badge_bg: Option<String>,
    spark_cpu: Option<String>,
    spark_mem: Option<String>,
    spark_net_down: Option<String>,
    spark_net_up: Option<String>,
    spark_gpu: Option<String>,
    spark_vram: Option<String>,
    mem_used: Option<String>,
    mem_buffers: Option<String>,
    mem_cached: Option<String>,
    mem_free: Option<String>,
    proc_r: Option<String>,
    proc_d: Option<String>,
    proc_z: Option<String>,
    proc_t: Option<String>,
    proc_i: Option<String>,
    proc_other: Option<String>,
    group_container: Option<String>,
    group_vm: Option<String>,
    group_runtime: Option<String>,
    group_app: Option<String>,
    group_system: Option<String>,
    group_native: Option<String>,
    battery_good: Option<String>,
    battery_mid: Option<String>,
    battery_low: Option<String>,
    gauge_empty: Option<String>,
    swap_high: Option<String>,
    swap_mid: Option<String>,
    swap_low: Option<String>,
    perf_slow: Option<String>,
    perf_mid: Option<String>,
    perf_ok: Option<String>,
    err_warn_bg: Option<String>,
    err_info_bg: Option<String>,
    highlight_bg: Option<String>,
    gpu_name: Option<String>,
    filter_bg: Option<String>,
}

// Apply TOML colour overrides onto a Theme.
macro_rules! apply_override {
    ($theme:expr, $colors:expr, $($field:ident),* $(,)?) => {
        $(
            if let Some(ref v) = $colors.$field {
                if let Some(c) = parse_color(v) {
                    $theme.$field = c;
                } else {
                    eprintln!("neotop: ignoring invalid colour '{v}' for theme.{}", stringify!($field));
                }
            }
        )*
    };
}

fn apply_overrides(mut theme: Theme, colors: &ConfigColors) -> Theme {
    apply_override!(
        theme,
        colors,
        cpu_idle,
        cpu_low,
        cpu_mid,
        cpu_high,
        label,
        border,
        badge_fg,
        badge_bg,
        spark_cpu,
        spark_mem,
        spark_net_down,
        spark_net_up,
        spark_gpu,
        spark_vram,
        mem_used,
        mem_buffers,
        mem_cached,
        mem_free,
        proc_r,
        proc_d,
        proc_z,
        proc_t,
        proc_i,
        proc_other,
        group_container,
        group_vm,
        group_runtime,
        group_app,
        group_system,
        group_native,
        battery_good,
        battery_mid,
        battery_low,
        gauge_empty,
        swap_high,
        swap_mid,
        swap_low,
        perf_slow,
        perf_mid,
        perf_ok,
        err_warn_bg,
        err_info_bg,
        highlight_bg,
        gpu_name,
        filter_bg,
    );
    theme
}

/// Resolve a preset name to a `ThemePreset`.  Falls back to `Dark`.
fn resolve_preset(name: &str) -> ThemePreset {
    match name.to_lowercase().as_str() {
        "light" => ThemePreset::Light,
        "monokai" => ThemePreset::Monokai,
        "tty" => ThemePreset::Tty,
        _ => ThemePreset::Dark,
    }
}

/// Load theme from config file.  If the file doesn't exist, returns
/// the Dark preset.  Invalid colour strings produce a stderr warning
/// and use the preset default.
pub(crate) fn load(config_path: Option<&Path>) -> (Theme, ThemePreset) {
    let path = config_path
        .map(std::path::Path::to_path_buf)
        .or_else(|| dirs::config_dir().map(|d| d.join("neotop").join("config.toml")));

    let Some(path) = path else {
        return (ThemePreset::Dark.colors(), ThemePreset::Dark);
    };

    if !path.exists() {
        return (ThemePreset::Dark.colors(), ThemePreset::Dark);
    }

    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("neotop: cannot read {}: {e}", path.display());
            return (ThemePreset::Dark.colors(), ThemePreset::Dark);
        }
    };

    let cfg: ConfigFile = match toml::from_str(&contents) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("neotop: cannot parse {}: {e}", path.display());
            return (ThemePreset::Dark.colors(), ThemePreset::Dark);
        }
    };

    let preset = resolve_preset(&cfg.theme);
    let theme = apply_overrides(preset.colors(), &cfg.colors);
    (theme, preset)
}

// ---------------------------------------------------------------------------
// Convenience methods matching the old free functions
// ---------------------------------------------------------------------------

impl Theme {
    /// Four-stop CPU load colour ramp.  Same breakpoints as the old
    /// `cpu_load_color()` / `cpu_glyph_color()`.
    pub(crate) fn cpu_load_color(&self, pct: f64) -> Color {
        if pct >= 80.0 {
            self.cpu_high
        } else if pct >= 50.0 {
            self.cpu_mid
        } else if pct >= 20.0 {
            self.cpu_low
        } else {
            self.cpu_idle
        }
    }

    /// GPU busy% colour (three-stop: high / mid / low).
    pub(crate) fn gpu_busy_color(&self, busy: f64) -> Color {
        if busy >= 80.0 {
            self.cpu_high
        } else if busy >= 50.0 {
            self.cpu_mid
        } else {
            self.cpu_low
        }
    }

    /// Process state character colour.
    pub(crate) fn proc_state_color(&self, c: char) -> Color {
        match c {
            'R' => self.proc_r,
            'D' => self.proc_d,
            'Z' => self.proc_z,
            'T' | 't' => self.proc_t,
            'I' => self.proc_i,
            _ => self.proc_other,
        }
    }

    /// Group band colour.
    pub(crate) fn group_band_color(&self, band: crate::groups::GroupBand) -> Color {
        match band {
            crate::groups::GroupBand::Container => self.group_container,
            crate::groups::GroupBand::Vm => self.group_vm,
            crate::groups::GroupBand::Runtime => self.group_runtime,
            crate::groups::GroupBand::App => self.group_app,
            crate::groups::GroupBand::System => self.group_system,
            crate::groups::GroupBand::Native => self.group_native,
        }
    }

    /// Battery colour based on status and percent.
    pub(crate) fn battery_color(&self, b: &crate::battery::Battery) -> Color {
        if b.status == "Charging" || b.status == "Full" {
            self.battery_good
        } else if b.percent < 15 {
            self.battery_low
        } else if b.percent < 30 {
            self.battery_mid
        } else {
            self.battery_good
        }
    }

    /// Perf ms colour ramp.
    pub(crate) fn ms_color(&self, ms: f64) -> Color {
        if ms >= 100.0 {
            self.perf_slow
        } else if ms >= 20.0 {
            self.perf_mid
        } else {
            self.perf_ok
        }
    }

    /// Swap usage colour ramp.
    pub(crate) fn swap_color(&self, pct: f64) -> Color {
        if pct >= 50.0 {
            self.swap_high
        } else if pct >= 10.0 {
            self.swap_mid
        } else {
            self.swap_low
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_preset_matches_catppuccin_mocha() {
        let t = ThemePreset::Dark.colors();
        // Catppuccin Mocha Rgb values
        assert_eq!(t.cpu_load_color(90.0), Color::Rgb(243, 139, 168)); // Red
        assert_eq!(t.cpu_load_color(60.0), Color::Rgb(250, 179, 135)); // Peach
        assert_eq!(t.cpu_load_color(30.0), Color::Rgb(166, 227, 161)); // Green
        assert_eq!(t.cpu_load_color(10.0), Color::Rgb(108, 112, 134)); // Overlay0
        assert_eq!(t.label, Color::Rgb(108, 112, 134));
        assert_eq!(t.badge_bg, Color::Rgb(137, 180, 250)); // Blue
        assert_eq!(t.spark_cpu, Color::Rgb(166, 227, 161)); // Green
        assert_eq!(t.mem_used, Color::Rgb(243, 139, 168)); // Red
    }

    #[test]
    fn parse_named_colour() {
        assert_eq!(parse_color("Red"), Some(Color::Red));
        assert_eq!(parse_color("DarkGray"), Some(Color::DarkGray));
        assert_eq!(parse_color("Reset"), Some(Color::Reset));
    }

    #[test]
    fn parse_indexed_colour() {
        assert_eq!(parse_color("i196"), Some(Color::Indexed(196)));
        assert_eq!(parse_color("i244"), Some(Color::Indexed(244)));
    }

    #[test]
    fn parse_rgb_colour() {
        assert_eq!(parse_color("#ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_color("128,64,32"), Some(Color::Rgb(128, 64, 32)));
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert_eq!(parse_color("notacolor"), None);
    }

    #[test]
    fn next_preset_cycles() {
        assert_eq!(ThemePreset::Dark.next(), ThemePreset::Light);
        assert_eq!(ThemePreset::Light.next(), ThemePreset::Monokai);
        assert_eq!(ThemePreset::Monokai.next(), ThemePreset::Tty);
        assert_eq!(ThemePreset::Tty.next(), ThemePreset::Dark);
    }

    #[test]
    fn load_missing_file_returns_dark() {
        let (theme, preset) = load(Some(Path::new("/nonexistent/config.toml")));
        assert_eq!(preset, ThemePreset::Dark);
        // Catppuccin Mocha Red
        assert_eq!(theme.cpu_high, Color::Rgb(243, 139, 168));
    }

    #[test]
    fn config_toml_override() {
        let toml = r#"
theme = "light"
[colors]
cpu_high = "Magenta"
"#;
        let cfg: ConfigFile = toml::from_str(toml).unwrap();
        assert_eq!(cfg.theme, "light");
        assert_eq!(cfg.colors.cpu_high.as_deref(), Some("Magenta"));

        let preset = resolve_preset(&cfg.theme);
        assert_eq!(preset, ThemePreset::Light);
        let theme = apply_overrides(preset.colors(), &cfg.colors);
        assert_eq!(theme.cpu_high, Color::Magenta);
        // Non-overridden fields keep the Light preset value
        assert_eq!(theme.cpu_low, Color::Green);
    }
}
