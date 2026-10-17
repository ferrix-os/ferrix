//! Whether an open of the card maps its own buffers and nothing else: the
//! check `cargo xtask test-display` makes of E-02's fix (`docs/DISPLAY.md`
//! §2.1).
//!
//! Before it, every buffer was a range of one card VMO and the card's node
//! mapped the whole of it: an offset past a buffer, or one no buffer of this
//! open had, mapped somebody's pixels, and a mapping kept after its buffer
//! was destroyed saw whichever buffer was given that range next. Each is
//! tried here, on an open of the card's own that is closed again before the
//! program shows anything:
//!
//! 1. a mapping one page longer than the buffer is refused;
//! 2. an offset that names no buffer of this open is refused;
//! 3. a mapping kept after `DESTROY_DUMB` sees none of the pixels drawn in
//!    the buffers made after it;
//! 4. the destroyed buffer's offset maps nothing new, as Linux revokes it
//!    from the open with the handle;
//! 5. nor does a destroyed buffer's that a framebuffer still holds, whose
//!    pages the card keeps for the framebuffer and not for a new mapping.
//!
//! A mapping holds its open, as on Linux, so closing the card does not end
//! a buffer that is still mapped; destroying it does.
//!
//! The answer is one line, [`PREFIX`] then `ok` or what leaked.

use std::io;

use ferrix_linux_abi::drm::{self, CreateDumb, DestroyDumb, FbCmd2, Field, MapDumb};

/// What the line starts with, which the display test looks for.
pub const PREFIX: &str = "card-isolation: ";

/// The probe buffer's side, in pixels: one 16 KiB buffer.
const SIDE: u32 = 64;

/// What the destroyed buffer was drawn with, and what the later ones are.
const FIRST: u8 = 0xA1;
const SECOND: u8 = 0x5B;

/// How many buffers are made after the destroyed one: the core gives a
/// range back once its driver has let go, which is a reply away, so the
/// first may come before it and a later one after.
const LATER: usize = 4;

/// An open of the card that closes when it goes.
struct Open(libc::c_int);

impl Open {
    fn card() -> io::Result<Self> {
        // SAFETY: a NUL-terminated path; the descriptor is owned by `Open`.
        let fd = unsafe { libc::open(c"/dev/dri/card0".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(fd))
    }

    /// A `SIDE`-square dumb buffer: its handle, size and map offset.
    fn dumb(&self) -> io::Result<(u32, usize, libc::off_t)> {
        let mut create = CreateDumb {
            width: SIDE,
            height: SIDE,
            bpp: 32,
            ..CreateDumb::ZERO
        };
        crate::card::ioctl_on(self.0, drm::IOCTL_MODE_CREATE_DUMB, &mut create)?;
        let mut map = MapDumb {
            handle: create.handle,
            ..MapDumb::ZERO
        };
        crate::card::ioctl_on(self.0, drm::IOCTL_MODE_MAP_DUMB, &mut map)?;
        let size = usize::try_from(create.size).map_err(io::Error::other)?;
        let offset = libc::off_t::try_from(map.offset).map_err(io::Error::other)?;
        Ok((create.handle, size, offset))
    }

    /// Map `size` bytes at `offset`, shared and writable.
    fn map(&self, size: usize, offset: libc::off_t) -> io::Result<Mapping> {
        // SAFETY: a null hint lets the kernel choose; the result is checked.
        let base = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.0,
                offset,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping { base, size })
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: the descriptor `Open::card` made, closed once.
        let _ = unsafe { libc::close(self.0) };
    }
}

/// A mapping that is taken away when it goes.
struct Mapping {
    base: *mut libc::c_void,
    size: usize,
}

impl Mapping {
    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: `base` maps `size` bytes, readable and writable, until drop.
        unsafe { core::slice::from_raw_parts_mut(self.base.cast::<u8>(), self.size) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping `Open::map` made, unmapped once.
        let _ = unsafe { libc::munmap(self.base, self.size) };
    }
}

