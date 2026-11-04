//! The kernel's restricted space as a cache of the shadow
//! (docs/microkernel/M5-LIFT.md §3): `unmap` keeps it from outliving the shadow;
//! a fault fills it. Which root is which space is `crate::arch::prepare_user_root`'s.

use alloc::collections::BTreeMap;

use crate::sync::SpinLock;

/// A shadow's root to the kernel's space handle, once it has one.
static SPACES: SpinLock<BTreeMap<u64, u32>> = SpinLock::new(BTreeMap::new());

pub fn bind(root: u64, space: u32) {
    let _ = SPACES.lock().insert(root, space);
}
pub fn space_of(root: u64) -> Option<u32> {
    SPACES.lock().get(&root).copied()
}
/// A root frame used again: it names no kernel space now. The handle stays until the
/// kernel process it was bound with has lost its last thread, which may be later: a
/// space's root goes when the program's memory does, before its last thread is gone.
pub fn forget(root: u64) {
    let _ = SPACES.lock().remove(&root);
}
/// Called with the shadow held. A kernel that still maps what the shadow has
/// let go is a program reaching memory about to be someone else's: the partition
/// ends (6012 (R2)).
pub fn unmap(root: u64, virt: u64, len: u64) {
    if let Some(space) = space_of(root)
        && !crate::sys::space_unmap(space, virt, len)
    {
        panic!("linuxd: SPACE_UNMAP refused {virt:#x}+{len:#x}");
    }
}

const PAGE: u64 = 4096;

/// A neighbour the kernel would not take. None is expected, the shadow having said yes: it
/// is said, and its own fault still fills it (ledger 6537, A4).
fn note_short(pages: usize) {
    use core::sync::atomic::{AtomicU64, Ordering};
    static SHORT: AtomicU64 = AtomicU64::new(0);
    if SHORT.fetch_add(pages as u64, Ordering::Relaxed) == 0 {
        crate::println!("linuxd: the kernel took {pages} of a fault's neighbours short");
    }
}

/// Each kernel process, by its handle, unique while it is open: how many threads it has
/// here, its restricted space's handle, and the root that was bound under. Not by root:
/// a root frame can be another process's before this one's last thread has ended.
struct KernelProcess {
    handle: u32,
    threads: u32,
    space: u32,
    root: u64,
}
static KERNEL_PROCESSES: SpinLock<alloc::vec::Vec<KernelProcess>> =
    SpinLock::new(alloc::vec::Vec::new());
pub fn note_kernel_process(root: u64, handle: u32, space: u32) {
    KERNEL_PROCESSES.lock().push(KernelProcess {
        handle,
        threads: 1,
        space,
        root,
    });
}
pub fn kernel_process_of(process: &crate::object::process::Process) -> Option<u32> {
    let root = process.own_space().root_table();
    let space = space_of(root)?;
    KERNEL_PROCESSES
        .lock()
        .iter()
        .find(|entry| entry.space == space)
        .map(|entry| entry.handle)
}
/// The kernel process `root`'s space is bound with, a new thread of it about to start.
pub fn kernel_process_thread(root: u64) -> Option<u32> {
    let space = space_of(root)?;
    let mut list = KERNEL_PROCESSES.lock();
    let entry = list.iter_mut().find(|entry| entry.space == space)?;
    entry.threads += 1;
    Some(entry.handle)
}
/// A thread of it has ended; with its last, its handles go too, or a machine that
/// runs many programs runs out of them.
pub fn kernel_process_thread_gone(handle: u32) {
    let mut list = KERNEL_PROCESSES.lock();
    let Some(at) = list.iter().position(|entry| entry.handle == handle) else {
        return;
    };
    list[at].threads -= 1;
    if list[at].threads == 0 {
        let entry = list.swap_remove(at);
        drop(list);
        {
            let mut spaces = SPACES.lock();
            if spaces.get(&entry.root) == Some(&entry.space) {
                let _ = spaces.remove(&entry.root);
            }
        }
        crate::sys::handle_close(entry.handle);
        crate::sys::handle_close(entry.space);
    }
}

/// One lock per shadow, by its root's hash: held across every change of a shadow
/// and the kernel's call that goes with it, and across a fill's read of the shadow
/// and its `SPACE_MAP`. So a fill can never map a frame a thread has just unmapped
/// and freed: the two orders are the only two (M5-LIFT.md §10, ledger 6012 (R1)).
const SHARDS: usize = 64;
static SHADOW_LOCKS: [SpinLock<()>; SHARDS] = [const { SpinLock::new(()) }; SHARDS];

