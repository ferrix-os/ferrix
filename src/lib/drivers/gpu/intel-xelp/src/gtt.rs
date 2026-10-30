// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_gtt.h,
// gt/intel_gtt.c, gt/gen8_ppgtt.c, gt/intel_ppgtt.c, gt/intel_ggtt.c,
// i915_pci.c (TGL_CACHELEVEL) and include/drm/intel/i915_drm.h:
// Copyright © 2020 Intel Corporation (intel_gtt.h, intel_gtt.c,
//                                     gen8_ppgtt.c, intel_ggtt.c)
// Copyright © 2019 Intel Corporation (intel_ppgtt.c)
// Copyright © 2016 Intel Corporation (i915_pci.c)
// Copyright 2003 Tungsten Graphics, Inc., Cedar Park, Texas. (i915_drm.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! The GPU's page tables on Gen12.0: the global GTT (one table in BAR0's
//! upper half, `GTTADR`) and the per-process PPGTT (a 4-level, 48-bit tree
//! in memory), their entry formats, and the PAT the entries index.

use crate::reg::Reg;

/// `I915_GTT_PAGE_SIZE_4K`.
pub const GTT_PAGE_SIZE: u64 = 4096;

/// `GEN8_PAGE_PRESENT`.
pub const GEN8_PAGE_PRESENT: u64 = 1 << 0;
/// `GEN8_PAGE_RW`.
pub const GEN8_PAGE_RW: u64 = 1 << 1;
/// `_PAGE_PWT`, which i915 borrows from x86's page tables.
const PAGE_PWT: u64 = 1 << 3;
/// `_PAGE_PCD`.
const PAGE_PCD: u64 = 1 << 4;
/// `PPAT_UNCACHED`: `_PAGE_PWT | _PAGE_PCD`.
pub const PPAT_UNCACHED: u64 = PAGE_PWT | PAGE_PCD;
/// `PPAT_CACHED_PDE`: WB LLC.
pub const PPAT_CACHED_PDE: u64 = 0;

/// `GEN12_PPGTT_PTE_PAT0`.
pub const GEN12_PPGTT_PTE_PAT0: u64 = 1 << 3;
/// `GEN12_PPGTT_PTE_PAT1`.
pub const GEN12_PPGTT_PTE_PAT1: u64 = 1 << 4;
/// `GEN12_PPGTT_PTE_PAT2`.
pub const GEN12_PPGTT_PTE_PAT2: u64 = 1 << 7;
/// `GEN8_PDE_PS_2M`: a level-1 entry that maps 2 MiB itself rather than
/// naming a leaf table. The same bit as [`GEN12_PPGTT_PTE_PAT2`], which
/// Gen12.0 never sets ([`MAX_PAT_INDEX`] is 3).
pub const GEN8_PDE_PS_2M: u64 = 1 << 7;
/// `GEN8_PDE_IPS_64K`: the leaf table a level-1 entry names holds 64 KiB
/// pages, of which the hardware reads every 16th entry.
pub const GEN8_PDE_IPS_64K: u64 = 1 << 11;
/// `GEN12_GGTT_PTE_ADDR_MASK`: bits 45..12.
pub const GEN12_GGTT_PTE_ADDR_MASK: u64 = ((1 << 46) - 1) & !((1 << 12) - 1);

/// `PTE_READ_ONLY`.
pub const PTE_READ_ONLY: u32 = 1 << 0;

/// `enum i915_cache_level`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CacheLevel {
    /// `I915_CACHE_NONE`.
    None,
    /// `I915_CACHE_LLC`.
    Llc,
    /// `I915_CACHE_L3_LLC`.
    L3Llc,
    /// `I915_CACHE_WT`.
    Wt,
}

