//! Base address registers: where a function's registers are, and how big.
//!
//! A BAR holds an address and, in its low bits, what kind of address it is —
//! I/O port or memory, 32 or 64 bits wide, prefetchable or not. The *size*
//! is not stored anywhere. It is found by writing all ones and reading back
//! which bits stuck: the bits a device hardwires to zero below its aperture
//! are the ones inside it, so the mask read back is one run of ones, and the
//! run's lowest bit is the size.
//!
//! [`size`] does that, and does the two things a sizing routine forgets.
//!
//! * **Decoding is switched off first.** Between the write of all ones and the
//!   restore, the BAR claims an aperture at the top of the address space. A
//!   function still decoding memory at that moment answers reads and writes
//!   meant for whatever really lives there — on most machines, firmware.
//! * **Both halves of a 64-bit BAR are sized and restored.** Size only the
//!   low half and a 4 GiB aperture reads as a 0-byte one; restore only the low
//!   half and the device is left decoding somewhere above 4 GiB that nothing
//!   assigned it.
//!
//! Neither mistake is visible until two devices share a machine, which on
//! QEMU is never, and on a board is the first time anything is plugged in.

use crate::header::{BAR0, COMMAND, COMMAND_IO_SPACE, COMMAND_MEMORY_SPACE, HeaderKind};
use crate::{Address, ConfigSpace, PciError};

/// The low bit: set for an I/O BAR, clear for a memory BAR.
pub const IO_SPACE: u32 = 1 << 0;
/// The bits of an I/O BAR that are not address.
pub const IO_FLAGS: u32 = 0x3;
/// The bits of a memory BAR that are not address.
pub const MEMORY_FLAGS: u32 = 0xF;
/// A memory BAR's type field, bits 2:1.
pub const MEMORY_TYPE: u32 = 0x6;
/// Memory type: a 32-bit BAR.
pub const MEMORY_TYPE_32: u32 = 0x0;
/// Memory type: a 64-bit BAR, whose upper half is the next slot.
pub const MEMORY_TYPE_64: u32 = 0x4;
/// A memory BAR's prefetchable bit.
pub const MEMORY_PREFETCHABLE: u32 = 1 << 3;

/// What a BAR says, without its size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bar {
    /// A memory aperture.
    Memory {
        /// The address firmware assigned. Zero usually means none was.
        address: u64,
        /// Whether the BAR is 64 bits wide and so occupies two slots.
        wide: bool,
        /// Whether reads have no side effects, so the aperture may be mapped
        /// cacheable.
        prefetchable: bool,
    },
    /// A range of I/O ports, which only x86 has a way to reach.
    Io {
        /// The first port.
        port: u32,
    },
}

impl Bar {
    /// How many BAR slots this BAR occupies.
    #[must_use]
    pub const fn slots(self) -> u8 {
        match self {
            Bar::Memory { wide: true, .. } => 2,
            Bar::Memory { wide: false, .. } | Bar::Io { .. } => 1,
        }
    }

    /// The address or port, widened.
    #[must_use]
    pub const fn address(self) -> u64 {
        match self {
            Bar::Memory { address, .. } => address,
            Bar::Io { port } => port as u64,
        }
    }
}

/// The offset of BAR slot `index`.
const fn offset(index: u8) -> u16 {
    BAR0 + 4 * index as u16
}

/// Decode the BAR in slot `index`, given its raw value and a way to read the
/// slot above it.
fn decode<C: ConfigSpace + ?Sized>(
    space: &C,
    function: Address,
    slots: u8,
    index: u8,
) -> Result<Bar, PciError> {
    let raw = space.read32(function, offset(index));
    if raw & IO_SPACE != 0 {
        return Ok(Bar::Io {
            port: raw & !IO_FLAGS,
        });
    }
    let prefetchable = raw & MEMORY_PREFETCHABLE != 0;
    let low = u64::from(raw & !MEMORY_FLAGS);
    match raw & MEMORY_TYPE {
        MEMORY_TYPE_32 => Ok(Bar::Memory {
            address: low,
            wide: false,
            prefetchable,
        }),
        MEMORY_TYPE_64 => {
            let upper = index + 1;
            if upper >= slots {
                return Err(PciError::TruncatedBar { function, index });
            }
            let high = u64::from(space.read32(function, offset(upper)));
            Ok(Bar::Memory {
                address: high << 32 | low,
                wide: true,
                prefetchable,
            })
        }
        // 0b01 was "below 1 MiB" in PCI 2.x and is reserved since 3.0; 0b11
        // was never assigned.
        _ => Err(PciError::ReservedBarType { function, index }),
    }
}

/// The BARs of one function, in slot order, with the upper half of each
/// 64-bit BAR skipped rather than decoded as a BAR of its own.
///
/// A decoding error ends the walk: once a type field is reserved, whether the
/// next slot is a BAR or an upper half is unknowable.
#[derive(Debug)]
pub struct Bars<'s, C: ?Sized> {
    /// The space being read.
    space: &'s C,
    /// The function.
    function: Address,
    /// The header's slot count.
    slots: u8,
    /// The next slot to decode.
    next: u8,
}

