//! M2, "a GPU command executes" (`docs/INTEL-GPU.md` §7), after M1's
//! lines and under M1's forcewake:
//!
//! 1. One pool of pages, pinned into the device's domain and mapped
//!    write-back (an LLC part: the GPU snoops), for every object below.
//! 2. The PAT, the GT workarounds and the MOCS written and read back.
//! 3. RCS0 and BCS0 sanitized and resumed (`execlists_resume`): status
//!    page, engine workarounds and whitelist read back, interrupts
//!    unmasked; the GT's interrupt banks enabled and the MSI bound.
//! 4. One 4-level PPGTT for every context.
//! 5. On each engine a first context, restore-inhibited, runs the context
//!    workarounds; its saved image becomes the engine's default.
//! 6. On contexts made from the defaults: the context workarounds read
//!    back by `MI_STORE_REGISTER_MEM`; `MI_STORE_DWORD_IMM` into a pinned
//!    page; `XY_FAST_COPY_BLT` on BCS0 between PPGTT pages; a batch run
//!    from the PPGTT.
//!
//! Every request's completion is waited for on the interrupt's port with a
//! bounded poll (MSI has never arrived on the box, `docs/N150.md` §13), and
//! counted by which of the two saw it first. Every object the driver binds
//! is in its own GGTT window above the firmware's entries; no entry M1
//! counted is written.

use core::ptr;

use ferrix_intel_xelp::commands::{Cs, emit_fast_copy, emit_store_dword, gpu};
use ferrix_intel_xelp::engine::{ENGINES, EngineInfo, ring_imr};
use ferrix_intel_xelp::generated::i915_reg::{GEN11_GFX_MSTR_IRQ, GEN11_MASTER_IRQ};
use ferrix_intel_xelp::ggtt_space::{GgttSpace, Range};
use ferrix_intel_xelp::io::{Clock, GgttPtes, Mmio, StatusPage};
use ferrix_intel_xelp::lrc;
use ferrix_intel_xelp::ppgtt::{Ppgtt, PtMemory};
use ferrix_intel_xelp::request::{self, Payload};
use ferrix_intel_xelp::submission::{self, ContextPlace, EngineState, HwContext};
use ferrix_intel_xelp::workarounds;
use ferrix_intel_xelp::{gt, irq, mocs};
use ferrix_native_abi::types::PACKET_INTERRUPT;
use ferrix_rt::Kernel;
use ferrix_rt::native::device::{Device, Interrupt};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::Deadline;
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::{Pin, PinAccess, device_address};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::{self, Vmo};

use crate::{Bar0, Step, Time, say};

/// Pages in the pool: four context images and their rings, two default
/// images, two status pages, the test pages and the page tables, with room.
const POOL_PAGES: usize = 256;
/// Bytes in a page.
const PAGE: usize = 4096;
/// Dwords in a page.
const PAGE_DWORDS: usize = PAGE / 4;
/// Every context's ring: i915's default for a user context.
const RING_BYTES: u32 = 16 << 10;
/// The lowest GGTT offset the driver uses: past BAR 2's 256 MiB aperture,
/// whatever the firmware left below it.
const GGTT_WINDOW_FLOOR: u64 = 256 << 20;
/// How often the status page is looked at while no interrupt comes.
const POLL_US: u64 = 1_000;
/// How long a request may take before it counts as hung.
const REQUEST_PATIENCE_US: u64 = 500_000;
/// The port key of the GPU's interrupt.
const KEY_INTERRUPT: u64 = 1;

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// One run of pages in the pool, and where it is in the GGTT if bound.
#[derive(Clone, Copy, Debug)]
struct Obj {
    first: usize,
    pages: usize,
    ggtt: u32,
}

/// Pages pinned into the device's domain, mapped write-back.
struct Pool {
    _pin: Pin<Kernel>,
    _vmo: Vmo<Kernel>,
    base: usize,
    device: [u64; POOL_PAGES],
    used: [bool; POOL_PAGES],
}

