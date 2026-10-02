//! A surface's `wl_shm` buffers: two, and a third while the compositor holds
//! both.
//!
//! A buffer attached and committed belongs to the compositor until it sends
//! `wl_buffer.release`; drawing into it before then tears the frame the
//! compositor is reading. So each surface keeps a few, each marked busy from
//! its commit to its release, and a frame goes into whichever is free. A
//! buffer of the wrong size -- the surface was configured again -- is
//! destroyed when it is next free rather than reused.

use compositor_shm::Shared;
use compositor_wire::ObjectId;

/// One buffer and the pool it was cut from.
#[derive(Debug)]
pub(crate) struct Slot {
    /// The memory.
    pub(crate) shared: Shared,
    /// Its `wl_shm_pool`.
    pub(crate) pool: ObjectId,
    /// Its `wl_buffer`.
    pub(crate) buffer: ObjectId,
    /// Its size in pixels.
    pub(crate) width: u32,
    /// Its size in pixels.
    pub(crate) height: u32,
    /// Its `wl_shm` format: [`ARGB8888`] or [`XRGB8888`].
    pub(crate) format: u32,
    /// Committed and not yet released.
    pub(crate) busy: bool,
}

/// `wl_shm`'s `ARGB8888`: B, G, R and premultiplied alpha in memory.
pub(crate) const ARGB8888: u32 = 0;

/// `wl_shm`'s `XRGB8888`: B, G, R and a byte that means nothing.
pub(crate) const XRGB8888: u32 = 1;

/// Copy premultiplied RGBA (tiny-skia's order) into premultiplied `ARGB8888`
/// as `wl_shm` lays it out on a little-endian machine: B, G, R, A.
pub(crate) fn swizzle_into(rgba: &[u8], argb: &mut [u8]) {
    for (from, to) in rgba.chunks_exact(4).zip(argb.chunks_exact_mut(4)) {
        if let ([r, g, b, a], [tb, tg, tr, ta]) = (from, to) {
            *tb = *b;
            *tg = *g;
            *tr = *r;
            *ta = *a;
        }
    }
}

/// The bytes a `width` × `height` `ARGB8888` buffer needs, if it is a size
/// that can be made.
pub(crate) fn length(width: u32, height: u32) -> Option<usize> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))?
        .checked_mul(4)?;
    // A pool's size is an `int` on the wire.
    if bytes == 0 || bytes > i32::MAX as u64 {
        return None;
    }
    usize::try_from(bytes).ok()
}
