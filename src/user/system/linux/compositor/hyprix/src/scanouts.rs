//! Frame buffers from nvidia-drm, for the vtest renderer to draw into
//! (`~/.local/share/ferrix/steam-race/zerocopy.md`, P5's first half).
//!
//! On the RTX 3060 hyprix's frames are drawn by virglrenderer's test server
//! on NVIDIA's EGL. Drawn into a texture of the server's own, every frame is
//! read back and copied to the card. Drawn into a buffer NVKMS can scan out
//! -- allocated here through nvidia-drm and handed to the server as a
//! dmabuf, which Ferrix's patched server imports -- the frame is already
//! where the display engine can show it, once the display core flips such
//! buffers (displayctl v9, the consultant's Z1-Z10).
//!
//! Every step falls back: no nvidia-drm node, a driver that will not export
//! the buffer (video memory before name-only dmabufs land), or a stock
//! server, and the frame is the server's texture as before. What happened
//! is noted for the log.

use std::cell::RefCell;
use std::io;
use std::rc::Rc;

use compositor_drm::Render;
use compositor_virgl::vtest::{Scanout, ScanoutSource};

/// Rows of a scanout buffer start on this many bytes, NVKMS's pitch
/// alignment on every GPU nvrm drives (nvrm's `kms.c` uses the same when
/// the caps say none).
const PITCH_ALIGN: u32 = 256;

/// Set to `1` to draw frames into nvidia-drm's buffers. Off by default
/// until the display core flips them (displayctl v9): before that the
/// frame is still fetched, from a buffer in system memory, which gains
/// nothing over the server's own texture.
const SCANOUT_ENV: &str = "FERRIX_VTEST_SCANOUT";

/// nvidia-drm's render node, and the lines for the log.
#[derive(Debug)]
pub(crate) struct NvidiaScanouts {
    node: Rc<Render>,
    notes: Rc<RefCell<Vec<String>>>,
}

/// The driver's handle for one buffer, let go of with it.
#[derive(Debug)]
struct Held {
    node: Rc<Render>,
    handle: u32,
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = self.node.close(self.handle);
    }
}

impl NvidiaScanouts {
    /// nvidia-drm's render node, if there is one and the environment asks
    /// for it (`FERRIX_VTEST_SCANOUT=1`); and where its notes will be.
    pub(crate) fn open() -> Option<(Self, Rc<RefCell<Vec<String>>>)> {
        if !std::env::var(SCANOUT_ENV).is_ok_and(|value| value.trim() == "1") {
            return None;
        }
        let node = Render::open_driven_by("nvidia-drm").ok()?;
        let notes = Rc::new(RefCell::new(Vec::new()));
        Some((
            Self {
                node: Rc::new(node),
                notes: Rc::clone(&notes),
            },
            notes,
        ))
    }
}

/// The stride of a `width`-pixel row, aligned for NVKMS.
fn stride(width: u32) -> Option<u32> {
    width.checked_mul(4)?.checked_next_multiple_of(PITCH_ALIGN)
}

impl ScanoutSource for NvidiaScanouts {
    fn allocate(&mut self, width: u32, height: u32) -> io::Result<Scanout> {
        let stride = stride(width).ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        let size = u64::from(stride) * u64::from(height);
        let handle = self.node.nvidia_alloc_scanout(size)?;
        let held = Held {
            node: Rc::clone(&self.node),
            handle,
        };
        // Video memory has no dmabuf until name-only ones land; the held
        // handle goes with the error.
        let fd = self.node.export(handle)?;
        Ok(Scanout {
            fd,
            stride,
            offset: 0,
            modifier: 0,
            keep: Box::new(held),
        })
    }

    fn note(&mut self, line: String) {
        self.notes.borrow_mut().push(line);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rows_are_aligned_for_nvkms() {
        assert_eq!(super::stride(1920), Some(7680));
        assert_eq!(super::stride(1366), Some(5632));
        assert_eq!(super::stride(1), Some(256));
        assert_eq!(super::stride(u32::MAX), None);
    }
}
