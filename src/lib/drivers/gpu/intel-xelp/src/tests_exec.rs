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

/// Map `pages` (device addresses the model holds) at `address` in the
/// rig's VM.
fn map(rig: &Rig, address: u64, pages: &[u64]) {
    let mut gpu = rig.gpu.borrow_mut();
    for &page in pages {
        gpu.add_page(page);
    }
    let vm = rig.vm;
    vm.alloc_va_range(&mut gpu.tables, address, pages.len() as u64 * 4096)
        .expect("tables");
    let _ = vm
        .insert_entries(&mut gpu.tables, pages.iter().copied(), address, 0)
        .expect("allocated");
}

#[test]
fn a_batch_runs_from_the_ppgtt() {
    use crate::commands::{Cs, emit_store_dword};
    let rig = Rig::new(ENGINES[0]);
    let mut state = rig.bring_up();
    let default = rig.record_default(&mut state);
    let mut context = rig.context(rig.place(IMAGE2, RING2, 1), Some(&default));
    // A batch page and a target page, at addresses above 4 GiB.
    let batch_va = 0x1_0000_0000_u64;
    let target_va = 0x1_0000_1000_u64;
    map(&rig, batch_va, &[0x800_0000, 0x800_1000]);
    let mut batch = [0_u32; 6];
    let mut cs = Cs::new(&mut batch);
    emit_store_dword(&mut cs, target_va + 0x80, 0x5eed_0001);
    cs.push(gpu::MI_BATCH_BUFFER_END);
    {
        let mut gpu = rig.gpu.borrow_mut();
        let page = gpu.pages.get_mut(&0x800_0000).expect("page");
        page[..6].copy_from_slice(&batch);
    }
    let (seqno, tail) = rig.emit(
        &mut context,
        Payload::Batch {
            address: batch_va,
            flags: 0,
        },
    );
    let _ = rig.submit(&mut state, &mut context, tail);
    assert_eq!(rig.gpu.borrow_mut().run(), Ran::Idle);
    let (report, _) = rig.process(&mut state);
    assert_eq!((report.promoted, report.completed), (1, 1));
    assert!(rig.complete(&context, seqno));
    assert_eq!(rig.gpu.borrow().pages[&0x800_1000][0x20], 0x5eed_0001);
    // Unmapped again, the batch's address reads the scratch page: a batch
    // there would run zeros (MI_NOOPs) and never reach its end.
    let mut gpu = rig.gpu.borrow_mut();
    let vm = rig.vm;
    vm.clear_range(&mut gpu.tables, batch_va, 2 * 4096)
        .expect("range");
    assert_eq!(gpu.ppgtt_read(vm.pml4, batch_va), 0);
}

#[test]
fn a_fast_copy_runs_on_the_copy_engine() {
    use crate::commands::{Cs, emit_fast_copy};
    let rig = Rig::new(ENGINES[1]);
    let mut state = rig.bring_up();
    let default = rig.record_default(&mut state);
    let mut context = rig.context(rig.place(IMAGE2, RING2, 1), Some(&default));
    let src_va = 0x20_0000_u64;
    let dst_va = 0x40_0000_u64;
    map(&rig, src_va, &[0x900_0000, 0x900_1000]);
    map(&rig, dst_va, &[0xa00_0000, 0xa00_1000]);
    {
        let mut gpu = rig.gpu.borrow_mut();
        for (n, page) in [0x900_0000_u64, 0x900_1000].iter().enumerate() {
            let words = gpu.pages.get_mut(page).expect("page");
            for (i, word) in words.iter_mut().enumerate() {
                *word = (n as u32) << 16 | i as u32;
            }
        }
    }
    let mut copy = [0_u32; 10];
    emit_fast_copy(&mut Cs::new(&mut copy), dst_va, src_va, 2 * 4096);
    let (seqno, tail) = rig.emit(&mut context, Payload::Commands(&copy));
    let _ = rig.submit(&mut state, &mut context, tail);
    assert_eq!(rig.gpu.borrow_mut().run(), Ran::Idle);
    let _ = rig.process(&mut state);
    assert!(rig.complete(&context, seqno));
    let gpu = rig.gpu.borrow();
    assert_eq!(gpu.pages[&0xa00_0000], gpu.pages[&0x900_0000]);
    assert_eq!(gpu.pages[&0xa00_1000], gpu.pages[&0x900_1000]);
    assert_eq!(gpu.pages[&0xa00_1000][5], (1 << 16) | 5);
}

