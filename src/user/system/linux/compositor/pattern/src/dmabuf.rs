//! The pattern drawn into GPU buffers and handed over as dmabufs, rather
//! than into shared memory: `--dmabuf` (`docs/GPU.md` §3.13).
//!
//! The buffers come from `src/user/system/linux/compositor/gbm`, the pixels
//! are written into them with `gbm_bo_write`'s shape, and each becomes a
//! `wl_buffer` through `zwp_linux_dmabuf_v1`'s `create_immed`. The picture
//! is the same either way, which is what the test holds it to: a
//! compositor that showed the buffer it was handed, sampled where it lies,
//! shows exactly what the `wl_shm` client would have.
//!
//! Before its first buffer the client checks the render node's import
//! itself, on a second open of the node, and says each result on a line of
//! its own: a pipe is no dmabuf, its own export imports as its own handle,
//! and a buffer imported, let go of and imported again is imported again.
//! A boot requires every line (the consultant's C3 of 2026-10-07).

use std::os::fd::{AsFd, OwnedFd};

use compositor_gbm::{Bo, Device, Format, Usage};
use compositor_protocol::linux_dmabuf::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1};
use compositor_wire::{Arg, ArgType, Fd, ObjectId, Writer};

/// `EINVAL`, which the node answers a descriptor that is no buffer of its.
const EINVAL: i32 = 22;

/// The GPU side of a client that presents dmabufs.
#[derive(Debug)]
pub(crate) struct Gpu {
    device: Device,
    /// The buffers, one a `wl_buffer`, in the order the client fills them.
    bos: Vec<Bo>,
    /// The descriptors handed over with the last buffers made. A message
    /// carrying one is sent when the client flushes, which copies it, so
    /// each is kept until the next buffers are made.
    handed: Vec<OwnedFd>,
    /// Whether the import has been checked, which is done once.
    checked: bool,
}

impl Gpu {
    /// Open the render node.
    pub(crate) fn open() -> Result<Self, String> {
        Ok(Self {
            device: Device::open().map_err(|error| format!("the render node: {error}"))?,
            bos: Vec::new(),
            handed: Vec::new(),
            checked: false,
        })
    }

    /// Make `ids.len()` buffers of `width` × `height` and `format`, and a
    /// `wl_buffer` of each under the ids given: one parameters object each,
    /// one plane, `create_immed`, and the parameters destroyed.
    pub(crate) fn make(
        &mut self,
        out: &mut Writer,
        dmabuf: ObjectId,
        ids: &[(ObjectId, ObjectId)],
        (width, height): (u32, u32),
        format: Format,
    ) -> Result<(), String> {
        if !self.checked {
            self.check()?;
            self.checked = true;
        }
        self.bos.clear();
        self.handed.clear();
        for &(params, buffer) in ids {
            let bo = self
                .device
                .create(
                    width,
                    height,
                    format,
                    Usage::RENDERING.and(Usage::WRITE).and(Usage::LINEAR),
                )
                .map_err(|error| format!("a GPU buffer of {width}x{height}: {error}"))?;
            let fd = bo
                .export()
                .map_err(|error| format!("exporting a GPU buffer: {error}"))?;
            let (high, low) = (
                u32::try_from(bo.modifier() >> 32).unwrap_or(u32::MAX),
                u32::try_from(bo.modifier() & 0xffff_ffff).unwrap_or(u32::MAX),
            );
            let _ = out.write(
                dmabuf,
                zwp_linux_dmabuf_v1::request::CREATE_PARAMS,
                &[ArgType::NewId],
                &[Arg::NewId(params)],
            );
            let _ = out.write(
                params,
                zwp_linux_buffer_params_v1::request::ADD,
                &[
                    ArgType::Fd,
                    ArgType::Uint,
                    ArgType::Uint,
                    ArgType::Uint,
                    ArgType::Uint,
                    ArgType::Uint,
                ],
                &[
                    Arg::Fd(Fd(std::os::fd::AsRawFd::as_raw_fd(&fd))),
                    Arg::Uint(0),
                    Arg::Uint(bo.offset()),
                    Arg::Uint(bo.stride()),
                    Arg::Uint(high),
                    Arg::Uint(low),
                ],
            );
            let _ = out.write(
                params,
                zwp_linux_buffer_params_v1::request::CREATE_IMMED,
                &[
                    ArgType::NewId,
                    ArgType::Int,
                    ArgType::Int,
                    ArgType::Uint,
                    ArgType::Uint,
                ],
                &[
                    Arg::NewId(buffer),
                    Arg::Int(i32::try_from(width).unwrap_or(i32::MAX)),
                    Arg::Int(i32::try_from(height).unwrap_or(i32::MAX)),
                    Arg::Uint(format.fourcc()),
                    Arg::Uint(0),
                ],
            );
            let _ = out.write(
                params,
                zwp_linux_buffer_params_v1::request::DESTROY,
                &[],
                &[],
            );
            self.handed.push(fd);
            self.bos.push(bo);
        }
        Ok(())
    }