/// `TGL_CACHELEVEL`'s `cachelevel_to_pat`, which `GEN12_FEATURES` gives
/// ADL-P/N: `i915_gem_get_pat_index`.
#[must_use]
pub const fn cachelevel_to_pat(level: CacheLevel) -> u32 {
    match level {
        CacheLevel::None => 3,
        CacheLevel::Llc | CacheLevel::L3Llc => 0,
        CacheLevel::Wt => 2,
    }
}

/// `GEN8_PPAT_WB`.
pub const GEN8_PPAT_WB: u32 = 3;
/// `GEN8_PPAT_WT`.
pub const GEN8_PPAT_WT: u32 = 2;
/// `GEN8_PPAT_WC`.
pub const GEN8_PPAT_WC: u32 = 1;
/// `GEN8_PPAT_UC`.
pub const GEN8_PPAT_UC: u32 = 0;

/// `GEN12_PAT_INDEX(index)`.
#[must_use]
pub const fn gen12_pat_index(index: u32) -> Reg {
    Reg(0x4800 + index * 4)
}

/// `tgl_setup_private_ppat`, which `setup_private_pat` uses for Gen12 below
/// 12.55: "TGL doesn't support LLC or AGE settings". The writes, in order.
#[must_use]
pub const fn tgl_setup_private_ppat() -> [(Reg, u32); 8] {
    [
        (gen12_pat_index(0), GEN8_PPAT_WB),
        (gen12_pat_index(1), GEN8_PPAT_WC),
        (gen12_pat_index(2), GEN8_PPAT_WT),
        (gen12_pat_index(3), GEN8_PPAT_UC),
        (gen12_pat_index(4), GEN8_PPAT_WB),
        (gen12_pat_index(5), GEN8_PPAT_WB),
        (gen12_pat_index(6), GEN8_PPAT_WB),
        (gen12_pat_index(7), GEN8_PPAT_WB),
    ]
}

/// `.max_pat_index` of `GEN12_FEATURES`: the highest index a client may name
/// (`I915_GEM_CREATE_EXT_SET_PAT` is refused above it).
pub const MAX_PAT_INDEX: u32 = 3;

/// `gen12_pte_encode`: a PPGTT leaf entry. `addr` is the page's device
/// address, page-aligned.
#[must_use]
pub const fn gen12_pte_encode(addr: u64, pat_index: u32, flags: u32) -> u64 {
    let mut pte = addr | GEN8_PAGE_PRESENT | GEN8_PAGE_RW;
    if flags & PTE_READ_ONLY != 0 {
        pte &= !GEN8_PAGE_RW;
    }
    if pat_index & (1 << 0) != 0 {
        pte |= GEN12_PPGTT_PTE_PAT0;
    }
    if pat_index & (1 << 1) != 0 {
        pte |= GEN12_PPGTT_PTE_PAT1;
    }
    if pat_index & (1 << 2) != 0 {
        pte |= GEN12_PPGTT_PTE_PAT2;
    }
    pte
}

/// `gen8_pde_encode`: an entry of a page directory at any level above the
/// leaves. `__set_pd_entry` uses `I915_CACHE_LLC`; the scratch directories
/// use `I915_CACHE_NONE`.
#[must_use]
pub const fn gen8_pde_encode(addr: u64, level: CacheLevel) -> u64 {
    let pde = addr | GEN8_PAGE_PRESENT | GEN8_PAGE_RW;
    if matches!(level, CacheLevel::None) {
        pde | PPAT_UNCACHED
    } else {
        pde | PPAT_CACHED_PDE
    }
}

/// `gen8_ggtt_pte_encode` (no local memory on this part).
#[must_use]
pub const fn gen8_ggtt_pte_encode(addr: u64) -> u64 {
    addr | GEN8_PAGE_PRESENT
}

/// `gen8_ggtt_pte_decode`'s address and presence.
#[must_use]
pub const fn gen8_ggtt_pte_decode(pte: u64) -> Option<u64> {
    if pte & GEN8_PAGE_PRESENT != 0 {
        Some(pte & GEN12_GGTT_PTE_ADDR_MASK)
    } else {
        None
    }
}

