//! Physical Region Pages: how a command says where its data is (NVMe Base
//! Specification 2.0, section 4.1.1).
//!
//! A command has two PRP entries. The first is the address of the data's
//! first byte and may point into a page; everything after it is whole
//! pages. With `MPS` = 0 a page is 4 KiB.
//!
//! * Data that ends in the first page: the second entry is unused.
//! * Data that ends in the second page: the second entry is that page.
//! * Anything longer: the second entry is the address of a *PRP list*, a
//!   page of 512 eight-byte entries naming the pages after the first. A
//!   transfer needing more than 512 of them uses the list's last entry as
//!   the address of the next list page, and so on.
//!
//! The pages are the data region's own, pinned once; nothing is copied.
//! Their device addresses need not be contiguous, which is why a transfer
//! beyond two pages needs the list at all.

use crate::Memory;
use crate::regs::PAGE;

/// Entries in one PRP list page.
pub const LIST_ENTRIES: usize = PAGE / 8;

/// Why a transfer has no PRPs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrpError {
    /// No bytes to move.
    Empty,
    /// The first byte is not on a four-byte boundary, which a PRP entry
    /// cannot say.
    Unaligned,
    /// The transfer runs past the data region.
    OutsideData,
    /// More list pages are needed than the command has.
    TooLong,
}

/// The two PRP entries of a command.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Prp {
    /// PRP1: the data's first byte.
    pub first: u64,
    /// PRP2: nothing, the second page, or the list.
    pub second: u64,
}

/// How many list pages a transfer of `pages_after_first` pages beyond the
/// first needs.
#[must_use]
pub const fn list_pages_for(pages_after_first: usize) -> usize {
    if pages_after_first <= 1 {
        return 0;
    }
    // Every list page but the last gives one entry to the chain.
    let mut pages = 1;
    let mut left = pages_after_first;
    while left > LIST_ENTRIES {
        left -= LIST_ENTRIES - 1;
        pages += 1;
    }
    pages
}

/// The PRPs for `length` bytes at `offset` of `data`, writing a list, if
/// one is needed, into the `list_pages` pages of `lists` from page
/// `first_list_page`.
///
/// # Errors
///
/// [`PrpError`]; nothing the controller reads has been published when it
/// is returned.
pub fn build<D: Memory, L: Memory>(
    data: &D,
    lists: &mut L,
    first_list_page: usize,
    list_pages: usize,
    offset: u64,
    length: u64,
) -> Result<Prp, PrpError> {
    if length == 0 {
        return Err(PrpError::Empty);
    }
    if !offset.is_multiple_of(4) {
        return Err(PrpError::Unaligned);
    }
    let pages = data.pages();
    let end = offset.checked_add(length).ok_or(PrpError::OutsideData)?;
    if end > (pages.len() as u64) * PAGE as u64 {
        return Err(PrpError::OutsideData);
    }
    let first_page = usize::try_from(offset / PAGE as u64).map_err(|_| PrpError::OutsideData)?;
    let last_page = usize::try_from((end - 1) / PAGE as u64).map_err(|_| PrpError::OutsideData)?;
    let within = offset % PAGE as u64;
    let address = |page: usize| pages.get(page).copied().ok_or(PrpError::OutsideData);
    let first = address(first_page)? + within;
    let after = last_page - first_page;
    let second = match after {
        0 => 0,
        1 => address(first_page + 1)?,
        _ => {
            if list_pages_for(after) > list_pages {
                return Err(PrpError::TooLong);
            }
            write_list(pages, lists, first_list_page, first_page + 1, after)?
        }
    };
    Ok(Prp { first, second })
}

/// Write the list naming `count` data pages from `from`, chained through as
/// many list pages from `first_list_page` as it takes; the first list
/// page's address.
fn write_list<L: Memory>(
    data_pages: &[u64],
    lists: &mut L,
    first_list_page: usize,
    from: usize,
    count: usize,
) -> Result<u64, PrpError> {
    let list_address =
        |lists: &L, page: usize| lists.pages().get(page).copied().ok_or(PrpError::TooLong);
    let mut list_page = first_list_page;
    let mut slot = 0;
    for index in 0..count {
        let left = count - index;
        // The last slot of a list page chains to the next, unless the
        // entries left fit without it.
        if slot == LIST_ENTRIES - 1 && left > 1 {
            let next = list_address(lists, list_page + 1)?;
            put(lists, list_page, slot, next);
            list_page += 1;
            slot = 0;
        }
        let entry = data_pages
            .get(from + index)
            .copied()
            .ok_or(PrpError::OutsideData)?;
        put(lists, list_page, slot, entry);
        slot += 1;
    }
    list_address(lists, first_list_page)
}

/// Write list entry `slot` of list page `page`.
fn put<L: Memory>(lists: &mut L, page: usize, slot: usize, value: u64) {
    let at = page * PAGE + slot * 8;
    for (index, byte) in value.to_le_bytes().into_iter().enumerate() {
        lists.write_u8(at + index, byte);
    }
}
