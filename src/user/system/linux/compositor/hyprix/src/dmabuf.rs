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
//! are drawn on the GPU -- or where the GPU is NVIDIA's (`docs/NVIDIA.md`
//! §4.6, N3b): its buffers are linear system memory the GPU wrote, so the
//! dmabuf is mapped and read as it is, and no node is asked at all.
//!
//! A third way is the test server's, where the frames are drawn through
//! `virgl_test_server` on the GPU's own EGL (`--renderer vtest`, N3c) and
//! the server is Ferrix's patched one: the server imports the descriptor as
//! an EGL image and samples it, which is the only way a buffer in video
//! memory -- NVIDIA's block-linear ones, which no processor can map -- is
//! shown at all. What that EGL says it imports is what is offered
//! ([`Importer::offered`]). A buffer that can be mapped is mapped and read,
//! as beside a screen drawn in software, and is not imported: memory that
//! is not the driver's own an EGL may take a copy of when it imports it
//! (NVIDIA's does, of a `udmabuf`), and a copy shows the frame the client
//! drew first for ever. One that cannot be mapped is the driver's own
//! memory, the same in every process that imports it, and is taken only
//! once a screen's renderer has imported it ([`Imported::needs_a_gpu`]).

use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::rc::Rc;

use compositor_drm::{Mapping, Render};
use compositor_server::{Dmabuf, Reported, Taken};

/// What draws the frames, which is what decides how a client's buffer can
/// be taken in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drawn {
    /// The processor, on every screen.
    Software,
    /// The GPU behind the render node (virgl).
    Node,
    /// The test server on the GPU's own EGL, with the modifiers that EGL
    /// says it imports every offered format with -- or `None` from a server
    /// that takes no dmabufs, a stock one.
    Server(Option<Vec<Reported>>),
}

/// How clients' dmabufs are taken in.
#[derive(Debug, Clone)]
pub enum Importer {
    /// Through this compositor's open of the render node, whose GPU draws
    /// the frames.
    Node(Rc<Render>),
    /// Mapped from the descriptor itself, for screens drawn in software
    /// beside a GPU whose buffers are linear memory: NVIDIA's.
    Map,
    /// By the test server the frames are drawn through, as an EGL image:
    /// the modifiers its EGL reported. A buffer that can be mapped is
    /// mapped instead.
    Sampled(Rc<[Reported]>),
}

impl Importer {
    /// The importer for frames drawn as `drawn` says.
    ///
    /// A test server that imports is asked to. One that does not, and
    /// screens drawn in software, map the buffers of an NVIDIA render node
    /// if there is one, found by its driver's name (the consultant's B8,
    /// ledger 316): the server's own node speaks NVIDIA's ioctls, not
    /// virgl's, and nothing could be imported through it. Otherwise frames
    /// drawn on a GPU import through the first render node.
    #[must_use]
    pub fn find(drawn: &Drawn) -> Option<Self> {
        Self::pick(drawn, || Render::open_driven_by("nvidia-drm").is_ok()).or_else(|| {
            matches!(drawn, Drawn::Node | Drawn::Server(None))
                .then(Render::open)
                .and_then(Result::ok)
                .map(|node| Self::Node(Rc::new(node)))
        })
    }

    /// [`Importer::find`]'s choice among the ways that need no render node
    /// of virgl's, with `nvidia` saying whether there is an NVIDIA one.
    fn pick(drawn: &Drawn, nvidia: impl FnOnce() -> bool) -> Option<Self> {
        match drawn {
            Drawn::Server(Some(reported)) => Some(Self::Sampled(Rc::from(reported.as_slice()))),
            Drawn::Software | Drawn::Server(None) => nvidia().then_some(Self::Map),
            Drawn::Node => None,
        }
    }

    /// The modifiers to offer clients: what this importer can take in.
    #[must_use]
    pub fn offered(&self) -> Vec<u64> {
        compositor_server::modifiers_offered(match self {
            Self::Node(_) => Taken::Node,
            // Buffers mapped from the CPU must be linear: offered alone, so
            // NVIDIA's GBM backend allocates pitch-linear system memory.
            Self::Map => Taken::Mapped,
            Self::Sampled(reported) => Taken::Sampled(reported),
        })
    }
}

/// One imported buffer.
pub struct Imported {
    /// The node and this open's handle for it, for one imported through a
    /// node.
    node: Option<(Rc<Render>, u32)>,
    fd: OwnedFd,
    /// The buffer's backing, where it has one a processor can read.
    mapping: Option<Mapping>,
    /// Whether a GPU renderer is handed the descriptor to import.
    sampled: bool,
    /// The layout its client said it has.
    modifier: u64,
}

