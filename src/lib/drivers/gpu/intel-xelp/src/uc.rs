// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/uc/intel_uc.c and
// gt/uc/intel_guc_reg.h:
// Copyright © 2016-2019 Intel Corporation (intel_uc.c)
// Copyright © 2014-2019 Intel Corporation (intel_guc_reg.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! The one thing of i915's microcontroller code an execlists driver needs:
//! whether `GuC` has been loaded since the device last reset.
//! `__uc_check_hw` refuses execlists then ("We can silently continue
//! without `GuC` only if it was never enabled before on this system after
//! reboot, otherwise we risk GPU hangs."). On the box, after Linux had run
//! `GuC`, i915 with `enable_guc=0` wedged the GT with "Enabling uc failed
//! (-5)"; a PCI function-level reset cleared the lock and execlists ran (M0,
//! 2026-10-18).

use crate::io::Mmio;
use crate::reg::Reg;

/// `GUC_WOPCM_SIZE`.
pub const GUC_WOPCM_SIZE: Reg = Reg(0xc050);
/// `GUC_WOPCM_SIZE_LOCKED`.
pub const GUC_WOPCM_SIZE_LOCKED: u32 = 1 << 0;
/// `DMA_GUC_WOPCM_OFFSET`.
pub const DMA_GUC_WOPCM_OFFSET: Reg = Reg(0xc340);
/// `GUC_WOPCM_OFFSET_VALID`.
pub const GUC_WOPCM_OFFSET_VALID: u32 = 1 << 0;

/// `uc_is_wopcm_locked`.
pub fn uc_is_wopcm_locked<M: Mmio + ?Sized>(mmio: &mut M) -> bool {
    mmio.read32(GUC_WOPCM_SIZE.offset()) & GUC_WOPCM_SIZE_LOCKED != 0
        || mmio.read32(DMA_GUC_WOPCM_OFFSET.offset()) & GUC_WOPCM_OFFSET_VALID != 0
}
