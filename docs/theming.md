# Theming

Theme config lives at `~/.config/neotop/config.toml` (or pass
`--config <path>`). All fields optional — missing ones inherit from the
named preset.

## Presets

| Name | Intended terminal | Style |
|------|-------------------|-------|
| `dark` *(default)* | true-colour, dark background | Catppuccin Mocha |
| `light` | true-colour, light background | soft pastel on off-white |
| `monokai` | true-colour, warm dark | Monokai-inspired |
| `tty` | 16-colour ANSI | no RGB, maximum portability |

Cycle presets live with `T`.

## TOML schema

```toml
theme = "dark"

[colors]
# Any field below can be a hex RGB, decimal RGB, 256-colour index, or
# ratatui named colour. First matching parse wins.

cpu_idle    = "#6c7086"     # hex
cpu_low     = "148,226,213" # "r,g,b"
cpu_mid     = "i228"        # 256-colour index (0..=255)
cpu_high    = "Red"         # ratatui named colour
```

## Colour fields

See `src/theme.rs` for the canonical list. Grouped by role:

### CPU load ramp
- `cpu_idle` / `cpu_low` / `cpu_mid` / `cpu_high`

### UI chrome
- `label` / `border` / `badge_fg` / `badge_bg` / `highlight_bg` / `filter_bg`

### Sparklines
- `spark_cpu` / `spark_mem` / `spark_net_down` / `spark_net_up` / `spark_gpu` / `spark_vram`

### Memory bar segments
- `mem_used` / `mem_buffers` / `mem_cached` / `mem_free` / `gauge_empty`

### Process state
- `proc_r` (running) / `proc_d` (disk-wait) / `proc_z` (zombie) / `proc_t` (stopped) / `proc_i` (idle) / `proc_other`

### [[grouping|Group bands]]
- `group_container` / `group_vm` / `group_runtime` / `group_app` *(new in v0.28)* / `group_system` / `group_native`

### Battery
- `battery_good` / `battery_mid` / `battery_low`

### Swap
- `swap_low` / `swap_mid` / `swap_high`

### Perf
- `perf_ok` / `perf_mid` / `perf_slow`

### Errors
- `err_info_bg` / `err_warn_bg`

### GPU
- `gpu_name`

## Colour value formats

| Form | Example | Notes |
|------|---------|-------|
| Hex RGB | `"#f38ba8"` | 6 or 3 hex digits |
| Decimal | `"243,139,168"` | three 0..=255 values |
| 256-colour | `"i228"` | prefix `i` + index 0..=255 |
| Named | `"LightRed"` | ratatui's `Color::*` enum: Black, Red, Green, Yellow, Blue, Magenta, Cyan, Gray, DarkGray, LightRed, LightGreen, LightYellow, LightBlue, LightMagenta, LightCyan, White |

Invalid values are logged to stderr (`neotop: ignoring invalid colour
'...' for theme.<field>`) and the preset default is kept.

## See also

- [[modules|theme.rs]] — source
- [[controls]] — `T` key binding for live cycling