impl Pool {
    fn new(device: &Device<Kernel>) -> Result<Pool, Step> {
        let bytes = POOL_PAGES * PAGE;
        let vmo = vmo::create(Kernel, bytes).map_err(|_| Step::Memory)?;
        let pin = device
            .pin(&vmo, 0, bytes, PinAccess::ReadWrite)
            .map_err(|_| Step::Memory)?;
        let mut raw = [[0_u8; 8]; POOL_PAGES];
        let got = pin.addresses(&mut raw).map_err(|_| Step::Memory)?;
        if !got.is_complete() || got.pages != POOL_PAGES {
            return Err(Step::Memory);
        }
        let mut addresses = [0_u64; POOL_PAGES];
        for (slot, bytes) in addresses.iter_mut().zip(raw) {
            *slot = device_address(bytes);
        }
        let base = vmo
            .map(None, bytes, Protection::ReadWrite, 0)
            .map_err(|_| Step::Memory)?;
        Ok(Pool {
            _pin: pin,
            _vmo: vmo,
            base,
            device: addresses,
            used: [false; POOL_PAGES],
        })
    }

    /// `pages` free pages in a row, first fit.
    fn take(&mut self, pages: usize) -> Option<usize> {
        let mut run = 0;
        for index in 0..POOL_PAGES {
            run = if self.used.get(index).copied().unwrap_or(true) {
                0
            } else {
                run + 1
            };
            if run == pages {
                let first = index + 1 - pages;
                for slot in self.used.iter_mut().skip(first).take(pages) {
                    *slot = true;
                }
                return Some(first);
            }
        }
        None
    }

    fn give(&mut self, first: usize, pages: usize) {
        for slot in self.used.iter_mut().skip(first).take(pages) {
            *slot = false;
        }
    }

    fn index_of(&self, device: u64) -> Option<usize> {
        self.device.iter().position(|&page| page == device & !0xfff)
    }

    /// The address of dword `index` of page `page`.
    fn at(&self, page: usize, index: usize) -> Option<usize> {
        (page < POOL_PAGES && index < PAGE_DWORDS).then(|| self.base + page * PAGE + index * 4)
    }

    /// An object's dwords. Borrowing the pool mutably keeps every other
    /// object's slice out of reach while this one is held; what the GPU
    /// writes into an object is read through [`Pool::read32`], not a slice.
    fn dwords(&mut self, obj: Obj) -> &mut [u32] {
        let len = obj.pages.min(POOL_PAGES.saturating_sub(obj.first)) * PAGE_DWORDS;
        let start = self.base + obj.first.min(POOL_PAGES) * PAGE;
        // SAFETY: `start..start + len * 4` lies inside the pool's mapping,
        // which this process made read-write and `self` keeps (`_vmo`); the
        // `&mut self` borrow makes it the only slice of the pool alive.
        unsafe { core::slice::from_raw_parts_mut(start as *mut u32, len) }
    }

    /// Two different objects at once: `a` to write, `b` to read.
    fn two(&mut self, a: Obj, b: Obj) -> Option<(&mut [u32], &[u32])> {
        let overlap = a.first < b.first + b.pages && b.first < a.first + a.pages;
        if overlap || a.first + a.pages > POOL_PAGES || b.first + b.pages > POOL_PAGES {
            return None;
        }
        let at = |obj: Obj| self.base + obj.first * PAGE;
        let (a_start, b_start) = (at(a), at(b));
        // SAFETY: both ranges lie inside the pool's mapping (checked above)
        // and do not overlap, and `&mut self` keeps every other slice of
        // the pool out of reach while they live.
        let a_words =
            unsafe { core::slice::from_raw_parts_mut(a_start as *mut u32, a.pages * PAGE_DWORDS) };
        // SAFETY: as above; read only.
        let b_words =
            unsafe { core::slice::from_raw_parts(b_start as *const u32, b.pages * PAGE_DWORDS) };
        Some((a_words, b_words))
    }

    /// Dword `index` of `obj`, as the GPU left it.
    fn read32(&self, obj: Obj, index: usize) -> u32 {
        let Some(address) = self.at(obj.first + index / PAGE_DWORDS, index % PAGE_DWORDS) else {
            return 0;
        };
        // SAFETY: inside the pool's mapping, aligned; volatile, since the
        // GPU writes it.
        unsafe { ptr::read_volatile(address as *const u32) }
    }

    fn write32(&mut self, obj: Obj, index: usize, value: u32) {
        if let Some(address) = self.at(obj.first + index / PAGE_DWORDS, index % PAGE_DWORDS) {
            // SAFETY: as for `read32`.
            unsafe { ptr::write_volatile(address as *mut u32, value) }
        }
    }

    fn device_pages(&self, obj: Obj) -> &[u64] {
        self.device
            .get(obj.first..obj.first + obj.pages)
            .unwrap_or_default()
    }
}