#[test]
fn context_workarounds_are_read_back_from_a_request() {
    let rig = Rig::new(ENGINES[0]);
    let mut state = rig.bring_up();
    let default = rig.record_default(&mut state);
    let mut context = rig.context(rig.place(IMAGE2, RING2, 1), Some(&default));
    let wal = workarounds::ctx_workarounds(&rig.engine, mocs::UC_INDEX);
    let mut srm = [0_u32; 256];
    let n = workarounds::wa_list_srm(&wal, PAGE, &mut srm).expect("room");
    let skipped = wal
        .iter()
        .filter(|wa| workarounds::mcr_range(wa.reg.offset()))
        .count();
    assert_eq!(n, 4 * (wal.len() - skipped));
    let (seqno, tail) = rig.emit(&mut context, Payload::Commands(&srm[..n]));
    let _ = rig.submit(&mut state, &mut context, tail);
    assert_eq!(rig.gpu.borrow_mut().run(), Ran::Idle);
    let _ = rig.process(&mut state);
    assert!(rig.complete(&context, seqno));
    let results = rig.gpu.borrow().objects[&PAGE].clone();
    assert_eq!(
        workarounds::wa_list_verify_results(&wal, &results).count(),
        0
    );
    // One register that lost its bits is named.
    let first = wal
        .iter()
        .position(|wa| !workarounds::mcr_range(wa.reg.offset()))
        .expect("one");
    let mut lost = results;
    lost[first] = !lost[first];
    let named: Vec<_> = workarounds::wa_list_verify_results(&wal, &lost).collect();
    assert_eq!(named.len(), 1);
    assert_eq!(named[0].0, wal.iter().nth(first).expect("entry").reg);
}

/// `igt_spinner`'s batch: an arbitration point, then a jump back to its own
/// start, from the PPGTT.
fn spinner(at: u64) -> [u32; 4] {
    [
        gpu::MI_ARB_CHECK,
        gpu::MI_BATCH_BUFFER_START_GEN8 | (1 << 8),
        at as u32,
        (at >> 32) as u32,
    ]
}

/// The rig's engine resumed again after a reset (`intel_engine_resume`).
fn resume(rig: &Rig) {
    let report = gt::intel_engine_resume(
        &mut Regs(&rig.gpu),
        rig.engine.mmio_base,
        &workarounds::engine_workarounds(&rig.engine, mocs::UC_INDEX),
        &workarounds::whitelist(&rig.engine),
        HWSP,
    );
    assert_eq!(report.still_in_error, 0);
}

