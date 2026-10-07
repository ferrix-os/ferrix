//! `/dev/dri/renderD<N>` as a program opens it: the virtgpu ioctls
//! `docs/GPU.md` step 2 answers.
//!
//! The node is the *device-independent* layer of §3.3's seam, as
//! `drm_gem.c` is on Linux: the inode, the generic ioctls, the handle table
//! and an object's lifetime. Nothing here knows virgl or virtio. What a resource's format words mean is the renderer's
//! own language, and it passes through as bytes; what userspace learns from
//! this node is the driver's *name*, which is how it picks a back end, on
//! Ferrix exactly as on Linux.
//!
//! # Many opens, unlike a card
//!
//! [`crate::interfaces::display::drm::CardFile`] allows one open at a time, a written
//! deviation from Linux. A render node does not: Linux's render nodes exist
//! precisely so that every GL client opens one of its own without being the
//! display's master, and §3.3 puts the handle table at the open for that
//! reason. So this refuses nobody, and what an open owns it owns alone.
//!
//! # What is answered
//!
//! `DRM_IOCTL_VERSION`, which names the driver, `VIRTGPU_GETPARAM`, which
//! says what the device can do, and `VIRTGPU_GET_CAPS`, the capability set
//! itself. All three are answered from what the core learned before it
//! published the renderer, so none costs a message.
//!
//! `RESOURCE_CREATE` makes an object on the device, with a backing of its
//! own, and puts it in this open's handle table; `RESOURCE_INFO` reads that
//! table back and `MAP` names the backing for `mmap`. `TRANSFER_TO_HOST` and
//! `TRANSFER_FROM_HOST` move bytes between the backing and the device's copy,
//! and `EXECBUFFER` runs a command stream, whose bytes are the renderer's
//! own language and are never read here. `TRANSFER_TO_HOST` and `EXECBUFFER`
//! return once the work is on its way, as on Linux, and `WAIT` waits for it:
//! a program writing a backing the device may still be reading calls `WAIT`
//! first (`super`'s note).
//!
//! An open has one context, made the first time it is needed, as Linux makes
//! one for a device that has no `CONTEXT_INIT`: what one program draws and
//! the resources it may name are apart from every other's.
//!
//! `DRM_IOCTL_GEM_CLOSE` lets a handle go, and an open that closes lets go
//! of the rest.
//!
//! # Venus: contexts of a capability set, blobs and fences
//!
//! What Mesa's Venus driver asks of a render node (`docs/GPU.md` §6.1).
//! `CONTEXT_INIT` makes this open's context for the capability set it names,
//! with up to [`MAX_RINGS`] rings. `RESOURCE_CREATE_BLOB` makes a blob -- host
//! memory the device's renderer allocates, named by the context's own
//! `blob_id` -- after running the commands that name it, and a mappable one is
//! placed in the device's host-visible window as it is made; `MAP` and `mmap`
//! then reach those pages, cached as the device said. `EXECBUFFER` on a ring
//! with `EXECBUF_FENCE_FD_OUT` answers a descriptor ([`super::fence`]) that
//! polls readable when the work has finished. `DRM_IOCTL_GET_CAP` says there
//! are no sync objects, which is what sends Venus to its own on top of those
//! descriptors.
//!
//! What is not answered, and what is in the way of each:
//!
//! * **Fences into a submission**, `EXECBUF_FENCE_FD_IN`, and sync objects:
//!   Venus waits on its fences itself, by polling them.
//! * **Blobs of guest memory**, `BLOB_MEM_GUEST` and `BLOB_MEM_HOST3D_GUEST`,
//!   which need the guest's pages handed to the device as a resource's
//!   backing is; Venus keeps everything in host memory when the window is
//!   there.
//! * **An unfenced submission's work finishing.** `WAIT` still waits for
//!   what this open sent before it to be *answered*, which for a stream on no
//!   ring is the device having taken it, not the GPU having finished it.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;

use ferrix_linux_abi::drm::{self, GemClose, GetCap, PrimeHandle, Version};
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::socket::Width;
use ferrix_linux_abi::types;
use ferrix_linux_abi::virtgpu::{
    self, ContextInit, ContextSetParam, ExecBuffer, Field, GetCaps, GetParam, Layout, Map,
    ResourceCreate, ResourceCreateBlob, ResourceInfo, TransferToHost, Wait,
};
use ferrix_renderctl::message::{Direction, MAX_RINGS, NO_RING, Region, Status, Transfer, flags};
use ferrix_renderctl::session::RequestError;
use ferrix_vfs::{Inode, Metadata, Readiness, Result as VfsResult};

use super::{COMMAND_BYTES, Placed, RenderError, Renderer};
use crate::sync::SpinLock;
use crate::syscall::process::{self, Process};
use crate::syscall::uaccess;
use crate::user::vmo::Vmo;

/// How far up a `VIRTGPU_MAP` offset the handle is: the low half is a place
/// in the object, which no object is too big for, and the high half is
/// which object. An offset is a name here, not a place in a file, as it is
/// on Linux, where the numbers come out of a fake-offset allocator instead.
const MAP_SHIFT: u32 = 32;

/// The width this kernel's programs use, which is the width its structures
/// are read and written at.
const NATIVE: Width = if size_of::<usize>() == 8 {
    Width::Bits64
} else {
    Width::Bits32
};

/// Where an ioctl number keeps its argument's size: bits 16 to 29.
const IOC_SIZE_SHIFT: u32 = 16;
const IOC_SIZE_MASK: u32 = 0x3FFF << IOC_SIZE_SHIFT;

/// Whether `request` is `known` with an argument of another size.
///
/// DRM matches a driver's ioctl by its number alone and takes an argument of
/// any size, copying what both sides know and zeroing the rest
/// (`drm_ioctl`): a program built against a newer header, whose structure
/// grew, still reaches the call it meant. Mesa 26 carries a
/// `drm_virtgpu_resource_create_blob` with a `blob_hints` word the Linux
/// headers this crate's table was probed from have not got yet, so its
/// number is not theirs.
const fn same_call(request: u32, known: u32) -> bool {
    request & !IOC_SIZE_MASK == known & !IOC_SIZE_MASK
}

