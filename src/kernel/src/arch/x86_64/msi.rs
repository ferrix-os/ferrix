//! Message-signalled interrupts on x86-64: a local APIC vector per message.
//!
//! An MSI on a PC is a write to the local APIC's fixed address range. There
//! is no controller line to program and nothing to enable: the vector is
//! taken from a range nothing else uses, and a device that writes the
//! message raises it.
//!
//! **Where a VT-d unit remaps interrupts** (`docs/NVIDIA.md` §12.3, N0g),
//! the message names an entry in the unit's interrupt remapping table -- the
//! vector's slot among the sixty-four is its index -- which holds the vector,
//! the destination, and the requester ID the message must arrive with; the
//! unit refuses a message in compatibility format, and one naming the entry
//! from any other function. Where no unit remaps, the message carries the
//! vector itself, in compatibility format, as before.

use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_pci::msix::local_apic_message;

use super::{apic, trap};
use crate::iommu::remapping::{self, Route};
use crate::irq::Msi;

/// The first vector handed out. Sixty-four vectors from here stay below the
/// legacy system call gate at 0x80 and far below the kernel's own at 0xFC to
/// 0xFF.
const FIRST_VECTOR: u64 = 0x40;

/// Which of the sixty-four are allocated, one bit each. A vector is never
/// given back, so an interrupt remapping entry indexed by one is written
/// once and never reused.
static TAKEN: AtomicU64 = AtomicU64::new(0);

/// Take a vector and say what the device whose requester ID is `device`
/// writes to raise it.
///
/// The message is aimed at the local APIC of the processor asking. Any
/// processor can service the interrupt, so which one it lands on is a
/// question of balance, not correctness. Where a unit remaps, the entry is
/// written and invalidated before this returns, so the message can be
/// programmed at once; a function the remapping units cannot tell apart
/// from another gets no vector.
///
/// # Errors
///
/// Every vector already taken, a local APIC identifier too wide for a
/// message's eight-bit destination, a function refused a route
/// (`iommu::route`), or an entry the unit would not take.
pub fn msi_allocate(device: u32) -> Result<Msi, &'static str> {
    let apic_id = apic::id();
    let destination =
        u8::try_from(apic_id).map_err(|_| "this local APIC's identifier is too wide for MSI")?;
    let requester = u16::try_from(device).map_err(|_| "a requester ID wider than 16 bits")?;
    // Before a vector is taken: one handed to a refused function would be
    // spent for nothing.
    let route = remapping::route(requester)?;
    let vector = allocate_vector().ok_or("every MSI vector is allocated")?;
    let (address, data) = match route {
        Route::Compatibility => {
            let message = local_apic_message(destination, vector as u8);
            (message.address, message.data)
        }
        Route::Remapped(unit) => (
            remapping::remap_message(unit, requester, vector as u8, apic_id)?,
            0,
        ),
    };
    Ok(Msi {
        number: (vector - trap::IRQ_BASE) as u32,
        address,
        data,
    })
}

/// Which of the sixty-four vectors are handed out, one bit each from 0x40:
/// for interrupt remapping's bring-up, which requires them all to be the
/// console line's (check R9).
pub(crate) fn taken() -> u64 {
    TAKEN.load(Ordering::Acquire)
}

/// A vector for interrupt remapping's bring-up, as `iommu` takes it.
pub(crate) fn take_vector() -> Option<u8> {
    allocate_vector().and_then(|vector| u8::try_from(vector).ok())
}

/// Take a device vector from the same sixty-four a message uses, for an
/// interrupt that arrives some other way: an I/O APIC input.
pub(crate) fn allocate_vector() -> Option<u64> {
    let index = loop {
        let taken = TAKEN.load(Ordering::Relaxed);
        let free = taken.trailing_ones();
        if free >= u64::BITS {
            return None;
        }
        if TAKEN
            .compare_exchange(
                taken,
                taken | 1 << free,
                Ordering::SeqCst,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            break u64::from(free);
        }
    };
    Some(FIRST_VECTOR + index)
}
