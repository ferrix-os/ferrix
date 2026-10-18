// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_execlists_submission.c
// (execlists_context_pre_pin, execlists_dequeue, execlists_update_context,
// __execlists_schedule_in), gt/intel_lrc.c (lrc_pre_pin, lrc_init_state),
// gt/intel_timeline.c and i915_request.h (i915_seqno_passed):
// Copyright © 2014 Intel Corporation (intel_execlists_submission.c, intel_lrc.c)
// Copyright © 2016-2018 Intel Corporation (intel_timeline.c)
// Copyright © 2008-2018 Intel Corporation (i915_request.h)
// Under the MIT notice in this crate's i915/NOTICE.

//! A context on an engine and its requests, as i915's execlists back end
//! keeps them, over memory the caller maps: the context image, the ring,
//! and the engine's status page, where each context's timeline has a seqno.
//!
//! i915's request and scheduler machinery (`i915_request.c`,
//! `i915_scheduler.c`) is left out (`docs/INTEL-GPU.md` §3.6): requests on
//! a context complete in order, a request is complete when its context's
//! seqno has passed it, and the caller decides when to submit. What is
//! kept is what the hardware sees: the descriptor, the image's ring
//! registers, the commands around each request, and the submit queue.
//!
//! The timeline's seqno lives in the engine's status page, one cache line
//! per context from [`TIMELINE_FIRST_DWORD`] (i915 gives each timeline a
//! cache line of a status page of its own, `intel_timeline_pin`).

use crate::engine::{EngineInfo, RENDER_CLASS};
use crate::engine_cs::I915_GEM_HWS_PREEMPT_ADDR;
use crate::execlists::{self, Execlists, Port};
use crate::io::{Mmio, StatusPage};
use crate::lrc::{self, OffsetsError, OffsetsTable, RingState, ctx};
use crate::request::{self, Built, Payload};
use crate::ring::{NoSpace, Ring};

/// The first status-page dword a context's seqno may use: past the CSB
/// (`0x10`..`0x2f`), the preemption flag (`0x32`), the scratch and
/// `I915_GEM_HWS_SEQNO_ADDR` (`0x40`).
pub const TIMELINE_FIRST_DWORD: usize = 0x80;
/// Dwords between two contexts' seqnos: a cache line, as i915's
/// `TIMELINE_SEQNO_BYTES` cachelines.
pub const TIMELINE_STRIDE_DWORDS: usize = 16;
/// How many contexts one status page holds seqnos for.
pub const TIMELINES_PER_PAGE: usize = (1024 - TIMELINE_FIRST_DWORD) / TIMELINE_STRIDE_DWORDS;

/// `i915_seqno_passed(seq1, seq2)`: whether `seq1` is at or past `seq2`,
/// across the wrap.
#[must_use]
pub const fn i915_seqno_passed(seq1: u32, seq2: u32) -> bool {
    seq1.wrapping_sub(seq2) as i32 >= 0
}

/// The engine-side table of an engine: which image layout it has.
#[must_use]
pub const fn offsets_table(engine: &EngineInfo) -> OffsetsTable {
    if engine.class == RENDER_CLASS {
        OffsetsTable::Rcs
    } else {
        OffsetsTable::Xcs
    }
}

/// Where a context's objects are in the GGTT, and its VM.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ContextPlace {
    /// The image's GGTT offset, page-aligned: [`lrc::alloc_state_size`]
    /// bytes.
    pub image_ggtt: u32,
    /// The ring's GGTT offset.
    pub ring_ggtt: u32,
    /// The ring's size in bytes, a power of two of at least 4 KiB.
    pub ring_size: u32,
    /// The engine's status page in the GGTT.
    pub hwsp_ggtt: u32,
    /// Which timeline slot of the status page is this context's, below
    /// [`TIMELINES_PER_PAGE`].
    pub timeline: usize,
    /// `context_tag`: 0 to 62, unique among the engine's live contexts.
    pub tag: u32,
    /// The PML4 of the context's VM.
    pub pml4: u64,
}

/// Why a context could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ContextError {
    /// The image, the default state or a table did not fit.
    Image(OffsetsError),
    /// The indirect-context batch did not fit its page.
    IndirectBatch,
    /// A ring size, timeline slot or tag out of range, or an unaligned
    /// GGTT offset.
    Place,
}

/// One context on one engine (`struct intel_context` with its ring,
/// timeline and `lrc` state).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct HwContext {
    /// The engine.
    pub engine: EngineInfo,
    /// Where its objects are.
    pub place: ContextPlace,
    /// `ce->ring`'s bookkeeping.
    pub ring: Ring,
    /// `ce->lrc.desc`: the image's address and mode in the low dword, the
    /// `ccid` in the high.
    pub desc: u64,
    /// The last seqno emitted on this context's timeline.
    pub seqno: u32,
    /// Where the last submitted request ends (`rq->wa_tail` of the last one
    /// in the image): what `CTX_RING_TAIL` is set to on the next
    /// submission.
    pub submitted_tail: u32,
    /// `intel_context_is_banned`: a hang was this context's; nothing more is
    /// submitted on it.
    pub banned: bool,
    /// Whether the image was restore-inhibited (no default state yet): its
    /// first run is what the default state is recorded from.
    pub inhibited: bool,
}

