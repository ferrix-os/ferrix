//! Device registers.
//!
//! A device register is not memory: the read has a side effect, the write must
//! actually happen, and neither may be merged with its neighbour or hoisted
//! out of a loop. `read_volatile` and `write_volatile` are how Rust says that,
//! and this is the one place in the kernel that says it, through
//! `crate::arch::mmio` — everything else names a register through one of
//! these windows.
//!
//! On Arm, volatile is not enough. Under KVM a register access traps to the
//! hypervisor, which can only emulate a load or store whose syndrome it can
//! read: one register, no writeback. Volatile leaves the instruction to
//! LLVM, which folds the address arithmetic of repeated reads at one offset
//! into a pre-indexed `ldrh w, [x, #4]!`. Arm64 KVM then fails `KVM_RUN` with
//! ENOSYS ("Data abort outside memslots with no valid syndrome info"), and
//! crosvm stops the guest without a word. That is how the Pixel 7's VM
//! stopped after stage 10's DMA switch check. So each access goes through
//! `crate::arch::mmio`: on AArch64 and ARMv7-A one `ldr`/`str` in `asm!`,
//! with nothing but a base register, as Linux's `readl` and `writel` are,
//! and on x86-64 volatile, since KVM there decodes the instruction itself.
//!
//! A window with no base address reads zero and discards writes rather than
//! dereferencing null. That turns "the controller was never mapped" from
//! undefined behaviour into a value the caller can notice, which during
//! bring-up is the difference between a diagnosis and a triple fault.

/// A mapped register window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mmio {
    /// Virtual address the window starts at, or zero when there is none.
    base: u64,
}

impl Mmio {
    /// A window that is not mapped. Every access is a no-op.
    pub const fn unmapped() -> Self {
        Mmio { base: 0 }
    }

    /// A window at `base`, which must be a mapped device address.
    pub const fn at(base: u64) -> Self {
        Mmio { base }
    }

    /// The address `offset` bytes into the window, or `None` if unmapped.
    fn address(self, offset: u64) -> Option<u64> {
        if self.base == 0 {
            return None;
        }
        self.base.checked_add(offset)
    }

    /// Read an 8-bit register.
    pub fn read8(self, offset: u64) -> u8 {
        let Some(at) = self.address(offset) else {
            return 0;
        };
        // SAFETY: (DEVICE) as `read32`; a byte has no alignment to get wrong.
        unsafe { crate::arch::mmio::read8(at) }
    }

    /// Write an 8-bit register.
    pub fn write8(self, offset: u64, value: u8) {
        let Some(at) = self.address(offset) else {
            return;
        };
        // SAFETY: (DEVICE) as `read8`.
        unsafe { crate::arch::mmio::write8(at, value) };
    }

    /// Read a 16-bit register. The caller keeps `offset` even.
    pub fn read16(self, offset: u64) -> u16 {
        let Some(at) = self.address(offset) else {
            return 0;
        };
        // SAFETY: (DEVICE) as `read32`, with the caller holding the offset to a
        // multiple of two.
        unsafe { crate::arch::mmio::read16(at) }
    }

    /// Write a 16-bit register. The caller keeps `offset` even.
    pub fn write16(self, offset: u64, value: u16) {
        let Some(at) = self.address(offset) else {
            return;
        };
        // SAFETY: (DEVICE) as `read16`.
        unsafe { crate::arch::mmio::write16(at, value) };
    }

    /// Read a 32-bit register.
    pub fn read32(self, offset: u64) -> u32 {
        let Some(at) = self.address(offset) else {
            return 0;
        };
        // SAFETY: (DEVICE) `at` is inside a window the caller mapped as device memory,
        // and register offsets are naturally aligned by the hardware's own
        // layout. The access is not elided, merged or reordered with its
        // neighbours, because the read may have a side effect (the header).
        unsafe { crate::arch::mmio::read32(at) }
    }

    /// Write a 32-bit register.
    pub fn write32(self, offset: u64, value: u32) {
        let Some(at) = self.address(offset) else {
            return;
        };
        // SAFETY: (DEVICE) as `read32`.
        unsafe { crate::arch::mmio::write32(at, value) };
    }
}
