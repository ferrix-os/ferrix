//! Syncobjs: `drm_syncobj.c`, with timelines as `dma_fence_chain`s
//! (`dma-fence-chain.c`'s semantics: a point signals once its fence and every
//! earlier point have).
//!
//! A syncobj is a file's own. It can be exported as a sync file of its
//! fence (`EXPORT_SYNC_FILE`) and a sync file imported into it
//! (`IMPORT_SYNC_FILE`), both through the chardev core's fences. It cannot
//! be exported as an opaque descriptor of the syncobj itself: the chardev
//! core has no object for that, and adding one is a kernel interface
//! (refused by name, `EOPNOTSUPP`).

use ferrix_linux_abi::errno::Errno;

use super::drm::{Call, put, unserved_as};
use super::fence::{self, Hold, WaitKind};
use super::objects::{FenceRef, Requests, SyncFence, Syncobj, Tables};
use super::{Backend, ETIME, Frontend, Host, limits};
use crate::uapi::{
    SyncobjArray, SyncobjCreate, SyncobjDestroy, SyncobjHandle, SyncobjTimelineArray,
    SyncobjTimelineWait, SyncobjTransfer, SyncobjWait, drm as consts,
};

/// The point's fence is not there: the syncobj has no fence, or its timeline
/// has not reached the point (`dma_fence_chain_find_seqno`'s `-EINVAL`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotSubmitted;

impl SyncFence {
    /// `drm_syncobj_fence_get` and `dma_fence_chain_find_seqno(point)`: the
    /// fence to wait on for `point` (0: the whole fence).
    ///
    /// # Errors
    ///
    /// [`NotSubmitted`].
    pub fn find(&self, point: u64) -> Result<FenceRef, NotSubmitted> {
        match *self {
            SyncFence::None => Err(NotSubmitted),
            SyncFence::Single(fence) => {
                if point == 0 {
                    Ok(fence)
                } else {
                    Err(NotSubmitted)
                }
            }
            SyncFence::Chain {
                prefix,
                base,
                points,
                len,
            } => {
                let kept = points.get(..usize::from(len)).unwrap_or(&[]);
                let head = kept.last().map_or(base, |(value, _)| *value);
                if point == 0 {
                    return Ok(kept
                        .iter()
                        .fold(prefix, |all, (_, fence)| all.merge(*fence)));
                }
                if head < point {
                    return Err(NotSubmitted);
                }
                if point <= base {
                    return Ok(FenceRef::Signalled(0));
                }
                let mut all = prefix;
                for (value, fence) in kept {
                    all = all.merge(*fence);
                    if *value >= point {
                        break;
                    }
                }
                Ok(all)
            }
        }
    }

    /// `drm_syncobj_add_point`: a new point `value` signalled by `fence`, on
    /// top of whatever fence there was. A point not above the head is
    /// added at the head's value, as `dma_fence_chain_init` does.
    pub fn add_point(&mut self, value: u64, fence: FenceRef, requests: &Requests) {
        match *self {
            SyncFence::None | SyncFence::Single(_) => {
                let prefix = match *self {
                    SyncFence::Single(previous) => previous,
                    _ => FenceRef::Signalled(0),
                };
                let mut points = [(0, FenceRef::Signalled(0)); limits::POINTS];
                if let Some(first) = points.first_mut() {
                    *first = (value, fence);
                }
                *self = SyncFence::Chain {
                    prefix,
                    base: 0,
                    points,
                    len: 1,
                };
            }
            SyncFence::Chain {
                ref mut prefix,
                ref mut base,
                ref mut points,
                ref mut len,
            } => {
                fold_signalled(prefix, base, points, len, requests);
                if usize::from(*len) == limits::POINTS {
                    merge_lowest(points, len);
                }
                let head = if *len > 0 {
                    points
                        .get(usize::from(*len) - 1)
                        .map_or(*base, |(value, _)| *value)
                } else {
                    *base
                };
                if let Some(slot) = points.get_mut(usize::from(*len)) {
                    *slot = (value.max(head), fence);
                    *len += 1;
                }
            }
        }
    }

