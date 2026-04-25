# neotop — task runner
#
# `just` is a command runner: https://github.com/casey/just
#   install:  cargo install just   |  mise use -g just
#
# Type `just` (no args) to see every recipe with a one-line summary.
# This file is the single source of truth for "how do I do X on neotop?".
# Recipes mirror `.github/workflows/ci.yml` so passing `just check`
# locally guarantees CI will pass too — no surprises on push.

# Use bash explicitly. The repo's user runs fish, but recipes that chain
# commands need the bash idioms (`&&`, `||`, `$(...)`).
set shell := ["bash", "-cu"]

# Don't auto-load .env files: neotop has no secrets, and surprises here
# would only confuse newcomers.
set dotenv-load := false

# Path to the optimized binary produced by `cargo build --release`.
bin := "target/release/neotop"


# === Default & onboarding ====================================================

# List every recipe with its summary (this runs when you type just `just`).
default:
    @just --list --unsorted

# Print a curated cheat sheet for new contributors.
help:
    @echo "neotop · common commands"
    @echo ""
    @echo "  just setup           Verify your toolchain has everything we need."
    @echo "  just run             Build (debug) and launch neotop."
    @echo "  just dev             Auto-rebuild + re-run on save (needs cargo-watch)."
    @echo "  just test            Run all unit tests."
    @echo "  just check           Mirror CI: fmt --check, clippy -D warnings, test."
    @echo "  just fix             Auto-apply rustfmt + clippy autofixes."
    @echo "  just release         Optimized build at {{bin}}."
    @echo "  just install         Install neotop into ~/.cargo/bin."
    @echo "  just doc             Open the rustdoc browser."
    @echo "  just clean           Remove the target/ directory."
    @echo ""
    @echo "Release flow (maintainer-only):"
    @echo "  just release-prep    Run all checks the CI workflow runs."
    @echo "  just tag VERSION     Create an annotated git tag for VERSION."
    @echo ""
    @echo "Run 'just' alone to see every recipe."

# Verify the local toolchain has everything CI uses.
setup:
    @echo "Checking toolchain..."
    @command -v rustc >/dev/null || { echo "  ✗ rustc not found — install via https://rustup.rs/"; exit 1; }
    @command -v cargo >/dev/null || { echo "  ✗ cargo not found"; exit 1; }
    @rustc --version
    @cargo --version
    @cargo fmt --version >/dev/null 2>&1 || { echo "  ✗ rustfmt missing — run: rustup component add rustfmt"; exit 1; }
    @cargo clippy --version >/dev/null 2>&1 || { echo "  ✗ clippy missing — run: rustup component add clippy"; exit 1; }
    @echo ""
    @echo "Optional but recommended:"
    @command -v cargo-watch >/dev/null && echo "  ✓ cargo-watch ($(cargo watch --version))" \
        || echo "  · cargo-watch (for 'just dev'):  cargo install cargo-watch"
    @echo ""
    @echo "All required components present. Try 'just run'."


# === Build & run =============================================================

# Build (debug) and run neotop. Forwards extra args, e.g. `just run -- --refresh-ms 100`.
run *ARGS:
    cargo run -- {{ARGS}}

# Auto-rebuild and re-run on every save (requires cargo-watch).
dev *ARGS:
    cargo watch -x 'run -- {{ARGS}}'

# Optimized release build, then print the binary's size.
release:
    cargo build --release --locked
    @ls -lh {{bin}}

# Run the release build directly (no rebuild if up to date).
run-release *ARGS:
    cargo run --release -- {{ARGS}}

# Install neotop into ~/.cargo/bin (then `neotop` is on your PATH).
install:
    cargo install --path . --locked

# Print the on-disk size of the release binary plus its `file` info.
size: release
    @echo ""
    @ls -lh {{bin}}
    @file {{bin}} 2>/dev/null || true


# === Test, lint, format ======================================================

# Run all unit + doc tests.
test:
    cargo test --all-targets --locked

# Run a single test by substring. Example: `just test-one ema_blend`.
test-one PATTERN:
    cargo test --all-targets --locked {{PATTERN}}

# Run only the `#[ignore]`d tests (slow, hardware-dependent benches).
test-bench:
    cargo test --release -- --ignored --nocapture

# Format every Rust file in place.
fmt:
    cargo fmt --all

# Verify formatting without writing anything (same flag CI uses).
fmt-check:
    cargo fmt --all --check

# Run clippy with `-D warnings` (matches CI's denial level).
lint:
    cargo clippy --all-targets --locked -- -D warnings

