//! topology_macos.rs — CPU topology: SMT siblings and NUMA node grouping on macOS.
//!
//! Uses sysctl to query CPU topology information:
//! - `hw.logicalcpu`: number of logical CPUs
//! - `hw.physicalcpu`: number of physical CPUs (cores)
//! - `machdep.cpu.core_count`: number of physical cores
//!
//! macOS is typically UMA (no NUMA), so all CPUs are assigned to node 0.
//! SMT grouping is calculated based on logical vs physical CPU ratio.

use libc::{c_int, c_void, size_t, sysctlbyname};

use crate::topology::CpuTopology;

/// Read the topology from sysctl. Returns `Default` on any failure
/// so callers degrade gracefully (linear order, no separators).
pub(crate) fn read_topology() -> CpuTopology {
    // Get logical CPU count
    let logical_cpus = sysctl_u32(b"hw.logicalcpu\0").unwrap_or(1);
    if logical_cpus == 0 {
        return CpuTopology::default();
    }

    // Get physical CPU count (cores)
    let physical_cpus = sysctl_u32(b"hw.physicalcpu\0").unwrap_or(logical_cpus);

    // Calculate threads per core (SMT)
    let threads_per_core = logical_cpus.checked_div(physical_cpus).unwrap_or(1);

    // macOS is UMA, so all CPUs are in node 0
    let mut package = vec![0u32; logical_cpus as usize];
    let mut core_id = vec![0u32; logical_cpus as usize];
    let mut numa_node = vec![0u32; logical_cpus as usize];

    // Assign core IDs: group logical CPUs by physical core
    // For example, with 8 logical CPUs and 4 physical cores (2 threads per core):
    // - CPU 0,4 -> core 0
    // - CPU 1,5 -> core 1
    // - CPU 2,6 -> core 2
    // - CPU 3,7 -> core 3
    for (i, cid) in core_id.iter_mut().enumerate().take(logical_cpus as usize) {
        *cid = (i % physical_cpus as usize) as u32;
    }

    CpuTopology {
        package,
        core_id,
        numa_node,
    }
}

/// Read a u32 value from sysctlbyname.
fn sysctl_u32(name: &[u8]) -> Option<u32> {
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as size_t;

    let result = unsafe {
        sysctlbyname(
            name.as_ptr() as *const i8,
            &mut value as *mut u32 as *mut c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };

    if result == 0 && size == std::mem::size_of::<u32>() as size_t {
        Some(value)
    } else {
        None
    }
}
