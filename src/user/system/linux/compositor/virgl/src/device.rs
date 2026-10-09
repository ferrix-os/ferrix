//! What a renderer needs of whatever runs its streams.
//!
//! Two things do: `/dev/dri/renderD128` in a guest, and virglrenderer's test
//! server on a host. A renderer written against this trait is the same code
//! on both, which is the point -- every shader and every pass is tested on
//! the host, in a second, against the very renderer that a guest's frames
//! will reach through QEMU.

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};

use crate::Region;

/// A texture to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Texture {
    /// Its width in pixels.
    pub width: u32,
    /// Its height in pixels.
    pub height: u32,
    /// `VIRGL_FORMAT_*`.
    pub format: u32,
    /// `VIRGL_BIND_*`: what it may be used as.
    pub bind: u32,
    /// Whether pixels will be moved to or from it. One that is only ever
    /// drawn into and sampled from -- a blur's scratch -- needs no memory on
    /// this side at all, and on a guest that is the difference between a
    /// page and a screen's worth of pinned pages.
    pub moved: bool,
    /// Whether a screen may be shown it: what a compositor's own frame is
    /// made with, so that the finished picture never has to leave the
    /// device. It costs the texture nothing to say so and a device may
    /// refuse a scanout of one that did not.
    pub scanout: bool,
}

/// Somewhere streams run.
pub trait Device: core::fmt::Debug {
    /// Make a texture, and answer the number a stream names it by.
    ///
    /// # Errors
    ///
    /// The device's.
    fn texture(&mut self, texture: Texture) -> io::Result<u32>;

    /// Make a buffer of `bytes`, for vertices.
    ///
    /// # Errors
    ///
    /// The device's.
    fn buffer(&mut self, bytes: u32) -> io::Result<u32>;

    /// Write pixels into `region` of a texture made to be `moved`. `data`
    /// begins at the region's first pixel and its rows are `stride` bytes
    /// apart, which is how a client's padded buffer is handed over without
    /// being gathered first.
    ///
    /// # Errors
    ///
    /// The device's; a `data` too short for the region.
    fn upload(&mut self, resource: u32, region: Region, stride: u32, data: &[u8])
    -> io::Result<()>;

    /// Run a stream. It may still be running when this returns; a
    /// [`Device::read`] comes after it.
    ///
    /// # Errors
    ///
    /// The device's. What the *renderer* made of the words is not among
    /// them: a stream it could not run is a picture that is wrong.
    fn submit(&mut self, words: &[u32]) -> io::Result<()>;

    /// Read `region` of a texture made to be `moved` back: four bytes a
    /// pixel, rows packed.
    ///
    /// # Errors
    ///
    /// The device's.
    fn read(&mut self, resource: u32, region: Region) -> io::Result<Vec<u8>>;

    /// [`Device::read`] into `into`, rows `stride` bytes apart: a screen's
    /// buffer, say, so that a device that can put the pixels there itself
    /// saves the copy through a vector.
    ///
    /// # Errors
    ///
    /// The device's; an `into` too short for the region.
    fn read_into(
        &mut self,
        resource: u32,
        region: Region,
        into: &mut [u8],
        stride: usize,
    ) -> io::Result<()> {
        let pixels = self.read(resource, region)?;
        let row = region.width as usize * 4;
        if row == 0 {
            return Ok(());
        }
        for (index, from) in pixels.chunks_exact(row).enumerate() {
            let start = index * stride;
            let target = into
                .get_mut(start..start + row)
                .ok_or_else(|| io::Error::other("pixels too short for their region"))?;
            target.copy_from_slice(from);
        }
        Ok(())
    }

    /// A descriptor for `resource` that a card can be shown, if this device
    /// is the kind that has one.
    ///
    /// `None` is a device whose resources no screen can be pointed at --
    /// a test server, which has no card beside it -- and the answer for a
    /// renderer that asks is to fetch its frame instead.
    ///
    /// # Errors
    ///
    /// The device's.
    fn export(&mut self, resource: u32) -> io::Result<Option<OwnedFd>> {
        let _ = resource;
        Ok(None)
    }

    /// Take a buffer another program made on this device and exported as a
    /// dmabuf, and answer the number a stream names it by here, if this
    /// device is the kind that can (`docs/GPU.md` §3.13).
    ///
    /// The resource is the other program's: a renderer samples it and never
    /// moves pixels to or from it, and lets go of it with
    /// [`Device::release`] like any other. `None` is a device with no such
    /// thing -- a test server -- and the answer for a renderer that asks is
    /// to upload the pixels it was also given.
    ///
    /// # Errors
    ///
    /// The device's: a descriptor that is not a buffer of this device.
    fn import(&mut self, fd: BorrowedFd<'_>) -> io::Result<Option<u32>> {
        let _ = fd;
        Ok(None)
    }

    /// Let go of a resource nothing will name again.
    ///
    /// A renderer keeps a texture for the next thing of its size, because
    /// making one costs a message and a pinned backing; what it may not do
    /// is keep every size anything ever was. This is how the ones it has
    /// given up on go.
    ///
    /// # Errors
    ///
    /// The device's. A caller that cannot let go keeps the resource, which
    /// is not wrong -- only wasteful.
    fn release(&mut self, resource: u32) -> io::Result<()>;
}

/// A device behind a pointer is one too, which is how a compositor holds
/// "whichever there was" without being written twice.
impl<D: Device + ?Sized> Device for Box<D> {
    fn texture(&mut self, texture: Texture) -> io::Result<u32> {
        (**self).texture(texture)
    }

    fn buffer(&mut self, bytes: u32) -> io::Result<u32> {
        (**self).buffer(bytes)
    }

    fn upload(
        &mut self,
        resource: u32,
        region: Region,
        stride: u32,
        data: &[u8],
    ) -> io::Result<()> {
        (**self).upload(resource, region, stride, data)
    }

    fn submit(&mut self, words: &[u32]) -> io::Result<()> {
        (**self).submit(words)
    }

    fn read(&mut self, resource: u32, region: Region) -> io::Result<Vec<u8>> {
        (**self).read(resource, region)
    }

    fn read_into(
        &mut self,
        resource: u32,
        region: Region,
        into: &mut [u8],
        stride: usize,
    ) -> io::Result<()> {
        (**self).read_into(resource, region, into, stride)
    }

    fn release(&mut self, resource: u32) -> io::Result<()> {
        (**self).release(resource)
    }

    fn export(&mut self, resource: u32) -> io::Result<Option<OwnedFd>> {
        (**self).export(resource)
    }

    fn import(&mut self, fd: BorrowedFd<'_>) -> io::Result<Option<u32>> {
        (**self).import(fd)
    }
}
