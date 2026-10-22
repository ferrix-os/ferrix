//! Buffers on an Intel render node, for `--dmabuf` beside the i915 uAPI
//! (`docs/INTEL-GPU.md` §2.1): what a game's Vulkan swap chain hands the
//! compositor on the N150, made here by hand so that a test can show it.
//!
//! A buffer is a GEM object (`DRM_IOCTL_I915_GEM_CREATE`), mapped write-back
//! (`DRM_IOCTL_I915_GEM_MMAP_OFFSET` with `I915_MMAP_OFFSET_WB`), written by
//! the processor and exported as a dmabuf (`DRM_IOCTL_PRIME_HANDLE_TO_FD`).
//! Its rows are linear, 64 bytes apart at least, as anv lays out a linear
//! image; the one modifier is `DRM_FORMAT_MOD_LINEAR`. Every number is
//! Linux v7.0's `include/uapi/drm/i915_drm.h` and `drm.h`, as the driver's
//! own probe printed them (`src/lib/drivers/gpu/intel-xelp/probe/i915-64.txt`).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use compositor_drm::Render;

/// `DRM_IOCTL_I915_GEM_CREATE`: `_IOWR('d', 0x5b, struct drm_i915_gem_create)`.
const GEM_CREATE: u64 = 0xC010_645B;
/// `DRM_IOCTL_I915_GEM_MMAP_OFFSET`: `_IOWR('d', 0x64, struct drm_i915_gem_mmap_offset)`.
const GEM_MMAP_OFFSET: u64 = 0xC020_6464;
/// `DRM_IOCTL_PRIME_HANDLE_TO_FD`: `_IOWR('d', 0x2d, struct drm_prime_handle)`.
const PRIME_HANDLE_TO_FD: u64 = 0xC00C_642D;
/// `I915_MMAP_OFFSET_WB`.
const MMAP_OFFSET_WB: u64 = 2;
/// `DRM_CLOEXEC | DRM_RDWR`.
const PRIME_FLAGS: u32 = 0x0008_0000 | 0x2;
/// Row alignment, as anv aligns a linear image's rows.
const ROW_ALIGN: u32 = 64;
/// A page.
const PAGE: u64 = 4096;

/// `struct drm_i915_gem_create`.
#[repr(C)]
struct Create {
    size: u64,
    handle: u32,
    pad: u32,
}

/// `struct drm_i915_gem_mmap_offset`.
#[repr(C)]
struct MmapOffset {
    handle: u32,
    pad: u32,
    offset: u64,
    flags: u64,
    extensions: u64,
}

/// `struct drm_prime_handle`.
#[repr(C)]
struct Prime {
    handle: u32,
    flags: u32,
    fd: i32,
}

/// The render node, when its driver is i915.
#[derive(Debug)]
pub(crate) struct I915 {
    node: Render,
}

/// One buffer: its handle, its rows mapped, and its shape.
#[derive(Debug)]
pub(crate) struct Bo {
    fd: libc::c_int,
    handle: u32,
    map: *mut u8,
    len: usize,
    width: u32,
    height: u32,
    stride: u32,
}

impl I915 {
    /// The first render node driven by i915, if there is one.
    pub(crate) fn open() -> io::Result<Self> {
        Ok(Self {
            node: Render::open_driven_by("i915")?,
        })
    }

    /// `ioctl` on the node, the argument in place.
    fn ioctl<T>(&self, request: u64, argument: &mut T) -> io::Result<()> {
        // SAFETY: `argument` is the `repr(C)` structure `request`'s number
        // encodes the size of, live and writable for the call.
        let result = unsafe {
            libc::ioctl(
                self.node.as_raw_fd(),
                request as _,
                core::ptr::from_mut(argument),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// A linear buffer `width` × `height`, four bytes a pixel, mapped.
    pub(crate) fn create(&self, width: u32, height: u32) -> io::Result<Bo> {
        let stride = width
            .checked_mul(4)
            .and_then(|row| row.checked_next_multiple_of(ROW_ALIGN))
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        let size = u64::from(stride)
            .checked_mul(u64::from(height))
            .and_then(|size| size.checked_next_multiple_of(PAGE))
            .filter(|&size| size > 0)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        let mut create = Create {
            size,
            handle: 0,
            pad: 0,
        };
        self.ioctl(GEM_CREATE, &mut create)?;
        let mut offset = MmapOffset {
            handle: create.handle,
            pad: 0,
            offset: 0,
            flags: MMAP_OFFSET_WB,
            extensions: 0,
        };
        self.ioctl(GEM_MMAP_OFFSET, &mut offset)?;
        let len = usize::try_from(create.size).map_err(|_| io::Error::other("too large"))?;
        // SAFETY: a fresh shared mapping of the object's `len` bytes at the
        // offset the node gave; nothing else in this program refers to it.
        let map = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.node.as_raw_fd(),
                libc::off_t::try_from(offset.offset).map_err(|_| io::Error::other("offset"))?,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Bo {
            fd: self.node.as_raw_fd(),
            handle: create.handle,
            map: map.cast(),
            len,
            width,
            height,
            stride,
        })
    }
}

impl Bo {
    /// The dmabuf.
    pub(crate) fn export(&self) -> io::Result<OwnedFd> {
        let mut prime = Prime {
            handle: self.handle,
            flags: PRIME_FLAGS,
            fd: -1,
        };
        // SAFETY: as `I915::ioctl`: the structure the number encodes.
        let result = unsafe {
            libc::ioctl(
                self.fd,
                PRIME_HANDLE_TO_FD as _,
                core::ptr::from_mut(&mut prime),
            )
        };
        if result < 0 || prime.fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the node handed this program a new descriptor.
        Ok(unsafe { OwnedFd::from_raw_fd(prime.fd) })
    }

    /// Bytes from one row's start to the next.
    pub(crate) const fn stride(&self) -> u32 {
        self.stride
    }

    /// Write `pixels`, rows of `width * 4` bytes one after another.
    pub(crate) fn write(&mut self, pixels: &[u8]) -> io::Result<()> {
        let row = self.width as usize * 4;
        if pixels.len() != row * self.height as usize {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // SAFETY: the mapping is this buffer's own, `len` bytes, live until
        // drop, and nothing else here holds a reference into it.
        let bytes = unsafe { core::slice::from_raw_parts_mut(self.map, self.len) };
        for (from, into) in pixels
            .chunks(row)
            .zip(bytes.chunks_mut(self.stride as usize))
        {
            if let Some(into) = into.get_mut(..row) {
                into.copy_from_slice(from);
            }
        }
        Ok(())
    }
}

impl Drop for Bo {
    fn drop(&mut self) {
        // SAFETY: the mapping `create` made, unmapped once. The object goes
        // with the node's handle when the node closes; the exported dmabuf
        // keeps it alive for whoever holds that.
        let _ = unsafe { libc::munmap(self.map.cast(), self.len) };
    }
}
