// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/gen8_ppgtt.c and
// gt/intel_gtt.c (setup_scratch_page, fill_page_dma):
// Copyright © 2020 Intel Corporation
// Under the MIT notice in this crate's i915/NOTICE.

//! A per-process GTT on Gen12.0: a 4-level, 48-bit tree of 4 KiB tables in
//! memory, as `gen8_ppgtt.c` builds it. Every entry that maps nothing
//! points at a *scratch* table of the level below, down to one zeroed
//! scratch page, so a stray access by a batch reads zeros instead of
//! faulting the engine (`gen8_init_scratch`).
//!
//! i915 mirrors the tree in `struct i915_page_directory` objects with use
//! counts. This crate has no allocator, so the tree is its own record: an
//! entry is "used" when it is not its level's scratch entry, and a table
//! is released when every entry in it is scratch again. The memory itself
//! is the caller's, through [`PtMemory`]: the driver process backs it with
//! pinned pages, a test with a map.
//!
//! Large pages are `gen8_ppgtt_insert_huge`'s ([`Ppgtt::insert_huge`],
//! `docs/INTEL-GPU.md` §14): a level-1 entry with [`gtt::GEN8_PDE_PS_2M`]
//! maps 2 MiB itself and is a leaf every walk here stops at; one with
//! [`gtt::GEN8_PDE_IPS_64K`] names a leaf table whose 512 entries are all
//! written as 4 KiB entries, of which the hardware reads every 16th as a
//! 64 KiB page. Below 12.50 there is no 64 KiB scratch, so IPS is set only
//! on a table one run of pages fills, as i915 sets it (`index ==
//! I915_PDES`), and any clear in such a table drops IPS first.
//!
//! Left out, as ADL-P/N never takes them: the 3-level (32-bit) layout and
//! its preallocated PDPs, the 12.50+ 64 KiB layout (`HAS_64K_PAGES`: 32
//! entries a table and a 64 KiB scratch), local memory, vGPU notification
//! and `Wa_16018031267`'s reserved page (12.55+). Gen12 has no read-only
//! PTEs (`has_read_only` is false on 11 and 12, "HSDES#:1807136187"), so
//! [`gtt::PTE_READ_ONLY`] is never given here.

use crate::gtt::{
    self, CacheLevel, GEN8_PDE_IPS_64K, GEN8_PDE_PS_2M, GEN8_PDES, GEN8_PTE_SHIFT, PPGTT_TOP_48,
    gen8_pd_shift, gen8_pde_encode, gen12_pte_encode,
};

/// `I915_GTT_PAGE_SIZE_4K` as a table's size.
pub const PT_BYTES: u64 = 4096;
/// `I915_GTT_PAGE_SIZE_64K`.
pub const PAGE_64K: u64 = 64 << 10;
/// `I915_GTT_PAGE_SIZE_2M`: what one level-1 entry covers.
pub const PAGE_2M: u64 = 2 << 20;
/// `vm->total` for a 4-level VM: 48 bits.
pub const PPGTT_TOTAL_48: u64 = 1 << 48;
/// The address bits of an entry at any level (bits 47..12).
pub const PTE_ADDR_MASK: u64 = ((1 << 48) - 1) & !((1 << 12) - 1);

/// The memory the tables live in: pages the device can read, by their
/// device addresses. `vm->alloc_pt_dma`, `px_vaddr` and `free_px`.
pub trait PtMemory {
    /// A 4 KiB page for a table (or the scratch page), its device address;
    /// `None` when there is none left. Its contents need not be zero.
    fn alloc_pt(&mut self) -> Option<u64>;
    /// Give a table back.
    fn free_pt(&mut self, table: u64);
    /// Entry `index` (0..512) of the table at `table`.
    fn read_entry(&mut self, table: u64, index: usize) -> u64;
    /// Write entry `index` of the table at `table`. Whatever makes the write
    /// seen by the GPU's walk (`drm_clflush_virt_range`) is the
    /// implementation's.
    fn write_entry(&mut self, table: u64, index: usize, value: u64);
}