/// How many bytes of argument `request` says it has.
const fn argument_size(request: u32) -> usize {
    ((request & IOC_SIZE_MASK) >> IOC_SIZE_SHIFT) as usize
}

/// `DRM_VIRTGPU_BLOB_FLAG_HINT_DEFER_MAPPING`, the one hint a grown
/// `drm_virtgpu_resource_create_blob` carries: that the program may never
/// map the blob. Taken and not acted on -- a mappable blob is placed in the
/// window as it is made either way, which costs a place and nothing else.
const BLOB_HINT_DEFER_MAPPING: u32 = 1;

/// What `DRM_IOCTL_VERSION` reports beside the driver's own name.
const VERSION_MAJOR: i32 = 0;
const VERSION_MINOR: i32 = 1;
const VERSION_PATCH: i32 = 0;

/// An object of the renderer, and everything about it that outlives the
/// handle a program names it by.
///
/// Held by whatever names it -- an open's handle table, a descriptor
/// [`export`] made -- and let go of when the last of them goes. That is what
/// lets a program hand its drawn frame to the card and then close the
/// handle, without the resource going while the screen is showing it.
pub(crate) struct Object {
    renderer: Arc<Renderer>,
    /// What the core and the device call it. It is also the `res_handle`
    /// answered to a program: the driver names the device's resource by the
    /// core's object id, so the two are one number and not two.
    id: u32,
    /// The shape it was made with, which is what a card needs to show it.
    width: u32,
    height: u32,
    stride: u32,
    /// How many bytes of backing it was made with.
    bytes: u32,
    /// That backing, which `mmap` maps and the driver pinned for the device.
    backing: Option<Arc<Vmo>>,
    /// For a blob, which kind of memory it is, as `RESOURCE_INFO` reports.
    blob_mem: u32,
    /// For a mappable blob, where its pages are in the device's window.
    placed: Option<Placed>,
}

/// A mapped blob, as `mmap` keeps it: the object, held for as long as any
/// region maps its pages, so the device cannot be told to let it go -- and
/// its place in the window cannot be given to another blob -- while a
/// program can still reach them.
#[derive(Debug)]
pub(crate) struct Window {
    object: Arc<Object>,
}

impl Window {
    /// Where the blob's pages are, how many, and whether the device said
    /// they may be cached: `VIRTIO_GPU_MAP_CACHE_CACHED`, or no word at all,
    /// which Linux's `virtio_gpu_vram_mmap` maps cached too. Write-combining
    /// is mapped uncached, which is slower and never wrong.
    pub(crate) fn place(&self) -> Option<(u64, u64, bool)> {
        let placed = self.object.placed?;
        let cache = placed.map_info & 0x0f;
        Some((placed.phys, placed.len, cache == 0x00 || cache == 0x01))
    }
}

impl core::fmt::Debug for Object {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Object")
            .field("id", &self.id)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl Object {
    /// What the device calls it, and its shape: what a card is told to show
    /// it by.
    pub(crate) const fn shown(&self) -> (u32, u32, u32, u32) {
        (self.id, self.width, self.height, self.stride)
    }
}

impl Drop for Object {
    /// Give it back to the device, without waiting: the reply goes to the
    /// renderer's task, which drops it. The same bargain a close makes.
    fn drop(&mut self) {
        self.renderer.release(&[self.id], None);
    }
}

/// One object this open has a handle for.
#[derive(Clone, Debug)]
struct Handle {
    /// What a program calls it: `bo_handle`, small and this open's alone.
    handle: u32,
    /// The object itself, which outlives this handle if anything else names
    /// it.
    object: Arc<Object>,
}

/// One open of a render node.
///
/// It holds the renderer rather than the device: an open outlives nothing,
/// and a renderer whose driver has gone answers `ENODEV`.
///
/// The handle table is the open's, which is why a render node takes any
/// number of opens: two programs' `bo_handle` 1 are different objects, and
/// neither can name the other's (`docs/GPU.md` §3.3).
pub(crate) struct RenderFile {
    renderer: Arc<Renderer>,
    handles: SpinLock<Handles>,
    /// This open's context on the device, once something has needed one,
    /// and how many rings `CONTEXT_INIT` gave it.
    context: SpinLock<Option<(u32, u32)>>,
}

/// An open's handle table.
struct Handles {
    live: Vec<Handle>,
    /// The next `bo_handle` to hand out. Handles count from 1: zero is "no
    /// object" in every call that takes one.
    next: u32,
}

impl Handles {
    /// The handle the table has for `object`, if it has one: the one place
    /// an import asks, so one handle an object holds for both of its
    /// looks, before the attach and after it.
    fn handle_of(&self, object: &Arc<Object>) -> Option<u32> {
        self.live
            .iter()
            .find(|held| Arc::ptr_eq(&held.object, object))
            .map(|held| held.handle)
    }
}

impl core::fmt::Debug for RenderFile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RenderFile")
            .field("renderer", &self.renderer.index)
            .finish_non_exhaustive()
    }
}

impl RenderFile {
    /// Open `renderer`. Any number of opens may hold one.
    pub(crate) fn open(renderer: Arc<Renderer>) -> Result<Arc<RenderFile>, Errno> {
        if renderer.is_gone() {
            return Err(Errno::ENXIO);
        }
        Ok(Arc::new(RenderFile {
            renderer,
            handles: SpinLock::new(Handles {
                live: Vec::new(),
                next: 1,
            }),
            context: SpinLock::new(None),
        }))
    }