/// `GEN8_PDES`: entries in one page-table page.
pub const GEN8_PDES: u64 = 512;
/// `GEN8_PTE_SHIFT`.
pub const GEN8_PTE_SHIFT: u32 = 12;
/// The PPGTT's top level for a 48-bit address space: `vm->top`, 3 (a PML4).
pub const PPGTT_TOP_48: u32 = 3;

/// `gen8_pd_shift(lvl)`.
#[must_use]
pub const fn gen8_pd_shift(level: u32) -> u32 {
    level * 9
}

/// `__gen8_pte_index(addr, lvl)`: the index of `addr`'s entry in its table at
/// `level` (0 the leaves, 3 the PML4).
#[must_use]
pub const fn gen8_pte_index(addr: u64, level: u32) -> usize {
    ((addr >> (GEN8_PTE_SHIFT + gen8_pd_shift(level))) & (GEN8_PDES - 1)) as usize
}

/// The configuration-space word i915 reads the GGTT's size from.
pub const SNB_GMCH_CTRL: u16 = 0x50;
/// `BDW_GMCH_GGMS_SHIFT`.
pub const BDW_GMCH_GGMS_SHIFT: u32 = 6;
/// `BDW_GMCH_GGMS_MASK`.
pub const BDW_GMCH_GGMS_MASK: u16 = 0x3;
/// `BDW_GMCH_GMS_SHIFT`: the stolen memory's size field.
pub const BDW_GMCH_GMS_SHIFT: u32 = 8;
/// `BDW_GMCH_GMS_MASK`.
pub const BDW_GMCH_GMS_MASK: u16 = 0xff;
/// `INTEL_BSM`: the base of stolen memory, a configuration dword.
pub const INTEL_BSM: u16 = 0x5c;
/// `INTEL_BSM_MASK`.
pub const INTEL_BSM_MASK: u32 = !((1 << 20) - 1);

/// `gen8_get_total_gtt_size`: bytes of GGTT page-table entries.
#[must_use]
pub const fn gen8_get_total_gtt_size(gmch_ctl: u16) -> u32 {
    let ggms = (gmch_ctl >> BDW_GMCH_GGMS_SHIFT) & BDW_GMCH_GGMS_MASK;
    if ggms == 0 { 0 } else { (1 << ggms) << 20 }
}

/// `gen8_gmch_probe`'s `ggtt->vm.total`: bytes of address the GGTT maps.
#[must_use]
pub const fn ggtt_total(gmch_ctl: u16) -> u64 {
    (gen8_get_total_gtt_size(gmch_ctl) as u64 / 8) * GTT_PAGE_SIZE
}

/// `gen6_gttadr_offset` for Gen8+: the GGTT's entries start 8 MiB into
/// BAR0 (GTTMMADR is 16 MiB; registers below, `GTTADR` above).
///
/// `needs_wc_ggtt_mapping` is false on Gen11+: "writes larger than 64 bit
/// to the GTT pagetable range will be dropped", so the driver maps this
/// range uncached and writes each entry as one 64-bit store, and needs no
/// `GFX_FLSH_CNTL_GEN6` flush.
pub const GTTADR_OFFSET: u64 = 8 << 20;

/// `GEN12_GUC_TLB_INV_CR`, which `guc_ggtt_invalidate` writes on Gen12 when
/// `GuC` submission is on but its TLB invalidation is not. Under execlists
/// `ggtt->invalidate` is `gen8_ggtt_invalidate`, which writes nothing on
/// Gen11+ (`gen8_gmch_probe`); this driver does not write it either.
pub const GEN12_GUC_TLB_INV_CR: Reg = Reg(0xcee8);
/// `GEN12_GUC_TLB_INV_CR_INVALIDATE`.
pub const GEN12_GUC_TLB_INV_CR_INVALIDATE: u32 = 1 << 0;
