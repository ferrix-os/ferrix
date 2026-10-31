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
//! from any other function. **Where an AMD-Vi unit remaps**
//! (`docs/AMD-VI.md` §3), the message is the plain address with the entry's
//! index as its data, and the entry is in the function's own table, so no
//! other function can name it. Where no unit remaps, the message carries
//! the vector itself, in compatibility format, as before.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use ferrix_pci::msix::local_apic_message;

use super::{apic, trap};
use crate::iommu::remapping::{self, Remapper, Route};
use crate::irq::Msi;
use crate::sync::SpinLock;

/// The first vector handed out. Sixty-four vectors from here stay below the
/// legacy system call gate at 0x80 and far below the kernel's own at 0xFB to
/// 0xFF.
const FIRST_VECTOR: u64 = 0x40;

/// Which of the sixty-four are allocated, one bit each. A vector is never
/// given back, so an interrupt remapping entry indexed by one is written
/// once and never reused.
static TAKEN: AtomicU64 = AtomicU64::new(0);

/// Where each of the sixty-four messages is aimed: its destination's APIC
/// identifier plus one, 0 for a vector no message was made for. Chosen
/// among the processors in service and written inside one read-side
/// section, so the grace period a processor going out of service waits for
/// sees every message aimed at it (`docs/CORE-QUARANTINE.md` §3.3,
/// `L.iommu.103`).
static AIMED: [AtomicU32; 64] = [const { AtomicU32::new(0) }; 64];

/// Each remapped message's unit and requester, recorded once its entry is
/// written: what a retarget rewrites. `None` for a vector in compatibility
/// format, whose destination is in the device's own table and cannot be
/// moved from here, or one not yet made.
static ROUTES: SpinLock<[Option<(Remapper, u16)>; 64]> = SpinLock::new([None; 64]);

/// Take a vector and say what the device whose requester ID is `device`
/// writes to raise it.
///
/// Messages are aimed at the processors in service in turn ([`aim`]). Any
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
    let requester = u16::try_from(device).map_err(|_| "a requester ID wider than 16 bits")?;
    // Before a vector is taken: one handed to a refused function would be
    // spent for nothing.
    let route = remapping::route(requester)?;
    let (apic_id, vector) = aim()?;
    let made = match route {
        Route::Compatibility => u8::try_from(apic_id)
            .map(|destination| {
                let message = local_apic_message(destination, vector as u8);
                (message.address, message.data)
            })
            .map_err(|_| "this local APIC's identifier is too wide for MSI"),
        Route::Remapped(unit) => remapping::remap_message(unit, requester, vector as u8, apic_id),
    };
    let index = (vector - FIRST_VECTOR) as usize;
    let (address, data) = made.inspect_err(|_| {
        // Never handed out, so aimed nowhere.
        if let Some(aimed) = AIMED.get(index) {
            aimed.store(0, Ordering::Release);
        }
    })?;
    if let (Route::Remapped(unit), Some(slot)) = (route, ROUTES.lock().get_mut(index)) {
        *slot = Some((unit, requester));
    }
    Ok(Msi {
        number: (vector - trap::IRQ_BASE) as u32,
        address,
        data,
    })
}

/// Take a vector aimed at the next processor in service, in turn, and
/// record the aim: the destination's APIC identifier, and the vector. The
/// choice and the record are one read-side section, so a processor whose
/// `ACTIVE` bit is cleared is either seen out of service here or has the
/// record made before its grace period ends (`docs/CORE-QUARANTINE.md`
/// §3.3). Before the scheduler says which are in service, the asking
/// processor's own.
fn aim() -> Result<(u32, u64), &'static str> {
    crate::smp::read_section(|| {
        let apic_id = next_in_service().unwrap_or_else(apic::id);
        let taken = if apic_id > u32::from(u8::MAX) {
            Err("this local APIC's identifier is too wide for MSI")
        } else {
            allocate_vector().ok_or("every MSI vector is allocated")
        };
        if let Ok(vector) = taken
            && let Some(aimed) = AIMED.get((vector - FIRST_VECTOR) as usize)
        {
            aimed.store(apic_id + 1, Ordering::Release);
        }
        taken.map(|vector| (apic_id, vector))
    })
}

/// The turn [`next_in_service`] is at.
static TURN: AtomicU32 = AtomicU32::new(0);

/// The APIC identifier of the next processor in service, in turn, or
/// `None` while none is.
fn next_in_service() -> Option<u32> {
    let topology = crate::smp::topology()?;
    let serving = || topology.cpus().iter().filter(|cpu| cpu.in_service());
    let count = u32::try_from(serving().count())
        .ok()
        .filter(|&count| count > 0)?;
    let turn = TURN.fetch_add(1, Ordering::Relaxed) % count;
    serving()
        .nth(turn as usize)
        .and_then(|cpu| u32::try_from(cpu.hardware_id).ok())
}

/// Whether a message is aimed at the local APIC whose identifier is
/// `apic_id` that cannot be moved: one in compatibility format, or one
/// whose entry is still being written. A processor with one cannot go out
/// of service (`docs/CORE-QUARANTINE.md` §3.3).
pub(crate) fn immovable_aimed_at(apic_id: u64) -> bool {
    let routes = *ROUTES.lock();
    aimed_at(apic_id).any(|index| routes.get(index).is_some_and(Option::is_none))
}

/// Aim every message aimed at the local APIC `from` at the local APIC `to`
/// instead, through its remapping entry (`L.iommu.101`): refused before any
/// moves if one cannot be moved; a message moved stays where it was moved
/// (`docs/CORE-QUARANTINE.md` §3.3, C1). Answers how many moved.
///
/// # Errors
///
/// A message that cannot be moved, or the retarget a unit refused, after
/// which the messages before it stay moved.
pub(crate) fn move_messages(from: u64, to: u32) -> Result<usize, &'static str> {
    let routes = *ROUTES.lock();
    // The vectors to move, found once: refused before any moves if one has
    // no route to rewrite.
    let mut moving = [None; 64];
    for index in aimed_at(from) {
        let route = routes.get(index).copied().flatten();
        let slot = moving
            .get_mut(index)
            .ok_or("a vector past the sixty-four")?;
        *slot = Some(route.ok_or("a message in compatibility format, or one whose entry is still being written, cannot be moved")?);
    }
    let mut count = 0;
    for (index, route) in moving.into_iter().enumerate() {
        let Some((unit, requester)) = route else {
            continue;
        };
        let vector = u8::try_from(FIRST_VECTOR + index as u64).unwrap_or(u8::MAX);
        remapping::retarget_message(unit, requester, vector, to)?;
        if let Some(aimed) = AIMED.get(index) {
            aimed.store(to + 1, Ordering::Release);
        }
        count += 1;
    }
    Ok(count)
}

/// The vectors' indices whose messages are aimed at the local APIC
/// `apic_id`.
fn aimed_at(apic_id: u64) -> impl Iterator<Item = usize> {
    let wanted = u32::try_from(apic_id.saturating_add(1)).unwrap_or(0);
    AIMED
        .iter()
        .enumerate()
        .filter(move |(_, aimed)| wanted != 0 && aimed.load(Ordering::Acquire) == wanted)
        .map(|(index, _)| index)
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
