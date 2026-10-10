//! The buffers a frame is drawn from and into, checked once when they are
//! described so drawing never reads or writes past one.

use crate::Error;
use crate::canvas::MAX_SIZE;

/// A pixel format a client's buffer can have: the two every `wl_shm` must
/// offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// `ARGB8888`: premultiplied alpha, as `wl_shm` defines it, drawn with
    /// source-over.
    Argb8888,
    /// `XRGB8888`: opaque, the X byte ignored, copied.
    Xrgb8888,
}

impl Format {
    /// The format's `wl_shm.format` value.
    #[must_use]
    pub const fn wl_shm(self) -> u32 {
        match self {
            Self::Argb8888 => 0,
            Self::Xrgb8888 => 1,
        }
    }

    /// The format for a `wl_shm.format` value, if it is one of these.
    #[must_use]
    pub const fn from_wl_shm(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Argb8888),
            1 => Some(Self::Xrgb8888),
            _ => None,
        }
    }

    /// The format's DRM fourcc, which is what Smithay's `ImportMem` names
    /// formats by.
    #[must_use]
    pub const fn fourcc(self) -> u32 {
        match self {
            Self::Argb8888 => u32::from_le_bytes(*b"AR24"),
            Self::Xrgb8888 => u32::from_le_bytes(*b"XR24"),
        }
    }
}

/// Check a buffer's shape: a size in `1..=MAX_SIZE` each way, a stride of at
/// least four bytes a pixel, and bytes through the last pixel of the last
/// row. Returns the bytes needed.
fn check(len: usize, width: u32, height: u32, stride: u32) -> Result<usize, Error> {
    if width == 0 || height == 0 || width > MAX_SIZE || height > MAX_SIZE {
        return Err(Error::Size { width, height });
    }
    let row = u64::from(width) * 4;
    if u64::from(stride) < row {
        return Err(Error::Stride { width, stride });
    }
    let needed = u64::from(stride) * u64::from(height - 1) + row;
    let needed = usize::try_from(needed).unwrap_or(usize::MAX);
    if len < needed {
        return Err(Error::Short { needed, len });
    }
    Ok(needed)
}

/// The buffer on a GPU a [`Surface`]'s pixels are: a client's dmabuf.
#[derive(Debug, Clone, Copy)]
pub struct OnDevice<'a> {
    /// The dmabuf.
    pub fd: std::os::fd::BorrowedFd<'a>,
    /// What the buffer is called for as long as it lives.
    pub key: u64,
    /// Where the buffer's first row starts in the dmabuf, in bytes.
    pub offset: u32,
    /// The buffer's DRM format modifier: `0` for linear rows.
    pub modifier: u64,
    /// The whole buffer's width in pixels, which is the surface's unless
    /// the surface is a part of it.
    pub width: u32,
    /// The whole buffer's height in pixels.
    pub height: u32,
    /// Where the surface's first pixel is in the buffer.
    pub x: u32,
    /// See [`OnDevice::x`].
    pub y: u32,
}

/// A client's pixels: a `wl_shm` buffer, or any other 32-bit buffer of the
/// two formats, with the stride the client gave.
#[derive(Debug, Clone, Copy)]
pub struct Surface<'a> {
    data: &'a [u8],
    width: u32,
    height: u32,
    stride: u32,
    format: Format,
    name: u64,
    /// The buffer on a GPU the pixels also are, as a dmabuf, and what it is
    /// called from frame to frame: a renderer on that GPU samples it where
    /// it lies rather than uploading `data`.
    device: Option<OnDevice<'a>>,
}

impl<'a> Surface<'a> {
    /// Describe `data` as a `width` × `height` buffer of `stride` bytes a
    /// row, starting at its first byte.
    ///
    /// # Errors
    ///
    /// [`Error::Size`], [`Error::Stride`] or [`Error::Short`] when the shape
    /// does not fit the bytes.
    pub fn new(
        data: &'a [u8],
        width: u32,
        height: u32,
        stride: u32,
        format: Format,
    ) -> Result<Self, Error> {
        let _ = check(data.len(), width, height, stride)?;
        Ok(Self {
            data,
            width,
            height,
            stride,
            format,
            name: 0,
            device: None,
        })
    }

