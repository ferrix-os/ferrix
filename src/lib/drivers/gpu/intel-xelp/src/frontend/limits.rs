//! The tables' sizes. Every one is a fixed bound, and running past it is
//! the error i915 gives for running out of memory (`ENOMEM`), or, for a held
//! wait, `ENOMEM` before anything waits.

/// Open files of the render node at once.
pub const FILES: usize = 32;
/// GEM objects alive at once, every file's together.
pub const OBJECTS: usize = 8192;
/// GEM handles at once, every file's together.
pub const HANDLES: usize = 16384;
/// Bindings of objects into VMs at once.
pub const BINDINGS: usize = 16384;
/// VMs at once.
pub const VMS: usize = 256;
/// VM ids at once: a VM may have several, each `GETPARAM(VM)` makes one.
pub const VM_IDS: usize = 512;
/// Contexts at once, each file's default context included.
pub const CONTEXTS: usize = 512;
/// The engines one context's map may hold (`I915_EXEC_RING_MASK + 1` is 64
/// in i915; anv asks for at most one per queue it creates).
pub const CONTEXT_ENGINES: usize = 8;
/// Syncobjs at once.
pub const SYNCOBJS: usize = 4096;
/// The timeline points one syncobj keeps apart; older ones are merged.
pub const POINTS: usize = 4;
/// Requests in flight or queued at once.
pub const REQUESTS: usize = 1024;
/// The fences one request waits on, kept apart; more are merged.
pub const REQUEST_DEPS: usize = 16;
/// Held waits at once: under the chardev core's 256 outstanding requests
/// per control (C1, ledger 2352).
pub const WAITS: usize = 64;
/// The syncobjs one held wait names.
pub const WAIT_HANDLES: usize = 64;
/// Kernel fences (sync files and dmabuf slots) not yet signalled.
pub const KERNEL_FENCES: usize = 2048;
/// Exec objects one execbuffer may list.
pub const EXEC_OBJECTS: usize = 4096;
/// Fences one execbuffer may name.
pub const EXEC_FENCES: usize = 256;
/// The read fences one object keeps apart; older ones are merged.
pub const OBJECT_READS: usize = 4;
