// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/i915/gt/intel_execlists_submission.c
// (execlists_dequeue, execlists_reset_active, __execlists_reset),
// gt/intel_engine_heartbeat.c (the stall a heartbeat finds), gt/intel_gt.c
// (__engines_record_defaults) and i915_request.c (i915_request_retire,
// __i915_request_reset):
// Copyright © 2014 Intel Corporation (intel_execlists_submission.c)
// Copyright © 2019 Intel Corporation (intel_engine_heartbeat.c, intel_gt.c)
// Copyright © 2008-2015 Intel Corporation (i915_request.c)
// Under the MIT notice in this crate's i915/NOTICE.

//! The GPU as the i915 front end's back end sees it (`frontend::Backend`,
//! agreed with M3 on 2026-10-18): VMs, bindings, hardware contexts,
//! submissions and their completions, over what the driver process supplies
//! ([`Platform`]). Everything M2 brought up runs here: the PAT, MOCS and
//! workarounds, RCS0 and BCS0 under execlists, the default images, and an
//! engine reset when a request stalls.
//!
//! i915's scheduler and request machinery stays out (`docs/INTEL-GPU.md`
//! §3.6). What is kept is its rule for the hardware: one context in the
//! ports at a time per engine (no preemption, no timeslices yet), a
//! context's requests retired in order by its seqno, a stalled request
//! reset by its engine alone, the guilty request [`Completion::Hung`] and
//! the rest of its context [`Completion::Cancelled`], skipped and replayed
//! so their breadcrumbs still land. The stall is found as i915's heartbeat
//! finds it: the running request's seqno has not moved for
//! [`HANG_NS`] (its `heartbeat_interval_ms` and `preempt_timeout_ms` cover a
//! similar span), checked on each [`XelpBackend::service`].
//!
//! No memory is allocated: every table has a fixed size, and a full one is
//! [`Error::NoMemory`].

use core::cell::RefCell;

use crate::engine::{ENGINES, EngineInfo, ring_imr, ring_timestamp};
use crate::generated::i915_reg::{GEN11_GFX_MSTR_IRQ, GEN11_MASTER_IRQ};
use crate::io::{Clock, Mmio, StatusPage};
use crate::ppgtt::{Ppgtt, PtMemory};
use crate::request::{self, Built, Payload};
use crate::submission::{self, ContextPlace, EngineState, HwContext, TIMELINES_PER_PAGE};
use crate::{gt, irq, lrc, mocs, reset, workarounds};

/// How long a running request's seqno may stand still before its engine is
/// reset.
pub const HANG_NS: u64 = 2_000_000_000;
/// How often [`XelpBackend::service`] wants to be called while work is
/// outstanding: the bounded poll for a box whose MSI does not arrive.
pub const POLL_NS: u64 = 1_000_000;
/// VMs at once.
pub const MAX_VMS: usize = 64;
/// Hardware contexts at once on one engine: one timeline slot each in its
/// status page.
pub const MAX_CONTEXTS_PER_ENGINE: usize = TIMELINES_PER_PAGE;
/// The engines this back end drives: RCS0 and BCS0.
pub const MAX_ENGINES: usize = 2;
/// Requests in flight on one context.
pub const MAX_INFLIGHT: usize = 32;
/// Completions waiting for [`XelpBackend::completed`].
pub const MAX_COMPLETIONS: usize = 256;
/// The smallest and largest ring a context may ask for.
pub const RING_BYTES: (u32, u32) = (4 << 10, 2 << 20);

/// Why something was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// No memory, or a table is full (`ENOMEM`).
    NoMemory,
    /// The pin budget is spent (`EDQUOT`).
    Quota,
    /// An argument out of range (`EINVAL`).
    Invalid,
    /// The context was banned after a hang (`EIO`).
    Banned,
    /// The GT could not be brought up; the step that failed.
    Init(&'static str),
}

/// A buffer of the driver's own in the GGTT: pages contiguous in the
/// process's view, bound at `ggtt`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KernelObject {
    /// The platform's name for it.
    pub id: u32,
    /// Pages.
    pub pages: u32,
    /// Its GGTT offset.
    pub ggtt: u32,
}

/// What the driver process supplies: the registers, time, page-table pages,
/// the driver's own GGTT objects, and the client's memory.
pub trait Platform: Mmio + Clock + PtMemory {
    /// One GEM object's memory.
    type Memory;
    /// Memory for an object of `bytes` (a multiple of 4 KiB).
    ///
    /// # Errors
    ///
    /// [`Error::NoMemory`] or [`Error::Quota`].
    fn create_memory(&mut self, bytes: u64) -> Result<Self::Memory, Error>;
    /// The device address of page `index` of `memory`, pinning it into the
    /// device's domain at its first use.
    ///
    /// # Errors
    ///
    /// [`Error::NoMemory`], [`Error::Quota`], or [`Error::Invalid`] past its
    /// end.
    fn memory_page(&mut self, memory: &Self::Memory, index: u64) -> Result<u64, Error>;
    /// `pages` zeroed pages bound into the driver's GGTT window.
    ///
    /// # Errors
    ///
    /// [`Error::NoMemory`].
    fn kernel_object(&mut self, pages: u32) -> Result<KernelObject, Error>;
    /// Give a kernel object back, unbound.
    fn free_kernel_object(&mut self, object: KernelObject);
    /// A kernel object's dwords, to write before the GPU reads them.
    fn words(&mut self, object: KernelObject) -> &mut [u32];
    /// Dword `index` of a kernel object as the GPU left it (volatile).
    fn read_object(&mut self, object: KernelObject, index: usize) -> u32;
    /// Write dword `index` of a kernel object (volatile).
    fn write_object(&mut self, object: KernelObject, index: usize, value: u32);
}

