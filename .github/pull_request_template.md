<!--
Thanks for the patch! Keep the body short — the goal is enough
context for the reviewer (the maintainer) to merge fast.
-->

## What this changes

<!-- One paragraph: what behaviour, screen, or metric is different. -->

## Why

<!-- Link to the issue, advisory, or upstream change driving this. -->

## How it was verified

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --all-targets --locked -- -D warnings`
- [ ] `cargo test --all-targets --locked`
- [ ] `cargo build --release --locked`
- [ ] If touching deps: `cargo audit` and `cargo deny check` clean (or new waiver added with rationale in `deny.toml`)
- [ ] Manual smoke (paste the relevant TUI screen / log if behaviour-visible)

## Out of scope / follow-ups

<!--
List anything you noticed but deliberately *didn't* fix. Helps the
reviewer separate "this PR" from "next PR" without re-discovering
the same things later.
-->

---

**By submitting this PR you confirm:** the change is licensed
under Apache-2.0 (matching the repository), and you've read
[`SECURITY.md`](./SECURITY.md) — security-sensitive issues go
through a private advisory, not a public PR.
