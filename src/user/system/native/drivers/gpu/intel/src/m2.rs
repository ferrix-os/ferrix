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

use ferrix_intel_xelp::commands::{Cs, emit_fast_copy, emit_rect_copy, emit_store_dword, gpu};
use ferrix_intel_xelp::engine::{ENGINES, EngineInfo, ring_imr};
use ferrix_intel_xelp::generated::i915_reg::{GEN11_GFX_MSTR_IRQ, GEN11_MASTER_IRQ};
use ferrix_intel_xelp::ggtt_space::{GgttSpace, Range};
use ferrix_intel_xelp::io::{Clock, GgttPtes, Mmio, StatusPage};
use ferrix_intel_xelp::lrc;
use ferrix_intel_xelp::ppgtt::{Ppgtt, PtMemory};
use ferrix_intel_xelp::request::{self, Payload};
use ferrix_intel_xelp::submission::{self, ContextPlace, EngineState, HwContext};
use ferrix_intel_xelp::workarounds;
use ferrix_intel_xelp::{gt, irq, mocs, reset};
use ferrix_native_abi::types::PACKET_INTERRUPT;
use ferrix_rt::Kernel;
use ferrix_rt::native::device::{Device, Interrupt};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::Deadline;
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::{Pin, PinAccess, device_address};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::{self, Vmo};

use crate::steps::{At, Steps};
use crate::{Bar0, Step, Time};

/// Pages in the pool: six context images and their rings (two for the
/// request-cost probe), two default images, two status pages, the test
/// pages and the page tables, with room.
const POOL_PAGES: usize = 448;
/// Bytes in a page.
const PAGE: usize = 4096;
/// Copies of each variant the request-cost probe runs in one request.
const COST_REPEATS: usize = 24;
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
/// How long the deliberately hung batch runs before it is reset.
const HANG_PATIENCE_US: u64 = 100_000;
/// The port key of the GPU's interrupt.
const KEY_INTERRUPT: u64 = 1;
/// How far M2 goes on the box (`intelgpu-m2.sh` builds each checkpoint by
/// setting this): 1 the PAT, MOCS, workarounds, engines and a PPGTT read
/// back, no command run; 2 the default images and `MI_STORE_DWORD_IMM`; 3
/// the fast-copy blit and the PPGTT batch; 4 a hung batch reset and banned.
pub(crate) const CHECKPOINT: u8 = 4;

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
pub(crate) struct Pool<const N: usize = POOL_PAGES> {
    _pin: Pin<Kernel>,
    _vmo: Vmo<Kernel>,
    base: usize,
    device: [u64; N],
    used: [bool; N],
}

impl<const N: usize> Pool<N> {
    pub(crate) fn new(device: &Device<Kernel>) -> Result<Self, Step> {
        let bytes = N * PAGE;
        let vmo = vmo::create(Kernel, bytes).map_err(|_| Step::Memory)?;
        let pin = device
            .pin(&vmo, 0, bytes, PinAccess::ReadWrite)
            .map_err(|_| Step::Memory)?;
        let mut raw = [[0_u8; 8]; N];
        let got = pin.addresses(&mut raw).map_err(|_| Step::Memory)?;
        if !got.is_complete() || got.pages != N {
            return Err(Step::Memory);
        }
        let mut addresses = [0_u64; N];
        for (slot, bytes) in addresses.iter_mut().zip(raw) {
            *slot = device_address(bytes);
        }
        let base = vmo
            .map(None, bytes, Protection::ReadWrite, 0)
            .map_err(|_| Step::Memory)?;
        Ok(Self {
            _pin: pin,
            _vmo: vmo,
            base,
            device: addresses,
            used: [false; N],
        })
    }

