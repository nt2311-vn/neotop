# Controls

Single source of truth for the keybindings. Press `?` in neotop for the
same overlay, live.

## Navigation

| Key | Action |
|-----|--------|
| `j` / `↓` | select next row |
| `k` / `↑` | select previous row |
| `PgDn` / `PgUp` | jump 10 rows |
| `Home` / `End` | first / last row |

## View mode

| Key | Action |
|-----|--------|
| `s` | cycle sort: CPU → MEM → PID → CMD |
| `t` | toggle tree view (parent → children) |
| `g` | toggle [[grouping|group view]] (combines with `t` → group-tree) |
| `H` | toggle per-core CPU **spectrum** (sparkline per logical CPU) |
| `T` | cycle [[theming|theme]]: Dark → Light → Monokai → Tty |

## Timing

| Key | Action |
|-----|--------|
| `+` | faster tick (floor 50 ms) |
| `-` | slower tick (ceiling 5 s) |
| `space` | pause / resume |
| `r` | force one refresh now |

## Filter

| Key | Action |
|-----|--------|
| `/` | enter filter mode |
| `Esc` | clear filter |
| `Enter` | confirm (exit mode) |

Filter grammar: simple substring match against `command`. Case-sensitive.

## Actions

| Key | Action |
|-----|--------|
| `K` | send `SIGTERM` to selected pid (with confirm) |
| `Ctrl-K` | send `SIGKILL` to selected pid (with confirm) |
| `?` | toggle keybindings overlay |
| `q` / `Ctrl-C` | quit |

## Combinations

The view toggles compose:

| State | After `t` | After `g` |
|-------|-----------|-----------|
| Flat | Tree | Group |
| Tree | Flat | GroupTree |
| Group | GroupTree | Flat |
| GroupTree | Group | Tree |

The cursor tracks the selected PID across mode changes (not the row
index) so switching between views doesn't jump you to a random
process.

## See also

- [[theming]] — TOML overrides for the colour palette
- [[architecture]] — how the tick loop reacts to keys