/// The page tables live in the pool, a page each.
impl PtMemory for Pool {
    fn alloc_pt(&mut self) -> Option<u64> {
        let first = self.take(1)?;
        self.device.get(first).copied()
    }

    fn free_pt(&mut self, table: u64) {
        if let Some(index) = self.index_of(table) {
            self.give(index, 1);
        }
    }

    fn read_entry(&mut self, table: u64, index: usize) -> u64 {
        let Some(page) = self.index_of(table) else {
            return 0;
        };
        let obj = Obj {
            first: page,
            pages: 1,
            ggtt: 0,
        };
        u64::from(self.read32(obj, 2 * index)) | (u64::from(self.read32(obj, 2 * index + 1)) << 32)
    }

    fn write_entry(&mut self, table: u64, index: usize, value: u64) {
        let Some(page) = self.index_of(table) else {
            return;
        };
        if let Some(address) = self.at(page, 2 * index) {
            // SAFETY: inside the pool's mapping, eight-byte aligned (an
            // even dword of a page); one store, so the GPU's walk never
            // sees half an entry. The GPU snoops the LLC on this part, so
            // no clflush is needed (i915's `drm_clflush_virt_range` is a
            // no-op on LLC for coherent PT objects).
            unsafe { ptr::write_volatile(address as *mut u64, value) }
        }
    }
}

/// An engine's status page in the pool, as the execlists code reads it.
struct Hwsp<'a> {
    pool: &'a mut Pool,
    page: Obj,
}

impl StatusPage for Hwsp<'_> {
    fn read32(&mut self, index: usize) -> u32 {
        self.pool.read32(self.page, index)
    }

    fn write32(&mut self, index: usize, value: u32) {
        self.pool.write32(self.page, index, value);
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// How a request's completion was first seen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Seen {
    /// After an interrupt packet.
    Interrupt,
    /// On a poll at the deadline.
    Poll,
    /// Not within [`REQUEST_PATIENCE_US`].
    Hung,
}

/// One engine as M2 drives it.
struct Engine {
    info: EngineInfo,
    state: EngineState<u32>,
    hwsp: Obj,
    default: Option<Obj>,
}

/// Everything M2 holds.
struct M2<'a> {
    bar: &'a mut Bar0,
    time: &'a mut Time,
    pool: Pool,
    ggtt: GgttSpace,
    vm: Ppgtt,
    port: Port<Kernel>,
    interrupt: Option<Interrupt<Kernel>>,
    /// Completions first seen after an interrupt, and by a poll.
    by_interrupt: u32,
    by_poll: u32,
    /// Interrupt packets, and passes of the handler that found GT sources.
    packets: u32,
    gt_sources: u32,
}

/// Where M1 left the GGTT: how many entries it read and the last present.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Firmware {
    /// Entries read from the start.
    pub(crate) scanned: u64,
    /// The last present entry's index, if any.
    pub(crate) last_present: Option<u64>,
    /// The GGTT's size in bytes.
    pub(crate) total: u64,
    /// The fused-on dual-subslices.
    pub(crate) subslice_mask: u8,
    /// The engines the media fuses leave, as `engine::bit` flags.
    pub(crate) engine_mask: u32,
}

