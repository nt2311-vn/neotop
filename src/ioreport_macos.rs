#![allow(clippy::missing_transmute_annotations, clippy::doc_lazy_continuation)]
//! ioreport_macos.rs — thin Rust wrapper around Apple's `IOReport`
//! private framework.
//!
//! `IOReport` lives in `/System/Library/PrivateFrameworks/IOReport.
//! framework/IOReport`. There's no public header and no Rust crate
//! that wraps it (every available macOS metrics tool — `macmon`,
//! `asitop`, `tegrastats`-style ports — reaches in here directly).
//! We load it lazily with `dlopen` so a `cargo build` on Linux still
//! cross-compiles cleanly and a fresh macOS that one day drops the
//! framework just degrades to "no Apple Silicon GPU busy%".
//!
//! The five symbols we need (signatures reverse-engineered from the
//! framework's binary plus matching macmon's bindings — kept on
//! one screen for clarity):
//!
//! ```c
//! CFMutableDictionaryRef IOReportCopyChannelsInGroup(
//!     CFStringRef group, CFStringRef subgroup,
//!     uint64_t a, uint64_t b, uint64_t c);
//! IOReportSubscriptionRef IOReportCreateSubscription(
//!     void *a, CFMutableDictionaryRef channels,
//!     CFMutableDictionaryRef *subbedChannels,
//!     uint64_t channelId, CFTypeRef b);
//! CFDictionaryRef IOReportCreateSamples(
//!     IOReportSubscriptionRef sub,
//!     CFMutableDictionaryRef channels, CFTypeRef a);
//! CFDictionaryRef IOReportCreateSamplesDelta(
//!     CFDictionaryRef prev, CFDictionaryRef cur, CFTypeRef a);
//! int   IOReportStateGetCount   (CFDictionaryRef chan);
//! int64_t IOReportStateGetResidency(CFDictionaryRef chan, int idx);
//! CFStringRef IOReportStateGetNameForIndex(CFDictionaryRef chan, int idx);
//! ```
//!
//! The output `samples` is a `CFDictionary` whose key
//! `"IOReportChannels"` maps to a `CFArray` of per-channel
//! dictionaries. To compute GPU busy% we filter for the channel
//! whose group/subgroup matches `"GPU Stats" / "GPU
//! Performance State"` and walk its state residency vector — state
//! 0 is `"IDLE_NS"`, the rest are active P-states. Busy% is the
//! ratio of non-idle residency over the delta window.

use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, TCFType, TCFTypeRef};
use core_foundation::dictionary::{CFDictionary, CFMutableDictionary};
use core_foundation::string::{CFString, CFStringRef};
use libc::{c_char, c_int, c_void, RTLD_LOCAL};
use std::ffi::CString;
use std::sync::OnceLock;

// `RTLD_NOW` isn't re-exported from `libc::` on Apple; the value
// is fixed at 2 by `<dlfcn.h>` on every BSD/macOS release.
const RTLD_NOW: c_int = 2;

/// Opaque IOReport subscription handle.
#[repr(transparent)]
#[derive(Clone, Copy)]
struct IoReportSubscriptionRef(*mut c_void);
// SAFETY: the subscription handle is a CF-style opaque pointer
// that's only touched by IOReport APIs; we hold one per process
// and never share it across threads.
unsafe impl Send for IoReportSubscriptionRef {}

#[allow(non_snake_case)]
struct Vtable {
    IOReportCopyChannelsInGroup: unsafe extern "C" fn(
        group: CFStringRef,
        subgroup: CFStringRef,
        a: u64,
        b: u64,
        c: u64,
    ) -> *mut c_void, // CFMutableDictionaryRef
    IOReportCreateSubscription: unsafe extern "C" fn(
        a: *mut c_void,
        channels: *mut c_void,             // CFMutableDictionaryRef
        subbed_channels: *mut *mut c_void, // CFMutableDictionaryRef*
        channel_id: u64,
        b: *const c_void,
    ) -> *mut c_void, // IOReportSubscriptionRef
    IOReportCreateSamples: unsafe extern "C" fn(
        sub: *mut c_void,
        channels: *const c_void,
        a: *const c_void,
    ) -> *mut c_void, // CFDictionaryRef
    IOReportCreateSamplesDelta: unsafe extern "C" fn(
        prev: *const c_void,
        cur: *const c_void,
        a: *const c_void,
    ) -> *mut c_void, // CFDictionaryRef
    IOReportStateGetCount: unsafe extern "C" fn(channel: *const c_void) -> c_int,
    IOReportStateGetResidency: unsafe extern "C" fn(channel: *const c_void, idx: c_int) -> i64,
    IOReportStateGetNameForIndex:
        unsafe extern "C" fn(channel: *const c_void, idx: c_int) -> CFStringRef,
    #[allow(dead_code)]
    IOReportChannelGetGroup: unsafe extern "C" fn(channel: *const c_void) -> CFStringRef,
    #[allow(dead_code)]
    IOReportChannelGetSubGroup: unsafe extern "C" fn(channel: *const c_void) -> CFStringRef,
}