/// Why a PPGTT operation failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PpgttError {
    /// `-ENOMEM`: no page for a table.
    NoMemory,
    /// A range that is empty, not page-aligned or past `vm->total`
    /// (i915's `GEM_BUG_ON`s, reported).
    Range,
    /// An insert into a range [`Ppgtt::alloc_va_range`] did not allocate.
    NotAllocated,
}

/// `fill_page_dma`: every entry of `table` set to `value`.
fn fill_px<M: PtMemory + ?Sized>(mem: &mut M, table: u64, value: u64) {
    for index in 0..GEN8_PDES as usize {
        mem.write_entry(table, index, value);
    }
}

/// `gen8_pd_range`: from page index `start` towards `end` at level `lvl`,
/// the first entry's index and how many entries of this table are touched.
#[must_use]
pub const fn gen8_pd_range(start: u64, end: u64, lvl: u32) -> (usize, usize) {
    let shift = gen8_pd_shift(lvl);
    let mask = !0_u64 << gen8_pd_shift(lvl + 1);
    let end = end + (!mask >> gen8_pd_shift(1));
    let idx = ((start >> shift) & (GEN8_PDES - 1)) as usize;
    if (start ^ end) & mask != 0 {
        (idx, GEN8_PDES as usize - idx)
    } else {
        (idx, ((end >> shift) & (GEN8_PDES - 1)) as usize - idx)
    }
}

/// `gen8_pt_count`: how many leaf entries of one table `start..end` (page
/// indices) covers from `start`.
#[must_use]
pub const fn gen8_pt_count(start: u64, end: u64) -> u64 {
    if (start ^ end) >> gen8_pd_shift(1) != 0 {
        GEN8_PDES - (start & (GEN8_PDES - 1))
    } else {
        end - start
    }
}

/// `gen8_pd_index(i, lvl)`.
#[must_use]
pub const fn gen8_pd_index(page: u64, lvl: u32) -> usize {
    ((page >> gen8_pd_shift(lvl)) & (GEN8_PDES - 1)) as usize
}

/// A 4-level PPGTT (`struct i915_ppgtt` for `vm->top == 3`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ppgtt {
    /// `px_dma(ppgtt->pd)`: the PML4, which a context image's `PDP0` names
    /// (`init_ppgtt_regs`, `ASSIGN_CTX_PML4`).
    pub pml4: u64,
    /// `vm->scratch[i]`'s device address: 0 the zeroed page, 1 a table of
    /// scratch PTEs, 2 and 3 directories of scratch entries.
    pub scratch: [u64; 4],
    /// `vm->scratch[i]->encode`: the entry that points at `scratch[i]`, which
    /// is what an unused entry at level `i` holds.
    pub scratch_encode: [u64; 4],
}

impl Ppgtt {
    /// `gen8_ppgtt_create` for Gen12 without local memory: `gen8_init_scratch`
    /// (the scratch page and one scratch table per level, each filled with
    /// the encode of the one below), then `gen8_alloc_top_pd` (the PML4,
    /// every one of its `gen8_pd_top_count` = 512 entries scratch).
    ///
    /// The scratch page is filled with zeroes here (`setup_scratch_page`
    /// clears it), through [`PtMemory::write_entry`].
    ///
    /// # Errors
    ///
    /// [`PpgttError::NoMemory`]; the pages taken so far are given back.
    pub fn create<M: PtMemory + ?Sized>(mem: &mut M) -> Result<Self, PpgttError> {
        let mut taken = [0_u64; 5];
        let mut count = 0;
        let result = Self::create_into(mem, &mut taken, &mut count);
        if result.is_err() {
            for &page in taken.iter().take(count) {
                mem.free_pt(page);
            }
        }
        result
    }

