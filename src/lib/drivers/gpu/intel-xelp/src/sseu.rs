// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_sseu.c,
// gt/intel_sseu.h, gt/intel_gt_regs.h and i915_query.c:
// Copyright © 2019 Intel Corporation (intel_sseu.c, intel_sseu.h)
// Copyright © 2022 Intel Corporation (intel_gt_regs.h)
// Copyright © 2018 Intel Corporation (i915_query.c)
// Under the MIT notice in this crate's i915/NOTICE.

//! Slices, dual-subslices and EUs, from the fuse registers, as
//! `gen12_sseu_info_init` reads them; and the bytes of
//! `DRM_I915_QUERY_TOPOLOGY_INFO` that Mesa sizes its thread dispatch from.

use crate::io::Mmio;
use crate::reg::{Reg, field_get, genmask};

/// `GEN11_EU_DISABLE`.
pub const GEN11_EU_DISABLE: Reg = Reg(0x9134);
/// `GEN11_EU_DIS_MASK`: one bit per *pair* of EUs.
pub const GEN11_EU_DIS_MASK: u32 = genmask(7, 0);
/// `GEN11_GT_SLICE_ENABLE`.
pub const GEN11_GT_SLICE_ENABLE: Reg = Reg(0x9138);
/// `GEN11_GT_S_ENA_MASK`.
pub const GEN11_GT_S_ENA_MASK: u32 = genmask(7, 0);
/// `GEN12_GT_GEOMETRY_DSS_ENABLE`.
pub const GEN12_GT_GEOMETRY_DSS_ENABLE: Reg = Reg(0x913c);
/// `GEN11_GT_VEBOX_VDBOX_DISABLE`: which media engines the fuses removed.
pub const GEN11_GT_VEBOX_VDBOX_DISABLE: Reg = Reg(0x9140);

/// `intel_sseu_set_info(sseu, 1, 6, 16)` for Gen12.
pub const MAX_SLICES: u8 = 1;
/// Dual-subslices.
pub const MAX_SUBSLICES: u8 = 6;
/// EUs per dual-subslice.
pub const MAX_EUS_PER_SUBSLICE: u8 = 16;

/// The part of `struct sseu_dev_info` Gen12 fills.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SseuInfo {
    /// `slice_mask`.
    pub slice_mask: u8,
    /// `subslice_mask.hsw[0]`: one bit per dual-subslice present.
    pub subslice_mask: u8,
    /// `eu_mask.hsw[0][ss]`: the same EU mask for every present one.
    pub eu_mask: u16,
    /// `eu_per_subslice`.
    pub eu_per_subslice: u32,
    /// `eu_total`.
    pub eu_total: u32,
    /// What `GEN11_GT_SLICE_ENABLE` said; i915 warns unless it is 1.
    pub slice_enable: u8,
}

/// The fuse values `gen12_sseu_info_init` reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fuses {
    /// `GEN11_GT_SLICE_ENABLE`.
    pub slice_enable: u32,
    /// `GEN12_GT_GEOMETRY_DSS_ENABLE`.
    pub geometry_dss_enable: u32,
    /// `GEN11_EU_DISABLE`.
    pub eu_disable: u32,
}

impl Fuses {
    /// Read them (the GT domain must be awake: `0x9134`-`0x913c` are GT's).
    pub fn read<M: Mmio + ?Sized>(mmio: &mut M) -> Self {
        Self {
            slice_enable: mmio.read32(GEN11_GT_SLICE_ENABLE.offset()),
            geometry_dss_enable: mmio.read32(GEN12_GT_GEOMETRY_DSS_ENABLE.offset()),
            eu_disable: mmio.read32(GEN11_EU_DISABLE.offset()),
        }
    }
}

/// `gen12_sseu_info_init` with `gen11_compute_sseu_info`.
#[must_use]
pub fn gen12_sseu_info_init(fuses: Fuses) -> SseuInfo {
    let slice_enable = field_get(GEN11_GT_S_ENA_MASK, fuses.slice_enable) as u8;
    let eu_en_fuse = !field_get(GEN11_EU_DIS_MASK, fuses.eu_disable) as u8;
    let mut eu_en: u16 = 0;
    for eu in 0..u32::from(MAX_EUS_PER_SUBSLICE) / 2 {
        if eu_en_fuse & (1 << eu) != 0 {
            eu_en |= (1 << (eu * 2)) | (1 << (eu * 2 + 1));
        }
    }
    let valid_ss_mask = genmask(u32::from(MAX_SUBSLICES) - 1, 0);
    let subslice_mask = (fuses.geometry_dss_enable & valid_ss_mask) as u8;
    let eu_per_subslice = eu_en.count_ones();
    SseuInfo {
        slice_mask: 1,
        subslice_mask,
        eu_mask: eu_en,
        eu_per_subslice,
        eu_total: subslice_mask.count_ones() * eu_per_subslice,
        slice_enable,
    }
}

/// `GEN_SSEU_STRIDE(max_entries)`.
#[must_use]
pub const fn gen_sseu_stride(max_entries: u8) -> usize {
    (max_entries as usize).div_ceil(8)
}

/// The bytes of the `DRM_I915_QUERY_TOPOLOGY_INFO` (and, on Gen12, the
/// same `GEOMETRY_SUBSLICES`) item: `fill_topology_info`'s header, slice
/// mask, subslice mask and EU masks. The length is `fill_topology_info`'s
/// `total_length`.
pub const TOPOLOGY_LEN: usize = 16
    + 1
    + MAX_SLICES as usize * gen_sseu_stride(MAX_SUBSLICES)
    + (MAX_SLICES * MAX_SUBSLICES) as usize * gen_sseu_stride(MAX_EUS_PER_SUBSLICE);

/// `fill_topology_info` with `intel_sseu_copy_ssmask_to_user` and
/// `intel_sseu_copy_eumask_to_user`.
#[must_use]
pub fn fill_topology_info(sseu: &SseuInfo) -> [u8; TOPOLOGY_LEN] {
    let ss_stride = gen_sseu_stride(MAX_SUBSLICES);
    let eu_stride = gen_sseu_stride(MAX_EUS_PER_SUBSLICE);
    let slice_length: usize = 1;
    let subslice_length = usize::from(MAX_SLICES) * ss_stride;
    let header: [u16; 8] = [
        0, // flags
        u16::from(MAX_SLICES),
        u16::from(MAX_SUBSLICES),
        u16::from(MAX_EUS_PER_SUBSLICE),
        slice_length as u16,                     // subslice_offset
        ss_stride as u16,                        // subslice_stride
        (slice_length + subslice_length) as u16, // eu_offset
        eu_stride as u16,                        // eu_stride
    ];
    let mut out = [0_u8; TOPOLOGY_LEN];
    for (slot, word) in out.chunks_exact_mut(2).zip(header) {
        slot.copy_from_slice(&word.to_le_bytes());
    }
    if let Some(byte) = out.get_mut(16) {
        *byte = sseu.slice_mask;
    }
    if let Some(byte) = out.get_mut(17) {
        *byte = sseu.subslice_mask;
    }
    for ss in 0..usize::from(MAX_SUBSLICES) {
        let mask = if sseu.subslice_mask & (1 << ss) != 0 {
            sseu.eu_mask
        } else {
            0
        };
        for i in 0..eu_stride {
            if let Some(byte) = out.get_mut(18 + ss * eu_stride + i) {
                *byte = (mask >> (8 * i)) as u8;
            }
        }
    }
    out
}
