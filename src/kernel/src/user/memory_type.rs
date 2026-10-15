//! One memory type per page of device memory, across every user mapping.
//!
//! A device page reaches ring 3 three ways: `io_mapping_map` maps an
//! aperture as device memory (uncacheable), `io_mapping_map_combining` maps a
//! prefetchable one write-combining, and `mmap` of a render node's blob maps
//! a window of the device's memory cacheable when the device says it may be
//! cached (`syscall::memory::map_window`). Two of those over one page at once
//! are an alias of two memory types. The SDM (Vol. 3A §11.12.4) forbids a
//! write-combining page aliasing a cacheable one, because write-combining
//! stores need not snoop the caches, and the consequence can be a machine
//! check that is not confined to the device; on Arm, Normal non-cacheable
//! beside Normal write-back is a mismatched-attribute alias that loses
//! coherence. Uncacheable beside write-combining is not forbidden, but it
//! is not needed either, and one rule is easier to hold than two.
//!
//! So every user mapping of device memory holds a [`Hold`] on its physical
//! range for as long as any region of any address space maps it, and a
//! mapping whose type differs from a live hold's on any page of its range is
//! refused. The hold is kept beside the region's id, as a window's keeper
//! is, and dropped only after the unmap's shootdown has returned
//! (`AddressSpace::give_back`), so no processor can still hold a translation
//! of the old type when a mapping of the new one is allowed.
//!
//! The kernel's own mappings are not here, and need not be, because none of
//! them is of a page a user mapping can reach -- with one exception, which
//! holds a type here for good. Its direct map covers RAM runs only
//! (`mm::direct_map_ram`), and no aperture or window is RAM. The boot
//! framebuffer, which it maps itself for the boot console and the panic
//! screen (`arch::FRAMEBUFFER_FLAGS`), is in `device::Reserved` with the
//! interrupt controllers, timers and firmware's own memory it maps, and an
//! aperture overlapping anything reserved is withheld rather than minted
//! (`device.rs`). A PCI function's MSI-X table and pending-bit pages, which
//! the kernel maps to mint vectors, are cut out of the function's apertures
//! (`msix::withheld`). Configuration space is not a BAR and is never an
//! aperture.
//!
//! The exception: under `ferrix.efifb`, on x86-64, the boot framebuffer's
//! visible lines are lent to a node of their own (`discovery::firmware`,
//! `docs/DISPLAY.md` §8), so a user mapping can reach pages the kernel maps
//! for its panic screen. Before the node is published the kernel takes a
//! [`Hold`] of its own mapping's type over them, [`MemoryType::Device`],
//! and never drops it: every user mapping of the window is then held to
//! that type, and one of any other type is refused for the life of the
//! machine (L.device.39, checked by stage 10's `check_framebuffer`).
//!
//! # A residual: cache lines across a type change
//!
//! The hold orders translations -- it goes only after the unmap's
//! shootdown -- but not caches. After the last region mapping a window
//! cached is gone, dirty write-back lines of its device pages may still be
//! in a processor's cache, and may be written back after a later
//! write-combining or uncached mapping of the same pages has written them
//! (SDM Vol. 3A 11.11.9 and 11.12.4 ask for a flush on a type change). No
//! two types are ever mapped at once, so this is no simultaneous alias and
//! has no machine-wide effect: the worst is stale data written over the
//! device's own memory, the memory of the device whose driver and clients
//! made both mappings. It is recorded, not flushed (VULNERABILITY-ANALYSIS
//! T.DMA, MEMORY-AND-TIMING 2.2f); flushing a cached range as its hold is
//! released would close it.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::fallible;
use crate::sync::SpinLock;

/// How a processor maps a page of device memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MemoryType {
    /// Device memory, uncacheable: an aperture of registers, or a window the
    /// device did not say may be cached.
    Device,
    /// Write-combining: a prefetchable aperture a driver asked to combine.
    Combining,
    /// Ordinary cacheable memory: a window the device said may be cached.
    Cached,
}

