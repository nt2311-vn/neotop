//! container_macos.rs — container detection on macOS.
//!
//! macOS doesn't have `/proc/<pid>/cgroup`. Instead, we detect containers by:
//! 1. Process tree analysis using libproc's `proc_listallpids()`
//! 2. Checking if a process is a child of known container runtime processes
//!    (docker, podman, containerd, containerd-shim)
//!
//! This is a best-effort detection: macOS containers run in Linux VMs,
//! so we're detecting the host-side proxy processes, not the actual
//! container processes inside the VM.

use libc::{c_char, c_int, c_void, pid_t};
use std::collections::HashMap;
use std::ffi::CStr;
use std::path::Path;
use std::time::Instant;

use crate::groups::{Container, ContainerRuntime};

const RUNTIME_BASENAMES: &[&str] = &[
    "com.docker.backend",
    "com.docker.virtualization",
    "com.docker.vmnetd",
    "docker",
    "dockerd",
    "podman",
    "podman-mac-helper",
    "containerd",
    "containerd-shim",
    "containerd-shim-runc-v2",
];

fn classify_runtime_basename(name: &str) -> Option<ContainerRuntime> {
    if !RUNTIME_BASENAMES.contains(&name) {
        return None;
    }
    match name {
        "com.docker.backend"
        | "com.docker.virtualization"
        | "com.docker.vmnetd"
        | "docker"
        | "dockerd" => Some(ContainerRuntime::Docker),
        "podman" | "podman-mac-helper" => Some(ContainerRuntime::Podman),
        "containerd" | "containerd-shim" | "containerd-shim-runc-v2" => {
            Some(ContainerRuntime::Containerd)
        }
        _ => None,
    }
}

/// Cache of parent PIDs to avoid repeated syscalls
#[derive(Debug, Default)]
pub(crate) struct ContainerDetector {
    parent_cache: HashMap<pid_t, pid_t>,
    runtime_pids: Vec<pid_t>,
    last_check: Option<Instant>,
}

impl ContainerDetector {
    /// Create a new container detector
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Refresh the list of runtime PIDs (docker, podman, containerd)
    pub(crate) fn refresh_runtime_pids(&mut self) {
        let now = Instant::now();
        // Refresh every 5 seconds to avoid excessive syscalls
        if let Some(last) = self.last_check {
            if now.duration_since(last) < std::time::Duration::from_secs(5) {
                return;
            }
        }
        self.last_check = Some(now);
        self.runtime_pids = self.find_runtime_pids();
    }

    /// Check if a process belongs to a container by walking the process tree
    pub(crate) fn detect_container(&mut self, pid: pid_t) -> Option<Container> {
        self.refresh_runtime_pids();

        // Walk up the process tree looking for runtime processes
        let mut current_pid = pid;
        let mut depth = 0;
        const MAX_DEPTH: u32 = 10; // Don't walk too far up

        while depth < MAX_DEPTH {
            if let Some(parent_pid) = self.get_parent_pid(current_pid) {
                if parent_pid == 0 {
                    break; // Reached root
                }

                // Check if this parent is a runtime process
                if let Some(runtime) = self.check_runtime(parent_pid) {
                    // Extract container ID from the process name or path
                    if let Some(container_id) = self.extract_container_id(current_pid, runtime) {
                        return Some(Container {
                            runtime,
                            id: container_id,
                        });
                    }
                }

                current_pid = parent_pid;
                depth += 1;
            } else {
                break;
            }
        }

        None
    }

    /// Get the parent PID of a process with caching
    fn get_parent_pid(&mut self, pid: pid_t) -> Option<pid_t> {
        if let Some(&cached) = self.parent_cache.get(&pid) {
            return Some(cached);
        }

        let parent_pid = self.get_parent_pid_syscall(pid)?;
        self.parent_cache.insert(pid, parent_pid);
        Some(parent_pid)
    }

    /// Get parent PID via proc_pidinfo (PROC_PIDTBSDINFO)
    fn get_parent_pid_syscall(&self, pid: pid_t) -> Option<pid_t> {
        let mut binfo: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let bsize = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let result = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                std::ptr::addr_of_mut!(binfo).cast::<c_void>(),
                bsize,
            )
        };

        if result > 0 {
            Some(binfo.pbi_ppid as pid_t)
        } else {
            None
        }
    }

    /// Find PIDs of known container runtime processes
    fn find_runtime_pids(&self) -> Vec<pid_t> {
        let mut pids = Vec::new();

        // Get all PIDs using proc_listallpids
        let mut pid_buffer: Vec<pid_t> = vec![0; 4096];
        let count = unsafe {
            libc::proc_listallpids(
                pid_buffer.as_mut_ptr().cast::<c_void>(),
                (pid_buffer.len() * std::mem::size_of::<pid_t>()) as i32,
            )
        };

        if count <= 0 {
            return pids;
        }

        let count = count as usize;
        for &pid in pid_buffer.iter().take(count) {
            if pid <= 0 {
                continue;
            }
            if let Some(name) = self.get_proc_name(pid) {
                if classify_runtime_basename(&name).is_some() {
                    pids.push(pid);
                }
            }
        }

        pids
    }

    /// Get the name of a process
    fn get_proc_name(&self, pid: pid_t) -> Option<String> {
        let mut path = [0i8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let result = unsafe {
            libc::proc_pidpath(
                pid,
                path.as_mut_ptr().cast::<c_void>(),
                libc::PROC_PIDPATHINFO_MAXSIZE as u32,
            )
        };

        if result <= 0 {
            return None;
        }

        let path_str = unsafe { CStr::from_ptr(path.as_ptr()) }.to_str().ok()?;
        Path::new(path_str)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
    }

    /// Check if a PID is a container runtime and return the runtime type
    fn check_runtime(&self, pid: pid_t) -> Option<ContainerRuntime> {
        let name = self.get_proc_name(pid)?;
        classify_runtime_basename(&name)
    }

    /// Extract container ID from a process
    fn extract_container_id(&self, pid: pid_t, runtime: ContainerRuntime) -> Option<String> {
        if let Some(name) = self.get_proc_name(pid) {
            // Try to extract a container-like ID from the process name
            // This is a heuristic - real container IDs would require
            // querying the Docker Desktop socket
            if name.len() >= 12 {
                // Take first 12 chars as a pseudo-ID
                return Some(name.chars().take(12).collect());
            }
        }

        // Fallback: use PID as ID (not ideal but functional)
        Some(format!("{pid:x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_desktop_helper_renderer_is_not_runtime() {
        assert_eq!(
            classify_runtime_basename("Docker Desktop Helper (Renderer)"),
            None
        );
    }

    #[test]
    fn docker_desktop_app_is_not_runtime() {
        assert_eq!(classify_runtime_basename("Docker Desktop"), None);
    }

    #[test]
    fn com_docker_backend_is_docker_runtime() {
        assert_eq!(
            classify_runtime_basename("com.docker.backend"),
            Some(ContainerRuntime::Docker)
        );
    }

    #[test]
    fn containerd_shim_is_containerd_runtime() {
        assert_eq!(
            classify_runtime_basename("containerd-shim-runc-v2"),
            Some(ContainerRuntime::Containerd)
        );
    }
}
