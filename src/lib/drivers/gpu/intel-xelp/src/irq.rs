// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_gt_irq.c,
// i915_irq.c (gen11_irq_handler, gen11_master_intr_*), gt/intel_gt_regs.h
// and i915_reg.h:
// Copyright © 2019 Intel Corporation (intel_gt_irq.c)
// Copyright 2003 Tungsten Graphics, Inc., Cedar Park, Texas. (i915_irq.c)
// Copyright © 2022 Intel Corporation (intel_gt_regs.h)
// Copyright 2003 Tungsten Graphics, Inc., Cedar Park, Texas. (i915_reg.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! Gen11+ interrupts, for the GT alone: the master control, the two GT
//! banks, and the identity registers that name each source's engine.
//! The display's interrupt sources are left as the firmware left them;
//! render-only never enables or acknowledges them.

use crate::engine::{COPY_ENGINE_CLASS, EngineInfo};
use crate::generated::i915_reg::{GEN11_GFX_MSTR_IRQ, GEN11_MASTER_IRQ};
use crate::generated::intel_gt_regs as gr;
use crate::io::{Clock, Mmio};
use crate::reg::Reg;

/// `GT_RENDER_USER_INTERRUPT`.
pub const GT_RENDER_USER_INTERRUPT: u32 = 1 << 0;
/// `GT_CS_MASTER_ERROR_INTERRUPT`.
pub const GT_CS_MASTER_ERROR_INTERRUPT: u32 = 1 << 3;
/// `GT_CONTEXT_SWITCH_INTERRUPT`.
pub const GT_CONTEXT_SWITCH_INTERRUPT: u32 = 1 << 8;
/// `GT_WAIT_SEMAPHORE_INTERRUPT`.
pub const GT_WAIT_SEMAPHORE_INTERRUPT: u32 = 1 << 11;

/// `GEN11_GT_DW_IRQ(x)`.
#[must_use]
pub const fn gen11_gt_dw_irq(bank: u32) -> u32 {
    1 << bank
}

/// `GEN11_INTR_ENGINE_CLASS(x)`.
#[must_use]
pub const fn gen11_intr_engine_class(identity: u32) -> u8 {
    ((identity >> 16) & 0x7) as u8
}

/// `GEN11_INTR_ENGINE_INSTANCE(x)`.
#[must_use]
pub const fn gen11_intr_engine_instance(identity: u32) -> u8 {
    ((identity >> 20) & 0x3f) as u8
}

/// `GEN11_INTR_ENGINE_INTR(x)`.
#[must_use]
pub const fn gen11_intr_engine_intr(identity: u32) -> u16 {
    (identity & 0xffff) as u16
}

/// `gen11_gt_irq_postinstall` with execlists: user interrupts, master
/// errors, context switches and semaphore waits, for both engines of each
/// enable and mask register. The writes, in i915's order (the registers
/// for engines ADL-P lacks left out).
#[must_use]
pub fn gen11_gt_irq_postinstall() -> [(Reg, u32); 9] {
    let irqs = GT_RENDER_USER_INTERRUPT
        | GT_CS_MASTER_ERROR_INTERRUPT
        | GT_CONTEXT_SWITCH_INTERRUPT
        | GT_WAIT_SEMAPHORE_INTERRUPT;
    let dmask = (irqs << 16) | irqs;
    let smask = irqs << 16;
    [
        (gr::GEN11_RENDER_COPY_INTR_ENABLE, dmask),
        (gr::GEN11_VCS_VECS_INTR_ENABLE, dmask),
        (gr::GEN11_RCS0_RSVD_INTR_MASK, !smask),
        (gr::GEN11_BCS_RSVD_INTR_MASK, !smask),
        (gr::GEN11_VCS0_VCS1_INTR_MASK, !dmask),
        (gr::GEN11_VCS2_VCS3_INTR_MASK, !dmask),
        (gr::GEN11_VECS0_VECS1_INTR_MASK, !dmask),
        // RPS and GuC interrupts stay masked until rps/uc enable them.
        (gr::GEN11_GPM_WGBOXPERF_INTR_MASK, !0),
        (gr::GEN11_GUC_SG_INTR_MASK, !0),
    ]
}

/// `gen11_gt_irq_reset`, for ADL-P's engines.
#[must_use]
pub fn gen11_gt_irq_reset() -> [(Reg, u32); 13] {
    [
        (gr::GEN11_RENDER_COPY_INTR_ENABLE, 0),
        (gr::GEN11_VCS_VECS_INTR_ENABLE, 0),
        (gr::GEN11_RCS0_RSVD_INTR_MASK, !0),
        (gr::GEN11_BCS_RSVD_INTR_MASK, !0),
        (gr::GEN11_VCS0_VCS1_INTR_MASK, !0),
        (gr::GEN11_VCS2_VCS3_INTR_MASK, !0),
        (gr::GEN11_VECS0_VECS1_INTR_MASK, !0),
        (gr::GEN11_GPM_WGBOXPERF_INTR_ENABLE, 0),
        (gr::GEN11_GPM_WGBOXPERF_INTR_MASK, !0),
        (gr::GEN11_GUC_SG_INTR_ENABLE, 0),
        (gr::GEN11_GUC_SG_INTR_MASK, !0),
        (gr::GEN11_CRYPTO_RSVD_INTR_ENABLE, 0),
        (gr::GEN11_CRYPTO_RSVD_INTR_MASK, !0),
    ]
}

