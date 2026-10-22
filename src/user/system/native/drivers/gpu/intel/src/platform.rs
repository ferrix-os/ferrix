//! The real GPU under `ferrix-intel-xelp`'s back end (`backend::Platform`):
//! BAR 0's registers and GGTT entries, time, page tables in a pinned pool,
//! the driver's own objects (rings, context images, status pages) each a
//! pinned VMO bound into the driver's GGTT window, and the client's objects
//! as VMOs pinned into 00:02.0's domain at their first bind.
//!
//! Nothing here allocates: every table has a fixed size, and a full one is
//! `ENOMEM` to the program that asked.

use core::cell::OnceCell;
use core::ptr;

use ferrix_intel_xelp::backend::{Error, FrequencyReport, KernelObject, Platform};
use ferrix_intel_xelp::ggtt_space::{GgttSpace, Range};
use ferrix_intel_xelp::io::{Clock, Mmio};
use ferrix_intel_xelp::ppgtt::PtMemory;
use ferrix_intel_xelp::pt_pool::{CHUNK_PAGES, ChunkStore, PtPool};
use ferrix_intel_xelp::uncore::{AutoForcewake, ForcewakeDomains, FwDomains};
use ferrix_rt::Kernel;
use ferrix_rt::native::device::{Device, Interrupt};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::{DeviceAddress, Pin, PinAccess, device_address};
use ferrix_rt::native::vmo::{self, Vmo};

use crate::serve::AsVmo;
use crate::{Bar0, Time};

/// Page-table chunks at most, 4 MiB each (`pt_pool::CHUNK_PAGES`): 64 of
/// them hold the tables for about 64 GiB bound at once, every VM's
/// together. The first is made at start, the others as binds need them.
const TABLE_CHUNKS: usize = 64;
/// The driver's own objects at once: two per context on two engines, the
/// status pages and the default images.
const MAX_OBJECTS: usize = 256;
/// The largest of them, in pages: a 2 MiB ring.
const MAX_OBJECT_PAGES: usize = 512;
/// Bytes in a page.
const PAGE: usize = 4096;

/// One of the driver's own objects.
struct Object {
    _pin: Pin<Kernel>,
    _vmo: Vmo<Kernel>,
    base: usize,
    pages: usize,
    range: Range,
}

/// A client's object: a VMO, pinned into 00:02.0's domain at its first
/// bind, its pages' device addresses kept in a VMO of their own.
pub(crate) struct Memory {
    vmo: Vmo<Kernel>,
    pages: usize,
    pinned: OnceCell<Pinned>,
}

struct Pinned {
    pin: Pin<Kernel>,
    _addresses: Vmo<Kernel>,
    map: usize,
    len: usize,
}

impl Drop for Pinned {
    fn drop(&mut self) {
        let _ = ferrix_rt::linux::munmap(self.map, self.len);
    }
}

impl AsVmo for Memory {
    fn vmo(&self) -> &Vmo<Kernel> {
        &self.vmo
    }
}

/// The real platform.
pub(crate) struct Real<'a> {
    bar: Bar0,
    time: Time,
    device: &'a Device<Kernel>,
    tables: PtPool<Chunks<'a>, TABLE_CHUNKS>,
    ggtt: GgttSpace,
    objects: [Option<Object>; MAX_OBJECTS],
    interrupt: Option<Interrupt<Kernel>>,
    /// Forcewake per access, once [`Real::forcewake_per_access`] handed the
    /// bring-up's hold over; until then the process holds it.
    forcewake: Option<AutoForcewake>,
    /// Forcewake failures already said.
    forcewake_said: u32,
}

impl<'a> Real<'a> {
    /// The platform over BAR 0, time, the device and the GGTT window
    /// `ggtt` above everything the firmware left. `interrupt` is bound to the
    /// serving loop's port already, if there is one.
    pub(crate) fn new(
        bar: Bar0,
        time: Time,
        device: &'a Device<Kernel>,
        ggtt: GgttSpace,
        interrupt: Option<Interrupt<Kernel>>,
    ) -> Result<Self, crate::Step> {
        Ok(Real {
            bar,
            time,
            device,
            tables: PtPool::new(Chunks {
                device,
                chunks: [const { None }; TABLE_CHUNKS],
            })
            .ok_or(crate::Step::Memory)?,
            ggtt,
            objects: [const { None }; MAX_OBJECTS],
            interrupt,
            forcewake: None,
            forcewake_said: 0,
        })
    }

