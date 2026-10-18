//! Host tests for M2's submission: the engine brought up, the first context
//! run restore-inhibited with the context workarounds and its image
//! recorded as the default, contexts made from it, requests through the
//! submit queue and the status buffer, on the model engine
//! ([`crate::model`]).

extern crate std;

use core::cell::RefCell;
use std::vec::Vec;

use crate::commands::gpu;
use crate::engine::{ENGINES, EngineInfo};
use crate::execlists::{CTX_DESC_FORCE_RESTORE, GEN12_IDLE_CTX_ID, Port};
use crate::lrc::{self, OffsetsTable, ctx};
use crate::model::{Gpu, Ran, Regs, Status, Ticks};
use crate::ppgtt::Ppgtt;
use crate::request::{self, Payload};
use crate::submission::{ContextPlace, EngineState, HwContext};
use crate::{gt, mocs, workarounds};

pub(crate) const HWSP: u32 = 0x10_0000;
pub(crate) const IMAGE: u32 = 0x20_0000;
pub(crate) const RING: u32 = 0x30_0000;
pub(crate) const PAGE: u32 = 0x40_0000;
pub(crate) const IMAGE2: u32 = 0x50_0000;
pub(crate) const RING2: u32 = 0x60_0000;
pub(crate) const RING_SIZE: u32 = 16 << 10;

/// An engine on the model with its status page, a VM, and the objects of
/// two contexts and a target page in the GGTT.
pub(crate) struct Rig {
    pub(crate) gpu: RefCell<Gpu>,
    pub(crate) vm: Ppgtt,
    pub(crate) engine: EngineInfo,
}

impl Rig {
    pub(crate) fn new(engine: EngineInfo) -> Self {
        let mut gpu = Gpu::new(&engine, HWSP);
        let image = lrc::alloc_state_size(crate::submission::offsets_table(&engine)).0;
        gpu.add_object(IMAGE, image);
        gpu.add_object(IMAGE2, image);
        gpu.add_object(RING, RING_SIZE);
        gpu.add_object(RING2, RING_SIZE);
        gpu.add_object(PAGE, 4096);
        let vm = Ppgtt::create(&mut gpu.tables).expect("pages");
        let _ = gpu.vms.insert(vm.pml4, vm);
        Rig {
            gpu: RefCell::new(gpu),
            vm,
            engine,
        }
    }

    pub(crate) fn place(&self, image: u32, ring: u32, slot: usize) -> ContextPlace {
        ContextPlace {
            image_ggtt: image,
            ring_ggtt: ring,
            ring_size: RING_SIZE,
            hwsp_ggtt: HWSP,
            timeline: slot,
            tag: slot as u32,
            pml4: self.vm.pml4,
        }
    }

    /// `execlists_sanitize`, then `intel_engine_resume`.
    pub(crate) fn bring_up(&self) -> EngineState<u32> {
        let mut regs = Regs(&self.gpu);
        let mut status = Status(&self.gpu);
        let state = EngineState::sanitize(self.engine, &mut regs, &mut status);
        let report = gt::intel_engine_resume(
            &mut regs,
            self.engine.mmio_base,
            &workarounds::engine_workarounds(&self.engine, mocs::UC_INDEX),
            &workarounds::whitelist(&self.engine),
            HWSP,
        );
        assert_eq!(report.still_in_error, 0);
        state
    }

    pub(crate) fn context(&self, place: ContextPlace, default: Option<&[u32]>) -> HwContext {
        let mut gpu = self.gpu.borrow_mut();
        HwContext::new(self.engine, place, gpu.object(place.image_ggtt), default).expect("fits")
    }

    /// Emit `payload` on `context` and return the seqno and the tail to
    /// submit (`rq->tail`).
    pub(crate) fn emit(&self, context: &mut HwContext, payload: Payload<'_>) -> (u32, u32) {
        let mut gpu = self.gpu.borrow_mut();
        let (seqno, built) = context
            .emit(gpu.object(context.place.ring_ggtt), payload)
            .expect("room");
        (seqno, built.tail)
    }

    /// Submit `context` up to `tail`; the descriptor written.
    pub(crate) fn submit(
        &self,
        state: &mut EngineState<u32>,
        context: &mut HwContext,
        tail: u32,
    ) -> u64 {
        assert!(state.can_submit(), "nothing pending");
        let desc = {
            let mut gpu = self.gpu.borrow_mut();
            let regs = lrc::reg_state(gpu.object(context.place.image_ggtt)).expect("page");
            context.update_context(regs, tail)
        };
        assert!(state.submit(&mut Regs(&self.gpu), desc, context.place.tag));
        desc
    }

    /// Drain the status buffer; the contexts that left the engine.
    pub(crate) fn process(
        &self,
        state: &mut EngineState<u32>,
    ) -> (crate::execlists::CsbReport, Vec<Port<u32>>) {
        let mut out = Vec::new();
        let mut clock = Ticks::default();
        let report = state.process_csb(
            &mut Regs(&self.gpu),
            &mut Status(&self.gpu),
            &mut clock,
            |port| {
                out.push(port);
            },
        );
        (report, out)
    }