/// Run M2. Returns how far it got; each step says its line.
pub(crate) fn run(
    bar: &mut Bar0,
    device: &Device<Kernel>,
    time: &mut Time,
    firmware: Firmware,
) -> Result<(), Step> {
    let started = time.now_us();
    let pool = Pool::new(device).inspect_err(|_| {
        say(format_args!(
            "intel: M2: {POOL_PAGES} pages could not be pinned and mapped"
        ));
    })?;
    let window_start = (firmware
        .scanned
        .max(firmware.last_present.map_or(0, |l| l + 1))
        * 4096)
        .max(GGTT_WINDOW_FLOOR);
    let ggtt = GgttSpace::new(window_start, firmware.total);
    say(format_args!(
        "intel: M2: {POOL_PAGES} pages pinned; GGTT window {window_start:#x}..{:#x} (M1 read {} entries, last present {:?})",
        firmware.total, firmware.scanned, firmware.last_present
    ));
    let port = port::create(Kernel).map_err(|_| Step::Port)?;
    let mut pool = pool;
    let vm = Ppgtt::create(&mut pool).map_err(|_| Step::Memory)?;
    let mut m2 = M2 {
        bar,
        time,
        pool,
        ggtt,
        vm,
        port,
        interrupt: None,
        by_interrupt: 0,
        by_poll: 0,
        packets: 0,
        gt_sources: 0,
    };
    m2.gt_init(firmware);
    m2.interrupts(device);
    let mut rcs = m2.engine(ENGINES[0])?;
    let mut bcs = m2.engine(ENGINES[1])?;
    m2.irq_on();
    say(format_args!(
        "intel: M2: PPGTT PML4 at {:#x}, scratch page {:#x}",
        m2.vm.pml4, m2.vm.scratch[0]
    ));
    for engine in [&mut rcs, &mut bcs] {
        m2.record_default(engine)?;
    }
    m2.verify_ctx_workarounds(&mut rcs)?;
    m2.store_dword(&mut rcs)?;
    m2.fast_copy(&mut bcs)?;
    m2.ppgtt_batch(&mut rcs)?;
    say(format_args!(
        "intel: M2: a GPU command executes: completions {} by interrupt, {} by the {} us poll; {} interrupt packets, {} with GT sources ({} ms)",
        m2.by_interrupt,
        m2.by_poll,
        POLL_US,
        m2.packets,
        m2.gt_sources,
        m2.time.now_us().saturating_sub(started) / 1_000
    ));
    Ok(())
}

