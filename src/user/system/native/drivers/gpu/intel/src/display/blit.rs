//! card0's copy on the GT's blitter (H5 of `docs/GAME-MODE.md` §5.7): each
//! flushed rectangle of the buffer shown on the lit plane copied by BCS0's
//! `XY_FAST_COPY_BLT` -- the command M2 checks at every boot -- in place of
//! the processor, so the copy takes no core and runs beside the render
//! engine.
//!
//! The card keeps a VM of its own on the render node's back end, a copy
//! context in it, and maps there:
//!
//! * the lit plane, at [`PLANE_VA`]: the pages the plane's GGTT entries
//!   point at, read from the GGTT (the plane is the firmware's; nothing of
//!   it changes), uncached, since the display engine does not snoop;
//! * each buffer it is asked to show, at [`SOURCE_VA`] plus a span per
//!   buffer slot, uncached too, since Mesa writes a shared object past the
//!   LLC (`super::evict`'s reason). A buffer of the display core's own (a
//!   compositor's dumb buffer) is pinned read-only into the device's domain
//!   by the card. A client's buffer shown directly is this driver's own GEM
//!   object, already pinned into the same domain, which the kernel refuses
//!   to pin twice: the card finds the object by the random tag in the page
//!   past its end (`crate::platform::Memory`) and maps its pinned pages.
//!
//! A flip is answered FLIPPED once its blit completes, which the serving
//! loop sees through [`Gpu::service`]; a DETACH or a mode change while one
//! runs waits for it. Anything the blitter cannot take -- a scaled mode, the
//! second scanout, a pitch the command cannot express, a buffer neither
//! pinned nor found, a refused submit -- is copied by the processor as
//! before, and a blit that hangs turns the blitter off for the boot.
//! `ferrix.intelgpu.blit=off` keeps every copy on the processor.

use ferrix_intel_xelp::commands::{Cs, emit_rect_copy_at};
use ferrix_intel_xelp::frontend::{
    Backend, Completion, ContextParams, Engine, HwContext, SubmitError, VmId,
};
use ferrix_rt::Kernel;
use ferrix_rt::native::device::Device;
use ferrix_rt::native::error::Error;
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::{DeviceAddress, Pin, PinAccess, device_address};
use ferrix_rt::native::vmo::{self, Vmo};

use crate::m2::ScreenPlane;
use crate::serve::{AsVmo, Served};

/// Where the lit plane is mapped in the card's VM.
pub(crate) const PLANE_VA: u64 = 1 << 39;
/// Where buffer slot 0 is mapped; slot `n` at `n` times [`SOURCE_SPAN`]
/// past it.
pub(crate) const SOURCE_VA: u64 = 1 << 40;
/// The VM span of one buffer slot: the largest buffer, 8192 by 8192
/// four-byte pixels.
pub(crate) const SOURCE_SPAN: u64 = 1 << 28;
/// i915's PAT index for `I915_CACHE_NONE` on Gen12: uncached.
const PAT_UC: u8 = 3;
/// A page.
const PAGE: u64 = 4096;
/// Device addresses bound at a time.
const BIND_CHUNK: usize = 256;
/// `I915_ENGINE_CLASS_COPY`.
const COPY_CLASS: u16 = 1;
/// The longest a DETACH or a mode change waits for a blit before the card
/// takes the blitter as hung: a frame's blit takes about a millisecond, and
/// the back end resets a request that stands still for `HANG_NS` (2 s),
/// which ends it as hung well within this.
pub(crate) const WAIT_NS: u64 = 3_000_000_000;

