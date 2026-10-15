//! I/O mappings: a device's registers, mappable into a driver's address
//! space, and nothing outside them.
//!
//! `docs/ARCHITECTURE.md` §7 gives a driver "an `IoMapping` for each BAR or
//! MMIO window, and nothing outside it". The "nothing outside" is enforced
//! before this module is reached: an [`IoMapping`] is built only from an
//! [`Aperture`], and only `crate::device` can make one, from what enumeration
//! found. This module's own rule is the one a page table forces on top.
//!
//! # Whole pages, or refused
//!
//! A mapping is made of pages, and an aperture need not be. QEMU's ARM
//! machines pack their virtio-mmio transports 0x200 bytes apart, several to a
//! page, so rounding an aperture out to its page would hand a driver its
//! neighbours' registers as well as its own — exactly what the aperture type
//! exists to prevent. So an aperture that is not a whole number of pages is
//! refused rather than rounded. Sub-page apertures need trapping each access,
//! which is a later story; drivers on those machines use virtio-pci, whose
//! BARs are whole pages.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_vma::VmaFlags;

use crate::arch;
use crate::device::Aperture;
use crate::user::space::{AddressSpace, SpaceError};

/// Set by stage 9's check alone, for one call, to count one processor's PAT
/// as not programmed: the refusal below is then run, as a machine with such
/// a processor would run it. Nothing else sets it, and it is set only before
/// any program runs.
static ONE_UNPROGRAMMED_FOR_CHECK: AtomicBool = AtomicBool::new(false);

/// Whether a write-combining mapping may be made: on x86-64 only once every
/// processor programmed its PAT with the write-combining entry. Until then a
/// descriptor selecting entry 1 would select the power-on table's
/// write-through, which caches reads of the aperture, so
/// `io_mapping_map_combining` is refused rather than mapped so. The Arm
/// architectures have no table to program.
pub fn combining_ready() -> bool {
    let unprogrammed = usize::from(ONE_UNPROGRAMMED_FOR_CHECK.load(Ordering::Acquire));
    arch::write_combining_processors()
        .is_none_or(|programmed| programmed.saturating_sub(unprogrammed) == crate::smp::count())
}

/// Count one processor's PAT as not programmed while `on`: stage 9's check of
/// the refusal (`object::check`), which runs before any program does.
pub fn count_one_unprogrammed_for_check(on: bool) {
    ONE_UNPROGRAMMED_FOR_CHECK.store(on, Ordering::Release);
}

/// Why an I/O mapping could not be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoMappingError {
    /// The aperture does not start on a page boundary or is not a whole
    /// number of pages, so no page mapping covers it and nothing else.
    NotWholePages,
    /// There was no memory for it.
    NoMemory,
}

/// A device aperture a driver may map.
#[derive(Debug)]
pub struct IoMapping {
    /// What it covers, exactly.
    aperture: Aperture,
}

impl IoMapping {
    /// An I/O mapping of `aperture`.
    ///
    /// # Errors
    ///
    /// [`IoMappingError::NotWholePages`], [`IoMappingError::NoMemory`].
    pub fn new(aperture: Aperture) -> Result<Arc<IoMapping>, IoMappingError> {
        if !aperture.whole_pages() {
            return Err(IoMappingError::NotWholePages);
        }
        crate::fallible::try_arc(IoMapping { aperture }).map_err(|_| IoMappingError::NoMemory)
    }

    /// Whether the device says reads of it have no side effects: a
    /// prefetchable BAR, which alone may be mapped write-combining.
    pub const fn prefetchable(&self) -> bool {
        self.aperture.cacheable()
    }

    /// Map it into `space`, at `at` or wherever it fits, readable and
    /// writable, write-combining if `combining`. Returns where.
    ///
    /// # Errors
    ///
    /// Whatever [`AddressSpace::map_device`] refuses.
    pub fn map_into(
        &self,
        space: &AddressSpace,
        at: Option<u64>,
        combining: bool,
    ) -> Result<u64, SpaceError> {
        space.map_device(
            at,
            self.aperture.len(),
            self.aperture.phys(),
            VmaFlags::READ_WRITE,
            combining,
        )
    }
}
