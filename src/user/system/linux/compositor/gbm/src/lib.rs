//! A GBM-shaped allocator: buffers on the GPU that a client draws into and
//! hands the compositor as dmabufs (`docs/GPU.md` §3.13).
//!
//! Mesa's `libgbm` is how a Linux client gets a buffer it can render into
//! and share: `gbm_create_device` on a render node, `gbm_bo_create` with a
//! size, a format and a use, `gbm_bo_get_fd` for the dmabuf a compositor
//! takes through `zwp_linux_dmabuf_v1`, `gbm_bo_get_stride` and
//! `gbm_bo_get_modifier` for what to tell it, and `gbm_bo_write` for bytes
//! drawn on the CPU. This is that shape in Rust, over `compositor-drm`'s
//! render node, for the clients on Ferrix that exist today; Mesa's own GBM
//! comes with Mesa (§3a).
//!
//! # What a buffer is here
//!
//! A virgl resource: a 2D texture of four bytes a pixel, made bindable as a
//! render target, a sampler view and a scanout, so that a client can draw
//! into it with its own virgl stream ([`Bo::resource`] names it there), a
//! compositor can sample it, and a card could show it. Its layout seen from
//! the guest is linear -- rows `stride` bytes apart in its backing -- which
//! is the one modifier it has. Both formats are made as virgl's
//! `B8G8R8A8_UNORM`, which is `ARGB8888`'s bytes; an `XRGB8888` buffer is
//! the same bytes whose alpha the compositor ignores.

use std::io;
use std::os::fd::OwnedFd;
use std::rc::Rc;

use compositor_drm::{Box3d, Mapping, Render, ResourceCreate};
use compositor_virgl::pipe;

/// The DRM format code `ARGB8888`: `fourcc_code('A', 'R', '2', '4')`.
pub const DRM_FORMAT_ARGB8888: u32 = 0x3432_5241;

/// The DRM format code `XRGB8888`: `fourcc_code('X', 'R', '2', '4')`.
pub const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;

/// `DRM_FORMAT_MOD_LINEAR`: rows one after another.
pub const MOD_LINEAR: u64 = 0;

/// A buffer's pixel format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    /// `GBM_FORMAT_ARGB8888`: premultiplied, the alpha meant.
    Argb8888,
    /// `GBM_FORMAT_XRGB8888`: the same bytes, the alpha ignored.
    Xrgb8888,
}

impl Format {
    /// Its DRM format code, which `zwp_linux_buffer_params_v1` is told.
    #[must_use]
    pub const fn fourcc(self) -> u32 {
        match self {
            Self::Argb8888 => DRM_FORMAT_ARGB8888,
            Self::Xrgb8888 => DRM_FORMAT_XRGB8888,
        }
    }
}

/// What a buffer will be used for: `GBM_BO_USE_*`. Every buffer here can
/// be every one of these, so the word is taken for its shape and checked
/// for nothing but being one this crate knows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Usage(pub u32);

impl Usage {
    /// `GBM_BO_USE_SCANOUT`: a card may show it.
    pub const SCANOUT: Self = Self(1 << 0);
    /// `GBM_BO_USE_RENDERING`: drawn into on the GPU.
    pub const RENDERING: Self = Self(1 << 2);
    /// `GBM_BO_USE_WRITE`: written from the CPU with [`Bo::write`].
    pub const WRITE: Self = Self(1 << 3);
    /// `GBM_BO_USE_LINEAR`: rows one after another, which every buffer
    /// here is.
    pub const LINEAR: Self = Self(1 << 4);

    /// Both.
    #[must_use]
    pub const fn and(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every bit is one this crate knows.
    const fn known(self) -> bool {
        self.0 & !(Self::SCANOUT.0 | Self::RENDERING.0 | Self::WRITE.0 | Self::LINEAR.0) == 0
    }
}

/// The largest side a buffer may have: virgl's own texture limit on the
/// hosts this runs on, and far past any screen.
pub const MAX_SIDE: u32 = 16384;

/// A render node, as `gbm_create_device` opens one.
#[derive(Debug, Clone)]
pub struct Device {
    node: Rc<Render>,
}

impl Device {
    /// Open the render node.
    ///
    /// # Errors
    ///
    /// The node's: `ENOENT` where the card has no GPU behind it.
    pub fn open() -> io::Result<Self> {
        Ok(Self {
            node: Rc::new(Render::open()?),
        })
    }

    /// The node, for a program's own command streams and for importing.
    #[must_use]
    pub fn node(&self) -> &Render {
        &self.node
    }