/// An engine as the uAPI names it (`I915_ENGINE_CLASS_*`, instance).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct UapiEngine {
    /// `I915_ENGINE_CLASS_RENDER` 0, `_COPY` 1, `_VIDEO` 2, `_VIDEO_ENHANCE` 3.
    pub class: u16,
    /// The instance.
    pub instance: u16,
}

/// The engines told to programs, in uAPI order.
pub const UAPI_ENGINES: [UapiEngine; MAX_ENGINES] = [
    UapiEngine {
        class: 0,
        instance: 0,
    },
    UapiEngine {
        class: 1,
        instance: 0,
    },
];

/// How a request ended (the front end's `Completion`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Completion {
    /// It ran to its end.
    Ok,
    /// It was running when its engine was reset: the guilty one.
    Hung,
    /// Queued behind the guilty one on the same context, skipped.
    Cancelled,
}

/// Why a submission was not taken (the front end's `SubmitError`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubmitError {
    /// The ring or the in-flight table is full: offer it again after the
    /// next completion.
    Full,
    /// Refused for good.
    Failed(Error),
}

/// A request in flight.
#[derive(Clone, Copy, Debug)]
struct Request {
    id: u64,
    seqno: u32,
    built: Built,
    /// Already handed out (a hang reported it); retired silently.
    reported: bool,
}

/// One hardware context.
#[derive(Clone, Copy, Debug)]
struct Context {
    hw: HwContext,
    vm: usize,
    image: KernelObject,
    ring: KernelObject,
    inflight: [Option<Request>; MAX_INFLIGHT],
    /// The emitted requests not yet submitted to the engine.
    unsubmitted: bool,
    /// The last emitted request's `rq->tail`: what a submission runs to.
    last_tail: u32,
}

impl Context {
    fn count(&self) -> usize {
        self.inflight.iter().flatten().count()
    }

    fn push(&mut self, request: Request) -> bool {
        match self.inflight.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => {
                *slot = Some(request);
                true
            }
            None => false,
        }
    }
}

/// One engine.
struct Engine {
    info: EngineInfo,
    state: EngineState<u16>,
    hwsp: KernelObject,
    default: KernelObject,
    contexts: [Option<Context>; MAX_CONTEXTS_PER_ENGINE],
    /// The context in the ports, if any.
    active: Option<usize>,
    /// Where the round-robin over runnable contexts starts.
    cursor: usize,
    /// The running request's seqno and when it was last seen to move.
    watch: Option<(usize, u32, u64)>,
    /// Engine resets so far.
    resets: u32,
}

/// What the process knows of the device and the machine that the GT does
/// not say: the uAPI's `CHIPSET_ID`, `REVISION`, `GET_APERTURE` and
/// `MEMORY_REGIONS` answers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Identity {
    /// The PCI device id.
    pub device_id: u16,
    /// The PCI revision.
    pub revision: u8,
    /// The GGTT's size in bytes.
    pub ggtt_bytes: u64,
    /// System memory in bytes.
    pub system_memory_bytes: u64,
}

/// The engines the front end is told of, in uAPI order.
static FRONTEND_ENGINES: [crate::frontend::Engine; MAX_ENGINES] = [
    crate::frontend::Engine {
        class: 0,
        instance: 0,
    },
    crate::frontend::Engine {
        class: 1,
        instance: 0,
    },
];

/// The back end.
pub struct XelpBackend<P: Platform> {
    platform: RefCell<P>,
    engines: [Option<Engine>; MAX_ENGINES],
    vms: [Option<Ppgtt>; MAX_VMS],
    done: [Option<(u64, Completion)>; MAX_COMPLETIONS],
    facts: crate::frontend::Facts,
    last_service_ns: u64,
}

impl<P: Platform> core::fmt::Debug for XelpBackend<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("XelpBackend")
            .field("facts", &self.facts)
            .field("engines", &self.engines.iter().flatten().count())
            .finish_non_exhaustive()
    }
}

// The platform, borrowed for one call at a time, as each of the traits the
// translated code takes separately.
struct Regs<'a, P>(&'a RefCell<P>);
struct Time<'a, P>(&'a RefCell<P>);
struct Status<'a, P> {
    platform: &'a RefCell<P>,
    page: KernelObject,
}

impl<P: Platform> Mmio for Regs<'_, P> {
    fn read32(&mut self, offset: u32) -> u32 {
        Mmio::read32(&mut *self.0.borrow_mut(), offset)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        Mmio::write32(&mut *self.0.borrow_mut(), offset, value);
    }
}

impl<P: Platform> Clock for Time<'_, P> {
    fn now_us(&mut self) -> u64 {
        self.0.borrow_mut().now_us()
    }

    fn delay_us(&mut self, us: u32) {
        self.0.borrow_mut().delay_us(us);
    }
}

