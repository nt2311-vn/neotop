# Obsidian vault config

Open the neotop repo folder directly in [Obsidian](https://obsidian.md):
`File → Open Vault → Open folder as vault → pick repo root`.

The notes live in [`../docs/`](../docs/). Entry point is
[`../docs/index.md`](../docs/index.md).

## What's enabled

- Graph view — colour-grouped by topic (architecture, platforms, grouping, status).
- Backlinks pane on the right.
- Outgoing-link pane on the right.
- Outline on the right.
- `[[wikilink]]` link style by default (no Markdown `[text](path)` noise).
- New notes land in `docs/` automatically.
- Mermaid renders natively — all the charts in the docs just work.

## Why this is committed

Agents and human contributors both benefit from a consistent vault
experience. Committing `.obsidian/` is the Obsidian-recommended approach
for shared vaults (see the [Obsidian docs](https://help.obsidian.md/Files+and+folders/Manage+vaults#Shared+vault)).
It's a small JSON footprint.

Per-user session state (workspace layout, recent files) is kept
minimal so divergent local edits don't cause merge noise.

## What's **not** committed

- No `.obsidian/plugins/` community plugins — keep the vault dependency-free.
- No `.obsidian/themes/` — the default Obsidian theme is good enough.
- No `.obsidian/canvas/` — we don't use Canvas here.
