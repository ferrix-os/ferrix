//! Whether an RMRR can be honoured with an identity domain, and the pages
//! that domain then holds for good (`vtd-design.md` §3.9(b), K2 of
//! `docs/INTEL-GPU.md`).
//!
//! An RMRR (VT-d 4.0 §8.4) is memory firmware says a device keeps reaching
//! whatever the kernel decides: on the N150, the iGPU's stolen memory,
//! which holds the GGTT and the framebuffer the display engine scans out.
//! A unit that translates must map it for that device before translation
//! goes on, at the addresses the device uses, which are the physical ones.
//!
//! The kernel honours one only when doing so opens no hole into memory it
//! owns: the region is whole pages, inside the bits a domain reaches,
//! bounded, and outside every region the memory map calls RAM -- every
//! frame the allocator is ever given is in one of those, and so is
//! everything the kernel keeps. Anything else is refused, and the unit is
//! left remapping only, as before RMRRs were honoured.

use core::fmt;

use super::unit::DOMAIN_BITS;

/// Bytes of a page.
pub const PAGE: u64 = 0x1000;

/// The most pages one RMRR may hold: 1 GiB, which bounds the tables an
/// identity domain takes and the time its maps take at bring-up. The N150's
/// RMRR is 17,408.
pub const MOST_PAGES: u64 = 1 << 18;

/// The pages an honoured RMRR identity-maps: from `base`, `pages` of them,
/// each at its own address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Identity {
    /// The first page's address.
    pub base: u64,
    /// How many pages.
    pub pages: u64,
}

impl Identity {
    /// The first address past the last page.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.base + self.pages * PAGE
    }

    /// Whether `address` lies in one of its pages.
    #[must_use]
    pub const fn holds(self, address: u64) -> bool {
        address >= self.base && address < self.end()
    }

    /// Whether it shares a page with `other`.
    #[must_use]
    pub const fn overlaps(self, other: Identity) -> bool {
        self.base < other.end() && other.base < self.end()
    }

    /// Every page's address, in order.
    pub fn page_addresses(self) -> impl Iterator<Item = u64> {
        (0..self.pages).map(move |page| self.base + page * PAGE)
    }
}

/// Why an RMRR is not honoured.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// Its limit is below its base.
    Inverted,
    /// Its base, or the byte after its limit, is not on a page boundary.
    Unaligned,
    /// It reaches at or above the bits a domain reaches.
    OutOfReach,
    /// It holds more than [`MOST_PAGES`].
    TooLarge,
    /// It shares a byte with a region the memory map calls RAM, which
    /// starts at `base` and is `len` bytes.
    OverRam {
        /// The region's base.
        base: u64,
        /// Its length.
        len: u64,
    },
    /// It shares a page with another RMRR for the same function, at `base`.
    Overlapping {
        /// The other region's base.
        base: u64,
    },
    /// The frame allocator holds the page at `page`: on a free list or
    /// handed out.
    AllocatorFrame {
        /// The page's address.
        page: u64,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Refusal::Inverted => f.write_str("its limit is below its base"),
            Refusal::Unaligned => f.write_str("it is not whole pages"),
            Refusal::OutOfReach => f.write_str("it reaches past the 39 bits a domain reaches"),
            Refusal::TooLarge => f.write_str("it is larger than the 1 GiB an RMRR may be"),
            Refusal::OverRam { base, len } => write!(
                f,
                "it overlaps RAM the memory map gives at {base:#x}-{:#x}",
                base.saturating_add(len).saturating_sub(1)
            ),
            Refusal::Overlapping { base } => {
                write!(f, "it overlaps the function's other RMRR at {base:#x}")
            }
            Refusal::AllocatorFrame { page } => {
                write!(f, "the frame allocator holds its page {page:#x}")
            }
        }
    }
}

/// The identity domain's pages for an RMRR from `base` to `limit`
/// (inclusive, as the table gives it), given every region the memory map
/// calls RAM as `(base, len)`.
///
/// # Errors
///
/// Why it is not honoured: limit below base, not whole pages, at or past
/// 2^39, more than [`MOST_PAGES`], or a byte in RAM.
pub fn identity(
    base: u64,
    limit: u64,
    ram: impl IntoIterator<Item = (u64, u64)>,
) -> Result<Identity, Refusal> {
    if limit < base {
        return Err(Refusal::Inverted);
    }
    let end = limit.checked_add(1).ok_or(Refusal::OutOfReach)?;
    if !base.is_multiple_of(PAGE) || !end.is_multiple_of(PAGE) {
        return Err(Refusal::Unaligned);
    }
    if end > 1 << DOMAIN_BITS {
        return Err(Refusal::OutOfReach);
    }
    let pages = (end - base) / PAGE;
    if pages > MOST_PAGES {
        return Err(Refusal::TooLarge);
    }
    for (at, len) in ram {
        if len != 0 && at < end && base < at.saturating_add(len) {
            return Err(Refusal::OverRam { base: at, len });
        }
    }
    Ok(Identity { base, pages })
}

/// Whether `address` lies in any of `held`'s pages.
#[must_use]
pub fn held(held: &[Identity], address: u64) -> bool {
    held.iter().any(|region| region.holds(address))
}

/// Whether `regions` share no page: two RMRRs for one function may not,
/// since each page is mapped once.
///
/// # Errors
///
/// [`Refusal::Overlapping`] for the first region that overlaps an earlier
/// one, naming the earlier.
pub fn disjoint(regions: &[Identity]) -> Result<(), Refusal> {
    for (index, region) in regions.iter().enumerate() {
        if let Some(earlier) = regions
            .iter()
            .take(index)
            .find(|earlier| earlier.overlaps(*region))
        {
            return Err(Refusal::Overlapping { base: earlier.base });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
