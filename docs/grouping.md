# Process grouping

The `g` toggle aggregates the flat process list into bands. Every
surviving row ends up in a named group; the only silent aggregate is the
System band (launchd / kernel daemons would drown real workloads).

See [[modules|groups.rs]] for the source and [[glossary|band]] for the
terminology.

## Pipeline

```mermaid
flowchart TD
  start([cmdline + exe]) --> container{"in a container?<br/>Linux: cgroup path<br/>macOS: runtime ancestor"}
  container -- yes --> bContainer["[[glossary#Band|Container]]<br/>docker:abc123"]
  container -- no --> hv{"hypervisor?<br/>qemu-system-* / firecracker /<br/>cloud-hypervisor / crosvm / lkvm"}
  hv -- yes --> bVm["[[glossary#Band|VM]]<br/>qemu:ubuntu-dev"]
  hv -- no --> lang{"known interpreter?<br/>java / node / python /<br/>ruby / php / bun / deno / ..."}
  lang -- yes --> bRuntime["[[glossary#Band|Runtime]] lang:app<br/>java:server.jar"]
  lang -- no --> app{".app bundle?<br/>outer *.app/ segment<br/>(macOS only)"}
  app -- yes --> bApp["[[glossary#Band|App]]<br/>app:Xcode"]
  app -- no --> sys{"system daemon?<br/>PID 1 / kthread /<br/>systemd / launchd /<br/>dbus-* / udev / sshd"}
  sys -- yes --> bSystem["[[glossary#Band|System]]<br/>aggregated, no header"]
  sys -- no --> mach{"Mach-O / ELF scan<br/>library/std/src/<br/>go.buildid / _RNv /<br/>.gopclntab"}
  mach -- Rust --> bRust["[[glossary#Band|Runtime]]<br/>rust:neotop"]
  mach -- Go --> bGo["[[glossary#Band|Runtime]]<br/>go:caddy"]
  mach -- neither --> bNative["[[glossary#Band|Native]]<br/>native:&lt;basename&gt;"]
```

First match wins. The order is intentional: container beats hypervisor
beats runtime beats bundle beats system beats catch-all.

## Bands and visuals

| Band | Sort key prefix | Header shown? | Theme field |
|------|-----------------|---------------|-------------|
| Container | `0_` | ✅ | `group_container` |
| VM | `1_` | ✅ | `group_vm` |
| Runtime | `2_` | ✅ | `group_runtime` |
| App *(macOS)* | `3_` | ✅ | `group_app` |
| System | `4_` | ❌ (aggregated silently) | `group_system` |
| Native | `5_` | ✅ | `group_native` |

When the user sorts by CPU or MEM, the busiest group bubbles to the top
regardless of band priority. When sorting by PID or Command, band order
is restored (Container first, Native last).

## Why each band exists

- **Container** — "which of my containers is hot?" Flat lists bury this
  because each container launches 3–15 sibling processes whose names
  repeat across images. Clustering by runtime + short ID turns a wall of
  `nginx` / `postgres` / `redis` into "this Docker container is using
  72 % CPU".
- **VM** — same question for hypervisors. Each QEMU process exposes
  `-name`, `-smp`, `-m` via its cmdline; we parse those into
  `qemu:ubuntu-dev (4 vCPU, 8 GiB)`.
- **Runtime** — every developer laptop has 30 Node processes and 5 Java
  services. Clustering by `lang:app` (`node:server.js`, `java:app.jar`)
  tells you "the JVM eating CPU is `app.jar`, not `gradle`". The `[sig]`
  tag (`[event loop]`, `[vthreads]`, `[goroutines]`) is a concurrency-
  model hint so the thread count reads correctly.
- **App** *(macOS only)* — Electron / Chromium / Xcode spawn 10–30
  helper processes per window. Without this band they'd spread across
  many `native:*` rows. Clustering by the outermost `.app/` in the
  executable path collapses all those helpers under `app:Google Chrome`
  / `app:Visual Studio Code` / `app:Xcode`.
- **System** — PID 1, kernel threads, launchd, systemd, dbus-*. Real
  workloads aren't here and a header sum would always be the largest
  row in the table, drowning useful signal. Members render without a
  banner.
- **Native** *(now per-basename)* — anything that survived the pipeline.
  Previously one giant "native" bucket; now grouped by argv[0]
  basename, so `native:sshd (3)`, `native:fish (2)`, `native:mdworker
  (5)`, etc. Each distinct binary gets its own header.

## Interpreter vs compiled runtimes

Scripted runtimes are detected by `classify_lang` via argv[0] basename
match against a fixed table. Their `app` field is parsed from the rest
of the argv:

| Interpreter | Strategy |
|-------------|----------|
| Java | `-jar foo.jar` → `foo.jar`, else last non-flag (main class) |
| Python | `-m pkg.mod` → `pkg.mod`, `-c` → `(inline)`, else first script |
| Node / Bun / Deno | first non-flag after interpreter |
| Ruby / PHP / Perl / Lua / R / .NET | first non-flag after interpreter |
| Erlang | — (empty, BEAM cmdlines too varied) |

Compiled runtimes (Rust, Go) can't be told apart from any other native
binary by cmdline alone. After `classify_process` returns
`Group::Native(...)`, `procs.rs` makes a second pass:

```mermaid
flowchart LR
  native[Group::Native] --> probe{read exe file}
  probe -->|Linux| elf[ELF64 section scan]
  probe -->|macOS| macho[Mach-O + FAT slice scan]
  elf --> goHit[".note.go.buildid<br/>.gopclntab"]
  elf --> rustHit["library/std/src/<br/>_RNv"]
  macho --> goHit2["go.buildid<br/>runtime.goexit"]
  macho --> rustHit2["library/std/src/<br/>/rustc/"]
  goHit & goHit2 --> upgradeGo[upgrade → Runtime Go, basename]
  rustHit & rustHit2 --> upgradeRust[upgrade → Runtime Rust, basename]
```

This runs once per new PID (result cached in `StaticInfo`) so the I/O
cost is bounded.

## See also

- [[modules|groups.rs]] — enum + classifier
- [[modules|elf.rs]] — ELF / Mach-O scanner
- [[architecture]] — where the classifier sits in the tick
- [[platforms-macos]] — how `.app` bundles are extracted from exe path