impl<P: Platform> StatusPage for Status<'_, P> {
    fn read32(&mut self, index: usize) -> u32 {
        Platform::read_object(&mut *self.platform.borrow_mut(), self.page, index)
    }

    fn write32(&mut self, index: usize, value: u32) {
        Platform::write_object(&mut *self.platform.borrow_mut(), self.page, index, value);
    }
}

/// The engine of a uAPI class and instance, among [`ENGINES`].
fn engine_index(engine: UapiEngine) -> Option<usize> {
    UAPI_ENGINES.iter().position(|e| *e == engine)
}

/// How long the bring-up waits for each engine's first context.
const DEFAULT_IMAGE_PATIENCE_US: u64 = 500_000;

impl<P: Platform> XelpBackend<P> {
    /// Bring the GT up as M2 does, under the caller's forcewake of every
    /// domain: the PAT, `intel_gt_init_hw` (the GT workarounds, the MOCS),
    /// the interrupts, then RCS0 and BCS0 (or the first `engines` of them)
    /// sanitized and resumed, each one's default image recorded from a
    /// first context that runs the context workarounds.
    ///
    /// # Errors
    ///
    /// [`Error::Init`] naming the step, or the platform's errors.
    pub fn new(
        platform: P,
        subslice_mask: u8,
        fused_engines: &[EngineInfo],
        engines: usize,
        identity: Identity,
    ) -> Result<Self, Error> {
        let platform = RefCell::new(platform);
        {
            let mut regs = Regs(&platform);
            gt::setup_private_pat(&mut regs);
            let wal = workarounds::gt_init_workarounds(subslice_mask, fused_engines);
            let _report = gt::intel_gt_init_hw(&mut regs, &wal)
                .map_err(|_| Error::Init("the WOPCM is locked"))?;
            for (reg, value) in irq::gen11_gt_irq_reset() {
                regs.write32(reg.offset(), value);
            }
        }
        let sseu = {
            let mut regs = Regs(&platform);
            crate::sseu::gen12_sseu_info_init(crate::sseu::Fuses::read(&mut regs))
        };
        let cs_timestamp_hz = crate::clock::gen11_read_clock_frequency(&mut Regs(&platform));
        let mut backend = XelpBackend {
            platform,
            engines: [None, None],
            vms: [None; MAX_VMS],
            done: [None; MAX_COMPLETIONS],
            facts: crate::frontend::Facts {
                device_id: identity.device_id,
                revision: identity.revision,
                sseu,
                cs_timestamp_hz,
                engines: FRONTEND_ENGINES
                    .get(..engines.min(MAX_ENGINES))
                    .unwrap_or_default(),
                ggtt_bytes: identity.ggtt_bytes,
                ppgtt_bits: 48,
                system_memory_bytes: identity.system_memory_bytes,
                max_pat_index: crate::gtt::MAX_PAT_INDEX as u8,
            },
            last_service_ns: 0,
        };
        for (slot, info) in ENGINES.iter().take(engines.min(MAX_ENGINES)).enumerate() {
            backend.engine_up(slot, *info)?;
        }
        {
            let mut regs = Regs(&backend.platform);
            for (reg, value) in irq::gen11_gt_irq_postinstall() {
                regs.write32(reg.offset(), value);
            }
            regs.write32(GEN11_GFX_MSTR_IRQ.offset(), GEN11_MASTER_IRQ);
        }
        Ok(backend)
    }

    /// The facts the uAPI answers from.
    #[must_use]
    pub const fn facts(&self) -> &crate::frontend::Facts {
        &self.facts
    }

    /// The platform, for the process's own use between calls.
    pub fn platform(&mut self) -> &mut P {
        self.platform.get_mut()
    }

    /// Engine resets so far, per engine.
    #[must_use]
    pub fn resets(&self) -> [u32; MAX_ENGINES] {
        let mut out = [0; MAX_ENGINES];
        for (slot, engine) in out.iter_mut().zip(&self.engines) {
            *slot = engine.as_ref().map_or(0, |e| e.resets);
        }
        out
    }

    fn resume(platform: &RefCell<P>, info: &EngineInfo, hwsp: KernelObject) -> u32 {
        let mut regs = Regs(platform);
        let report = gt::intel_engine_resume(
            &mut regs,
            info.mmio_base,
            &workarounds::engine_workarounds(info, mocs::UC_INDEX),
            &workarounds::whitelist(info),
            hwsp.ggtt,
        );
        regs.write32(
            ring_imr(info.mmio_base).offset(),
            irq::gen8_logical_ring_imr_enabled(),
        );
        report.still_in_error
    }

