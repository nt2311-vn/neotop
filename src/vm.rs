//! vm.rs — hypervisor / VM detection from `/proc/<pid>/cmdline`.
//!
//! Pure parser. Given the joined argv string (the form `procs.rs`
//! caches as `ProcessRow.command`), recognise QEMU / KVM,
//! Firecracker, Cloud Hypervisor, crosvm, and lkvm; pull out a
//! human-readable VM name, vCPU count, and memory cap.
//!
//! Phase 1 of the VM-support plan. Per-vCPU threads, KVM exit
//! counters, vhost-net queue depth come in later phases.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(clippy::enum_variant_names)]
pub(crate) enum Hypervisor {
    Qemu,
    Firecracker,
    CloudHypervisor,
    Crosvm,
    Lkvm,
}

impl Hypervisor {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Qemu => "qemu",
            Self::Firecracker => "firecracker",
            Self::CloudHypervisor => "cloud-hv",
            Self::Crosvm => "crosvm",
            Self::Lkvm => "lkvm",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct VmInfo {
    pub(crate) hypervisor: Hypervisor,
    pub(crate) name: String,
    pub(crate) vcpus: Option<u32>,
    pub(crate) mem_bytes: Option<u64>,
}

impl VmInfo {
    /// `qemu/myapp-prod (4 vCPU, 8 GiB)` style header label.
    pub(crate) fn label(&self) -> String {
        let mut s = format!("{}/{}", self.hypervisor.label(), self.name);
        let mut tags = Vec::new();
        if let Some(n) = self.vcpus {
            tags.push(format!("{n} vCPU"));
        }
        if let Some(b) = self.mem_bytes {
            tags.push(human_mem(b));
        }
        if !tags.is_empty() {
            use std::fmt::Write;
            let _ = write!(s, " ({})", tags.join(", "));
        }
        s
    }
}

/// Detect a hypervisor from the joined cmdline (`procs::ProcessRow.command`).
/// Returns `None` for non-VM processes — fast-path on the binary name
/// before doing any further parsing.
pub(crate) fn detect(command: &str) -> Option<VmInfo> {
    let bin = first_token_basename(command);
    let hv = classify_bin(bin)?;
    let name = parse_name(command, hv).unwrap_or_else(|| fallback_name(command, hv));
    let vcpus = parse_vcpus(command, hv);
    let mem_bytes = parse_memory(command, hv);
    Some(VmInfo {
        hypervisor: hv,
        name,
        vcpus,
        mem_bytes,
    })
}

fn first_token_basename(command: &str) -> &str {
    let first = command.split_whitespace().next().unwrap_or("");
    first.rsplit('/').next().unwrap_or(first)
}

fn classify_bin(bin: &str) -> Option<Hypervisor> {
    if bin.starts_with("qemu-system-") || bin == "qemu-kvm" {
        Some(Hypervisor::Qemu)
    } else if bin == "firecracker" {
        Some(Hypervisor::Firecracker)
    } else if bin == "cloud-hypervisor" {
        Some(Hypervisor::CloudHypervisor)
    } else if bin == "crosvm" {
        Some(Hypervisor::Crosvm)
    } else if bin == "lkvm" || bin == "vmm" {
        Some(Hypervisor::Lkvm)
    } else {
        None
    }
}

/// Pull `-name X` / `-name guest=X[,...]` for QEMU; `--name X` for
/// Cloud Hypervisor / Firecracker. Argv is space-joined, so flags
/// are simple to scan.
fn parse_name(command: &str, hv: Hypervisor) -> Option<String> {
    let toks: Vec<&str> = command.split_whitespace().collect();
    let flag = match hv {
        Hypervisor::Qemu => "-name",
        Hypervisor::CloudHypervisor | Hypervisor::Crosvm | Hypervisor::Lkvm => "--name",
        Hypervisor::Firecracker => "--id",
    };
    let mut it = toks.iter();
    while let Some(tok) = it.next() {
        if *tok == flag {
            if let Some(val) = it.next() {
                return Some(qemu_name_first_field(val));
            }
        }
    }
    None
}

/// QEMU `-name` accepts `guest=X,debug-threads=on,process=...`. Take
/// the first comma-separated kv pair, and strip a `guest=` prefix.
fn qemu_name_first_field(val: &str) -> String {
    let head = val.split(',').next().unwrap_or(val);
    head.strip_prefix("guest=").unwrap_or(head).to_string()
}

/// Last-resort name when no `-name` flag is present: derive from
/// the first `-drive file=...` basename, the firecracker `--api-sock`
/// path, or just the binary tag.
fn fallback_name(command: &str, hv: Hypervisor) -> String {
    if let Some(disk) = parse_drive_file(command) {
        return disk;
    }
    if let Some(sock) = parse_api_sock(command) {
        return sock;
    }
    hv.label().to_string()
}

fn parse_drive_file(command: &str) -> Option<String> {
    for tok in command.split_whitespace() {
        if let Some(rest) = tok.strip_prefix("file=") {
            let path = rest.split(',').next().unwrap_or(rest);
            return Some(basename_no_ext(path));
        }
    }
    None
}

fn parse_api_sock(command: &str) -> Option<String> {
    let toks: Vec<&str> = command.split_whitespace().collect();
    let mut it = toks.iter();
    while let Some(tok) = it.next() {
        if *tok == "--api-sock" || *tok == "--api-socket" {
            if let Some(val) = it.next() {
                return Some(basename_no_ext(val));
            }
        }
    }
    None
}

fn basename_no_ext(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.rsplit_once('.')
        .map_or(base, |(stem, _)| stem)
        .to_string()
}

/// Parse `-smp N` or `-smp cpus=N,sockets=...`. Cloud Hypervisor uses
/// `--cpus boot=N`. Firecracker JSON-only — Phase 1 returns None
/// for it.
fn parse_vcpus(command: &str, hv: Hypervisor) -> Option<u32> {
    let toks: Vec<&str> = command.split_whitespace().collect();
    match hv {
        Hypervisor::Qemu => {
            let mut it = toks.iter();
            while let Some(tok) = it.next() {
                if *tok == "-smp" {
                    if let Some(val) = it.next() {
                        return parse_smp_value(val);
                    }
                }
            }
            None
        }
        Hypervisor::CloudHypervisor => find_kv(&toks, "--cpus", "boot"),
        Hypervisor::Crosvm => find_flag_u32(&toks, "--cpus"),
        Hypervisor::Lkvm => find_flag_u32(&toks, "--cpus").or_else(|| find_flag_u32(&toks, "-c")),
        Hypervisor::Firecracker => None,
    }
}

fn parse_smp_value(val: &str) -> Option<u32> {
    if let Ok(n) = val.parse() {
        return Some(n);
    }
    for kv in val.split(',') {
        if let Some(n) = kv.strip_prefix("cpus=") {
            return n.parse().ok();
        }
    }
    None
}

/// QEMU `-m 8G` / `-m size=8G,...`; Cloud Hypervisor `--memory size=8G`.
fn parse_memory(command: &str, hv: Hypervisor) -> Option<u64> {
    let toks: Vec<&str> = command.split_whitespace().collect();
    match hv {
        Hypervisor::Qemu => {
            let mut it = toks.iter();
            while let Some(tok) = it.next() {
                if *tok == "-m" {
                    if let Some(val) = it.next() {
                        return parse_mem_value(val);
                    }
                }
            }
            None
        }
        Hypervisor::CloudHypervisor => {
            let raw = find_kv_str(&toks, "--memory", "size")?;
            parse_mem_value(&raw)
        }
        Hypervisor::Crosvm => {
            let mut it = toks.iter();
            while let Some(tok) = it.next() {
                if *tok == "--mem" {
                    if let Some(val) = it.next() {
                        return parse_mem_value(val);
                    }
                }
            }
            None
        }
        Hypervisor::Lkvm => {
            find_flag_u64_mb(&toks, "--mem").or_else(|| find_flag_u64_mb(&toks, "-m"))
        }
        Hypervisor::Firecracker => None,
    }
}

fn parse_mem_value(val: &str) -> Option<u64> {
    if let Some(rest) = val.strip_prefix("size=") {
        return parse_mem_value(rest.split(',').next().unwrap_or(rest));
    }
    let head = val.split(',').next().unwrap_or(val);
    parse_size_with_suffix(head)
}

/// Parse `8G`, `512M`, `2048` (MiB by default for QEMU `-m`).
fn parse_size_with_suffix(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (num_s, mult): (&str, u64) = if let Some(n) = s.strip_suffix(['G', 'g']) {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(['M', 'm']) {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(['K', 'k']) {
        (n, 1024)
    } else if let Some(n) = s.strip_suffix(['B', 'b']) {
        (n, 1)
    } else {
        // QEMU default unit for `-m` is MiB.
        (s, 1024 * 1024)
    };
    let n: u64 = num_s.trim().parse().ok()?;
    Some(n.saturating_mul(mult))
}

fn find_kv(toks: &[&str], flag: &str, key: &str) -> Option<u32> {
    let raw = find_kv_str(toks, flag, key)?;
    raw.parse().ok()
}

fn find_kv_str(toks: &[&str], flag: &str, key: &str) -> Option<String> {
    let mut it = toks.iter();
    while let Some(tok) = it.next() {
        if *tok == flag {
            if let Some(val) = it.next() {
                for kv in val.split(',') {
                    if let Some(n) = kv.strip_prefix(&format!("{key}=")) {
                        return Some(n.to_string());
                    }
                }
            }
        }
    }
    None
}

fn find_flag_u32(toks: &[&str], flag: &str) -> Option<u32> {
    let mut it = toks.iter();
    while let Some(tok) = it.next() {
        if *tok == flag {
            if let Some(val) = it.next() {
                return val.parse().ok();
            }
        }
    }
    None
}

fn find_flag_u64_mb(toks: &[&str], flag: &str) -> Option<u64> {
    let mut it = toks.iter();
    while let Some(tok) = it.next() {
        if *tok == flag {
            if let Some(val) = it.next() {
                let n: u64 = val.parse().ok()?;
                return Some(n.saturating_mul(1024 * 1024));
            }
        }
    }
    None
}

/// Render bytes as `512 MiB` / `8.0 GiB`. Mirrors `proc::human_bytes`
/// so VM headers and process detail blocks read consistently.
fn human_mem(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    #[allow(clippy::cast_precision_loss)]
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.0} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.0} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_qemu_with_guest_name() {
        let cmd = "/usr/bin/qemu-system-x86_64 -name guest=myapp,debug-threads=on -smp 4 -m 8G";
        let v = detect(cmd).expect("VM");
        assert_eq!(v.hypervisor, Hypervisor::Qemu);
        assert_eq!(v.name, "myapp");
        assert_eq!(v.vcpus, Some(4));
        assert_eq!(v.mem_bytes, Some(8 * 1024 * 1024 * 1024));
    }

    #[test]
    fn detects_qemu_smp_kv_form() {
        let cmd = "qemu-system-aarch64 -name vm1 -smp cpus=8,sockets=1,cores=8,threads=1 -m 4G";
        let v = detect(cmd).expect("VM");
        assert_eq!(v.vcpus, Some(8));
    }

    #[test]
    fn detects_qemu_size_kv_for_mem() {
        let cmd = "qemu-system-x86_64 -name foo -smp 2 -m size=2048M,slots=4";
        let v = detect(cmd).expect("VM");
        assert_eq!(v.mem_bytes, Some(2048 * 1024 * 1024));
    }

    #[test]
    fn fallbacks_to_drive_file_basename_when_no_name() {
        let cmd = "qemu-system-x86_64 -drive file=/var/lib/myapp/disk.qcow2,if=virtio -smp 2";
        let v = detect(cmd).expect("VM");
        assert_eq!(v.name, "disk");
    }

    #[test]
    fn detects_firecracker_with_api_sock() {
        let cmd = "/usr/bin/firecracker --api-sock /tmp/api-runner-1.sock";
        let v = detect(cmd).expect("VM");
        assert_eq!(v.hypervisor, Hypervisor::Firecracker);
        assert_eq!(v.name, "api-runner-1");
    }

    #[test]
    fn detects_cloud_hypervisor_cpus_and_memory() {
        let cmd = "cloud-hypervisor --cpus boot=2 --memory size=512M";
        let v = detect(cmd).expect("VM");
        assert_eq!(v.hypervisor, Hypervisor::CloudHypervisor);
        assert_eq!(v.vcpus, Some(2));
        assert_eq!(v.mem_bytes, Some(512 * 1024 * 1024));
    }

    #[test]
    fn detects_crosvm() {
        let cmd = "/usr/bin/crosvm run --cpus 4 --mem 4G --rwroot disk.img";
        let v = detect(cmd).expect("VM");
        assert_eq!(v.hypervisor, Hypervisor::Crosvm);
        assert_eq!(v.vcpus, Some(4));
        assert_eq!(v.mem_bytes, Some(4 * 1024 * 1024 * 1024));
    }

    #[test]
    fn returns_none_for_non_vm_processes() {
        assert!(detect("/usr/bin/firefox --no-remote").is_none());
        assert!(detect("").is_none());
        assert!(detect("/usr/bin/python3 server.py").is_none());
    }

    #[test]
    fn label_renders_compact_summary() {
        let v = VmInfo {
            hypervisor: Hypervisor::Qemu,
            name: "myvm".into(),
            vcpus: Some(4),
            mem_bytes: Some(8 * 1024 * 1024 * 1024),
        };
        assert_eq!(v.label(), "qemu/myvm (4 vCPU, 8.0 GiB)");
    }

    #[test]
    fn label_omits_missing_fields() {
        let v = VmInfo {
            hypervisor: Hypervisor::Firecracker,
            name: "fc1".into(),
            vcpus: None,
            mem_bytes: None,
        };
        assert_eq!(v.label(), "firecracker/fc1");
    }
}
