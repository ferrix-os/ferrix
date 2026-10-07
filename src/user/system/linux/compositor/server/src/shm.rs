//! Shared memory: `wl_shm`, its pools and the buffers cut out of them.
//!
//! A client draws into memory it shares with the compositor and names a
//! rectangle of it as a `wl_buffer`. This module owns what that rectangle is
//! and whether it lies inside the memory; mapping the memory is the binary's,
//! since this crate holds no descriptor.
//!
//! # The formats
//!
//! `ARGB8888` and `XRGB8888`, the two every compositor must offer, and
//! nothing else. They are what `src/user/system/linux/compositor/render` draws, and a format
//! advertised but not drawn is a client that renders a frame nobody can show.

use compositor_wire::Fd;

/// A pixel format a buffer may be in.
///
/// The values are `wl_shm`'s, which for these two are not the DRM fourcc
/// codes the rest of the list uses: 0 and 1 are aliases the protocol fixes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Format {
    /// `ARGB8888`, premultiplied, as `wl_shm` numbers it.
    Argb8888,
    /// `XRGB8888`: the same bytes with the alpha ignored.
    Xrgb8888,
}

impl Format {
    /// Bytes one pixel takes. Both formats are four.
    pub const BYTES: i32 = 4;

    /// The format `value` names, if it is one this compositor draws.
    #[must_use]
    pub const fn from_wl_shm(value: u32) -> Option<Self> {
        match value {
            compositor_protocol::core::wl_shm::format::ARGB8888 => Some(Self::Argb8888),
            compositor_protocol::core::wl_shm::format::XRGB8888 => Some(Self::Xrgb8888),
            _ => None,
        }
    }

    /// The number `wl_shm.format` carries.
    #[must_use]
    pub const fn to_wl_shm(self) -> u32 {
        match self {
            Self::Argb8888 => compositor_protocol::core::wl_shm::format::ARGB8888,
            Self::Xrgb8888 => compositor_protocol::core::wl_shm::format::XRGB8888,
        }
    }

    /// Whether the alpha byte is a client's to set.
    #[must_use]
    pub const fn has_alpha(self) -> bool {
        matches!(self, Self::Argb8888)
    }
}

/// Every format the compositor offers, in the order `wl_shm` announces them.
pub const FORMATS: [Format; 2] = [Format::Argb8888, Format::Xrgb8888];

/// Which pool a buffer was cut from, for as long as the connection lasts.
///
/// Not the `wl_shm_pool`'s object id: a client may destroy a pool and go on
/// drawing with the buffers cut from it, and libwayland hands the freed id
/// to the next object it makes -- Chrome's next pool, a cursor's few
/// kilobytes, took the id of the one its window's buffer was cut from, and
/// the window was then read out of the wrong memory and drawn blank. A key
/// is given once per pool and never again on the connection. Zero is no
/// pool, which a single-pixel buffer is in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct PoolKey(pub u64);

impl PoolKey {
    /// No pool: a buffer that is one colour.
    pub const NONE: Self = Self(0);
}

/// A pool: memory a client shares and cuts buffers out of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pool {
    /// The descriptor the client sent. The binary maps it; nothing here
    /// touches it.
    pub fd: Fd,
    /// How many bytes it holds. A `resize` may only grow it.
    pub size: i32,
    /// Which pool it is, whatever its object id is now or is later.
    pub key: PoolKey,
}

/// A rectangle of a pool.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Buffer {
    /// The pool it is cut from.
    pub pool: PoolKey,
    /// Where in the pool the first row starts.
    pub offset: i32,
    /// In pixels.
    pub width: i32,
    /// In pixels.
    pub height: i32,
    /// Bytes from one row's start to the next.
    pub stride: i32,
    /// What the pixels are.
    pub format: Format,
    /// The one colour this buffer is, for a `wp_single_pixel_buffer_v1`.
    ///
    /// Such a buffer has no pool: the colour *is* the buffer, and `pool` is
    /// [`PoolKey::NONE`]. Held as the four bytes the renderer reads, in the
    /// order `Format::Argb8888` puts them on a little-endian machine.
    pub solid: Option<[u8; 4]>,
    /// Whether this is a `zwp_linux_dmabuf_v1` buffer: `pool` is then the
    /// key its import is known by, not a `wl_shm_pool`'s, and the buffer
    /// is that import's alone (`docs/GPU.md` §3.13).
    pub dmabuf: bool,
}

