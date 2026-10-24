//! Stage 10's AMD-Vi checks that need a device to do the DMA: QEMU's `edu`,
//! whose DMA engine writes 64 bytes where it is told (`docs/AMD-VI.md`
//! §5.1). Run from `device/check.rs`'s MSI check, before `edu`'s domain is
//! made, on the AMD-Vi row only.
//!
//! * **A1, blocked by default**: with no domain, `edu`'s write to a page
//!   lands nowhere, and its unit records the provoked fault.
//! * **A8, a unity map holds**: an IVMD identity domain made for `edu` at
//!   check time, holding one page, lets its write land there at the page's
//!   own address; its write to the page beside faults; a pin over the page
//!   is refused. The domain is then `edu`'s own, for every check after.
//! * **Unpin forgets** (ledger 3867, condition 3): a page pinned into that
//!   domain takes `edu`'s write; once unpinned, the same write faults and
//!   lands nowhere, though the unit had cached the translation.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_paging::MapFlags;
use ferrix_paging::vtd::rmrr::Identity;
use ferrix_pci::Address;

use super::{Domain, DomainError, Fault, IdentityDomain};
use crate::device::{DeviceNode, Location};
use crate::mmio::Mmio;
use crate::{mm, timer};

/// `edu`'s DMA source register.
const DMA_SOURCE: u64 = 0x80;
/// `edu`'s DMA destination register.
const DMA_DESTINATION: u64 = 0x88;
/// `edu`'s DMA count register.
const DMA_COUNT: u64 = 0x90;
/// `edu`'s DMA command register: bit 0 runs it, bit 1 writes to RAM.
const DMA_COMMAND: u64 = 0x98;
/// `edu`'s own buffer, the source of a write to RAM.
const EDU_BUFFER: u64 = 0x4_0000;
/// Bytes each write moves.
const BYTES: u32 = 64;
/// The addresses `edu` can reach: the machine widens its DMA mask to 32
/// bits (`xtask`'s `attach_rng`), which its 32-bit registers' writes hold.
const EDU_REACH: u64 = 1 << 32;
/// What a page is filled with, so a write of `edu`'s zeroes shows.
const FILL: u64 = 0xAAAA_AAAA_AAAA_AAAA;
/// How long a fault may take to be recorded; QEMU's `edu` runs its DMA
/// 100 ms after the command.
const PATIENCE_NANOS: u64 = 1_000_000_000;

/// The checks, on `node` (`edu`) with its registers at `edu`: `None` where
/// no translating AMD-Vi unit places it.
///
/// # Errors
///
/// What did not hold, as a sentence.
///
/// Verifies: L.iommu.84, L.iommu.88, L.iommu.94
pub(crate) fn check_edu(
    node: &Arc<DeviceNode>,
    edu: Mmio,
) -> Result<Option<alloc::string::String>, &'static str> {
    let Location::Pci(function) = node.location() else {
        return Ok(None);
    };
    let Some(unit) = amdvi_unit_of(function) else {
        return Ok(None);
    };
    if node.domain_made().is_some() {
        return Err("edu had a domain before the AMD-Vi checks");
    }
    let stream = u32::from(function.requester_id());
    let mut spare = Vec::new();
    let pages = (0..4)
        .map(|_| low_frame(&mut spare))
        .collect::<Option<Vec<u64>>>();
    for frame in spare {
        mm::deallocate_frames(frame, 0);
    }
    let Some(&[blocked, unity, beside, pinned]) = pages.as_deref() else {
        return Err("no frames below 4 GiB for edu's DMA");
    };
    node.enable_dma()?;
    let result = (|| {
        // A1.
        fill(blocked);
        provoke_on(unit, stream, blocked);
        write(edu, blocked)?;
        if !faulted(|| take_from(unit), stream, blocked) {
            return Err("edu's write with no domain was not recorded as its fault");
        }
        if !filled(blocked) {
            return Err("edu's write with no domain landed");
        }
        // A8.
        let region = Identity {
            base: unity,
            pages: 1,
        };
        let attached = unit.attach_identity(function, &[region], MapFlags::DMA)?;
        // FATAL-ALLOC: boot only: the check's identity domain is made once and, as every identity domain, kept for the boot.
        let identity: &'static IdentityDomain = alloc::boxed::Box::leak(alloc::boxed::Box::new(
            IdentityDomain::new_amdvi(function, attached, alloc::vec![region]),
        ));
        let domain = identity
            .claim_amdvi(unit)
            .ok_or("the check's identity domain could not be claimed")?;
        if domain.resolve(unity) != Some(unity) || domain.resolve(beside).is_some() {
            return Err("an IVMD identity domain does not map exactly its page");
        }
        if !matches!(
            domain.pin(&[unity / PAGE_SIZE], MapFlags::DMA),
            Err(DomainError::Reserved)
        ) {
            return Err("a pin over an IVMD page was not refused");
        }
        fill(unity);
        write(edu, unity)?;
        if !landed(unity) {
            return Err("edu's write to its IVMD page did not land at the page's own address");
        }
        fill(beside);
        domain.provoke(beside);
        write(edu, beside)?;
        if !faulted(|| domain.take_fault(), stream, beside) || !filled(beside) {
            return Err("edu's write beside its IVMD page was not refused and recorded");
        }
        // Unpin forgets.
        fill(pinned);
        let pin = domain
            .pin(&[pinned / PAGE_SIZE], MapFlags::DMA)
            .map_err(|_| "a page beside the IVMD could not be pinned")?;
        write(edu, pinned)?;
        if !landed(pinned) {
            pin.leak();
            return Err("edu's write to a pinned page did not land");
        }
        if let Err((_, pin)) = domain.unpin(pin) {
            pin.leak();
            return Err("a pinned page could not be unpinned");
        }
        fill(pinned);
        domain.provoke(pinned);
        write(edu, pinned)?;
        if !faulted(|| domain.take_fault(), stream, pinned) || !filled(pinned) {
            return Err(
                "edu's write after the unpin was not refused: the unit was not made to \
                        forget the page",
            );
        }
        let domain: Arc<Domain> =
            crate::fallible::try_arc(domain).map_err(|_| "no memory for edu's domain")?;
        node.give_check_domain(domain)?;
        Ok(alloc::format!(
            "edu on AMD-Vi: its write with no domain refused and recorded; an IVMD identity \
             domain of page {unity:#x} took its write there, refused the page beside and a pin \
             over it; a page pinned took its write and, unpinned, refused the same write"
        ))
    })();
    node.disable_dma()?;
    // The unity page is the identity domain's for the boot; the others go
    // back once nothing reaches them.
    for page in [blocked, beside, pinned] {
        mm::deallocate_frames(page / PAGE_SIZE, 0);
    }
    result.map(Some)
}