    fn create_into<M: PtMemory + ?Sized>(
        mem: &mut M,
        taken: &mut [u64; 5],
        count: &mut usize,
    ) -> Result<Self, PpgttError> {
        let mut take = |mem: &mut M| -> Result<u64, PpgttError> {
            let page = mem.alloc_pt().ok_or(PpgttError::NoMemory)?;
            if let Some(slot) = taken.get_mut(*count) {
                *slot = page;
            }
            *count += 1;
            Ok(page)
        };
        let mut scratch = [0_u64; 4];
        let mut scratch_encode = [0_u64; 4];
        // setup_scratch_page
        let page = take(mem)?;
        fill_px(mem, page, 0);
        scratch[0] = page;
        // "vm->pte_encode(px_dma(vm->scratch[0]), I915_CACHE_NONE, pte_flags)",
        // pte_flags = has_read_only = 0 on Gen12.
        scratch_encode[0] = gen12_pte_encode(page, gtt::cachelevel_to_pat(CacheLevel::None), 0);
        let mut below = scratch_encode[0];
        for (table_at, encode_at) in scratch.iter_mut().zip(scratch_encode.iter_mut()).skip(1) {
            let table = take(mem)?;
            fill_px(mem, table, below);
            *table_at = table;
            below = gen8_pde_encode(table, CacheLevel::None);
            *encode_at = below;
        }
        // gen8_alloc_top_pd
        let pml4 = take(mem)?;
        fill_px(mem, pml4, below);
        Ok(Ppgtt {
            pml4,
            scratch,
            scratch_encode,
        })
    }

    fn scratch_at(&self, lvl: u32) -> u64 {
        self.scratch_encode.get(lvl as usize).copied().unwrap_or(0)
    }

    /// The range `[start, start + length)` as page indices, checked as
    /// i915's `GEM_BUG_ON`s check it.
    fn pages(start: u64, length: u64) -> Result<(u64, u64), PpgttError> {
        let aligned = |x: u64| x.is_multiple_of(1 << GEN8_PTE_SHIFT);
        let end = start.checked_add(length).ok_or(PpgttError::Range)?;
        if length == 0 || !aligned(start) || !aligned(length) || end > PPGTT_TOTAL_48 {
            return Err(PpgttError::Range);
        }
        Ok((start >> GEN8_PTE_SHIFT, end >> GEN8_PTE_SHIFT))
    }

    /// `gen8_ppgtt_alloc`: make sure every table on the way to each page of
    /// `[start, start + length)` exists. New tables are filled with their
    /// level's scratch entries (i915 skips that for a leaf table the range
    /// covers whole, which it then writes in full; here every new table is
    /// filled, so a table is never seen half-written).
    ///
    /// # Errors
    ///
    /// [`PpgttError::Range`], [`PpgttError::NoMemory`]. Tables made before
    /// the failure stay, as i915's stash would leave them to the next call.
    pub fn alloc_va_range<M: PtMemory + ?Sized>(
        &self,
        mem: &mut M,
        start: u64,
        length: u64,
    ) -> Result<(), PpgttError> {
        let (mut start, end) = Self::pages(start, length)?;
        self.alloc_level(mem, self.pml4, &mut start, end, PPGTT_TOP_48)
    }

    /// `__gen8_ppgtt_alloc`.
    fn alloc_level<M: PtMemory + ?Sized>(
        &self,
        mem: &mut M,
        pd: u64,
        start: &mut u64,
        end: u64,
        lvl: u32,
    ) -> Result<(), PpgttError> {
        let (idx, len) = gen8_pd_range(*start, end, lvl);
        let lvl = lvl - 1;
        for idx in idx..idx + len {
            let mut entry = mem.read_entry(pd, idx);
            if lvl == 0 && entry & GEN8_PDE_PS_2M != 0 {
                // A 2 MiB page is there: an overlapping bind.
                return Err(PpgttError::Range);
            }
            if entry == self.scratch_at(lvl + 1) {
                let pt = mem.alloc_pt().ok_or(PpgttError::NoMemory)?;
                fill_px(mem, pt, self.scratch_at(lvl));
                // __set_pd_entry: gen8_pde_encode(px_dma(to), I915_CACHE_LLC)
                entry = gen8_pde_encode(pt, CacheLevel::Llc);
                mem.write_entry(pd, idx, entry);
            }
            if lvl > 0 {
                self.alloc_level(mem, entry & PTE_ADDR_MASK, start, end, lvl)?;
            } else {
                *start += gen8_pt_count(*start, end);
            }
        }
        Ok(())
    }