    /// `drm_syncobj_query_ioctl`'s point: the last submitted (the head), or
    /// the highest point every point up to which has signalled. Not a chain:
    /// 0.
    #[must_use]
    pub fn query(&self, last_submitted: bool, requests: &Requests) -> u64 {
        match *self {
            SyncFence::None | SyncFence::Single(_) => 0,
            SyncFence::Chain {
                prefix,
                base,
                points,
                len,
            } => {
                let kept = points.get(..usize::from(len)).unwrap_or(&[]);
                if last_submitted {
                    return kept.last().map_or(base, |(value, _)| *value);
                }
                if requests.status(prefix).is_none() {
                    return base;
                }
                let mut reached = base;
                for (value, fence) in kept {
                    if requests.status(*fence).is_none() {
                        break;
                    }
                    reached = *value;
                }
                reached
            }
        }
    }
}

/// Fold the signalled points at the bottom of a chain into its base, as
/// `dma_fence_chain_walk` collects them.
fn fold_signalled(
    prefix: &mut FenceRef,
    base: &mut u64,
    points: &mut [(u64, FenceRef); limits::POINTS],
    len: &mut u8,
    requests: &Requests,
) {
    while *len > 0 && requests.status(*prefix).is_some() {
        let Some((first_value, first_fence)) = points.first().copied() else {
            break;
        };
        if requests.status(first_fence).is_none() {
            break;
        }
        *base = first_value;
        *prefix = FenceRef::Signalled(0);
        points.copy_within(1.., 0);
        *len -= 1;
    }
}

/// Merge a full chain's two lowest points: waiting for the lower one then
/// waits for both, never for less.
fn merge_lowest(points: &mut [(u64, FenceRef); limits::POINTS], len: &mut u8) {
    let (Some(first), Some(second)) = (points.first().copied(), points.get(1).copied()) else {
        return;
    };
    points.copy_within(1.., 0);
    if let Some(merged) = points.first_mut() {
        *merged = (second.0, first.1.merge(second.1));
    }
    *len -= 1;
}

/// A held syncobj wait.
#[derive(Clone, Copy, Debug)]
pub struct SyncWait {
    /// `TIMELINE_WAIT` rather than `WAIT`.
    pub timeline: bool,
    /// The wait's flags.
    pub flags: u32,
    /// How many syncobjs.
    pub count: u8,
    /// Their slots.
    pub slots: [u16; limits::WAIT_HANDLES],
    /// Their points.
    pub points: [u64; limits::WAIT_HANDLES],
    /// The fence each waits on, once there is one (`entries[i].fence`).
    pub fences: [Option<FenceRef>; limits::WAIT_HANDLES],
    /// `first_signaled` as the first pass found it.
    pub first: u32,
}

/// A new syncobj of `file`, with `fence`.
fn new_syncobj<M>(state: &mut Tables<M>, file: u8, fence: SyncFence) -> Result<u32, Errno> {
    let slot = state
        .syncobjs
        .insert(Syncobj { file, fence })
        .ok_or(Errno::ENOMEM)?;
    u32::try_from(slot + 1).map_err(|_| Errno::ENOMEM)
}

/// The slot a file's handle names.
fn find<M>(state: &Tables<M>, file: u8, handle: u32) -> Result<usize, Errno> {
    state.syncobj_of(file, handle).ok_or(Errno::ENOENT)
}

/// The fence of syncobj `slot` at `point`, for an execbuffer or an export.
///
/// # Errors
///
/// `ENOENT`, or `EINVAL` when there is no fence for the point.
pub fn fence_at<M>(state: &Tables<M>, slot: usize, point: u64) -> Result<FenceRef, Errno> {
    let sync = state.syncobjs.get(slot).ok_or(Errno::ENOENT)?;
    sync.fence.find(point).map_err(|NotSubmitted| Errno::EINVAL)
}

/// `drm_syncobj_replace_fence`.
pub fn replace<M>(state: &mut Tables<M>, slot: usize, fence: SyncFence) {
    if let Some(sync) = state.syncobjs.get_mut(slot) {
        sync.fence = fence;
    }
}

/// `drm_syncobj_add_point`.
pub fn add_point<M>(state: &mut Tables<M>, slot: usize, value: u64, fence: FenceRef) {
    let Tables {
        syncobjs, requests, ..
    } = state;
    if let Some(sync) = syncobjs.get_mut(slot) {
        sync.fence.add_point(value, fence, requests);
    }
}

