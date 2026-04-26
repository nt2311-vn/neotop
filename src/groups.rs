//! groups.rs — classify host processes into developer-meaningful groups.
//!
//! `htop` / `btop` / `btm` show every PID undifferentiated. On a developer
//! laptop with 30 Node processes, 5 Java services, and a handful of
//! Podman containers running their own init trees, that's a wall of text
//! that buries the signal. This module classifies each row into one of:
//!
//! * **Container** (Docker / Podman / Kubernetes / Containerd / LXC) —
//!   by inspecting `/proc/<pid>/cgroup` for runtime-specific path
//!   patterns. Carries a short 12-char hash so the user can `docker
//!   ps` / `podman ps` it to recover the human name.
//! * **Runtime** (Java / Node / Bun / Deno / Python / Ruby / PHP /
//!   Perl / Lua / Erlang / .NET / R) — by inspecting the binary
//!   name from `/proc/<pid>/cmdline`. Static-link languages
//!   (Go, Rust, C, C++) fall through to **Native** because we'd
//!   need ELF symbol parsing to detect them and that's not worth
//!   the per-tick I/O.
//! * **System** — PID 1, kernel threads (cmdline rendered as
//!   `[name]`), and the usual systemd / dbus / udev daemons.
//! * **Native** — everything else.
//!
//! Container detection wins over runtime detection: a `node` running
//! inside `docker run myapp` is more usefully grouped with the
//! container than lumped in with all other Node processes on the
//! host.
//!
//! Cost: classification is a pure function of the cmdline + cgroup
//! path strings, both of which are read once and cached in
//! `procs::StaticInfo`. Steady-state CPU cost is essentially zero.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Lang {
    Java,
    Node,
    Bun,
    Deno,
    Python,
    Ruby,
    Php,
    Perl,
    Lua,
    Erlang,
    DotNet,
    R,
    /// Go — detected by ELF section parsing of `/proc/<pid>/exe`
    /// (cmdline alone is useless because `go build` produces a
    /// statically-linked binary named after the target).
    Go,
    /// Rust — same story as Go, detected by ELF inspection of the
    /// executable's read-only data sections.
    Rust,
}