/// What card0 reaches of the GT: the render node's front end while both are
/// served, nothing ([`NoGpu`]) while card0 is served alone.
pub(crate) trait Gpu {
    /// Whether anything is reached at all.
    fn present(&self) -> bool;
    /// The device address GGTT entry `index` points at.
    fn ggtt_page(&mut self, index: u64) -> Option<u64>;
    /// A VM of the card's own.
    fn vm_create(&mut self) -> Option<VmId>;
    /// Map `pages` (device addresses) at `address` in `vm`, uncached.
    fn bind_pages(&mut self, vm: VmId, address: u64, pages: &[u64]) -> bool;
    /// Map the first `bytes` of the object whose tag page holds `tag`, and
    /// whose VMO is `vmo_bytes` long, at `address` in `vm`, uncached; the
    /// object's slot.
    fn bind_tagged(
        &mut self,
        vm: VmId,
        address: u64,
        tag: u64,
        vmo_bytes: u64,
        bytes: u64,
    ) -> Option<usize>;
    /// Whether object slot `slot` still holds the object tagged `tag`.
    fn tagged(&self, slot: usize, tag: u64) -> bool;
    /// Unmap `bytes` at `address` in `vm`.
    fn unbind(&mut self, vm: VmId, address: u64, bytes: u64);
    /// A context on BCS0 in `vm`.
    fn copy_context(&mut self, vm: VmId) -> Option<HwContext>;
    /// Run `commands` in `context`'s ring; the request's id.
    fn submit(&mut self, context: HwContext, commands: &[u32]) -> Result<u64, SubmitError>;
    /// How request `id` ended, once, if it has.
    fn completion(&mut self, id: u64) -> Option<Completion>;
    /// Let the back end run and settle what finished.
    fn service(&mut self);
}

/// Card0 served alone: no GT to reach.
pub(crate) struct NoGpu;

impl Gpu for NoGpu {
    fn present(&self) -> bool {
        false
    }
    fn ggtt_page(&mut self, _index: u64) -> Option<u64> {
        None
    }
    fn vm_create(&mut self) -> Option<VmId> {
        None
    }
    fn bind_pages(&mut self, _vm: VmId, _address: u64, _pages: &[u64]) -> bool {
        false
    }
    fn bind_tagged(&mut self, _: VmId, _: u64, _: u64, _: u64, _: u64) -> Option<usize> {
        None
    }
    fn tagged(&self, _slot: usize, _tag: u64) -> bool {
        false
    }
    fn unbind(&mut self, _vm: VmId, _address: u64, _bytes: u64) {}
    fn copy_context(&mut self, _vm: VmId) -> Option<HwContext> {
        None
    }
    fn submit(&mut self, _context: HwContext, _commands: &[u32]) -> Result<u64, SubmitError> {
        Err(SubmitError::Full)
    }
    fn completion(&mut self, _id: u64) -> Option<Completion> {
        None
    }
    fn service(&mut self) {}
}

impl<B: Backend> Gpu for Served<'_, B>
where
    B::Memory: AsVmo,
{
    fn present(&self) -> bool {
        true
    }

    fn ggtt_page(&mut self, index: u64) -> Option<u64> {
        self.frontend.backend.ggtt_page(index)
    }

    fn vm_create(&mut self) -> Option<VmId> {
        self.frontend.backend.vm_create().ok()
    }

    fn bind_pages(&mut self, vm: VmId, address: u64, pages: &[u64]) -> bool {
        self.frontend
            .backend
            .bind_pages(vm, address, pages, PAT_UC)
            .is_ok()
    }

    fn bind_tagged(
        &mut self,
        vm: VmId,
        address: u64,
        tag: u64,
        vmo_bytes: u64,
        bytes: u64,
    ) -> Option<usize> {
        let frontend = &mut *self.frontend;
        // The sizes first: the tag is a read of the object's VMO.
        let (slot, object) = frontend.state.objects.iter().find(|(_, object)| {
            object.memory.bytes() as u64 + PAGE == vmo_bytes
                && object.size >= bytes
                && object.memory.tag() == Some(tag)
        })?;
        frontend
            .backend
            .bind(vm, address, &object.memory, bytes, PAT_UC)
            .is_ok()
            .then_some(slot)
    }

    fn tagged(&self, slot: usize, tag: u64) -> bool {
        self.frontend
            .state
            .objects
            .get(slot)
            .is_some_and(|object| object.memory.tag() == Some(tag))
    }

    fn unbind(&mut self, vm: VmId, address: u64, bytes: u64) {
        self.frontend.backend.unbind(vm, address, bytes);
    }

    fn copy_context(&mut self, vm: VmId) -> Option<HwContext> {
        let engine = Engine {
            class: COPY_CLASS,
            instance: 0,
        };
        self.frontend
            .backend
            .context_create(vm, engine, ContextParams::DEFAULT)
            .ok()
    }

    fn submit(&mut self, context: HwContext, commands: &[u32]) -> Result<u64, SubmitError> {
        self.frontend.driver_submit(context, commands)
    }

    fn completion(&mut self, id: u64) -> Option<Completion> {
        self.frontend.driver_completion(id)
    }

    fn service(&mut self) {
        self.frontend.service(self.host, false);
    }
}

