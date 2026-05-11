# neotop — documentation vault

Open this folder in [Obsidian](https://obsidian.md) to get graph view,
backlinks, and clickable `[[wikilinks]]`. Every note below cross-links
into the others, so you can wander from a high-level question down to a
specific syscall without leaving the vault.

## Entry points

- [[architecture]] — tick loop, data flow, slow-tick cadence
- [[grouping]] — classifier pipeline: Container → VM → Runtime → App → System → Native
- [[status]] — what works on each platform, with ⚠ / ❌ notes
- [[modules]] — one-line summary of every `src/*.rs` file
- [[glossary]] — SMT, NUMA, EMA, tick, band, runtime, etc.

## Platform deep dives

- [[platforms-linux]] — every `/proc` and `/sys` path neotop reads
- [[platforms-macos]] — sysctl / IOKit / Mach / Mach-O map

## Operating notes

- [[controls]] — keybindings, filter grammar
- [[theming]] — TOML schema, preset list, colour fields
- [[performance]] — tick budget, hot paths, why 1 Hz

## Project conduct

- [[contributing]] — branching, CI gate, clippy pitfalls
- [[release-process]] — bump / tag / publish checklist
- [[roadmap]] — what's next

## External

- [`../README.md`](../README.md) — top-level project pitch (lives at repo root)
- [`../CHANGELOG.md`](../CHANGELOG.md) — full release history
- [`../AGENTS.md`](../AGENTS.md) — agent / contributor rules (CI gates, cfg discipline)
