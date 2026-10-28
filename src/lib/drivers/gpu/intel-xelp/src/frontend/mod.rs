// SPDX-License-Identifier: MIT
//
// Translated from Linux v7.0, drivers/gpu/drm/drm_ioctl.c, drm_gem.c,
// drm_prime.c and drm_syncobj.c, and drivers/gpu/drm/i915/i915_getparam.c,
// i915_query.c, gem/i915_gem_create.c, gem/i915_gem_mman.c,
// gem/i915_gem_context.c, gem/i915_gem_execbuffer.c, gem/i915_gem_wait.c,
// gem/i915_gem_busy.c, gem/i915_gem_tiling.c and gem/i915_gem_domain.c:
// Copyright © 2008-2021 Intel Corporation (i915)
// Copyright 2017 Red Hat (drm_syncobj.c)
// Copyright 2012 Red Hat (drm_prime.c)
// Copyright © 2008 Intel Corporation (drm_gem.c)
// Copyright 1999 Precision Insight, Inc.; Copyright 2000 VA Linux Systems,
// Inc. (drm_ioctl.c)
// Under the MIT notice in this crate's i915/NOTICE (the DRM core files carry
// the same MIT permission notice).

//! The i915 uAPI front end (`docs/INTEL-GPU.md` §2.1, §6, M3): every ioctl,
//! open, mmap and release a program makes on the driver's render node, and
//! the dmabuf events the chardev core passes on, answered as Linux's DRM core
//! and i915 answer them on Gen12 with full PPGTT and softpin.
//!
//! It holds every object a program names through the node: GEM objects and
//! their handles, VMs, contexts, syncobjs, and the requests made of the GPU,
//! with their fences. It reaches the program through a [`Host`] (the chardev
//! core's copies, replies, dmabufs and sync files) and the GPU through a
//! [`Backend`] (memory, PPGTTs, bindings, hardware contexts and
//! submissions), so it runs whole in a host test against fakes of both, and
//! in `test-intel-uapi` against a back end that completes every batch at once
//! (never cited for any K2 claim: the consultant's C2, ledger 2352).
//!
//! What the back end may rely on (agreed with M2, 2026-10-18):
//!
//! * a request is submitted only once every fence it waits on has
//!   signalled, so the GPU never waits on a semaphore;
//! * the requests of one hardware context are submitted in order;
//! * a binding is made before the first request that uses it is submitted,
//!   lies wholly inside 48 bits, is 4 KiB-aligned and overlaps no other
//!   binding of its VM, and is unbound only after every request that used it
//!   completed;
//! * a VM and a hardware context are destroyed only when nothing uses them.
//!
//! No memory is allocated: every table has a fixed size ([`limits`]), and a
//! full table is the error i915 would give for running out of memory.
//!
//! Where it departs from i915 on purpose, each in its function's comment:
//! relocations and unpinned objects are refused (Gen12 past TGL refuses
//! relocations itself, and Mesa pins every object); a syncobj cannot be
//! exported as an opaque file (only as a sync file); a binding that would
//! overlap one still in use answers `EAGAIN` rather than sleeping; userptr,
//! the legacy mmap, pread, pwrite and perf are `ENODEV`.

