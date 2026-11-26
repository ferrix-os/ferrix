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
    check_arena_refusal()
}

/// Frames another processor may give back between linuxd's promise and the check of
/// it, which would let the admission grant a little more: the check asks for this
/// many past what was admitted, so it judges "not less" and does not flake.
const PROMISE_SLACK: u64 = 256;

/// linuxd's promise is taken once the initramfs is given back, as large as the
/// reservation's own admission allows then, and its arena is made from it
/// (`docs/microkernel/K16.md`, R.2's sizing; ledger 10128 Y1). Asked by
/// `linuxd_start` on every checked boot of a `kernel_memory=retyped` kernel, with
/// whether the initramfs was still held when the promise was taken, what the
/// admission allowed and the arena it made; it stops the boot by name.
///
/// Verifies: L.object.343
pub fn check_linuxd_promise(job: &Job, held_at_promise: bool, arena_admitted: (u64, u64)) {
    let (admitted, arena) = arena_admitted;
    if held_at_promise {
        crate::panic::fatal!(
            crate::panic::catalog::RETYPED,
            "linuxd's promise was taken while the initramfs was held"
        );
    }
    // The admission's own body once more, not a copy of its sum: a promise that took
    // all it could is refused this much more.
    let entry = job.draws_from();
    if !matches!(
        super::reservation_resize(entry, admitted + PROMISE_SLACK),
        Err(super::Refused::Short(_))
    ) {
        crate::panic::fatal!(
            crate::panic::catalog::RETYPED,
            "linuxd's promise was short of what the admission allowed"
        );
    }
    let (size, _) = super::reservation_usage(entry);
    if arena != size / 8 * 7 {
        crate::panic::fatal!(
            crate::panic::catalog::RETYPED,
            "linuxd's arena was not made from its promise"
        );
    }
}

/// An arena under what linuxd's root and programs need is refused, and one of
/// exactly that is not: the figures `linuxd_start` refuses a machine by (R.5).
///
/// Verifies: L.object.345
pub(crate) fn check_arena_refusal() -> Result<(), &'static str> {
    use super::seal::{arena_needed, arena_refused};
    const ROOT: u64 = 100 << 20;
    let needed = arena_needed(ROOT);
    if !arena_refused(needed - 1, ROOT) || arena_refused(needed, ROOT) {
        return Err("an arena was not refused at exactly what linuxd's root and programs need");
    }
    if needed <= ROOT >> 12 {
        return Err("the arena linuxd needs did not count its root");
    }
    Ok(())
}