#[test]
fn a_hung_batch_is_reset_skipped_and_banned() {
    use crate::reset;
    let rig = Rig::new(ENGINES[0]);
    let mut state = rig.bring_up();
    let default = rig.record_default(&mut state);
    let mut hung = rig.context(rig.place(IMAGE2, RING2, 1), Some(&default));
    let at = 0x3_0000_0000_u64;
    map(&rig, at, &[0xb00_0000]);
    rig.gpu
        .borrow_mut()
        .pages
        .get_mut(&0xb00_0000)
        .expect("page")[..4]
        .copy_from_slice(&spinner(at));
    let (seqno, built) = {
        let mut gpu = rig.gpu.borrow_mut();
        hung.emit(
            gpu.object(RING2),
            Payload::Batch {
                address: at,
                flags: 0,
            },
        )
        .expect("room")
    };
    let _ = rig.submit(&mut state, &mut hung, built.tail);
    assert_eq!(rig.gpu.borrow_mut().run(), Ran::Hung);
    let (report, _) = rig.process(&mut state);
    assert_eq!((report.promoted, report.completed), (1, 0));
    assert!(!rig.complete(&hung, seqno));

    // The engine reset alone: stopped, made ready, its domain reset.
    let mut clock = Ticks::default();
    let reset =
        reset::intel_engine_reset(&mut Regs(&rig.gpu), &mut clock, &rig.engine).expect("a domain");
    assert!(reset.is_reset(), "{reset:?}");
    assert!(reset.stopped.is_ok());
    {
        let gpu = rig.gpu.borrow();
        // gen6_hw_domain_reset writes the domain twice below 12.70.
        assert_eq!(gpu.resets, 2);
        assert_eq!(
            gpu.regs[&crate::generated::intel_gt_regs::GEN6_GDRST.offset()],
            0x2
        );
        // Cancelled again: the ring is not left stopped.
        assert!(!gpu.stopped && !gpu.reset_requested);
    }
    // The ports: the hung context was active and is handed back.
    let mut cancelled = Vec::new();
    state.execlists.reset(
        &mut Regs(&rig.gpu),
        &mut Status(&rig.gpu),
        rig.engine.mmio_base,
        |p| {
            cancelled.push(p);
        },
    );
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].context, 1);
    resume(&rig);

    // Rewound: the batch skipped, the context banned, its ring replayed
    // from the request's head so the breadcrumbs write its seqno.
    {
        let mut gpu = rig.gpu.borrow_mut();
        request::skip(gpu.object(RING2), RING_SIZE, &built);
        assert!(
            gpu.objects[&RING2][(built.infix / 4) as usize..(built.postfix / 4) as usize]
                .iter()
                .all(|&dw| dw == 0)
        );
    }
    let desc = {
        let mut gpu = rig.gpu.borrow_mut();
        let regs = lrc::reg_state(gpu.object(IMAGE2)).expect("page");
        let desc = reset::rewind(&mut hung, regs, &built, &[]);
        assert_eq!(regs[ctx::RING_HEAD], built.head);
        assert_eq!(regs[ctx::RING_TAIL], built.tail);
        assert_eq!(regs[lrc::lrc_ring_mi_mode() + 1] & lrc::STOP_RING, 0);
        desc
    };
    assert!(hung.banned);
    assert_ne!(desc & CTX_DESC_FORCE_RESTORE, 0);
    assert!(state.submit(&mut Regs(&rig.gpu), desc, 1));
    assert_eq!(rig.gpu.borrow_mut().run(), Ran::Idle);
    let (report, out) = rig.process(&mut state);
    assert_eq!(
        (report.promoted, report.completed, report.error),
        (1, 1, None)
    );
    assert_eq!(out.len(), 1);
    assert!(
        rig.complete(&hung, seqno),
        "the replayed breadcrumb wrote the seqno"
    );

    // The engine runs other contexts again.
    let mut other = rig.context(rig.place(IMAGE, RING, 2), Some(&default));
    let (seqno, tail) = rig.emit(&mut other, Payload::Commands(&store(0x20, 0xa11e_0004)));
    let _ = rig.submit(&mut state, &mut other, tail);
    assert_eq!(rig.gpu.borrow_mut().run(), Ran::Idle);
    let _ = rig.process(&mut state);
    assert!(rig.complete(&other, seqno));
    assert_eq!(rig.gpu.borrow().objects[&PAGE][8], 0xa11e_0004);
}

#[test]
fn an_engine_that_never_gets_ready_is_not_reset() {
    let rig = Rig::new(ENGINES[1]);
    let _state = rig.bring_up();
    rig.gpu.borrow_mut().never_ready = true;
    let mut clock = Ticks::default();
    let report = crate::reset::intel_engine_reset(&mut Regs(&rig.gpu), &mut clock, &rig.engine)
        .expect("a domain");
    assert!(report.ready.is_err());
    assert!(!report.is_reset());
    let gpu = rig.gpu.borrow();
    assert_eq!(gpu.resets, 0, "GEN6_GDRST was not written");
    // The request was still cancelled, as gen8_reset_engines' skip_reset does.
    assert!(!gpu.reset_requested);
    // A video engine has no reset domain here.
    assert!(crate::reset::reset_domain(&ENGINES[2]).is_none());
}