/// Requests that wrote an object a dmabuf of shares with other programs,
/// counted from the driver's start: what `card0` asks before a copy
/// whether a buffer it reads may hold a GPU's writes that went past the
/// LLC (Mesa writes an exported or imported object with uncached MOCS). A
/// screen drawn in software beside no GPU client never moves it.
static SHARED_WRITES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Count a request that writes a shared object.
pub(crate) fn note_shared_write() {
    let _ = SHARED_WRITES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// How many requests have written a shared object since the driver
/// started.
#[must_use]
pub fn shared_writes() -> u64 {
    SHARED_WRITES.load(core::sync::atomic::Ordering::Relaxed)
}

pub mod backend;
pub mod drm;
pub mod exec;
pub mod fence;
pub mod gem;
pub mod getparam;
pub mod host;
pub mod limits;
pub mod objects;
pub mod query;
pub mod syncobj;
pub mod vm;

#[cfg(test)]
mod tests;

pub use backend::{
    Backend, Completion, ContextParams, Engine, Facts, HwContext, SubmitError, VmId,
};
pub use host::{Host, Installed};

use ferrix_linux_abi::errno::Errno;

/// `ETIME`, which `ferrix_linux_abi` does not name.
pub const ETIME: Errno = Errno(62);

/// A request's id: one per execbuffer, from 1, never reused.
pub type RequestId = u64;

/// What answering an ioctl came to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    /// Answered now with this status (0 or a negative errno).
    Now(i32),
    /// Held: answered later, by [`Frontend::service`], when what it waits
    /// for signals or its deadline passes. The chardev core keeps the
    /// program waiting meanwhile (C1, ledger 2352: replies in any order).
    Later,
}

impl Answer {
    /// An error's answer.
    #[must_use]
    pub const fn error(errno: Errno) -> Self {
        Answer::Now(-(errno.0 as i32))
    }
}

/// The front end: every table, and the back end.
pub struct Frontend<B: Backend> {
    /// The GPU.
    pub backend: B,
    /// The tables.
    pub state: objects::Tables<B::Memory>,
}

impl<B: Backend> core::fmt::Debug for Frontend<B> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Frontend")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl<B: Backend> Frontend<B> {
    /// A front end over `backend`, with every table empty. The tables are
    /// several megabytes: build this in place (a `static`, or a thread with
    /// a large stack in a test), never on a small stack.
    pub fn new(backend: B) -> Self {
        Self {
            backend,
            state: objects::Tables::new(),
        }
    }

    /// Say each held wait, its answer, each failed ioctl, and a wait held
    /// unanswered for [`fence::STUCK_NS`] with its fences
    /// (`ferrix.intelgpu.uapitrace`): what a box run reads to find a call
    /// that never completes.
    pub fn set_trace(&mut self, on: bool) {
        self.state.trace = on;
    }

    /// The earliest time [`Frontend::service`] must be called by, in
    /// monotonic nanoseconds: a held wait's deadline or the back end's.
    #[must_use]
    pub fn next_deadline(&self) -> Option<u64> {
        let waits = self.state.waits.next_deadline(self.state.trace);
        match (waits, self.backend.next_deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// An mmap of file `file` at `offset`, `pages` long: answered with the
    /// object's VMO, or an error.
    pub fn mmap<H: Host<B::Memory>>(
        &mut self,
        host: &mut H,
        request: u64,
        file: u64,
        offset: u64,
        pages: u32,
    ) {
        gem::mmap(self, host, request, file, offset, pages);
    }

    /// The last reference to one of this driver's dmabufs went.
    pub fn dmabuf_release<H: Host<B::Memory>>(&mut self, host: &mut H, cookie: u64) {
        gem::dmabuf_released(&mut self.state, cookie);
        fence::settle(self, host);
    }

    /// A program's `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` on one of this driver's
    /// dmabufs, answered as an ioctl: `kind` is the chardev protocol's
    /// `IMPORT_*`, `value` the fence's cookie or status.
    pub fn dmabuf_import<H: Host<B::Memory>>(
        &mut self,
        host: &mut H,
        request: u64,
        cookie: u64,
        flags: u32,
        kind: u32,
        value: u64,
    ) {
        let status = match gem::dmabuf_import(&mut self.state, cookie, flags, kind, value) {
            Ok(()) => 0,
            Err(errno) => -i32::from(errno.0),
        };
        host.reply(request, status);
    }

    /// After an interrupt, a deadline, or any request: let the back end run,
    /// take its completions, submit what became ready, signal the kernel's
    /// fences, retire what is idle, and answer every held wait that is
    /// settled.
    pub fn service<H: Host<B::Memory>>(&mut self, host: &mut H, interrupt: bool) {
        let now = host.now_ns();
        self.backend.service(now, interrupt);
        fence::settle(self, host);
    }
}