    /// A buffer `width` × `height` of `format`, for `usage`:
    /// `gbm_bo_create`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a size of nothing, past [`MAX_SIDE`], or a use this
    /// crate does not know; the node's otherwise.
    pub fn create(&self, width: u32, height: u32, format: Format, usage: Usage) -> io::Result<Bo> {
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || !usage.known() {
            return Err(io::Error::from_raw_os_error(22));
        }
        let stride = width * 4;
        let size = stride
            .checked_mul(height)
            .ok_or_else(|| io::Error::from_raw_os_error(22))?;
        let (handle, resource) = self.node.create(ResourceCreate {
            target: pipe::TEXTURE_2D,
            format: pipe::FORMAT_B8G8R8A8_UNORM,
            bind: pipe::BIND_RENDER_TARGET | pipe::BIND_SAMPLER_VIEW | pipe::BIND_SCANOUT,
            width,
            height,
            depth: 1,
            array_size: 1,
            last_level: 0,
            nr_samples: 0,
            // The rows the way the compositor's own textures keep them, so
            // that what it samples here is the right way up beside them.
            flags: 0,
            bo_handle: 0,
            res_handle: 0,
            size,
            stride,
        })?;
        let mapping = match self.node.map(handle, size as usize) {
            Ok(mapping) => mapping,
            Err(error) => {
                let _ = self.node.close(handle);
                return Err(error);
            }
        };
        Ok(Bo {
            node: Rc::clone(&self.node),
            handle,
            resource,
            width,
            height,
            stride,
            format,
            mapping,
        })
    }
}

/// A buffer object: `struct gbm_bo`.
pub struct Bo {
    node: Rc<Render>,
    handle: u32,
    resource: u32,
    width: u32,
    height: u32,
    stride: u32,
    format: Format,
    mapping: Mapping,
}

impl core::fmt::Debug for Bo {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Bo")
            .field("handle", &self.handle)
            .field("resource", &self.resource)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .finish_non_exhaustive()
    }
}

impl Bo {
    /// Its width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Its height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Bytes from one row's start to the next: `gbm_bo_get_stride`.
    #[must_use]
    pub const fn stride(&self) -> u32 {
        self.stride
    }

    /// Where its first row starts: `gbm_bo_get_offset`, always 0.
    #[must_use]
    pub const fn offset(&self) -> u32 {
        0
    }

    /// Its format.
    #[must_use]
    pub const fn format(&self) -> Format {
        self.format
    }

    /// Its layout: `gbm_bo_get_modifier`, always linear.
    #[must_use]
    pub const fn modifier(&self) -> u64 {
        MOD_LINEAR
    }

    /// This open's handle for it: `gbm_bo_get_handle`.
    #[must_use]
    pub const fn handle(&self) -> u32 {
        self.handle
    }

    /// The resource a command stream names it by, to draw into it on the
    /// GPU.
    #[must_use]
    pub const fn resource(&self) -> u32 {
        self.resource
    }

    /// A dmabuf of it, to hand a compositor: `gbm_bo_get_fd`. The
    /// descriptor holds the buffer alive on its own.
    ///
    /// # Errors
    ///
    /// The node's.
    pub fn export(&self) -> io::Result<OwnedFd> {
        self.node.export(self.handle)
    }

    /// Write `pixels`, rows of `width * 4` bytes one after another, into
    /// the whole buffer and move them to the GPU: `gbm_bo_write`. Returns
    /// once they are there, so the next write may begin at once.
    ///
    /// # Errors
    ///
    /// `EINVAL` for pixels that are not the buffer's size; the node's
    /// otherwise.
    pub fn write(&mut self, pixels: &[u8]) -> io::Result<()> {
        let row = self.width as usize * 4;
        if pixels.len() != row * self.height as usize {
            return Err(io::Error::from_raw_os_error(22));
        }
        let stride = self.stride as usize;
        for (from, into) in pixels
            .chunks(row)
            .zip(self.mapping.bytes_mut().chunks_mut(stride))
        {
            if let Some(into) = into.get_mut(..row) {
                into.copy_from_slice(from);
            }
        }
        self.node.transfer_to_host(
            self.handle,
            Box3d {
                x: 0,
                y: 0,
                z: 0,
                w: self.width,
                h: self.height,
                d: 1,
            },
            0,
            self.stride,
        )?;
        self.node.wait(self.handle)
    }
}

impl Drop for Bo {
    /// `gbm_bo_destroy`: this open's handle goes. A dmabuf exported from
    /// it, and a compositor's import of that, keep the buffer itself.
    fn drop(&mut self) {
        let _ = self.node.close(self.handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The format codes are DRM's, which a compositor checks them against.
    #[test]
    fn the_format_codes_are_drms() {
        let fourcc = |code: &[u8; 4]| u32::from_le_bytes(*code);
        assert_eq!(Format::Argb8888.fourcc(), fourcc(b"AR24"));
        assert_eq!(Format::Xrgb8888.fourcc(), fourcc(b"XR24"));
    }

    /// A use is a word of known bits, and no other.
    #[test]
    fn a_use_is_known_bits() {
        assert!(
            Usage::SCANOUT
                .and(Usage::RENDERING)
                .and(Usage::WRITE)
                .known()
        );
        assert!(Usage::LINEAR.known());
        assert!(!Usage(1 << 1).known(), "GBM_BO_USE_CURSOR is not offered");
    }
}