#[test]
fn a_rectangle_is_blitted_into_a_wider_surface_and_back() {
    use crate::commands::{Cs, emit_rect_copy, gen8_emit_flush_dw};
    let rig = Rig::new(ENGINES[1]);
    let mut state = rig.bring_up();
    let default = rig.record_default(&mut state);
    let mut context = rig.context(rig.place(IMAGE2, RING2, 1), Some(&default));
    // A "screen" two pages a row (8 KiB pitch), 4 rows; a 16x4 rectangle at
    // pixel 100 of each row; the source and the read-back packed (64 bytes
    // a row).
    let (screen, src, back) = (0x70_0000_u64, 0x80_0000_u64, 0x90_0000_u64);
    let screen_pages: Vec<u64> = (0..8).map(|n| 0xc00_0000 + n * 4096).collect();
    map(&rig, screen, &screen_pages);
    map(&rig, src, &[0xd00_0000]);
    map(&rig, back, &[0xe00_0000]);
    {
        let mut gpu = rig.gpu.borrow_mut();
        for (i, word) in gpu
            .pages
            .get_mut(&0xd00_0000)
            .expect("page")
            .iter_mut()
            .enumerate()
            .take(64)
        {
            *word = 0x0055_0000 | i as u32;
        }
    }
    let dst = screen + 100 * 4;
    let mut copy = [0_u32; 24];
    {
        let mut cs = Cs::new(&mut copy);
        emit_rect_copy(&mut cs, dst, 8192, src, 64, 16, 4);
        gen8_emit_flush_dw(&mut cs, 0, 0, 0);
        emit_rect_copy(&mut cs, back, 64, dst, 8192, 16, 4);
    }
    let (seqno, tail) = rig.emit(&mut context, Payload::Commands(&copy));
    let _ = rig.submit(&mut state, &mut context, tail);
    assert_eq!(rig.gpu.borrow_mut().run(), Ran::Idle);
    let _ = rig.process(&mut state);
    assert!(rig.complete(&context, seqno));
    let mut gpu = rig.gpu.borrow_mut();
    let vm = rig.vm;
    // Row 2, pixel 103 of the screen holds source pixel 2 * 16 + 3.
    assert_eq!(
        gpu.ppgtt_read(vm.pml4, screen + 2 * 8192 + 103 * 4),
        0x0055_0000 | 35
    );
    // Outside the rectangle nothing was written.
    assert_eq!(gpu.ppgtt_read(vm.pml4, screen + 2 * 8192 + 116 * 4), 0);
    // And it came back whole.
    let back_page = gpu.pages[&0xe00_0000][..64].to_vec();
    let src_page = gpu.pages[&0xd00_0000][..64].to_vec();
    assert_eq!(back_page, src_page);
}

/// card0's blit (H5): a rectangle addressed by coordinates in the command,
/// both surfaces at page-aligned bases, lands at its place in the wider
/// surface and nowhere else.
#[test]
fn a_rectangle_is_blitted_by_its_coordinates() {
    use crate::commands::{Cs, emit_rect_copy_at};
    let rig = Rig::new(ENGINES[1]);
    let mut state = rig.bring_up();
    let default = rig.record_default(&mut state);
    let mut context = rig.context(rig.place(IMAGE2, RING2, 1), Some(&default));
    // A "screen" two pages a row (8 KiB pitch), 4 rows; a source 64 bytes a
    // row (16 pixels), 4 rows.
    let (screen, src) = (0x70_0000_u64, 0x80_0000_u64);
    let screen_pages: Vec<u64> = (0..8).map(|n| 0xc00_0000 + n * 4096).collect();
    map(&rig, screen, &screen_pages);
    map(&rig, src, &[0xd00_0000]);
    {
        let mut gpu = rig.gpu.borrow_mut();
        for (i, word) in gpu
            .pages
            .get_mut(&0xd00_0000)
            .expect("page")
            .iter_mut()
            .enumerate()
            .take(64)
        {
            *word = 0x0066_0000 | i as u32;
        }
    }
    // Source pixels (4, 1) to (11, 2), 8 by 2, to screen (100, 2).
    let mut copy = [0_u32; 10];
    {
        let mut cs = Cs::new(&mut copy);
        emit_rect_copy_at(&mut cs, screen, 8192, (100, 2), src, 64, (4, 1), (8, 2));
    }
    let (seqno, tail) = rig.emit(&mut context, Payload::Commands(&copy));
    let _ = rig.submit(&mut state, &mut context, tail);
    assert_eq!(rig.gpu.borrow_mut().run(), Ran::Idle);
    let _ = rig.process(&mut state);
    assert!(rig.complete(&context, seqno));
    let mut gpu = rig.gpu.borrow_mut();
    let vm = rig.vm;
    // Screen (100, 2) holds source (4, 1); screen (107, 3) source (11, 2).
    assert_eq!(
        gpu.ppgtt_read(vm.pml4, screen + 2 * 8192 + 100 * 4),
        0x0066_0000 | 20
    );
    assert_eq!(
        gpu.ppgtt_read(vm.pml4, screen + 3 * 8192 + 107 * 4),
        0x0066_0000 | 43
    );
    // Around the rectangle nothing was written.
    assert_eq!(gpu.ppgtt_read(vm.pml4, screen + 2 * 8192 + 99 * 4), 0);
    assert_eq!(gpu.ppgtt_read(vm.pml4, screen + 2 * 8192 + 108 * 4), 0);
    assert_eq!(gpu.ppgtt_read(vm.pml4, screen + 8192 + 100 * 4), 0);
    assert_eq!(gpu.ppgtt_read(vm.pml4, screen + 4 * 8192 + 100 * 4), 0);
}