    /// One engine sanitized, resumed, and its default image recorded.
    fn engine_up(&mut self, slot: usize, info: EngineInfo) -> Result<(), Error> {
        let table = submission::offsets_table(&info);
        let (hwsp, default) = {
            let mut platform = self.platform.borrow_mut();
            let hwsp = platform.kernel_object(1)?;
            let default = platform.kernel_object(lrc::context_size(table) / 4096)?;
            (hwsp, default)
        };
        let state = EngineState::sanitize(
            info,
            &mut Regs(&self.platform),
            &mut Status {
                platform: &self.platform,
                page: hwsp,
            },
        );
        if Self::resume(&self.platform, &info, hwsp) != 0 {
            return Err(Error::Init("an engine resumed still in error"));
        }
        if let Some(engine) = self.engines.get_mut(slot) {
            *engine = Some(Engine {
                info,
                state,
                hwsp,
                default,
                contexts: [None; MAX_CONTEXTS_PER_ENGINE],
                active: None,
                cursor: 0,
                watch: None,
                resets: 0,
            });
        }
        self.record_default(slot)
    }

    /// `__engines_record_defaults` for one engine: a restore-inhibited
    /// context in a VM of its own runs the context workarounds; its saved
    /// image is the engine's default.
    fn record_default(&mut self, slot: usize) -> Result<(), Error> {
        let vm = self.vm_create()?;
        let context = self.make_context(slot, vm, RING_BYTES.0, false)?;
        let info = self.engine(slot)?.info;
        let wal = workarounds::ctx_workarounds(&info, mocs::UC_INDEX);
        let mut payload = [0_u32; 256];
        let n = request::ctx_wa_payload(&mut Regs(&self.platform), &info, &wal, &mut payload)
            .ok_or(Error::NoMemory)?;
        let id = u64::MAX;
        self.emit(
            slot,
            context,
            Payload::Commands(payload.get(..n).unwrap_or_default()),
            id,
        )
        .map_err(|_| Error::Init("no room for the first request"))?;
        let start = self.platform.borrow_mut().now_us();
        loop {
            let now = self.platform.borrow_mut().now_us();
            self.service(now.saturating_mul(1_000), false);
            if self.context(slot, context).is_some_and(|c| c.count() == 0) {
                break;
            }
            if now.saturating_sub(start) > DEFAULT_IMAGE_PATIENCE_US {
                return Err(Error::Init("the first context did not complete"));
            }
            self.platform.borrow_mut().delay_us(100);
        }
        // Its completion is the bring-up's, not a program's.
        for entry in &mut self.done {
            if entry.is_some_and(|(r, _)| r == id) {
                *entry = None;
            }
        }
        let table = submission::offsets_table(&info);
        let image = self.context(slot, context).ok_or(Error::NoMemory)?.image;
        let default = self.engine(slot)?.default;
        let dwords = (lrc::context_size(table) / 4) as usize;
        for i in 0..dwords {
            let value = self.platform.borrow_mut().read_object(image, i);
            self.platform.borrow_mut().write_object(default, i, value);
        }
        self.context_destroy(slot, context);
        self.vm_destroy(vm);
        Ok(())
    }

    fn engine(&self, slot: usize) -> Result<&Engine, Error> {
        self.engines
            .get(slot)
            .and_then(Option::as_ref)
            .ok_or(Error::Invalid)
    }

    fn engine_mut(&mut self, slot: usize) -> Result<&mut Engine, Error> {
        self.engines
            .get_mut(slot)
            .and_then(Option::as_mut)
            .ok_or(Error::Invalid)
    }

    fn context(&self, slot: usize, context: usize) -> Option<&Context> {
        self.engines
            .get(slot)?
            .as_ref()?
            .contexts
            .get(context)?
            .as_ref()
    }

    /// A new 4-level PPGTT; its id.
    ///
    /// # Errors
    ///
    /// [`Error::NoMemory`].
    pub fn vm_create(&mut self) -> Result<usize, Error> {
        let at = self
            .vms
            .iter()
            .position(Option::is_none)
            .ok_or(Error::NoMemory)?;
        let vm = Ppgtt::create(&mut *self.platform.borrow_mut()).map_err(|_| Error::NoMemory)?;
        if let Some(slot) = self.vms.get_mut(at) {
            *slot = Some(vm);
        }
        Ok(at)
    }

    /// Destroy a VM nothing uses any more (one a context still uses is kept).
    pub fn vm_destroy(&mut self, vm: usize) {
        let used = self
            .engines
            .iter()
            .flatten()
            .any(|e| e.contexts.iter().flatten().any(|c| c.vm == vm));
        if used {
            return;
        }
        if let Some(ppgtt) = self.vms.get_mut(vm).and_then(Option::take) {
            ppgtt.destroy(&mut *self.platform.borrow_mut());
        }
    }

    /// Map `bytes` of `memory` at `address` in `vm`, `pat_index` in each
    /// entry.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for a PAT index past the table, an unknown VM or a
    /// range i915 would refuse; [`Error::NoMemory`]; the platform's.
    pub fn bind(
        &mut self,
        vm: usize,
        address: u64,
        memory: &P::Memory,
        bytes: u64,
        pat_index: u8,
    ) -> Result<(), Error> {
        if u32::from(pat_index) > crate::gtt::MAX_PAT_INDEX {
            return Err(Error::Invalid);
        }
        let ppgtt = self.vms.get(vm).copied().flatten().ok_or(Error::Invalid)?;
        let mut platform = self.platform.borrow_mut();
        ppgtt
            .alloc_va_range(&mut *platform, address, bytes)
            .map_err(|_| Error::Invalid)?;
        for page in 0..bytes / 4096 {
            let device = platform.memory_page(memory, page)?;
            ppgtt
                .insert_entry(
                    &mut *platform,
                    device,
                    address + page * 4096,
                    u32::from(pat_index),
                )
                .map_err(|_| Error::Invalid)?;
        }
        Ok(())
    }