/// A frame below `edu`'s reach, by its physical address; the frames above
/// it taken on the way are kept in `spare`, for the caller to give back.
fn low_frame(spare: &mut Vec<u64>) -> Option<u64> {
    for _ in 0..4096 {
        let frame = mm::allocate_frames(0)?;
        if frame * PAGE_SIZE + PAGE_SIZE <= EDU_REACH {
            mm::zero_frame(frame);
            return Some(frame * PAGE_SIZE);
        }
        crate::fallible::push_within(spare, frame).ok()?;
    }
    None
}

/// Make `edu` write its 64 bytes at `page`, and wait until it says it has.
fn write(edu: Mmio, page: u64) -> Result<(), &'static str> {
    edu.write32(DMA_SOURCE, EDU_BUFFER as u32);
    edu.write32(
        DMA_DESTINATION,
        u32::try_from(page).map_err(|_| "a page past edu's reach")?,
    );
    edu.write32(DMA_COUNT, BYTES);
    edu.write32(DMA_COMMAND, 0b11);
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while edu.read32(DMA_COMMAND) & 1 != 0 {
        if timer::now_nanos() > deadline {
            return Err("edu's DMA never finished");
        }
        core::hint::spin_loop();
    }
    Ok(())
}

/// Whether a fault `take` reads is `stream`'s write to `page`, within the
/// patience.
fn faulted(mut take: impl FnMut() -> Option<Fault>, stream: u32, page: u64) -> bool {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while timer::now_nanos() <= deadline {
        if let Some(fault) = take() {
            return fault == Fault::write_to(stream, page);
        }
        core::hint::spin_loop();
    }
    false
}

/// Fill the first 64 bytes of `page` with [`FILL`].
fn fill(page: u64) {
    for word in 0..u64::from(BYTES) / 8 {
        // SAFETY: (DMA) `page` is a frame this check took from the frame
        // allocator, below `edu`'s reach; the direct map covers it, and
        // `edu` may write it too, which is why the access is volatile.
        unsafe { core::ptr::write_volatile(mm::direct_map(page + word * 8) as *mut u64, FILL) };
    }
}

/// Whether the first 64 bytes of `page` still hold [`FILL`].
fn filled(page: u64) -> bool {
    (0..u64::from(BYTES) / 8).all(|word| read(page + word * 8) == FILL)
}

/// Whether `edu`'s zeroes landed over every one of the first 64 bytes.
fn landed(page: u64) -> bool {
    (0..u64::from(BYTES) / 8).all(|word| read(page + word * 8) == 0)
}

/// Read the word at `at`.
fn read(at: u64) -> u64 {
    // SAFETY: (DMA) as `fill`'s.
    unsafe { core::ptr::read_volatile(mm::direct_map(at) as *const u64) }
}

/// The translating AMD-Vi unit `function` is placed behind, for stage 10's
/// AMD-Vi checks (`device/check.rs`).
fn amdvi_unit_of(function: Address) -> Option<&'static super::amdvi::Unit> {
    super::amdvi_unit_for(super::PROGRAMMED.get()?, function).filter(|unit| unit.translating())
}