/// How a buffer slot's pixels reach the blitter.
#[derive(Default)]
pub(crate) enum Source {
    /// Not looked at yet.
    #[default]
    Untried,
    /// Copied by the processor: the blitter cannot take it.
    Processor,
    /// A display core's buffer, pinned read-only by the card.
    Pinned { _pin: Pin<Kernel>, bytes: u64 },
    /// A GEM object of this driver's, by its tag, in front-end slot
    /// `object`.
    Object { object: usize, tag: u64, bytes: u64 },
}

impl Source {
    /// The bytes mapped for it, if any.
    fn bound(&self) -> Option<u64> {
        match self {
            Source::Pinned { bytes, .. } | Source::Object { bytes, .. } => Some(*bytes),
            Source::Untried | Source::Processor => None,
        }
    }
}

/// A blit running, and the FLUSH it answers.
#[derive(Clone, Copy)]
pub(crate) struct Inflight {
    pub(crate) request: u64,
    /// The buffer slot it reads.
    pub(crate) slot: usize,
    pub(crate) sequence: u64,
    /// When its FLUSH came.
    pub(crate) at: u64,
    /// When it was submitted.
    pub(crate) started: u64,
    pub(crate) torn: bool,
}

/// The card's blitter, once made.
pub(crate) struct Blitter<'d> {
    vm: VmId,
    context: HwContext,
    pitch: u32,
    /// How each buffer slot reaches it.
    pub(crate) sources: [Source; super::MAX_BUFFERS],
    pub(crate) inflight: Option<Inflight>,
    /// The driver's device, to pin buffers with (its handle has no
    /// `DUPLICATE`, so it is borrowed).
    device: &'d Device<Kernel>,
}

/// Where the blitter stands.
#[allow(
    clippy::large_enum_variant,
    reason = "one a card, never moved after it is made; no allocator to box it"
)]
pub(crate) enum State<'d> {
    /// Every copy on the processor: asked, card0 alone, or the blitter
    /// could not be made or hung.
    Off,
    /// Made at the first flush the render node is served beside.
    Unmade {
        plane: ScreenPlane,
        device: &'d Device<Kernel>,
    },
    On(Blitter<'d>),
}

impl<'d> State<'d> {
    /// The blitter for `plane`, unless `ferrix.intelgpu.blit=off`.
    pub(crate) fn new(plane: ScreenPlane, device: &'d Device<Kernel>) -> State<'d> {
        if blit_off() {
            ferrix_rt::info!("intel: card0: copies on the processor (ferrix.intelgpu.blit=off)");
            return State::Off;
        }
        State::Unmade { plane, device }
    }

    /// The blitter, made now if it is not yet; `None` when it cannot be.
    pub(crate) fn get(&mut self, gpu: &mut dyn Gpu) -> Option<&mut Blitter<'d>> {
        if matches!(self, State::Unmade { .. }) {
            if !gpu.present() {
                return None;
            }
            let State::Unmade { plane, device } = core::mem::replace(self, State::Off) else {
                return None;
            };
            match Blitter::make(plane, device, gpu) {
                Ok(blitter) => {
                    ferrix_rt::info!(
                        "intel: card0: frames copied by BCS0's XY_FAST_COPY_BLT into the lit {}x{} plane",
                        plane.width,
                        plane.height
                    );
                    *self = State::On(blitter);
                }
                Err(why) => {
                    ferrix_rt::warn!("intel: card0: no blitter ({why}); copies on the processor");
                }
            }
        }
        match self {
            State::On(blitter) => Some(blitter),
            State::Off | State::Unmade { .. } => None,
        }
    }

    /// The blitter if it is made, without making it.
    pub(crate) fn made(&mut self) -> Option<&mut Blitter<'d>> {
        match self {
            State::On(blitter) => Some(blitter),
            State::Off | State::Unmade { .. } => None,
        }
    }
}

/// `ferrix.intelgpu.blit=off`.
fn blit_off() -> bool {
    let mut line = [0_u8; 4096];
    crate::steps::read_cmdline(&mut line).is_some_and(|length| {
        line.get(..length)
            .unwrap_or_default()
            .split(u8::is_ascii_whitespace)
            .any(|word| word == b"ferrix.intelgpu.blit=off")
    })
}

