//! passthrough.rs — VFIO + vhost + tap discovery for a VM PID.
//!
//! Phase 4 of `VMPLAN.md`. We answer two questions about a selected
//! VM purely from kernel-exposed file surfaces:
//!
//! 1. **What devices is it passing through?** Walk
//!    `/proc/<pid>/fd/`, find symlinks pointing at `/dev/vfio/<N>`,
//!    then resolve each IOMMU group through
//!    `/sys/kernel/iommu_groups/<N>/devices/` to PCI BDFs and read
//!    vendor / device / class IDs from
//!    `/sys/bus/pci/devices/<bdf>/`.
//!
//! 2. **What virtual NICs is it using?** Same fd walk, picking out
//!    `/dev/net/tun` handles. The tap interface name lives in
//!    `/proc/<pid>/fdinfo/<fd>` as an `iff:<name>` line — `iproute2`
//!    has used this surface for years. Cross-reference with the
//!    existing `net::Tracker` snapshot for rx / tx rates.
//!
//! Also identifies open `/dev/vhost-{net,vsock,scsi,fs}` handles so
//! the detail pane can show which vhost back-ends the VM is using.
//!
//! Deliberately *not* implemented: queue depth. `vhost-net`'s
//! per-queue counters live behind ioctls (`VHOST_GET_VRING_*`) —
//! reaching them would mean a syscall surface with no read-only
//! guarantees, breaking neotop's "observe via /proc and /sys only"
//! contract. The existing `net::Iface` byte-rate is already a good
//! proxy for "is the guest's NIC busy".

use std::fs;
use std::path::Path;

/// Snapshot of everything passthrough-related for one VM PID.
/// `Default` is the empty result — what we return for non-VM PIDs
/// or when `/proc/<pid>/fd` isn't readable (the process exited or
/// permission was revoked).
#[derive(Debug, Clone, Default)]
pub(crate) struct Passthrough {
    pub(crate) vfio_groups: Vec<VfioGroup>,
    pub(crate) vhost: Vec<VhostKind>,
    pub(crate) taps: Vec<String>,
}

impl Passthrough {
    /// True when none of the three lists has anything — used by the
    /// renderer to skip the whole "── devices ──" / "── network ──"
    /// block instead of drawing an empty section header.
    pub(crate) fn is_empty(&self) -> bool {
        self.vfio_groups.is_empty() && self.vhost.is_empty() && self.taps.is_empty()
    }
}

/// One IOMMU group as exposed via VFIO. The QEMU PID has an open
/// fd on `/dev/vfio/<group_id>`, and the group's PCI BDFs come from
/// `/sys/kernel/iommu_groups/<group_id>/devices/`.
#[derive(Debug, Clone)]
pub(crate) struct VfioGroup {
    pub(crate) group_id: u32,
    pub(crate) devices: Vec<PciDevice>,
}

/// Minimal description of a PCI function. Vendor / device / class
/// IDs are the canonical source-of-truth — the human names are a
/// best-effort lookup against a tiny in-binary table.
#[derive(Debug, Clone)]
pub(crate) struct PciDevice {
    /// Bus-Device-Function form: `0000:01:00.0`. Identical to what
    /// `lspci` prints.
    pub(crate) bdf: String,
    pub(crate) vendor: u16,
    pub(crate) device: u16,
    /// 24-bit PCI class code: `BB SS PP` (base, subclass, prog-if).
    pub(crate) class: u32,
    /// `Some("NVIDIA")`, `Some("Intel")`, … when the vendor matches
    /// the in-binary table; `None` otherwise. We deliberately don't
    /// ship a full `pci.ids` blob — the vendor on its own is enough
    /// for "is this GPU passthrough?", and the BDF + IDs are
    /// already plenty for the user to run `lspci -s …` themselves.
    pub(crate) vendor_label: Option<&'static str>,
}

impl PciDevice {
    /// Compact one-line label for the detail pane: `0000:01:00.0
    /// NVIDIA 10de:1d01 [display]`. Length stays ≤ ~40 chars so it
    /// fits the right-hand pane comfortably.
    pub(crate) fn label(&self) -> String {
        let ids = match self.vendor_label {
            Some(v) => format!("{v} {:04x}:{:04x}", self.vendor, self.device),
            None => format!("{:04x}:{:04x}", self.vendor, self.device),
        };
        match pci_class_label(self.class) {
            Some(k) => format!("{} {ids} [{k}]", self.bdf),
            None => format!("{} {ids}", self.bdf),
        }
    }
}

/// Categorical label for a `/dev/vhost-*` handle. We keep just the
/// flavour — open-count or per-queue stats would need ioctls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VhostKind {
    Net,
    Vsock,
    Scsi,
    Fs,
}