/// A released file's syncobjs.
pub fn release_file<M>(state: &mut Tables<M>, file: u8) {
    let mut doomed = [usize::MAX; 256];
    loop {
        let mut count = 0;
        for (slot, sync) in state.syncobjs.iter() {
            if sync.file == file
                && let Some(cell) = doomed.get_mut(count)
            {
                *cell = slot;
                count += 1;
            }
        }
        for slot in doomed.iter().take(count) {
            let _ = state.syncobjs.remove(*slot);
        }
        if count < doomed.len() {
            break;
        }
    }
}

/// `drm_syncobj_create_ioctl`.
pub fn create<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = SyncobjCreate::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.flags & !consts::SYNCOBJ_CREATE_SIGNALED != 0 {
        return Err(Errno::EINVAL);
    }
    let fence = if args.flags & consts::SYNCOBJ_CREATE_SIGNALED != 0 {
        SyncFence::Single(FenceRef::Signalled(0))
    } else {
        SyncFence::None
    };
    args.handle = new_syncobj(&mut frontend.state, call.file, fence)?;
    put(data, &args.to_bytes());
    Ok(())
}

/// `drm_syncobj_destroy_ioctl`.
pub fn destroy<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = SyncobjDestroy::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.pad != 0 {
        return Err(Errno::EINVAL);
    }
    let slot = find(&frontend.state, call.file, args.handle)?;
    let _ = frontend.state.syncobjs.remove(slot);
    Ok(())
}

/// `drm_syncobj_handle_to_fd_ioctl`: with `EXPORT_SYNC_FILE`, a sync file
/// of the fence at the point (`drm_syncobj_export_sync_file`); without it, an
/// opaque descriptor, which is not served.
pub fn handle_to_fd<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let mut args = SyncobjHandle::from_bytes(data).ok_or(Errno::EFAULT)?;
    let valid = consts::SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE
        | consts::SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE;
    if args.pad != 0 || args.flags & !valid != 0 {
        return Err(Errno::EINVAL);
    }
    let point = if args.flags & consts::SYNCOBJ_HANDLE_TO_FD_FLAGS_TIMELINE != 0 {
        args.point
    } else {
        0
    };
    if args.flags & consts::SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE == 0 {
        if args.point != 0 {
            return Err(Errno::EINVAL);
        }
        let _ = find(&frontend.state, call.file, args.handle)?;
        return Err(unserved_as(
            frontend,
            host,
            call,
            "SYNCOBJ_HANDLE_TO_FD as an opaque syncobj descriptor (a sync file is served)",
            Errno::EOPNOTSUPP,
        ));
    }
    let slot = find(&frontend.state, call.file, args.handle)?;
    let fence = fence_at(&frontend.state, slot, point)?;
    args.fd = export_sync_file(frontend, host, call, fence)?;
    put(data, &args.to_bytes());
    Ok(())
}

/// A sync file of `fence` in the program waiting in `call`, close-on-exec as
/// `get_unused_fd_flags(O_CLOEXEC)` makes it.
///
/// # Errors
///
/// What installing it was refused with; `ENOMEM` for a full table.
pub fn export_sync_file<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    fence: FenceRef,
) -> Result<i32, Errno> {
    let state = &mut frontend.state;
    let status = state.requests.status(fence);
    if status.is_none() && state.kernel_fences.len() == limits::KERNEL_FENCES {
        return Err(Errno::ENOMEM);
    }
    let cookie = state.cookie();
    let fd = host.sync_install(call.request, cookie, true)?;
    match status {
        Some(status) => host.sync_signal(cookie, status),
        None => fence::track_kernel_fence(state, cookie, fence)?,
    }
    Ok(fd)
}

/// The fence a sync file of this driver's stands for, by `sync_resolve`.
///
/// # Errors
///
/// `EINVAL` for a descriptor that is no sync file of this driver's
/// (`sync_file_get_fence` failing).
pub fn import_sync_file<M, H: Host<M>>(
    state: &Tables<M>,
    host: &mut H,
    call: &Call,
    fd: i32,
) -> Result<FenceRef, Errno> {
    let (cookie, signalled) = host
        .sync_resolve(call.request, fd)
        .map_err(|_| Errno::EINVAL)?;
    if signalled {
        return Ok(FenceRef::Signalled(0));
    }
    Ok(state
        .kernel_fences
        .iter()
        .find(|(_, kernel)| kernel.cookie == cookie)
        .map_or(FenceRef::Signalled(0), |(_, kernel)| kernel.fence))
}