/// Run the three checks: [`PREFIX`] and `ok`, or the first that failed.
#[must_use]
pub fn isolation() -> String {
    match check() {
        Ok(()) => format!("{PREFIX}ok"),
        Err(why) => format!("{PREFIX}LEAK: {why}"),
    }
}

fn check() -> Result<(), String> {
    let first = Open::card().map_err(|why| format!("card0 did not open: {why}"))?;
    let (handle, size, offset) = first
        .dumb()
        .map_err(|why| format!("no dumb buffer: {why}"))?;
    let page = 4096;

    // 1. Nothing past the buffer's end.
    if first.map(size + page, offset).is_ok() {
        return Err(format!(
            "a mapping {page} bytes longer than buffer {handle} was allowed"
        ));
    }
    // 2. No offset that names no buffer of this open: the next handle's, and
    // the place one buffer's length further on, where the next range of a
    // shared card VMO would start.
    let next = MapDumb {
        handle: handle + 1,
        ..MapDumb::ZERO
    };
    let mut asked = next;
    if crate::card::ioctl_on(first.0, drm::IOCTL_MODE_MAP_DUMB, &mut asked).is_ok() {
        return Err(format!(
            "handle {} has a map offset but no buffer",
            handle + 1
        ));
    }
    let further = offset + libc::off_t::try_from(size).map_err(|why| why.to_string())?;
    if first.map(size, further).is_ok() {
        return Err(format!(
            "offset {further:#x}, past buffer {handle}'s, mapped what no buffer of this open is"
        ));
    }

    // 3. A mapping kept past its buffer.
    let mut kept = first
        .map(size, offset)
        .map_err(|why| format!("buffer {handle} did not map: {why}"))?;
    kept.bytes().fill(FIRST);
    let mut destroy = DestroyDumb { handle };
    crate::card::ioctl_on(first.0, drm::IOCTL_MODE_DESTROY_DUMB, &mut destroy)
        .map_err(|why| format!("buffer {handle} was not destroyed: {why}"))?;
    if first.map(size, offset).is_ok() {
        return Err(format!("destroyed buffer {handle}'s offset mapped again"));
    }
    // 5. A destroyed buffer a framebuffer still holds: the card keeps it for
    // the framebuffer, and its offset maps nothing new.
    let (held, held_size, held_offset) = first
        .dumb()
        .map_err(|why| format!("no framebuffer's dumb buffer: {why}"))?;
    let mut framebuffer = FbCmd2 {
        width: SIDE,
        height: SIDE,
        pixel_format: drm::FORMAT_XRGB8888,
        handles: [held, 0, 0, 0],
        pitches: [SIDE * 4, 0, 0, 0],
        ..FbCmd2::ZERO
    };
    crate::card::ioctl_on(first.0, drm::IOCTL_MODE_ADDFB2, &mut framebuffer)
        .map_err(|why| format!("no framebuffer for buffer {held}: {why}"))?;
    let mut destroy = DestroyDumb { handle: held };
    crate::card::ioctl_on(first.0, drm::IOCTL_MODE_DESTROY_DUMB, &mut destroy)
        .map_err(|why| format!("buffer {held} was not destroyed: {why}"))?;
    if first.map(held_size, held_offset).is_ok() {
        return Err(format!(
            "destroyed buffer {held}, which a framebuffer holds, mapped again"
        ));
    }
    let mut later = Vec::with_capacity(LATER);
    for _ in 0..LATER {
        let (made, size, offset) = first
            .dumb()
            .map_err(|why| format!("no later dumb buffer: {why}"))?;
        let mut drawn = first
            .map(size, offset)
            .map_err(|why| format!("buffer {made} did not map: {why}"))?;
        drawn.bytes().fill(SECOND);
        if kept.bytes().contains(&SECOND) {
            return Err(format!(
                "a mapping kept past buffer {handle} sees buffer {made}'s pixels"
            ));
        }
        later.push(drawn);
    }
    Ok(())
}