    /// The leaf table holding page index `page`, if allocated: the walk of
    /// `gen8_ppgtt_insert_entry` (`gen8_pdp_for_page_index`, then levels 2
    /// and 1).
    ///
    /// A 2 MiB page at level 1 has no leaf table: `None`. A table whose
    /// level-1 entry has IPS is its 64 KiB layout no more after this, which
    /// only an overlapping 4 KiB insert asks: IPS is dropped first.
    fn leaf<M: PtMemory + ?Sized>(&self, mem: &mut M, page: u64) -> Option<u64> {
        let pd = self.directory(mem, page)?;
        let index = gen8_pd_index(page, 1);
        let entry = mem.read_entry(pd, index);
        if entry == self.scratch_at(1) || entry & GEN8_PDE_PS_2M != 0 {
            return None;
        }
        if entry & GEN8_PDE_IPS_64K != 0 {
            mem.write_entry(pd, index, entry & !GEN8_PDE_IPS_64K);
        }
        Some(entry & PTE_ADDR_MASK)
    }

    /// The level-1 table (the leaf tables' directory) holding page index
    /// `page`'s entry, if allocated.
    fn directory<M: PtMemory + ?Sized>(&self, mem: &mut M, page: u64) -> Option<u64> {
        let mut table = self.pml4;
        for lvl in (2..=PPGTT_TOP_48).rev() {
            let entry = mem.read_entry(table, gen8_pd_index(page, lvl));
            if entry == self.scratch_at(lvl) {
                return None;
            }
            table = entry & PTE_ADDR_MASK;
        }
        Some(table)
    }

    /// `gen8_ppgtt_insert_entry`: map the page at device address `addr` at
    /// GPU address `offset`, with the PAT index `pat_index` (at most
    /// [`gtt::MAX_PAT_INDEX`]).
    ///
    /// # Errors
    ///
    /// [`PpgttError::Range`] for an offset outside the VM, an unaligned
    /// address or a PAT index above the table; [`PpgttError::NotAllocated`].
    pub fn insert_entry<M: PtMemory + ?Sized>(
        &self,
        mem: &mut M,
        addr: u64,
        offset: u64,
        pat_index: u32,
    ) -> Result<(), PpgttError> {
        let (page, _) = Self::pages(offset, PT_BYTES)?;
        if addr & !PTE_ADDR_MASK != 0 || pat_index > gtt::MAX_PAT_INDEX {
            return Err(PpgttError::Range);
        }
        let leaf = self.leaf(mem, page).ok_or(PpgttError::NotAllocated)?;
        mem.write_entry(
            leaf,
            gen8_pd_index(page, 0),
            gen12_pte_encode(addr, pat_index, 0),
        );
        Ok(())
    }

    /// `gen8_ppgtt_insert` for 4 KiB pages: `pages` (device addresses, in
    /// order) mapped from `offset` on. Returns how many were written.
    ///
    /// # Errors
    ///
    /// As [`Ppgtt::insert_entry`], at the first page that fails.
    pub fn insert_entries<M: PtMemory + ?Sized>(
        &self,
        mem: &mut M,
        pages: impl IntoIterator<Item = u64>,
        offset: u64,
        pat_index: u32,
    ) -> Result<usize, PpgttError> {
        let mut written = 0;
        for (n, addr) in pages.into_iter().enumerate() {
            let at = offset
                .checked_add(n as u64 * PT_BYTES)
                .ok_or(PpgttError::Range)?;
            self.insert_entry(mem, addr, at, pat_index)?;
            written += 1;
        }
        Ok(written)
    }

