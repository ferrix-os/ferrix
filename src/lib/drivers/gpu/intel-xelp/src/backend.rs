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
/// outstanding and a program waits on it, or a context waits for the
/// ports: the bounded poll for a box whose MSI does not arrive (the N150,
/// docs/N150.md section 13).
pub const POLL_NS: u64 = 1_000_000;
/// The longest the poll backs off to while work is outstanding but nothing
/// waits on it: no program in a wait, no kernel fence to signal, no request
/// the front end holds for its fences, no context waiting for the ports. A
/// completion is then seen at most this late, which delays only retiring
/// and freeing.
pub const POLL_MAX_NS: u64 = 4_000_000;

/// The poll's next interval after one at `prev`: back to [`POLL_NS`] when
/// the service moved anything (`progressed`: a request retired or a
/// context went into the ports) or a program or context waits (`urgent`),
/// else doubled up to [`POLL_MAX_NS`].
#[must_use]
pub const fn next_poll_ns(prev: u64, progressed: bool, urgent: bool) -> u64 {
    if progressed || urgent {
        POLL_NS
    } else if prev >= POLL_MAX_NS / 2 {
        POLL_MAX_NS
    } else if prev < POLL_NS {
        POLL_NS * 2
    } else {
        prev * 2
    }
}
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
    /// An engine a previous driver left running could not be reset
    /// ([`XelpBackend::new`]): nothing may be brought up, and no HELLO
    /// sent, while it may still run the dead driver's contexts.
    Inherited,
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
    /// The GT's interrupt was serviced ([`XelpBackend::service`] with
    /// `interrupt`): acknowledge the vector, so the next one is delivered.
    fn interrupt_serviced(&mut self) {}
    /// One second of [`XelpBackend::set_freqlog`]'s log.
    fn frequency_report(&mut self, report: &FrequencyReport) {
        let _ = report;
    }
    /// Forcewake taken per access: release the domains not used since the
    /// last call that are due at `now_ns` (`intel_uncore_fw_release_timer`).
    fn forcewake_release(&mut self, now_ns: u64) {
        let _ = now_ns;
    }
    /// When [`Platform::forcewake_release`] must next run, while a domain
    /// is awake.
    fn forcewake_due(&self) -> Option<u64> {
        None
    }
}

/// One line of the per-second frequency log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrequencyReport {
    /// What the GT does now.
    pub sample: crate::power::Sample,
    /// The package's average power over the second, in mW.
    pub milliwatts: u64,
    /// The GT's share of the second in RC6, in tenths of a percent.
    pub rc6_permille: u64,
    /// The busiest engine's share of the second in the ports, in tenths of
    /// a percent.
    pub busy_permille: u64,
    /// Whether RPS is parked, and whether the request is pinned.
    pub parked: bool,
    /// See [`FrequencyReport::parked`].
    pub pinned: bool,
    /// How often RPS parked since the last line.
    pub parks: u32,
    /// How many requests a waiting program boosted since the last line.
    pub boosts: u32,
    /// Requests a program waits on now (`num_waiters`).
    pub waiters: u32,
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
    /// A program waits on it: counted in [`XelpBackend::waiters`] until
    /// it retires (`I915_FENCE_FLAG_BOOST`).
    boost: bool,
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
    /// Time in the ports so far, in ns (i915's engine busy statistics, by
    /// the context going in and out): counted from `seen_ns`, the last
    /// check, when `was_active` was then.
    busy_ns: u64,
    seen_ns: u64,
    was_active: bool,
}

