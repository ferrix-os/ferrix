//! Device registers.
//!
//! A device register is not memory: the read has a side effect, the write must
//! actually happen, and neither may be merged with its neighbour or hoisted
//! out of a loop. `read_volatile` and `write_volatile` are how Rust says that,
//! and this is the one place in the kernel that says it — everything else
//! names a register through one of these windows.
//!
//! A window with no base address reads zero and discards writes rather than
//! dereferencing null. That turns "the controller was never mapped" from
//! undefined behaviour into a value the caller can notice, which during
//! bring-up is the difference between a diagnosis and a triple fault.

/// A mapped register window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Mmio {
    /// Virtual address the window starts at, or zero when there is none.
    base: u64,
}

impl Mmio {
    /// A window that is not mapped. Every access is a no-op.
    pub(crate) const fn unmapped() -> Self {
        Mmio { base: 0 }
    }

    /// A window at `base`, which must be a mapped device address.
    pub(crate) const fn at(base: u64) -> Self {
        Mmio { base }
    }

    /// The address `offset` bytes into the window, or `None` if unmapped.
    fn address(self, offset: u64) -> Option<u64> {
        if self.base == 0 {
            return None;
        }
        self.base.checked_add(offset)
    }

    /// Read a 32-bit register.
    pub(crate) fn read32(self, offset: u64) -> u32 {
        let Some(at) = self.address(offset) else {
            return 0;
        };
        // SAFETY: `at` is inside a window the caller mapped as device memory,
        // and register offsets are naturally aligned by the hardware's own
        // layout. Volatile because the read may have a side effect and must
        // not be elided or reordered with its neighbours.
        unsafe { core::ptr::read_volatile(at as *const u32) }
    }

    /// Write a 32-bit register.
    pub(crate) fn write32(self, offset: u64, value: u32) {
        let Some(at) = self.address(offset) else {
            return;
        };
        // SAFETY: as `read32`.
        unsafe { core::ptr::write_volatile(at as *mut u32, value) };
    }
}
