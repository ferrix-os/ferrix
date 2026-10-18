// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_mocs.c and
// gt/intel_gt_regs.h:
// Copyright © 2015 Intel Corporation (intel_mocs.c)
// Copyright © 2022 Intel Corporation (intel_gt_regs.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! Memory Object Control State: the caching a surface's MOCS index selects.
//! Userspace (anv, iris) puts an index in every surface and buffer state, so
//! the table must be the one Mesa expects for the platform, i915's.
//!
//! `get_mocs_settings` gives ADL-P/N (Gen12, neither TGL nor RKL, DG1 or DG2)
//! `gen12_mocs_table`, with `unused_entries_index` 2 and `uc_index` 3. Gen12
//! has global MOCS registers (`HAS_GLOBAL_MOCS`), written once by
//! `intel_mocs_init`, and the render engine's L3 control table
//! (`HAS_RENDER_L3CC`).

use crate::generated::intel_mocs::GEN12_MOCS_TABLE;
use crate::reg::Reg;

/// `struct drm_i915_mocs_entry`, for an entry i915 marks `used`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MocsEntry {
    /// The table index (`MOCS_ENTRY`'s first argument).
    pub index: u32,
    /// `.control_value`: the `LE_*` fields.
    pub control_value: u32,
    /// `.l3cc_value`: the `L3_*` fields.
    pub l3cc_value: u16,
}

/// `GEN9_NUM_MOCS_ENTRIES`: `table->n_entries` for Gen12.
pub const NUM_MOCS_ENTRIES: u32 = 64;
/// `table->unused_entries_index` for `gen12_mocs_table`.
pub const UNUSED_ENTRIES_INDEX: u32 = 2;
/// `table->uc_index` for `gen12_mocs_table`: the uncached entry.
pub const UC_INDEX: u32 = 3;

/// `GEN12_GLOBAL_MOCS(i)`.
#[must_use]
pub const fn gen12_global_mocs(i: u32) -> Reg {
    Reg(0x4000 + i * 4)
}

/// `GEN9_LNCFCMOCS(i)`: the L3 cache control, two entries a register.
#[must_use]
pub const fn gen9_lncfcmocs(i: u32) -> Reg {
    Reg(0xb020 + i * 4)
}

fn entry(index: u32) -> Option<&'static MocsEntry> {
    GEN12_MOCS_TABLE.iter().find(|entry| entry.index == index)
}

/// `get_entry_control`: an unused index gets the unused entry's value.
#[must_use]
pub fn get_entry_control(index: u32) -> u32 {
    entry(index)
        .or_else(|| entry(UNUSED_ENTRIES_INDEX))
        .map_or(0, |e| e.control_value)
}

/// `get_entry_l3cc`.
#[must_use]
pub fn get_entry_l3cc(index: u32) -> u16 {
    entry(index)
        .or_else(|| entry(UNUSED_ENTRIES_INDEX))
        .map_or(0, |e| e.l3cc_value)
}

/// `l3cc_combine`.
#[must_use]
pub const fn l3cc_combine(low: u16, high: u16) -> u32 {
    low as u32 | (high as u32) << 16
}

/// `intel_mocs_init`'s writes for Gen12, in i915's order: every global MOCS
/// register (`__init_mocs_table` at `global_mocs_offset`), then the L3
/// control table (`init_l3cc_table`). Called under forcewake of every
/// domain, as i915 does.
pub fn intel_mocs_init_writes() -> impl Iterator<Item = (Reg, u32)> {
    let control = (0..NUM_MOCS_ENTRIES).map(|i| (gen12_global_mocs(i), get_entry_control(i)));
    let l3cc = (0..NUM_MOCS_ENTRIES.div_ceil(2)).map(|i| {
        (
            gen9_lncfcmocs(i),
            l3cc_combine(get_entry_l3cc(2 * i), get_entry_l3cc(2 * i + 1)),
        )
    });
    control.chain(l3cc)
}