    /// `gen8_ppgtt_insert_huge` for one 2 MiB window: `pages` (device
    /// addresses, in order) mapped from `offset` on, all inside the 2 MiB
    /// that `offset` is in, with the tables they need allocated here.
    ///
    /// * A whole window from a 2 MiB-aligned address, in one run, is one
    ///   2 MiB entry at level 1 (`GEN8_PDE_PS_2M`), with no leaf table; a
    ///   leaf table there that holds only scratch is given back.
    /// * A whole window whose every 64 KiB is one run from a 64 KiB-aligned
    ///   address is 512 4 KiB entries, then `GEN8_PDE_IPS_64K`.
    /// * Anything else is 4 KiB entries.
    ///
    /// Returns the page size used.
    ///
    /// # Errors
    ///
    /// As [`Ppgtt::alloc_va_range`] and [`Ppgtt::insert_entry`];
    /// [`PpgttError::Range`] for pages past the window, and for a 2 MiB
    /// entry over a leaf table that maps anything (an overlapping bind).
    pub fn insert_huge<M: PtMemory + ?Sized>(
        &self,
        mem: &mut M,
        pages: &[u64],
        offset: u64,
        pat_index: u32,
    ) -> Result<u64, PpgttError> {
        let bytes = pages.len() as u64 * PT_BYTES;
        let (page, end) = Self::pages(offset, bytes)?;
        if (page ^ (end - 1)) >> gen8_pd_shift(1) != 0
            || pat_index > gtt::MAX_PAT_INDEX
            || pages.iter().any(|addr| addr & !PTE_ADDR_MASK != 0)
        {
            return Err(PpgttError::Range);
        }
        let whole = bytes == PAGE_2M;
        // Whether `len` pages from `from` are one run from an `align`-aligned
        // address.
        let run = |from: usize, len: usize, align: u64| {
            pages.get(from).is_some_and(|&first| {
                first.is_multiple_of(align)
                    && pages
                        .iter()
                        .skip(from)
                        .take(len)
                        .zip(0..)
                        .all(|(&addr, n)| addr == first + n * PT_BYTES)
            })
        };
        let per_64k = (PAGE_64K / PT_BYTES) as usize;
        if let Some(&first) = pages.first()
            && whole
            && run(0, pages.len(), PAGE_2M)
        {
            self.insert_2m(mem, first, offset, pat_index)?;
            return Ok(PAGE_2M);
        }
        self.alloc_va_range(mem, offset, bytes)?;
        let leaf = self.leaf(mem, page).ok_or(PpgttError::NotAllocated)?;
        let first = gen8_pd_index(page, 0);
        for (n, &addr) in pages.iter().enumerate() {
            mem.write_entry(leaf, first + n, gen12_pte_encode(addr, pat_index, 0));
        }
        let ips = whole
            && (0..pages.len())
                .step_by(per_64k)
                .all(|n| run(n, per_64k, PAGE_64K));
        if !ips {
            return Ok(PT_BYTES);
        }
        let pd = self.directory(mem, page).ok_or(PpgttError::NotAllocated)?;
        let index = gen8_pd_index(page, 1);
        let entry = mem.read_entry(pd, index);
        mem.write_entry(pd, index, entry | GEN8_PDE_IPS_64K);
        Ok(PAGE_64K)
    }

    /// One 2 MiB entry for the window at `offset`, from `addr`.
    fn insert_2m<M: PtMemory + ?Sized>(
        &self,
        mem: &mut M,
        addr: u64,
        offset: u64,
        pat_index: u32,
    ) -> Result<(), PpgttError> {
        // The tables above level 1: allocating the window's first page
        // makes them, and a leaf table for it, which goes again below.
        self.alloc_va_range(mem, offset, PT_BYTES)?;
        let page = offset >> GEN8_PTE_SHIFT;
        let pd = self.directory(mem, page).ok_or(PpgttError::NotAllocated)?;
        let index = gen8_pd_index(page, 1);
        let entry = mem.read_entry(pd, index);
        if entry != self.scratch_at(1) {
            let pt = entry & PTE_ADDR_MASK;
            if !self.is_all_scratch(mem, pt, 0) {
                return Err(PpgttError::Range);
            }
            mem.write_entry(pd, index, self.scratch_at(1));
            mem.free_pt(pt);
        }
        mem.write_entry(
            pd,
            index,
            gen12_pte_encode(addr, pat_index, 0) | GEN8_PDE_PS_2M,
        );
        Ok(())
    }

    /// `gen8_ppgtt_clear`: every page of `[start, start + length)` back to
    /// the scratch page, and every table left holding only scratch entries
    /// released (`release_pd_entry`, `free_px`).
    ///
    /// # Errors
    ///
    /// [`PpgttError::Range`].
    pub fn clear_range<M: PtMemory + ?Sized>(
        &self,
        mem: &mut M,
        start: u64,
        length: u64,
    ) -> Result<(), PpgttError> {
        let (start, end) = Self::pages(start, length)?;
        let _ = self.clear_level(mem, self.pml4, start, end, PPGTT_TOP_48);
        Ok(())
    }