impl Engine {
    /// The time since the last check counted busy if the engine was in
    /// the ports then, and the check moved to `now_ns`.
    fn account(&mut self, now_ns: u64) {
        if self.was_active {
            self.busy_ns = self
                .busy_ns
                .saturating_add(now_ns.saturating_sub(self.seen_ns));
        }
        self.seen_ns = now_ns.max(self.seen_ns);
        self.was_active = self.active.is_some();
    }
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
    /// The engines the fuses leave, for RC6's per-engine idle counts.
    fused: [EngineInfo; 5],
    fused_count: usize,
    /// RPS, once [`XelpBackend::power_up`] ran.
    rps: Option<crate::power::Rps>,
    /// The per-second log: when the next line is due, and the last sample
    /// with its time.
    freqlog: Option<Freqlog>,
    /// Since when nothing has been in flight, while RPS is unparked.
    idle_since: Option<u64>,
    last_service_ns: u64,
    /// The front end holds requests not yet submitted here (waiting on
    /// their fences): work in flight for the park rule.
    held: bool,
    /// Parks since the last frequency log line.
    parks: u32,
    /// `rps->num_waiters`: requests in flight a program waits on; recounted
    /// at each service while not 0.
    waiters: u32,
    /// Requests boosted since the last frequency log line.
    boosts: u32,
    /// The poll's interval now ([`next_poll_ns`]), and whether the current
    /// service retired or submitted anything.
    poll_ns: u64,
    progressed: bool,
    /// A kernel fence is still to be signalled (`Backend::set_watched`).
    watched: bool,
    /// What [`XelpBackend::new`] found of a driver before this one: each
    /// engine that was pointed at a status page and so reset alone before
    /// bring-up, with how its reset went (`docs/INTEL-GPU.md` §11, a driver
    /// started again), in [`ENGINES`]' order.
    inherited: [Option<Inherited>; ENGINES.len()],
}

/// An engine a previous driver had brought up, found at [`XelpBackend::new`]
/// and reset by its domain alone before anything else is written: the dead
/// driver's contexts may still be running on it, reading and writing pages
/// the kernel gives back once this driver's render node is accepted
/// (`object::pin`'s quarantine). Never the whole GT, so the screen the
/// firmware lit stays (`reset`'s module note).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Inherited {
    /// The engine's `RING_HWS_PGA` as found: the status page the previous
    /// driver gave it.
    pub hws_pga: u32,
    /// How the reset went; `None` for an engine with no reset domain here.
    pub report: Option<reset::ResetReport>,
}

/// The per-second log's state: when the next line is due, and the last
/// line's sample, time and busy totals.
#[derive(Clone, Copy, Debug)]
struct Freqlog {
    due: u64,
    sample: crate::power::Sample,
    at: u64,
    busy: [u64; crate::power::POLICY_ENGINES],
}

/// How long nothing may be in flight before RPS parks (`intel_rps_park`,
/// which i915 runs when the GT's last wakeref goes):
/// [`crate::rps::IDLE_HOLD_NS`], one longest evaluation interval past the
/// last retire.
pub const PARK_NS: u64 = crate::rps::IDLE_HOLD_NS;

/// The request id of the driver's own load ([`XelpBackend::load_start`]).
const LOAD_ID: u64 = u64::MAX - 1;

