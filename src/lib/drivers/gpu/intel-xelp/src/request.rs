// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/i915_request.c,
// gt/intel_execlists_submission.c (execlists_request_alloc) and
// gem/i915_gem_execbuffer.c (eb_request_submit):
// Copyright © 2008-2015 Intel Corporation (i915_request.c)
// Copyright © 2014 Intel Corporation (intel_execlists_submission.c)
// Copyright © 2008,2010 Intel Corporation (i915_gem_execbuffer.c)
// Under the MIT notice in this crate's i915/NOTICE.

//! A request's commands in a context's ring, in i915's order:
//! `execlists_request_alloc`'s unconditional invalidation, then
//! `eb_request_submit`'s initial breadcrumb and batch start, then
//! `i915_request_add`'s final breadcrumb, which ends in the user interrupt.
//! Space for the final breadcrumb is reserved from the start
//! (`rq->reserved_space`, twice its length), so it can never fail.

use crate::commands::Cs;
use crate::engine::{EngineInfo, RENDER_CLASS};
use crate::engine_cs::{self, EMIT_INVALIDATE, Tails};
use crate::ring::{NoSpace, Ring};

/// `EXECLISTS_REQUEST_SIZE`: "Typical size of the average request (2
/// pipecontrols and a `MI_BB`)", added to the reserve while the request is
/// begun.
pub const EXECLISTS_REQUEST_SIZE: u32 = 64;

/// `engine->emit_fini_breadcrumb_dw`, measured as i915 measures it (by
/// emitting once into scratch): 22 for render, 18 for the others.
#[must_use]
pub const fn fini_breadcrumb_dw(engine: &EngineInfo) -> u32 {
    if engine.class == RENDER_CLASS { 22 } else { 18 }
}

/// Where a built request lies in its ring.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Built {
    /// `rq->head`: its first byte.
    pub head: u32,
    /// `rq->tail`: what `RING_TAIL` is set to in the context image.
    pub tail: u32,
    /// `rq->wa_tail`: past the `WaIdleLiteRestore` pair.
    pub wa_tail: u32,
}

/// Build a request for a batch at `batch` (PPGTT) in `ring`, whose
/// dwords are `memory` (the ring's whole buffer). `hwsp_offset` is the
/// request's seqno slot in the GGTT, `preempt_address` the engine's
/// preemption flag.
pub fn build(
    ring: &mut Ring,
    memory: &mut [u32],
    engine: &EngineInfo,
    batch: u64,
    seqno: u32,
    hwsp_offset: u32,
    preempt_address: u32,
) -> Result<Built, NoSpace> {
    build_payload(
        ring,
        memory,
        engine,
        Payload::Batch {
            address: batch,
            flags: 0,
        },
        seqno,
        hwsp_offset,
        preempt_address,
    )
}

/// What a request runs between its breadcrumbs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Payload<'a> {
    /// `eb_request_submit`'s `emit_bb_start`: a batch at `address`, from the
    /// PPGTT unless `flags` has [`engine_cs::I915_DISPATCH_SECURE`].
    Batch {
        /// The batch's first byte.
        address: u64,
        /// `I915_DISPATCH_*`.
        flags: u32,
    },
    /// Commands written into the ring itself, as the driver's own requests
    /// are (`intel_engine_emit_ctx_wa`, a selftest's `MI_STORE_DWORD_IMM`):
    /// they run with the ring's privilege. Padded with `MI_NOOP` to an even
    /// count.
    Commands(&'a [u32]),
}

/// [`build`] for any [`Payload`]. On [`NoSpace`] the ring is left as it
/// was, so the caller can retire requests and try again.
pub fn build_payload(
    ring: &mut Ring,
    memory: &mut [u32],
    engine: &EngineInfo,
    payload: Payload<'_>,
    seqno: u32,
    hwsp_offset: u32,
    preempt_address: u32,
) -> Result<Built, NoSpace> {
    let before = *ring;
    let built = build_into(
        ring,
        memory,
        engine,
        payload,
        seqno,
        hwsp_offset,
        preempt_address,
    );
    if built.is_err() {
        *ring = before;
    }
    built
}