    /// Unmap `bytes` at `address` in `vm`.
    pub fn unbind(&mut self, vm: usize, address: u64, bytes: u64) {
        if let Some(ppgtt) = self.vms.get(vm).copied().flatten() {
            let _ = ppgtt.clear_range(&mut *self.platform.borrow_mut(), address, bytes);
        }
    }

    /// A hardware context on the uAPI engine `engine` in `vm`, with a ring of
    /// `ring_bytes`: its (engine, context) id.
    ///
    /// # Errors
    ///
    /// [`Error::Invalid`] for an engine this back end does not run, a ring
    /// size out of range or an unknown VM; [`Error::NoMemory`].
    pub fn context_create(
        &mut self,
        vm: usize,
        engine: UapiEngine,
        ring_bytes: u32,
    ) -> Result<(usize, usize), Error> {
        let slot = engine_index(engine).ok_or(Error::Invalid)?;
        let _ = self.engine(slot)?;
        if self.vms.get(vm).copied().flatten().is_none() {
            return Err(Error::Invalid);
        }
        let context = self.make_context(slot, vm, ring_bytes, true)?;
        Ok((slot, context))
    }

    fn make_context(
        &mut self,
        slot: usize,
        vm: usize,
        ring_bytes: u32,
        from_default: bool,
    ) -> Result<usize, Error> {
        if !ring_bytes.is_power_of_two() || ring_bytes < RING_BYTES.0 || ring_bytes > RING_BYTES.1 {
            return Err(Error::Invalid);
        }
        let pml4 = self
            .vms
            .get(vm)
            .copied()
            .flatten()
            .ok_or(Error::Invalid)?
            .pml4;
        let engine = self.engine(slot)?;
        let (info, hwsp, default) = (engine.info, engine.hwsp, engine.default);
        let index = engine
            .contexts
            .iter()
            .position(Option::is_none)
            .ok_or(Error::NoMemory)?;
        let table = submission::offsets_table(&info);
        let mut platform = self.platform.borrow_mut();
        let image = platform.kernel_object(lrc::alloc_state_size(table).0 / 4096)?;
        let ring = match platform.kernel_object(ring_bytes / 4096) {
            Ok(ring) => ring,
            Err(error) => {
                platform.free_kernel_object(image);
                return Err(error);
            }
        };
        let place = ContextPlace {
            image_ggtt: image.ggtt,
            ring_ggtt: ring.ggtt,
            ring_size: ring_bytes,
            hwsp_ggtt: hwsp.ggtt,
            timeline: index,
            tag: index as u32,
            pml4,
        };
        let made = if from_default {
            // The default copied in a page at a time (both live in the
            // platform's memory), then the image built around it.
            let dwords = (lrc::context_size(table) / 4) as usize;
            let mut page = [0_u32; 1024];
            for p in 0..dwords / 1024 {
                for (i, word) in page.iter_mut().enumerate() {
                    *word = platform.read_object(default, p * 1024 + i);
                }
                if let Some(to) = platform.words(image).get_mut(p * 1024..(p + 1) * 1024) {
                    to.copy_from_slice(&page);
                }
            }
            HwContext::new_from_copied_default(info, place, platform.words(image))
        } else {
            HwContext::new(info, place, platform.words(image), None)
        }
        .map_err(|_| Error::NoMemory);
        let hw = match made {
            Ok(hw) => hw,
            Err(error) => {
                platform.free_kernel_object(ring);
                platform.free_kernel_object(image);
                return Err(error);
            }
        };
        drop(platform);
        let engine = self.engine_mut(slot)?;
        if let Some(entry) = engine.contexts.get_mut(index) {
            *entry = Some(Context {
                hw,
                vm,
                image,
                ring,
                inflight: [None; MAX_INFLIGHT],
                unsubmitted: false,
                last_tail: 0,
            });
        }
        Ok(index)
    }
}

impl<P: Platform> XelpBackend<P> {
    /// Destroy a context that is idle: its image and ring given back.
    pub fn context_destroy(&mut self, slot: usize, context: usize) {
        let Some(engine) = self.engines.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        if engine.active == Some(context) {
            return; // still in the ports; the front end destroys only idle ones
        }
        let Some(gone) = engine.contexts.get_mut(context).and_then(Option::take) else {
            return;
        };
        let mut platform = self.platform.borrow_mut();
        platform.free_kernel_object(gone.ring);
        platform.free_kernel_object(gone.image);
    }

    /// Run the batch at `batch` in the context's VM as request `id`; it
    /// completes later, through [`XelpBackend::completed`].
    ///
    /// # Errors
    ///
    /// [`SubmitError::Full`] to be offered again; [`SubmitError::Failed`]
    /// for a banned or unknown context.
    pub fn submit(
        &mut self,
        slot: usize,
        context: usize,
        batch: u64,
        id: u64,
    ) -> Result<(), SubmitError> {
        let banned = self
            .context(slot, context)
            .ok_or(SubmitError::Failed(Error::Invalid))?
            .hw
            .banned;
        if banned {
            return Err(SubmitError::Failed(Error::Banned));
        }
        self.emit(
            slot,
            context,
            Payload::Batch {
                address: batch,
                flags: 0,
            },
            id,
        )
    }