impl Buffer {
    /// One pixel of one colour, as `create_u32_rgba_buffer` makes it.
    #[must_use]
    pub const fn solid(alpha: u8, red: u8, green: u8, blue: u8) -> Self {
        Self {
            pool: PoolKey::NONE,
            offset: 0,
            width: 1,
            height: 1,
            stride: 4,
            format: Format::Argb8888,
            solid: Some([blue, green, red, alpha]),
            dmabuf: false,
        }
    }
}

/// Why a `wl_shm_pool.create_buffer` is refused.
///
/// Each is one of `wl_shm`'s own error codes, which is what the client is
/// told.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BufferError {
    /// A format the compositor does not draw.
    Format(u32),
    /// A rectangle that is not one, or does not lie inside the pool.
    Geometry,
}

impl BufferError {
    /// The `wl_shm.error` code the client is given.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Format(_) => compositor_protocol::core::wl_shm::error::INVALID_FORMAT,
            Self::Geometry => compositor_protocol::core::wl_shm::error::INVALID_STRIDE,
        }
    }

    /// The sentence the client is given.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::Format(value) => format!("format 0x{value:x} is not one this compositor draws"),
            Self::Geometry => {
                "the buffer's width, height, stride or offset does not fit the pool".to_owned()
            }
        }
    }
}

impl Pool {
    /// A pool of `size` bytes over `fd`, known as `key`.
    #[must_use]
    pub const fn new(fd: Fd, size: i32, key: PoolKey) -> Self {
        Self { fd, size, key }
    }

    /// Grow the pool to `size`.
    ///
    /// `false` when the size is not larger, which the protocol forbids:
    /// "this request can only be used to make the pool bigger". libwayland
    /// answers a shrink by ignoring it, so this does too -- it is not one of
    /// `wl_shm`'s three error codes, and inventing an error for it would end
    /// connections libwayland keeps.
    pub const fn resize(&mut self, size: i32) -> bool {
        if size <= self.size {
            return false;
        }
        self.size = size;
        true
    }

    /// Cut a buffer out of the pool, or say why not.
    ///
    /// # Where this is stricter than libwayland, on purpose
    ///
    /// `wayland-shm.c`'s check is
    ///
    /// ```text
    /// offset < 0 || width <= 0 || height <= 0 || stride < width ||
    /// INT32_MAX / stride < height || offset > pool->size - stride * height
    /// ```
    ///
    /// `stride < width` compares bytes with pixels. For a four-byte format a
    /// client may therefore pass `stride == width`, a quarter of the row it
    /// needs, and libwayland accepts it: the pool is only required to hold
    /// `stride * height` bytes, while a compositor reading `width` pixels
    /// from each row reads `width * 4` from the last row's start and runs
    /// past the end. Every real toolkit sends `width * 4` or more, so
    /// requiring it costs nothing and closes an out-of-bounds read.
    ///
    /// The arithmetic is checked rather than guarded: libwayland's
    /// `INT32_MAX / stride < height` is there to keep `stride * height` from
    /// overflowing, and a checked multiply says the same thing without a
    /// division by a `stride` that another branch already allowed to be zero.
    pub fn buffer(
        &self,
        pool: PoolKey,
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
        format: u32,
    ) -> Result<Buffer, BufferError> {
        let Some(format) = Format::from_wl_shm(format) else {
            return Err(BufferError::Format(format));
        };
        if offset < 0 || width <= 0 || height <= 0 {
            return Err(BufferError::Geometry);
        }
        let row = width
            .checked_mul(Format::BYTES)
            .ok_or(BufferError::Geometry)?;
        if stride < row {
            return Err(BufferError::Geometry);
        }
        let needed = stride
            .checked_mul(height)
            .and_then(|bytes| bytes.checked_add(offset))
            .ok_or(BufferError::Geometry)?;
        if needed > self.size {
            return Err(BufferError::Geometry);
        }
        Ok(Buffer {
            pool,
            offset,
            width,
            height,
            stride,
            format,
            solid: None,
            dmabuf: false,
        })
    }
}

impl Buffer {
    /// The bytes of the pool this buffer covers: `offset` up to the end of
    /// its last row.
    ///
    /// The binary maps the pool and takes this range out of it, so the bound
    /// a client is held to and the bound the renderer reads to are the same
    /// number.
    #[must_use]
    pub fn range(&self) -> Option<(usize, usize)> {
        // A buffer that is one colour is in no pool, so it covers no part
        // of one; the renderer reads `solid` instead.
        if self.solid.is_some() {
            return None;
        }
        let start = usize::try_from(self.offset).ok()?;
        let len = usize::try_from(self.stride.checked_mul(self.height)?).ok()?;
        Some((start, start.checked_add(len)?))
    }
}