    /// Whether buffers have been made.
    pub(crate) fn made(&self) -> bool {
        !self.bos.is_empty()
    }

    /// Write `pixels` into buffer `slot` and move them to the GPU.
    pub(crate) fn write(&mut self, slot: usize, pixels: &[u8]) -> Result<(), String> {
        self.bos
            .get_mut(slot)
            .ok_or("no GPU buffer in that slot")?
            .write(pixels)
            .map_err(|error| format!("writing a GPU buffer: {error}"))
    }

    /// Check the node's import on a second open of it, and say so.
    fn check(&self) -> Result<(), String> {
        let node = self.device.node();
        let other = Device::open().map_err(|error| format!("a second render node: {error}"))?;
        let mut probe = self
            .device
            .create(1, 1, Format::Argb8888, Usage::LINEAR)
            .map_err(|error| format!("a GPU buffer: {error}"))?;
        let fd = probe
            .export()
            .map_err(|error| format!("exporting a GPU buffer: {error}"))?;

        // Not a dmabuf at all.
        let (reader, _writer) = std::io::pipe().map_err(|error| format!("a pipe: {error}"))?;
        match node.import(reader.as_fd()) {
            Err(error) if error.raw_os_error() == Some(EINVAL) => {
                say("pattern: import: a pipe is no dmabuf (EINVAL)");
            }
            Err(error) => return Err(format!("importing a pipe said {error}, not EINVAL")),
            Ok(handle) => return Err(format!("a pipe imported as handle {handle}")),
        }
        // Its own export, which is the handle it already has.
        let own = node
            .import(fd.as_fd())
            .map_err(|error| format!("importing its own export: {error}"))?;
        if own != probe.handle() {
            return Err(format!(
                "its own export imported as handle {own}, not its own {}",
                probe.handle()
            ));
        }
        say("pattern: import: its own export is its own handle");
        // Another open: imported, let go of, and imported again.
        let first = other
            .node()
            .import(fd.as_fd())
            .map_err(|error| format!("importing on a second open: {error}"))?;
        other
            .node()
            .close(first)
            .map_err(|error| format!("closing an import: {error}"))?;
        let again = other
            .node()
            .import(fd.as_fd())
            .map_err(|error| format!("importing again after a close: {error}"))?;
        let (resource, _) = other
            .node()
            .resource_info(again)
            .map_err(|error| format!("asking about an import: {error}"))?;
        if resource != probe.resource() {
            return Err(format!(
                "the import names resource {resource}, not the buffer's {}",
                probe.resource()
            ));
        }
        say("pattern: import: imported, closed and imported again on a second open");
        // The importer lets go altogether; the maker's buffer is still its.
        drop(other);
        probe
            .write(&[0x11, 0x22, 0x33, 0xff])
            .map_err(|error| format!("writing after the importer let go: {error}"))?;
        say("pattern: import: the maker's buffer still takes pixels after the importer let go");
        Ok(())
    }
}

/// Say a line, flushed, as the client's other lines are.
fn say(line: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}