    /// This open's context, made now for the default capability set if it
    /// has none.
    ///
    /// Two threads of one program may both find none and both make one; the
    /// second is given straight back. Making one sleeps, so the lock cannot
    /// be held across it.
    fn context(&self) -> Result<u32, Errno> {
        if let Some((context, _)) = *self.context.lock() {
            return Ok(context);
        }
        let made = self
            .renderer
            .make_context(self.renderer.default_capset())
            .map_err(errno_of)?;
        let mut held = self.context.lock();
        match *held {
            Some((first, _)) => {
                drop(held);
                self.renderer.release(&[], Some(made));
                Ok(first)
            }
            None => {
                *held = Some((made, 0));
                Ok(made)
            }
        }
    }

    /// Put `object` in this open's handle table, and answer its handle.
    fn hold(&self, object: Arc<Object>) -> u32 {
        let mut handles = self.handles.lock();
        let handle = handles.next;
        handles.next = handles.next.saturating_add(1);
        handles.live.push(Handle { handle, object });
        handle
    }

    /// The handle this open has for `object`, if it has one.
    fn handle_of(&self, object: &Arc<Object>) -> Option<u32> {
        self.handles.lock().handle_of(object)
    }

    /// Put `object` in this open's handle table unless it is there already,
    /// and answer its handle: one handle an object, whichever of two
    /// importing threads gets here first.
    fn hold_once(&self, object: Arc<Object>) -> u32 {
        let mut handles = self.handles.lock();
        if let Some(handle) = handles.handle_of(&object) {
            return handle;
        }
        let handle = handles.next;
        handles.next = handles.next.saturating_add(1);
        handles.live.push(Handle { handle, object });
        handle
    }

    /// What is behind `handle`, if this open has it.
    fn held(&self, handle: u32) -> Result<Handle, Errno> {
        self.handles
            .lock()
            .live
            .iter()
            .find(|held| held.handle == handle)
            .cloned()
            .ok_or(Errno::ENOENT)
    }
}

impl Drop for RenderFile {
    /// Let go of every object this open made, without waiting: the replies go
    /// to the renderer's task, which drops them. The same bargain a card's
    /// open makes, and for the same reason -- a close does not wait on a
    /// device.
    fn drop(&mut self) {
        let objects: Vec<u32> = self
            .handles
            .get_mut()
            .live
            .iter()
            .map(|held| held.object.id)
            .collect();
        // The objects go with the handles, which is what dropping them does;
        // what is left is the context, which the core takes away once they
        // have. Nothing is said about the objects here, so a handle another
        // descriptor still names keeps its object.
        let _ = objects;
        self.renderer
            .release(&[], self.context.get_mut().map(|(context, _)| context));
    }
}