    pub(crate) fn complete(&self, context: &HwContext, seqno: u32) -> bool {
        context.is_complete(&mut Status(&self.gpu), seqno)
    }

    /// The first context, restore-inhibited, runs the context workarounds;
    /// its saved image becomes the default. Returns the default.
    pub(crate) fn record_default(&self, state: &mut EngineState<u32>) -> Vec<u32> {
        let mut kernel = self.context(self.place(IMAGE, RING, 0), None);
        assert!(kernel.inhibited);
        let wal = workarounds::ctx_workarounds(&self.engine, mocs::UC_INDEX);
        let mut payload = [0_u32; 512];
        let n = request::ctx_wa_payload(&mut Regs(&self.gpu), &self.engine, &wal, &mut payload)
            .expect("room");
        let (seqno, tail) = self.emit(&mut kernel, Payload::Commands(&payload[..n]));
        let _ = self.submit(state, &mut kernel, tail);
        assert_eq!(self.gpu.borrow_mut().run(), Ran::Idle);
        let (report, out) = self.process(state);
        assert_eq!(
            (report.promoted, report.completed, report.error),
            (1, 1, None)
        );
        assert_eq!(out.len(), 1);
        assert!(self.complete(&kernel, seqno));
        assert!(state.execlists.is_idle());
        let mut default = std::vec![0_u32; (lrc::context_size(crate::submission::offsets_table(&self.engine)) / 4) as usize];
        let mut gpu = self.gpu.borrow_mut();
        lrc::record_default_state(
            gpu.object(IMAGE),
            &mut default,
            crate::submission::offsets_table(&self.engine),
        )
        .expect("fits");
        // Every workaround was loaded (beside the AUX invalidations of the
        // request's three flushes).
        for wa in wal.iter() {
            assert!(
                gpu.lri.iter().any(|&(reg, _)| reg == wa.reg.offset()),
                "{} {:#x}",
                self.engine.name,
                wa.reg.offset()
            );
        }
        assert_eq!(gpu.lri.len(), wal.len() + 3, "{}", self.engine.name);
        default
    }
}

/// A store into the target page through the GGTT, from the ring.
pub(crate) fn store(offset: u32, value: u32) -> [u32; 4] {
    [
        gpu::MI_STORE_DWORD_IMM_GEN4 | gpu::MI_USE_GGTT,
        PAGE + offset,
        0,
        value,
    ]
}

#[test]
fn engine_comes_up_with_its_status_buffer_reset() {
    let rig = Rig::new(ENGINES[0]);
    let state = rig.bring_up();
    let gpu = rig.gpu.borrow();
    let hwsp = &gpu.objects[&HWSP];
    // reset_csb_pointers: both pointers at the last entry, every entry -1.
    assert_eq!(hwsp[0x2f], 11);
    assert!(hwsp[0x10..0x28].iter().all(|&dw| dw == u32::MAX));
    assert_eq!(gpu.regs[&(0x2000 + 0x3a0)], (0xffff << 16) | (11 << 8) | 11);
    assert_eq!(state.execlists.csb_head, 11);
    // enable_execlists pointed the engine at the status page.
    assert_eq!(gpu.regs[&0x2080], HWSP);
}

#[test]
fn the_default_image_is_recorded_from_the_first_context() {
    for engine in &ENGINES[..2] {
        let rig = Rig::new(*engine);
        let mut state = rig.bring_up();
        let default = rig.record_default(&mut state);
        // The engine saved the ring's head in the image it ran.
        let saved_head = default[1024 + ctx::RING_HEAD];
        assert_ne!(saved_head, 0, "{}", engine.name);

        let mut second = rig.context(rig.place(IMAGE2, RING2, 1), Some(&default));
        assert!(!second.inhibited);
        let mut gpu = rig.gpu.borrow_mut();
        let image = gpu.object(IMAGE2).clone();
        drop(gpu);
        // From the default: restore no longer inhibited, the ring's
        // registers this context's own, the PPHWSP cleared.
        let regs = &image[1024..2048];
        assert_eq!(
            regs[ctx::CONTEXT_CONTROL] & lrc::CTX_CTRL_ENGINE_CTX_RESTORE_INHIBIT,
            0
        );
        assert_eq!(regs[ctx::RING_START], RING2);
        assert_eq!(regs[ctx::RING_HEAD], 0);
        assert!(image[..1024].iter().all(|&dw| dw == 0));
        // Pages past the register state came from the default.
        let past = default.len().min(4096);
        assert_eq!(image[2048..past], default[2048..past]);
        // Its first request runs.
        let (seqno, tail) = rig.emit(&mut second, Payload::Commands(&store(0x40, 0xc0ff_ee00)));
        let desc = rig.submit(&mut state, &mut second, tail);
        assert_ne!(
            desc & CTX_DESC_FORCE_RESTORE,
            0,
            "a context's first submission restores it"
        );
        assert_eq!(rig.gpu.borrow_mut().run(), Ran::Idle);
        let (report, out) = rig.process(&mut state);
        assert_eq!((report.promoted, report.completed), (1, 1));
        assert_eq!(out[0].context, 1);
        assert!(rig.complete(&second, seqno));
        assert_eq!(
            rig.gpu.borrow().objects[&PAGE][0x10],
            0xc0ff_ee00,
            "{}",
            engine.name
        );
        // The kernel context's request and this one: one interrupt each.
        assert_eq!(rig.gpu.borrow().interrupts, 2);
    }
}

