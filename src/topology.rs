//! topology.rs — CPU topology: SMT siblings and NUMA node grouping.
//!
//! Reads `/sys/devices/system/cpu/cpuN/topology/` once per slow tick.
//! Used by the per-core spectrum to group SMT siblings adjacent and
//! insert NUMA-node boundary separators when more than one node is
//! present.

use std::fs;
use std::path::Path;

/// CPU topology snapshot.  Fields are indexed by logical CPU number
/// (`cpu0 → index 0`, etc.).  Defaults to an empty struct when sysfs
/// is unavailable; callers fall back to linear rendering in that case.
#[derive(Debug, Clone, Default)]
pub(crate) struct CpuTopology {
    /// `physical_package_id` for each logical CPU.
    pub(crate) package: Vec<u32>,
    /// `core_id` for each logical CPU (unique within a package).
    pub(crate) core_id: Vec<u32>,
    /// NUMA node index for each logical CPU (`0` on non-NUMA systems).
    pub(crate) numa_node: Vec<u32>,
}

impl CpuTopology {
    /// Read the topology from sysfs.  Returns `Default` on any failure
    /// so callers degrade gracefully (linear order, no separators).
    #[cfg(target_os = "linux")]
    pub(crate) fn read() -> Self {
        let Ok(entries) = fs::read_dir("/sys/devices/system/cpu") else {
            return Self::default();
        };

        let mut cpus: Vec<(usize, u32, u32, u32)> = Vec::new();

        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Only `cpuN` directories — skip `cpufreq`, `cpuidle`, etc.
            let Some(rest) = name.strip_prefix("cpu") else {
                continue;
            };
            let Ok(idx) = rest.parse::<usize>() else {
                continue;
            };

            let base = entry.path();
            let pkg = read_topology_u32(&base, "physical_package_id").unwrap_or(0);
            let cid = read_topology_u32(&base, "core_id")
                .unwrap_or_else(|| u32::try_from(idx).unwrap_or(u32::MAX));
            let numa = read_numa_node(&base).unwrap_or(0);
            cpus.push((idx, pkg, cid, numa));
        }

        if cpus.is_empty() {
            return Self::default();
        }

        let max_idx = cpus.iter().map(|(i, ..)| *i).max().unwrap_or(0);
        let n = max_idx + 1;
        let mut package = vec![0u32; n];
        let mut core_id = vec![0u32; n];
        let mut numa_node = vec![0u32; n];

        for (idx, pkg, cid, numa) in cpus {
            package[idx] = pkg;
            core_id[idx] = cid;
            numa_node[idx] = numa;
        }

        CpuTopology {
            package,
            core_id,
            numa_node,
        }
    }

    /// No-op on non-Linux: topology is Linux-sysfs-specific.
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn read() -> Self {
        Self::default()
    }

    /// Whether the topology carries useful grouping information.
    pub(crate) fn is_empty(&self) -> bool {
        self.core_id.is_empty()
    }

    /// Whether more than one NUMA node is present.
    pub(crate) fn is_numa(&self) -> bool {
        if self.numa_node.is_empty() {
            return false;
        }
        let first = self.numa_node[0];
        self.numa_node.iter().any(|&n| n != first)
    }

    /// Logical CPU count (number of entries in the topology).
    pub(crate) fn len(&self) -> usize {
        self.core_id.len()
    }

    /// Return logical CPU indices grouped by NUMA node, then by physical
    /// core (SMT siblings share a physical core and appear adjacent).
    ///
    /// Return value: `Vec<(numa_node, Vec<Vec<logical_cpu_idx>>)>`.
    /// The inner `Vec` is SMT groups (1 = single-threaded, 2 = HT pair,
    /// etc.) sorted by their smallest logical index.
    ///
    /// Returns an empty Vec when the topology has no data.
    pub(crate) fn numa_groups(&self) -> Vec<(u32, Vec<Vec<usize>>)> {
        if self.is_empty() {
            return Vec::new();
        }

        // Group logical CPUs by (numa, package, core_id).
        let mut map: std::collections::HashMap<(u32, u32, u32), Vec<usize>> =
            std::collections::HashMap::new();

        for i in 0..self.len() {
            let key = (
                self.numa_node.get(i).copied().unwrap_or(0),
                self.package.get(i).copied().unwrap_or(0),
                self.core_id
                    .get(i)
                    .copied()
                    .unwrap_or_else(|| u32::try_from(i).unwrap_or(u32::MAX)),
            );
            map.entry(key).or_default().push(i);
        }

        // Sort each SMT group internally.
        for v in map.values_mut() {
            v.sort_unstable();
        }

        // Flatten to a sortable Vec and sort by NUMA node, then first logical CPU.
        let mut groups: Vec<(u32, u32, u32, Vec<usize>)> = map
            .into_iter()
            .map(|((numa, pkg, cid), v)| (numa, pkg, cid, v))
            .collect();
        groups.sort_unstable_by_key(|(numa, _, _, v)| (*numa, v[0]));

        // Group by NUMA node.
        let mut result: Vec<(u32, Vec<Vec<usize>>)> = Vec::new();
        for (numa, _pkg, _cid, smt_group) in groups {
            if result.last().map(|(n, _)| *n) == Some(numa) {
                result.last_mut().unwrap().1.push(smt_group);
            } else {
                result.push((numa, vec![smt_group]));
            }
        }

        result
    }
}