/// Lazily-loaded IOReport function pointers. `None` means the
/// framework couldn't be dlopened or one of the symbols is
/// missing — caller falls back to "no data".
fn vtable() -> Option<&'static Vtable> {
    static VTABLE: OnceLock<Option<Vtable>> = OnceLock::new();
    VTABLE
        .get_or_init(|| {
            // SAFETY: each `dlsym` returns either a valid function
            // pointer in the loaded image or NULL on failure. We
            // check every result; missing symbols make us bail
            // before transmuting.
            unsafe {
                let path =
                    CString::new("/System/Library/PrivateFrameworks/IOReport.framework/IOReport")
                        .ok()?;
                let handle = libc::dlopen(path.as_ptr(), RTLD_NOW | RTLD_LOCAL);
                if handle.is_null() {
                    return None;
                }
                macro_rules! sym {
                    ($name:literal) => {{
                        let name = CString::new($name).ok()?;
                        let p = libc::dlsym(handle, name.as_ptr());
                        if p.is_null() {
                            return None;
                        }
                        std::mem::transmute(p)
                    }};
                }
                Some(Vtable {
                    IOReportCopyChannelsInGroup: sym!("IOReportCopyChannelsInGroup"),
                    IOReportCreateSubscription: sym!("IOReportCreateSubscription"),
                    IOReportCreateSamples: sym!("IOReportCreateSamples"),
                    IOReportCreateSamplesDelta: sym!("IOReportCreateSamplesDelta"),
                    IOReportStateGetCount: sym!("IOReportStateGetCount"),
                    IOReportStateGetResidency: sym!("IOReportStateGetResidency"),
                    IOReportStateGetNameForIndex: sym!("IOReportStateGetNameForIndex"),
                    IOReportChannelGetGroup: sym!("IOReportChannelGetGroup"),
                    IOReportChannelGetSubGroup: sym!("IOReportChannelGetSubGroup"),
                })
            }
        })
        .as_ref()
}

/// Persistent state for GPU busy% sampling: a subscription handle
/// + the previous CF-dictionary sample. Hold one for the lifetime
/// of the process — the framework charges for the initial subscribe,
/// not for `IOReportCreateSamples` calls.
pub(crate) struct GpuBusySampler {
    sub: IoReportSubscriptionRef,
    channels: CFMutableDictionary,
    prev_sample: Option<CFType>,
}

// SAFETY: every field is either a CF retain we own exclusively or
// an opaque IOReport handle that's safe to move across threads.
unsafe impl Send for GpuBusySampler {}

impl GpuBusySampler {
    /// Initialise a subscription for `GPU Stats / GPU Performance
    /// State`. Returns `None` when IOReport isn't loadable or no
    /// matching channel exists (e.g. Intel-only Mac with a discrete
    /// GPU, where the equivalent data lives in
    /// `PerformanceStatistics` instead).
    pub(crate) fn new() -> Option<Self> {
        let vt = vtable()?;
        // SAFETY: standard `IOReportCopyChannelsInGroup` invocation
        // followed by `IOReportCreateSubscription`. Both return
        // retained CF objects we wrap immediately so the Drop
        // impls handle release.
        unsafe {
            let group = CFString::from_static_string("GPU Stats");
            let subgroup = CFString::from_static_string("GPU Performance State");
            let raw = (vt.IOReportCopyChannelsInGroup)(
                group.as_concrete_TypeRef(),
                subgroup.as_concrete_TypeRef(),
                0,
                0,
                0,
            );
            if raw.is_null() {
                return None;
            }
            let channels: CFMutableDictionary =
                CFMutableDictionary::wrap_under_create_rule(raw.cast());

            let mut subbed: *mut c_void = std::ptr::null_mut();
            let sub_raw = (vt.IOReportCreateSubscription)(
                std::ptr::null_mut(),
                channels.as_concrete_TypeRef().cast::<c_void>(),
                std::ptr::addr_of_mut!(subbed),
                0,
                std::ptr::null(),
            );
            if sub_raw.is_null() {
                return None;
            }
            // `subbed` is the kernel's notion of what we actually
            // subscribed to — usually identical to `channels` but
            // safer to use for subsequent sample calls.
            let effective_channels = if subbed.is_null() {
                channels
            } else {
                CFMutableDictionary::wrap_under_create_rule(subbed.cast())
            };
            Some(Self {
                sub: IoReportSubscriptionRef(sub_raw),
                channels: effective_channels,
                prev_sample: None,
            })
        }
    }