fn build_into(
    ring: &mut Ring,
    memory: &mut [u32],
    engine: &EngineInfo,
    payload: Payload<'_>,
    seqno: u32,
    hwsp_offset: u32,
    preempt_address: u32,
) -> Result<Built, NoSpace> {
    let fini = fini_breadcrumb_dw(engine);
    let reserved = 2 * fini * 4 + EXECLISTS_REQUEST_SIZE;
    let head = ring.tail();
    // execlists_request_alloc: "Unconditionally invalidate GPU caches and TLBs."
    let invalidate = if engine.class == RENDER_CLASS {
        1 + 6 + 8 + 1 + 6
    } else {
        1 + 4 + 8 + 1
    };
    let _ = emit(ring, memory, round_even(invalidate), reserved, |cs| {
        engine_cs::gen12_emit_flush(engine, EMIT_INVALIDATE, cs);
    })?;
    // eb_request_submit
    let _ = emit(ring, memory, 6, reserved, |cs| {
        engine_cs::gen8_emit_init_breadcrumb(cs, hwsp_offset, seqno);
    })?;
    match payload {
        Payload::Batch { address, flags } => {
            let _ = emit(ring, memory, 6, reserved, |cs| {
                engine_cs::gen8_emit_bb_start(cs, address, flags);
            })?;
        }
        Payload::Commands(commands) => {
            let len = u32::try_from(commands.len()).map_err(|_| NoSpace)?;
            let _ = emit(ring, memory, round_even(len), reserved, |cs| {
                for &dword in commands {
                    cs.push(dword);
                }
            })?;
        }
    }
    // i915_request_add -> __i915_request_add_to_timeline: the reserve is
    // released and the final breadcrumb takes its place.
    let mut tails = Tails {
        tail: 0,
        wa_tail: 0,
    };
    // Where it really starts: a wrap moves it to the ring's start.
    let start = emit(ring, memory, fini, 0, |cs| {
        tails = if engine.class == RENDER_CLASS {
            engine_cs::gen12_emit_fini_breadcrumb_rcs(cs, seqno, hwsp_offset, preempt_address)
        } else {
            engine_cs::gen12_emit_fini_breadcrumb_xcs(cs, seqno, hwsp_offset, preempt_address)
        };
    })?;
    Ok(Built {
        head,
        tail: ring.wrap(start + tails.tail as u32 * 4),
        wa_tail: ring.wrap(start + tails.wa_tail as u32 * 4),
    })
}

const fn round_even(dwords: u32) -> u32 {
    dwords + dwords % 2
}

/// `intel_ring_begin` for `dwords`, the emitter run into the reserved span,
/// any wrap padded with `MI_NOOP` (0) and any unused tail of the span too.
/// Returns the byte offset the commands start at.
fn emit(
    ring: &mut Ring,
    memory: &mut [u32],
    dwords: u32,
    reserved: u32,
    body: impl FnOnce(&mut Cs<'_>),
) -> Result<u32, NoSpace> {
    let got = ring.begin(dwords, reserved)?;
    if let Some(from) = got.wrap_from {
        for slot in memory.iter_mut().skip((from / 4) as usize) {
            *slot = 0;
        }
    }
    let at = (got.at / 4) as usize;
    let span = memory.get_mut(at..at + dwords as usize).ok_or(NoSpace)?;
    span.fill(0);
    let mut cs = Cs::new(span);
    body(&mut cs);
    if cs.overflowed() {
        Err(NoSpace)
    } else {
        Ok(got.at)
    }
}

/// `intel_engine_emit_ctx_wa`'s commands, into `out`: an `EMIT_BARRIER`
/// flush, the context workarounds as one `MI_LOAD_REGISTER_IMM`
/// ([`crate::workarounds::emit_ctx_wa`]), and another barrier. They go in
/// the first request on a context whose image is restore-inhibited, so the
/// image the engine saves, the default state, carries them. Returns the
/// dwords used, or `None` if `out` is short.
pub fn ctx_wa_payload<M: crate::io::Mmio + ?Sized>(
    mmio: &mut M,
    engine: &EngineInfo,
    wal: &crate::workarounds::WaList,
    out: &mut [u32],
) -> Option<usize> {
    let mut at = barrier(engine, out)?;
    at += crate::workarounds::emit_ctx_wa(mmio, wal, out.get_mut(at..)?)?;
    at += barrier(engine, out.get_mut(at..)?)?;
    Some(at)
}

fn barrier(engine: &EngineInfo, out: &mut [u32]) -> Option<usize> {
    let mut cs = Cs::new(out);
    engine_cs::gen12_emit_flush(engine, engine_cs::EMIT_BARRIER, &mut cs);
    (!cs.overflowed()).then_some(cs.len())
}
