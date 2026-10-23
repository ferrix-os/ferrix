//! Walking the call chain, for a panic report.
//!
//! The kernel is built with frame pointers on every architecture, which
//! `.cargo/config.toml` argues at the flag. Each frame therefore begins with
//! the previous frame's pointer and the address the call will return to, and
//! that is enough to say who called what without the unwind tables a
//! freestanding binary does not carry.
//!
//! # Trusting nothing
//!
//! This runs after something has already gone wrong, possibly on a stack that
//! is itself the problem. So every step is checked: the chain has to climb,
//! stay within one stack's worth of where it started, stay aligned, and land
//! on mapped pages — read straight from the page tables rather than through
//! [`crate::mm::translate`], whose lock the panicking processor may be holding.
//! Anything else ends the walk. A backtrace that stops early is a fact about
//! the machine; a fault inside the panic handler is a report nobody sees.

use crate::mm;

// Where the link script puts the kernel image's first and last byte. Declared
// rather than defined: these have no value of their own, only an address.
unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

/// How many frames a report prints.
///
/// Enough for the deepest path in the kernel with room to spare, and short
/// enough that a chain that is somehow circular but passes every other check
/// still ends.
pub(crate) const MAX_FRAMES: usize = 24;

/// How far above its first frame the walk will follow the chain.
///
/// Four times the 16 KiB a task's stack gets, so a legitimate chain is never
/// cut short, and small enough that a wild pointer into the direct map is not
/// followed for long.
const MAX_SPAN: u64 = 64 * 1024;

/// Report the return address of each frame above `start`, innermost first,
/// and return how many were reported.
pub(crate) fn walk(start: u64, mut report: impl FnMut(u64)) -> usize {
    let word = u64::try_from(size_of::<usize>()).unwrap_or(8);
    let root = mm::root_table();
    let mut frame = start;
    let mut found = 0;

    while found < MAX_FRAMES {
        // Aligned, at or above where the walk began, and no further above it
        // than a stack can be deep.
        if !frame.is_multiple_of(word) || frame < start || frame.saturating_sub(start) > MAX_SPAN {
            break;
        }
        let (Some(previous), Some(address)) = (
            read_word(root, frame),
            read_word(root, frame.saturating_add(word)),
        ) else {
            break;
        };
        // A return address is an address in kernel code. Anything else means
        // the chain has left the frames the kernel built — past `_start`,
        // where the loader's frame is whatever firmware happened to leave.
        if !in_kernel(address) {
            break;
        }
        report(address);
        found = found.saturating_add(1);
        // Stacks grow down, so the caller's frame is above this one. Equal or
        // below means the chain is not one.
        if previous <= frame {
            break;
        }
        frame = previous;
    }

    found
}

/// Where the running image begins: its link address, or wherever the loader
/// moved it, since the code computes this as it computes every other address
/// in the image.
pub fn image_start() -> u64 {
    u64::try_from((&raw const __kernel_start).addr()).unwrap_or(u64::MAX)
}

/// Whether `address` is inside the kernel image, and so could be a return
/// address into kernel code.
fn in_kernel(address: u64) -> bool {
    let start = image_start();
    let end = u64::try_from((&raw const __kernel_end).addr()).unwrap_or(0);
    (start..end).contains(&address)
}

/// Read one machine word at `virt`, or `None` if nothing is mapped there or
/// what is mapped there is not RAM.
///
/// The two halves of a frame record are read separately because a record may
/// straddle a page boundary, and the second page can be missing where the
/// first is not.
///
/// A stack is RAM, and a word anywhere else ends the walk. The span a chain
/// may climb reaches past a stack's top into whatever the kernel arena put
/// beside it, a device's registers included, where a read is not a read of
/// memory: it can change the device's state, or never complete and hang the
/// panic that asked (F-55). Before memory is up nothing is recorded to ask,
/// and nothing but the early console and framebuffer is mapped as a device,
/// so a walk then takes any page.
fn read_word(root: u64, virt: u64) -> Option<u64> {
    let physical = mm::translate_in(root, virt)?;
    if mm::ram_recorded() && mm::direct_map_ram(physical).is_none() {
        return None;
    }
    let address = usize::try_from(virt).ok()? as *const usize;
    // SAFETY: (KMEM) a page is mapped at `virt`, the address is word-aligned by the
    // caller's check, and reading a word of stack has no side effects.
    let value = unsafe { address.read_volatile() };
    Some(value as u64)
}

/// Whether `address` is inside the kernel image as it runs: the test every
/// return address of a walk passes, for `sched::inspect`'s walk of another
/// task's chain (`docs/DEBUG.md` §1.1).
pub(crate) fn in_image(address: u64) -> bool {
    in_kernel(address)
}

/// How far the image was moved from where it was linked (KASLR), zero for an
/// image at its link address: what `sched::inspect` takes off each address so
/// that it names the ELF's, as the panic report's `kaslr slide` line lets
/// `xtask` do.
pub(crate) fn slide() -> u64 {
    image_start().wrapping_sub(ferrix_bootinfo::KERNEL_VIRT_BASE)
}

/// The image's length, from its first byte to its last.
pub(crate) fn image_len() -> u64 {
    u64::try_from((&raw const __kernel_end).addr())
        .unwrap_or(0)
        .saturating_sub(image_start())
}