    /// `__gen8_ppgtt_clear`, which returns where it got to.
    fn clear_level<M: PtMemory + ?Sized>(
        &self,
        mem: &mut M,
        pd: u64,
        mut start: u64,
        end: u64,
        lvl: u32,
    ) -> u64 {
        let (idx, len) = gen8_pd_range(start, end, lvl);
        let scratch = self.scratch_at(lvl);
        let lvl = lvl - 1;
        for idx in idx..idx + len {
            let entry = mem.read_entry(pd, idx);
            if entry == scratch {
                // Nothing below: skip what this entry covers.
                let span = 1_u64 << gen8_pd_shift(lvl + 1);
                start = (start & !(span - 1)) + span;
                continue;
            }
            if lvl == 0 && entry & GEN8_PDE_PS_2M != 0 {
                // A 2 MiB page goes whole: one binding alone ever covers it.
                mem.write_entry(pd, idx, scratch);
                let span = 1_u64 << gen8_pd_shift(1);
                start = (start & !(span - 1)) + span;
                continue;
            }
            if lvl == 0 && entry & GEN8_PDE_IPS_64K != 0 {
                // Back to 4 KiB pages before any entry turns scratch: a
                // scratch entry read as 64 KiB would reach past the
                // scratch page.
                mem.write_entry(pd, idx, entry & !GEN8_PDE_IPS_64K);
            }
            let pt = entry & PTE_ADDR_MASK;
            if lvl > 0 {
                start = self.clear_level(mem, pt, start, end, lvl);
            } else {
                let count = gen8_pt_count(start, end);
                let first = gen8_pd_index(start, 0);
                for pte in first..first + count as usize {
                    mem.write_entry(pt, pte, self.scratch_at(0));
                }
                start += count;
            }
            if self.is_all_scratch(mem, pt, lvl) {
                mem.write_entry(pd, idx, scratch);
                mem.free_pt(pt);
            }
        }
        start.min(end)
    }

    fn is_all_scratch<M: PtMemory + ?Sized>(&self, mem: &mut M, table: u64, lvl: u32) -> bool {
        let scratch = self.scratch_at(lvl);
        (0..GEN8_PDES as usize).all(|index| mem.read_entry(table, index) == scratch)
    }

    /// The device address GPU address `offset` reaches, walking the tree as
    /// the GPU does (for checks and hang reports): `None` for the scratch
    /// page or an entry not present.
    pub fn translate<M: PtMemory + ?Sized>(&self, mem: &mut M, offset: u64) -> Option<u64> {
        let page = offset >> GEN8_PTE_SHIFT;
        let mut table = self.pml4;
        let mut within = PT_BYTES;
        for lvl in (0..=PPGTT_TOP_48).rev() {
            let mut index = gen8_pd_index(page, lvl);
            if lvl == 0 && within == PAGE_64K {
                index &= !((PAGE_64K / PT_BYTES) as usize - 1);
            }
            let entry = mem.read_entry(table, index);
            if entry & gtt::GEN8_PAGE_PRESENT == 0 || entry == self.scratch_at(lvl) {
                return None;
            }
            if lvl == 1 && entry & GEN8_PDE_PS_2M != 0 {
                return Some((entry & PTE_ADDR_MASK) | (offset & (PAGE_2M - 1)));
            }
            if lvl == 1 && entry & GEN8_PDE_IPS_64K != 0 {
                within = PAGE_64K;
            }
            table = entry & PTE_ADDR_MASK;
        }
        Some(table | (offset & (within - 1)))
    }

    /// `gen8_ppgtt_cleanup` for a VM that [`Ppgtt::clear_range`] emptied:
    /// the PML4 and the scratch tables given back. Whatever is still mapped
    /// is the caller's to clear first; its tables would leak.
    pub fn destroy<M: PtMemory + ?Sized>(self, mem: &mut M) {
        mem.free_pt(self.pml4);
        for page in self.scratch {
            mem.free_pt(page);
        }
    }
}