    /// A request on `context`: written into its ring and queued, then the
    /// engine offered work.
    fn emit(
        &mut self,
        slot: usize,
        context: usize,
        payload: Payload<'_>,
        id: u64,
    ) -> Result<(), SubmitError> {
        let mut platform = self.platform.borrow_mut();
        let engine = self
            .engines
            .get_mut(slot)
            .and_then(Option::as_mut)
            .ok_or(SubmitError::Failed(Error::Invalid))?;
        let ctx = engine
            .contexts
            .get_mut(context)
            .and_then(Option::as_mut)
            .ok_or(SubmitError::Failed(Error::Invalid))?;
        if ctx.count() >= MAX_INFLIGHT {
            return Err(SubmitError::Full);
        }
        let (seqno, built) = ctx
            .hw
            .emit(platform.words(ctx.ring), payload)
            .map_err(|_| SubmitError::Full)?;
        let _ = ctx.push(Request {
            id,
            seqno,
            built,
            reported: false,
        });
        ctx.unsubmitted = true;
        ctx.last_tail = built.tail;
        drop(platform);
        self.dequeue(slot);
        Ok(())
    }

    /// `execlists_dequeue`, one context in the ports at a time: the running
    /// context again if it has more (a lite restore), otherwise, once the
    /// engine is idle, the next runnable context round-robin.
    fn dequeue(&mut self, slot: usize) {
        let Some(engine) = self.engines.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        if !engine.state.can_submit() {
            return;
        }
        let runnable =
            |c: &Option<Context>| c.as_ref().is_some_and(|c| c.unsubmitted && !c.hw.banned);
        let pick = match engine.active {
            Some(active) => engine
                .contexts
                .get(active)
                .filter(|c| runnable(c))
                .map(|_| active),
            None => (0..MAX_CONTEXTS_PER_ENGINE)
                .map(|n| (engine.cursor + n) % MAX_CONTEXTS_PER_ENGINE)
                .find(|&n| engine.contexts.get(n).is_some_and(runnable)),
        };
        let Some(pick) = pick else {
            return;
        };
        let Some(ctx) = engine.contexts.get_mut(pick).and_then(Option::as_mut) else {
            return;
        };
        let tail = ctx.last_tail;
        let mut platform = self.platform.borrow_mut();
        let desc = match lrc::reg_state(platform.words(ctx.image)) {
            Some(regs) => ctx.hw.update_context(regs, tail),
            None => return,
        };
        drop(platform);
        if engine
            .state
            .submit(&mut Regs(&self.platform), desc, pick as u16)
        {
            ctx.unsubmitted = false;
            engine.active = Some(pick);
            engine.cursor = (pick + 1) % MAX_CONTEXTS_PER_ENGINE;
        }
    }

    /// Let the GPU's state machines run: after an interrupt (`interrupt`,
    /// the GT's handler run here; the caller acknowledges the vector) or at
    /// a deadline. Drains each engine's status buffer, retires what its
    /// seqno passed, resets an engine whose running request stalled, and
    /// submits what is waiting.
    pub fn service(&mut self, now_ns: u64, interrupt: bool) {
        self.last_service_ns = now_ns;
        if interrupt {
            let _ = irq::gen11_irq_handler(&mut Regs(&self.platform), &mut Time(&self.platform));
        }
        for slot in 0..MAX_ENGINES {
            if self.engines.get(slot).and_then(Option::as_ref).is_none() {
                continue;
            }
            self.process_csb(slot);
            self.retire(slot);
            self.watch(slot, now_ns);
            self.dequeue(slot);
        }
    }

    fn process_csb(&mut self, slot: usize) {
        let Some(engine) = self.engines.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        let mut left = [None; 12];
        let mut n = 0;
        let report = engine.state.process_csb(
            &mut Regs(&self.platform),
            &mut Status {
                platform: &self.platform,
                page: engine.hwsp,
            },
            &mut Time(&self.platform),
            |port| {
                if let Some(slot) = left.get_mut(n) {
                    *slot = Some(port.context);
                    n += 1;
                }
            },
        );
        for gone in left.iter().flatten() {
            if engine.active == Some(usize::from(*gone))
                && engine.state.execlists.active().is_none()
            {
                engine.active = None;
            }
        }
        if engine.state.execlists.active().is_none() && !engine.state.execlists.is_pending() {
            engine.active = None;
        }
        // ERROR_CSB: i915 resets the engine; the watch below does when the
        // running request then stalls.
        let _ = report;
    }

