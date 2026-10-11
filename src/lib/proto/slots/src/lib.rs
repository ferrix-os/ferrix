//! A/B system slots: the formats the loader and `bootctl` share
//! (`docs/AB-BOOT.md`).
//!
//! * [`record`]: the boot control record, its two copies, which one is
//!   current, and where the next write goes (§3).
//! * [`slot`]: a slot's header and its three blobs, and when a slot
//!   verifies (§2).
//! * [`gpt`]: finding the control partition and the two slots in a GUID
//!   partition table by their Ferrix type GUIDs.
//! * [`plan`]: what the loader tries, in which order, and the command-line
//!   words it puts first (§4).
//! * [`crc`] and [`sha256`]: the checksums the formats use.
//!
//! Pure functions of bytes, with no allocation and no `unsafe`, so the
//! loader can run them before it has anything but firmware's pages, and the
//! host tests reach every one of them.

#![no_std]
#![forbid(unsafe_code)]

pub mod crc;
pub mod gpt;
pub mod plan;
pub mod record;
pub mod sha256;
pub mod slot;

#[cfg(test)]
mod tests;

/// The two slots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// The slot with the lower GPT entry index.
    A,
    /// The other one.
    B,
}

impl Slot {
    /// Both, in order.
    pub const BOTH: [Slot; 2] = [Slot::A, Slot::B];

    /// The byte the record stores for this slot.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Slot::A => 0,
            Slot::B => 1,
        }
    }

    /// The slot a record byte names, if it names one.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Slot> {
        match code {
            0 => Some(Slot::A),
            1 => Some(Slot::B),
            _ => None,
        }
    }

    /// The other slot.
    #[must_use]
    pub const fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }

    /// `a` or `b`, as the command line says it.
    #[must_use]
    pub const fn letter(self) -> &'static str {
        match self {
            Slot::A => "a",
            Slot::B => "b",
        }
    }

    /// The slot `ferrix.slot=` names.
    #[must_use]
    pub fn from_letter(letter: &str) -> Option<Slot> {
        match letter {
            "a" => Some(Slot::A),
            "b" => Some(Slot::B),
            _ => None,
        }
    }
}

/// Read a little-endian `u16` at `at`, or `None` past the end.
pub(crate) fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    let raw = bytes.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes(raw.try_into().ok()?))
}

/// Read a little-endian `u32` at `at`, or `None` past the end.
pub(crate) fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    let raw = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes(raw.try_into().ok()?))
}

/// Read a little-endian `u64` at `at`, or `None` past the end.
pub(crate) fn le64(bytes: &[u8], at: usize) -> Option<u64> {
    let raw = bytes.get(at..at.checked_add(8)?)?;
    Some(u64::from_le_bytes(raw.try_into().ok()?))
}

/// Copy `value` into `out` at `at`; nothing when it does not fit, which the
/// fixed layouts here never ask for.
pub(crate) fn put(out: &mut [u8], at: usize, value: &[u8]) {
    if let Some(end) = at.checked_add(value.len())
        && let Some(slot) = out.get_mut(at..end)
    {
        slot.copy_from_slice(value);
    }
}