    /// BAR 0, for the forcewake the process holds around the back end.
    pub(crate) fn bar(&mut self) -> &mut Bar0 {
        &mut self.bar
    }

    /// From here on, forcewake per access over `domains` as i915 takes it,
    /// `held` (the bring-up's) handed over at `now_ns` and released once
    /// unused like the rest.
    pub(crate) fn forcewake_per_access(
        &mut self,
        domains: FwDomains,
        held: ForcewakeDomains,
        now_ns: u64,
    ) {
        self.forcewake = Some(AutoForcewake::new(domains, held, now_ns));
    }

    fn object(&self, id: u32) -> Option<&Object> {
        self.objects.get(id as usize).and_then(Option::as_ref)
    }

    fn at(&self, object: KernelObject, index: usize) -> Option<usize> {
        let o = self.object(object.id)?;
        (index < o.pages * PAGE / 4).then(|| o.base + index * 4)
    }

    /// Pin `memory` and keep its pages' addresses.
    fn pin(&self, memory: &Memory) -> Result<(), Error> {
        let bytes = memory.pages * PAGE;
        let pin = self
            .device
            .pin(&memory.vmo, 0, bytes, PinAccess::ReadWrite)
            .map_err(pin_error)?;
        let len = (memory.pages * 8).next_multiple_of(PAGE);
        let addresses = vmo::create(Kernel, len).map_err(|_| Error::NoMemory)?;
        let map = addresses
            .map(None, len, Protection::ReadWrite, 0)
            .map_err(|_| Error::NoMemory)?;
        let pinned = Pinned {
            pin,
            _addresses: addresses,
            map,
            len,
        };
        // SAFETY: `map` is this process's own read-write mapping of `len`
        // bytes, which `pinned` keeps until it is dropped; eight-byte
        // entries, `memory.pages` of them, fit in it, and nothing else holds
        // a slice of it.
        let out =
            unsafe { core::slice::from_raw_parts_mut(map as *mut DeviceAddress, memory.pages) };
        let got = pinned.pin.addresses(out).map_err(|_| Error::NoMemory)?;
        if !got.is_complete() {
            return Err(Error::NoMemory);
        }
        let _ = memory.pinned.set(pinned);
        Ok(())
    }
}

/// A pin's refusal toward the program: the iGPU's pin budget (K3) is
/// `EDQUOT`, everything else no memory.
fn pin_error(error: ferrix_rt::native::error::Error) -> Error {
    use ferrix_rt::native::error::Error as E;
    match error {
        E::LimitReached | E::AccessDenied => Error::Quota,
        _ => Error::NoMemory,
    }
}

impl Mmio for Real<'_> {
    fn read32(&mut self, offset: u32) -> u32 {
        if let Some(forcewake) = self.forcewake.as_mut() {
            forcewake.access(&mut self.bar, &mut self.time, offset, false);
        }
        self.bar.read32(offset)
    }

    fn write32(&mut self, offset: u32, value: u32) {
        if let Some(forcewake) = self.forcewake.as_mut() {
            forcewake.access(&mut self.bar, &mut self.time, offset, true);
        }
        self.bar.write32(offset, value);
    }
}

impl Clock for Real<'_> {
    fn now_us(&mut self) -> u64 {
        self.time.now_us()
    }

    fn delay_us(&mut self, us: u32) {
        self.time.delay_us(us);
    }
}

/// One chunk of page-table pages: pinned into the device's domain and
/// mapped write-back here.
struct ChunkMemory {
    _pin: Pin<Kernel>,
    _vmo: Vmo<Kernel>,
    base: usize,
}

/// The page-table chunks' memory.
pub(crate) struct Chunks<'a> {
    device: &'a Device<Kernel>,
    chunks: [Option<ChunkMemory>; TABLE_CHUNKS],
}

impl Chunks<'_> {
    /// The address of word `word` of page `page` of chunk `chunk`.
    fn at(&self, chunk: usize, page: usize, word: usize) -> Option<usize> {
        let memory = self.chunks.get(chunk)?.as_ref()?;
        (page < CHUNK_PAGES && word < 512).then(|| memory.base + page * PAGE + word * 8)
    }
}

