//! MSI-X: the table a function's interrupts are programmed through, the
//! messages written into it, and the part of a BAR a driver must never see.
//!
//! An MSI-X interrupt is a write the device makes: to an address, of a value,
//! both of which the kernel put in a table entry in one of the device's own
//! BARs. Whoever can write that table decides which interrupt the device
//! raises — including interrupts that belong to the kernel or to another
//! driver. So the table, and the pending-bit array beside it, are the one part
//! of a device's memory a driver in ring 3 is never given, and [`mappable`]
//! is the arithmetic that cuts them out of a BAR, a page at a time because a
//! mapping is.
//!
//! The messages themselves depend on the interrupt controller rather than on
//! PCI, and both kinds Ferrix meets are here as plain functions: the local
//! APIC's fixed address range on x86-64, and a `GICv2m` frame's `SETSPI`
//! register on the Arm machines.

use crate::bar::Region;
use crate::capability::{BarOffset, MSIX_ENTRY_SIZE, MsiX};
use crate::{Address, ConfigSpace};

/// Offset in a table entry of the message address's low half.
pub const ENTRY_ADDRESS_LOW: u64 = 0x0;
/// Offset in a table entry of the message address's high half.
pub const ENTRY_ADDRESS_HIGH: u64 = 0x4;
/// Offset in a table entry of the message data.
pub const ENTRY_DATA: u64 = 0x8;
/// Offset in a table entry of the vector control word.
pub const ENTRY_VECTOR_CONTROL: u64 = 0xC;
/// Vector control: the entry is masked and raises nothing.
pub const VECTOR_CONTROL_MASKED: u32 = 1;

/// Offset of the MSI-X capability's message control word.
pub const CAPABILITY_CONTROL: u16 = 2;
/// Message control: MSI-X is enabled.
pub const CONTROL_ENABLE: u16 = 1 << 15;
/// Message control: every vector is masked at once.
pub const CONTROL_FUNCTION_MASK: u16 = 1 << 14;

/// Where in its BAR table entry `index` is, or `None` past the table's end.
#[must_use]
pub const fn entry_offset(msix: &MsiX, index: u16) -> Option<u64> {
    if index >= msix.table_size {
        return None;
    }
    Some(msix.table.offset as u64 + index as u64 * MSIX_ENTRY_SIZE)
}

/// What a table entry is programmed with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Message {
    /// Where the device writes.
    pub address: u64,
    /// What it writes.
    pub data: u32,
}

/// The address range every local APIC answers message writes at.
pub const LOCAL_APIC_MSI_BASE: u64 = 0xFEE0_0000;

/// The message that raises `vector` on the local APIC `apic_id`.
///
/// Fixed delivery, physical destination, edge-triggered: the address carries
/// the destination in bits 19:12 and the data carries the vector with every
/// mode bit clear, which is what each of those three means.
#[must_use]
pub const fn local_apic_message(apic_id: u8, vector: u8) -> Message {
    Message {
        address: LOCAL_APIC_MSI_BASE | (apic_id as u64) << 12,
        data: vector as u32,
    }
}

/// Offset in a `GICv2m` frame of `MSI_TYPER`, which says which SPIs it raises.
pub const GICV2M_TYPER: u64 = 0x08;
/// Offset in a `GICv2m` frame of `MSI_SETSPI_NS`, the register a device writes.
pub const GICV2M_SETSPI: u64 = 0x40;

/// The GIC interrupts a `GICv2m` frame can raise.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SpiRange {
    /// The first, as a GIC interrupt identifier.
    pub first: u32,
    /// How many.
    pub count: u32,
}

impl SpiRange {
    /// Whether `id` is one of them.
    #[must_use]
    pub const fn contains(self, id: u32) -> bool {
        id >= self.first && id - self.first < self.count
    }
}

/// Decode a frame's `MSI_TYPER`: the first identifier in bits 25:16, the
/// count in bits 9:0.
///
/// `None` for a frame claiming no SPIs, or a range that starts below the
/// shared peripherals or runs past the last identifier a GIC delivers.
#[must_use]
pub const fn gicv2m_spis(typer: u32) -> Option<SpiRange> {
    let first = (typer >> 16) & 0x3FF;
    let count = typer & 0x3FF;
    if count == 0 || first < 32 || first + count > 1020 {
        return None;
    }
    Some(SpiRange { first, count })
}

/// The message that raises GIC interrupt `id` through the frame at `frame`.
///
/// The device writes the identifier itself, not an index into the frame's
/// range: the frame subtracts its base, and ignores a write outside it.
#[must_use]
pub const fn gicv2m_message(frame: u64, id: u32) -> Option<Message> {
    match frame.checked_add(GICV2M_SETSPI) {
        Some(address) => Some(Message { address, data: id }),
        None => None,
    }
}

/// Byte ranges of a BAR, as offsets into it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ranges {
    /// Up to three `(offset, length)` pairs, in order.
    ranges: [(u64, u64); 3],
    /// How many are in use.
    len: usize,
}

impl Ranges {
    /// No ranges.
    const EMPTY: Ranges = Ranges {
        ranges: [(0, 0); 3],
        len: 0,
    };

    /// Add a range. Callers never add more than three.
    fn push(&mut self, offset: u64, len: u64) {
        if let Some(slot) = self.ranges.get_mut(self.len) {
            *slot = (offset, len);
            self.len += 1;
        }
    }

    /// The ranges, in order.
    #[must_use]
    pub fn as_slice(&self) -> &[(u64, u64)] {
        self.ranges.get(..self.len).unwrap_or(&[])
    }
}