    /// `pages` free pages in a row, first fit.
    fn take(&mut self, pages: usize) -> Option<usize> {
        let mut run = 0;
        for index in 0..N {
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
        (page < N && index < PAGE_DWORDS).then(|| self.base + page * PAGE + index * 4)
    }

    /// An object's dwords. Borrowing the pool mutably keeps every other
    /// object's slice out of reach while this one is held; what the GPU
    /// writes into an object is read through [`Pool::read32`], not a slice.
    fn dwords(&mut self, obj: Obj) -> &mut [u32] {
        let len = obj.pages.min(N.saturating_sub(obj.first)) * PAGE_DWORDS;
        let start = self.base + obj.first.min(N) * PAGE;
        // SAFETY: `start..start + len * 4` lies inside the pool's mapping,
        // which this process made read-write and `self` keeps (`_vmo`); the
        // `&mut self` borrow makes it the only slice of the pool alive.
        unsafe { core::slice::from_raw_parts_mut(start as *mut u32, len) }
    }

    /// Two different objects at once: `a` to write, `b` to read.
    fn two(&mut self, a: Obj, b: Obj) -> Option<(&mut [u32], &[u32])> {
        let overlap = a.first < b.first + b.pages && b.first < a.first + a.pages;
        if overlap || a.first + a.pages > N || b.first + b.pages > N {
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
impl<const N: usize> PtMemory for Pool<N> {
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
    /// Pipe A's plane 1, when the firmware lit it.
    pub(crate) plane: Option<ScreenPlane>,
}

/// The firmware's lit plane, as M1 read it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScreenPlane {
    /// `PLANE_SURF`'s GGTT offset.
    pub(crate) ggtt: u32,
    /// `PLANE_STRIDE`, in 64-byte units for a linear plane.
    pub(crate) stride: u32,
    /// `PLANE_CTL`.
    pub(crate) ctl: u32,
    /// Pixels.
    pub(crate) width: u32,
    /// Rows.
    pub(crate) height: u32,
    /// DRM's connector type for the socket pipe A drives, from
    /// `TRANS_DDI_FUNC_CTL_A` and the panel power sequencer, 0 for unknown
    /// (`ferrix_intel_xelp::plane::Output`).
    pub(crate) connector: u32,
}

impl ScreenPlane {
    /// `PLANE_CTL_TILED_LINEAR` and `PLANE_CTL_FORMAT_XRGB_8888` (bits 27:23
    /// 4 << 1 on ICL+).
    pub(crate) fn linear_xrgb8888(&self) -> bool {
        (self.ctl >> 10) & 0x7 == 0 && (self.ctl >> 23) & 0x1f == 4 << 1
    }

    /// Bytes a row.
    pub(crate) fn pitch_bytes(&self) -> u32 {
        (self.stride & 0xfff) * 64
    }
}

/// Run M2. Returns how far it got; each step says its line.
pub(crate) fn run(
    bar: &mut Bar0,
    device: &Device<Kernel>,
    time: &mut Time,
    firmware: Firmware,
    steps: &Steps,
) -> Result<(), Step> {
    let started = time.now_us();
    steps.before(
        At::Pin,
        format_args!(
            "pin {POOL_PAGES} pages into 00:02.0's domain (the kernel turns bus mastering on), map them, make a PPGTT in memory"
        ),
    )?;
    let pool = Pool::new(device).inspect_err(|_| {
        ferrix_rt::err!("intel: M2: {POOL_PAGES} pages could not be pinned and mapped");
    })?;
    let window_start = (firmware
        .scanned
        .max(firmware.last_present.map_or(0, |l| l + 1))
        * 4096)
        .max(GGTT_WINDOW_FLOOR);
    let ggtt = GgttSpace::new(window_start, firmware.total);
    ferrix_rt::info!(
        "intel: M2: {POOL_PAGES} pages pinned; GGTT window {window_start:#x}..{:#x} (M1 read {} entries, last present {:?})",
        firmware.total,
        firmware.scanned,
        firmware.last_present
    );
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
    m2.gt_init(firmware, steps)?;
    m2.interrupts(device, steps)?;
    steps.before(
        At::Rcs0,
        format_args!(
            "RCS0 at 0x2000: status page bound in the GGTT, sanitized, workarounds, whitelist, RING_MODE, HWS_PGA, EMR, IMR"
        ),
    )?;
    let mut rcs = m2.engine(ENGINES[0])?;
    steps.before(At::Bcs0, format_args!("BCS0 at 0x22000: the same"))?;
    let mut bcs = m2.engine(ENGINES[1])?;
    steps.before(
        At::IrqOn,
        format_args!("GT interrupts enabled, then the master GFX_MSTR_IRQ 0x190010"),
    )?;
    m2.irq_on();
    steps.before(
        At::Ppgtt,
        format_args!("a PPGTT mapping made and walked back (memory only)"),
    )?;
    m2.ppgtt_readback()?;
    m2.checkpoints(&mut rcs, &mut bcs, firmware.plane, steps)?;
    ferrix_rt::info!(
        "intel: M2: checkpoint {CHECKPOINT} done: completions {} by interrupt, {} by the {} us poll; {} interrupt packets, {} with GT sources ({} ms)",
        m2.by_interrupt,
        m2.by_poll,
        POLL_US,
        m2.packets,
        m2.gt_sources,
        m2.time.now_us().saturating_sub(started) / 1_000
    );
    Ok(())
}

impl M2<'_> {
    /// Checkpoints 2 to 4, as far as [`CHECKPOINT`] says: what the GPU runs.
    fn checkpoints(
        &mut self,
        rcs: &mut Engine,
        bcs: &mut Engine,
        plane: Option<ScreenPlane>,
        steps: &Steps,
    ) -> Result<(), Step> {
        let m2 = self;
        if CHECKPOINT >= 2 {
            steps.before(
            At::Commands,
            format_args!(
                "checkpoint 2: the default images and MI_STORE_DWORD_IMM, the first commands the GPU runs"
            ),
        )?;
            for engine in [&mut *rcs, &mut *bcs] {
                m2.record_default(engine)?;
            }
            m2.verify_ctx_workarounds(rcs)?;
            m2.store_dword(rcs, 2)?;
        }
        if CHECKPOINT >= 3 {
            steps.before(
                At::Blits,
                format_args!("checkpoint 3: XY_FAST_COPY_BLT, the screen blit, a PPGTT batch"),
            )?;
            m2.fast_copy(bcs)?;
            m2.screen_blit(bcs, plane)?;
            m2.ppgtt_batch(rcs)?;
            // A measurement: said, never a failed check.
            for engine in [&mut *rcs, &mut *bcs] {
                if let Err(step) = m2.request_cost(engine) {
                    ferrix_rt::warn!(
                        "intel: M2: request cost on {} not measured ({step:?})",
                        engine.info.name
                    );
                }
            }
        }
        if CHECKPOINT >= 4 {
            steps.before(
                At::Hang,
                format_args!("checkpoint 4: a hung batch on RCS0, reset and banned"),
            )?;
            m2.hang(rcs)?;
        }
        Ok(())
    }

    /// `i915_gem_init`'s PAT, then `intel_gt_init_hw`: the GT workarounds
    /// and the MOCS, each read back.
    fn gt_init(&mut self, firmware: Firmware, steps: &Steps) -> Result<(), Step> {
        steps.before(
            At::Pat,
            format_args!("write the PAT 0x4800..0x481c and read it back"),
        )?;
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
        steps.before(
            At::GtInit,
            format_args!(
                "intel_gt_init_hw: {} GT workarounds written and read back (MCR steering 0xfdc first), the WOPCM, MOCS and L3CC",
                wal.len()
            ),
        )?;
        let lost = match gt::intel_gt_init_hw(self.bar, &wal) {
            Ok(report) => report.lost_workarounds,
            Err(error) => {
                ferrix_rt::err!("intel: M2: intel_gt_init_hw: {error:?}");
                wal.len()
            }
        };
        let mocs_lost = mocs::intel_mocs_verify(self.bar).count();
        ferrix_rt::info!(
            "intel: M2: PAT written, {pat_lost} of 8 lost; GT workarounds {} applied, {lost} lost; MOCS and L3CC written, {mocs_lost} lost",
            wal.len()
        );
        // The values themselves, against i915's (tgl_setup_private_ppat,
        // gen12_mocs_table): what the checkpoint script compares.
        let mut pat = [0_u32; 8];
        for (slot, (reg, _)) in pat
            .iter_mut()
            .zip(ferrix_intel_xelp::gtt::tgl_setup_private_ppat())
        {
            *slot = self.bar.read32(reg.offset());
        }
        ferrix_rt::debug!("intel: M2: PAT read {pat:x?}");
        let mut global = [0_u32; 8];
        for (i, slot) in global.iter_mut().enumerate() {
            *slot = self.bar.read32(mocs::gen12_global_mocs(i as u32).offset());
        }
        let mut l3cc = [0_u32; 4];
        for (i, slot) in l3cc.iter_mut().enumerate() {
            *slot = self.bar.read32(mocs::gen9_lncfcmocs(i as u32).offset());
        }
        ferrix_rt::debug!(
            "intel: M2: MOCS read GLOBAL_MOCS[0..8] {global:x?} LNCFCMOCS[0..4] {l3cc:x?}"
        );
        Ok(())
    }

    /// A PPGTT mapping made and walked back: two pool pages at 8 GiB, each
    /// page's entry and translation said, then unmapped again.
    fn ppgtt_readback(&mut self) -> Result<(), Step> {
        const AT: u64 = 0x2_0000_0000;
        let before = self.pool.used.iter().filter(|&&used| used).count();
        let obj = self.ppgtt_object(2, AT)?;
        let vm = self.vm;
        let mut wrong = 0;
        let mut pages = [0_u64; 2];
        for (slot, &page) in pages.iter_mut().zip(self.pool.device_pages(obj)) {
            *slot = page;
        }
        for (n, &page) in pages.iter().enumerate() {
            let va = AT + (n * PAGE) as u64;
            let translated = vm.translate(&mut self.pool, va + 0x123);
            wrong += u32::from(translated != Some(page + 0x123));
            ferrix_rt::debug!("intel: M2: PPGTT {va:#x} -> {translated:x?} (page {page:#x})");
        }
        vm.clear_range(&mut self.pool, AT, 2 * PAGE as u64)
            .map_err(|_| Step::Memory)?;
        self.pool.give(obj.first, obj.pages);
        let after = self.pool.used.iter().filter(|&&used| used).count();
        let scratch = vm.translate(&mut self.pool, AT);
        ferrix_rt::info!(
            "intel: M2: PPGTT PML4 {:#x}, scratch {:#x} (PTE {:#x}, PDEs {:#x} {:#x} {:#x}): 2 pages mapped and walked back, {wrong} wrong; unmapped, the address reads {}, pool pages {before} -> {after}",
            vm.pml4,
            vm.scratch[0],
            vm.scratch_encode[0],
            vm.scratch_encode[1],
            vm.scratch_encode[2],
            vm.scratch_encode[3],
            if scratch.is_none() {
                "scratch"
            } else {
                "STILL MAPPED"
            }
        );
        if wrong == 0 && scratch.is_none() && before == after {
            Ok(())
        } else {
            Err(Step::Wrong)
        }
    }

    /// The interrupt: claimed and bound to the port. Without it the driver
    /// polls, which it does anyway until each completion.
    fn interrupts(&mut self, device: &Device<Kernel>, steps: &Steps) -> Result<(), Step> {
        steps.before(
            At::IrqReset,
            format_args!("gen11_gt_irq_reset: the GT's interrupt enables off and masks set"),
        )?;
        for (reg, value) in irq::gen11_gt_irq_reset() {
            self.bar.write32(reg.offset(), value);
        }
        steps.before(
            At::Msi,
            format_args!("claim the MSI and bind it to the port (the kernel programs 00:02.0's MSI capability)"),
        )?;
        match device.interrupt(0) {
            Ok(interrupt) => match interrupt.bind(&self.port, KEY_INTERRUPT) {
                Ok(()) => self.interrupt = Some(interrupt),
                Err(error) => ferrix_rt::warn!(
                    "intel: M2: the interrupt could not be bound ({error:?}); polling only"
                ),
            },
            Err(error) => ferrix_rt::warn!("intel: M2: no interrupt ({error:?}); polling only"),
        }
        Ok(())
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
            ferrix_rt::err!("intel: M2: the pool has no {pages} pages in a row");
            Step::Memory
        })?;
        let range = self
            .ggtt
            .alloc((pages * PAGE) as u64, PAGE as u64)
            .map_err(|error| {
                ferrix_rt::err!("intel: M2: GGTT window: {error:?}");
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
                ferrix_rt::err!("intel: M2: GGTT bind at {:#x}: {error:?}", range.start);
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
        // The registers M0's i915_engine_info printed for the same engine
        // after i915's resume (RING_MODE, EMR, EL_STAT, the CSB pointers),
        // read before RING_IMR is opened.
        {
            use ferrix_intel_xelp::engine::{
                ring_context_status_ptr, ring_execlist_status_hi, ring_execlist_status_lo,
                ring_mode_gen7,
            };
            let base = info.mmio_base;
            let mode = self.bar.read32(ring_mode_gen7(base).offset());
            let emr = self
                .bar
                .read32(ferrix_intel_xelp::generated::intel_engine_regs::ring_emr(base).offset());
            let el_lo = self.bar.read32(ring_execlist_status_lo(base).offset());
            let el_hi = self.bar.read32(ring_execlist_status_hi(base).offset());
            let csb = self.bar.read32(ring_context_status_ptr(base).offset());
            let write = self
                .pool
                .read32(hwsp, ferrix_intel_xelp::execlists::ICL_HWS_CSB_WRITE_INDEX);
            ferrix_rt::debug!(
                "intel: M2: {} RING_MODE {mode:#010x} RING_EMR {emr:#010x} EL_STAT_HI {el_hi:#010x} EL_STAT_LO {el_lo:#010x} CSB_PTR {csb:#010x} HWSP CSB write {write}",
                info.name
            );
        }
        self.bar.write32(
            ring_imr(info.mmio_base).offset(),
            irq::gen8_logical_ring_imr_enabled(),
        );
        ferrix_rt::info!(
            "intel: M2: {} resumed, status page at GGTT {:#x}: engine workarounds {}, {lost} lost; whitelist {}, {nonpriv_lost} slots lost; ESR {:#x}",
            info.name,
            hwsp.ggtt,
            wal.len(),
            whitelist.len(),
            report.still_in_error
        );
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
            ferrix_rt::err!("intel: M2: {} context: {error:?}", engine.info.name);
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
        let (seqno, _, queued) = self.submit(engine, context, image, ring, payload)?;
        let seen = self.wait(engine, context, seqno, queued, REQUEST_PATIENCE_US);
        let took = self.time.now_us().saturating_sub(queued);
        if seen == Seen::Hung {
            ferrix_rt::err!(
                "intel: M2: {} seqno {seqno} not complete after {} ms: {}",
                engine.info.name,
                took / 1_000,
                Stuck::read(self.bar, engine.info.mmio_base)
            );
            return Err(Step::Hung);
        }
        // i915_request_retire: the ring is free up to the saved head.
        context.retire(self.pool.read32(image, PAGE_DWORDS + lrc::ctx::RING_HEAD));
        Ok((seen, took))
    }

    /// Emit `payload` on `context` and submit it: its seqno, where it lies
    /// in the ring, and when it was queued.
    fn submit(
        &mut self,
        engine: &mut Engine,
        context: &mut HwContext,
        image: Obj,
        ring: Obj,
        payload: Payload<'_>,
    ) -> Result<(u32, request::Built, u64), Step> {
        if context.banned {
            return Err(Step::Banned);
        }
        let (seqno, built) = context.emit(self.pool.dwords(ring), payload).map_err(|_| {
            ferrix_rt::warn!("intel: M2: {}: no room in the ring", engine.info.name);
            Step::Memory
        })?;
        if !engine.state.can_submit() {
            ferrix_rt::warn!(
                "intel: M2: {}: a submission is still pending",
                engine.info.name
            );
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
        Ok((seqno, built, queued))
    }

    /// Wait for `context`'s `seqno`: on the port until the next poll, an
    /// interrupt serviced (`gen11_irq_handler`) and acknowledged whenever
    /// one comes, the status buffer drained on every wake.
    fn wait(
        &mut self,
        engine: &mut Engine,
        context: &HwContext,
        seqno: u32,
        queued: u64,
        patience_us: u64,
    ) -> Seen {
        let end = queued.saturating_add(patience_us);
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
                ferrix_rt::warn!("intel: M2: port wait: {error:?}");
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
                ferrix_rt::err!(
                    "intel: M2: {}: status buffer entry {csb:#018x} makes no sense",
                    engine.info.name
                );
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
        ferrix_rt::info!(
            "intel: M2: {} default image recorded: {} context workarounds in its first request, done in {took} us ({seen:?}); saved RING_HEAD {:#x}",
            engine.info.name,
            wal.len(),
            self.pool.read32(image, PAGE_DWORDS + lrc::ctx::RING_HEAD)
        );
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
            ferrix_rt::warn!(
                "intel: M2: context workaround lost: {:#x} reads {value:#010x}",
                reg.offset()
            );
        }
        ferrix_rt::info!(
            "intel: M2: {} context workarounds read back by {} SRMs from a context of the default image: {count} lost ({took} us, {seen:?})",
            wal.len(),
            n / 4
        );
        Ok(())
    }

    /// `MI_STORE_DWORD_IMM` into a pinned page, from the ring, by GGTT.
    fn store_dword(&mut self, engine: &mut Engine, slot: usize) -> Result<(), Step> {
        let (mut context, image, ring) = self.context(engine, slot)?;
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
        ferrix_rt::info!(
            "intel: M2: MI_STORE_DWORD_IMM on {} into the pinned page at GGTT {:#x} (device {:#x}): read {read:#010x}, {} ({took} us, {seen:?})",
            engine.info.name,
            target.ggtt + 0x40,
            self.pool.device_pages(target).first().copied().unwrap_or(0) + 0x40,
            if read == VALUE {
                "as written"
            } else {
                "NOT what was written"
            }
        );
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
        // Every page, one entry each. (Until 2026-10-20 the addresses went
        // through an eight-entry array, so a larger object mapped nothing:
        // the box's screen blit read and wrote the scratch page, 32768 of
        // 32768 pixels differing.)
        for page in 0..pages {
            let device = self
                .pool
                .device_pages(obj)
                .get(page)
                .copied()
                .ok_or(Step::Memory)?;
            vm.insert_entry(&mut self.pool, device, address + (page * PAGE) as u64, 0)
                .map_err(|_| Step::Memory)?;
        }
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
        // FNV-1a over each side's bytes, as the checkpoint compares them.
        let (mut src_hash, mut dst_hash) = (FNV_OFFSET, FNV_OFFSET);
        for i in 0..2 * PAGE_DWORDS {
            let (s, d) = (self.pool.read32(src, i), self.pool.read32(dst, i));
            differ += u32::from(s != d);
            src_hash = fnv1a(src_hash, s);
            dst_hash = fnv1a(dst_hash, d);
        }
        ferrix_rt::info!(
            "intel: M2: XY_FAST_COPY_BLT on {}: 8 KiB from PPGTT {SRC:#x} to {DST:#x}, {differ} of {} dwords differ, FNV-1a source {src_hash:#018x} copy {dst_hash:#018x} ({took} us, {seen:?})",
            engine.info.name,
            2 * PAGE_DWORDS
        );
        if differ == 0 {
            Ok(())
        } else {
            Err(Step::Wrong)
        }
    }

    /// What the commands the driver wraps around every request cost the
    /// GPU (H4 of `docs/GAME-MODE.md` §5.7): the engine's timestamp stored
    /// before and after [`COST_REPEATS`] copies of each variant, run in one
    /// request on an otherwise idle engine, so each line is the commands'
    /// own time, not the stall they put into a busy pipeline. Variants: as
    /// every request has them (the invalidation with the AUX table's, the
    /// start breadcrumb, the final breadcrumb), without the AUX table's
    /// invalidation and its quiescing flush, and each part alone.
    fn request_cost(&mut self, engine: &mut Engine) -> Result<(), Step> {
        use ferrix_intel_xelp::engine_cs as cs_;
        let scratch = self.object(1)?;
        let info = engine.info;
        let render = info.class == ferrix_intel_xelp::engine::RENDER_CLASS;
        let timestamp = ferrix_intel_xelp::engine::ring_timestamp(info.mmio_base).offset();
        let hz = u64::from(ferrix_intel_xelp::clock::gen11_read_clock_frequency(
            self.bar,
        ))
        .max(1);
        let seqno_at = scratch.ggtt + 0x100;
        let preempt_at = scratch.ggtt + 0x200;
        let full = |cs: &mut Cs<'_>| cs_::gen12_emit_flush(&info, cs_::EMIT_INVALIDATE, cs);
        let bare = |cs: &mut Cs<'_>| {
            if render {
                cs_::gen12_emit_invalidate_rcs_without_aux(cs);
            } else {
                cs_::gen12_emit_invalidate_xcs_without_aux(cs);
            }
        };
        let start = |cs: &mut Cs<'_>| cs_::gen8_emit_init_breadcrumb(cs, seqno_at, 1);
        let fini = |cs: &mut Cs<'_>| {
            let _ = if render {
                cs_::gen12_emit_fini_breadcrumb_rcs(cs, 1, seqno_at, preempt_at)
            } else {
                cs_::gen12_emit_fini_breadcrumb_xcs(cs, 1, seqno_at, preempt_at)
            };
        };
        type Part<'p> = &'p dyn Fn(&mut Cs<'_>);
        let variants: [(&str, [Option<Part<'_>>; 3]); 5] = [
            ("as every request", [Some(&full), Some(&start), Some(&fini)]),
            (
                "without the AUX invalidation",
                [Some(&bare), Some(&start), Some(&fini)],
            ),
            ("the invalidation alone", [Some(&full), None, None]),
            (
                "the invalidation without AUX alone",
                [Some(&bare), None, None],
            ),
            ("the final breadcrumb alone", [None, None, Some(&fini)]),
        ];
        let (mut context, image, ring) = self.context(engine, 3)?;
        for (name, parts) in variants {
            let mut commands = [0_u32; 2048];
            let length = {
                let mut cs = Cs::new(&mut commands);
                let stamp = |cs: &mut Cs<'_>, at: u32| {
                    cs.push(gpu::MI_STORE_REGISTER_MEM_GEN8 | gpu::MI_SRM_LRM_GLOBAL_GTT);
                    cs.push(timestamp);
                    cs.push(scratch.ggtt + at);
                    cs.push(0);
                };
                stamp(&mut cs, 0);
                for part in (0..COST_REPEATS).flat_map(|_| parts.iter().flatten()) {
                    part(&mut cs);
                }
                stamp(&mut cs, 4);
                cs.len()
            };
            let Some(payload) = commands.get(..length).filter(|_| length < commands.len()) else {
                return Err(Step::Memory);
            };
            let (seen, took) = self.run(
                engine,
                &mut context,
                image,
                ring,
                Payload::Commands(payload),
            )?;
            let ticks = self
                .pool
                .read32(scratch, 1)
                .wrapping_sub(self.pool.read32(scratch, 0));
            let ns = u64::from(ticks) * 1_000_000_000 / hz / COST_REPEATS as u64;
            ferrix_rt::info!(
                "intel: M2: request cost on {}: {name}: {ns} ns a request on the GPU ({} dwords, {COST_REPEATS} in {ticks} ticks at {hz} Hz; {took} us, {seen:?})",
                info.name,
                length / COST_REPEATS,
            );
        }
        Ok(())
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
        ferrix_rt::info!(
            "intel: M2: a batch from the PPGTT at {BATCH:#x} on {}: its store read {read:#010x}, {} ({took} us, {seen:?})",
            engine.info.name,
            if read == VALUE {
                "as written"
            } else {
                "NOT what was written"
            }
        );
        if read == VALUE {
            Ok(())
        } else {
            Err(Step::Wrong)
        }
    }
    /// `__intel_engine_reset_bh` under execlists: the engine reset alone,
    /// its ports and status buffer reset, and the engine resumed.
    fn reset_engine(&mut self, engine: &mut Engine) -> Result<(), Step> {
        let started = self.time.now_us();
        let report =
            reset::intel_engine_reset(self.bar, self.time, &engine.info).ok_or(Step::Hung)?;
        let mut cancelled = 0;
        engine.state.execlists.reset(
            self.bar,
            &mut Hwsp {
                pool: &mut self.pool,
                page: engine.hwsp,
            },
            engine.info.mmio_base,
            |_| cancelled += 1,
        );
        let wal = workarounds::engine_workarounds(&engine.info, mocs::UC_INDEX);
        let whitelist = workarounds::whitelist(&engine.info);
        let resumed = gt::intel_engine_resume(
            self.bar,
            engine.info.mmio_base,
            &wal,
            &whitelist,
            engine.hwsp.ggtt,
        );
        self.bar.write32(
            ring_imr(engine.info.mmio_base).offset(),
            irq::gen8_logical_ring_imr_enabled(),
        );
        ferrix_rt::info!(
            "intel: M2: {} reset in {} us: stop {:x?}, pending forcewakes {:#x}, ready {:x?}, GDRST {:x?}; {cancelled} port(s) cancelled; ESR after resume {:#x}",
            engine.info.name,
            self.time.now_us().saturating_sub(started),
            report.stopped,
            report.pending_fw,
            report.ready,
            report.reset,
            resumed.still_in_error
        );
        if !report.is_reset() {
            return Err(Step::Hung);
        }
        Ok(())
    }

    /// A deliberately hung batch (`igt_spinner`: an arbitration point and a
    /// jump back to itself, from the PPGTT) on `engine`: not complete in
    /// [`HANG_PATIENCE_US`], the engine reset alone (`GEN11_GRDOM_*`, never
    /// the GT or the function), the ports and status buffer reset, the
    /// engine resumed, the batch skipped and its context banned and
    /// replayed so its seqno is written; a later request on it refused;
    /// then a store on another context, to show the engine runs again.
    fn hang(&mut self, engine: &mut Engine) -> Result<(), Step> {
        const SPINNER: u64 = 0x3_0000_0000;
        let (mut context, image, ring) = self.context(engine, 4)?;
        let batch = self.ppgtt_object(1, SPINNER)?;
        {
            let words = self.pool.dwords(batch);
            let mut cs = Cs::new(words);
            cs.push(gpu::MI_ARB_CHECK);
            cs.push(gpu::MI_BATCH_BUFFER_START_GEN8 | (1 << 8));
            cs.push(SPINNER as u32);
            cs.push((SPINNER >> 32) as u32);
        }
        let (seqno, built, queued) = self.submit(
            engine,
            &mut context,
            image,
            ring,
            Payload::Batch {
                address: SPINNER,
                flags: 0,
            },
        )?;
        let seen = self.wait(engine, &context, seqno, queued, HANG_PATIENCE_US);
        let stuck = Stuck::read(self.bar, engine.info.mmio_base);
        if seen != Seen::Hung {
            ferrix_rt::warn!(
                "intel: M2: the spinner on {} completed ({seen:?}): no hang to reset",
                engine.info.name
            );
            return Err(Step::Wrong);
        }
        ferrix_rt::info!(
            "intel: M2: the spinner on {} hung as meant after {} ms: {stuck}",
            engine.info.name,
            HANG_PATIENCE_US / 1_000
        );

        self.reset_engine(engine)?;

        request::skip(self.pool.dwords(ring), context.place.ring_size, &built);
        let desc = {
            let regs = lrc::reg_state(self.pool.dwords(image)).ok_or(Step::Memory)?;
            reset::rewind(&mut context, regs, &built, &[])
        };
        let queued = self.time.now_us();
        if !engine.state.submit(self.bar, desc, context.place.tag) {
            return Err(Step::Submit);
        }
        let replay = self.wait(engine, &context, seqno, queued, REQUEST_PATIENCE_US);
        ferrix_rt::info!(
            "intel: M2: the hung request replayed with its batch skipped: seqno {seqno} {} ({replay:?}); context banned {}",
            if replay == Seen::Hung {
                "NOT written"
            } else {
                "written"
            },
            context.banned
        );
        if replay == Seen::Hung {
            return Err(Step::Hung);
        }
        let refused = self.submit(
            engine,
            &mut context,
            image,
            ring,
            Payload::Batch {
                address: SPINNER,
                flags: 0,
            },
        );
        ferrix_rt::info!(
            "intel: M2: a later request on the banned context: {}",
            if matches!(refused, Err(Step::Banned)) {
                "refused"
            } else {
                "NOT refused"
            }
        );
        if !matches!(refused, Err(Step::Banned)) {
            return Err(Step::Wrong);
        }
        // The engine works again: a store on a fresh context.
        self.store_dword(engine, 5)
    }
    /// A fast-copy blit onto the screen, for the checkpoint's photograph: a
    /// 256x128 pattern from the pool into the top-right corner of the
    /// firmware's lit plane, then the same rectangle blitted back into the
    /// pool and both hashed. The plane's pages are reached by mapping, in
    /// the PPGTT, the pages its GGTT entries already point at (read, never
    /// written), uncached (PAT index 3) since the display does not snoop.
    /// Only linear XRGB8888 planes are drawn on.
    fn screen_blit(&mut self, engine: &mut Engine, plane: Option<ScreenPlane>) -> Result<(), Step> {
        const W: u32 = 256;
        const H: u32 = 128;
        const VA_FB: u64 = 0x40_0000_0000;
        const VA_SRC: u64 = 0x50_0000_0000;
        const VA_BACK: u64 = 0x60_0000_0000;
        let Some(plane) = plane else {
            ferrix_rt::info!("intel: M2: no lit plane to blit onto");
            return Ok(());
        };
        if !plane.linear_xrgb8888() || plane.width < W + 32 || plane.height < H + 32 {
            ferrix_rt::warn!(
                "intel: M2: the lit plane is not linear XRGB8888 of at least {}x{} (ctl {:#010x}); no screen blit",
                W + 32,
                H + 32,
                plane.ctl
            );
            return Ok(());
        }
        let pitch = plane.pitch_bytes();
        let (x, y) = ((plane.width - W - 16) & !15, 16_u32);
        // The plane's pages from its first row to its last, mapped at VA_FB
        // plus their GGTT offset.
        let first = u64::from(plane.ggtt) + u64::from(y) * u64::from(pitch);
        let last = first + u64::from(H) * u64::from(pitch);
        let (page0, page1) = (first / PAGE as u64, last.div_ceil(PAGE as u64));
        let vm = self.vm;
        vm.alloc_va_range(
            &mut self.pool,
            VA_FB + page0 * PAGE as u64,
            (page1 - page0) * PAGE as u64,
        )
        .map_err(|_| Step::Memory)?;
        for page in page0..page1 {
            let Some(device) =
                ferrix_intel_xelp::gtt::gen8_ggtt_pte_decode(self.bar.read_pte(page))
            else {
                ferrix_rt::warn!(
                    "intel: M2: the plane's GGTT entry {page:#x} is not present; no screen blit"
                );
                return Ok(());
            };
            vm.insert_entry(&mut self.pool, device, VA_FB + page * PAGE as u64, 3)
                .map_err(|_| Step::Memory)?;
        }
        let pages = (W * H * 4) as usize / PAGE;
        let src = self.ppgtt_object(pages, VA_SRC)?;
        let back = self.ppgtt_object(pages, VA_BACK)?;
        // Eight vertical bars, each a colour, the last row white.
        for (i, word) in self.pool.dwords(src).iter_mut().enumerate() {
            let (px, py) = (i as u32 % W, i as u32 / W);
            const BARS: [u32; 8] = [
                0x00ff_0000,
                0x0000_ff00,
                0x0000_00ff,
                0x00ff_ff00,
                0x00ff_00ff,
                0x0000_ffff,
                0x00ff_ffff,
                0x0080_8080,
            ];
            *word = if py == H - 1 {
                0x00ff_ffff
            } else {
                BARS.get((px / (W / 8)) as usize % 8).copied().unwrap_or(0)
            };
        }
        let dst =
            VA_FB + u64::from(plane.ggtt) + u64::from(y) * u64::from(pitch) + u64::from(x) * 4;
        let mut copy = [0_u32; 24];
        {
            let mut cs = Cs::new(&mut copy);
            emit_rect_copy(&mut cs, dst, pitch, VA_SRC, W * 4, W, H);
            // The second blit reads what the first wrote.
            ferrix_intel_xelp::commands::gen8_emit_flush_dw(&mut cs, 0, 0, 0);
            emit_rect_copy(&mut cs, VA_BACK, W * 4, dst, pitch, W, H);
        }
        let (mut context, image, ring) = self.context(engine, 2)?;
        let (seen, took) = self.run(engine, &mut context, image, ring, Payload::Commands(&copy))?;
        let (mut src_hash, mut back_hash) = (FNV_OFFSET, FNV_OFFSET);
        let mut differ = 0;
        for i in 0..(W * H) as usize {
            let (s, b) = (self.pool.read32(src, i), self.pool.read32(back, i));
            differ += u32::from(s & 0x00ff_ffff != b & 0x00ff_ffff);
            src_hash = fnv1a(src_hash, s & 0x00ff_ffff);
            back_hash = fnv1a(back_hash, b & 0x00ff_ffff);
        }
        ferrix_rt::info!(
            "intel: M2: screen blit on {}: {W}x{H} bars at ({x}, {y}) of the lit {}x{} plane (GGTT {:#x}, pitch {pitch}), read back by a second blit: {differ} of {} pixels differ, FNV-1a pattern {src_hash:#018x} screen {back_hash:#018x} ({took} us, {seen:?}); it stays on the screen",
            engine.info.name,
            plane.width,
            plane.height,
            plane.ggtt,
            W * H
        );
        if differ == 0 {
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

/// FNV-1a's 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a over one dword's bytes, little-endian.
fn fnv1a(mut hash: u64, word: u32) -> u64 {
    for byte in word.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