    /// A `width` × `height` buffer whose pixels this program cannot read:
    /// video memory, which only the GPU that made it can sample. It is
    /// drawn only once [`Surface::on_device`] has said which buffer it is,
    /// and only by a renderer on that GPU; anything else draws nothing for
    /// it.
    ///
    /// # Errors
    ///
    /// [`Error::Size`] or [`Error::Stride`] when the shape is not one a
    /// buffer has.
    pub fn without_pixels(
        width: u32,
        height: u32,
        stride: u32,
        format: Format,
    ) -> Result<Self, Error> {
        let _ = check(usize::MAX, width, height, stride)?;
        Ok(Self {
            data: &[],
            width,
            height,
            stride,
            format,
            name: 0,
            device: None,
        })
    }

    /// Whether the pixels can be read here: all but
    /// [`Surface::without_pixels`]'s.
    #[must_use]
    pub const fn has_pixels(&self) -> bool {
        !self.data.is_empty()
    }

    /// The same pixels, said to be those of the thing called `name`: a
    /// `wl_surface`, say, by its client and its id.
    ///
    /// The software renderer reads a surface's pixels afresh every time and
    /// has no use for this. A renderer that keeps a copy of them somewhere
    /// slower to reach -- a texture on a GPU -- needs to know that this
    /// frame's pixels and the last one's are the *same surface*, so that it
    /// moves only what changed; a client that draws into two buffers in
    /// turn is one surface, and its pixels' address is not. Zero is no name
    /// at all, and such a surface is moved whole every time it is drawn.
    #[must_use]
    pub const fn named(mut self, name: u64) -> Self {
        self.name = name;
        self
    }

    /// The part of these pixels `width` × `height` from `(x, y)`, as a
    /// surface of its own over the same bytes: a window without the shadows
    /// its client draws around it. Its name moves with the part, so that a
    /// renderer keeping a copy by name does not take one part's for
    /// another's. `None` for a part that is empty or not inside.
    #[must_use]
    pub fn cropped(&self, x: u32, y: u32, width: u32, height: u32) -> Option<Self> {
        if width == 0
            || height == 0
            || x.checked_add(width)? > self.width
            || y.checked_add(height)? > self.height
        {
            return None;
        }
        let name = match self.name {
            0 => 0,
            name => name ^ (u64::from(x) << 40) ^ (u64::from(y) << 52),
        };
        if !self.has_pixels() {
            // Nothing to read a part of: the part is the same buffer on
            // its GPU, with where in it the part begins.
            let on = self.device?;
            return Some(Self {
                width,
                height,
                name,
                device: Some(OnDevice {
                    x: on.x.checked_add(x)?,
                    y: on.y.checked_add(y)?,
                    ..on
                }),
                ..*self
            });
        }
        let start = usize::try_from(y)
            .ok()?
            .checked_mul(usize::try_from(self.stride).ok()?)?
            .checked_add(usize::try_from(x).ok()?.checked_mul(4)?)?;
        let data = self.data.get(start..)?;
        let part = Self::new(data, width, height, self.stride, self.format).ok()?;
        Some(part.named(name))
    }

    /// The same pixels, said also to be the GPU buffer `fd` -- a client's
    /// dmabuf -- known as `key` for as long as that buffer lives
    /// (`docs/GPU.md` §3.13).
    ///
    /// The software renderer reads `data` and has no use for this. A
    /// renderer on the same GPU imports the buffer once and samples it,
    /// which is a client's frame shown without its pixels being copied
    /// anywhere; `data` is what it falls back to when it cannot.
    #[must_use]
    pub const fn on_device(mut self, fd: std::os::fd::BorrowedFd<'a>, key: u64) -> Self {
        self.device = Some(OnDevice {
            fd,
            key,
            offset: 0,
            modifier: 0,
            width: self.width,
            height: self.height,
            x: 0,
            y: 0,
        });
        self
    }