impl ChunkStore for Chunks<'_> {
    fn grow(&mut self, chunk: usize, device: &mut [u64; CHUNK_PAGES]) -> bool {
        let Some(cell) = self.chunks.get_mut(chunk) else {
            return false;
        };
        let bytes = CHUNK_PAGES * PAGE;
        let Ok(vmo) = vmo::create(Kernel, bytes) else {
            return false;
        };
        let Ok(pin) = self.device.pin(&vmo, 0, bytes, PinAccess::ReadWrite) else {
            return false;
        };
        let mut raw = [[0_u8; 8]; CHUNK_PAGES];
        let Ok(got) = pin.addresses(&mut raw) else {
            return false;
        };
        if !got.is_complete() || got.pages != CHUNK_PAGES {
            return false;
        }
        for (slot, bytes) in device.iter_mut().zip(raw) {
            *slot = device_address(bytes);
        }
        let Ok(base) = vmo.map(None, bytes, Protection::ReadWrite, 0) else {
            return false;
        };
        *cell = Some(ChunkMemory {
            _pin: pin,
            _vmo: vmo,
            base,
        });
        true
    }

    fn read(&mut self, chunk: usize, page: usize, word: usize) -> u64 {
        let Some(address) = self.at(chunk, page, word) else {
            return 0;
        };
        // SAFETY: inside chunk `chunk`'s mapping (`at` checked the page and
        // word), which this process made read-write and `ChunkMemory`
        // keeps; eight-byte aligned; volatile, as the GPU reads it.
        unsafe { ptr::read_volatile(address as *const u64) }
    }

    fn write(&mut self, chunk: usize, page: usize, word: usize, value: u64) {
        if let Some(address) = self.at(chunk, page, word) {
            // SAFETY: as for `read`; one store, so the GPU's walk never sees
            // half an entry. The GPU snoops the LLC on this part, so no
            // clflush is needed (i915's `drm_clflush_virt_range` is a no-op
            // on LLC for coherent PT objects).
            unsafe { ptr::write_volatile(address as *mut u64, value) }
        }
    }
}

impl PtMemory for Real<'_> {
    fn alloc_pt(&mut self) -> Option<u64> {
        self.tables.alloc_pt()
    }

    fn free_pt(&mut self, table: u64) {
        self.tables.free_pt(table);
    }

    fn read_entry(&mut self, table: u64, index: usize) -> u64 {
        self.tables.read_entry(table, index)
    }

    fn write_entry(&mut self, table: u64, index: usize, value: u64) {
        self.tables.write_entry(table, index, value);
    }
}