/// One engine's interrupt bits, as `intel_engine_cs_irq` receives them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EngineIrq {
    /// `engine->class`.
    pub class: u8,
    /// `engine->instance`.
    pub instance: u8,
    /// The `GT_*_INTERRUPT` bits.
    pub iir: u16,
}

/// What a pass of the handler found: up to 64 sources (32 a bank).
#[derive(Clone, Copy, Debug)]
pub struct Sources {
    list: [Option<EngineIrq>; 64],
    count: usize,
    /// Identities that never turned valid (`"INTR_IDENTITY_REG ... not
    /// valid!"`).
    pub invalid: u32,
    /// Sources of a class this driver does not serve (`OTHER_CLASS`: `GuC`,
    /// GTPM, ...), counted, not handled.
    pub other: u32,
}

impl Sources {
    /// The engine sources, in bank and bit order.
    pub fn iter(&self) -> impl Iterator<Item = &EngineIrq> {
        self.list.iter().take(self.count).flatten()
    }
}

/// `gen11_irq_handler` for the GT: master off (`gen11_master_intr_disable`),
/// each set bank (`gen11_gt_bank_handler`, each bit's identity by
/// `gen11_gt_engine_identity`, ~100 us), the bank cleared after it is
/// served, master on again. `None` when the master showed nothing
/// (`IRQ_NONE`).
pub fn gen11_irq_handler<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
) -> Option<Sources> {
    mmio.write32(GEN11_GFX_MSTR_IRQ.offset(), 0);
    let master_ctl = mmio.read32(GEN11_GFX_MSTR_IRQ.offset());
    if master_ctl == 0 {
        mmio.write32(GEN11_GFX_MSTR_IRQ.offset(), GEN11_MASTER_IRQ);
        return None;
    }
    let mut sources = Sources {
        list: [None; 64],
        count: 0,
        invalid: 0,
        other: 0,
    };
    for bank in 0..2_u32 {
        if master_ctl & gen11_gt_dw_irq(bank) == 0 {
            continue;
        }
        let intr_dw = mmio.read32(gr::gen11_gt_intr_dw(bank).offset());
        for bit in 0..32 {
            if intr_dw & (1 << bit) == 0 {
                continue;
            }
            let identity = gen11_gt_engine_identity(mmio, clock, bank, bit);
            let intr = gen11_intr_engine_intr(identity);
            if identity == 0 || intr == 0 {
                sources.invalid += u32::from(identity == 0);
                continue;
            }
            let class = gen11_intr_engine_class(identity);
            if class > COPY_ENGINE_CLASS {
                sources.other += 1;
                continue;
            }
            if let Some(slot) = sources.list.get_mut(sources.count) {
                *slot = Some(EngineIrq {
                    class,
                    instance: gen11_intr_engine_instance(identity),
                    iir: intr,
                });
                sources.count += 1;
            }
        }
        // "Clear must be after shared has been served for engine"
        mmio.write32(gr::gen11_gt_intr_dw(bank).offset(), intr_dw);
    }
    mmio.write32(GEN11_GFX_MSTR_IRQ.offset(), GEN11_MASTER_IRQ);
    Some(sources)
}

/// `gen11_gt_engine_identity`: select the bit, wait for the identity to
/// turn valid, acknowledge it. 0 when it never did.
fn gen11_gt_engine_identity<M: Mmio + ?Sized, C: Clock + ?Sized>(
    mmio: &mut M,
    clock: &mut C,
    bank: u32,
    bit: u32,
) -> u32 {
    mmio.write32(gr::gen11_iir_reg_selector(bank).offset(), 1 << bit);
    let start = clock.now_us();
    let ident = loop {
        let ident = mmio.read32(gr::gen11_intr_identity_reg(bank).offset());
        if ident & gr::GEN11_INTR_DATA_VALID != 0 || clock.now_us().saturating_sub(start) > 100 {
            break ident;
        }
    };
    if ident & gr::GEN11_INTR_DATA_VALID == 0 {
        return 0;
    }
    mmio.write32(
        gr::gen11_intr_identity_reg(bank).offset(),
        gr::GEN11_INTR_DATA_VALID,
    );
    ident
}

/// Which engine of `engines` an interrupt names (`gt->engine_class[][]`).
#[must_use]
pub fn engine_of<'a>(engines: &'a [EngineInfo], irq: &EngineIrq) -> Option<&'a EngineInfo> {
    engines
        .iter()
        .find(|e| e.class == irq.class && e.instance == irq.instance)
}