impl HwContext {
    /// `intel_context_create` and its first pin, for execlists:
    /// `lrc_init_state` (from `default` when the engine's default state is
    /// recorded), `setup_indirect_ctx_bb` into the image's indirect page,
    /// `lrc_update_regs` (the ring, the power clock state, the indirect
    /// batch), and the descriptor (`lrc_descriptor` with the tag's `ccid`).
    /// `image` is the whole context object, as dwords.
    ///
    /// # Errors
    ///
    /// [`ContextError`].
    pub fn new(
        engine: EngineInfo,
        place: ContextPlace,
        image: &mut [u32],
        default: Option<&[u32]>,
    ) -> Result<Self, ContextError> {
        Self::build(
            engine,
            place,
            image,
            default.map_or(Init::Inhibited, Init::Copy),
        )
    }

    /// [`HwContext::new`] for an image the engine's default state was
    /// already copied into ([`lrc::lrc_init_state_after_copy`]).
    ///
    /// # Errors
    ///
    /// [`ContextError`].
    pub fn new_from_copied_default(
        engine: EngineInfo,
        place: ContextPlace,
        image: &mut [u32],
    ) -> Result<Self, ContextError> {
        Self::build(engine, place, image, Init::Copied)
    }

    fn build(
        engine: EngineInfo,
        place: ContextPlace,
        image: &mut [u32],
        default: Init<'_>,
    ) -> Result<Self, ContextError> {
        let aligned = |x: u32| x.is_multiple_of(4096);
        if !place.ring_size.is_power_of_two()
            || place.ring_size < 4096
            || place.timeline >= TIMELINES_PER_PAGE
            || place.tag > 62
            || !aligned(place.image_ggtt)
            || !aligned(place.ring_ggtt)
            || !aligned(place.hwsp_ggtt)
        {
            return Err(ContextError::Place);
        }
        let table = offsets_table(&engine);
        let inhibited = match default {
            Init::Copy(default) => {
                lrc::lrc_init_state(image, Some(default), table, engine.mmio_base, place.pml4)
            }
            Init::Inhibited => {
                lrc::lrc_init_state(image, None, table, engine.mmio_base, place.pml4)
            }
            Init::Copied => {
                lrc::lrc_init_state_after_copy(image, false, table, engine.mmio_base, place.pml4)
                    .map(|()| false)
            }
        }
        .map_err(ContextError::Image)?;
        let (_, wa_page) = lrc::alloc_state_size(table);
        let indirect_ggtt = place.image_ggtt + wa_page * 4096;
        let wa = wa_page as usize * 1024;
        let indirect_size = image
            .get_mut(wa..wa + 1024)
            .and_then(|page| lrc::setup_indirect_ctx_bb(page, &engine, place.image_ggtt))
            .ok_or(ContextError::IndirectBatch)?;
        let regs = lrc::reg_state(image).ok_or(ContextError::Image(OffsetsError::TooSmall))?;
        lrc::lrc_update_regs(
            regs,
            RingState {
                start: place.ring_ggtt,
                head: 0,
                tail: 0,
                size: place.ring_size,
            },
            engine.class == RENDER_CLASS,
            indirect_ggtt,
            indirect_size,
        );
        let lrca = execlists::lrc_descriptor(place.image_ggtt, true);
        let ccid = execlists::context_ccid(
            place.tag,
            execlists::engine_ccid(engine.class, engine.instance),
        );
        Ok(HwContext {
            engine,
            place,
            ring: Ring::new(place.ring_size),
            // lrc_update_regs: "return lrc_descriptor(ce) | CTX_DESC_FORCE_RESTORE"
            desc: execlists::descriptor(lrca, ccid) | execlists::CTX_DESC_FORCE_RESTORE,
            seqno: 0,
            submitted_tail: 0,
            banned: false,
            inhibited,
        })
    }

    /// The GGTT address of this context's seqno.
    #[must_use]
    pub const fn seqno_ggtt(&self) -> u32 {
        self.place.hwsp_ggtt + (self.seqno_dword() * 4) as u32
    }

    /// The status-page dword of this context's seqno.
    #[must_use]
    pub const fn seqno_dword(&self) -> usize {
        TIMELINE_FIRST_DWORD + self.place.timeline * TIMELINE_STRIDE_DWORDS
    }

    /// The software context id the status buffer names this context by.
    #[must_use]
    pub const fn sw_ctx_id(&self) -> u32 {
        execlists::ccid_sw_ctx_id((self.desc >> 32) as u32)
    }

