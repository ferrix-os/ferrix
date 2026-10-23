//! The second pipe's own surface (`docs/DESKTOP.md` §3.5): memory the
//! driver pins into 00:02.0's domain, entered into the GGTT below the
//! driver's own window where no plane scans out (the firmware keeps entries
//! far past its lit framebuffer), and written through the driver's own
//! mapping of it.
//! BAR 2's aperture is not lent to the driver (the box lends BAR 0 alone),
//! and the display engine does not read through the processor's caches,
//! so each line the copy writes is flushed to memory after it
//! ([`crate::arch::flush_range`]), as i915 flushes a scanout buffer the
//! processor drew into (`drm_clflush_virt_range`).
//!
//! [`light::SURFACE_BYTES`] at [`light::SURFACE_PITCH`], whatever the mode:
//! a monitor plugged in later, or a smaller mode, is shown from the same
//! memory. The GGTT entries are written from this process through a
//! mapping of their own, each as one 64-bit store (`gen8_set_pte`); on
//! Gen11+ no flush follows (`gtt::GTTADR_OFFSET`).

use core::ptr;

use ferrix_intel_xelp::display::light::{self, SURFACE_BYTES};
use ferrix_intel_xelp::gtt::{GTT_PAGE_SIZE, GTTADR_OFFSET, gen8_ggtt_pte_encode};
use ferrix_native_abi::types::IoMappingSpec;
use ferrix_rt::Kernel;
use ferrix_rt::native::device::Device;
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::{DeviceAddress, Pin, PinAccess, device_address};
use ferrix_rt::native::vmo::{self, Vmo};

use super::regs::Regs;
use crate::say;

/// Pages pinned at a time: one GGTT page of entries.
const CHUNK: usize = 512;
/// Chunks the surface takes at most: 16 MiB.
const CHUNKS: usize = 8;

/// The surface: its memory, kept pinned, and where the driver writes it.
pub(crate) struct Surface {
    _vmo: Vmo<Kernel>,
    _pins: [Option<Pin<Kernel>>; CHUNKS],
    /// Where it is mapped, cached, in this process: every write is
    /// flushed.
    pub(crate) base: usize,
    /// Its bytes.
    pub(crate) len: usize,
    /// Its GGTT offset, `PLANE_SURF`'s value.
    pub(crate) ggtt: u32,
}

impl Surface {
    /// Make it, cleared to black, in a GGTT range below the driver's own
    /// window that no plane scans out: a free one where the firmware left
    /// one, else one whose firmware entries are taken over
    /// ([`light::place`]). `None`, said, when any part is refused: the
    /// second scanout is then there but not lit.
    pub(crate) fn make(
        _asked: super::modeset::Written,
        device: &Device<Kernel>,
        apertures: u32,
        regs: &mut Regs,
    ) -> Option<Self> {
        let bar0 = (0..apertures as usize)
            .filter_map(|index| device.aperture(index).ok())
            .find(|aperture| aperture.bar == 0 && aperture.offset == 0);
        let Some(bar0) = bar0 else {
            say(format_args!(
                "intel: card0: no second surface: BAR 0 is not lent"
            ));
            return None;
        };
        // The entries below the driver's window: 64 Ki of them, 512 KiB.
        let below = light::SURFACE_GGTT_LIMIT / GTT_PAGE_SIZE;
        let entries = device
            .io_mapping(IoMappingSpec {
                phys: bar0.phys + GTTADR_OFFSET,
                len: below * 8,
            })
            .inspect_err(|error| {
                say(format_args!(
                    "intel: card0: no second surface: the GGTT's entries refused ({error:?})"
                ));
            })
            .ok()?;
        let table = entries.map(None).ok()?;
        let placed = place(table, below, regs)?;
        let at = placed.placed.at;
        let planes = placed.planes;
        say(format_args!(
            "intel: card0: the second surface at GGTT {at:#x}..{:#x}, {} ({planes} planes scanned out elsewhere)",
            at + SURFACE_BYTES,
            if placed.placed.reclaimed {
                "over firmware entries no plane scans out"
            } else {
                "in entries the firmware left free"
            }
        ));
        let len = usize::try_from(SURFACE_BYTES).ok()?;
        let pages = len.div_ceil(GTT_PAGE_SIZE as usize);
        let first = at / GTT_PAGE_SIZE;
        let memory = vmo::create(Kernel, len).ok()?;
        let mut pins: [Option<Pin<Kernel>>; CHUNKS] = Default::default();
        let mut addresses: [DeviceAddress; CHUNK] = [[0; 8]; CHUNK];
        for (chunk, slot) in pins.iter_mut().enumerate() {
            let start = chunk * CHUNK;
            if start >= pages {
                break;
            }
            let count = CHUNK.min(pages - start);
            let pin = device
                .pin(&memory, start * 4096, count * 4096, PinAccess::ReadWrite)
                .inspect_err(|error| {
                    say(format_args!(
                        "intel: card0: no second surface: {count} pages not pinned ({error:?})"
                    ));
                })
                .ok()?;
            let got = pin.addresses(&mut addresses).ok()?;
            if got.written < count {
                return None;
            }
            for (page, address) in addresses.iter().take(count).enumerate() {
                let pte = gen8_ggtt_pte_encode(device_address(*address));
                // SAFETY: inside the entries' mapping (`pages` entries),
                // eight-byte aligned; one 64-bit store, as `gen8_set_pte`.
                unsafe {
                    ptr::write_volatile(
                        (table + (first as usize + start + page) * 8) as *mut u64,
                        pte,
                    );
                };
            }
            *slot = Some(pin);
        }
        drop(entries);
        let ggtt = u32::try_from(at).ok()?;
        let base = memory.map(None, len, Protection::ReadWrite, 0).ok()?;
        // Black, flushed to memory.
        for word in 0..len / 8 {
            // SAFETY: inside the VMO's mapping just made, eight-byte aligned.
            unsafe { ptr::write_volatile((base + word * 8) as *mut u64, 0) };
        }
        crate::arch::flush_range(base, len);
        say(format_args!(
            "intel: card0: a second surface, {len:#x} bytes at GGTT {ggtt:#x} ({pages} pages pinned)"
        ));
        Some(Self {
            _vmo: memory,
            _pins: pins,
            base,
            len,
            ggtt,
        })
    }
}

/// Where the surface goes: the GGTT's first `below` entries (mapped at
/// `table`) read for the ones present, and every plane's scanned range
/// kept clear.
fn place(table: usize, below: u64, regs: &mut Regs) -> Option<Placement> {
    let mut present = |page: u64| {
        page < below
            // SAFETY: inside the entries' mapping (`below` entries),
            // eight-byte aligned.
            && unsafe { ptr::read_volatile((table + page as usize * 8) as *const u64) } & 1 != 0
    };
    let mut busy = [(0, 0); 24];
    let planes = light::scanned(regs, &mut busy);
    let Some(placed) = light::place(
        &mut present,
        busy.get(..planes).unwrap_or_default(),
        light::SURFACE_GGTT_LIMIT,
    ) else {
        say(format_args!(
            "intel: card0: no second surface: no {SURFACE_BYTES:#x} bytes below GGTT {:#x} that no plane scans out",
            light::SURFACE_GGTT_LIMIT
        ));
        return None;
    };
    Some(Placement { placed, planes })
}

/// [`light::place`]'s answer and how many planes it kept clear of.
struct Placement {
    placed: light::Placed,
    planes: usize,
}