impl VhostKind {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::Net => "vhost-net",
            Self::Vsock => "vhost-vsock",
            Self::Scsi => "vhost-scsi",
            Self::Fs => "vhost-fs",
        }
    }
}

/// Walk a single VM PID's open file descriptors and collect VFIO
/// groups, vhost back-ends, and tap interfaces. ~one syscall per fd
/// (`readlink`) plus a constant amount of work per unique device,
/// so a VM with 5 VFIO devices and 2 NICs costs maybe 30 syscalls
/// total — easily affordable on a 1 Hz selection refresh.
pub(crate) fn snapshot(pid: i32) -> Passthrough {
    let fd_dir = format!("/proc/{pid}/fd");
    let Ok(entries) = fs::read_dir(&fd_dir) else {
        return Passthrough::default();
    };

    let mut vfio_ids: Vec<u32> = Vec::new();
    let mut vhost: Vec<VhostKind> = Vec::new();
    let mut taps: Vec<String> = Vec::new();

    for entry in entries.flatten() {
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        let target = target.to_string_lossy();

        // /dev/vfio/<N> — the IOMMU group fd. The bare control fd
        // /dev/vfio/vfio is also opened by QEMU; skip it (parsing
        // the suffix as u32 fails, which is the natural filter).
        if let Some(rest) = target.strip_prefix("/dev/vfio/") {
            if let Ok(n) = rest.parse::<u32>() {
                if !vfio_ids.contains(&n) {
                    vfio_ids.push(n);
                }
            }
            continue;
        }

        // /dev/vhost-* — flavour matters (net vs vsock vs scsi),
        // anything else falls through.
        if let Some(rest) = target.strip_prefix("/dev/vhost-") {
            let kind = match rest {
                "net" => Some(VhostKind::Net),
                "vsock" => Some(VhostKind::Vsock),
                "scsi" => Some(VhostKind::Scsi),
                "fs" => Some(VhostKind::Fs),
                _ => None,
            };
            if let Some(k) = kind {
                if !vhost.contains(&k) {
                    vhost.push(k);
                }
            }
            continue;
        }

        // /dev/net/tun — the symlink target is the same /dev node
        // regardless of which tap interface this fd is attached to;
        // the actual interface name lives in fdinfo as `iff:<name>`.
        if target == "/dev/net/tun" {
            let fd_name = entry.file_name();
            let info_path = format!("/proc/{pid}/fdinfo/{}", fd_name.to_string_lossy());
            if let Some(name) = read_tap_name_from_fdinfo(&info_path) {
                if !taps.contains(&name) {
                    taps.push(name);
                }
            }
        }
    }

    vfio_ids.sort_unstable();
    vhost.sort_by_key(VhostKind::label);
    taps.sort();
    Passthrough {
        vfio_groups: vfio_ids
            .into_iter()
            .map(|g| VfioGroup {
                group_id: g,
                devices: read_iommu_group_devices(g),
            })
            .collect(),
        vhost,
        taps,
    }
}

/// `/proc/<pid>/fdinfo/<fd>` holds key:value lines. Tap fds carry
/// `iff:<ifname>`. Returns `None` when the file is gone (race) or
/// the line is missing (unlikely on a real tun fd).
fn read_tap_name_from_fdinfo(path: &str) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("iff:") {
            let name = rest.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Resolve an IOMMU group id to its constituent PCI functions. The
/// kernel exposes the group's members as symlinks under
/// `/sys/kernel/iommu_groups/<N>/devices/`; the link names are the
/// PCI BDFs themselves.
fn read_iommu_group_devices(group_id: u32) -> Vec<PciDevice> {
    let dir = format!("/sys/kernel/iommu_groups/{group_id}/devices");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut devices: Vec<PciDevice> = entries
        .flatten()
        .map(|e| {
            let bdf = e.file_name().to_string_lossy().into_owned();
            read_pci_device(&bdf)
        })
        .collect();
    // Sort so the rendered list is stable across ticks. BDF sorts
    // lexicographically the same way `lspci` lists them.
    devices.sort_by(|a, b| a.bdf.cmp(&b.bdf));
    devices
}

fn read_pci_device(bdf: &str) -> PciDevice {
    let base = format!("/sys/bus/pci/devices/{bdf}");
    let vendor = read_hex16(&format!("{base}/vendor"));
    let device = read_hex16(&format!("{base}/device"));
    let class = read_hex32(&format!("{base}/class"));
    PciDevice {
        bdf: bdf.to_string(),
        vendor,
        device,
        class,
        vendor_label: vendor_name(vendor),
    }
}

/// Read a `0xABCD\n` hex file to a `u16`; returns 0 on any error.
/// PCI sysfs entries are always exactly that shape.
fn read_hex16(path: &str) -> u16 {
    let raw = fs::read_to_string(Path::new(path)).unwrap_or_default();
    let s = raw.trim().trim_start_matches("0x");
    u16::from_str_radix(s, 16).unwrap_or(0)
}

fn read_hex32(path: &str) -> u32 {
    let raw = fs::read_to_string(Path::new(path)).unwrap_or_default();
    let s = raw.trim().trim_start_matches("0x");
    u32::from_str_radix(s, 16).unwrap_or(0)
}

/// Tiny in-binary lookup for the vendors we care about most in a
/// passthrough context. Keeps the binary lean (no `pci.ids` ship)
/// while still rendering "NVIDIA" instead of `10de` for the GPU
/// passthrough case that motivates this pane.
fn vendor_name(vendor: u16) -> Option<&'static str> {
    match vendor {
        0x10de => Some("NVIDIA"),
        0x8086 => Some("Intel"),
        // 0x1002 GPU + 0x1022 CPU/chipset both belong to AMD; the
        // user reads them as one vendor in the table.
        0x1002 | 0x1022 => Some("AMD"),
        0x15b3 => Some("Mellanox"),
        0x14e4 => Some("Broadcom"),
        0x10ec => Some("Realtek"),
        0x1af4 => Some("Red Hat"), // virtio-* devices
        0x1b21 => Some("ASMedia"),
        0x144d => Some("Samsung"),
        0x1b4b => Some("Marvell"),
        _ => None,
    }
}