    /// Where the buffer [`Surface::on_device`] named begins in its dmabuf
    /// and how it is laid out, as its client said: what a renderer that
    /// cannot ask the buffer itself imports it by. Linear from the first
    /// byte when this is not said.
    #[must_use]
    pub const fn laid_out(mut self, offset: u32, modifier: u64) -> Self {
        if let Some(on) = self.device {
            self.device = Some(OnDevice {
                offset,
                modifier,
                ..on
            });
        }
        self
    }

    /// The GPU buffer [`Surface::on_device`] said these pixels are.
    #[must_use]
    pub const fn device(&self) -> Option<OnDevice<'a>> {
        self.device
    }

    /// What [`Surface::named`] called it, or zero.
    #[must_use]
    pub const fn name(&self) -> u64 {
        self.name
    }

    /// The same bytes read as opaque, whatever the client's format said.
    ///
    /// `windowrule = opaque`: a client that leaves rubbish in its alpha
    /// channel is drawn blotchy, and the rule is a person saying "there is
    /// nothing to see through here". The bytes are not touched; only what
    /// the fourth one *means* changes, which is exactly what the rule says.
    #[must_use]
    pub const fn as_opaque(self) -> Self {
        Self {
            format: Format::Xrgb8888,
            ..self
        }
    }

    /// The width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The stride in bytes.
    #[must_use]
    pub const fn stride(&self) -> u32 {
        self.stride
    }

    /// The format.
    #[must_use]
    pub const fn format(&self) -> Format {
        self.format
    }

    /// The bytes as given.
    #[must_use]
    pub const fn data(&self) -> &'a [u8] {
        self.data
    }

    /// Row `y`'s pixels, `width` × 4 bytes, without the padding.
    pub(crate) fn row(&self, y: u32) -> Option<&'a [u8]> {
        let start = usize::try_from(u64::from(y) * u64::from(self.stride)).ok()?;
        let len = usize::try_from(u64::from(self.width) * 4).ok()?;
        self.data.get(start..start.checked_add(len)?)
    }
}

/// Where a frame is presented: the mapping of an `XRGB8888` dumb buffer,
/// with the stride `MODE_CREATE_DUMB` returned.
#[derive(Debug)]
pub struct Target<'a> {
    data: &'a mut [u8],
    width: u32,
    height: u32,
    stride: u32,
}

impl<'a> Target<'a> {
    /// Describe `data` as a `width` × `height` `XRGB8888` buffer of `stride`
    /// bytes a row.
    ///
    /// # Errors
    ///
    /// [`Error::Size`], [`Error::Stride`] or [`Error::Short`] when the shape
    /// does not fit the bytes.
    pub fn new(data: &'a mut [u8], width: u32, height: u32, stride: u32) -> Result<Self, Error> {
        let _ = check(data.len(), width, height, stride)?;
        Ok(Self {
            data,
            width,
            height,
            stride,
        })
    }

    /// The width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The stride in bytes.
    #[must_use]
    pub const fn stride(&self) -> u32 {
        self.stride
    }

    /// The bytes.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        self.data
    }

    /// The bytes, to write into.
    pub(crate) fn data_mut(&mut self) -> &mut [u8] {
        self.data
    }

    /// The `len` bytes of row `y` from column `x`.
    pub(crate) fn span_mut(&mut self, x: u32, y: u32, len: usize) -> Option<&mut [u8]> {
        let start = u64::from(y) * u64::from(self.stride) + u64::from(x) * 4;
        let start = usize::try_from(start).ok()?;
        self.data.get_mut(start..start.checked_add(len)?)
    }
}

