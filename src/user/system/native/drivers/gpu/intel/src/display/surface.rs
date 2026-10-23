//! The second pipe's own surface (`docs/DESKTOP.md` §3.5): memory the
//! driver pins into 00:02.0's domain, entered into the GGTT just past what
//! the firmware mapped, and written through BAR 2's aperture at the same
//! GGTT offset -- write-combining, as the lit window is -- so the plane
//! reads what the copy wrote with no cache to flush between them.
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
use ferrix_rt::native::device::{Device, IoMapping};
use ferrix_rt::native::pin::{DeviceAddress, Pin, PinAccess, device_address};
use ferrix_rt::native::vmo::{self, Vmo};

use crate::say;

/// Pages pinned at a time: one GGTT page of entries.
const CHUNK: usize = 512;
/// Chunks the surface takes at most: 16 MiB.
const CHUNKS: usize = 8;

/// The surface: its memory, kept pinned, and where the driver writes it.
pub(crate) struct Surface {
    _vmo: Vmo<Kernel>,
    _pins: [Option<Pin<Kernel>>; CHUNKS],
    _window: IoMapping<Kernel>,
    /// Where it is mapped, write-combining, in this process.
    pub(crate) base: usize,
    /// Its bytes.
    pub(crate) len: usize,
    /// Its GGTT offset, `PLANE_SURF`'s value.
    pub(crate) ggtt: u32,
}

impl Surface {
    /// Make it past `firmware_end` (the end of the firmware's framebuffer
    /// in the GGTT), cleared to black. `None`, said, when any part is
    /// refused: there is no second screen then.
    pub(crate) fn make(device: &Device<Kernel>, apertures: u32, firmware_end: u64) -> Option<Self> {
        let find = |bar: u8| {
            (0..apertures as usize)
                .filter_map(|index| device.aperture(index).ok())
                .find(|aperture| aperture.bar == bar && aperture.offset == 0)
        };
        let (Some(bar0), Some(bar2)) = (find(0), find(2)) else {
            say(format_args!(
                "intel: card0: no second surface: BAR 0 or BAR 2 is not lent"
            ));
            return None;
        };
        let Some(at) = light::surface_place(firmware_end, bar2.len) else {
            say(format_args!(
                "intel: card0: no second surface: {SURFACE_BYTES:#x} bytes past {firmware_end:#x} do not fit BAR 2's {:#x}",
                bar2.len
            ));
            return None;
        };
        let len = usize::try_from(SURFACE_BYTES).ok()?;
        let pages = len.div_ceil(GTT_PAGE_SIZE as usize);
        let first = at / GTT_PAGE_SIZE;
        // The entries: one GGTT page holds 512, and `at` is 2 MiB aligned,
        // so the range starts on a page.
        let entries = device
            .io_mapping(IoMappingSpec {
                phys: bar0.phys + GTTADR_OFFSET + first * 8,
                len: (pages * 8).next_multiple_of(4096) as u64,
            })
            .inspect_err(|error| {
                say(format_args!(
                    "intel: card0: no second surface: the GGTT's entries refused ({error:?})"
                ));
            })
            .ok()?;
        let table = entries.map(None).ok()?;
        // Nothing of the firmware's may be there.
        for index in 0..pages {
            // SAFETY: inside the entries' mapping, eight-byte aligned.
            let pte = unsafe { ptr::read_volatile((table + index * 8) as *const u64) };
            if pte & 1 != 0 {
                say(format_args!(
                    "intel: card0: no second surface: GGTT entry {:#x} is the firmware's",
                    first + index as u64
                ));
                return None;
            }
        }
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
                unsafe { ptr::write_volatile((table + (start + page) * 8) as *mut u64, pte) };
            }
            *slot = Some(pin);
        }
        drop(entries);
        let ggtt = u32::try_from(at).ok()?;
        let window = device
            .io_mapping(IoMappingSpec {
                phys: bar2.phys + at,
                len: SURFACE_BYTES,
            })
            .ok()?;
        let base = window.map_combining(None).ok()?;
        // Black, written through the aperture.
        for word in 0..len / 8 {
            // SAFETY: inside the window just mapped, eight-byte aligned.
            unsafe { ptr::write_volatile((base + word * 8) as *mut u64, 0) };
        }
        ferrix_rt::device_barrier();
        say(format_args!(
            "intel: card0: a second surface, {len:#x} bytes at GGTT {ggtt:#x} ({pages} pages pinned)"
        ));
        Some(Self {
            _vmo: memory,
            _pins: pins,
            _window: window,
            base,
            len,
            ggtt,
        })
    }
}
