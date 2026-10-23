//! The process's memory footprint, the figure iOS uses to decide when to kill an app
//! for using too much memory (`phys_footprint` of `TASK_VM_INFO`).

use std::ffi::c_int;

/// `TASK_VM_INFO` from `<mach/task_info.h>`.
const TASK_VM_INFO: u32 = 22;

/// The leading part of `task_vm_info_data_t` (`TASK_VM_INFO_REV1`), up to and including
/// `phys_footprint`. The kernel fills as many 32-bit words as the count passed in says.
#[repr(C, packed(4))]
#[derive(Default)]
// Only `phys_footprint` is read; the other fields give it its offset.
#[allow(dead_code)]
struct TaskVmInfoRev1 {
    virtual_size: u64,
    region_count: i32,
    page_size: i32,
    resident_size: u64,
    resident_size_peak: u64,
    device: u64,
    device_peak: u64,
    internal: u64,
    internal_peak: u64,
    external: u64,
    external_peak: u64,
    reusable: u64,
    reusable_peak: u64,
    purgeable_volatile_pmap: u64,
    purgeable_volatile_resident: u64,
    purgeable_volatile_virtual: u64,
    compressed: u64,
    compressed_peak: u64,
    compressed_lifetime: u64,
    phys_footprint: u64,
}

// `TASK_VM_INFO_REV1_COUNT` is 38 words.
const _: () = assert!(size_of::<TaskVmInfoRev1>() == 38 * 4);

unsafe extern "C" {
    /// The current task's port, which `mach_task_self()` reads.
    static mach_task_self_: u32;
    fn task_info(task: u32, flavor: u32, info: *mut c_int, count: *mut u32) -> c_int;
}

/// The process's physical footprint in bytes, or `None` if the kernel refuses.
pub fn footprint() -> Option<u64> {
    let mut info = TaskVmInfoRev1::default();
    let words = (size_of::<TaskVmInfoRev1>() / size_of::<u32>()) as u32;
    let mut count = words;
    // SAFETY: `mach_task_self_` is set before any user code runs; `info` is writable for
    // `count` 32-bit words, and the kernel writes no more than that.
    let status = unsafe {
        task_info(
            mach_task_self_,
            TASK_VM_INFO,
            (&raw mut info).cast(),
            &mut count,
        )
    };
    // A kernel that fills fewer words has not written `phys_footprint`.
    (status == 0 && count >= words).then_some(info.phys_footprint)
}