#[cfg(test)]
mod tests {
    use super::{Format, Surface};

    /// A 4 × 3 buffer whose every pixel's first byte is its index.
    fn numbered() -> Vec<u8> {
        (0..12_u8).flat_map(|at| [at, 0, 0, 0xff]).collect()
    }

    /// A buffer no processor can read is a surface with no bytes, and a
    /// part of it is the same buffer on its GPU with where the part begins.
    #[test]
    fn a_buffer_without_pixels_is_cropped_by_where_its_part_begins() {
        use std::os::fd::AsFd;
        let file = std::fs::File::open("/dev/null").unwrap();
        assert!(Surface::without_pixels(0, 3, 16, Format::Argb8888).is_err());
        assert!(Surface::without_pixels(4, 3, 15, Format::Argb8888).is_err());
        let bare = Surface::without_pixels(4, 3, 16, Format::Argb8888).unwrap();
        assert!(!bare.has_pixels());
        assert!(bare.row(0).is_none());
        // Not said to be on a GPU, there is nothing to take a part of.
        assert!(bare.cropped(1, 1, 2, 2).is_none());
        let whole = bare
            .named(7)
            .on_device(file.as_fd(), 9)
            .laid_out(64, 0x0300_0000_0060_6014);
        let on = whole.device().unwrap();
        assert_eq!((on.width, on.height, on.x, on.y), (4, 3, 0, 0));
        assert_eq!((on.offset, on.modifier), (64, 0x0300_0000_0060_6014));
        let part = whole.cropped(1, 1, 2, 2).unwrap();
        let part = part.cropped(1, 0, 1, 2).unwrap();
        assert_eq!((part.width(), part.height(), part.stride()), (1, 2, 16));
        let on = part.device().unwrap();
        assert_eq!((on.key, on.width, on.height, on.x, on.y), (9, 4, 3, 2, 1));
        assert_eq!((on.offset, on.modifier), (64, 0x0300_0000_0060_6014));
        assert!(whole.cropped(3, 0, 2, 1).is_none());
        // A surface whose pixels are here keeps its crop in the bytes, and
        // is the upload's from then on, as before.
        let bytes = numbered();
        let read = Surface::new(&bytes, 4, 3, 16, Format::Argb8888)
            .unwrap()
            .on_device(file.as_fd(), 9);
        assert!(read.has_pixels());
        assert!(read.cropped(1, 1, 2, 2).unwrap().device().is_none());
    }

    #[test]
    fn a_crop_is_the_part_it_names_with_the_same_stride() {
        let bytes = numbered();
        let whole = Surface::new(&bytes, 4, 3, 16, Format::Argb8888)
            .unwrap()
            .named(7);
        let part = whole.cropped(1, 1, 2, 2).unwrap();
        assert_eq!((part.width(), part.height(), part.stride()), (2, 2, 16));
        // Row 1 from column 1: pixels 5 and 6, then row 2's 9 and 10.
        let first = |row: usize, column: usize| part.data()[row * 16 + column * 4];
        assert_eq!(
            [first(0, 0), first(0, 1), first(1, 0), first(1, 1)],
            [5, 6, 9, 10]
        );
        // Another part of the same surface is not taken for this one.
        assert_ne!(part.name(), whole.cropped(0, 0, 2, 2).unwrap().name());
        assert_eq!(whole.cropped(0, 0, 4, 3).unwrap().data().len(), bytes.len());
    }

    #[test]
    fn a_crop_that_is_empty_or_reaches_outside_is_refused() {
        let bytes = numbered();
        let whole = Surface::new(&bytes, 4, 3, 16, Format::Argb8888).unwrap();
        assert!(whole.cropped(0, 0, 0, 1).is_none());
        assert!(whole.cropped(3, 0, 2, 1).is_none());
        assert!(whole.cropped(0, 2, 1, 2).is_none());
        assert!(whole.cropped(u32::MAX, 0, 1, 1).is_none());
    }
}