#[test]
fn requests_queue_on_a_context_and_wrap_its_ring() {
    let rig = Rig::new(ENGINES[0]);
    let mut state = rig.bring_up();
    let default = rig.record_default(&mut state);
    let mut context = rig.context(rig.place(IMAGE2, RING2, 1), Some(&default));
    // Two requests, one submission up to the second's tail: both run.
    let (first, _) = rig.emit(&mut context, Payload::Commands(&store(0, 1)));
    let (second, tail) = rig.emit(&mut context, Payload::Commands(&store(4, 2)));
    let desc = rig.submit(&mut state, &mut context, tail);
    assert_ne!(desc & CTX_DESC_FORCE_RESTORE, 0);
    assert!(!state.can_submit(), "one submission at a time");
    assert_eq!(rig.gpu.borrow_mut().run(), Ran::Idle);
    let _ = rig.process(&mut state);
    assert!(rig.complete(&context, first) && rig.complete(&context, second));
    // The same tail again would confuse the engine: force a restore.
    let desc = rig.submit(&mut state, &mut context, tail);
    assert_ne!(desc & CTX_DESC_FORCE_RESTORE, 0);
    assert_eq!(rig.gpu.borrow_mut().run(), Ran::Idle);
    let _ = rig.process(&mut state);
    // Many more, each retired before the next: the 16 KiB ring wraps.
    let mut wraps = 0_u32;
    for value in 0..200_u32 {
        let before = context.ring.emit;
        let (seqno, tail) = rig.emit(
            &mut context,
            Payload::Commands(&store(8 + 4 * (value % 8), value)),
        );
        wraps += u32::from(context.ring.emit < before);
        let desc = rig.submit(&mut state, &mut context, tail);
        assert_eq!(
            desc & CTX_DESC_FORCE_RESTORE,
            0,
            "request {value} advances the tail"
        );
        assert_eq!(rig.gpu.borrow_mut().run(), Ran::Idle);
        let (report, _) = rig.process(&mut state);
        assert_eq!(report.error, None);
        assert!(rig.complete(&context, seqno));
        // i915_request_retire: the ring is free up to where the engine read.
        let head = rig.gpu.borrow().objects[&IMAGE2][1024 + ctx::RING_HEAD];
        context.retire(head);
    }
    assert!(wraps >= 2, "the ring wrapped {wraps} times");
    assert_eq!(rig.gpu.borrow().objects[&PAGE][2 + 199 % 8], 199);
}

#[test]
fn a_status_buffer_entry_missing_from_the_page_is_read_over_mmio() {
    let rig = Rig::new(ENGINES[1]);
    let mut state = rig.bring_up();
    let default = rig.record_default(&mut state);
    let mut context = rig.context(rig.place(IMAGE2, RING2, 1), Some(&default));
    rig.gpu.borrow_mut().csb_to_mmio_only = true;
    let (seqno, tail) = rig.emit(&mut context, Payload::Commands(&store(0, 7)));
    let _ = rig.submit(&mut state, &mut context, tail);
    assert_eq!(rig.gpu.borrow_mut().run(), Ran::Idle);
    let mut clock = Ticks::default();
    let mut out = Vec::new();
    let report = state.process_csb(
        &mut Regs(&rig.gpu),
        &mut Status(&rig.gpu),
        &mut clock,
        |p| out.push(p),
    );
    assert_eq!(
        (report.promoted, report.completed, report.error),
        (1, 1, None)
    );
    // wa_csb_read waited its 10 us for each entry first.
    assert!(clock.now >= 2 * 10);
    assert!(rig.complete(&context, seqno));
    // Each entry was consumed: -1 again in the page.
    let gpu = rig.gpu.borrow();
    assert!(
        gpu.objects[&HWSP][0x10..0x28]
            .iter()
            .all(|&dw| dw == u32::MAX)
    );
}

#[test]
fn a_status_buffer_that_makes_no_sense_is_an_error() {
    let rig = Rig::new(ENGINES[0]);
    let mut state = rig.bring_up();
    let _ = rig.record_default(&mut state);
    // A completion with nothing running.
    rig.gpu
        .borrow_mut()
        .csb((u64::from(1_u32 << 15) << 32) | u64::from(GEN12_IDLE_CTX_ID << 15));
    let (report, out) = rig.process(&mut state);
    assert!(report.error.is_some());
    assert!(out.is_empty());
    // A write pointer out of range.
    {
        let mut gpu = rig.gpu.borrow_mut();
        gpu.object(HWSP)[0x2f] = 12;
    }
    let (report, _) = rig.process(&mut state);
    assert_eq!(report.error, Some(12));
    // A context of the RCS image layout is 14 pages and its object 16.
    assert_eq!(lrc::alloc_state_size(OffsetsTable::Rcs).0, 16 * 4096);
}