/// `drm_syncobj_fd_to_handle_ioctl`: with `IMPORT_SYNC_FILE`, the sync
/// file's fence into the syncobj at the point
/// (`drm_syncobj_import_sync_file_fence`); without it, an opaque
/// descriptor, which none of this driver's can be.
pub fn fd_to_handle<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = SyncobjHandle::from_bytes(data).ok_or(Errno::EFAULT)?;
    let valid = consts::SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE
        | consts::SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE;
    if args.pad != 0 || args.flags & !valid != 0 {
        return Err(Errno::EINVAL);
    }
    let point = if args.flags & consts::SYNCOBJ_FD_TO_HANDLE_FLAGS_TIMELINE != 0 {
        args.point
    } else {
        0
    };
    if args.flags & consts::SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE == 0 {
        if args.point != 0 {
            return Err(Errno::EINVAL);
        }
        // `drm_syncobj_fd_to_handle`: `fget` of a file that is no syncobj's
        // is `EINVAL`; this driver makes none.
        return Err(Errno::EINVAL);
    }
    let fence = import_sync_file(&frontend.state, host, call, args.fd)?;
    let slot = find(&frontend.state, call.file, args.handle)?;
    if point != 0 {
        add_point(&mut frontend.state, slot, point, fence);
    } else {
        replace(&mut frontend.state, slot, SyncFence::Single(fence));
    }
    Ok(())
}

/// `drm_syncobj_transfer_ioctl`: the source's fence at its point onto the
/// destination, as a new point or replacing its fence. `WAIT_FOR_SUBMIT`
/// would sleep in the kernel until the point is submitted; it is answered
/// as the point is now.
pub fn transfer<B: Backend>(
    frontend: &mut Frontend<B>,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = SyncobjTransfer::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.pad != 0 {
        return Err(Errno::EINVAL);
    }
    let state = &mut frontend.state;
    let destination = find(state, call.file, args.dst_handle)?;
    if args.flags & !consts::SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT != 0 {
        return Err(Errno::EINVAL);
    }
    let source = find(state, call.file, args.src_handle)?;
    let fence = fence_at(state, source, args.src_point)?;
    if args.dst_point != 0 {
        add_point(state, destination, args.dst_point, fence);
    } else {
        replace(state, destination, SyncFence::Single(fence));
    }
    Ok(())
}

/// Copy in `count` `u32` handles and find each (`drm_syncobj_array_find`).
fn find_array<M, H: Host<M>>(
    state: &Tables<M>,
    host: &mut H,
    call: &Call,
    address: u64,
    count: u32,
    out: &mut [u16; limits::WAIT_HANDLES],
) -> Result<usize, Errno> {
    let count = usize::try_from(count).map_err(|_| Errno::ENOMEM)?;
    if count > limits::WAIT_HANDLES {
        return Err(Errno::ENOMEM);
    }
    for (index, cell) in out.iter_mut().enumerate().take(count) {
        let mut raw = [0_u8; 4];
        host.copy_in(call.request, address + 4 * index as u64, &mut raw)
            .map_err(|_| Errno::EFAULT)?;
        let slot = find(state, call.file, u32::from_le_bytes(raw))?;
        *cell = u16::try_from(slot).map_err(|_| Errno::ENOENT)?;
    }
    Ok(count)
}

/// `drm_syncobj_reset_ioctl`.
pub fn reset<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = SyncobjArray::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.pad != 0 || args.count_handles == 0 {
        return Err(Errno::EINVAL);
    }
    let mut slots = [0_u16; limits::WAIT_HANDLES];
    let count = find_array(
        &frontend.state,
        host,
        call,
        args.handles,
        args.count_handles,
        &mut slots,
    )?;
    for slot in slots.iter().take(count) {
        replace(&mut frontend.state, usize::from(*slot), SyncFence::None);
    }
    Ok(())
}

/// `drm_syncobj_signal_ioctl`: each replaced by a signalled stub.
pub fn signal<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = SyncobjArray::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.pad != 0 || args.count_handles == 0 {
        return Err(Errno::EINVAL);
    }
    let mut slots = [0_u16; limits::WAIT_HANDLES];
    let count = find_array(
        &frontend.state,
        host,
        call,
        args.handles,
        args.count_handles,
        &mut slots,
    )?;
    for slot in slots.iter().take(count) {
        replace(
            &mut frontend.state,
            usize::from(*slot),
            SyncFence::Single(FenceRef::Signalled(0)),
        );
    }
    Ok(())
}