    /// Take one sample. Returns the busy fraction in `0.0..=100.0`
    /// once two consecutive samples are available; first call
    /// always returns `None` because there's no delta window yet.
    pub(crate) fn sample(&mut self) -> Option<f64> {
        let vt = vtable()?;
        // SAFETY: `self.sub` is a live subscription handle; the
        // channels dict is the one the framework handed us back at
        // subscribe time. `IOReportCreateSamples` returns a +1
        // retained dictionary that we wrap into a `CFType` so Drop
        // releases it whether we keep it as `prev_sample` or not.
        unsafe {
            let cur_raw = (vt.IOReportCreateSamples)(
                self.sub.0,
                self.channels
                    .as_concrete_TypeRef()
                    .cast::<c_void>()
                    .cast_const(),
                std::ptr::null(),
            );
            if cur_raw.is_null() {
                return None;
            }
            let cur = CFType::wrap_under_create_rule(cur_raw.cast());

            // First sample: stash it, no delta possible yet.
            let Some(prev) = self.prev_sample.take() else {
                self.prev_sample = Some(cur);
                return None;
            };

            let delta_raw = (vt.IOReportCreateSamplesDelta)(
                prev.as_concrete_TypeRef().cast::<c_void>(),
                cur.as_concrete_TypeRef().cast::<c_void>(),
                std::ptr::null(),
            );
            // Whatever the delta call returns, we want `cur` to
            // become the new baseline for next tick.
            self.prev_sample = Some(cur);
            if delta_raw.is_null() {
                return None;
            }
            let delta = CFType::wrap_under_create_rule(delta_raw.cast());

            compute_busy_from_delta(&delta, vt)
        }
    }
}

impl Drop for GpuBusySampler {
    fn drop(&mut self) {
        // CFTypes hold their own retain counts; the subscription
        // ref isn't a CFType so we leak it on shutdown — same
        // behaviour as macmon. Process exit reclaims it.
    }
}

/// Walk the delta dictionary's `IOReportChannels` array, sum
/// residencies per channel, and convert to a busy percentage.
/// "Idle" is recognised by its state name starting with `IDLE`
/// (Apple's convention across M1/M2/M3 generations).
fn compute_busy_from_delta(delta: &CFType, vt: &Vtable) -> Option<f64> {
    // SAFETY: `delta` is a CFDictionary that contains an "IOReportChannels" key
    // mapping to a CFArray of channel dicts. The CF Get-rule wrappers below
    // never retain ownership beyond their lexical scope.
    unsafe {
        let key = CFString::from_static_string("IOReportChannels");
        let dict: CFDictionary =
            CFDictionary::wrap_under_get_rule(delta.as_concrete_TypeRef().cast());
        let arr_ptr: *const c_void = dict
            .find(key.as_concrete_TypeRef().cast::<c_void>())
            .map(|v| *v)?;
        if arr_ptr.is_null() {
            return None;
        }
        let arr: CFArray<CFType> = CFArray::wrap_under_get_rule(arr_ptr as CFArrayRef);

        let mut idle_ns: i64 = 0;
        let mut active_ns: i64 = 0;

        for chan in arr.iter() {
            let chan_ptr = chan.as_concrete_TypeRef().cast::<c_void>();
            let count = (vt.IOReportStateGetCount)(chan_ptr);
            if count <= 0 {
                continue;
            }
            for i in 0..count {
                let res = (vt.IOReportStateGetResidency)(chan_ptr, i);
                if res < 0 {
                    continue;
                }
                let name_ref = (vt.IOReportStateGetNameForIndex)(chan_ptr, i);
                let is_idle = if name_ref.is_null() {
                    i == 0 // by Apple convention state 0 = IDLE
                } else {
                    let name = CFString::wrap_under_get_rule(name_ref);
                    let s = name.to_string();
                    s.starts_with("IDLE") || s == "OFF"
                };
                if is_idle {
                    idle_ns = idle_ns.saturating_add(res);
                } else {
                    active_ns = active_ns.saturating_add(res);
                }
            }
        }

        let total = idle_ns.saturating_add(active_ns);
        if total <= 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        let busy = (active_ns as f64 / total as f64) * 100.0;
        Some(busy.clamp(0.0, 100.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live smoke test — only meaningful on real Apple Silicon
    /// hardware. Run with:
    ///
    /// ```sh
    /// cargo test --target aarch64-apple-darwin \
    ///   -- --ignored ioreport_gpu_busy_smoke --nocapture
    /// ```
    #[test]
    #[ignore = "requires real macOS hardware"]
    fn ioreport_gpu_busy_smoke() {
        let mut s = match GpuBusySampler::new() {
            Some(s) => s,
            None => {
                eprintln!("IOReport not loadable on this host — skipping");
                return;
            }
        };
        assert!(s.sample().is_none(), "first sample yields no delta yet");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let v = s.sample();
        eprintln!("GPU busy = {v:?}");
        if let Some(b) = v {
            assert!((0.0..=100.0).contains(&b));
        }
    }
}
