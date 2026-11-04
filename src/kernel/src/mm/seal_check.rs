//! K16's seal holds itself to its rule, at each boot of a `kernel_memory=retyped` kernel
//! (`docs/microkernel/K16.md` §5).

use core::alloc::GlobalAlloc;

use crate::object::job::Job;
use crate::object::quota;

/// Verifies: H.MEM.32
/// Verifies: L.object.336
/// Verifies: L.object.337
/// Verifies: L.object.338
/// Verifies: L.object.340
/// What the seal is held to, on the boot's own thread, with `promised` a job with a
/// promise and `bare` one with none, neither holding anything. Answers what failed.
pub fn check(promised: &Job, bare: &Job) -> Result<(), &'static str> {
    // The allocator itself, not `alloc::alloc`: the compiler may drop a pair of calls
    // to that, answer "not null" with no allocation, and so pass or fail this by how it
    // inlined (a boot in eight once read a planted request as served).
    let alloc = |layout| {
        // SAFETY: (KMEM) a plain layout of a non-zero size.
        unsafe { GlobalAlloc::alloc(&super::KernelAllocator, layout) }
    };
    let dealloc = |pointer, layout| {
        // SAFETY: (KMEM) what `alloc` just above answered, for this layout.
        unsafe { GlobalAlloc::dealloc(&super::KernelAllocator, pointer, layout) };
    };
    let small = core::alloc::Layout::new::<[u8; 64]>();
    // A job with no promise is refused, the pool being the kernel's (L.object.336).
    let refused = quota::charging_to(bare.quota_index(), || {
        let planted = alloc(small);
        if planted.is_null() {
            return true;
        }
        dealloc(planted, small);
        false
    });
    if !refused {
        return Err("a planted allocation after the seal was served");
    }
    // One with a promise draws on that, as a wrong charge target would not (L.object.337).
    let entry = quota::charging_to(promised.quota_index(), || {
        let object = alloc(small);
        if object.is_null() {
            return super::NO_RESERVATION;
        }
        let entry = super::heap_drawn_from(object as u64);
        dealloc(object, small);
        entry
    });
    if entry != promised.draws_from() {
        return Err("an object on another job's page");
    }
    // A task with no job pays nothing to whoever the processor last ran (K16.4).
    let drawn = crate::sched::with_running_group_for_check(promised.quota_index(), || {
        let object = alloc(small);
        if object.is_null() {
            return super::NO_RESERVATION;
        }
        let entry = super::heap_drawn_from(object as u64);
        dealloc(object, small);
        entry
    });
    if drawn != super::seal::pool() {
        return Err("a kernel task was charged to whoever was running");
    }
    // A frame goes back to untyped with nothing of its job's on it (K16.8).
    let large = core::alloc::Layout::new::<[u8; 8192]>();
    let kept = quota::charging_to(promised.quota_index(), || {
        let frames = alloc(large);
        if frames.is_null() {
            return true;
        }
        // SAFETY: (KMEM) `frames` is 8192 bytes of this call's.
        unsafe { frames.write_bytes(0xA5, 8192) };
        dealloc(frames, large);
        // SAFETY: (KMEM) the direct map's, so that its address reads after it is given back;
        // another's use of it is not this pattern.
        unsafe { frames.cast::<u64>().read_volatile() == 0xA5A5_A5A5_A5A5_A5A5 }
    });
    if kept {
        return Err("a retyped frame went back with its bytes");
    }
    Ok(())
}