impl Lang {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Java => "java",
            Self::Node => "node",
            Self::Bun => "bun",
            Self::Deno => "deno",
            Self::Python => "python",
            Self::Ruby => "ruby",
            Self::Php => "php",
            Self::Perl => "perl",
            Self::Lua => "lua",
            Self::Erlang => "erlang",
            Self::DotNet => "dotnet",
            Self::R => "r",
            Self::Go => "go",
            Self::Rust => "rust",
        }
    }

    /// One-token concurrency-model tag rendered alongside the group
    /// label so the user can tell at a glance *how* a runtime spends
    /// its time. These are the model the runtime exposes to user
    /// code — what shows up when you read documentation, not the
    /// implementation detail underneath. The strings stay short
    /// because they share row-budget with the count + totals.
    pub(crate) fn signature(self) -> &'static str {
        match self {
            // Goroutines are the headline concurrency primitive of
            // Go — M:N scheduled by the runtime onto OS threads.
            Self::Go => "goroutines",
            // Rust has no built-in runtime; the user picks one.
            // tokio is overwhelmingly dominant for async, std::thread
            // for synchronous code. Lump them as async/threads.
            Self::Rust => "async/threads",
            // Project Loom virtual threads (1:N green threads on top
            // of platform threads) are the modern face of JVM
            // concurrency since JDK 21.
            Self::Java => "vthreads",
            // libuv event loop, single-threaded JS execution.
            Self::Node | Self::Bun | Self::Deno => "event loop",
            // The GIL serialises Python bytecode, but `asyncio`
            // gives the runtime its async story.
            Self::Python => "GIL+asyncio",
            // Ruby's Fibers / async gem layered on a GIL-equivalent.
            Self::Ruby => "GVL+fibers",
            // PHP-FPM is process-per-request; no shared concurrency.
            Self::Php => "process pool",
            Self::Perl => "threads",
            Self::Lua => "coroutines",
            // BEAM scheduler runs lightweight processes (actors)
            // across one scheduler thread per core.
            Self::Erlang => "actors/BEAM",
            // .NET's TPL = task scheduler over a thread pool.
            Self::DotNet => "TPL",
            Self::R => "single-thread",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ContainerRuntime {
    Docker,
    Podman,
    Kubernetes,
    Containerd,
    Lxc,
}

impl ContainerRuntime {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
            Self::Kubernetes => "k8s",
            Self::Containerd => "containerd",
            Self::Lxc => "lxc",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Container {
    pub(crate) runtime: ContainerRuntime,
    /// 12-char short hash (or human name for LXC where the cgroup
    /// path stores the container name verbatim). Same length the
    /// docker/podman CLIs use when listing.
    pub(crate) id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Group {
    Container(Container),
    Vm(crate::vm::VmInfo),
    /// A language runtime *plus* the app running in it. Splitting on
    /// the app name keeps separate Rust binaries (or distinct Java
    /// jars / Node scripts) from collapsing into one giant pile —
    /// which would otherwise dwarf every real workload the same way
    /// the old `native` aggregate did. Empty `app` is allowed for
    /// runtimes whose cmdline doesn't expose a script (e.g. a bare
    /// `python3` REPL); those still cluster as a single group.
    Runtime(Lang, String),
    System,
    Native,
}

impl Group {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Container(c) => format!("{}:{}", c.runtime.label(), c.id),
            Self::Vm(v) => v.label(),
            // Runtime label carries both the concrete app name and
            // the concurrency-model hint — `rust:neotop
            // [async/threads]`, `java:app.jar [vthreads]`,
            // `python:server.py [GIL+asyncio]`. App empty → just
            // `lang [sig]` so we still distinguish bands.
            Self::Runtime(l, app) => {
                if app.is_empty() {
                    format!("{} [{}]", l.label(), l.signature())
                } else {
                    format!("{}:{} [{}]", l.label(), app, l.signature())
                }
            }
            Self::System => "system".into(),
            Self::Native => "native".into(),
        }
    }

    /// Stable order: container > vm > runtime > system > native.
    /// Inside the runtime band, sub-key on app name so each app gets
    /// its own bucket — the bug fix for "all Rust processes summed
    /// into one row" that used to push the runtime band to the top
    /// of CPU-sorted listings regardless of which app was actually
    /// busy.
    pub(crate) fn sort_key(&self) -> String {
        match self {
            Self::Container(c) => format!("0_{}_{}", c.runtime.label(), c.id),
            Self::Vm(v) => format!("1_{}_{}", v.hypervisor.label(), v.name),
            Self::Runtime(l, app) => format!("2_{}_{}", l.label(), app),
            Self::System => "3_system".into(),
            Self::Native => "4_native".into(),
        }
    }

    pub(crate) fn band(&self) -> GroupBand {
        match self {
            Self::Container(_) => GroupBand::Container,
            Self::Vm(_) => GroupBand::Vm,
            Self::Runtime(_, _) => GroupBand::Runtime,
            Self::System => GroupBand::System,
            Self::Native => GroupBand::Native,
        }
    }

    /// Same as `label()`, but resolves container short-IDs to names.
    pub(crate) fn label_with_names(&self, names: &ContainerNames) -> String {
        match self {
            Self::Container(c) => match names.lookup(&c.id) {
                Some(name) => format!("{}:{}", c.runtime.label(), name),
                None => format!("{}:{}", c.runtime.label(), c.id),
            },
            _ => self.label(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum GroupBand {
    Container,
    Vm,
    Runtime,
    System,
    #[default]
    Native,
}

/// Top-level classifier: container > vm > runtime > system > native.
pub(crate) fn classify_process(cmdline: &str, cgroup: Option<&str>) -> Group {
    if let Some(c) = cgroup.and_then(parse_container_cgroup) {
        return Group::Container(c);
    }
    if let Some(vm) = crate::vm::detect(cmdline) {
        return Group::Vm(vm);
    }
    if let Some(lang) = classify_lang(cmdline) {
        let app = extract_app(cmdline, lang);
        return Group::Runtime(lang, app);
    }
    if is_system(cmdline) {
        return Group::System;
    }
    Group::Native
}

/// Public so `procs::Tracker` can call it after an ELF-based
/// upgrade to `Lang::Go` or `Lang::Rust`. Returns the basename of
/// the executable, which for compiled languages *is* the app
/// identifier; for interpreted languages this is rarely the right
/// answer (you'd get "java" or "python3"), but procs.rs only calls
/// it on the ELF-detected path so that's fine.
pub(crate) fn argv0_basename_or_empty(cmdline: &str) -> String {
    argv0_basename(cmdline).unwrap_or_default()
}

/// Pull the app identifier out of a runtime cmdline. The strategy
/// depends on the runtime:
///
/// * **Compiled** (Go, Rust): the executable basename — the binary
///   *is* the app.
/// * **Java**: `-jar foo.jar` → `foo.jar`; otherwise the first
///   non-flag token after `java` (typically the main class).
/// * **Python**: `-m foo.bar` → `foo.bar`; otherwise the first
///   non-flag token after `python` (the script path's basename).
/// * **Node / Bun / Deno / Ruby / PHP / Perl / Lua / R / .NET**:
///   first non-flag token after the interpreter — almost always
///   the script path. Take the basename so `node /opt/srv/idx.js`
///   and `node ./idx.js` cluster as `idx.js`.
/// * **Erlang**: empty — `beam.smp` cmdlines are too varied to
///   parse reliably; processes still cluster as `erlang [actors]`.
///
/// Empty result is allowed and means "couldn't identify a script";
/// the caller renders a single-language group instead of per-app.
pub(crate) fn extract_app(cmdline: &str, lang: Lang) -> String {
    match lang {
        Lang::Go | Lang::Rust => argv0_basename(cmdline).unwrap_or_default(),
        Lang::Java => extract_java_app(cmdline),
        Lang::Python => extract_python_app(cmdline),
        Lang::Erlang => String::new(),
        // Generic interpreters: skip flags, take basename of the
        // first script-shaped argument.
        Lang::Node
        | Lang::Bun
        | Lang::Deno
        | Lang::Ruby
        | Lang::Php
        | Lang::Perl
        | Lang::Lua
        | Lang::DotNet
        | Lang::R => extract_first_script(cmdline),
    }
}

/// Walk argv tokens, skip the first (interpreter), then take the
/// basename of the first token that doesn't look like a CLI flag.
/// Returns the empty string when nothing qualifies.
fn extract_first_script(cmdline: &str) -> String {
    let mut tokens = cmdline.split_whitespace();
    tokens.next(); // interpreter
    for tok in tokens {
        if tok.starts_with('-') {
            continue;
        }
        return basename(tok);
    }
    String::new()
}

/// Java: prefer the `-jar foo.jar` form; if absent, fall back to
/// the last non-flag token (the main class). `java -cp x:y
/// com.example.Main` → `com.example.Main`. Skips `-cp` /
/// `--class-path` value pairs so they don't shadow the real entry
/// point.
fn extract_java_app(cmdline: &str) -> String {
    let toks: Vec<&str> = cmdline.split_whitespace().collect();
    let mut i = 1; // skip interpreter
    let mut last_non_flag: Option<&str> = None;
    while i < toks.len() {
        let t = toks[i];
        if t == "-jar" {
            if let Some(jar) = toks.get(i + 1) {
                return basename(jar);
            }
            i += 1;
        } else if t == "-cp" || t == "-classpath" || t == "--class-path" {
            // Eat the value too; classpaths are colon-separated
            // and would otherwise be picked up as "the script".
            i += 2;
            continue;
        } else if !t.starts_with('-') {
            last_non_flag = Some(t);
        }
        i += 1;
    }
    last_non_flag.map(basename).unwrap_or_default()
}

/// Python: handle `-m module.path` (run a module) and `-c "code"`
/// (inline). Otherwise the first non-flag token is the script.
fn extract_python_app(cmdline: &str) -> String {
    let toks: Vec<&str> = cmdline.split_whitespace().collect();
    let mut i = 1;
    while i < toks.len() {
        let t = toks[i];
        if t == "-m" {
            return toks
                .get(i + 1)
                .map(|s| (*s).to_string())
                .unwrap_or_default();
        }
        if t == "-c" {
            return "(inline)".into();
        }
        if !t.starts_with('-') {
            return basename(t);
        }
        i += 1;
    }
    String::new()
}

/// Trim a path down to its file name. Pure utility so the per-lang
/// extractors don't each spell it out.
fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |s| s.to_string_lossy().to_string())
}

/// Detect a language runtime from the binary name in cmdline. Cheap:
/// extracts `argv0` and matches against a static table. Returns
/// `None` for unrecognised binaries — caller decides whether they're
/// system or native.
pub(crate) fn classify_lang(cmdline: &str) -> Option<Lang> {
    let bin = argv0_basename(cmdline)?;
    Some(match bin.as_str() {
        "java" | "javaw" => Lang::Java,
        "node" | "nodejs" => Lang::Node,
        "bun" => Lang::Bun,
        "deno" => Lang::Deno,
        "python" | "python2" | "python3" => Lang::Python,
        s if s.starts_with("python3.") || s.starts_with("python2.") => Lang::Python,
        "ruby" | "rubyw" | "irb" | "bundle" | "rake" => Lang::Ruby,
        "php" | "php-fpm" | "php-cgi" => Lang::Php,
        "perl" => Lang::Perl,
        "lua" | "luajit" => Lang::Lua,
        "beam.smp" | "erl" | "erlexec" => Lang::Erlang,
        "dotnet" => Lang::DotNet,
        "R" | "Rscript" => Lang::R,
        _ => return None,
    })
}

/// Recognise PID 1, kernel threads, and the canonical systemd / dbus
/// / udev daemons so they cluster as `system` rather than landing in
/// the catch-all `native` bucket.
fn is_system(cmdline: &str) -> bool {
    // Kernel threads: procs.rs renders their cmdline as `[name]` when
    // /proc/<pid>/cmdline is empty.
    let trimmed = cmdline.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        return true;
    }
    let Some(bin) = argv0_basename(cmdline) else {
        return false;
    };
    matches!(
        bin.as_str(),
        "systemd"
            | "init"
            | "dbus-daemon"
            | "dbus-broker"
            | "dbus-broker-launch"
            | "udevd"
            | "systemd-udevd"
            | "systemd-journald"
            | "systemd-logind"
            | "systemd-resolved"
            | "systemd-networkd"
            | "systemd-timesyncd"
            | "rsyslogd"
            | "syslog-ng"
            | "cron"
            | "crond"
            | "agetty"
            | "login"
            | "sshd"
    )
}

/// Extract the file-name component of `argv0`. Handles cmdlines
/// that arrive null-separated (the raw `/proc/<pid>/cmdline` form)
/// or space-separated (the cleaned-up form `procs.rs` produces).
///
/// Some daemons rewrite their `argv[0]` to advertise role status —
/// e.g. PHP-FPM renames the master process to `php-fpm: master
/// process (...)` and Postgres renames its workers to `postgres:
/// 13/main: walwriter`. Trim the trailing `:` so the binary name
/// matches the lookup table.
fn argv0_basename(cmdline: &str) -> Option<String> {
    let argv0 = cmdline
        .split('\0')
        .next()
        .unwrap_or(cmdline)
        .split_whitespace()
        .next()?;
    let argv0 = argv0
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches(':');
    if argv0.is_empty() {
        return None;
    }
    Some(
        Path::new(argv0)
            .file_name()
            .map_or_else(|| argv0.to_string(), |s| s.to_string_lossy().to_string()),
    )
}

/// Parse a `/proc/<pid>/cgroup` line (or full file body) for known
/// container-runtime patterns. Returns the runtime + the short
/// container ID (first 12 chars of the hash, or the LXC name).
///
/// Patterns recognised (cgroup v1 or v2; we don't care which):
///
/// * `/docker/<sha>` — older docker
/// * `/system.slice/docker-<sha>.scope` — modern systemd-managed docker
/// * `/.../libpod-<sha>.scope` — podman
/// * `/.../podman-<sha>.scope` — newer podman variants
/// * `/lxc/<name>` — LXC (the slug after `/lxc/` is the container name)
/// * `/system.slice/containerd.service/.../containerd-<sha>.scope` — containerd
/// * `/kubepods.slice/...` — Kubernetes (priority over the inner runtime)
pub(crate) fn parse_container_cgroup(raw: &str) -> Option<Container> {
    // cgroup v2 lines look like `0::/...`. v1 has many lines —
    // try each in turn since some controllers carry the container
    // path while others don't.
    for line in raw.lines() {
        if let Some(c) = match_one(line) {
            return Some(c);
        }
    }
    match_one(raw)
}

fn match_one(line: &str) -> Option<Container> {
    // cgroup v1 lines: `<n>:<controller>:<path>`. cgroup v2 lines:
    // `0::<path>`. Either way the path is everything after the last
    // colon. `rsplit_once` is the clearest expression of that.
    let path = line.rsplit_once(':').map_or(line, |(_, p)| p);

    // Kubernetes wins: pods *contain* a runtime segment, but we want
    // to surface "k8s" first because that's the abstraction the user
    // is most likely managing through.
    if path.contains("/kubepods.slice/") || path.contains("/kubepods/") {
        for tag in ["crio-", "docker-", "containerd-"] {
            if let Some(id) = scope_id_after(path, tag) {
                return Some(Container {
                    runtime: ContainerRuntime::Kubernetes,
                    id: short(&id),
                });
            }
        }
        return Some(Container {
            runtime: ContainerRuntime::Kubernetes,
            id: "unknown".into(),
        });
    }

    if let Some(id) = scope_id_after(path, "libpod-") {
        return Some(Container {
            runtime: ContainerRuntime::Podman,
            id: short(&id),
        });
    }
    if let Some(id) = scope_id_after(path, "podman-") {
        return Some(Container {
            runtime: ContainerRuntime::Podman,
            id: short(&id),
        });
    }
    if let Some(id) = scope_id_after(path, "docker-") {
        return Some(Container {
            runtime: ContainerRuntime::Docker,
            id: short(&id),
        });
    }
    if let Some(rest) = path.strip_prefix("/docker/") {
        let id = rest.split('/').next().unwrap_or(rest);
        if !id.is_empty() {
            return Some(Container {
                runtime: ContainerRuntime::Docker,
                id: short(id),
            });
        }
    }
    if path.contains("containerd") {
        if let Some(id) = scope_id_after(path, "containerd-") {
            return Some(Container {
                runtime: ContainerRuntime::Containerd,
                id: short(&id),
            });
        }
    }
    if let Some(rest) = path.strip_prefix("/lxc/") {
        let id = rest.split('/').next().unwrap_or(rest);
        if !id.is_empty() {
            // LXC stores the human-readable container name in the
            // cgroup path; keep it verbatim (no hash short-form).
            return Some(Container {
                runtime: ContainerRuntime::Lxc,
                id: id.to_string(),
            });
        }
    }
    None
}

/// Find a `<tag><id>.scope` (or `<tag><id>` with no `.scope` suffix —
/// older docker forms) occurrence in `path` and return `id`. Used by
/// the docker / podman / containerd / Kubernetes parsers.
fn scope_id_after(path: &str, tag: &str) -> Option<String> {
    let start = path.find(tag)? + tag.len();
    let rest = &path[start..];
    let end = rest
        .find(".scope")
        .or_else(|| rest.find('/'))
        .unwrap_or(rest.len());
    let id = &rest[..end];
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Truncate to 12 chars (UTF-8-safe, but container IDs are ASCII).
fn short(id: &str) -> String {
    let n = id.len().min(12);
    id[..n].to_string()
}

// -----------------------------------------------------------------------------
// Container-name resolution
// -----------------------------------------------------------------------------

/// TTL for a `docker ps` / `podman ps` snapshot.
const NAME_CACHE_TTL: Duration = Duration::from_secs(5);

/// Off-thread cache of container ID → name. The runtime CLIs
/// (`docker ps`, `podman ps`) can block for seconds when their
/// daemon is unhealthy; doing that on the UI thread froze the whole
/// app. The worker isolates the shell-out and a 1-second per-call
/// hard timeout caps the worst case even when the daemon is wedged.
pub(crate) struct ContainerNames {
    request_tx: mpsc::Sender<()>,
    result_rx: mpsc::Receiver<HashMap<String, String>>,
    in_flight: bool,
    last_request: Option<Instant>,
    map: HashMap<String, String>,
}

impl Default for ContainerNames {
    fn default() -> Self {
        Self::spawn()
    }
}

impl ContainerNames {
    pub(crate) fn spawn() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<()>();
        let (res_tx, res_rx) = mpsc::channel::<HashMap<String, String>>();
        thread::Builder::new()
            .name("neotop-ctnames".into())
            .spawn(move || {
                while req_rx.recv().is_ok() {
                    while req_rx.try_recv().is_ok() {}
                    let map = poll_runtimes();
                    if res_tx.send(map).is_err() {
                        return;
                    }
                }
            })
            .expect("spawn container-names worker");
        Self {
            request_tx: req_tx,
            result_rx: res_rx,
            in_flight: false,
            last_request: None,
            map: HashMap::new(),
        }
    }

    /// Queue a refresh if the TTL has elapsed and no request is in
    /// flight. Always non-blocking — the worker thread does the
    /// shell-out.
    pub(crate) fn refresh_if_stale(&mut self, now: Instant) {
        let stale = self
            .last_request
            .map_or(true, |t| now.duration_since(t) >= NAME_CACHE_TTL);
        if stale && !self.in_flight && self.request_tx.send(()).is_ok() {
            self.in_flight = true;
            self.last_request = Some(now);
        }
        if let Ok(m) = self.result_rx.try_recv() {
            self.map = m;
            self.in_flight = false;
        }
    }

    /// Resolve a 12-char short hash or full SHA to the human-readable
    /// name. Returns `None` if no daemon polled has reported it.
    pub(crate) fn lookup(&self, id: &str) -> Option<&str> {
        if let Some(name) = self.map.get(id) {
            return Some(name.as_str());
        }
        for (full, name) in &self.map {
            if full.starts_with(id) {
                return Some(name.as_str());
            }
        }
        None
    }
}

// mpsc Sender / Receiver aren't Debug, so we surface only the
// fields a human cares about (cache size + in-flight flag).
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for ContainerNames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContainerNames")
            .field("entries", &self.map.len())
            .field("in_flight", &self.in_flight)
            .finish_non_exhaustive()
    }
}