/// Copy in `count` `u64` points, or zeros for a null array.
fn points_in<M, H: Host<M>>(
    host: &mut H,
    call: &Call,
    address: u64,
    count: usize,
    out: &mut [u64; limits::WAIT_HANDLES],
) -> Result<(), Errno> {
    if address == 0 {
        return Ok(());
    }
    for (index, cell) in out.iter_mut().enumerate().take(count) {
        let mut raw = [0_u8; 8];
        host.copy_in(call.request, address + 8 * index as u64, &mut raw)
            .map_err(|_| Errno::EFAULT)?;
        *cell = u64::from_le_bytes(raw);
    }
    Ok(())
}

/// `drm_syncobj_timeline_signal_ioctl`: a signalled point on each.
pub fn timeline_signal<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = SyncobjTimelineArray::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.flags != 0 || args.count_handles == 0 {
        return Err(Errno::EINVAL);
    }
    let mut slots = [0_u16; limits::WAIT_HANDLES];
    let count = find_array(
        &frontend.state,
        host,
        call,
        args.handles,
        args.count_handles,
        &mut slots,
    )?;
    let mut points = [0_u64; limits::WAIT_HANDLES];
    points_in(host, call, args.points, count, &mut points)?;
    for (slot, point) in slots.iter().zip(points.iter()).take(count) {
        add_point(
            &mut frontend.state,
            usize::from(*slot),
            *point,
            FenceRef::Signalled(0),
        );
    }
    Ok(())
}

/// `drm_syncobj_query_ioctl`.
pub fn query<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
) -> Result<(), Errno> {
    let args = SyncobjTimelineArray::from_bytes(data).ok_or(Errno::EFAULT)?;
    if args.flags & !consts::SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED != 0 || args.count_handles == 0 {
        return Err(Errno::EINVAL);
    }
    let mut slots = [0_u16; limits::WAIT_HANDLES];
    let count = find_array(
        &frontend.state,
        host,
        call,
        args.handles,
        args.count_handles,
        &mut slots,
    )?;
    let last = args.flags & consts::SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED != 0;
    for (index, slot) in slots.iter().enumerate().take(count) {
        let state = &frontend.state;
        let point = state
            .syncobjs
            .get(usize::from(*slot))
            .map_or(0, |sync| sync.fence.query(last, &state.requests));
        host.copy_out(
            call.request,
            args.points + 8 * index as u64,
            &point.to_le_bytes(),
        )
        .map_err(|_| Errno::EFAULT)?;
    }
    Ok(())
}