impl Platform for Real<'_> {
    type Memory = Memory;

    fn create_memory(&mut self, bytes: u64) -> Result<Memory, Error> {
        let bytes = usize::try_from(bytes).map_err(|_| Error::NoMemory)?;
        if bytes == 0 || !bytes.is_multiple_of(PAGE) {
            return Err(Error::Invalid);
        }
        let vmo = vmo::create(Kernel, bytes).map_err(|_| Error::NoMemory)?;
        Ok(Memory {
            vmo,
            pages: bytes / PAGE,
            pinned: OnceCell::new(),
        })
    }

    fn memory_page(&mut self, memory: &Memory, index: u64) -> Result<u64, Error> {
        let index = usize::try_from(index).map_err(|_| Error::Invalid)?;
        if index >= memory.pages {
            return Err(Error::Invalid);
        }
        if memory.pinned.get().is_none() {
            self.pin(memory)?;
        }
        let pinned = memory.pinned.get().ok_or(Error::NoMemory)?;
        // SAFETY: inside the address mapping `pinned` keeps (index is below
        // `memory.pages`, each entry eight bytes), written by the kernel
        // before `pin` returned.
        let bytes = unsafe { ptr::read_volatile((pinned.map + index * 8) as *const DeviceAddress) };
        Ok(device_address(bytes))
    }

    fn kernel_object(&mut self, pages: u32) -> Result<KernelObject, Error> {
        let pages = pages as usize;
        if pages == 0 || pages > MAX_OBJECT_PAGES {
            return Err(Error::Invalid);
        }
        let id = self
            .objects
            .iter()
            .position(Option::is_none)
            .ok_or(Error::NoMemory)?;
        let bytes = pages * PAGE;
        let vmo = vmo::create(Kernel, bytes).map_err(|_| Error::NoMemory)?;
        let pin = self
            .device
            .pin(&vmo, 0, bytes, PinAccess::ReadWrite)
            .map_err(pin_error)?;
        let mut raw = [[0_u8; 8]; MAX_OBJECT_PAGES];
        let out = raw.get_mut(..pages).ok_or(Error::Invalid)?;
        let got = pin.addresses(out).map_err(|_| Error::NoMemory)?;
        if !got.is_complete() {
            return Err(Error::NoMemory);
        }
        let mut addresses = [0_u64; MAX_OBJECT_PAGES];
        for (slot, bytes) in addresses.iter_mut().zip(raw.iter().take(pages)) {
            *slot = device_address(*bytes);
        }
        let base = vmo
            .map(None, bytes, Protection::ReadWrite, 0)
            .map_err(|_| Error::NoMemory)?;
        let range = self
            .ggtt
            .alloc(bytes as u64, PAGE as u64)
            .map_err(|_| Error::NoMemory)?;
        let bound = self.ggtt.bind(
            &mut self.bar,
            range,
            addresses.get(..pages).unwrap_or_default(),
        );
        if bound.is_err() {
            let _ = self.ggtt.free(range);
            let _ = ferrix_rt::linux::munmap(base, bytes);
            return Err(Error::NoMemory);
        }
        if let Some(slot) = self.objects.get_mut(id) {
            *slot = Some(Object {
                _pin: pin,
                _vmo: vmo,
                base,
                pages,
                range,
            });
        }
        let object = KernelObject {
            id: id as u32,
            pages: pages as u32,
            ggtt: range.start as u32,
        };
        self.words(object).fill(0);
        Ok(object)
    }

    fn free_kernel_object(&mut self, object: KernelObject) {
        let Some(gone) = self
            .objects
            .get_mut(object.id as usize)
            .and_then(Option::take)
        else {
            return;
        };
        // Not present again: the GPU never reaches the freed pages.
        let _ = self.ggtt.clear(&mut self.bar, gone.range, 0);
        let _ = self.ggtt.free(gone.range);
        let _ = ferrix_rt::linux::munmap(gone.base, gone.pages * PAGE);
    }

    fn words(&mut self, object: KernelObject) -> &mut [u32] {
        let Some(o) = self.object(object.id) else {
            return &mut [];
        };
        let (base, len) = (o.base, o.pages * PAGE / 4);
        // SAFETY: the object's own read-write mapping, which `self.objects`
        // keeps while it is allocated; the `&mut self` borrow makes this the
        // only slice of any object alive.
        unsafe { core::slice::from_raw_parts_mut(base as *mut u32, len) }
    }

    fn read_object(&mut self, object: KernelObject, index: usize) -> u32 {
        match self.at(object, index) {
            Some(address) => {
                // The line out of this processor's caches first, so the read
                // is of what the GPU wrote (see `arch::flush_line`).
                crate::arch::flush_line(address);
                // SAFETY: inside the object's mapping, aligned; volatile,
                // since the GPU writes it.
                unsafe { ptr::read_volatile(address as *const u32) }
            }
            None => 0,
        }
    }

    fn write_object(&mut self, object: KernelObject, index: usize, value: u32) {
        if let Some(address) = self.at(object, index) {
            // SAFETY: as for `read_object`.
            unsafe { ptr::write_volatile(address as *mut u32, value) }
            // Written back and out of the caches, so a dirty line never
            // lands later over what the GPU wrote beside it (the CSB's
            // consumed entries share a line with its write pointer).
            crate::arch::flush_line(address);
        }
    }

    fn interrupt_serviced(&mut self) {
        if let Some(interrupt) = &self.interrupt {
            let _ = interrupt.ack();
        }
    }

    fn frequency_report(&mut self, report: &FrequencyReport) {
        let sample = &report.sample;
        crate::say(format_args!(
            "intel: freqlog: asked {} MHz{}, actual {} MHz (RPSTAT1 {:#010x}), limits {}, busy {}.{} %, RC6 {}.{} %, package {}.{:03} W",
            sample.requested_mhz,
            if report.pinned {
                " (pinned)"
            } else if report.parked {
                " (parked)"
            } else {
                ""
            },
            sample.actual_mhz,
            sample.rpstat,
            crate::Reasons(sample.limit_reasons),
            report.busy_permille / 10,
            report.busy_permille % 10,
            report.rc6_permille / 10,
            report.rc6_permille % 10,
            report.milliwatts / 1000,
            report.milliwatts % 1000
        ));
    }

    fn forcewake_release(&mut self, now_ns: u64) {
        let Some(forcewake) = self.forcewake.as_mut() else {
            return;
        };
        forcewake.release(&mut self.bar, now_ns);
        if forcewake.failures != self.forcewake_said {
            self.forcewake_said = forcewake.failures;
            crate::say(format_args!(
                "intel: forcewake: {} wakes failed so far, the last {:?}",
                forcewake.failures, forcewake.last_error
            ));
        }
    }

    fn forcewake_due(&self) -> Option<u64> {
        self.forcewake.as_ref().and_then(AutoForcewake::due)
    }
}