    /// `i915_request_retire` for every request whose seqno its context's
    /// timeline passed, in order.
    fn retire(&mut self, slot: usize) {
        let Some(engine) = self.engines.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        let hwsp = engine.hwsp;
        for ctx in engine.contexts.iter_mut().flatten() {
            let seen =
                Platform::read_object(&mut *self.platform.borrow_mut(), hwsp, ctx.hw.seqno_dword());
            let mut head = None;
            for entry in &mut ctx.inflight {
                let Some(request) = entry else {
                    continue;
                };
                if !submission::i915_seqno_passed(seen, request.seqno) {
                    continue;
                }
                if !request.reported && !push_done(&mut self.done, request.id, Completion::Ok) {
                    // The queue is full: retired on a later call, after the
                    // front end drained it. Nothing is lost.
                    break;
                }
                // The newest retired request: the ring is free up to its end.
                let age = ctx.hw.seqno.wrapping_sub(request.seqno);
                if head.is_none_or(|(a, _)| age < a) {
                    head = Some((age, request.built.wa_tail));
                }
                *entry = None;
            }
            if let Some((_, head)) = head {
                ctx.hw.retire(head);
            }
        }
    }

    /// The heartbeat's view: the running context's seqno has not moved for
    /// [`HANG_NS`] while it has requests, so its engine is reset.
    fn watch(&mut self, slot: usize, now_ns: u64) {
        let Some(engine) = self.engines.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        let Some(active) = engine.active else {
            engine.watch = None;
            return;
        };
        let Some(ctx) = engine.contexts.get(active).and_then(Option::as_ref) else {
            return;
        };
        if ctx.count() == 0 {
            engine.watch = None;
            return;
        }
        let seen = Platform::read_object(
            &mut *self.platform.borrow_mut(),
            engine.hwsp,
            ctx.hw.seqno_dword(),
        );
        match engine.watch {
            Some((c, s, since)) if c == active && s == seen => {
                if now_ns.saturating_sub(since) >= HANG_NS {
                    engine.watch = None;
                    self.reset(slot, active);
                }
            }
            _ => engine.watch = Some((active, seen, now_ns)),
        }
    }

    /// `__intel_engine_reset_bh` under execlists for a stalled `guilty`
    /// context: the engine reset by its domain alone, the ports and status
    /// buffer reset, the engine resumed; the guilty request
    /// [`Completion::Hung`], the rest of its context
    /// [`Completion::Cancelled`], all skipped, the context banned and
    /// replayed so their breadcrumbs land.
    fn reset(&mut self, slot: usize, guilty: usize) {
        let Some(engine) = self.engines.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        let info = engine.info;
        let _ =
            reset::intel_engine_reset(&mut Regs(&self.platform), &mut Time(&self.platform), &info);
        engine.resets += 1;
        engine.state.execlists.reset(
            &mut Regs(&self.platform),
            &mut Status {
                platform: &self.platform,
                page: engine.hwsp,
            },
            info.mmio_base,
            |_| {},
        );
        engine.active = None;
        let _ = Self::resume(&self.platform, &info, engine.hwsp);
        let Some(ctx) = engine.contexts.get_mut(guilty).and_then(Option::as_mut) else {
            return;
        };
        let mut order: [Option<Request>; MAX_INFLIGHT] = ctx.inflight;
        // Oldest first: the largest age behind the last emitted seqno.
        let last = ctx.hw.seqno;
        order.sort_unstable_by_key(|r| {
            r.map_or(u32::MAX, |r| u32::MAX - 1 - last.wrapping_sub(r.seqno))
        });
        let mut first = None;
        let mut later = [Built {
            head: 0,
            infix: 0,
            postfix: 0,
            tail: 0,
            wa_tail: 0,
        }; MAX_INFLIGHT];
        let mut n = 0;
        for request in order.iter().flatten() {
            let verdict = if first.is_none() {
                Completion::Hung
            } else {
                Completion::Cancelled
            };
            let _ = push_done(&mut self.done, request.id, verdict);
            if first.is_none() {
                first = Some(request.built);
            } else if let Some(slot) = later.get_mut(n) {
                *slot = request.built;
                n += 1;
            }
        }
        for entry in ctx.inflight.iter_mut().flatten() {
            entry.reported = true;
        }
        let Some(hung) = first else {
            return;
        };
        let mut platform = self.platform.borrow_mut();
        let later = later.get(..n).unwrap_or_default();
        for request in core::iter::once(&hung).chain(later) {
            request::skip(platform.words(ctx.ring), ctx.hw.place.ring_size, request);
        }
        let desc = match lrc::reg_state(platform.words(ctx.image)) {
            Some(regs) => reset::rewind(&mut ctx.hw, regs, &hung, later),
            None => return,
        };
        drop(platform);
        if engine
            .state
            .submit(&mut Regs(&self.platform), desc, guilty as u16)
        {
            ctx.unsubmitted = false;
            engine.active = Some(guilty);
        }
    }

    /// When [`XelpBackend::service`] must next be called: [`POLL_NS`] after
    /// the last call while any request is outstanding.
    #[must_use]
    pub fn next_deadline(&self) -> Option<u64> {
        let busy = self
            .engines
            .iter()
            .flatten()
            .any(|e| e.contexts.iter().flatten().any(|c| c.count() > 0));
        busy.then(|| self.last_service_ns.saturating_add(POLL_NS))
    }

    /// Hand each finished request to `sink`, once.
    pub fn completed(&mut self, sink: &mut dyn FnMut(u64, Completion)) {
        for entry in &mut self.done {
            if let Some((id, how)) = entry.take() {
                sink(id, how);
            }
        }
    }

