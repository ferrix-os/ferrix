//! An AMD-Vi event log record, decoded (`docs/AMD-VI.md` §1.6).
//!
//! Reference: #48882, "Event Log"; checked against Linux's
//! `iommu_print_event` and QEMU's `amdvi_encode_event` and
//! `amdvi_page_fault`, which the boot row runs.
//!
//! A record is sixteen bytes: the devid in bits 15:0, the event code in
//! bits 63:60 and its flags in 59:48 of the first quadword; the address in
//! the second.
//!
//! # QEMU's page faults, carried
//!
//! QEMU 10.2's `amdvi_page_fault` sets the `I` flag (an interrupt request's
//! fault) on every DMA page fault, and puts the access in flags bits 1:0 --
//! a write in bit 1 -- rather than in `RW` (bit 5). Hardware sets `I` only
//! for an interrupt request, whose address is the interrupt window. So a
//! fault is an interrupt request's only with `I` set **and** its address
//! in 0xFEE00000-0xFEEFFFFF; any other `IO_PAGE_FAULT` is a DMA fault, its
//! access `RW`, or flags bit 1 when `I` is set outside the window, which is
//! QEMU's encoding alone (ledger 3867, condition 10). QEMU's devid is the
//! devfn alone, the bus dropped; the boot row's devices are on bus 0.

/// `IO_PAGE_FAULT`.
pub const IO_PAGE_FAULT: u8 = 0x2;
/// `ILLEGAL_COMMAND_ERROR`.
pub const ILLEGAL_COMMAND: u8 = 0x5;
/// `COMMAND_HARDWARE_ERROR`.
pub const COMMAND_HARDWARE_ERROR: u8 = 0x6;

/// Flags: the request was an interrupt request.
const INTERRUPT: u16 = 1 << 3;
/// Flags: the access was a write.
const RW: u16 = 1 << 5;
/// Flags: QEMU's write bit (`AMDVI_PERM_WRITE`).
const QEMU_WRITE: u16 = 1 << 1;

/// The interrupt window: where a message is written.
const INTERRUPT_WINDOW: core::ops::RangeInclusive<u64> = 0xFEE0_0000..=0xFEEF_FFFF;

/// What a record says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// A DMA access refused.
    Access {
        /// The devid.
        devid: u16,
        /// The page addressed.
        page: u64,
        /// Whether it was a write.
        write: bool,
    },
    /// An interrupt request refused.
    Interrupt {
        /// The devid.
        devid: u16,
    },
    /// Any other event, by its code: a device table entry the unit would
    /// not use, a hardware error, a command it refused, an IOTLB timeout.
    Other {
        /// The devid the record names, if any.
        devid: u16,
        /// The event code.
        code: u8,
    },
}

/// The event `record` holds, its two quadwords as the unit wrote them.
#[must_use]
pub fn decode(record: [u64; 2]) -> Event {
    let devid = (record[0] & 0xFFFF) as u16;
    let code = (record[0] >> 60) as u8;
    let flags = ((record[0] >> 48) & 0xFFF) as u16;
    let address = record[1];
    if code != IO_PAGE_FAULT {
        return Event::Other { devid, code };
    }
    let interrupt = flags & INTERRUPT != 0;
    if interrupt && INTERRUPT_WINDOW.contains(&address) {
        return Event::Interrupt { devid };
    }
    let write = flags & RW != 0 || (interrupt && flags & QEMU_WRITE != 0);
    Event::Access {
        devid,
        page: address & !0xFFF,
        write,
    }
}

/// Whether a record slot has been written: the unit writes a record whole,
/// and the kernel zeroes each slot it has read (ledger 3867, C2).
#[must_use]
pub const fn written(record: [u64; 2]) -> bool {
    record[0] != 0 || record[1] != 0
}