/// PCI base class → human-readable category. Only the classes that
/// actually show up in passthrough scenarios; everything else falls
/// through to "no tag", which keeps the row short.
fn pci_class_label(class: u32) -> Option<&'static str> {
    match (class >> 16) & 0xff {
        0x01 => Some("storage"),
        0x02 => Some("net"),
        0x03 => Some("display"),
        0x04 => Some("multimedia"),
        0x07 => Some("comm"),
        0x0c => Some("serial-bus"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_device_label_includes_vendor_when_known() {
        let dev = PciDevice {
            bdf: "0000:01:00.0".into(),
            vendor: 0x10de,
            device: 0x1d01,
            class: 0x03_0000, // 0x03 = display
            vendor_label: Some("NVIDIA"),
        };
        assert_eq!(dev.label(), "0000:01:00.0 NVIDIA 10de:1d01 [display]");
    }

    #[test]
    fn pci_device_label_falls_back_to_ids_only() {
        let dev = PciDevice {
            bdf: "0000:02:00.0".into(),
            vendor: 0x9999,
            device: 0xabcd,
            class: 0xff_ffff, // unknown class → no tag
            vendor_label: None,
        };
        assert_eq!(dev.label(), "0000:02:00.0 9999:abcd");
    }

    #[test]
    fn pci_class_label_recognises_common_categories() {
        assert_eq!(pci_class_label(0x01_0601), Some("storage")); // SATA
        assert_eq!(pci_class_label(0x02_0000), Some("net"));
        assert_eq!(pci_class_label(0x03_0000), Some("display"));
        assert_eq!(pci_class_label(0x0c_0330), Some("serial-bus")); // USB xHCI
        assert_eq!(pci_class_label(0xff_0000), None);
    }

    #[test]
    fn vhost_kind_labels_are_stable() {
        // The detail pane sorts by these labels; they must stay
        // ASCII-only and lower-case so the order is predictable.
        assert_eq!(VhostKind::Net.label(), "vhost-net");
        assert_eq!(VhostKind::Vsock.label(), "vhost-vsock");
        assert_eq!(VhostKind::Scsi.label(), "vhost-scsi");
        assert_eq!(VhostKind::Fs.label(), "vhost-fs");
    }

    #[test]
    fn passthrough_default_is_empty() {
        let p = Passthrough::default();
        assert!(p.is_empty());
        assert!(p.vfio_groups.is_empty());
        assert!(p.vhost.is_empty());
        assert!(p.taps.is_empty());
    }

    #[test]
    fn snapshot_for_nonexistent_pid_returns_empty() {
        // PID 0 doesn't exist on Linux; readdir fails and we fall
        // through to the default. Importantly: no panic, no error
        // printed — the renderer just shows nothing.
        let p = snapshot(0);
        assert!(p.is_empty());
    }

    #[test]
    fn read_hex16_parses_pci_sysfs_format() {
        // PCI sysfs files have the literal "0x10de\n" form. Robust
        // against trailing newline, missing 0x prefix, and parse
        // failures (returns 0).
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "0x10de\n").unwrap();
        assert_eq!(read_hex16(tmp.path().to_str().unwrap()), 0x10de);
        std::fs::write(tmp.path(), "8086").unwrap();
        assert_eq!(read_hex16(tmp.path().to_str().unwrap()), 0x8086);
        std::fs::write(tmp.path(), "garbage").unwrap();
        assert_eq!(read_hex16(tmp.path().to_str().unwrap()), 0);
    }
}
