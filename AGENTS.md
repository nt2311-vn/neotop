# AGENTS.md — neotop project rules for AI agents

## Post-coding verification checklist

**Every code change must pass ALL of these before committing.** These mirror `.github/workflows/ci.yml` exactly — passing locally guarantees CI will pass.

### 1. Format check

```sh
cargo fmt --all --check
```

If it fails, run `cargo fmt --all` and re-stage.

### 2. Clippy — native target (Linux)

```sh
cargo clippy --all-targets --locked -- -D warnings
```

`-D warnings` promotes every warning to an error. Zero warnings or the commit is rejected.

### 3. Clippy — cross-compile macOS target

```sh
rustup target add aarch64-apple-darwin   # once
cargo clippy --target aarch64-apple-darwin --locked -- -D warnings
```

This catches macOS-only type mismatches, missing `cfg` gates, and FFI signature errors that the Linux build cannot see. **Do not skip this** — it is the #1 source of CI-only failures.

### 4. Tests

```sh
cargo test --all-targets --locked
```

All tests must pass. Ignored tests (gated behind `#[ignore = "requires real macOS hardware"]` etc.) are fine.

### 5. Build check — cross-compile macOS

```sh
cargo check --target aarch64-apple-darwin --locked
```

Separate from clippy — catches bare compilation errors faster.

### 6. Release build (optional but recommended before tagging)

```sh
cargo build --release --locked
```

Catches link-time issues and verifies the optimized binary links.

---

## Common clippy pitfalls in this project

These are the lints that most frequently bite new code in this repo. Fix them proactively:

| Lint | Pattern | Fix |
|------|---------|-----|
| `derivable_impls` | Manual `Default` impl that matches derive | Use `#[derive(Default)]` |
| `needless_range_loop` | `for i in 0..len { v[i] }` | Use `for (i, x) in v.iter_mut().enumerate()` or `for &x in v.iter().take(n)` |
| `manual_checked_ops` | `if x > 0 { a / x } else { y }` | Use `a.checked_div(x).unwrap_or(y)` |
| `items_after_test_module` | Code after `#[cfg(test)] mod tests` | Move code above the test module |
| `cast_precision_loss` / `cast_sign_loss` / `cast_possible_truncation` | `x as f64`, `x as u64` | Add `#[allow(clippy::cast_*)]` with a comment, or use `TryFrom` |
| `module_name_repetitions` | `fn foo_bar()` in `mod bar` | Allowed globally in `Cargo.toml` lints |

---

## Platform-specific code rules

### `#[cfg]` gates

- Every macOS-only module gets `#[cfg(target_os = "macos")]` on the `mod` declaration in `main.rs`.
- Every Linux-only function gets `#[cfg(target_os = "linux")]`.
- Use `#[cfg(not(any(target_os = "linux", target_os = "macos")))]` for the "other platforms" fallback — never bare `#[cfg(not(target_os = "linux"))]` which silently swallows macOS.
- Cross-platform functions that call into platform modules must have one `#[cfg]`-gated impl per platform.

### Visibility

- Structs and fields shared between `foo.rs` and `foo_macos.rs` must be `pub(crate)`.
- The `Sample` structs in `disk.rs` and `net.rs` are `pub(crate)` because `disk_macos.rs` and `net_macos.rs` construct them.
- The `Tracker.prev` fields are `pub(crate)` for the same reason.

### FFI / unsafe

- Every `unsafe` block must have a `SAFETY:` comment explaining why the call is sound.
- macOS FFI uses `libc` crate bindings (`proc_pidinfo`, `proc_listallpids`, `sysctl`, `sysctlbyname`).
- IOKit FFI uses `io-kit-sys` + `core-foundation` crates.
- **Never use `libc::kinfo_proc`** — it is not available when cross-compiling from Linux. Use `proc_pidinfo(PROC_PIDTBSDINFO)` instead.
- `libc::PROC_PIDPATHINFO_MAXSIZE` is `i32` — cast to `usize`/`u32` as needed for array sizing and function args.

---

## Commit conventions

- Use conventional-commit prefixes: `feat:`, `fix:`, `chore:`, `docs:`, `refactor:`.
- Scope platform: `feat(macos):`, `fix(linux):`.
- Keep subject line ≤50 chars.
- Body explains *why*, not *what*.
- Always include the Devin co-author trailer.

---

## Quick reference — just recipes

```sh
just check      # mirror CI: fmt --check + clippy -D warnings + test
just fix        # auto-apply rustfmt + clippy autofixes
just test       # cargo test --all-targets --locked
just release    # cargo build --release --locked
```
