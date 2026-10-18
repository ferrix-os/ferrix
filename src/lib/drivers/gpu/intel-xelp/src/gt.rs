// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_gt.c,
// i915_gem.c (i915_gem_init's PAT), gt/intel_engine_cs.c
// (intel_engine_resume) and gt/intel_execlists_submission.c
// (execlists_resume, enable_execlists, enable_error_interrupt):
// Copyright © 2019 Intel Corporation (intel_gt.c)
// Copyright © 2008-2015 Intel Corporation (i915_gem.c)
// Copyright © 2016 Intel Corporation (intel_engine_cs.c)
// Copyright © 2014 Intel Corporation (intel_execlists_submission.c)
// Under the MIT notice in this crate's i915/NOTICE.

//! The order i915 brings the GT up in, for ADL-P/N under execlists:
//! [`intel_gt_init_hw`] once (under forcewake of every domain), then
//! [`intel_engine_resume`] for each engine. Each takes the lists and tables
//! the other modules build, so the order itself is all that is here.

use crate::generated::intel_engine_regs::{
    GEN11_GFX_DISABLE_LEGACY_MODE, STOP_RING, ring_eir, ring_emr, ring_esr, ring_hws_pga,
    ring_hwstam, ring_mi_mode, ring_mode_gen7,
};
use crate::io::Mmio;
use crate::reg::{masked_bit_disable, masked_bit_enable};
use crate::workarounds::{self, WaList};
use crate::{gtt, mocs, uc};

/// Why the GT did not come up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InitError {
    /// `intel_uc_init_hw` with `GuC` off: the `WOPCM` is locked
    /// (`__uc_check_hw`, "Enabling uc failed (-5)"). An FLR clears it.
    WopcmLocked,
}

/// What [`intel_gt_init_hw`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InitReport {
    /// GT workarounds whose bits did not stick
    /// (`intel_gt_verify_workarounds(gt, "init")`); i915 logs and goes on.
    pub lost_workarounds: usize,
}

/// `setup_private_pat`, which `i915_gem_init` runs for Gen8+ before the GGTT
/// is initialised: [`gtt::tgl_setup_private_ppat`] on Gen12.0.
pub fn setup_private_pat<M: Mmio + ?Sized>(mmio: &mut M) {
    for (reg, value) in gtt::tgl_setup_private_ppat() {
        mmio.write32(reg.offset(), value);
    }
}

/// `intel_gt_init_hw` for ADL-P/N with execlists: apply and verify the GT
/// workarounds; `init_unused_rings` and swizzling have nothing on Gen12;
/// `i915_ppgtt_init_hw`'s `gtt_write_workarounds` has nothing on Gen12 either;
/// `intel_uc_init_hw` with `GuC` off is `__uc_check_hw`; then `intel_mocs_init`.
/// The caller holds forcewake of every domain.
pub fn intel_gt_init_hw<M: Mmio + ?Sized>(
    mmio: &mut M,
    gt_wa: &WaList,
) -> Result<InitReport, InitError> {
    workarounds::wa_list_apply(mmio, gt_wa);
    let lost_workarounds = workarounds::wa_list_verify(mmio, gt_wa).count();
    if uc::uc_is_wopcm_locked(mmio) {
        return Err(InitError::WopcmLocked);
    }
    for (reg, value) in mocs::intel_mocs_init_writes() {
        mmio.write32(reg.offset(), value);
    }
    Ok(InitReport { lost_workarounds })
}

/// `I915_ERROR_INSTRUCTION`: the one engine error `enable_error_interrupt`
/// unmasks (`"CP_PRIV ... we only unmask the instruction error bit"`).
pub const I915_ERROR_INSTRUCTION: u32 = 1 << 0;

/// What [`intel_engine_resume`] found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ResumeReport {
    /// `RING_ESR` after the errors were cleared: i915 says "engine resumed
    /// still in error" and resets the engine when it is not 0.
    pub still_in_error: u32,
}

/// `intel_engine_resume` -> `execlists_resume` for an engine at `base`
/// whose status page is at `hws_ggtt` in the GGTT: its workarounds, its
/// whitelist, (global MOCS needs nothing per engine), then
/// `enable_execlists`: every status-page write enabled (`HWSTAM`), legacy
/// mode off, the ring not stopped, the status page's address, and
/// `enable_error_interrupt`.
pub fn intel_engine_resume<M: Mmio + ?Sized>(
    mmio: &mut M,
    base: u32,
    engine_wa: &WaList,
    whitelist: &WaList,
    hws_ggtt: u32,
) -> ResumeReport {
    workarounds::wa_list_apply(mmio, engine_wa);
    for (reg, value) in workarounds::apply_whitelist(base, whitelist) {
        mmio.write32(reg.offset(), value);
    }
    // enable_execlists
    mmio.write32(ring_hwstam(base).offset(), !0);
    mmio.write32(
        ring_mode_gen7(base).offset(),
        masked_bit_enable(GEN11_GFX_DISABLE_LEGACY_MODE),
    );
    mmio.write32(ring_mi_mode(base).offset(), masked_bit_disable(STOP_RING));
    mmio.write32(ring_hws_pga(base).offset(), hws_ggtt);
    let _posting = mmio.read32(ring_hws_pga(base).offset());
    // enable_error_interrupt
    mmio.write32(ring_emr(base).offset(), !0);
    mmio.write32(ring_eir(base).offset(), !0); // "clear all existing errors"
    let still_in_error = mmio.read32(ring_esr(base).offset());
    mmio.write32(ring_emr(base).offset(), !I915_ERROR_INSTRUCTION);
    ResumeReport { still_in_error }
}

/// The PAT registers that do not hold what [`setup_private_pat`] wrote:
/// `(register, expected, read)`. i915 does not read them back; a PAT that
/// did not stick turns every client's caching wrong, so the driver does
/// (M2, "PAT and MOCS").
pub fn verify_private_pat<M: Mmio + ?Sized>(
    mmio: &mut M,
) -> impl Iterator<Item = (crate::reg::Reg, u32, u32)> + '_ {
    gtt::tgl_setup_private_ppat()
        .into_iter()
        .filter_map(move |(reg, value)| {
            let read = mmio.read32(reg.offset());
            (read != value).then_some((reg, value, read))
        })
}