/// Clear `unit`'s events and say that `stream` is about to be made to write
/// `page`, outside any domain: what `Domain::provoke` does, for a check
/// whose device has no domain yet (`device/check.rs`, A1).
fn provoke_on(unit: &super::amdvi::Unit, stream: u32, page: u64) {
    for _ in 0..256 {
        if take_from(unit).is_none() {
            break;
        }
    }
    super::record_provoked(stream, page);
}

/// The next fault `unit` recorded, counted and recorded as every fault is,
/// and counted stray unless a check provoked it.
fn take_from(unit: &super::amdvi::Unit) -> Option<Fault> {
    let fault = super::recorded(super::counted(unit.take_fault()))?;
    let _ = super::count_if_stray(fault);
    Some(fault)
}

// CHECKS-ONLY: iommu/domain_check.rs; home: super
/// The AMD-Vi units' tables and commands (`docs/AMD-VI.md` §5.1, A1 and
/// A5): every sampled device table entry no function holds -- every
/// 4,096th devid -- reads blocked in both halves, every PCI function with no
/// domain reads blocked in its DMA half, the exclusion registers read 0, and
/// every command of the boot was waited for by its sequence, with exactly
/// the one wait check R7 made fail failing. `None` with no AMD-Vi unit.
///
/// # Errors
///
/// What did not hold, as a sentence.
///
/// Verifies: L.iommu.84, L.iommu.86, L.iommu.87, L.iommu.96
#[doc(hidden)]
pub fn check_tables(
    failed_kept: usize,
) -> Result<Option<alloc::string::String>, alloc::string::String> {
    use ferrix_paging::amdvi::table;

    let Some(programmed) = super::PROGRAMMED.get() else {
        return Ok(None);
    };
    if programmed.amdvi.is_empty() {
        return Ok(None);
    }
    let mut sampled = 0_usize;
    for unit in &programmed.amdvi {
        let blocked = table::blocked(unit.blocked_dma());
        let held = |devid: u16| {
            crate::device::devices().iter().any(|node| {
                matches!(node.location(), Location::Pci(function) if function.requester_id() == devid)
            })
        };
        for devid in (0..=u16::MAX).step_by(4096) {
            if held(devid) || devid == unit.devid() {
                continue;
            }
            let entry = unit.device_entry(devid);
            let interrupts_left = !unit.remapping() && entry[2] == 0;
            if table::dma_half(&entry) != table::dma_half(&blocked)
                || (entry[2] != table::BLOCKED_INTERRUPTS && !interrupts_left)
            {
                return Err(alloc::format!(
                    "amd-vi unit {:#x}: device table entry {devid:#06x}, which no function holds, \
                     is not blocked: {entry:#x?}",
                    unit.phys()
                ));
            }
            sampled += 1;
        }
        for node in crate::device::devices() {
            let Location::Pci(function) = node.location() else {
                continue;
            };
            if node.domain_made().is_some() {
                continue;
            }
            let entry = unit.device_entry(function.requester_id());
            if table::dma_half(&entry) != table::dma_half(&blocked) {
                return Err(alloc::format!(
                    "amd-vi unit {:#x}: pci {function}, which has no domain, can reach memory",
                    unit.phys()
                ));
            }
        }
        if unit.exclusion() != (0, 0) {
            return Err(alloc::format!(
                "amd-vi unit {:#x}: its exclusion registers do not read 0",
                unit.phys()
            ));
        }
    }
    let commands = super::amdvi::commands();
    let provoked = u64::from(failed_kept != 0);
    if commands.issued == 0 || commands.failed != provoked {
        return Err(alloc::format!(
            "the AMD-Vi units issued {} commands, {} waits failed where check R7 made {provoked} \
             fail",
            commands.issued,
            commands.failed
        ));
    }
    Ok(Some(alloc::format!(
        "amd-vi: {sampled} sampled device table entries no function holds read blocked, every \
         function with no domain blocked, exclusion registers 0; {} commands issued and {} \
         completion waits stored by their sequence, {} failed as check R7 made it",
        commands.issued,
        commands.waited,
        commands.failed
    )))
}

// CHECKS-ONLY: object/pin/check.rs; home: super
/// The AMD-Vi units' completion waits that failed, for check R7
/// (`object/pin/check.rs`).
#[doc(hidden)]
#[must_use]
pub fn failed_waits() -> u64 {
    super::amdvi::commands().failed
}

// CHECKS-ONLY: iommu/domain_check.rs; home: super
/// Whether D10 exempted `function`, for stage 10's check that such a
/// function is refused a pin and keeps bus mastering off.
#[doc(hidden)]
#[must_use]
pub fn exempt(function: Address) -> bool {
    super::dma_exempt(function)
}

// CHECKS-ONLY: iommu/domain_check.rs; home: super
/// Whether `node` is a bus master now, for the same check.
#[doc(hidden)]
#[must_use]
pub fn bus_master(node: &DeviceNode) -> bool {
    node.bus_master_on()
}