    /// RCS0's `RING_TIMESTAMP`, 64 bits read high, low, high again.
    pub fn timestamp(&mut self) -> u64 {
        let base = crate::engine::RENDER_RING_BASE;
        let low = ring_timestamp(base).offset();
        let mut regs = Regs(&self.platform);
        loop {
            let high = regs.read32(low + 4);
            let value = regs.read32(low);
            if regs.read32(low + 4) == high {
                return (u64::from(high) << 32) | u64::from(value);
            }
        }
    }
}

/// Queue a completion in the first free slot; whether there was one.
fn push_done(
    done: &mut [Option<(u64, Completion)>; MAX_COMPLETIONS],
    id: u64,
    how: Completion,
) -> bool {
    match done.iter_mut().find(|slot| slot.is_none()) {
        Some(slot) => {
            *slot = Some((id, how));
            true
        }
        None => false,
    }
}

/// The errno the uAPI gives for each refusal.
#[must_use]
pub const fn errno(error: Error) -> ferrix_linux_abi::errno::Errno {
    use ferrix_linux_abi::errno::Errno;
    match error {
        Error::NoMemory => Errno::ENOMEM,
        Error::Quota => Errno::EDQUOT,
        Error::Invalid => Errno::EINVAL,
        Error::Banned | Error::Init(_) => Errno::EIO,
    }
}

/// A hardware context's name to the front end: its engine and its slot.
const fn hw_context(slot: usize, context: usize) -> crate::frontend::HwContext {
    crate::frontend::HwContext((slot * MAX_CONTEXTS_PER_ENGINE + context) as u32)
}

const fn split(context: crate::frontend::HwContext) -> (usize, usize) {
    let n = context.0 as usize;
    (n / MAX_CONTEXTS_PER_ENGINE, n % MAX_CONTEXTS_PER_ENGINE)
}

/// The front end's back end, as agreed (`frontend::Backend`): this module's
/// calls under the front end's names and errnos.
impl<P: Platform> crate::frontend::Backend for XelpBackend<P> {
    type Memory = P::Memory;

    fn facts(&self) -> &crate::frontend::Facts {
        XelpBackend::facts(self)
    }

    fn create_memory(&mut self, bytes: u64) -> Result<P::Memory, ferrix_linux_abi::errno::Errno> {
        self.platform.get_mut().create_memory(bytes).map_err(errno)
    }

    fn vm_create(&mut self) -> Result<crate::frontend::VmId, ferrix_linux_abi::errno::Errno> {
        XelpBackend::vm_create(self)
            .map(|vm| crate::frontend::VmId(vm as u32))
            .map_err(errno)
    }

    fn vm_destroy(&mut self, vm: crate::frontend::VmId) {
        XelpBackend::vm_destroy(self, vm.0 as usize);
    }

    fn bind(
        &mut self,
        vm: crate::frontend::VmId,
        address: u64,
        memory: &P::Memory,
        bytes: u64,
        pat_index: u8,
    ) -> Result<(), ferrix_linux_abi::errno::Errno> {
        XelpBackend::bind(self, vm.0 as usize, address, memory, bytes, pat_index).map_err(errno)
    }

    fn unbind(&mut self, vm: crate::frontend::VmId, address: u64, bytes: u64) {
        XelpBackend::unbind(self, vm.0 as usize, address, bytes);
    }

    fn context_create(
        &mut self,
        vm: crate::frontend::VmId,
        engine: crate::frontend::Engine,
        params: crate::frontend::ContextParams,
    ) -> Result<crate::frontend::HwContext, ferrix_linux_abi::errno::Errno> {
        let engine = UapiEngine {
            class: engine.class,
            instance: engine.instance,
        };
        XelpBackend::context_create(self, vm.0 as usize, engine, params.ring_bytes)
            .map(|(slot, context)| hw_context(slot, context))
            .map_err(errno)
    }

    fn context_destroy(&mut self, context: crate::frontend::HwContext) {
        let (slot, context) = split(context);
        XelpBackend::context_destroy(self, slot, context);
    }

    fn submit(
        &mut self,
        context: crate::frontend::HwContext,
        batch: u64,
        _length: u32,
        request: crate::frontend::RequestId,
    ) -> Result<(), crate::frontend::SubmitError> {
        let (slot, context) = split(context);
        XelpBackend::submit(self, slot, context, batch, request).map_err(|error| match error {
            SubmitError::Full => crate::frontend::SubmitError::Full,
            SubmitError::Failed(error) => crate::frontend::SubmitError::Failed(errno(error)),
        })
    }

    fn service(&mut self, now_ns: u64, interrupt: bool) {
        XelpBackend::service(self, now_ns, interrupt);
    }

    fn next_deadline(&self) -> Option<u64> {
        XelpBackend::next_deadline(self)
    }

    fn completed(
        &mut self,
        sink: &mut dyn FnMut(crate::frontend::RequestId, crate::frontend::Completion),
    ) {
        XelpBackend::completed(self, &mut |id, how| {
            sink(
                id,
                match how {
                    Completion::Ok => crate::frontend::Completion::Ok,
                    Completion::Hung => crate::frontend::Completion::Hung,
                    Completion::Cancelled => crate::frontend::Completion::Cancelled,
                },
            );
        });
    }

    fn timestamp(&mut self) -> u64 {
        XelpBackend::timestamp(self)
    }
}
