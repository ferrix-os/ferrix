//! RM's system memory: pages a device can reach (`docs/NVIDIA.md` §4.3).
//!
//! With a device attached, an allocation is a VMO, pinned into the card's
//! IOMMU domain with `vmo_pin` -- against the pin budget devmgr set (N0f,
//! §12.2) -- and mapped into nvrm. `VMO_PIN_ADDRESSES` gives each page's
//! device address, which the domain maps to the page's own physical
//! address, so it serves as RM's "physical" address as well as its DMA
//! address. The pages are not contiguous: a contiguous request of more than
//! one page succeeds only if the pins happen to be.
//!
//! With no device -- `nvrm-link-test`, and the host -- the memory is
//! anonymous and nothing can reach it by DMA. Each page's "physical" address
//! is then its address in nvrm, which is unique, and that is all RM's
//! bookkeeping (`os_match_mmap_offset`) needs of it without a GPU.

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_native::pending::Protection;
use ferrix_native::pin::{DeviceAddress, Pin, PinAccess};
use ferrix_native::vmo::{self, Vmo};

use crate::device;
use crate::futex::Kernel;
use crate::libc;
use crate::log::say;
use crate::status::{self, NvStatus};

/// A page.
const PAGE: usize = 4096;

/// An allocation, as the kept C holds it (`struct nvos_pages`).
#[derive(Debug)]
pub struct Pages {
    /// The VMO and its pin, for a device allocation.
    pinned: Option<(Vmo<Kernel>, Pin<Kernel>)>,
    /// Where it is mapped in nvrm.
    address: usize,
    /// Its length in bytes.
    bytes: usize,
}

/// Whether the device-less allocator has said so once.
static SAID_ANONYMOUS: AtomicBool = AtomicBool::new(false);

/// Allocate `count` pages; write each page's address to `addresses` (the
/// first only, for `contiguous`), where they are mapped to `mapped`, and the
/// allocation to `pages`.
///
/// # Safety
///
/// `addresses` has room for `count` entries, or one for `contiguous`;
/// `mapped` and `pages` are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_pages_alloc(
    count: u64,
    contiguous: bool,
    addresses: *mut u64,
    mapped: *mut *mut c_void,
    pages: *mut *mut Pages,
) -> NvStatus {
    let Some(bytes) = usize::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(PAGE))
        .filter(|&bytes| bytes > 0)
    else {
        return status::INVALID_ARGUMENT;
    };
    let count = bytes / PAGE;
    let entries = if contiguous { 1 } else { count };
    // SAFETY: the caller vouches for the room.
    let out = unsafe { core::slice::from_raw_parts_mut(addresses, entries) };
    let made = match device::for_pins() {
        Some(device) => pinned(&device, bytes, contiguous, out),
        None => anonymous(bytes, out),
    };
    match made {
        Ok(allocation) => {
            // SAFETY: a fresh block for one `Pages`; malloc's alignment
            // suffices.
            let block = unsafe { libc::malloc(size_of::<Pages>()) }.cast::<Pages>();
            if block.is_null() {
                release(allocation);
                return status::NO_MEMORY;
            }
            let address = allocation.address;
            // SAFETY: a fresh, aligned block.
            unsafe { block.write(allocation) };
            // SAFETY: the caller vouches for both.
            unsafe { mapped.write(address as *mut c_void) };
            // SAFETY: as above.
            unsafe { pages.write(block) };
            status::OK
        }
        Err(status) => status,
    }
}

/// Anonymous pages, each "physical" address its address in nvrm.
fn anonymous(bytes: usize, out: &mut [u64]) -> Result<Pages, NvStatus> {
    if !SAID_ANONYMOUS.swap(true, Ordering::Relaxed) {
        say!("no device: system memory is anonymous, and no device can reach it");
    }
    // SAFETY: a fresh private anonymous mapping, not at a fixed address.
    let address = unsafe {
        libc::mmap(
            ptr::null_mut(),
            bytes,
            libc::PROT_READ_WRITE,
            libc::MAP_PRIVATE_ANONYMOUS,
            -1,
            0,
        )
    };
    if address == libc::MAP_FAILED {
        return Err(status::NO_MEMORY);
    }
    let base = address as usize as u64;
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = base + (index * PAGE) as u64;
    }
    Ok(Pages {
        pinned: None,
        address: address as usize,
        bytes,
    })
}