/// `drm_syncobj_wait_ioctl` and `drm_syncobj_timeline_wait_ioctl`: the
/// first pass, answered at once when settled, held otherwise.
pub fn wait<B: Backend, H: Host<B::Memory>>(
    frontend: &mut Frontend<B>,
    host: &mut H,
    call: &Call,
    data: &mut [u8],
    timeline: bool,
) -> Result<Option<Hold>, Errno> {
    let (handles, points, timeout, count, flags) = if timeline {
        let args = SyncobjTimelineWait::from_bytes(data).ok_or(Errno::EFAULT)?;
        (
            args.handles,
            args.points,
            args.timeout_nsec,
            args.count_handles,
            args.flags,
        )
    } else {
        let args = SyncobjWait::from_bytes(data).ok_or(Errno::EFAULT)?;
        (
            args.handles,
            0,
            args.timeout_nsec,
            args.count_handles,
            args.flags,
        )
    };
    let mut possible = consts::SYNCOBJ_WAIT_FLAGS_WAIT_ALL
        | consts::SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT
        | consts::SYNCOBJ_WAIT_FLAGS_WAIT_DEADLINE;
    if timeline {
        possible |= consts::SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE;
    }
    if flags & !possible != 0 {
        return Err(Errno::EINVAL);
    }
    if count == 0 {
        return Ok(None);
    }
    let mut sync = SyncWait {
        timeline,
        flags,
        count: 0,
        slots: [0; limits::WAIT_HANDLES],
        points: [0; limits::WAIT_HANDLES],
        fences: [None; limits::WAIT_HANDLES],
        first: u32::MAX,
    };
    let found = find_array(&frontend.state, host, call, handles, count, &mut sync.slots)?;
    sync.count = u8::try_from(found).map_err(|_| Errno::ENOMEM)?;
    if timeline {
        points_in(host, call, points, found, &mut sync.points)?;
    }
    // The first pass (`drm_syncobj_array_wait_timeout`): each fence now, or
    // `EINVAL` for one not there unless waiting for its submission.
    let submit = flags
        & (consts::SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT | consts::SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE)
        != 0;
    for index in 0..found {
        let slot = usize::from(sync.slots.get(index).copied().unwrap_or(0));
        let point = sync.points.get(index).copied().unwrap_or(0);
        match fence_at(&frontend.state, slot, point) {
            Ok(fence) => {
                if let Some(cell) = sync.fences.get_mut(index) {
                    *cell = Some(fence);
                }
            }
            Err(_) if submit => {}
            Err(errno) => {
                return Err(frontend
                    .state
                    .refuse("a point not submitted, no WAIT_FOR_SUBMIT", errno));
            }
        }
    }
    for index in 0..found {
        if is_signalled(&frontend.state, &sync, index) {
            sync.first = u32::try_from(index).unwrap_or(u32::MAX);
            break;
        }
    }
    let mut copy = [0_u8; 48];
    if let Some(target) = copy.get_mut(..data.len().min(48)) {
        let length = target.len();
        target.copy_from_slice(data.get(..length).unwrap_or(&[]));
    }
    let expired = drm_timeout_is_poll(timeout, host.now_ns());
    if let Some(status) = evaluate_wait(&frontend.state, &mut sync, &mut copy, expired) {
        put(data, copy.get(..data.len().min(48)).unwrap_or(&[]));
        return if status == 0 {
            Ok(None)
        } else {
            Err(Errno((-status) as u16))
        };
    }
    let deadline = (timeout > 0).then_some(timeout as u64);
    Ok(Some(Hold {
        deadline,
        kind: WaitKind::Syncobjs(sync),
    }))
}

/// `drm_timeout_abs_to_jiffies(timeout) == 0`: 0 means poll, and so does a
/// time already past.
fn drm_timeout_is_poll(timeout: i64, now: u64) -> bool {
    timeout <= 0 || (timeout as u64) <= now
}

/// Whether entry `index` of a wait counts as signalled: its fence is there
/// and (unless only its submission is waited for) signalled.
fn is_signalled<M>(state: &Tables<M>, sync: &SyncWait, index: usize) -> bool {
    match sync.fences.get(index).copied().flatten() {
        None => false,
        Some(_) if sync.flags & consts::SYNCOBJ_WAIT_FLAGS_WAIT_AVAILABLE != 0 => true,
        Some(fence) => state.requests.status(fence).is_some(),
    }
}

/// A held syncobj wait: `Some(status)` once settled (`first_signaled`
/// written for a wait for any) or expired (`ETIME`).
pub fn evaluate_wait<M>(
    state: &Tables<M>,
    sync: &mut SyncWait,
    data: &mut [u8; 48],
    expired: bool,
) -> Option<i32> {
    let count = usize::from(sync.count);
    // An entry still waiting for its submission takes the fence once there
    // (`syncobj_wait_syncobj_func`).
    for index in 0..count {
        if sync.fences.get(index).copied().flatten().is_none() {
            let slot = usize::from(sync.slots.get(index).copied().unwrap_or(0));
            let point = sync.points.get(index).copied().unwrap_or(0);
            if let Ok(fence) = fence_at(state, slot, point)
                && let Some(cell) = sync.fences.get_mut(index)
            {
                *cell = Some(fence);
            }
        }
    }
    let all = sync.flags & consts::SYNCOBJ_WAIT_FLAGS_WAIT_ALL != 0;
    let mut signalled = 0;
    let mut first = None;
    for index in 0..count {
        if is_signalled(state, sync, index) {
            signalled += 1;
            let _ = first.get_or_insert(index);
        }
    }
    let first_offset = if sync.timeline { 32 } else { 24 };
    let write_first = |data: &mut [u8; 48], value: u32| {
        if let Some(target) = data.get_mut(first_offset..first_offset + 4) {
            target.copy_from_slice(&value.to_le_bytes());
        }
    };
    if all && signalled == count {
        write_first(data, sync.first);
        return Some(0);
    }
    if !all && let Some(index) = first {
        write_first(data, u32::try_from(index).unwrap_or(u32::MAX));
        return Some(0);
    }
    expired.then(|| -i32::from(ETIME.0))
}