fn read_topology_u32(cpu_path: &Path, field: &str) -> Option<u32> {
    let p = cpu_path.join("topology").join(field);
    fs::read_to_string(p).ok()?.trim().parse().ok()
}

fn read_numa_node(cpu_path: &Path) -> Option<u32> {
    // NUMA node appears as a `nodeN` symlink inside the cpu directory.
    let entries = fs::read_dir(cpu_path).ok()?;
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(idx_str) = name.strip_prefix("node") {
            if let Ok(n) = idx_str.parse::<u32>() {
                return Some(n);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_topology(cpus: &[(usize, u32, u32, u32)]) -> CpuTopology {
        let max_idx = cpus.iter().map(|(i, ..)| *i).max().unwrap_or(0);
        let n = max_idx + 1;
        let mut package = vec![0u32; n];
        let mut core_id = vec![0u32; n];
        let mut numa_node = vec![0u32; n];
        for &(idx, pkg, cid, numa) in cpus {
            package[idx] = pkg;
            core_id[idx] = cid;
            numa_node[idx] = numa;
        }
        CpuTopology {
            package,
            core_id,
            numa_node,
        }
    }

    #[test]
    fn empty_topology_has_no_groups() {
        let t = CpuTopology::default();
        assert!(t.is_empty());
        assert!(t.numa_groups().is_empty());
        assert!(!t.is_numa());
    }

    #[test]
    fn smt_siblings_grouped_together() {
        // 4 logical CPUs, 2 physical cores (HT), 1 package, 1 NUMA node.
        // cpu0+cpu2 share physical core 0; cpu1+cpu3 share physical core 1.
        let t = make_topology(&[(0, 0, 0, 0), (1, 0, 1, 0), (2, 0, 0, 0), (3, 0, 1, 0)]);
        assert!(!t.is_empty());
        assert!(!t.is_numa());
        let groups = t.numa_groups();
        assert_eq!(groups.len(), 1);
        let (node, smt_groups) = &groups[0];
        assert_eq!(*node, 0);
        // 2 physical cores → 2 SMT groups, each with 2 logical CPUs.
        assert_eq!(smt_groups.len(), 2);
        assert_eq!(smt_groups[0], vec![0, 2]);
        assert_eq!(smt_groups[1], vec![1, 3]);
    }

    #[test]
    fn single_threaded_cores_each_in_own_group() {
        // 4 logical CPUs, 4 physical cores (no HT).
        let t = make_topology(&[(0, 0, 0, 0), (1, 0, 1, 0), (2, 0, 2, 0), (3, 0, 3, 0)]);
        let groups = t.numa_groups();
        assert_eq!(groups.len(), 1);
        let (_, smt_groups) = &groups[0];
        assert_eq!(smt_groups.len(), 4);
        for (i, g) in smt_groups.iter().enumerate() {
            assert_eq!(g.len(), 1);
            assert_eq!(g[0], i);
        }
    }

    #[test]
    fn numa_multi_node_separates_correctly() {
        // 4 logical CPUs, 2 NUMA nodes (cpu0+cpu1 on node 0, cpu2+cpu3 on node 1).
        let t = make_topology(&[(0, 0, 0, 0), (1, 0, 1, 0), (2, 1, 0, 1), (3, 1, 1, 1)]);
        assert!(t.is_numa());
        let groups = t.numa_groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, 0);
        assert_eq!(groups[1].0, 1);
        assert_eq!(groups[0].1.len(), 2); // 2 physical cores on node 0
        assert_eq!(groups[1].1.len(), 2); // 2 physical cores on node 1
    }

    #[test]
    fn single_node_is_not_reported_as_numa() {
        let t = make_topology(&[(0, 0, 0, 0), (1, 0, 1, 0)]);
        assert!(!t.is_numa());
        let groups = t.numa_groups();
        assert_eq!(groups.len(), 1);
    }
}