/// Worker-thread body: shell out to docker / podman with a hard
/// timeout each, parse, return a merged map.
fn poll_runtimes() -> HashMap<String, String> {
    let mut map = HashMap::new();
    for cli in ["docker", "podman"] {
        if let Some(stdout) = run_with_timeout(cli, Duration::from_secs(1)) {
            for (id, name) in parse_ps_lines(&stdout) {
                map.insert(id.to_string(), name.to_string());
            }
        }
    }
    map
}

/// Run `<cli> ps --no-trunc --format '{{.ID}} {{.Names}}'` with a
/// hard wall-clock cap. Spawning + polling `try_wait` keeps a wedged
/// daemon from pinning the worker thread for minutes.
fn run_with_timeout(cli: &str, timeout: Duration) -> Option<String> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = Command::new(cli)
        .args(["ps", "--no-trunc", "--format", "{{.ID}} {{.Names}}"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut buf = String::new();
                child.stdout.take()?.read_to_string(&mut buf).ok()?;
                return Some(buf);
            }
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// Parse the output of `docker ps --no-trunc --format '{{.ID}} {{.Names}}'`.
/// Each non-empty line is `<id> <name>`. Both runtimes emit the same
/// shape so we share one parser.
fn parse_ps_lines(stdout: &str) -> Vec<(&str, &str)> {
    stdout
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            line.split_once(' ')
                .map(|(id, name)| (id.trim(), name.trim()))
        })
        .filter(|(id, name)| !id.is_empty() && !name.is_empty())
        .collect()
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_lang_java_node_python_etc() {
        assert_eq!(
            classify_lang("/usr/bin/java -jar app.jar"),
            Some(Lang::Java)
        );
        assert_eq!(classify_lang("node server.js"), Some(Lang::Node));
        assert_eq!(classify_lang("nodejs server.js"), Some(Lang::Node));
        assert_eq!(classify_lang("/usr/bin/bun run dev"), Some(Lang::Bun));
        assert_eq!(classify_lang("deno task start"), Some(Lang::Deno));
        assert_eq!(
            classify_lang("/usr/bin/python3 -m http.server"),
            Some(Lang::Python),
        );
        assert_eq!(classify_lang("python3.11 manage.py"), Some(Lang::Python));
        assert_eq!(classify_lang("ruby script.rb"), Some(Lang::Ruby));
        assert_eq!(classify_lang("php-fpm: master process"), Some(Lang::Php));
        assert_eq!(classify_lang("/usr/bin/perl -w foo.pl"), Some(Lang::Perl));
        assert_eq!(classify_lang("luajit"), Some(Lang::Lua));
        assert_eq!(classify_lang("beam.smp"), Some(Lang::Erlang));
        assert_eq!(classify_lang("dotnet myapp.dll"), Some(Lang::DotNet));
        assert_eq!(classify_lang("Rscript --vanilla foo.R"), Some(Lang::R));
    }

    #[test]
    fn classify_lang_returns_none_for_native_binaries() {
        // Go and Rust binaries can't be detected from cmdline alone.
        assert_eq!(classify_lang("/home/user/myapp/server"), None);
        assert_eq!(classify_lang("nginx: master"), None);
        assert_eq!(classify_lang(""), None);
    }

    #[test]
    fn classify_lang_handles_null_separated_cmdline() {
        // /proc/<pid>/cmdline arrives null-separated; our argv0
        // parser must split on '\0' before running its file_name
        // logic.
        assert_eq!(
            classify_lang("/usr/bin/python3\0-c\0print(1)"),
            Some(Lang::Python),
        );
    }

    #[test]
    fn parse_container_cgroup_modern_docker() {
        let raw = "0::/system.slice/docker-abcdef0123456789abcdef0123456789.scope";
        let c = parse_container_cgroup(raw).expect("docker recognised");
        assert_eq!(c.runtime, ContainerRuntime::Docker);
        assert_eq!(c.id, "abcdef012345"); // truncated to 12 chars
    }

    #[test]
    fn parse_container_cgroup_legacy_docker() {
        let raw = "0::/docker/abcdef0123456789";
        let c = parse_container_cgroup(raw).expect("legacy docker recognised");
        assert_eq!(c.runtime, ContainerRuntime::Docker);
        assert_eq!(c.id, "abcdef012345");
    }

    #[test]
    fn parse_container_cgroup_podman() {
        // Modern rootless podman puts containers under user.slice.
        let raw = "0::/user.slice/user-1000.slice/user@1000.service/user.slice/libpod-deadbeef0123456789abcdef.scope";
        let c = parse_container_cgroup(raw).expect("podman recognised");
        assert_eq!(c.runtime, ContainerRuntime::Podman);
        assert_eq!(c.id, "deadbeef0123");
    }

    #[test]
    fn parse_container_cgroup_kubernetes_wraps_docker() {
        // Kube pod with docker as the CRI: kubepods wins.
        let raw = "0::/kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod1234.slice/docker-cafe123456789abcdef0123456.scope";
        let c = parse_container_cgroup(raw).expect("k8s recognised");
        assert_eq!(c.runtime, ContainerRuntime::Kubernetes);
        assert_eq!(c.id, "cafe12345678");
    }

    #[test]
    fn parse_container_cgroup_lxc_name_is_kept_verbatim() {
        // LXC stores a human-readable container name; keep the slug
        // unchanged (no 12-char short form for names like "web-prod").
        let raw = "0::/lxc/web-prod";
        let c = parse_container_cgroup(raw).expect("lxc recognised");
        assert_eq!(c.runtime, ContainerRuntime::Lxc);
        assert_eq!(c.id, "web-prod");
    }

    #[test]
    fn parse_container_cgroup_returns_none_for_host_processes() {
        // Most host processes live under user.slice / system.slice
        // with no docker- / libpod- / kubepods marker.
        assert!(
            parse_container_cgroup("0::/user.slice/user-1000.slice/session-c1.scope").is_none()
        );
        assert!(parse_container_cgroup("0::/system.slice/sshd.service").is_none());
        assert!(parse_container_cgroup("").is_none());
    }

    #[test]
    fn parse_container_cgroup_handles_cgroup_v1_multiline() {
        // cgroup v1 has one line per controller; the runtime path is
        // the same on every line, so we should match on whichever we
        // see first.
        let raw = "11:cpuset:/docker/abcdef0123456789\n10:freezer:/docker/abcdef0123456789\n0::/user.slice";
        let c = parse_container_cgroup(raw).expect("v1 cgroup recognised");
        assert_eq!(c.runtime, ContainerRuntime::Docker);
        assert_eq!(c.id, "abcdef012345");
    }

    #[test]
    fn classify_process_container_wins_over_runtime() {
        // A node process inside a docker container is grouped with
        // the container, not lumped in with all other Node processes.
        let cmdline = "node server.js";
        let cgroup = Some("0::/system.slice/docker-cafebabe9999999999999999.scope");
        let g = classify_process(cmdline, cgroup);
        match g {
            Group::Container(c) => {
                assert_eq!(c.runtime, ContainerRuntime::Docker);
                assert_eq!(c.id, "cafebabe9999");
            }
            other => panic!("expected container group, got {other:?}"),
        }
    }

    #[test]
    fn classify_process_runtime_wins_when_no_container() {
        let g = classify_process("/usr/bin/java -jar app.jar", None);
        assert_eq!(g, Group::Runtime(Lang::Java, "app.jar".into()));
    }

    #[test]
    fn classify_process_falls_through_to_system_then_native() {
        // systemd → System.
        assert_eq!(
            classify_process("/lib/systemd/systemd --user", None),
            Group::System,
        );
        // Kernel threads (cmdline empty → procs.rs renders as `[kthreadd]`).
        assert_eq!(classify_process("[kthreadd]", None), Group::System);
        // Anything else → Native.
        assert_eq!(
            classify_process("/usr/local/bin/myapp --serve", None),
            Group::Native,
        );
    }

    #[test]
    fn group_sort_key_orders_containers_first() {
        // Containers should sort before runtimes, runtimes before
        // system, system before native — confirms the band ordering
        // the renderer relies on.
        let containerised = Group::Container(Container {
            runtime: ContainerRuntime::Docker,
            id: "abc12345".into(),
        });
        let runtime = Group::Runtime(Lang::Java, "app.jar".into());
        let system = Group::System;
        let native = Group::Native;
        let mut keys = [
            native.sort_key(),
            system.sort_key(),
            runtime.sort_key(),
            containerised.sort_key(),
        ];
        keys.sort();
        assert_eq!(keys[0], containerised.sort_key());
        assert_eq!(keys[1], runtime.sort_key());
        assert_eq!(keys[2], system.sort_key());
        assert_eq!(keys[3], native.sort_key());
    }

    #[test]
    fn group_label_distinguishes_kinds() {
        let c = Group::Container(Container {
            runtime: ContainerRuntime::Podman,
            id: "abc12".into(),
        });
        assert_eq!(c.label(), "podman:abc12");
        // Runtime labels carry the concurrency-model signature so
        // the user knows what kind of "busy" the group represents.
        // Runtime labels include the app identifier so each app
        // splits into its own bucket; empty app falls back to the
        // bare `lang [signature]` form.
        assert_eq!(
            Group::Runtime(Lang::Node, "server.js".into()).label(),
            "node:server.js [event loop]"
        );
        assert_eq!(
            Group::Runtime(Lang::Go, "caddy".into()).label(),
            "go:caddy [goroutines]"
        );
        assert_eq!(
            Group::Runtime(Lang::Rust, "neotop".into()).label(),
            "rust:neotop [async/threads]"
        );
        assert_eq!(
            Group::Runtime(Lang::Java, "app.jar".into()).label(),
            "java:app.jar [vthreads]"
        );
        // Empty app -> single bucket per language.
        assert_eq!(
            Group::Runtime(Lang::Python, String::new()).label(),
            "python [GIL+asyncio]"
        );
        assert_eq!(Group::System.label(), "system");
        assert_eq!(Group::Native.label(), "native");
    }

    #[test]
    fn extract_app_pulls_jar_for_java_dash_jar_form() {
        let app = extract_app(
            "/usr/bin/java -Xmx2g -jar /opt/srv/api.jar --port 8080",
            Lang::Java,
        );
        assert_eq!(app, "api.jar");
    }

    #[test]
    fn extract_app_falls_back_to_main_class_when_no_jar() {
        // Classpath value mustn't be mistaken for the main class —
        // -cp/-classpath/--class-path eat their next token.
        let app = extract_app(
            "java -cp lib/*.jar:src com.example.Main --serve",
            Lang::Java,
        );
        assert_eq!(app, "com.example.Main");
    }

    #[test]
    fn extract_app_handles_python_dash_m() {
        let app = extract_app("/usr/bin/python3 -m gunicorn config.wsgi", Lang::Python);
        assert_eq!(app, "gunicorn");
    }

    #[test]
    fn extract_app_python_script_basename() {
        let app = extract_app("python3 /opt/api/server.py --debug", Lang::Python);
        assert_eq!(app, "server.py");
    }

    #[test]
    fn extract_app_node_takes_first_non_flag() {
        let app = extract_app("/usr/bin/node --inspect /srv/api/index.js", Lang::Node);
        assert_eq!(app, "index.js");
    }

    #[test]
    fn extract_app_compiled_languages_use_argv0_basename() {
        // Go and Rust binaries: each distinct executable is its own
        // "app". Reading the cmdline alone is enough — same path the
        // ELF-detected upgrade in procs.rs takes.
        assert_eq!(extract_app("/usr/local/bin/caddy run", Lang::Go), "caddy");
        assert_eq!(
            extract_app("/home/me/projects/neotop/target/release/neotop", Lang::Rust),
            "neotop"
        );
    }

    #[test]
    fn extract_app_returns_empty_when_no_script_present() {
        // Plain `python3` REPL — no script argument. The renderer
        // falls back to a single `python [GIL+asyncio]` bucket.
        assert_eq!(extract_app("python3", Lang::Python), "");
        assert_eq!(extract_app("/usr/bin/node --inspect", Lang::Node), "");
    }

    #[test]
    fn classify_two_rust_binaries_produce_distinct_groups() {
        // Regression for the "all Rust processes pile into one giant
        // group" bug — sort_key must differ so the renderer puts them
        // in separate buckets.
        let a = Group::Runtime(Lang::Rust, "neotop".into());
        let b = Group::Runtime(Lang::Rust, "alacritty".into());
        assert_ne!(a.sort_key(), b.sort_key());
        // Same lang still clusters before another lang within the
        // runtime band — `2_rust_*` lexicographically precedes
        // `2_go_*`? No — `2_go_*` precedes `2_rust_*`. The point is
        // each `(lang, app)` pair is its own bucket.
        let c = Group::Runtime(Lang::Go, "caddy".into());
        assert_ne!(a.sort_key(), c.sort_key());
    }

    #[test]
    fn lang_signature_is_short_and_distinct_per_runtime() {
        // The signature shares row-budget with the count + totals
        // in the group banner — keep them ≤ ~14 chars so a 80-col
        // terminal still fits the rest of the row.
        for l in [
            Lang::Java,
            Lang::Node,
            Lang::Bun,
            Lang::Deno,
            Lang::Python,
            Lang::Ruby,
            Lang::Php,
            Lang::Perl,
            Lang::Lua,
            Lang::Erlang,
            Lang::DotNet,
            Lang::R,
            Lang::Go,
            Lang::Rust,
        ] {
            assert!(!l.signature().is_empty());
            assert!(
                l.signature().len() <= 14,
                "{} signature too long",
                l.label()
            );
        }
    }

    #[test]
    fn parse_ps_lines_extracts_id_name_pairs() {
        // What `docker ps --no-trunc --format '{{.ID}} {{.Names}}'`
        // looks like in practice. Two-word names are valid (Docker
        // doesn't allow spaces in names but the parser shouldn't
        // care — it splits on the first space only).
        let stdout = "\
abc1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcd myapp
def0987654321def0987654321def0987654321def0987654321def098765432 quirky_einstein

ghi5555555555ghi5555555555ghi5555555555ghi5555555555ghi555555555 redis-cache
";
        let pairs = parse_ps_lines(stdout);
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0].1, "myapp");
        assert_eq!(pairs[1].1, "quirky_einstein");
        assert_eq!(pairs[2].1, "redis-cache");
        // IDs are full SHAs (the actual character count is
        // irrelevant to the parser — what matters is that we keep
        // them intact for downstream prefix matching).
        for (id, _) in &pairs {
            assert!(id.len() >= 32, "SHA-shaped IDs only");
        }
    }

    #[test]
    fn parse_ps_lines_skips_blank_and_malformed() {
        // Header lines (which `--format` strips anyway) and
        // single-token lines must not crash the parser.
        let stdout = "\n   \nonlyone\nabc def\n";
        let pairs = parse_ps_lines(stdout);
        assert_eq!(pairs, vec![("abc", "def")]);
    }

    #[test]
    fn container_names_lookup_resolves_short_id_via_prefix() {
        // `Container::id` carries the 12-char short hash. The
        // cache stores the full SHA. Lookup must succeed via prefix
        // match.
        let mut cn = ContainerNames::default();
        cn.map.insert(
            "abc1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcd".into(),
            "myapp".into(),
        );
        assert_eq!(cn.lookup("abc123456789"), Some("myapp"));
        // Full SHA also resolves.
        assert_eq!(
            cn.lookup("abc1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcd"),
            Some("myapp")
        );
        // Unknown ID → None (no panic).
        assert_eq!(cn.lookup("ffffffffffff"), None);
    }

    #[test]
    fn group_label_with_names_prefers_resolved_name() {
        let mut cn = ContainerNames::default();
        cn.map.insert(
            "abc1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcd".into(),
            "myapp".into(),
        );
        let g = Group::Container(Container {
            runtime: ContainerRuntime::Docker,
            id: "abc123456789".into(), // 12-char short
        });
        assert_eq!(g.label_with_names(&cn), "docker:myapp");

        // Unresolved container → falls back to id form.
        let g2 = Group::Container(Container {
            runtime: ContainerRuntime::Podman,
            id: "ffffff".into(),
        });
        assert_eq!(g2.label_with_names(&cn), "podman:ffffff");

        // Non-container groups ignore the cache.
        assert_eq!(
            Group::Runtime(Lang::Java, "app.jar".into()).label_with_names(&cn),
            "java:app.jar [vthreads]"
        );
    }
}