/// Run `change` with `root`'s shadow held.
pub fn with_shadow<R>(root: u64, change: impl FnOnce() -> R) -> R {
    let at = (root / PAGE) as usize % SHARDS;
    let _held = SHADOW_LOCKS.get(at).map(SpinLock::lock);
    change()
}

/// A program's fault: answers whether it may go on from the instruction. A fill
/// the kernel refuses is the program's signal, never a retry (6012 (R2)).
pub fn resolve(
    address: u64,
    access: crate::user::space::Access,
) -> Result<(), Option<crate::user::space::SpaceError>> {
    let Some(task) = crate::sched::current() else {
        return Err(None);
    };
    let Some(process) = task.core_process() else {
        return Err(None);
    };
    let space = process.own_space();
    let root = space.root_table();
    let page = address & !(PAGE - 1);
    let filled = fill(root, page, access);
    if filled == Some(true) {
        return Ok(());
    }
    crate::object::oom::user_fault(space, address, access).map_err(Some)?;
    // The fault was the load's to answer, and it did. What another thread did since, an
    // unmap or a MADV_DONTNEED, is another fault, as it is in the hybrid: back to the
    // instruction either way. A refused fill is the one answer that ends it.
    let (around, count) = if !cfg!(ferrix_fault_ahead = "on") {
        ([0; crate::user::space::AROUND_PAGES as usize], 0)
    } else if access.write {
        space.write_ahead(address)
    } else {
        space.fault_around(address)
    };
    if fill_with(root, page, access, &around[..count]) != Some(false) {
        Ok(())
    } else {
        Err(None)
    }
}

/// `Some(true)` it is in the kernel's space now; `Some(false)` the kernel
/// refused; `None` the shadow does not allow it.
fn fill(root: u64, page: u64, access: crate::user::space::Access) -> Option<bool> {
    fill_with(root, page, access, &[])
}

/// [`fill`], with `around` behind it in one batch: pages the shadow has just gained, so the
/// kernel's space has none of them and nothing is replaced. Everything is read and placed
/// under one hold of the shadow (6012 R1). Only the first entry's answer counts: a
/// neighbour's refusal is never the program's signal. A neighbour goes only with its
/// own rights, of the first's class, and a driver's page never goes ahead, since placing
/// makes its VMO unchangeable for good.
fn fill_with(
    root: u64,
    page: u64,
    access: crate::user::space::Access,
    around: &[u64],
) -> Option<bool> {
    with_shadow(root, || {
        let flags = crate::mm::flags_in(root, page)?;
        let allowed =
            flags.user && (!access.write || flags.write) && (!access.execute || flags.execute);
        if !allowed {
            return None;
        }
        let physical = crate::mm::translate_in(root, page)?;
        let kernel = space_of(root)?;
        // A page a program made writable and executable is one or the other at
        // a time here, by what it just asked: K2 never maps both (ASR-2).
        let (write, execute) = match (flags.write, flags.execute) {
            (true, true) if access.write => (true, false),
            (true, true) => (false, true),
            other => other,
        };
        if around.is_empty() || crate::mm::is_foreign(physical / PAGE) {
            return Some(crate::sys::space_fill(
                kernel, page, physical, write, execute,
            ));
        }
        let mut entries =
            [(page, physical, write, execute); 1 + crate::user::space::AROUND_PAGES as usize];
        let mut count = 1;
        for &at in around {
            let (Some(flags), Some(neighbour)) = (
                crate::mm::flags_in(root, at),
                crate::mm::translate_in(root, at),
            ) else {
                continue;
            };
            if !flags.user
                || (flags.write && flags.execute)
                || flags.execute != execute
                || crate::mm::is_foreign(neighbour / PAGE)
            {
                continue;
            }
            if let Some(entry) = entries.get_mut(count) {
                *entry = (at, neighbour, flags.write, flags.execute);
                count += 1;
            }
        }
        let handle = if execute {
            crate::sys::arena::executable_handle()
        } else {
            crate::sys::arena::writable_handle()
        };
        let done = crate::sys::place(kernel, handle, &entries[..count]);
        if done != count {
            note_short(count - done);
        }
        Some(done >= 1)
    })
}
