//! A client's GPU buffer, imported: what a `zwp_linux_dmabuf_v1` buffer is
//! to this compositor (`docs/GPU.md` §3.13).
//!
//! `src/user/system/linux/compositor/server` checks the protocol and hands
//! the descriptor over; this imports it into the compositor's own open of
//! the render node, which is where the kernel says whether it is a buffer of
//! this GPU at all, and keeps it for as long as its `wl_buffer` lives.
//!
//! Two things come of the import. The descriptor, which each screen's GPU
//! renderer imports into its own context and samples where it lies -- no
//! copy, the client's pixels never leave the device. And the buffer's
//! backing, mapped, which is what a screen drawn in software reads instead:
//! the bytes a client wrote and moved to the device, as `gbm_bo_write`
//! does. A client that drew on the GPU and never moved its bytes back has
//! nothing there, which is why the global is offered only where the frames
//! are drawn on the GPU.

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::rc::Rc;

use compositor_drm::{Mapping, Render};
use compositor_server::Dmabuf;

/// One imported buffer.
pub struct Imported {
    node: Rc<Render>,
    fd: OwnedFd,
    handle: u32,
    mapping: Mapping,
}

impl core::fmt::Debug for Imported {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Imported")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl Imported {
    /// Import `fd`, which a client said is `dmabuf`.
    ///
    /// # Errors
    ///
    /// The node's: a descriptor that is not a buffer of this GPU. And a
    /// buffer too small for the rows the client said it holds, which is the
    /// protocol's `out_of_bounds` found where only the import can find it.
    pub fn new(node: &Rc<Render>, fd: OwnedFd, dmabuf: &Dmabuf) -> io::Result<Self> {
        let handle = node.import(fd.as_fd())?;
        let made = Self::checked(node, handle, dmabuf);
        match made {
            Ok(mapping) => Ok(Self {
                node: Rc::clone(node),
                fd,
                handle,
                mapping,
            }),
            Err(error) => {
                let _ = node.close(handle);
                Err(error)
            }
        }
    }

    /// The buffer's size against what the client said, and its backing
    /// mapped.
    fn checked(node: &Render, handle: u32, dmabuf: &Dmabuf) -> io::Result<Mapping> {
        let (_, size) = node.resource_info(handle)?;
        // Every row a whole stride, the last one included: the range a
        // `wl_shm` buffer is held to as well, and the one the pixels are
        // read from.
        let rows = u64::from(dmabuf.plane.stride) * u64::from(dmabuf.height.unsigned_abs());
        let end = u64::from(dmabuf.plane.offset) + rows;
        if end > u64::from(size) {
            return Err(io::Error::other(format!(
                "a buffer of {size} bytes holds no {}x{} rows of {} bytes from {}",
                dmabuf.width, dmabuf.height, dmabuf.plane.stride, dmabuf.plane.offset
            )));
        }
        node.map(handle, size as usize)
    }

    /// The descriptor, which a GPU renderer imports into its own context.
    #[must_use]
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// The buffer's backing: what a screen drawn in software reads.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.mapping.bytes()
    }
}

impl Drop for Imported {
    fn drop(&mut self) {
        // The mapping goes with the value; the handle is let go of here,
        // and the object with it once the client's own handle and every
        // screen's import have gone too.
        let _ = self.node.close(self.handle);
    }
}