impl MemoryType {
    /// The type a device region with these attributes is mapped with: what
    /// `AddressSpace::fault_device` puts in the page table.
    pub(crate) const fn of(cached: bool, combining: bool) -> MemoryType {
        if cached {
            MemoryType::Cached
        } else if combining {
            MemoryType::Combining
        } else {
            MemoryType::Device
        }
    }
}

/// Why a hold was not given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Refused {
    /// A page of the range is mapped with another type now.
    OtherType,
    /// There was no memory to record it.
    NoMemory,
}

/// One range held as one type, by `count` live mappings.
#[derive(Clone, Copy, Debug)]
struct Claim {
    start: u64,
    end: u64,
    kind: MemoryType,
    /// The [`Hold`]s alive for it: never zero while the entry is here.
    count: u64,
}

/// Every range held. A leaf lock: taken under an address space's lock by a
/// mapping, and on its own when a hold drops; nothing is taken inside it.
///
/// One entry per distinct range and type, counted, never one per mapping:
/// mapping the same range again, from any number of processes, raises a
/// count and adds nothing. The ranges are not the program's choice: an
/// I/O mapping holds its whole aperture, and a render node's window holds
/// its whole blob, whatever part of it the `mmap` asked for
/// (`AddressSpace::map_window`'s `whole`). Live blobs are disjoint whole
/// pages of the device's host-visible window, and a blob stays placed
/// while anything maps it. So the entries are at most the whole-page
/// apertures stage 10 found plus the pages of the host-visible windows:
/// fixed by the hardware, not raised by any number of mappings.
static CLAIMS: SpinLock<Vec<Claim>> = SpinLock::new(Vec::new());

/// A mapping's share in its range's [`Claim`]: given back when the last
/// region holding it goes.
#[derive(Debug)]
pub(crate) struct Hold {
    start: u64,
    end: u64,
    kind: MemoryType,
}

impl Drop for Hold {
    fn drop(&mut self) {
        release(self.start, self.end, self.kind);
    }
}

/// Lower the count of the claim on `start..end` as `kind`, and remove it at
/// zero.
fn release(start: u64, end: u64, kind: MemoryType) {
    let mut claims = CLAIMS.lock();
    let found = claims
        .iter()
        .position(|claim| claim.start == start && claim.end == end && claim.kind == kind);
    if let Some(at) = found
        && let Some(claim) = claims.get_mut(at)
    {
        claim.count = claim.count.saturating_sub(1);
        if claim.count == 0 {
            let _ = claims.swap_remove(at);
        }
    }
}

/// How many claims are recorded: what stage 9's check compares across
/// mappings of one range (L.user.109).
pub(crate) fn claims() -> usize {
    CLAIMS.lock().len()
}

/// Hold `physical..physical + len` as `kind`, unless a page of it is held as
/// another type.
///
/// The caller has checked the range is whole pages and does not wrap.
///
/// # Errors
///
/// [`Refused::OtherType`] when a live hold of another type overlaps the
/// range, and [`Refused::NoMemory`].
pub(crate) fn hold(physical: u64, len: u64, kind: MemoryType) -> Result<Arc<Hold>, Refused> {
    let end = physical.checked_add(len).ok_or(Refused::OtherType)?;
    let mut claims = CLAIMS.lock();
    let other = claims
        .iter()
        .any(|claim| claim.kind != kind && claim.start < end && physical < claim.end);
    let same = claims
        .iter()
        .position(|claim| claim.start == physical && claim.end == end && claim.kind == kind);
    let recorded = match (other, same.and_then(|at| claims.get_mut(at))) {
        (true, _) => Err(Refused::OtherType),
        (false, Some(claim)) => {
            claim.count = claim.count.saturating_add(1);
            Ok(())
        }
        (false, None) => fallible::try_push(
            &mut claims,
            Claim {
                start: physical,
                end,
                kind,
                count: 1,
            },
        )
        .map_err(|_| Refused::NoMemory),
    };
    drop(claims);
    recorded?;
    // Counted above, so made after. If there is no memory for it, the value
    // is dropped (`fallible::try_arc`), and its drop lowers the count again.
    fallible::try_arc(Hold {
        start: physical,
        end,
        kind,
    })
    .map_err(|_| Refused::NoMemory)
}
