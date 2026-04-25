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
    Runtime(Lang),
    System,
    Native,
}

impl Group {
    /// Compact label for the group header row. Distinct prefix per
    /// kind so the eye instantly separates `docker:abc12` from
    /// `java` from `system`.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Container(c) => format!("{}:{}", c.runtime.label(), c.id),
            Self::Runtime(l) => l.label().to_string(),
            Self::System => "system".into(),
            Self::Native => "native".into(),
        }
    }

    /// Stable ordering key. Containers first (most actionable for
    /// developers), then language runtimes, then system, then native.
    /// Within each band, the label provides a deterministic sub-order.
    pub(crate) fn sort_key(&self) -> String {
        match self {
            Self::Container(c) => format!("0_{}_{}", c.runtime.label(), c.id),
            Self::Runtime(l) => format!("1_{}", l.label()),
            Self::System => "2_system".into(),
            Self::Native => "3_native".into(),
        }
    }

    /// Quick band check — used by the renderer to colour container
    /// headers (cyan) differently from runtime headers (yellow) etc.
    pub(crate) fn band(&self) -> GroupBand {
        match self {
            Self::Container(_) => GroupBand::Container,
            Self::Runtime(_) => GroupBand::Runtime,
            Self::System => GroupBand::System,
            Self::Native => GroupBand::Native,
        }
    }

    /// Display label that consults the `ContainerNames` cache so
    /// container groups surface the human-readable name (e.g.
    /// `docker:myapp`) instead of the raw 12-char short hash. Falls
    /// back to `label()` for non-container groups and for containers
    /// the user hasn't named (anonymous `docker run` invocations get
    /// auto-generated names like `quirky_einstein` either way, but
    /// before `docker ps` has been polled the map is empty).
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

/// Coarser bucket for header colouring. Pulled out so the renderer
/// doesn't have to match on the inner `Container` / `Lang` to choose
/// a colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum GroupBand {
    Container,
    Runtime,
    System,
    #[default]
    Native,
}

/// Top-level classifier: container > runtime > system > native, in
/// that priority order.
pub(crate) fn classify_process(cmdline: &str, cgroup: Option<&str>) -> Group {
    if let Some(c) = cgroup.and_then(parse_container_cgroup) {
        return Group::Container(c);
    }
    if let Some(lang) = classify_lang(cmdline) {
        return Group::Runtime(lang);
    }
    if is_system(cmdline) {
        return Group::System;
    }
    Group::Native
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

/// How long a `docker ps` / `podman ps` snapshot stays valid before
/// we re-poll. 5 s is the same trade-off other tools make: containers
/// don't churn at sub-second rates in normal use, and we'd rather
/// pay one fork+exec every few seconds than every render frame.
const NAME_CACHE_TTL: Duration = Duration::from_secs(5);

/// Maps container ID (full 64-char SHA-256 from `docker ps
/// --no-trunc`) to the human-readable container name. Refreshed on
/// a slow tick by shelling out to the runtime CLIs. The shell-out
/// costs ~5 ms per tool when the daemon is up and is silent (`status
/// != 0` swallowed) when it isn't — that way neotop doesn't grow a
/// hard runtime dependency.
#[derive(Debug, Default)]
pub(crate) struct ContainerNames {
    /// Full-length container ID → name. We store the full 64-char
    /// hash because cgroup paths can carry either form (modern
    /// docker uses the full sha; podman trims to short ids in some
    /// systemd unit names) and we resolve via prefix match either
    /// way.
    map: HashMap<String, String>,
    last_refresh: Option<Instant>,
}

impl ContainerNames {
    /// Re-poll if the cache is stale (older than `NAME_CACHE_TTL`).
    /// Cheap to call every tick; only actually shells out when the
    /// TTL has elapsed.
    pub(crate) fn refresh_if_stale(&mut self, now: Instant) {
        // `Option::is_none_or` would read more naturally here but
        // it's only stable since 1.82 and our MSRV is 1.80; the
        // `map_or(true, ..)` shape is the equivalent on older
        // toolchains.
        let stale = self
            .last_refresh
            .map_or(true, |t| now.duration_since(t) >= NAME_CACHE_TTL);
        if stale {
            self.refresh_now(now);
        }
    }

    fn refresh_now(&mut self, now: Instant) {
        self.last_refresh = Some(now);
        let mut map = HashMap::new();
        for &cli in &["docker", "podman"] {
            // `--no-trunc` gives us the full 64-char SHA so we can
            // resolve against either short (12) or full IDs from
            // `/proc/<pid>/cgroup`. The format string is the same
            // for both runtimes.
            let out = Command::new(cli)
                .args(["ps", "--no-trunc", "--format", "{{.ID}} {{.Names}}"])
                .output();
            if let Ok(o) = out {
                if o.status.success() {
                    if let Ok(s) = std::str::from_utf8(&o.stdout) {
                        for (id, name) in parse_ps_lines(s) {
                            map.insert(id.to_string(), name.to_string());
                        }
                    }
                }
            }
        }
        self.map = map;
    }

    /// Resolve `id` to the human-readable name. Accepts either a
    /// 12-char short hash (the form `Container::id` carries) or a
    /// full 64-char SHA. Returns `None` if no matching container is
    /// known to the daemons we polled.
    pub(crate) fn lookup(&self, id: &str) -> Option<&str> {
        if let Some(name) = self.map.get(id) {
            return Some(name);
        }
        // Treat `id` as a prefix and look for a stored full SHA
        // that starts with it. O(n) over the map but n is "running
        // containers", which is rarely above 50.
        for (full, name) in &self.map {
            if full.starts_with(id) {
                return Some(name);
            }
        }
        None
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
        assert_eq!(g, Group::Runtime(Lang::Java));
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
        let runtime = Group::Runtime(Lang::Java);
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
        assert_eq!(Group::Runtime(Lang::Node).label(), "node");
        assert_eq!(Group::System.label(), "system");
        assert_eq!(Group::Native.label(), "native");
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
        assert_eq!(Group::Runtime(Lang::Java).label_with_names(&cn), "java");
    }
}