impl M2<'_> {
    /// `i915_gem_init`'s PAT, then `intel_gt_init_hw`: the GT workarounds
    /// and the MOCS, each read back.
    fn gt_init(&mut self, firmware: Firmware) {
        gt::setup_private_pat(self.bar);
        let pat_lost = gt::verify_private_pat(self.bar).count();
        let mut engines = [ENGINES[0]; 5];
        let mut count = 0;
        for engine in ENGINES.iter().filter(|e| {
            firmware.engine_mask & ferrix_intel_xelp::device_info::engine::bit(e.id) != 0
        }) {
            if let Some(slot) = engines.get_mut(count) {
                *slot = *engine;
                count += 1;
            }
        }
        let wal = workarounds::gt_init_workarounds(
            firmware.subslice_mask,
            engines.get(..count).unwrap_or_default(),
        );
        let lost = match gt::intel_gt_init_hw(self.bar, &wal) {
            Ok(report) => report.lost_workarounds,
            Err(error) => {
                say(format_args!("intel: M2: intel_gt_init_hw: {error:?}"));
                wal.len()
            }
        };
        let mocs_lost = mocs::intel_mocs_verify(self.bar).count();
        say(format_args!(
            "intel: M2: PAT written, {pat_lost} of 8 lost; GT workarounds {} applied, {lost} lost; MOCS and L3CC written, {mocs_lost} lost",
            wal.len()
        ));
    }

    /// The interrupt: claimed and bound to the port. Without it the driver
    /// polls, which it does anyway until each completion.
    fn interrupts(&mut self, device: &Device<Kernel>) {
        for (reg, value) in irq::gen11_gt_irq_reset() {
            self.bar.write32(reg.offset(), value);
        }
        match device.interrupt(0) {
            Ok(interrupt) => match interrupt.bind(&self.port, KEY_INTERRUPT) {
                Ok(()) => self.interrupt = Some(interrupt),
                Err(error) => say(format_args!(
                    "intel: M2: the interrupt could not be bound ({error:?}); polling only"
                )),
            },
            Err(error) => say(format_args!(
                "intel: M2: no interrupt ({error:?}); polling only"
            )),
        }
    }

    /// `gen11_gt_irq_postinstall` and the master enable.
    fn irq_on(&mut self) {
        for (reg, value) in irq::gen11_gt_irq_postinstall() {
            self.bar.write32(reg.offset(), value);
        }
        self.bar
            .write32(GEN11_GFX_MSTR_IRQ.offset(), GEN11_MASTER_IRQ);
    }

    /// Pages from the pool, bound into the GGTT window.
    fn object(&mut self, pages: usize) -> Result<Obj, Step> {
        let first = self.pool.take(pages).ok_or_else(|| {
            say(format_args!(
                "intel: M2: the pool has no {pages} pages in a row"
            ));
            Step::Memory
        })?;
        let range = self
            .ggtt
            .alloc((pages * PAGE) as u64, PAGE as u64)
            .map_err(|error| {
                say(format_args!("intel: M2: GGTT window: {error:?}"));
                Step::Memory
            })?;
        let obj = Obj {
            first,
            pages,
            ggtt: range.start as u32,
        };
        self.bind(obj, range)?;
        self.pool.dwords(obj).fill(0);
        Ok(obj)
    }

    fn bind(&mut self, obj: Obj, range: Range) -> Result<(), Step> {
        let pages = self.pool.device_pages(obj);
        self.ggtt
            .bind(self.bar, range, pages)
            .map(|_| ())
            .map_err(|error| {
                say(format_args!(
                    "intel: M2: GGTT bind at {:#x}: {error:?}",
                    range.start
                ));
                Step::Memory
            })
    }

    /// Pages from the pool the GPU never sees: a default image.
    fn cpu_object(&mut self, pages: usize) -> Result<Obj, Step> {
        let first = self.pool.take(pages).ok_or(Step::Memory)?;
        Ok(Obj {
            first,
            pages,
            ggtt: 0,
        })
    }

    /// `execlists_sanitize` and `intel_engine_resume` for one engine, with
    /// its workarounds and whitelist read back and its interrupts unmasked.
    fn engine(&mut self, info: EngineInfo) -> Result<Engine, Step> {
        let hwsp = self.object(1)?;
        let state = EngineState::sanitize(
            info,
            self.bar,
            &mut Hwsp {
                pool: &mut self.pool,
                page: hwsp,
            },
        );
        let wal = workarounds::engine_workarounds(&info, mocs::UC_INDEX);
        let whitelist = workarounds::whitelist(&info);
        let report = gt::intel_engine_resume(self.bar, info.mmio_base, &wal, &whitelist, hwsp.ggtt);
        let lost = workarounds::wa_list_verify(self.bar, &wal).count();
        let nonpriv_lost = workarounds::apply_whitelist(info.mmio_base, &whitelist)
            .iter()
            .filter(|(reg, value)| self.bar.read32(reg.offset()) != *value)
            .count();
        self.bar.write32(
            ring_imr(info.mmio_base).offset(),
            irq::gen8_logical_ring_imr_enabled(),
        );
        say(format_args!(
            "intel: M2: {} resumed, status page at GGTT {:#x}: engine workarounds {}, {lost} lost; whitelist {}, {nonpriv_lost} slots lost; ESR {:#x}",
            info.name,
            hwsp.ggtt,
            wal.len(),
            whitelist.len(),
            report.still_in_error
        ));
        Ok(Engine {
            info,
            state,
            hwsp,
            default: None,
        })
    }

    /// A context on `engine` in fresh objects, from its default if there is
    /// one. `slot` is its timeline slot and tag.
    fn context(&mut self, engine: &Engine, slot: usize) -> Result<(HwContext, Obj, Obj), Step> {
        let table = submission::offsets_table(&engine.info);
        let image_pages = (lrc::alloc_state_size(table).0 as usize) / PAGE;
        let image = self.object(image_pages)?;
        let ring = self.object(RING_BYTES as usize / PAGE)?;
        let place = ContextPlace {
            image_ggtt: image.ggtt,
            ring_ggtt: ring.ggtt,
            ring_size: RING_BYTES,
            hwsp_ggtt: engine.hwsp.ggtt,
            timeline: slot,
            tag: slot as u32,
            pml4: self.vm.pml4,
        };
        let context = match engine.default {
            Some(default) => {
                let (words, from) = self.pool.two(image, default).ok_or(Step::Memory)?;
                HwContext::new(engine.info, place, words, Some(from))
            }
            None => HwContext::new(engine.info, place, self.pool.dwords(image), None),
        }
        .map_err(|error| {
            say(format_args!(
                "intel: M2: {} context: {error:?}",
                engine.info.name
            ));
            Step::Memory
        })?;
        Ok((context, image, ring))
    }

    /// Emit `payload` on `context`, submit it, and wait.
    fn run(
        &mut self,
        engine: &mut Engine,
        context: &mut HwContext,
        image: Obj,
        ring: Obj,
        payload: Payload<'_>,
    ) -> Result<(Seen, u64), Step> {
        let (seqno, built) = context.emit(self.pool.dwords(ring), payload).map_err(|_| {
            say(format_args!(
                "intel: M2: {}: no room in the ring",
                engine.info.name
            ));
            Step::Memory
        })?;
        if !engine.state.can_submit() {
            say(format_args!(
                "intel: M2: {}: a submission is still pending",
                engine.info.name
            ));
            return Err(Step::Submit);
        }
        let desc = {
            let regs = lrc::reg_state(self.pool.dwords(image)).ok_or(Step::Memory)?;
            context.update_context(regs, built.tail)
        };
        let queued = self.time.now_us();
        if !engine.state.submit(self.bar, desc, context.place.tag) {
            return Err(Step::Submit);
        }
        let seen = self.wait(engine, context, seqno, queued);
        let took = self.time.now_us().saturating_sub(queued);
        if seen == Seen::Hung {
            say(format_args!(
                "intel: M2: {} seqno {seqno} not complete after {} ms: {}",
                engine.info.name,
                took / 1_000,
                Stuck::read(self.bar, engine.info.mmio_base)
            ));
            return Err(Step::Hung);
        }
        // i915_request_retire: the ring is free up to the saved head.
        context.retire(self.pool.read32(image, PAGE_DWORDS + lrc::ctx::RING_HEAD));
        Ok((seen, took))
    }

    /// Wait for `context`'s `seqno`: on the port until the next poll, an
    /// interrupt serviced (`gen11_irq_handler`) and acknowledged whenever
    /// one comes, the status buffer drained on every wake.
    fn wait(&mut self, engine: &mut Engine, context: &HwContext, seqno: u32, queued: u64) -> Seen {
        let end = queued.saturating_add(REQUEST_PATIENCE_US);
        loop {
            let now = self.time.now_us();
            let until = now.saturating_add(POLL_US).min(end);
            let woke = self.port.wait(Deadline::At(until.saturating_mul(1_000)));
            let interrupted = matches!(woke, Ok(packet) if packet.kind == PACKET_INTERRUPT && packet.key == KEY_INTERRUPT);
            if interrupted {
                self.packets += 1;
                if irq::gen11_irq_handler(self.bar, self.time).is_some() {
                    self.gt_sources += 1;
                }
                if let Some(interrupt) = &self.interrupt {
                    let _ = interrupt.ack();
                }
            }
            if let Err(error) = woke
                && error != Error::TimedOut
            {
                say(format_args!("intel: M2: port wait: {error:?}"));
            }
            let report = engine.state.process_csb(
                self.bar,
                &mut Hwsp {
                    pool: &mut self.pool,
                    page: engine.hwsp,
                },
                self.time,
                |_| {},
            );
            if let Some(csb) = report.error {
                say(format_args!(
                    "intel: M2: {}: status buffer entry {csb:#018x} makes no sense",
                    engine.info.name
                ));
            }
            if context.is_complete(
                &mut Hwsp {
                    pool: &mut self.pool,
                    page: engine.hwsp,
                },
                seqno,
            ) {
                if interrupted {
                    self.by_interrupt += 1;
                    return Seen::Interrupt;
                }
                self.by_poll += 1;
                return Seen::Poll;
            }
            if self.time.now_us() >= end {
                return Seen::Hung;
            }
        }
    }

    /// `__engines_record_defaults` for one engine.
    fn record_default(&mut self, engine: &mut Engine) -> Result<(), Step> {
        let (mut context, image, ring) = self.context(engine, 0)?;
        let wal = workarounds::ctx_workarounds(&engine.info, mocs::UC_INDEX);
        let mut payload = [0_u32; 256];
        let n = request::ctx_wa_payload(self.bar, &engine.info, &wal, &mut payload)
            .ok_or(Step::Memory)?;
        let commands = payload.get(..n).unwrap_or_default();
        let (seen, took) = self.run(
            engine,
            &mut context,
            image,
            ring,
            Payload::Commands(commands),
        )?;
        let table = submission::offsets_table(&engine.info);
        let default = self.cpu_object((lrc::context_size(table) as usize) / PAGE)?;
        let (to, from) = self.pool.two(default, image).ok_or(Step::Memory)?;
        lrc::record_default_state(from, to, table).map_err(|_| Step::Memory)?;
        engine.default = Some(default);
        say(format_args!(
            "intel: M2: {} default image recorded: {} context workarounds in its first request, done in {took} us ({seen:?}); saved RING_HEAD {:#x}",
            engine.info.name,
            wal.len(),
            self.pool.read32(image, PAGE_DWORDS + lrc::ctx::RING_HEAD)
        ));
        Ok(())
    }

    /// `engine_wa_list_verify` for the context list, from a context made
    /// from the default.
    fn verify_ctx_workarounds(&mut self, engine: &mut Engine) -> Result<(), Step> {
        let (mut context, image, ring) = self.context(engine, 1)?;
        let results = self.object(1)?;
        let wal = workarounds::ctx_workarounds(&engine.info, mocs::UC_INDEX);
        let mut srm = [0_u32; 512];
        let n = workarounds::wa_list_srm(&wal, results.ggtt, &mut srm).ok_or(Step::Memory)?;
        let (seen, took) = self.run(
            engine,
            &mut context,
            image,
            ring,
            Payload::Commands(srm.get(..n).unwrap_or_default()),
        )?;
        let mut read = [0_u32; 64];
        for (i, slot) in read.iter_mut().enumerate() {
            *slot = self.pool.read32(results, i);
        }
        let read = read.get(..wal.len().min(64)).unwrap_or_default();
        let lost = workarounds::wa_list_verify_results(&wal, read);
        let mut count = 0;
        for (reg, value) in lost {
            count += 1;
            say(format_args!(
                "intel: M2: context workaround lost: {:#x} reads {value:#010x}",
                reg.offset()
            ));
        }
        say(format_args!(
            "intel: M2: {} context workarounds read back by {} SRMs from a context of the default image: {count} lost ({took} us, {seen:?})",
            wal.len(),
            n / 4
        ));
        Ok(())
    }

    /// `MI_STORE_DWORD_IMM` into a pinned page, from the ring, by GGTT.
    fn store_dword(&mut self, engine: &mut Engine) -> Result<(), Step> {
        let (mut context, image, ring) = self.context(engine, 2)?;
        const VALUE: u32 = 0xc0ff_ee02;
        let target = self.object(1)?;
        let commands = [
            gpu::MI_STORE_DWORD_IMM_GEN4 | gpu::MI_USE_GGTT,
            target.ggtt + 0x40,
            0,
            VALUE,
        ];
        let (seen, took) = self.run(
            engine,
            &mut context,
            image,
            ring,
            Payload::Commands(&commands),
        )?;
        let read = self.pool.read32(target, 0x10);
        say(format_args!(
            "intel: M2: MI_STORE_DWORD_IMM on {} into the pinned page at GGTT {:#x} (device {:#x}): read {read:#010x}, {} ({took} us, {seen:?})",
            engine.info.name,
            target.ggtt + 0x40,
            self.pool.device_pages(target).first().copied().unwrap_or(0) + 0x40,
            if read == VALUE {
                "as written"
            } else {
                "NOT what was written"
            }
        ));
        if read == VALUE {
            Ok(())
        } else {
            Err(Step::Wrong)
        }
    }

    /// Pages from the pool mapped at `address` in the PPGTT (PAT index 0,
    /// write-back), unbound from the GGTT.
    fn ppgtt_object(&mut self, pages: usize, address: u64) -> Result<Obj, Step> {
        let obj = self.cpu_object(pages)?;
        self.pool.dwords(obj).fill(0);
        let bytes = (pages * PAGE) as u64;
        let vm = self.vm;
        vm.alloc_va_range(&mut self.pool, address, bytes)
            .map_err(|_| Step::Memory)?;
        let mut addresses = [0_u64; 8];
        for (slot, &page) in addresses.iter_mut().zip(self.pool.device_pages(obj)) {
            *slot = page;
        }
        let pages_at = addresses.get(..pages).unwrap_or_default();
        let _ = vm
            .insert_entries(&mut self.pool, pages_at.iter().copied(), address, 0)
            .map_err(|_| Step::Memory)?;
        Ok(obj)
    }

    /// `XY_FAST_COPY_BLT` on BCS0, two pages from one PPGTT range to another.
    fn fast_copy(&mut self, engine: &mut Engine) -> Result<(), Step> {
        const SRC: u64 = 0x10_0000_0000;
        const DST: u64 = 0x10_0001_0000;
        let (mut context, image, ring) = self.context(engine, 1)?;
        let src = self.ppgtt_object(2, SRC)?;
        let dst = self.ppgtt_object(2, DST)?;
        for (i, word) in self.pool.dwords(src).iter_mut().enumerate() {
            *word = 0xb17_0000 ^ (i as u32).wrapping_mul(0x9e37_79b9);
        }
        let mut copy = [0_u32; 10];
        emit_fast_copy(&mut Cs::new(&mut copy), DST, SRC, 2 * PAGE as u32);
        let (seen, took) = self.run(engine, &mut context, image, ring, Payload::Commands(&copy))?;
        let mut differ = 0;
        for i in 0..2 * PAGE_DWORDS {
            if self.pool.read32(dst, i) != self.pool.read32(src, i) {
                differ += 1;
            }
        }
        say(format_args!(
            "intel: M2: XY_FAST_COPY_BLT on {}: 8 KiB from PPGTT {SRC:#x} to {DST:#x}, {differ} of {} dwords differ ({took} us, {seen:?})",
            engine.info.name,
            2 * PAGE_DWORDS
        ));
        if differ == 0 {
            Ok(())
        } else {
            Err(Step::Wrong)
        }
    }

    /// A batch from the PPGTT on RCS0, not privileged, storing into a PPGTT
    /// page.
    fn ppgtt_batch(&mut self, engine: &mut Engine) -> Result<(), Step> {
        const BATCH: u64 = 0x1_0000_0000;
        const TARGET: u64 = 0x1_0000_1000;
        const VALUE: u32 = 0x5eed_0003;
        let (mut context, image, ring) = self.context(engine, 3)?;
        let batch = self.ppgtt_object(1, BATCH)?;
        let target = self.ppgtt_object(1, TARGET)?;
        {
            let words = self.pool.dwords(batch);
            let mut cs = Cs::new(words);
            emit_store_dword(&mut cs, TARGET + 0x80, VALUE);
            cs.push(gpu::MI_BATCH_BUFFER_END);
        }
        let (seen, took) = self.run(
            engine,
            &mut context,
            image,
            ring,
            Payload::Batch {
                address: BATCH,
                flags: 0,
            },
        )?;
        let read = self.pool.read32(target, 0x20);
        say(format_args!(
            "intel: M2: a batch from the PPGTT at {BATCH:#x} on {}: its store read {read:#010x}, {} ({took} us, {seen:?})",
            engine.info.name,
            if read == VALUE {
                "as written"
            } else {
                "NOT what was written"
            }
        ));
        if read == VALUE {
            Ok(())
        } else {
            Err(Step::Wrong)
        }
    }
}