impl Inode for RenderFile {
    fn metadata(&self) -> Metadata {
        self.renderer.metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    /// Linux's DRM files have no `splice_read`: `sendfile` and `splice`
    /// from one are `EINVAL`, into a pipe as into anything.
    fn splices_out(&self) -> bool {
        false
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// `pread64` is a `read`, and `lseek` `ESPIPE`, as on a card: see
    /// `crate::interfaces::display::drm`'s documentation on offsets, which measured
    /// both kinds of node.
    fn ignores_position(&self) -> bool {
        true
    }

    /// See `ignores_position`, above.
    fn seek_is_noop(&self) -> bool {
        false
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.read_stream(buf, false)
    }

    /// A read waits for an event, as a card's does, and a render node here
    /// never has one: `POLL_RINGS_MASK`, which would ask for fence events, is
    /// taken only as none (`context_init`). So it is `EAGAIN` under
    /// `O_NONBLOCK`, and otherwise a wait that only a signal ends, with a
    /// restart code. Linux's `drm_read` is the same code on both kinds of
    /// node: measured on a 7.0 host, `renderD128` (amdgpu) and `renderD129`
    /// (nvidia), with no event queued, answer `read` of 4096, of 8 and of 0,
    /// and `pread` of 8, with `EAGAIN` under `O_NONBLOCK`, and a blocking
    /// `read` of 4096 or of 0 waits until a signal and is `EINTR` from a
    /// handler without `SA_RESTART`. This used to answer `EINVAL`, taking a
    /// render node for one with no read at all.
    fn read_stream(&self, _buf: &mut [u8], nonblock: bool) -> VfsResult<usize> {
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        let Some(caller) = process::current() else {
            return Err(Errno::EAGAIN);
        };
        let _ = caller
            .signalled()
            .wait_until_deadline(|| caller.signal_pending(), u64::MAX);
        Err(Errno::ERESTARTSYS)
    }

    /// Never ready, since no event ever comes: `poll` of a render node for
    /// `POLLIN | POLLOUT` answers nothing on the same host, where
    /// [`Readiness::ALWAYS`], the default, would send an event loop round
    /// reads that never have anything.
    fn poll(&self) -> Readiness {
        Readiness::default()
    }

    /// Nothing ever changes, so `epoll` may watch it, as Linux's `drm_poll`
    /// lets it, and never hears from it.
    fn poll_changes(&self) -> Option<u64> {
        Some(0)
    }

    /// No queue to wake, and none needed: nothing [`Inode::poll`] reports
    /// ever changes, so a wait over it is ended by its other descriptors,
    /// its timeout or a signal.
    fn poll_queues(&self, _visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        true
    }

    /// The backing of the object a `VIRTGPU_MAP` offset names: its VMO, or
    /// for a blob, its place in the device's window.
    fn mapping_at(&self, offset: u64) -> Option<(Arc<dyn Any + Send + Sync>, u64)> {
        let handle = u32::try_from(offset >> MAP_SHIFT).ok()?;
        let object = self.held(handle).ok()?.object;
        let within = offset & ((1 << MAP_SHIFT) - 1);
        if let Some(backing) = object.backing.clone() {
            let backing: Arc<dyn Any + Send + Sync> = backing;
            return Some((backing, within));
        }
        let _ = object.placed?;
        let window: Arc<dyn Any + Send + Sync> = Arc::new(Window { object });
        Some((window, within))
    }
}

/// The open render node `file` reads and writes go to, if it is one.
pub(crate) fn of(io: &Arc<dyn Inode>) -> Option<Arc<RenderFile>> {
    Arc::clone(io).into_any().downcast::<RenderFile>().ok()
}

/// The number `renderD<N>` names, with no leading zero.
///
/// Linux numbers render nodes from 128, and so does the core; a name below
/// that is not one, which keeps `renderD0` from meaning anything.
pub(crate) fn render_number(name: &[u8]) -> Option<u32> {
    let digits = name.strip_prefix(b"renderD")?;
    if digits.is_empty() || digits.first() == Some(&b'0') {
        return None;
    }
    let number: u32 = core::str::from_utf8(digits).ok()?.parse().ok()?;
    (number >= 128).then_some(number)
}

/// Answer `request` on the open render node `file`, or `ENOTTY` for one this
/// subset does not have.
pub(crate) fn ioctl(
    process: &Process,
    file: &RenderFile,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    if file.renderer.is_gone() {
        return Err(Errno::ENODEV);
    }
    match request {
        request if request == drm::ioctl_version(NATIVE) => version(process, file, arg),
        virtgpu::IOCTL_GETPARAM => get_param(process, file, arg),
        virtgpu::IOCTL_RESOURCE_CREATE => resource_create(process, file, arg),
        virtgpu::IOCTL_RESOURCE_INFO => resource_info(process, file, arg),
        virtgpu::IOCTL_MAP => map(process, file, arg),
        virtgpu::IOCTL_TRANSFER_TO_HOST => transfer(process, file, arg, Direction::ToDevice),
        virtgpu::IOCTL_TRANSFER_FROM_HOST => transfer(process, file, arg, Direction::FromDevice),
        virtgpu::IOCTL_EXECBUFFER => exec_buffer(process, file, arg),
        virtgpu::IOCTL_WAIT => wait(process, file, arg),
        virtgpu::IOCTL_GET_CAPS => get_caps(process, file, arg),
        virtgpu::IOCTL_CONTEXT_INIT => context_init(process, file, arg),
        request if same_call(request, virtgpu::IOCTL_RESOURCE_CREATE_BLOB) => {
            resource_create_blob(process, file, arg, argument_size(request))
        }
        drm::IOCTL_GET_CAP => get_cap(process, arg),
        drm::IOCTL_GEM_CLOSE => gem_close(process, file, arg),
        drm::IOCTL_PRIME_HANDLE_TO_FD => export(process, file, arg),
        drm::IOCTL_PRIME_FD_TO_HANDLE => import(process, file, arg),
        _ => Err(Errno::ENOTTY),
    }
}

/// What a failed request answers a program.
fn errno_of(error: RenderError) -> Errno {
    match error {
        // No room for another object, which is the one request failure that
        // is about this device being full rather than about the call.
        RenderError::Request(RequestError::Full) => Errno::ENOSPC,
        RenderError::Request(_) => Errno::EINVAL,
        // The device was asked and said no. `ENOMEM` when it said so, and
        // `EINVAL` for a resource it would not make, which is what Linux's
        // virtio-gpu answers for each.
        RenderError::Refused(Status::OutOfMemory) => Errno::ENOMEM,
        RenderError::Refused(_) => Errno::EINVAL,
        RenderError::Busy => Errno::EBUSY,
        RenderError::Gone => Errno::ENODEV,
        RenderError::TimedOut => Errno::ETIMEDOUT,
    }
}

/// `VIRTGPU_RESOURCE_CREATE`: make a resource, and a handle for it.
///
/// The caller's words -- target, format, bind and the resource's shape --
/// are virgl's and are passed down as bytes; what they say is the driver's
/// business and this side never reads them (`docs/GPU.md` §3.3). What this
/// side decides is how many bytes of backing the object has, which is
/// `size`, as it is on Linux: a caller that will never move bytes to or from
/// a resource asks for a page and the device's copy is as big as its shape
/// says regardless.
///
/// `bo_handle` must be zero: attaching a resource to an object that already
/// exists is what a second resource on one buffer needs, and nothing here
/// makes one.
fn resource_create(process: &Process, file: &RenderFile, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; ResourceCreate::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let mut create = ResourceCreate::read(&bytes).ok_or(Errno::EFAULT)?;
    if create.bo_handle != 0 {
        return Err(Errno::EINVAL);
    }
    if create.size == 0 {
        return Err(Errno::EINVAL);
    }
    let context = file.context()?;
    let (object, backing) = file
        .renderer
        .make_object(
            context,
            u64::from(create.size),
            flags::MAPPABLE | flags::TO_DEVICE | flags::FROM_DEVICE,
            [
                create.target,
                create.format,
                create.bind,
                create.width,
                create.height,
                create.depth,
                create.array_size,
                create.last_level,
                create.nr_samples,
                create.flags,
            ],
        )
        .map_err(errno_of)?;
    let held = Arc::new(Object {
        renderer: Arc::clone(&file.renderer),
        id: object,
        width: create.width,
        height: create.height,
        stride: create.stride,
        bytes: create.size,
        backing,
        blob_mem: 0,
        placed: None,
    });
    let handle = file.hold(held);
    create.bo_handle = handle;
    create.res_handle = object;
    create.write(&mut bytes).ok_or(Errno::EFAULT)?;
    // The object is made and the handle is this open's; a program that
    // cannot be told its number still has both, and `EFAULT` here would
    // leave it no way to name them. The write above is the only failure
    // this can have, and it is the caller's own pointer that caused it.
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `VIRTGPU_MAP`: the offset to `mmap` this node at to reach an object's
/// backing.
fn map(process: &Process, file: &RenderFile, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; Map::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let mut map = Map::read(&bytes).ok_or(Errno::EFAULT)?;
    let object = file.held(map.handle)?.object;
    if object.backing.is_none() && object.placed.is_none() {
        return Err(Errno::EINVAL);
    }
    map.offset = u64::from(map.handle) << MAP_SHIFT;
    map.write(&mut bytes).ok_or(Errno::EFAULT)?;
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `VIRTGPU_TRANSFER_TO_HOST` and `VIRTGPU_TRANSFER_FROM_HOST`: move bytes
/// between an object's backing and the device's copy. To the device returns
/// once they are on their way, from the device once they have arrived.
///
/// The two structures are one layout, which a test in `src/lib/proto/linux-abi`
/// holds them to, so one reader serves both.
fn transfer(
    process: &Process,
    file: &RenderFile,
    arg: u64,
    direction: Direction,
) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; TransferToHost::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let asked = TransferToHost::read(&bytes).ok_or(Errno::EFAULT)?;
    let held = file.held(asked.bo_handle)?;
    let context = file.context()?;
    file.renderer
        .transfer(Transfer {
            object: held.object.id,
            context,
            direction,
            level: asked.level,
            offset: u64::from(asked.offset),
            region: Region {
                x: asked.r#box.x,
                y: asked.r#box.y,
                z: asked.r#box.z,
                width: asked.r#box.w,
                height: asked.r#box.h,
                depth: asked.r#box.d,
            },
            stride: asked.stride,
            layer_stride: asked.layer_stride,
        })
        .map_err(errno_of)?;
    Ok(0)
}

/// `VIRTGPU_EXECBUFFER`: run a command stream in this open's context, and
/// return once it is on its way.
///
/// The stream is copied once, into the core, and from there into the work
/// VMO, where the device reads it: a program's memory is not something a
/// driver in another process can be pointed at.
///
/// With `EXECBUF_RING_IDX` it runs on that ring of the context, which
/// `CONTEXT_INIT` gave it; with `EXECBUF_FENCE_FD_OUT` it is fenced there --
/// on ring 0 if none is named -- and the answer carries a descriptor that
/// polls readable once the work has finished. No fence comes *in*, and no
/// sync object either way: those are refused, as `GET_CAP` says there are
/// none. `bo_handles` is a hint on Linux -- which objects the stream
/// touches, for fencing them -- and a fence here is on the stream, so it is
/// not read.
fn exec_buffer(process: &Process, file: &RenderFile, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; ExecBuffer::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let mut exec = ExecBuffer::read(&bytes).ok_or(Errno::EFAULT)?;
    let known = virtgpu::EXECBUF_RING_IDX | virtgpu::EXECBUF_FENCE_FD_OUT;
    if exec.flags & !known != 0 || exec.num_in_syncobjs != 0 || exec.num_out_syncobjs != 0 {
        return Err(Errno::EINVAL);
    }
    // A stream is words, and one longer than a slot is not split: only its
    // writer knows where a command ends.
    if exec.size == 0 || !exec.size.is_multiple_of(4) || u64::from(exec.size) > COMMAND_BYTES {
        return Err(Errno::EINVAL);
    }
    let mut commands = vec![0u8; exec.size as usize];
    uaccess::copy_from_user(process.space(), exec.command, &mut commands)
        .map_err(|_| Errno::EFAULT)?;
    let context = file.context()?;
    let rings = file.context.lock().map_or(0, |(_, rings)| rings);
    let named = exec.flags & virtgpu::EXECBUF_RING_IDX != 0;
    let fenced = exec.flags & virtgpu::EXECBUF_FENCE_FD_OUT != 0;
    if named && exec.ring_idx >= rings {
        return Err(Errno::EINVAL);
    }
    let ring = match (fenced, named) {
        (true, true) => exec.ring_idx,
        (true, false) => 0,
        (false, _) => NO_RING,
    };
    if fenced && !file.renderer.has_rings() {
        return Err(Errno::EINVAL);
    }
    let fence = file
        .renderer
        .submit(context, ring, &commands)
        .map_err(errno_of)?;
    if fenced {
        let open = super::fence::open(Arc::clone(&file.renderer), fence)?;
        let descriptor = process.files().lock().insert(open, true)?;
        exec.fence_fd = descriptor;
        exec.write(&mut bytes).ok_or(Errno::EFAULT)?;
        uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `VIRTGPU_CONTEXT_INIT`: make this open's context for the capability set
/// the program names, with the rings it asks for.
///
/// Once an open, as Linux has it: an open whose context is made -- by this,
/// or by any call that needed one first -- is `EEXIST`. A set the driver did
/// not offer is `EINVAL`, and so is a ring count past [`MAX_RINGS`].
/// `POLL_RINGS_MASK` asks for events on the node's descriptor when a ring's
/// fence passes; nothing is read from this node, so only no rings is taken.
/// A debug name is for the host's log and is not carried.
fn context_init(process: &Process, file: &RenderFile, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; ContextInit::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let init = ContextInit::read(&bytes).ok_or(Errno::EFAULT)?;
    if init.pad != 0 || init.num_params == 0 || init.num_params > 4 {
        return Err(Errno::EINVAL);
    }
    let mut capset = file.renderer.default_capset();
    let mut rings = 0_u32;
    for index in 0..u64::from(init.num_params) {
        let mut param = vec![0u8; ContextSetParam::SIZE];
        let at = init
            .ctx_set_params
            .checked_add(index * ContextSetParam::SIZE as u64)
            .ok_or(Errno::EFAULT)?;
        uaccess::copy_from_user(process.space(), at, &mut param).map_err(|_| Errno::EFAULT)?;
        let param = ContextSetParam::read(&param).ok_or(Errno::EFAULT)?;
        match param.param {
            virtgpu::CONTEXT_PARAM_CAPSET_ID => {
                capset = u32::try_from(param.value).map_err(|_| Errno::EINVAL)?;
                if capset == 0
                    || capset >= u32::BITS
                    || file.renderer.capsets() & (1 << capset) == 0
                {
                    return Err(Errno::EINVAL);
                }
            }
            virtgpu::CONTEXT_PARAM_NUM_RINGS => {
                rings = u32::try_from(param.value).map_err(|_| Errno::EINVAL)?;
                if rings > MAX_RINGS {
                    return Err(Errno::EINVAL);
                }
            }
            virtgpu::CONTEXT_PARAM_POLL_RINGS_MASK if param.value == 0 => {}
            virtgpu::CONTEXT_PARAM_DEBUG_NAME => {}
            _ => return Err(Errno::EINVAL),
        }
    }
    if file.context.lock().is_some() {
        return Err(Errno::EEXIST);
    }
    let made = file.renderer.make_context(capset).map_err(errno_of)?;
    let mut held = file.context.lock();
    if held.is_some() {
        drop(held);
        file.renderer.release(&[], Some(made));
        return Err(Errno::EEXIST);
    }
    *held = Some((made, rings));
    Ok(0)
}

/// `VIRTGPU_RESOURCE_CREATE_BLOB`: make a blob, and a handle for it.
///
/// Host memory only, `BLOB_MEM_HOST3D`, in this open's context: the context's
/// own protocol names the memory by `blob_id`, which is why the commands a
/// program hands over with the call run first, in that context, before the
/// blob is asked for. A mappable one is placed in the device's window as it
/// is made, and one on a device with no window is `EINVAL`; so is sharing
/// across devices, which Linux also answers only where it has another
/// device to share with. The size is rounded up to whole pages, as Linux
/// rounds it.
///
/// `given` is how many bytes of argument the program's header said: the
/// structure this crate knows, or one grown by a hint word and its padding,
/// whose hint is checked and whose padding is zero.
fn resource_create_blob(
    process: &Process,
    file: &RenderFile,
    arg: u64,
    given: usize,
) -> Result<usize, Errno> {
    const GROWN: usize = ResourceCreateBlob::SIZE + 8;
    if given != ResourceCreateBlob::SIZE && given != GROWN {
        return Err(Errno::EINVAL);
    }
    let mut bytes = vec![0u8; given];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    if let Some(grown) = bytes.get(ResourceCreateBlob::SIZE..GROWN) {
        let word = |at: usize| {
            grown
                .get(at..at + 4)
                .and_then(|word| word.try_into().ok())
                .map_or(u32::MAX, u32::from_le_bytes)
        };
        if word(0) & !BLOB_HINT_DEFER_MAPPING != 0 || word(4) != 0 {
            return Err(Errno::EINVAL);
        }
    }
    let mut create = ResourceCreateBlob::read(&bytes).ok_or(Errno::EFAULT)?;
    let known = virtgpu::BLOB_FLAG_USE_MAPPABLE | virtgpu::BLOB_FLAG_USE_SHAREABLE;
    if create.blob_mem != virtgpu::BLOB_MEM_HOST3D
        || create.blob_flags & !known != 0
        || create.pad != 0
        || create.size == 0
    {
        return Err(Errno::EINVAL);
    }
    let page = ferrix_bootinfo::PAGE_SIZE;
    let size = create
        .size
        .checked_add(page - 1)
        .map(|size| size & !(page - 1))
        .ok_or(Errno::EINVAL)?;
    let mappable = create.blob_flags & virtgpu::BLOB_FLAG_USE_MAPPABLE != 0;
    if mappable && !file.renderer.has_window() {
        return Err(Errno::EINVAL);
    }
    let context = file.context()?;
    if create.cmd_size != 0 {
        if !create.cmd_size.is_multiple_of(4) || u64::from(create.cmd_size) > COMMAND_BYTES {
            return Err(Errno::EINVAL);
        }
        let mut commands = vec![0u8; create.cmd_size as usize];
        uaccess::copy_from_user(process.space(), create.cmd, &mut commands)
            .map_err(|_| Errno::EFAULT)?;
        let _ = file
            .renderer
            .submit(context, NO_RING, &commands)
            .map_err(errno_of)?;
    }
    let (object, placed) = file
        .renderer
        .make_blob(
            context,
            create.blob_mem,
            create.blob_flags,
            create.blob_id,
            size,
            mappable,
        )
        .map_err(errno_of)?;
    let held = Arc::new(Object {
        renderer: Arc::clone(&file.renderer),
        id: object,
        width: 0,
        height: 0,
        stride: 0,
        bytes: u32::try_from(size).unwrap_or(u32::MAX),
        backing: None,
        blob_mem: create.blob_mem,
        placed,
    });
    create.bo_handle = file.hold(held);
    create.res_handle = object;
    create.write(&mut bytes).ok_or(Errno::EFAULT)?;
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `DRM_IOCTL_GET_CAP` on a render node: that there are no sync objects,
/// which is how Venus learns to make its own on fence descriptors. Every
/// other capability is the card's to answer, and is `EINVAL` here.
fn get_cap(process: &Process, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; GetCap::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let mut cap = GetCap::read(&bytes).ok_or(Errno::EFAULT)?;
    cap.value = match cap.capability {
        drm::CAP_SYNCOBJ | drm::CAP_SYNCOBJ_TIMELINE => 0,
        _ => return Err(Errno::EINVAL),
    };
    cap.write(&mut bytes).ok_or(Errno::EFAULT)?;
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `VIRTGPU_WAIT`: wait until an object is idle.
///
/// Until every upload and stream this open sent before it has been
/// answered, which covers every one that touches the object: coarser than
/// Linux's wait on the object's own fences, and never shorter. With
/// `VIRTGPU_WAIT_NOWAIT` it only asks, and `EBUSY` is "not yet", as it is
/// for a wait that runs out of patience.
fn wait(process: &Process, file: &RenderFile, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; Wait::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let wait = Wait::read(&bytes).ok_or(Errno::EFAULT)?;
    let _ = file.held(wait.handle)?;
    if wait.flags & !virtgpu::WAIT_NOWAIT != 0 {
        return Err(Errno::EINVAL);
    }
    // An open that never needed a context has sent nothing.
    let Some((context, _)) = *file.context.lock() else {
        return Ok(0);
    };
    if wait.flags & virtgpu::WAIT_NOWAIT != 0 {
        return if file.renderer.is_settled(context) {
            Ok(0)
        } else {
            Err(Errno::EBUSY)
        };
    }
    match file.renderer.settle(context) {
        Ok(()) => Ok(0),
        Err(RenderError::TimedOut) => Err(Errno::EBUSY),
        Err(error) => Err(errno_of(error)),
    }
}

/// `VIRTGPU_GET_CAPS`: a capability set, as the device gave it.
///
/// As many bytes as the caller has room for, which is how Linux answers it:
/// a renderer built against an older, shorter set reads the front of a
/// newer one. A set the driver did not offer is `EINVAL`.
fn get_caps(process: &Process, file: &RenderFile, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; GetCaps::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let asked = GetCaps::read(&bytes).ok_or(Errno::EFAULT)?;
    let caps = file.renderer.caps(asked.cap_set_id).ok_or(Errno::EINVAL)?;
    if caps.is_empty() {
        return Err(Errno::EINVAL);
    }
    let given = caps
        .get(..caps.len().min(asked.size as usize))
        .unwrap_or(&[]);
    if asked.addr == 0 {
        return Err(Errno::EFAULT);
    }
    uaccess::copy_to_user(process.space(), asked.addr, given).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `VIRTGPU_RESOURCE_INFO`: what is behind a handle.
///
/// Answered from the open's own table rather than by asking the driver: the
/// three things a program asks for here were all settled when the object was
/// made, and a question the driver would have to be woken for is a question
/// answered slowly for no reason.
fn resource_info(process: &Process, file: &RenderFile, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; ResourceInfo::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let mut info = ResourceInfo::read(&bytes).ok_or(Errno::EFAULT)?;
    let held = file.held(info.bo_handle)?;
    info.res_handle = held.object.id;
    info.size = held.object.bytes;
    // Zero for a resource that is not a blob.
    info.blob_mem = held.object.blob_mem;
    info.write(&mut bytes).ok_or(Errno::EFAULT)?;
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// What `/proc/self/fd` calls an exported buffer object.
const EXPORTED_NAME: &[u8] = b"anon_inode:[dmabuf]";

/// A buffer object as a descriptor: what `DRM_IOCTL_PRIME_HANDLE_TO_FD`
/// answers, and what the card's `FD_TO_HANDLE` takes.
///
/// A dmabuf on Linux, and the same job here: a name for the object that
/// another node of the card can be given, which holds the object alive for
/// as long as it is held. What it is *not* is a buffer another process can
/// map or another device can read -- there is one device, and the whole of
/// what this carries between the two nodes is which resource the device
/// already holds.
#[derive(Debug)]
pub(crate) struct Exported {
    object: Arc<Object>,
}

impl Exported {
    /// The object it names.
    pub(crate) fn object(&self) -> Arc<Object> {
        Arc::clone(&self.object)
    }
}

impl Inode for Exported {
    fn metadata(&self) -> Metadata {
        crate::fs::anon::metadata()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// Nothing is read from it: it is a name for an object and not a stream
    /// of bytes, which is what Linux's dmabuf answers `read` for too.
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(Errno::EINVAL)
    }
}

/// The open render node an exported object came from, if `file` is one.
pub(crate) fn exported(io: &Arc<dyn Inode>) -> Option<Arc<Exported>> {
    Arc::clone(io).into_any().downcast::<Exported>().ok()
}

/// `DRM_IOCTL_PRIME_HANDLE_TO_FD`: a buffer object as a descriptor.
///
/// What a compositor does with it is give it to the card, which shows what
/// was drawn without the pixels ever leaving the device (`docs/GPU.md` §3.5
/// piece 6). The descriptor holds the object alive, so the handle may be
/// closed afterwards, as it may on Linux.
///
/// `DRM_RDWR` is taken and ignored: there is nothing to read or write
/// through it. `DRM_CLOEXEC` is the only flag that means anything here.
fn export(process: &Process, file: &RenderFile, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; PrimeHandle::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let mut prime = PrimeHandle::read(&bytes).ok_or(Errno::EFAULT)?;
    if prime.flags & !(types::O_CLOEXEC | types::O_RDWR) != 0 {
        return Err(Errno::EINVAL);
    }
    let held = file.held(prime.handle)?;
    let exported = Arc::new(Exported {
        object: held.object,
    });
    let open = crate::fs::anon::open(exported, EXPORTED_NAME, false)?;
    let descriptor = process
        .files()
        .lock()
        .insert(open, prime.flags & types::O_CLOEXEC != 0)?;
    prime.fd = descriptor;
    prime.write(&mut bytes).ok_or(Errno::EFAULT)?;
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `DRM_IOCTL_PRIME_FD_TO_HANDLE`: an object another open of this node
/// exported, given a handle in this one (`docs/GPU.md` §3.13).
///
/// This is how a compositor shows a client's GPU buffer: the client exports
/// it, hands the descriptor over a socket, and the compositor imports it
/// here, so its own context can sample the resource where it lies.
///
/// * A descriptor that is not an [`Exported`] of *this* renderer is
///   `EINVAL`, as Linux answers one that is not a dmabuf; there is one
///   device, so there is no other's buffer to import.
/// * An object this open already has a handle for answers that handle, as
///   Linux keeps one handle an object an open -- which is also what a
///   program importing its own export is given.
/// * Otherwise the object is attached to this open's context, made now if
///   there is none, and only then put in the handle table. The handle holds
///   the object as a made one does, so `RESOURCE_INFO`, `MAP`, the
///   transfers and `GEM_CLOSE` work on it unchanged.
///
/// Nothing detaches the object on `GEM_CLOSE`: the attachment lasts until
/// the context goes or the device unrefs the resource, which detaches it
/// from every context, and the core gives an object id out again only after
/// the device said it was gone -- so an old context can never name a new
/// object by a reused id. An import after a `GEM_CLOSE`, or two threads
/// importing one object at once, attach again, which the device takes as it
/// takes the first (Linux attaches on every handle it opens); the handle
/// table then keeps one handle, whichever thread put it there first.
fn import(process: &Process, file: &RenderFile, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; PrimeHandle::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let mut prime = PrimeHandle::read(&bytes).ok_or(Errno::EFAULT)?;
    let descriptor = crate::syscall::fd::file(process, crate::syscall::fd::arg(prime.fd as u64))
        .map_err(|_| Errno::EBADF)?;
    let object = exported(descriptor.io()).ok_or(Errno::EINVAL)?.object();
    if !Arc::ptr_eq(&object.renderer, &file.renderer) {
        return Err(Errno::EINVAL);
    }
    prime.handle = match file.handle_of(&object) {
        Some(handle) => handle,
        None => {
            let context = file.context()?;
            file.renderer.attach(object.id, context).map_err(errno_of)?;
            file.hold_once(object)
        }
    };
    prime.write(&mut bytes).ok_or(Errno::EFAULT)?;
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `DRM_IOCTL_GEM_CLOSE`: let a handle go.
///
/// The object goes with it, without waiting for the device: the same bargain
/// a close makes, and Linux's `drm_gem_close_ioctl` does not wait either. A
/// handle this open has not got is `EINVAL`, as Linux answers one.
///
/// Whatever the device does with the resource, the *handle* is gone here, so
/// a program that closes what it no longer draws with can go on making
/// objects for as long as it runs. Without this a compositor's textures
/// accumulated for the length of a session.
fn gem_close(process: &Process, file: &RenderFile, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; GemClose::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let close = GemClose::read(&bytes).ok_or(Errno::EFAULT)?;
    let object = {
        let mut handles = file.handles.lock();
        let at = handles
            .live
            .iter()
            .position(|held| held.handle == close.handle)
            .ok_or(Errno::EINVAL)?;
        handles.live.swap_remove(at)
    };
    // The object goes when the last thing naming it does, which is here
    // unless a descriptor [`export`] made still holds it.
    drop(object);
    Ok(0)
}

/// `DRM_IOCTL_VERSION`: who is driving this node.
///
/// The name is the driver's own, from its HELLO, because that is what
/// userspace picks a back end by -- `virtio_gpu` here, something else for
/// the card §4 describes. The three lengths are answered whether or not
/// there was room for the text, which is how a caller asks how much room to
/// make.
fn version(process: &Process, file: &RenderFile, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; Version::size(NATIVE)];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let mut version = Version::read(NATIVE, &bytes).ok_or(Errno::EFAULT)?;
    let give = |at: u64, capacity: u64, text: &[u8]| -> Result<u64, Errno> {
        let len = text.len() as u64;
        if at != 0 && capacity > 0 {
            let copied = text.get(..capacity.min(len) as usize).unwrap_or(text);
            uaccess::copy_to_user(process.space(), at, copied).map_err(|_| Errno::EFAULT)?;
        }
        Ok(len)
    };
    version.version_major = VERSION_MAJOR;
    version.version_minor = VERSION_MINOR;
    version.version_patchlevel = VERSION_PATCH;
    version.name_len = give(
        version.name,
        version.name_len,
        file.renderer.name().as_bytes(),
    )?;
    version.date_len = give(version.date, version.date_len, b"0")?;
    version.desc_len = give(version.desc, version.desc_len, b"virtio GPU")?;
    version.write(NATIVE, &mut bytes).ok_or(Errno::EFAULT)?;
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `VIRTGPU_GETPARAM`: what this device can do.
///
/// Answered from the HELLO rather than by asking the driver: these are
/// properties of the device the core was told about when it accepted the
/// conversation, and a question the driver would have to be woken for is a
/// question answered slowly for no reason.
///
/// A parameter this version does not know is `EINVAL`, as Linux answers one.
fn get_param(process: &Process, file: &RenderFile, arg: u64) -> Result<usize, Errno> {
    let mut bytes = vec![0u8; GetParam::SIZE];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let param = GetParam::read(&bytes).ok_or(Errno::EFAULT)?;
    let value: u64 = match param.param {
        // There is a renderer at all only because the device has 3D behind
        // it: the driver offers no render conversation for a plain scanout.
        virtgpu::PARAM_3D_FEATURES => 1,
        // The core asks for a capability set by its number, which is the
        // fixed query this parameter stands for.
        virtgpu::PARAM_CAPSET_QUERY_FIX => 1,
        // A bitmask, one bit per set: the ones the driver offered in its
        // HELLO, which are the ones a context may be made for.
        virtgpu::PARAM_SUPPORTED_CAPSET_IDS => u64::from(file.renderer.capsets()),
        // A context may be made for any of them, with rings.
        virtgpu::PARAM_CONTEXT_INIT => 1,
        // Blobs of host memory, mapped through the device's window: both
        // there only where the device has the window.
        virtgpu::PARAM_RESOURCE_BLOB | virtgpu::PARAM_HOST_VISIBLE => {
            u64::from(file.renderer.has_window())
        }
        // Sharing across devices and named contexts are not offered.
        virtgpu::PARAM_CROSS_DEVICE | virtgpu::PARAM_EXPLICIT_DEBUG_NAME => 0,
        _ => return Err(Errno::EINVAL),
    };
    // The answer goes where the caller's pointer says, not into the
    // structure: `value` is a user address.
    if param.value == 0 {
        return Err(Errno::EFAULT);
    }
    uaccess::copy_to_user(process.space(), param.value, &value.to_le_bytes())
        .map_err(|_| Errno::EFAULT)?;
    Ok(0)
}
