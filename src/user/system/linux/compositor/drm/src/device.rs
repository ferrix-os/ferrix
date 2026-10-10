//! The render node as somewhere `src/user/system/linux/compositor/virgl`'s streams run.
//!
//! [`compositor_virgl::Device`] is what a GPU renderer asks of whatever runs
//! its streams, and on a host that is virglrenderer's test server. In a
//! guest it is this: `/dev/dri/renderD128`, a resource a texture, its
//! backing mapped where pixels are to be moved, and a transfer either way.
//!
//! A texture that pixels are moved to or from has a backing as big as
//! itself, which the kernel pins for the device for as long as the texture
//! lives. One that is only drawn into and sampled from -- a blur's pyramid
//! -- is asked for with a page: `VIRTGPU_RESOURCE_CREATE`'s `size` is how
//! much backing, not how big the texture is, on Ferrix as on Linux.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};

use compositor_virgl::{Device, Region, Texture, pipe};
use ferrix_linux_abi::virtgpu::{Box3d, ResourceCreate};

use crate::render::{Mapping, Render};

/// The backing of a resource nothing is moved to or from: a page.
const NO_BACKING: u32 = 4096;

/// A texture pixels are moved to or from: where its backing is mapped.
#[derive(Debug)]
struct Moved {
    handle: u32,
    mapping: Mapping,
    width: u32,
    height: u32,
    /// Whether an upload from the backing may still be on its way: the
    /// backing is the device's to read until a wait says it is done.
    moving: bool,
}

/// An open render node, as a [`Device`].
#[derive(Debug)]
pub struct RenderDevice {
    node: Render,
    moved: BTreeMap<u32, Moved>,
    /// Buffers other programs made and this one imported, by resource: the
    /// handle each is held by here and how many imports name it.
    imported: BTreeMap<u32, (u32, u32)>,
}

impl RenderDevice {
    /// Open the render node.
    ///
    /// # Errors
    ///
    /// Whatever `open` said; `ENOENT` when the card has no GPU behind it.
    pub fn open() -> io::Result<Self> {
        Ok(Self {
            node: Render::open()?,
            moved: BTreeMap::new(),
            imported: BTreeMap::new(),
        })
    }

    /// Who is driving the node: `virtio_gpu` is the one whose streams
    /// `src/user/system/linux/compositor/virgl` writes.
    ///
    /// # Errors
    ///
    /// Whatever the node said.
    pub fn driver(&self) -> io::Result<String> {
        self.node.driver()
    }
}

/// Where `region`'s first byte is in a backing `width` pixels wide.
fn offset_of(region: Region, width: u32) -> Option<u32> {
    region
        .y
        .checked_mul(width)?
        .checked_add(region.x)?
        .checked_mul(4)
}

/// `region`, checked to lie inside a `width` by `height` texture.
fn inside(region: Region, width: u32, height: u32) -> io::Result<()> {
    let fits = region.width > 0
        && region.height > 0
        && region
            .x
            .checked_add(region.width)
            .is_some_and(|right| right <= width)
        && region
            .y
            .checked_add(region.height)
            .is_some_and(|bottom| bottom <= height);
    if fits {
        Ok(())
    } else {
        Err(io::Error::other("a region outside its texture"))
    }
}

const fn box_of(region: Region) -> Box3d {
    Box3d {
        x: region.x,
        y: region.y,
        z: 0,
        w: region.width,
        h: region.height,
        d: 1,
    }
}

impl Device for RenderDevice {
    fn texture(&mut self, texture: Texture) -> io::Result<u32> {
        let bytes = texture
            .width
            .checked_mul(texture.height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| io::Error::other("a texture too large to describe"))?;
        let bind = if texture.scanout {
            texture.bind | pipe::BIND_SCANOUT
        } else {
            texture.bind
        };
        let (handle, resource) = self.node.create(ResourceCreate {
            target: pipe::TEXTURE_2D,
            format: texture.format,
            bind,
            width: texture.width,
            height: texture.height,
            depth: 1,
            array_size: 1,
            last_level: 0,
            nr_samples: 0,
            // Not `VIRGL_RESOURCE_Y_0_TOP`. It says which way up a
            // texture's rows are, and *both* the transfers and the scanout
            // honour it, while what a renderer draws into the texture is
            // unaffected by it -- so setting it turned the screen and the
            // readback upside down together, which is what the first GPU
            // scanout showed. The rows stay the way GL keeps them and the
            // two readers agree.
            flags: 0,
            bo_handle: 0,
            res_handle: 0,
            size: if texture.moved { bytes } else { NO_BACKING },
            stride: texture.width.saturating_mul(4),
        })?;
        if texture.moved {
            let mapping = self.node.map(handle, bytes as usize)?;
            let _ = self.moved.insert(
                resource,
                Moved {
                    handle,
                    mapping,
                    width: texture.width,
                    height: texture.height,
                    moving: false,
                },
            );
        }
        Ok(resource)
    }