impl<'s, C: ConfigSpace + ?Sized> Bars<'s, C> {
    /// The BARs of `function`, whose header is of `kind`.
    pub const fn new(space: &'s C, function: Address, kind: HeaderKind) -> Self {
        Bars {
            space,
            function,
            slots: kind.bar_slots(),
            next: 0,
        }
    }
}

impl<C: ConfigSpace + ?Sized> Iterator for Bars<'_, C> {
    type Item = Result<(u8, Bar), PciError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.slots {
            return None;
        }
        let index = self.next;
        match decode(self.space, self.function, self.slots, index) {
            Ok(bar) => {
                self.next = index + bar.slots();
                Some(Ok((index, bar)))
            }
            Err(error) => {
                self.next = self.slots;
                Some(Err(error))
            }
        }
    }
}

/// Decode the BAR in slot `index`.
///
/// # Errors
///
/// [`PciError::NoSuchBar`] if the header has no such slot or the slot is the
/// upper half of a 64-bit BAR; the decoding errors of any slot below it,
/// because those decide where the BARs start.
pub fn read<C: ConfigSpace + ?Sized>(
    space: &C,
    function: Address,
    kind: HeaderKind,
    index: u8,
) -> Result<Bar, PciError> {
    for found in Bars::new(space, function, kind) {
        let (at, bar) = found?;
        if at == index {
            return Ok(bar);
        }
        if at > index {
            break;
        }
    }
    Err(PciError::NoSuchBar { function, index })
}

/// A BAR with its size.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Region {
    /// The slot the BAR is in.
    pub index: u8,
    /// What the BAR says.
    pub bar: Bar,
    /// The aperture's size in bytes, a power of two.
    pub size: u64,
}

impl Region {
    /// The last address inside the aperture. Cannot overflow: [`size`] refuses
    /// an address that is not a multiple of the size. The first address past
    /// it can — a 64-bit BAR
    /// may legitimately end at the top of the address space — which is why
    /// this is not `end`.
    #[must_use]
    pub const fn last(self) -> u64 {
        self.bar.address() + (self.size - 1)
    }

    /// Whether `len` bytes at `offset` into the aperture are inside it.
    #[must_use]
    pub const fn contains(self, offset: u64, len: u64) -> bool {
        match offset.checked_add(len) {
            Some(end) => end <= self.size,
            None => false,
        }
    }
}

/// Size the BAR in slot `index`, restoring it and the command register.
///
/// Returns `None` for a slot the device does not implement, which reads back
/// zero whatever is written.
///
/// # Errors
///
/// As [`read`], plus [`PciError::BarMask`] for a mask that describes no size
/// and [`PciError::BarPlacement`] for an address that is not aligned to the
/// size. In every
/// case the BAR and the command register have already been restored.
pub fn size<C: ConfigSpace + ?Sized>(
    space: &mut C,
    function: Address,
    kind: HeaderKind,
    index: u8,
) -> Result<Option<Region>, PciError> {
    let bar = read(space, function, kind, index)?;

    let command = space.read16(function, COMMAND);
    space.write16(
        function,
        COMMAND,
        command & !(COMMAND_IO_SPACE | COMMAND_MEMORY_SPACE),
    );

    let low = probe(space, function, offset(index));
    let high = match bar {
        Bar::Memory { wide: true, .. } => Some(probe(space, function, offset(index + 1))),
        Bar::Memory { wide: false, .. } | Bar::Io { .. } => None,
    };

    space.write16(function, COMMAND, command);

    let Some(mask) = mask(bar, low, high) else {
        return Ok(None);
    };
    // The size is the lowest bit that stuck. The bits that stuck must be one
    // run from there up — to the top of the BAR, or to the highest address
    // bit the device decodes, which is often well below it — and adding the
    // size to a single run carries it all the way out, leaving a power of
    // two or, for a run reaching bit 63, zero.
    #[expect(
        clippy::manual_isolate_lowest_one,
        reason = "`isolate_lowest_one` is newer than Ferrocene's Rust 1.95, which builds the kernel"
    )]
    let size = mask & mask.wrapping_neg();
    let carried = mask.wrapping_add(size);
    if carried & carried.wrapping_sub(1) != 0 {
        return Err(PciError::BarMask {
            function,
            index,
            mask,
        });
    }
    let below = size - 1;

    // An address that is a multiple of the size leaves at least `size` bytes
    // below the top of its register's width, so alignment is the whole check.
    if bar.address() & below != 0 {
        return Err(PciError::BarPlacement { function, index });
    }

    Ok(Some(Region { index, bar, size }))
}

/// Write all ones to the register at `at`, read what stuck, and put back what
/// was there.
fn probe<C: ConfigSpace + ?Sized>(space: &mut C, function: Address, at: u16) -> u32 {
    let original = space.read32(function, at);
    space.write32(function, at, u32::MAX);
    let probed = space.read32(function, at);
    space.write32(function, at, original);
    probed
}

/// The address bits a probe found writable, or `None` if the BAR is
/// unimplemented and none were.
fn mask(bar: Bar, low: u32, high: Option<u32>) -> Option<u64> {
    let bits = match bar {
        Bar::Io { .. } => u64::from(low & !IO_FLAGS),
        Bar::Memory { .. } => {
            u64::from(low & !MEMORY_FLAGS) | high.map_or(0, |high| u64::from(high) << 32)
        }
    };
    (bits != 0).then_some(bits)
}