# Auto-apply rustfmt + safe clippy fixes. Always run `just check` after.
fix:
    cargo fmt --all
    cargo clippy --all-targets --fix --allow-dirty --allow-staged -- -D warnings

# Mirror the full CI pipeline locally — run before every push.
check: fmt-check lint test
    @echo ""
    @echo "✓ all CI checks passed locally"

# Like `check` but also builds the release binary.
ci: check release


# === Documentation ===========================================================

# Build rustdoc and open it in your browser.
doc:
    cargo doc --no-deps --open

# Build rustdoc without opening (useful over SSH).
doc-build:
    cargo doc --no-deps


# === Hardware benchmarks =====================================================

# Print how long each /sys/class/hwmon device takes to scan (the v0.6.0 acpitz bench).
bench-hwmon:
    @for h in /sys/class/hwmon/hwmon*; do \
        name=$(cat "$h/name" 2>/dev/null || echo unknown); \
        printf '%-50s ' "$h ($name):"; \
        t0=$(date +%s%N); \
        for f in "$h"/temp*_input; do [ -f "$f" ] && cat "$f" >/dev/null 2>&1; done; \
        t1=$(date +%s%N); \
        echo "$(( (t1-t0)/1000000 )) ms"; \
    done

# Time how long it takes to `cat` /proc/<pid>/stat for every live pid.
bench-procs:
    @echo "Pid count:"
    @ls /proc | grep -cE '^[0-9]+$'
    @echo ""
    @echo "Timing one full sweep (rough upper bound on a slow tick):"
    @time bash -c 'for pid in $(ls /proc | grep -E "^[0-9]+\$"); do cat /proc/$$pid/stat >/dev/null 2>&1; done'


# === Release flow (maintainer only) ==========================================

# Verify the tree is releasable: clean checkout, all CI checks pass.
release-prep:
    @echo "Checking working tree is clean..."
    @git diff --quiet --exit-code || { echo "  ✗ uncommitted changes"; exit 1; }
    @git diff --cached --quiet --exit-code || { echo "  ✗ staged but uncommitted changes"; exit 1; }
    @echo "Running CI pipeline locally..."
    @just check
    @just release
    @echo ""
    @echo "Cargo.toml version: $(grep -m1 '^version' Cargo.toml | cut -d'\"' -f2)"
    @echo "Latest CHANGELOG header:"
    @grep -m1 '^## \[' CHANGELOG.md
    @echo ""
    @echo "If those two match, run: just tag <VERSION>"

# Create an annotated git tag for VERSION (no `v` prefix). Example: `just tag 0.7.0`.
tag VERSION:
    @echo "Verifying Cargo.toml version is {{VERSION}}..."
    @grep -q '^version = "{{VERSION}}"' Cargo.toml || { echo "  ✗ Cargo.toml version != {{VERSION}}"; exit 1; }
    @echo "Verifying CHANGELOG.md has a [{{VERSION}}] heading..."
    @grep -q '^## \[{{VERSION}}\]' CHANGELOG.md || { echo "  ✗ no '## [{{VERSION}}]' section in CHANGELOG.md"; exit 1; }
    @echo "Creating annotated tag v{{VERSION}}..."
    git tag -a v{{VERSION}} -m "neotop {{VERSION}}"
    @echo ""
    @echo "Tag created locally. Push with: git push origin main --tags"

# Print the 20 most recent release tags (newest first).
tags:
    @git tag --list --sort=-version:refname | head -20

# Show every commit since the last tag (helpful when drafting the changelog).
since-last-tag:
    @last=$(git describe --tags --abbrev=0 2>/dev/null) || last=""; \
    if [ -z "$last" ]; then \
        echo "(no tags yet — showing all commits)"; \
        git log --oneline; \
    else \
        echo "Commits since $last:"; \
        git log --oneline "$last"..HEAD; \
    fi


# === Maintenance =============================================================

# Remove the target/ directory (reclaims ~1-2 GB; next build is from scratch).
clean:
    cargo clean

# Show outdated dependencies (requires cargo-outdated). Read-only.
outdated:
    cargo outdated --root-deps-only || echo "(install: cargo install cargo-outdated)"

# Audit dependencies for known security advisories (requires cargo-audit).
audit:
    cargo audit || echo "(install: cargo install cargo-audit)"

# Print every TODO / FIXME / XXX / HACK comment in the source tree.
todo:
    @grep -rn --color=always -E '\b(TODO|FIXME|XXX|HACK)\b' src/ || echo "(none)"