/// The functions whose MSI-X the kernel never enables, by vendor and device:
/// their MSI-X table shares its page with registers their driver needs, so
/// the kernel holds MSI-X off, grants the page as an ordinary aperture and
/// offers the function's one MSI vector instead (`docs/DEVMGR.md` §3; the
/// consultant's review of 2026-10-15, ledger line 1097, D1).
///
/// Fixed at build: nothing a driver, devmgr or a boot option supplies adds
/// to it. The Motorcomm YT6801's glue registers -- its PHY's reset, its
/// eFuse, its interrupt delivery -- are in the page with its table at
/// `0x1200` of BAR 0.
pub const MSI_PREFERRED: [(u16, u16); 1] = [(0x1F0A, 0x6801)];

/// Whether `vendor:device` is in [`MSI_PREFERRED`].
#[must_use]
pub fn msi_preferred(vendor: u16, device: u16) -> bool {
    MSI_PREFERRED.contains(&(vendor, device))
}

/// QEMU's `e1000e` (`hw/net/e1000e.c`): an Intel 82574L model with both an
/// MSI capability (`msi_init` at 0xD0, one message) and an MSI-X table in a
/// BAR of its own. Stage 10's check of the held-off path runs on it.
pub const QEMU_E1000E: (u16, u16) = (0x8086, 0x10D3);

/// The subsystem xtask gives its `e1000e` (`subsys_ven=0x1af4,subsys=0x1100`,
/// the properties `hw/net/e1000e.c` declares; QEMU's own default for other
/// devices, `PCI_SUBVENDOR_ID_REDHAT_QUMRANET` and `PCI_SUBDEVICE_ID_QEMU`):
/// no real 82574L carries it, so a real card never meets the check.
pub const QEMU_SUBSYSTEM: (u16, u16) = (0x1AF4, 0x1100);

/// Whether a function is the one stage 10's held-off check may run on:
/// QEMU's `e1000e` with QEMU's subsystem, never a real 82574L.
#[must_use]
pub fn held_off_check_case(vendor: u16, device: u16, subsystem: (u16, u16)) -> bool {
    (vendor, device) == QEMU_E1000E && subsystem == QEMU_SUBSYSTEM
}

/// Hold a function's MSI-X off: write its control with Enable clear and
/// Function Mask set -- firmware may have left MSI-X on -- and answer whether
/// it reads back so. `capability` is the MSI-X capability's offset.
pub fn hold_off<C: ConfigSpace + ?Sized>(
    space: &mut C,
    function: Address,
    capability: u16,
) -> bool {
    let at = capability + CAPABILITY_CONTROL;
    let control = space.read16(function, at);
    space.write16(
        function,
        at,
        (control & !CONTROL_ENABLE) | CONTROL_FUNCTION_MASK,
    );
    let back = space.read16(function, at);
    back & CONTROL_ENABLE == 0 && back & CONTROL_FUNCTION_MASK != 0
}

/// The pages of `region` that hold the MSI-X table or pending-bit array, as
/// offsets into the BAR, merged and in order.
///
/// Each structure is rounded out to whole pages of `page_size`, which must be
/// a power of two, and clipped to the BAR. A structure in another BAR, or
/// lying entirely past this one's end, contributes nothing.
#[must_use]
pub fn withheld(region: &Region, msix: Option<&MsiX>, page_size: u64) -> Ranges {
    let mut out = Ranges::EMPTY;
    let Some(msix) = msix else {
        return out;
    };
    let page = if page_size.is_power_of_two() {
        page_size
    } else {
        1
    };
    let mut spans = [(0_u64, 0_u64); 2];
    let mut count = 0;
    for (at, len) in [
        (msix.table, msix.table_len()),
        (msix.pending, msix.pending_len()),
    ] {
        if let Some(span) = page_span(region, at, len, page)
            && let Some(slot) = spans.get_mut(count)
        {
            *slot = span;
            count += 1;
        }
    }
    let spans = spans.get_mut(..count).unwrap_or(&mut []);
    spans.sort_unstable();

    let mut current: Option<(u64, u64)> = None;
    for &(start, end) in spans.iter() {
        current = match current {
            Some((open, close)) if start <= close => Some((open, close.max(end))),
            Some((open, close)) => {
                out.push(open, close - open);
                Some((start, end))
            }
            None => Some((start, end)),
        };
    }
    if let Some((open, close)) = current {
        out.push(open, close - open);
    }
    out
}

/// `len` bytes at `at`, rounded out to pages and clipped to `region`, as
/// `start..end` offsets, or `None` if that leaves nothing.
fn page_span(region: &Region, at: BarOffset, len: u64, page: u64) -> Option<(u64, u64)> {
    if at.bar != region.index {
        return None;
    }
    let offset = u64::from(at.offset);
    let start = offset & !(page - 1);
    let end = offset
        .saturating_add(len)
        .checked_next_multiple_of(page)
        .unwrap_or(u64::MAX)
        .min(region.size);
    (start < end).then_some((start, end))
}

/// The ranges of `region` a driver may be given: everything outside
/// [`withheld`].
#[must_use]
pub fn mappable(region: &Region, msix: Option<&MsiX>, page_size: u64) -> Ranges {
    let mut out = Ranges::EMPTY;
    let mut cursor = 0;
    for &(offset, len) in withheld(region, msix, page_size).as_slice() {
        if offset > cursor {
            out.push(cursor, offset - cursor);
        }
        cursor = offset + len;
    }
    if cursor < region.size {
        out.push(cursor, region.size - cursor);
    }
    out
}