    /// `i915_request_create` to `i915_request_add`: the next seqno's request
    /// written into `ring` (the ring's dwords), running `payload`. Nothing
    /// is submitted. Returns the seqno and where it lies.
    ///
    /// # Errors
    ///
    /// [`NoSpace`]: retire requests (by [`HwContext::retire`]) and try
    /// again. The ring is left as it was.
    pub fn emit(
        &mut self,
        ring: &mut [u32],
        payload: Payload<'_>,
    ) -> Result<(u32, Built), NoSpace> {
        let seqno = self.seqno.wrapping_add(1);
        let seqno_ggtt = self.seqno_ggtt();
        let preempt = self.place.hwsp_ggtt + I915_GEM_HWS_PREEMPT_ADDR;
        let built = request::build_payload(
            &mut self.ring,
            ring,
            &self.engine,
            payload,
            seqno,
            seqno_ggtt,
            preempt,
        )?;
        self.seqno = seqno;
        Ok((seqno, built))
    }

    /// The seqno the engine last wrote for this context.
    pub fn hwsp_seqno<P: StatusPage + ?Sized>(&self, hwsp: &mut P) -> u32 {
        hwsp.read32(self.seqno_dword())
    }

    /// `i915_request_completed` for this context's request `seqno`.
    pub fn is_complete<P: StatusPage + ?Sized>(&self, hwsp: &mut P, seqno: u32) -> bool {
        i915_seqno_passed(self.hwsp_seqno(hwsp), seqno)
    }

    /// `i915_request_retire`'s ring bookkeeping: the engine has read the ring
    /// up to `head` (a completed request's `wa_tail`), so the space behind it
    /// is free again.
    pub fn retire(&mut self, head: u32) {
        self.ring.update_head(head);
    }

    /// `execlists_update_context` for a submission that runs the ring up to
    /// `tail` (the last request's `wa_tail`): `CTX_RING_TAIL` written into
    /// `regs` (the image's register-state page), and the descriptor, with
    /// `CTX_DESC_FORCE_RESTORE` when the tail does not advance. The caller
    /// orders the image's writes before the submit queue's (`wmb`).
    pub fn update_context(&mut self, regs: &mut [u32], tail: u32) -> u64 {
        let prev = self.submitted_tail;
        let desc = execlists::update_context_desc(self.desc, self.place.ring_size, tail, prev);
        if let Some(slot) = regs.get_mut(ctx::RING_TAIL) {
            *slot = tail;
        }
        self.submitted_tail = tail;
        self.desc &= !execlists::CTX_DESC_FORCE_RESTORE;
        desc
    }
}

/// An engine's submission state: its ports, and the status page's place.
#[derive(Clone, Copy, Debug)]
pub struct EngineState<T: Copy> {
    /// The engine.
    pub engine: EngineInfo,
    /// The ports.
    pub execlists: Execlists<T>,
}

impl<T: Copy> EngineState<T> {
    /// `execlists_sanitize` for an engine whose status page is `hwsp`: the
    /// page cleared and the CSB pointers reset, before
    /// [`crate::gt::intel_engine_resume`] points the engine at it.
    pub fn sanitize<M, P>(engine: EngineInfo, mmio: &mut M, hwsp: &mut P) -> Self
    where
        M: Mmio + ?Sized,
        P: StatusPage + ?Sized,
    {
        for dword in 0..1024 {
            hwsp.write32(dword, 0);
        }
        let mut execlists = Execlists::new();
        execlists.csb_head = execlists::reset_csb_pointers(mmio, hwsp, engine.mmio_base);
        EngineState { engine, execlists }
    }

    /// Whether a submission may be made now: none is pending
    /// (`execlists->pending[0]`). Ask before [`HwContext::update_context`],
    /// which records the tail as submitted.
    #[must_use]
    pub fn can_submit(&self) -> bool {
        !self.execlists.is_pending()
    }

    /// `execlists_dequeue` for one context: `desc`, from
    /// [`HwContext::update_context`] after [`EngineState::can_submit`],
    /// written alone into port 0, the context identified to the caller as
    /// `token`. Returns whether it was submitted.
    pub fn submit<M: Mmio + ?Sized>(&mut self, mmio: &mut M, desc: u64, token: T) -> bool {
        if !self.can_submit() {
            return false;
        }
        // wmb(): the image's tail before the submit queue's writes.
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.execlists.submit(
            mmio,
            self.engine.mmio_base,
            [
                Some(Port {
                    context: token,
                    desc,
                }),
                None,
            ],
        )
    }

    /// [`Execlists::process_csb`] on this engine.
    pub fn process_csb<M, P, C>(
        &mut self,
        mmio: &mut M,
        hwsp: &mut P,
        clock: &mut C,
        out: impl FnMut(Port<T>),
    ) -> execlists::CsbReport
    where
        M: Mmio + ?Sized,
        P: StatusPage + ?Sized,
        C: crate::io::Clock + ?Sized,
    {
        self.execlists
            .process_csb(mmio, hwsp, clock, self.engine.mmio_base, out)
    }
}

/// How a context's image starts.
#[derive(Clone, Copy, Debug)]
enum Init<'a> {
    /// Restore-inhibited: no default yet.
    Inhibited,
    /// From this default state.
    Copy(&'a [u32]),
    /// The default state is already in the image.
    Copied,
}