/// A VMO pinned into the device's domain and mapped.
fn pinned(
    device: &ferrix_native::device::Device<Kernel>,
    bytes: usize,
    contiguous: bool,
    out: &mut [u64],
) -> Result<Pages, NvStatus> {
    let vmo = vmo::create(Kernel, bytes).map_err(|why| {
        say!("vmo_create of {bytes} bytes refused: {why:?}");
        status::NO_MEMORY
    })?;
    let pin = device
        .pin(&vmo, 0, bytes, PinAccess::ReadWrite)
        .map_err(|why| {
            say!("vmo_pin of {bytes} bytes refused: {why:?} (the pin budget, N0f?)");
            status::NO_MEMORY
        })?;
    let count = bytes / PAGE;
    // The first page's address alone, for a contiguous request; every
    // page's otherwise, written in place as the native-endian words they
    // are.
    let mut first: [DeviceAddress; 1] = [[0; 8]];
    let found = if contiguous {
        pin.addresses(&mut first)
    } else {
        // SAFETY: `out` is `count` live, aligned `u64`s; an array of
        // `[u8; 8]` of the same count covers the same bytes, with weaker
        // alignment.
        let raw = unsafe {
            core::slice::from_raw_parts_mut(out.as_mut_ptr().cast::<DeviceAddress>(), count)
        };
        pin.addresses(raw)
    };
    let found = found.map_err(|why| {
        say!("vmo_pin_addresses refused: {why:?}");
        status::NO_MEMORY
    })?;
    if found.pages != count || (!contiguous && !found.is_complete()) {
        say!("vmo_pin_addresses gave {} of {count} pages", found.written);
        return Err(status::NO_MEMORY);
    }
    if contiguous {
        let base = u64::from_ne_bytes(first[0]);
        if count > 1 && !is_contiguous(&pin, base, count) {
            say!(
                "a contiguous allocation of {count} pages was refused: the pins are \
                 not contiguous (docs/NVIDIA.md §4.3)"
            );
            return Err(status::NO_MEMORY);
        }
        if let Some(slot) = out.first_mut() {
            *slot = base;
        }
    }
    let address = vmo
        .map(None, bytes, Protection::ReadWrite, 0)
        .map_err(|why| {
            say!("vmo_map of {bytes} bytes refused: {why:?}");
            status::NO_MEMORY
        })?;
    Ok(Pages {
        pinned: Some((vmo, pin)),
        address,
        bytes,
    })
}

/// Whether the pin's `count` pages follow each other from `base`.
fn is_contiguous(pin: &Pin<Kernel>, base: u64, count: usize) -> bool {
    let mut chunk: [DeviceAddress; 64] = [[0; 8]; 64];
    let Ok(found) = pin.addresses(&mut chunk) else {
        return false;
    };
    // Only the first 64 can be read this way; a longer run is refused,
    // which is the conservative answer.
    if found.pages > chunk.len() {
        return false;
    }
    chunk
        .iter()
        .take(count)
        .enumerate()
        .all(|(index, bytes)| u64::from_ne_bytes(*bytes) == base + (index * PAGE) as u64)
}

/// Unmap and release an allocation.
fn release(pages: Pages) {
    // SAFETY: the allocation's own mapping, which nothing uses any more.
    let _ = unsafe { libc::munmap(pages.address as *mut c_void, pages.bytes) };
    // The pin and the VMO close here, unpinning the pages.
    drop(pages.pinned);
}

/// Free an allocation [`nvos_pages_alloc`] made.
///
/// # Safety
///
/// `pages` is one [`nvos_pages_alloc`] made, freed once, and nothing uses
/// its memory any more.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nvos_pages_free(pages: *mut Pages) {
    if pages.is_null() {
        return;
    }
    // SAFETY: the caller vouches for it.
    let allocation = unsafe { pages.read() };
    // SAFETY: read just now; the block is used no more.
    unsafe { libc::free(pages.cast()) };
    release(allocation);
}