    fn buffer(&mut self, bytes: u32) -> io::Result<u32> {
        let (_, resource) = self.node.create_resource(bytes)?;
        Ok(resource)
    }

    fn upload(
        &mut self,
        resource: u32,
        region: Region,
        stride: u32,
        data: &[u8],
    ) -> io::Result<()> {
        let held = self
            .moved
            .get_mut(&resource)
            .ok_or_else(|| io::Error::other("a texture with no backing to write"))?;
        inside(region, held.width, held.height)?;
        let first = offset_of(region, held.width)
            .ok_or_else(|| io::Error::other("a region too far in to describe"))?;
        // The last upload returned when it was on its way; its bytes may
        // still be being read. By the next frame they almost always have
        // been, and the wait costs no trip to the device.
        if held.moving {
            self.node.wait(held.handle)?;
            held.moving = false;
        }
        let own_stride = held.width as usize * 4;
        let row_bytes = region.width as usize * 4;
        // Row by row into the backing, at the place the region has in the
        // texture: the device is then told the region and where it begins.
        let rows = data.chunks(stride.max(1) as usize);
        let into = held
            .mapping
            .bytes_mut()
            .get_mut(first as usize..)
            .unwrap_or(&mut [])
            .chunks_mut(own_stride);
        let mut written = 0;
        for (from, to) in rows.zip(into).take(region.height as usize) {
            let (Some(from), Some(to)) = (from.get(..row_bytes), to.get_mut(..row_bytes)) else {
                break;
            };
            to.copy_from_slice(from);
            written += 1;
        }
        if written != region.height {
            return Err(io::Error::other("pixels too short for their region"));
        }
        self.node.transfer_to_host(
            held.handle,
            box_of(region),
            first,
            held.width.saturating_mul(4),
        )?;
        held.moving = true;
        Ok(())
    }

    fn submit(&mut self, words: &[u32]) -> io::Result<()> {
        self.node.exec(words)
    }

    fn export(&mut self, resource: u32) -> io::Result<Option<OwnedFd>> {
        // Only a texture pixels move to or from is written down, and that is
        // what a compositor draws its frame into; a blur's scratch has no
        // handle to export and no reason to be shown.
        let Some(held) = self.moved.get(&resource) else {
            return Ok(None);
        };
        self.node.export(held.handle).map(Some)
    }

    fn import(
        &mut self,
        fd: BorrowedFd<'_>,
        _layout: &compositor_virgl::Layout,
    ) -> io::Result<Option<u32>> {
        // The node knows its own buffer's layout; the client's word for it
        // is not needed.
        let handle = self.node.import(fd)?;
        let (resource, _) = match self.node.resource_info(handle) {
            Ok(info) => info,
            Err(error) => {
                let _ = self.node.close(handle);
                return Err(error);
            }
        };
        // The node answers one handle an object an open, so a buffer
        // imported twice is one handle here too, closed with the last
        // release that names it.
        self.imported.entry(resource).or_insert((handle, 0)).1 += 1;
        Ok(Some(resource))
    }

    fn release(&mut self, resource: u32) -> io::Result<()> {
        if let Some((handle, count)) = self.imported.get_mut(&resource) {
            *count = count.saturating_sub(1);
            if *count > 0 {
                return Ok(());
            }
            let handle = *handle;
            let _ = self.imported.remove(&resource);
            return self.node.close(handle);
        }
        // A texture pixels are moved to or from is known by its resource;
        // one that is not was never written down, and there is no handle to
        // close. That is a renderer letting go of something it never
        // tracked, which is not an error.
        let Some(held) = self.moved.remove(&resource) else {
            return Ok(());
        };
        self.node.close(held.handle)
    }

    fn read(&mut self, resource: u32, region: Region) -> io::Result<Vec<u8>> {
        let held = self
            .moved
            .get(&resource)
            .ok_or_else(|| io::Error::other("a texture with no backing to read"))?;
        inside(region, held.width, held.height)?;
        let first = offset_of(region, held.width)
            .ok_or_else(|| io::Error::other("a region too far in to describe"))?;
        self.node.transfer_from_host(
            held.handle,
            box_of(region),
            first,
            held.width.saturating_mul(4),
        )?;
        self.node.wait(held.handle)?;
        let own_stride = held.width as usize * 4;
        let row_bytes = region.width as usize * 4;
        let mut data = Vec::with_capacity(row_bytes * region.height as usize);
        let rows = held
            .mapping
            .bytes()
            .get(first as usize..)
            .unwrap_or(&[])
            .chunks(own_stride);
        for row in rows.take(region.height as usize) {
            data.extend_from_slice(row.get(..row_bytes).unwrap_or(&[]));
        }
        Ok(data)
    }
}
