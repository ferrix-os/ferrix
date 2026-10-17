//! A fence as a descriptor: what `VIRTGPU_EXECBUFFER` answers with
//! `VIRTGPU_EXECBUF_FENCE_FD_OUT`.
//!
//! A sync file on Linux, and the part of one Venus uses (`docs/GPU.md` §6.1):
//! `poll` says `POLLIN` once the submission it was made for has finished on
//! the device, and never before. Venus simulates its sync objects on nothing
//! else -- it polls these, duplicates them and closes them -- so Linux's
//! `SYNC_IOC_*` ioctls, which merge and inspect sync files, are not answered.
//!
//! The descriptor holds the renderer, not the submission: a fence whose driver
//! has gone reads as signalled, with an error, because nothing will ever
//! signal it and a program waiting on it would wait for ever.

use alloc::sync::Arc;
use core::any::Any;

use ferrix_vfs::{Errno, Inode, Metadata, OpenFile, Readiness, Result as VfsResult};

use super::Renderer;
use crate::fs;

/// What `/proc/self/fd` calls one, as Linux names a sync file.
const NAME: &[u8] = b"anon_inode:sync_file";

/// One submission's fence.
pub(crate) struct Fence {
    renderer: Arc<Renderer>,
    fence: u64,
}

impl core::fmt::Debug for Fence {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Fence")
            .field("renderer", &self.renderer.index)
            .field("fence", &self.fence)
            .finish()
    }
}

/// An open descriptor for `fence` of `renderer`.
///
/// # Errors
///
/// What opening an anonymous file refuses.
pub(crate) fn open(renderer: Arc<Renderer>, fence: u64) -> Result<Arc<OpenFile>, Errno> {
    fs::anon::open(Arc::new(Fence { renderer, fence }), NAME, false)
}

impl Inode for Fence {
    fn metadata(&self) -> Metadata {
        fs::anon::metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// Readable once the work is done. Every answer the driver sends wakes
    /// the renderer's queue, so a wait on it hears the one that signals this.
    fn poll(&self) -> Readiness {
        let gone = self.renderer.is_gone();
        Readiness {
            readable: gone || self.renderer.fence_done(self.fence),
            writable: false,
            hangup: false,
            error: gone,
            priority: false,
        }
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(fs::wake::shared(self.renderer.changed()));
        true
    }

    /// The same queues, and the same answer, for its whole life.
    fn poll_queues_fixed(&self) -> bool {
        true
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(self.renderer.changed().wakes())
    }

    /// Nothing is read from a fence, which is what Linux answers too.
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(Errno::EINVAL)
    }
}