impl<'d> Blitter<'d> {
    /// A VM with the plane mapped, and a copy context in it.
    fn make(
        plane: ScreenPlane,
        device: &'d Device<Kernel>,
        gpu: &mut dyn Gpu,
    ) -> Result<Blitter<'d>, &'static str> {
        let pitch = plane.pitch_bytes();
        if !pitch.is_multiple_of(64) || pitch > 0xffff {
            return Err("the plane's pitch is not one the blitter takes");
        }
        let vm = gpu.vm_create().ok_or("no VM")?;
        let first = u64::from(plane.ggtt) / PAGE;
        let pages = (u64::from(pitch) * u64::from(plane.height)).div_ceil(PAGE);
        let mut chunk = [0_u64; BIND_CHUNK];
        let mut done = 0;
        while done < pages {
            let count = (pages - done).min(BIND_CHUNK as u64) as usize;
            for (i, slot) in chunk.iter_mut().take(count).enumerate() {
                *slot = gpu
                    .ggtt_page(first + done + i as u64)
                    .ok_or("a GGTT entry of the plane is not present")?;
            }
            if !gpu.bind_pages(
                vm,
                PLANE_VA + done * PAGE,
                chunk.get(..count).unwrap_or_default(),
            ) {
                return Err("the plane's pages could not be mapped");
            }
            done += count as u64;
        }
        let context = gpu.copy_context(vm).ok_or("no copy context")?;
        Ok(Blitter {
            vm,
            context,
            pitch,
            sources: Default::default(),
            inflight: None,
            device,
        })
    }

    /// Make slot `slot` (`bytes` of `vmo` from `offset`) reachable, if it is
    /// not yet: pinned, or found by its tag. Whether it is.
    pub(crate) fn prepare(
        &mut self,
        slot: usize,
        vmo: &Vmo<Kernel>,
        offset: u64,
        bytes: u64,
        stride: usize,
        gpu: &mut dyn Gpu,
    ) -> bool {
        let Some(source) = self.sources.get(slot) else {
            return false;
        };
        match source {
            Source::Processor => return false,
            Source::Object { object, tag, .. } => return gpu.tagged(*object, *tag),
            Source::Pinned { .. } => return true,
            Source::Untried => {}
        }
        let made = self.reach(slot, vmo, offset, bytes, stride, gpu);
        if let Some(entry) = self.sources.get_mut(slot) {
            *entry = made;
        }
        self.sources.get(slot).is_some_and(|s| s.bound().is_some())
    }

    fn reach(
        &mut self,
        slot: usize,
        vmo: &Vmo<Kernel>,
        offset: u64,
        bytes: u64,
        stride: usize,
        gpu: &mut dyn Gpu,
    ) -> Source {
        if !stride.is_multiple_of(64) || stride > 0xffff || !bytes.is_multiple_of(PAGE) {
            return Source::Processor;
        }
        let va = SOURCE_VA + slot as u64 * SOURCE_SPAN;
        let (Ok(length), Ok(start)) = (usize::try_from(bytes), usize::try_from(offset)) else {
            return Source::Processor;
        };
        match self.device.pin(vmo, start, length, PinAccess::ReadOnly) {
            Ok(pin) => {
                if bind_pin(&pin, length, self.vm, va, gpu) {
                    return Source::Pinned { _pin: pin, bytes };
                }
                gpu.unbind(self.vm, va, bytes);
                Source::Processor
            }
            // Already pinned into this domain: one of this driver's own
            // objects, found by its tag.
            Err(Error::AlreadyBound) if offset == 0 => {
                let Ok(vmo_bytes) = vmo.size() else {
                    return Source::Processor;
                };
                if vmo_bytes < bytes + PAGE {
                    return Source::Processor;
                }
                let mut tag = [0_u8; 8];
                if vmo.read(&mut tag, vmo_bytes - PAGE).is_err() {
                    return Source::Processor;
                }
                let tag = u64::from_ne_bytes(tag);
                if tag == 0 {
                    return Source::Processor;
                }
                match gpu.bind_tagged(self.vm, va, tag, vmo_bytes, bytes) {
                    Some(object) => Source::Object { object, tag, bytes },
                    None => Source::Processor,
                }
            }
            Err(_) => Source::Processor,
        }
    }