impl GgttPtes for Bar0 {
    fn read_pte(&mut self, index: u64) -> u64 {
        self.ggtt_entry(index).unwrap_or(0)
    }

    fn write_pte(&mut self, index: u64, pte: u64) {
        self.set_ggtt_entry(index, pte);
    }
}

/// What a hung engine's registers say, as i915's hang report prints them
/// (`intel_engine_dump`'s ring and execlist lines).
struct Stuck {
    head: u32,
    tail: u32,
    acthd: u32,
    ipehr: u32,
    instdone: u32,
    esr: u32,
    eir: u32,
    status: u64,
    csb_ptr: u32,
}

impl Stuck {
    fn read(bar: &mut Bar0, base: u32) -> Stuck {
        use ferrix_intel_xelp::engine::{
            ring_acthd, ring_context_status_ptr, ring_eir, ring_esr, ring_execlist_status_hi,
            ring_execlist_status_lo, ring_head, ring_instdone, ring_ipehr, ring_tail,
        };
        let mut read = |reg: ferrix_intel_xelp::reg::Reg| bar.read32(reg.offset());
        Stuck {
            head: read(ring_head(base)),
            tail: read(ring_tail(base)),
            acthd: read(ring_acthd(base)),
            ipehr: read(ring_ipehr(base)),
            instdone: read(ring_instdone(base)),
            esr: read(ring_esr(base)),
            eir: read(ring_eir(base)),
            status: u64::from(read(ring_execlist_status_lo(base)))
                | (u64::from(read(ring_execlist_status_hi(base))) << 32),
            csb_ptr: read(ring_context_status_ptr(base)),
        }
    }
}

impl core::fmt::Display for Stuck {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "RING_HEAD {:#x} RING_TAIL {:#x} ACTHD {:#x} IPEHR {:#x} INSTDONE {:#x} ESR {:#x} EIR {:#x} EXECLIST_STATUS {:#018x} CSB_PTR {:#x}",
            self.head,
            self.tail,
            self.acthd,
            self.ipehr,
            self.instdone,
            self.esr,
            self.eir,
            self.status,
            self.csb_ptr
        )
    }
}
