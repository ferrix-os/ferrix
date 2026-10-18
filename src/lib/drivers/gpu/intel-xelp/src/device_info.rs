// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/i915_pci.c and
// intel_device_info.c (MIT, full notice in those files):
// Copyright © 2016 Intel Corporation
// Under the MIT notice in this crate's i915/NOTICE.

//! Which devices this crate drives, and what i915 says about them:
//! `i915_pci.c`'s `adl_p_info` (`GEN12_FEATURES`), which i915 gives the
//! ADL-N ids too (`INTEL_ADLN_IDS(INTEL_VGA_DEVICE, &adl_p_info)`), marked
//! subplatform N by `intel_device_info.c`.

use crate::generated::pciids::ADLN_IDS;

/// Intel's PCI vendor id.
pub const VENDOR_INTEL: u16 = 0x8086;

/// `enum intel_engine_id` (`gt/intel_engine_types.h`), for the engines
/// ADL-P has, and their bits in `platform_engine_mask` (`BIT(id)`).
pub mod engine {
    /// `RCS0`: the render engine.
    pub const RCS0: u32 = 0;
    /// `BCS0`: the copy engine (`BCS1` to `BCS8` follow, for `Xe_HP`).
    pub const BCS0: u32 = 1;
    /// `VCS0`: the first video decode engine.
    pub const VCS0: u32 = 10;
    /// `VCS2`: the third video decode engine, ADL-P's second.
    pub const VCS2: u32 = 12;
    /// `VECS0`: the video enhancement engine.
    pub const VECS0: u32 = 18;

    /// `BIT(id)`.
    #[must_use]
    pub const fn bit(id: u32) -> u32 {
        1 << id
    }
}

/// What `struct intel_device_info` says of a platform, for the fields this
/// crate reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeviceInfo {
    /// `GEN(12)`.
    pub graphics_ver: u8,
    /// `.platform_engine_mask`: `BIT(id)` of each [`engine`].
    pub platform_engine_mask: u32,
    /// `.__runtime.ppgtt_size`: bits of PPGTT address.
    pub ppgtt_size: u8,
    /// `.dma_mask_size`: bits of a DMA address.
    pub dma_mask_size: u8,
    /// `.has_llc`, from `GEN9_FEATURES`.
    pub has_llc: bool,
    /// `.has_global_mocs`, from `GEN12_FEATURES`.
    pub has_global_mocs: bool,
    /// `.has_logical_ring_elsq`, from `GEN11_FEATURES`: the submit queue.
    pub has_logical_ring_elsq: bool,
    /// `.max_pat_index`, from `GEN12_FEATURES`.
    pub max_pat_index: u8,
    /// ADL-P's subplatform N (`IS_ALDERLAKE_P_N`).
    pub subplatform_n: bool,
}

/// `adl_p_info`, as ADL-N gets it.
pub const ADL_N_INFO: DeviceInfo = DeviceInfo {
    graphics_ver: 12,
    platform_engine_mask: engine::bit(engine::RCS0)
        | engine::bit(engine::BCS0)
        | engine::bit(engine::VECS0)
        | engine::bit(engine::VCS0)
        | engine::bit(engine::VCS2),
    ppgtt_size: 48,
    dma_mask_size: 39,
    has_llc: true,
    has_global_mocs: true,
    has_logical_ring_elsq: true,
    max_pat_index: 3,
    subplatform_n: true,
};

/// The platform a PCI identity is, if this crate drives it:
/// `i915_pci_probe`'s table lookup, for the ADL-N ids alone.
#[must_use]
pub fn lookup(vendor: u16, device: u16) -> Option<DeviceInfo> {
    (vendor == VENDOR_INTEL && ADLN_IDS.contains(&device)).then_some(ADL_N_INFO)
}