    /// Whether slot `slot`'s pixels may sit written in the processor's
    /// caches: a display core's buffer, drawn by a compositor's processor,
    /// which the blitter reads uncached past them. A GEM object is the
    /// GPU's, written past the LLC already.
    pub(crate) fn written_by_processor(&self, slot: usize) -> bool {
        matches!(self.sources.get(slot), Some(Source::Pinned { .. }))
    }

    /// Submit the blit of `area` of slot `slot` (its rows `stride` bytes)
    /// into the plane; the request's id.
    pub(crate) fn submit(
        &mut self,
        slot: usize,
        stride: usize,
        area: &super::Area,
        gpu: &mut dyn Gpu,
    ) -> Result<u64, SubmitError> {
        let source = SOURCE_VA + slot as u64 * SOURCE_SPAN;
        let coordinate = |value: usize| u32::try_from(value).ok().filter(|&v| v < 0x8000);
        let (Some(sx), Some(sy), Some(dx), Some(dy), Some(width), Some(height)) = (
            coordinate(area.source_x),
            coordinate(area.source_y),
            coordinate(area.screen_x),
            coordinate(area.screen_y),
            coordinate(area.columns),
            coordinate(area.rows),
        ) else {
            return Err(SubmitError::Failed(ferrix_linux_abi::errno::Errno::EINVAL));
        };
        let mut commands = [0_u32; 10];
        {
            let mut cs = Cs::new(&mut commands);
            emit_rect_copy_at(
                &mut cs,
                PLANE_VA,
                self.pitch,
                (dx, dy),
                source,
                stride as u32,
                (sx, sy),
                (width, height),
            );
        }
        gpu.submit(self.context, &commands)
    }

    /// Let go of slot `slot`'s mapping and pin. The caller made sure no
    /// blit reading it runs.
    pub(crate) fn forget(&mut self, slot: usize, gpu: &mut dyn Gpu) {
        let Some(source) = self.sources.get_mut(slot) else {
            return;
        };
        if let Some(bytes) = source.bound() {
            gpu.unbind(self.vm, SOURCE_VA + slot as u64 * SOURCE_SPAN, bytes);
        }
        *source = Source::Untried;
    }

    /// Wait, servicing the back end, for the running blit; how it ended,
    /// `None` when it did not within [`WAIT_NS`].
    pub(crate) fn wait(&mut self, gpu: &mut dyn Gpu) -> Option<(Inflight, Completion)> {
        let inflight = self.inflight?;
        let give_up = super::now_ns().saturating_add(WAIT_NS);
        loop {
            gpu.service();
            if let Some(how) = gpu.completion(inflight.request) {
                self.inflight = None;
                return Some((inflight, how));
            }
            if super::now_ns() >= give_up {
                return None;
            }
            core::hint::spin_loop();
        }
    }
}

/// Bind `pin`'s pages (`length` bytes) at `va` in `vm`, reading their
/// device addresses through a VMO of their own: no allocator here, and a
/// screen's buffer has thousands of pages.
fn bind_pin(pin: &Pin<Kernel>, length: usize, vm: VmId, va: u64, gpu: &mut dyn Gpu) -> bool {
    let pages = length / PAGE as usize;
    let list = (pages * 8).next_multiple_of(PAGE as usize);
    let Ok(addresses) = vmo::create(Kernel, list) else {
        return false;
    };
    let Ok(map) = addresses.map(None, list, Protection::ReadWrite, 0) else {
        return false;
    };
    // SAFETY: `map` is this process's own read-write mapping of `list`
    // bytes, which holds `pages` eight-byte entries, and nothing else holds
    // a slice of it; it is unmapped below before `addresses` is dropped.
    let out = unsafe { core::slice::from_raw_parts_mut(map as *mut DeviceAddress, pages) };
    let complete = pin
        .addresses(out)
        .is_ok_and(|got| got.is_complete() && got.pages == pages);
    let mut bound = complete;
    let mut chunk = [0_u64; BIND_CHUNK];
    let mut done = 0;
    while bound && done < pages {
        let count = (pages - done).min(BIND_CHUNK);
        for (slot, bytes) in chunk.iter_mut().zip(out.iter().skip(done).take(count)) {
            *slot = device_address(*bytes);
        }
        bound = gpu.bind_pages(
            vm,
            va + done as u64 * PAGE,
            chunk.get(..count).unwrap_or_default(),
        );
        done += count;
    }
    let _ = ferrix_rt::linux::munmap(map, list);
    bound
}