impl core::fmt::Debug for Imported {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Imported")
            .field("handle", &self.node.as_ref().map(|(_, handle)| *handle))
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
    pub fn new(importer: &Importer, fd: OwnedFd, dmabuf: &Dmabuf) -> io::Result<Self> {
        let modifier = dmabuf.plane.modifier;
        let node = match importer {
            Importer::Node(node) => node,
            Importer::Map => {
                let mapping = Mapping::of_dmabuf(fd.as_fd())?;
                Self::fits(mapping.bytes().len() as u64, dmabuf)?;
                return Ok(Self {
                    node: None,
                    fd,
                    mapping: Some(mapping),
                    sampled: false,
                    modifier,
                });
            }
            Importer::Sampled(_) => {
                // Video memory has no mapping (`ENODEV` from a name-only
                // dmabuf): the renderer's import is then all there is, and
                // it checks the size the server sees against the rows.
                let mapping = Mapping::of_dmabuf(fd.as_fd()).ok();
                if let Some(mapping) = &mapping {
                    Self::fits(mapping.bytes().len() as u64, dmabuf)?;
                }
                return Ok(Self {
                    node: None,
                    fd,
                    sampled: mapping.is_none(),
                    mapping,
                    modifier,
                });
            }
        };
        let handle = node.import(fd.as_fd())?;
        let made = Self::checked(node, handle, dmabuf);
        match made {
            Ok(mapping) => Ok(Self {
                node: Some((Rc::clone(node), handle)),
                fd,
                mapping: Some(mapping),
                sampled: true,
                modifier,
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
        Self::fits(u64::from(size), dmabuf)?;
        node.map(handle, size as usize)
    }

    /// Whether a buffer of `size` bytes holds the rows the client said.
    fn fits(size: u64, dmabuf: &Dmabuf) -> io::Result<()> {
        // Every row a whole stride, the last one included: the range a
        // `wl_shm` buffer is held to as well, and the one the pixels are
        // read from.
        let rows = u64::from(dmabuf.plane.stride) * u64::from(dmabuf.height.unsigned_abs());
        let end = u64::from(dmabuf.plane.offset) + rows;
        if end > size {
            return Err(io::Error::other(format!(
                "a buffer of {size} bytes holds no {}x{} rows of {} bytes from {}",
                dmabuf.width, dmabuf.height, dmabuf.plane.stride, dmabuf.plane.offset
            )));
        }
        Ok(())
    }

    /// The descriptor, which a GPU renderer imports into its own context:
    /// only for a buffer imported through the node, or one in video memory
    /// taken for the test server to sample.
    #[must_use]
    pub fn device_fd(&self) -> Option<BorrowedFd<'_>> {
        self.sampled.then(|| self.fd.as_fd())
    }

    /// The buffer's backing: what a screen drawn in software reads. Empty
    /// for a buffer in video memory.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.mapping.as_ref().map_or(&[], Mapping::bytes)
    }

    /// Whether no processor can read the buffer: it is shown only by a
    /// renderer that imported it, so it is taken only once one has.
    #[must_use]
    pub const fn needs_a_gpu(&self) -> bool {
        self.mapping.is_none()
    }

    /// The layout the client said the buffer has.
    #[must_use]
    pub const fn modifier(&self) -> u64 {
        self.modifier
    }
}

impl Drop for Imported {
    fn drop(&mut self) {
        // The mapping goes with the value; the handle is let go of here,
        // and the object with it once the client's own handle and every
        // screen's import have gone too.
        if let Some((node, handle)) = &self.node {
            let _ = node.close(*handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Drawn, Importer};
    use compositor_server::{MOD_INVALID, MOD_LINEAR, Reported};

    /// What NVIDIA's EGL reports (580.173.02), in short: two block-linear
    /// layouts for 2D textures and linear for external ones only.
    fn nvidia() -> Vec<Reported> {
        vec![
            Reported {
                modifier: 0x0300_0000_0060_6014,
                external_only: false,
            },
            Reported {
                modifier: 0x0300_0000_00e0_8014,
                external_only: false,
            },
            Reported {
                modifier: MOD_LINEAR,
                external_only: true,
            },
        ]
    }

    /// A test server that imports is the importer whether or not there is
    /// an NVIDIA node, and offers what its EGL reported and linear -- never
    /// the implicit layout.
    #[test]
    fn a_server_that_imports_offers_what_its_egl_reported() {
        for nvidia_node in [true, false] {
            let importer = Importer::pick(&Drawn::Server(Some(nvidia())), || nvidia_node)
                .expect("an importer");
            assert!(matches!(importer, Importer::Sampled(_)));
            assert_eq!(
                importer.offered(),
                [0x0300_0000_0060_6014, 0x0300_0000_00e0_8014, MOD_LINEAR]
            );
        }
        // A patched server whose EGL names no modifiers still takes linear.
        let importer =
            Importer::pick(&Drawn::Server(Some(Vec::new())), || false).expect("an importer");
        assert_eq!(importer.offered(), [MOD_LINEAR]);
    }

    /// Software screens, and a stock test server, map NVIDIA's buffers and
    /// offer linear alone, as before; without an NVIDIA node they have no
    /// importer of this kind.
    #[test]
    fn the_software_path_offers_linear_alone() {
        for drawn in [Drawn::Software, Drawn::Server(None)] {
            let importer = Importer::pick(&drawn, || true).expect("an importer");
            assert!(matches!(importer, Importer::Map));
            assert_eq!(importer.offered(), [MOD_LINEAR]);
            assert!(!importer.offered().contains(&MOD_INVALID));
            assert!(Importer::pick(&drawn, || false).is_none());
        }
        // Frames drawn through the render node are the node's to import,
        // whatever else is in the machine.
        assert!(Importer::pick(&Drawn::Node, || true).is_none());
    }
}