/// The driver's own load on RCS0 for the frequency checkpoint: a spinning
/// batch (`igt_spinner`'s arbitration point and jump back to itself), run
/// privileged from the GGTT in a context and VM of its own, until
/// [`XelpBackend::load_end`] writes `MI_BATCH_BUFFER_END` over its first
/// command.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Load {
    vm: usize,
    context: usize,
    batch: KernelObject,
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
        let inherited = Self::reset_inherited(&platform);
        // Fail closed: an engine this driver can reset that did not reset
        // may still run the dead driver's contexts, whose pages the kernel
        // gives back at this driver's HELLO. So no HELLO: the caller exits,
        // the kernel keeps the quarantine, and devmgr's budget applies.
        if inherited
            .iter()
            .flatten()
            .any(|found| found.report.is_some_and(|report| !report.is_reset()))
        {
            return Err(Error::Inherited);
        }
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
            fused: {
                let mut fused = [ENGINES[0]; 5];
                for (slot, e) in fused.iter_mut().zip(fused_engines) {
                    *slot = *e;
                }
                fused
            },
            fused_count: fused_engines.len().min(5),
            rps: None,
            freqlog: None,
            idle_since: None,
            held: false,
            parks: 0,
            waiters: 0,
            boosts: 0,
            poll_ns: POLL_NS,
            progressed: false,
            watched: false,
            inherited,
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

    /// Each engine a previous driver left pointed at a status page, reset by
    /// its domain alone (`intel_engine_reset`), as i915's
    /// `intel_gt_sanitize` resets what a previous driver left running before
    /// it takes the GT -- but by engine, never `GRDOM_FULL`, so the display
    /// keeps its plane. Every engine of [`ENGINES`] is looked at, not only
    /// those this boot brings up. An engine at a cold boot has
    /// `RING_HWS_PGA` 0 and is not touched, so the first start is as it
    /// was. One with no reset domain here (a video engine) has no report:
    /// this driver never brings one up, so no predecessor of its ran
    /// contexts there.
    fn reset_inherited(platform: &RefCell<P>) -> [Option<Inherited>; ENGINES.len()] {
        let mut inherited = [None; ENGINES.len()];
        for (slot, info) in inherited.iter_mut().zip(ENGINES.iter()) {
            let hws_pga = Regs(platform).read32(gt::hws_pga_offset(info.mmio_base));
            if hws_pga == 0 {
                continue;
            }
            let report = reset::intel_engine_reset(&mut Regs(platform), &mut Time(platform), info);
            *slot = Some(Inherited { hws_pga, report });
        }
        inherited
    }

    /// What [`XelpBackend::new`] found of a previous driver, per engine.
    #[must_use]
    pub const fn inherited(&self) -> &[Option<Inherited>; ENGINES.len()] {
        &self.inherited
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
                busy_ns: 0,
                seen_ns: 0,
                was_active: false,
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
        // No page for a table is i915's `-ENOMEM` from `alloc_pt_dma`, not a
        // bad range: anv retries an execbuffer's ENOMEM and marks the
        // device lost on EINVAL.
        ppgtt
            .alloc_va_range(&mut *platform, address, bytes)
            .map_err(ppgtt_error)?;
        for page in 0..bytes / 4096 {
            let device = platform.memory_page(memory, page)?;
            ppgtt
                .insert_entry(
                    &mut *platform,
                    device,
                    address + page * 4096,
                    u32::from(pat_index),
                )
                .map_err(ppgtt_error)?;
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
            boost: false,
        });
        ctx.unsubmitted = true;
        ctx.last_tail = built.tail;
        let now = platform.now_us().saturating_mul(1_000);
        drop(platform);
        self.poll_ns = POLL_NS;
        self.unpark(now);
        self.dequeue(slot, now);
        Ok(())
    }

    /// `execlists_dequeue`, one context in the ports at a time: the running
    /// context again if it has more (a lite restore), otherwise, once the
    /// engine is idle, the next runnable context round-robin.
    fn dequeue(&mut self, slot: usize, now_ns: u64) {
        let Some(engine) = self.engines.get_mut(slot).and_then(Option::as_mut) else {
            return;
        };
        engine.account(now_ns);
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
            self.progressed = true;
            engine.active = Some(pick);
            engine.cursor = (pick + 1) % MAX_CONTEXTS_PER_ENGINE;
        }
        engine.account(now_ns);
    }

    /// Let the GPU's state machines run: after an interrupt (`interrupt`,
    /// the GT's handler run here; the caller acknowledges the vector) or at
    /// a deadline. Drains each engine's status buffer, retires what its
    /// seqno passed, resets an engine whose running request stalled, and
    /// submits what is waiting.
    pub fn service(&mut self, now_ns: u64, interrupt: bool) {
        self.last_service_ns = now_ns;
        self.progressed = interrupt;
        self.freqlog_due(now_ns);
        if interrupt {
            let _ = irq::gen11_irq_handler(&mut Regs(&self.platform), &mut Time(&self.platform));
            // Reading acknowledges the GT; the vector after, or a level
            // interrupt would fire again at once (as gc400 does).
            self.platform.borrow_mut().interrupt_serviced();
        }
        for slot in 0..MAX_ENGINES {
            if self.engines.get(slot).and_then(Option::as_ref).is_none() {
                continue;
            }
            if let Some(engine) = self.engines.get_mut(slot).and_then(Option::as_mut) {
                engine.account(now_ns);
            }
            self.process_csb(slot);
            self.retire(slot);
            self.watch(slot, now_ns);
            self.dequeue(slot, now_ns);
        }
        self.power_service(now_ns);
        let urgent = self.waiters > 0 || self.held || self.watched || self.ports_wanted();
        self.poll_ns = next_poll_ns(self.poll_ns, self.progressed, urgent);
        self.platform.borrow_mut().forcewake_release(now_ns);
    }

    /// A context has requests not yet in the ports.
    fn ports_wanted(&self) -> bool {
        self.engines
            .iter()
            .flatten()
            .any(|e| e.contexts.iter().flatten().any(|c| c.unsubmitted))
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
                self.progressed = true;
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

    /// When [`XelpBackend::service`] must next be called: the poll's
    /// interval ([`next_poll_ns`]) after the last call while any request is
    /// outstanding.
    #[must_use]
    pub fn next_deadline(&self) -> Option<u64> {
        let busy = self
            .engines
            .iter()
            .flatten()
            .any(|e| e.contexts.iter().flatten().any(|c| c.count() > 0));
        let poll = busy.then(|| self.last_service_ns.saturating_add(self.poll_ns));
        let log = self.freqlog.map(|log| log.due);
        let policy = self.rps.and_then(|rps| rps.due());
        let park = self
            .rps
            .filter(|rps| rps.active && !rps.pinned && !self.held)
            .and(self.idle_since)
            .map(|since| since.saturating_add(PARK_NS));
        let forcewake = self.platform.borrow().forcewake_due();
        [poll, log, policy, park, forcewake]
            .into_iter()
            .flatten()
            .min()
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

/// A PPGTT failure as the back end's error.
const fn ppgtt_error(error: crate::ppgtt::PpgttError) -> Error {
    match error {
        crate::ppgtt::PpgttError::NoMemory => Error::NoMemory,
        _ => Error::Invalid,
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
        Error::Banned | Error::Init(_) | Error::Inherited => Errno::EIO,
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

    fn set_held(&mut self, held: bool) {
        XelpBackend::set_held(self, held);
    }

    fn set_watched(&mut self, watched: bool) {
        self.watched = watched;
    }

    fn boost(&mut self, request: crate::frontend::RequestId) {
        XelpBackend::boost(self, request);
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

/// How often the frequency log says a line.
pub const FREQLOG_NS: u64 = 1_000_000_000;

impl<P: Platform> XelpBackend<P> {
    /// `intel_rps_enable` and `intel_rc6_enable`: RPS on with its first
    /// request at RP1, or pinned at `start_mhz` (a fixed request, for
    /// measurement); RC6 programmed and enabled when `rc6`. Returns the RPS
    /// state.
    pub fn power_up(&mut self, start_mhz: Option<u32>, rc6: bool) -> crate::power::Rps {
        let clock_hz = self.facts.cs_timestamp_hz;
        let now = self.platform.borrow_mut().now_us().saturating_mul(1_000);
        let mut regs = Regs(&self.platform);
        let rps = crate::power::Rps::enable(&mut regs, clock_hz, start_mhz, now);
        if rc6 {
            let _ = crate::power::rc6_enable(
                &mut regs,
                self.fused.get(..self.fused_count).unwrap_or_default(),
            );
        }
        self.rps = Some(rps);
        self.idle_since = self.is_idle().then_some(now);
        rps
    }

    /// `intel_llc_enable`: the PCU's minimum ring frequency for each GT
    /// frequency (`gen6_update_ring_freq`); how many rows were refused and
    /// the first refusal. Nothing before [`XelpBackend::power_up`].
    pub fn update_ring_freq(&mut self) -> (u32, Option<crate::pcode::PcodeError>) {
        let Some(rps) = self.rps else {
            return (0, None);
        };
        crate::pcode::gen6_update_ring_freq(
            &mut Regs(&self.platform),
            &mut Time(&self.platform),
            rps.caps,
        )
    }

    /// The ring frequency table as the PCU answers it, `RPn` up, into `rows`;
    /// how many rows were read. Stops at the first refusal.
    pub fn ring_freq_table(&mut self, rows: &mut [crate::pcode::RingRow]) -> usize {
        let Some(rps) = self.rps else {
            return 0;
        };
        let (min, max) = crate::pcode::ring_table_rows(rps.caps);
        let mut n = 0;
        for (row, gpu_freq) in rows.iter_mut().zip(min..=max) {
            match crate::pcode::read_ring_row(
                &mut Regs(&self.platform),
                &mut Time(&self.platform),
                gpu_freq,
            ) {
                Ok(read) => *row = read,
                Err(_) => break,
            }
            n += 1;
        }
        n
    }

    /// What may hold the GT below its request, read now.
    pub fn limits(&mut self) -> crate::power::Limits {
        crate::power::Limits::read(&mut Regs(&self.platform))
    }

    /// The RPS state, once [`XelpBackend::power_up`] ran.
    #[must_use]
    pub const fn rps(&self) -> Option<crate::power::Rps> {
        self.rps
    }

    /// Request `mhz` (rounded, clamped to RPn..RP0), pinned there when
    /// `pin`, else from where the policy goes on; the MHz asked, or `None`
    /// before [`XelpBackend::power_up`]. A parked GT is unparked first.
    pub fn set_frequency_mhz(&mut self, mhz: u32, pin: bool) -> Option<u32> {
        let mut rps = self.rps?;
        let now = self.platform.borrow_mut().now_us().saturating_mul(1_000);
        let busy = self.busy_totals(now);
        let mut regs = Regs(&self.platform);
        rps.unpark(&mut regs, now, &busy);
        let asked = rps.set_mhz(&mut regs, mhz, pin);
        self.rps = Some(rps);
        Some(asked)
    }

    /// What the GT does now; `None` before [`XelpBackend::power_up`].
    pub fn frequency_sample(&mut self) -> Option<crate::power::Sample> {
        let rps = self.rps?;
        Some(rps.sample(&mut Regs(&self.platform)))
    }

    /// Each engine's time in the ports so far, at `now_ns`.
    #[must_use]
    pub fn busy_totals(&self, now_ns: u64) -> [u64; crate::power::POLICY_ENGINES] {
        let mut busy = [0; crate::power::POLICY_ENGINES];
        for (total, engine) in busy.iter_mut().zip(&self.engines) {
            if let Some(engine) = engine {
                *total = engine.busy_ns.saturating_add(if engine.was_active {
                    now_ns.saturating_sub(engine.seen_ns)
                } else {
                    0
                });
            }
        }
        busy
    }

    /// No engine has a request outstanding.
    fn is_idle(&self) -> bool {
        !self
            .engines
            .iter()
            .flatten()
            .any(|e| e.active.is_some() || e.contexts.iter().flatten().any(|c| c.count() > 0))
    }

    /// `intel_rps_unpark` when work arrives on a parked GT.
    fn unpark(&mut self, now_ns: u64) {
        self.idle_since = None;
        let Some(mut rps) = self.rps.filter(|rps| !rps.active) else {
            return;
        };
        let busy = self.busy_totals(now_ns);
        rps.unpark(&mut Regs(&self.platform), now_ns, &busy);
        self.rps = Some(rps);
    }

    /// The front end holds requests it has not submitted yet (`held`),
    /// waiting on their fences: RPS does not park while it does.
    pub fn set_held(&mut self, held: bool) {
        self.held = held;
    }

    /// `intel_rps_boost` for request `id`: marked, counted as a waiter until
    /// it retires, and the GT's request raised to RP0 now if it is lower.
    /// Once per request; an id not in flight here is ignored.
    pub fn boost(&mut self, id: u64) {
        let found = self
            .engines
            .iter_mut()
            .flatten()
            .flat_map(|engine| engine.contexts.iter_mut().flatten())
            .flat_map(|ctx| ctx.inflight.iter_mut().flatten())
            .find(|request| request.id == id);
        let Some(request) = found else {
            return;
        };
        if request.boost {
            return;
        }
        request.boost = true;
        self.poll_ns = POLL_NS;
        self.waiters = self.waiters.saturating_add(1);
        self.boosts = self.boosts.saturating_add(1);
        if let Some(mut rps) = self.rps {
            let _ = rps.boost(&mut Regs(&self.platform));
            self.rps = Some(rps);
        }
    }

    /// Requests in flight a program waits on, as of the last service.
    #[must_use]
    pub const fn waiters(&self) -> u32 {
        self.waiters
    }

    /// The boosted requests still in flight.
    fn count_waiters(&self) -> u32 {
        let count = self
            .engines
            .iter()
            .flatten()
            .flat_map(|engine| engine.contexts.iter().flatten())
            .flat_map(|ctx| ctx.inflight.iter().flatten())
            .filter(|request| request.boost)
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    /// The policy's part of [`XelpBackend::service`]: the frequency log,
    /// the park rule ([`crate::rps::park_decision`]: never while anything
    /// is in flight, after [`PARK_NS`] with nothing), and `rps_timer` when
    /// due.
    fn power_service(&mut self, now_ns: u64) {
        use crate::rps::{Park, park_decision};
        self.freqlog_due(now_ns);
        let Some(mut rps) = self.rps else {
            return;
        };
        let in_flight = self.held || !self.is_idle();
        let idle_ns = if in_flight {
            self.idle_since = None;
            0
        } else {
            now_ns.saturating_sub(*self.idle_since.get_or_insert(now_ns))
        };
        match park_decision(in_flight, rps.active, idle_ns) {
            Park::Unpark => {
                let busy = self.busy_totals(now_ns);
                rps.unpark(&mut Regs(&self.platform), now_ns, &busy);
            }
            Park::Park if !rps.pinned => {
                rps.park(&mut Regs(&self.platform));
                self.parks = self.parks.saturating_add(1);
            }
            Park::Park | Park::Hold => {}
        }
        if self.waiters > 0 {
            self.waiters = self.count_waiters();
        }
        let client_boost = self.waiters > 0;
        if client_boost {
            // A waiter that came while parked (i915: the work runs at the
            // unpark's first rps_work).
            let _ = rps.boost(&mut Regs(&self.platform));
        }
        let busy = self.busy_totals(now_ns);
        let _ = rps.evaluate(&mut Regs(&self.platform), now_ns, &busy, client_boost);
        self.rps = Some(rps);
    }

    /// Say the frequency, RC6 share, busy share and package power every
    /// second, through [`Platform::frequency_report`], from the next
    /// [`XelpBackend::service`] on; `false` stops it.
    pub fn set_freqlog(&mut self, on: bool, now_ns: u64) {
        self.freqlog = match (on, self.frequency_sample()) {
            (true, Some(sample)) => Some(Freqlog {
                due: now_ns.saturating_add(FREQLOG_NS),
                sample,
                at: now_ns,
                busy: self.busy_totals(now_ns),
            }),
            _ => None,
        };
    }

    fn freqlog_due(&mut self, now_ns: u64) {
        let Some(log) = self.freqlog else {
            return;
        };
        if now_ns < log.due {
            return;
        }
        let (Some(after), Some(rps)) = (self.frequency_sample(), self.rps) else {
            return;
        };
        let elapsed = now_ns.saturating_sub(log.at);
        let (milliwatts, rc6_permille) = crate::power::between(&log.sample, &after, elapsed);
        let busy = self.busy_totals(now_ns);
        let busiest = busy
            .iter()
            .zip(&log.busy)
            .map(|(now, then)| now.saturating_sub(*then))
            .max()
            .unwrap_or(0);
        let busy_permille = (busiest * 1000).checked_div(elapsed).unwrap_or(0).min(1000);
        self.platform
            .borrow_mut()
            .frequency_report(&FrequencyReport {
                sample: after,
                milliwatts,
                rc6_permille,
                busy_permille,
                parked: !rps.active,
                pinned: rps.pinned,
                parks: self.parks,
                boosts: self.boosts,
                waiters: self.waiters,
            });
        self.parks = 0;
        self.boosts = 0;
        self.freqlog = Some(Freqlog {
            due: now_ns.saturating_add(FREQLOG_NS),
            sample: after,
            at: now_ns,
            busy,
        });
    }

    /// Start the driver's own [`Load`] on RCS0.
    ///
    /// # Errors
    ///
    /// [`Error::NoMemory`], or [`Error::Invalid`] without RCS0.
    pub fn load_start(&mut self) -> Result<Load, Error> {
        let vm = self.vm_create()?;
        let context = match self.make_context(0, vm, RING_BYTES.0, true) {
            Ok(context) => context,
            Err(error) => {
                self.vm_destroy(vm);
                return Err(error);
            }
        };
        let made = self.platform.borrow_mut().kernel_object(1);
        let batch = match made {
            Ok(batch) => batch,
            Err(error) => {
                self.context_destroy(0, context);
                self.vm_destroy(vm);
                return Err(error);
            }
        };
        {
            let mut platform = self.platform.borrow_mut();
            let mut cs = crate::commands::Cs::new(platform.words(batch));
            cs.push(crate::commands::gpu::MI_ARB_CHECK);
            cs.push(crate::commands::gpu::MI_BATCH_BUFFER_START_GEN8);
            cs.push(batch.ggtt);
            cs.push(0);
        }
        let load = Load { vm, context, batch };
        let submitted = self.emit(
            0,
            context,
            Payload::Batch {
                address: u64::from(batch.ggtt),
                flags: crate::engine_cs::I915_DISPATCH_SECURE,
            },
            LOAD_ID,
        );
        if submitted.is_err() {
            self.load_free(load);
            return Err(Error::NoMemory);
        }
        Ok(load)
    }

    /// End `load`: `MI_BATCH_BUFFER_END` over its first command, then
    /// serviced until it completes, at most `patience_us`, and its context,
    /// VM and batch given back. Whether it completed; if not, the hang
    /// watch resets it within [`HANG_NS`], and it stays allocated.
    pub fn load_end(&mut self, load: Load, patience_us: u64) -> bool {
        self.platform
            .borrow_mut()
            .write_object(load.batch, 0, lrc::MI_BATCH_BUFFER_END);
        let start = self.platform.borrow_mut().now_us();
        let done = loop {
            let now = self.platform.borrow_mut().now_us();
            self.service(now.saturating_mul(1_000), false);
            if self.context(0, load.context).is_none_or(|c| c.count() == 0) {
                break true;
            }
            if now.saturating_sub(start) > patience_us {
                break false;
            }
            self.platform.borrow_mut().delay_us(100);
        };
        if done {
            self.load_free(load);
        }
        done
    }

    fn load_free(&mut self, load: Load) {
        for entry in &mut self.done {
            if entry.is_some_and(|(r, _)| r == LOAD_ID) {
                *entry = None;
            }
        }
        self.context_destroy(0, load.context);
        self.platform.borrow_mut().free_kernel_object(load.batch);
        self.vm_destroy(load.vm);
    }
}

#[cfg(test)]
impl<P: Platform> XelpBackend<P> {
    /// The device address `address` reaches in VM `vm`: what the GPU's walk
    /// finds, for a test comparing two back ends' views.
    pub(crate) fn translate(&mut self, vm: usize, address: u64) -> Option<u64> {
        let ppgtt = self.vms.get(vm).copied().flatten()?;
        ppgtt.translate(self.platform.get_mut(), address)
    }
}
