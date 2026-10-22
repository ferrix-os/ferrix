//! A dmabuf a chardev driver made: the whole of one of its VMOs as a
//! descriptor in the program it is answering (`docs/NVIDIA.md` §4.4, N3b;
//! the certification consultant's design verdict, ledger 316).
//!
//! nvidia-drm's `PRIME_HANDLE_TO_FD` answers a descriptor a compositor can
//! map, and its `PRIME_FD_TO_HANDLE` asks which buffer a descriptor is. The
//! driver serving the render node through this core holds the buffer as a
//! VMO, so `chardev_dmabuf_install` makes the descriptor from that VMO and
//! a cookie the driver names it by, and `chardev_dmabuf_resolve` gives the
//! cookie back for a descriptor this same control made.
//!
//! # What holds
//!
//! * Whole VMO only, and plain anonymous memory only, given with `TRANSFER`:
//!   a mapping of the descriptor can reach no byte the driver did not hand
//!   over, and no window, aperture or file (the consultant's (1), (2)).
//! * One object per (control, cookie): a live dmabuf for the cookie gets a
//!   new descriptor to the same object, so the driver hears
//!   [`Op::DmabufRelease`] once per object, when the last descriptor and
//!   the last mapping are gone (3, B4).
//! * The dmabuf holds its control weakly, so it keeps no claim on the
//!   device: a dead driver's dmabufs keep their bytes and their mappings,
//!   and a new driver's resolve refuses them (B2, B3, B7).
//! * One object per cookie at a time, also while the last one is going:
//!   an install that finds the cookie's entry dead but not yet out of the
//!   table (its drop has begun and not yet taken the table's lock) is
//!   `SHOULD_WAIT`, and nvrm retries, so the driver never hears MADE for a
//!   second object before the first one's release (ledger 726, B2).
//! * No object for a dead control: `find_or_make` reads gone under the
//!   table's lock and `finish` takes that lock after storing it (ledger
//!   726, B3). An install that made its object before then still puts its
//!   descriptor in the program even if the driver dies before replying and
//!   the call answers `ENODEV`: the same as a driver that installs and then
//!   dies, a working dmabuf the program closes (B7), never a claim on the
//!   device.
//! * `DMA_BUF_IOCTL_SYNC` answers 0, and poll is always ready (B9).
//!
//! # Its fence slot (K5)
//!
//! Mesa's anv refuses a device whose dmabufs do not answer
//! `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` (`docs/INTEL-GPU.md` §2.1 and §5, K5;
//! the consultant's K5 (1)-(5), ledger 2352). So each dmabuf has one fence
//! slot, which stands for its driver's latest queued GPU access of any
//! kind to it, reads included: the driver sets it for a GPU read of the
//! buffer as well as for a write, so that a fence exported for either kind
//! of access waits for both (the consultant's single-slot ruling, ledger
//! 2360):
//!
//! * Only the dmabuf's own driver sets it: `chardev_dmabuf_fence` names the
//!   dmabuf by cookie in the calling control's own table, so another
//!   control finds nothing (`BAD_STATE`), and the fence it puts there is a
//!   new one of the control's own, which only that control signals, which
//!   its deadline bounds, and which the control's end signals `ENODEV`
//!   (ledger 647's S-rules, unchanged: `sync.rs`).
//! * `EXPORT_SYNC_FILE` installs, in the calling program, a `sync_file` of
//!   the fence the slot holds at the call: a snapshot, as Linux's is, which
//!   a later set does not change; with the slot empty, a fence signalled
//!   already, as Linux's stub. Read or write asked, the one slot is what is
//!   waited for.
//! * `IMPORT_SYNC_FILE` changes nothing in the kernel: the import is passed
//!   to the dmabuf's driver as data ([`Op::DmabufImport`]: the dmabuf's
//!   cookie, the flags, and what the fence is), and the program's call
//!   waits for the driver's answer as an ioctl does, never for the fence.
//!   No other program and no kernel path waits on an imported fence.
//!   A dmabuf whose driver is gone answers `ENODEV`.
//! * The slot's lock is a leaf: it is held for a swap or a clone of one
//!   `Arc`, nothing is taken under it, and a fence it gives up is dropped
//!   after it is released, since a fence's drop takes its control's table.
//!
//! [`Op::DmabufImport`]: ferrix_chardevctl::message::Op::DmabufImport
//!
//! # Name-only dmabufs
//!
//! A buffer in video memory is shared as a dmabuf with no VMO at all
//! ([`DMABUF_NAME_ONLY`], ledgers 632 and 724): only the cookie and the size
//! its driver says it has. Nothing maps it (`mmap` is `ENODEV`) or runs it
//! (exec reads nothing from it), and only its maker's resolve gives the
//! cookie back, so its one use is to come back to the same driver from
//! another program. It is never writable: there is no handle whose `WRITE`
//! could decide the descriptor's mode, so `DMABUF_WRITABLE` with it is
//! `INVALID_ARGS` (724's V3). The size is the driver's claim and feeds
//! `fstat` and `lseek` only (N3). Everything else above holds for it as
//! written: the table, the cap, one object per cookie and the release.
//!
//! [`Op::DmabufRelease`]: ferrix_chardevctl::message::Op::DmabufRelease

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{
    DMABUF_CLOEXEC, DMABUF_FLAGS, DMABUF_MADE, DMABUF_NAME_MAX, DMABUF_NAME_ONLY, DMABUF_TELL_MADE,
    DMABUF_WRITABLE,
};
use ferrix_vfs::{Inode, Metadata, Readiness, Result as VfsResult};

use crate::object::Object;
use crate::object::process::Host;
use crate::sync::SpinLock;
use crate::syscall::process::Process;
use crate::syscall::uaccess;
use crate::user::vmo::Vmo;

use super::Control;
use super::sync::SyncFile;

/// The most dmabufs one control has alive; the next install is
/// `LIMIT_REACHED` (B5).
pub(crate) const MAX_DMABUFS: usize = 4096;

/// What `/proc/self/fd` calls one, as Linux names it.
const NAME: &[u8] = b"anon_inode:dmabuf";

/// `DMA_BUF_IOCTL_SYNC`: `_IOW('b', 0, struct dma_buf_sync)`.
const DMA_BUF_IOCTL_SYNC: u32 = 0x4008_6200;
/// `struct dma_buf_sync`'s flags Linux knows: `READ`, `WRITE` and `END`.
const SYNC_VALID: u64 = 0x7;
/// `DMA_BUF_SYNC_RW`: a sync names reading, writing or both.
const SYNC_RW: u64 = 0x3;
/// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`:
/// `_IOWR('b', 2, struct dma_buf_export_sync_file)`.
pub(crate) const DMA_BUF_IOCTL_EXPORT_SYNC_FILE: u32 = 0xC008_6202;
/// `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`:
/// `_IOW('b', 3, struct dma_buf_import_sync_file)`.
pub(crate) const DMA_BUF_IOCTL_IMPORT_SYNC_FILE: u32 = 0x4008_6203;

/// What a dmabuf is over: one of exactly two kinds (ledger 632's N2).
pub(crate) enum Backing {
    /// The whole of a plain anonymous VMO: what a mapping of it maps.
    Vmo(Arc<Vmo>),
    /// No memory the kernel knows: a buffer in its driver's video memory,
    /// of the size the driver says, which is only ever reported (N3).
    Name {
        /// Bytes, a multiple of 4096 and at most `DMABUF_NAME_MAX`.
        size: u64,
    },
}

impl Backing {
    /// Whether a live dmabuf over `self` may be handed out for an install
    /// asking for `wanted`: the same VMO, or a name of the same size.
    fn same(&self, wanted: &Backing) -> bool {
        match (self, wanted) {
            (Backing::Vmo(held), Backing::Vmo(asked)) => Arc::ptr_eq(held, asked),
            (Backing::Name { size: held }, Backing::Name { size: asked }) => held == asked,
            (Backing::Vmo(_), Backing::Name { .. }) | (Backing::Name { .. }, Backing::Vmo(_)) => {
                false
            }
        }
    }
}

/// One dmabuf. Every descriptor of it holds it, and every mapping holds a
/// descriptor's open file, so it goes with the last of both.
pub(crate) struct Dmabuf {
    backing: Backing,
    cookie: u64,
    /// Its maker, by identity: what resolve compares, and whom the release
    /// is for while it lives. Weak, so no claim is kept (B2).
    control: Weak<Control>,
    /// Whether an install of it reached a program: only then does the
    /// driver hear of it, made and released, and only then was the
    /// release's slot in the control's queue not given back.
    announced: AtomicBool,
    /// The fence its driver last put in its slot, if any: what an export
    /// snapshots (K5). A leaf lock.
    fence: SpinLock<Option<Arc<SyncFile>>>,
}

impl core::fmt::Debug for Dmabuf {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut out = formatter.debug_struct("Dmabuf");
        let _ = out.field("cookie", &self.cookie);
        let _ = match &self.backing {
            Backing::Vmo(vmo) => out.field("pages", &vmo.len_pages()),
            Backing::Name { size } => out.field("name_only_bytes", size),
        };
        out.finish_non_exhaustive()
    }
}

impl Dmabuf {
    /// The cookie its driver named it by.
    pub(crate) fn cookie(&self) -> u64 {
        self.cookie
    }

    /// Its VMO, for one that has one: what card0 hands a copying display
    /// driver `READ | MAP` when a program shows the buffer directly
    /// (`docs/DISPLAY.md` §8.2). `None` for a name-only one.
    pub(crate) fn vmo(&self) -> Option<&Arc<Vmo>> {
        match &self.backing {
            Backing::Vmo(vmo) => Some(vmo),
            Backing::Name { .. } => None,
        }
    }

    /// Whether `control` made it.
    pub(crate) fn made_by(&self, control: &Arc<Control>) -> bool {
        core::ptr::eq(self.control.as_ptr(), Arc::as_ptr(control))
    }
}

impl Drop for Dmabuf {
    fn drop(&mut self) {
        let Some(control) = self.control.upgrade() else {
            return;
        };
        let me: *const Dmabuf = self;
        let mut dmabufs = control.dmabufs.lock();
        dmabufs.retain(|(_, held)| !core::ptr::eq(held.as_ptr(), me));
        // Queued before the entry's absence can be seen: an install for the
        // cookie finds the dead entry (`SHOULD_WAIT`) or this release already
        // queued, never a new object ahead of it (ledger 726, B2). The
        // table's lock comes before the control's `state`, as in
        // `find_or_make`'s `hold_release`.
        if self.announced.load(Ordering::Acquire) {
            super::queue_dmabuf_release(&control, self.cookie);
        } else {
            super::unhold_release(&control);
        }
        drop(dmabufs);
    }
}

impl Inode for Dmabuf {
    /// An anonymous file the size of its buffer, which is what Linux's
    /// `lseek(fd, 0, SEEK_END)` on a dmabuf answers.
    fn metadata(&self) -> Metadata {
        let size = match &self.backing {
            Backing::Vmo(vmo) => vmo.len_bytes(),
            // The driver's claim, reported and never used (N3).
            Backing::Name { size } => *size,
        };
        Metadata {
            size,
            ..crate::fs::anon::metadata()
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn splices_out(&self) -> bool {
        false
    }

    /// Nothing is read or written through it, as Linux's dmabuf has no
    /// `read` or `write`: its bytes are reached by mapping it.
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(Errno::EINVAL)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8], _append: bool) -> VfsResult<(usize, u64)> {
        Err(Errno::EINVAL)
    }

    /// Always ready: there is no fence to wait for (B9).
    fn poll(&self) -> Readiness {
        Readiness {
            readable: true,
            writable: true,
            hangup: false,
            error: false,
            priority: false,
        }
    }

    /// Its VMO, from `offset`: `mmap` maps it through the open file, so a
    /// mapping holds the dmabuf, and refuses a range past the VMO's end,
    /// which is the buffer's end (the consultant's (1)). A name-only one
    /// has nothing to map: `mmap` turns this `None` into `ENODEV` (N5).
    fn mapping_at(&self, offset: u64) -> Option<(Arc<dyn Any + Send + Sync>, u64)> {
        match &self.backing {
            Backing::Vmo(vmo) => {
                let vmo: Arc<dyn Any + Send + Sync> = Arc::clone(vmo) as _;
                Some((vmo, offset))
            }
            Backing::Name { .. } => None,
        }
    }
}

/// The dmabuf behind an open file, if it is one.
pub(crate) fn of(io: &Arc<dyn Inode>) -> Option<Arc<Dmabuf>> {
    Arc::clone(io).into_any().downcast::<Dmabuf>().ok()
}

/// An ioctl on a dmabuf: `DMA_BUF_IOCTL_SYNC` checked as Linux checks it
/// and answered 0 (B9); `EXPORT_SYNC_FILE` and `IMPORT_SYNC_FILE` as the
/// module says (K5); anything else `ENOTTY`.
///
/// # Errors
///
/// `ENOTTY`, `EFAULT` for an unreadable argument, `EINVAL` for flags Linux
/// refuses, and an export's or an import's own.
pub(crate) fn ioctl(
    process: &Process,
    dmabuf: &Dmabuf,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    match request {
        DMA_BUF_IOCTL_SYNC => {
            let mut bytes = [0u8; 8];
            uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
            let flags = u64::from_ne_bytes(bytes);
            if flags & !SYNC_VALID != 0 || flags & SYNC_RW == 0 {
                return Err(Errno::EINVAL);
            }
            Ok(0)
        }
        DMA_BUF_IOCTL_EXPORT_SYNC_FILE => export(process, dmabuf, arg),
        DMA_BUF_IOCTL_IMPORT_SYNC_FILE => import(process, dmabuf, arg),
        _ => Err(Errno::ENOTTY),
    }
}

/// `struct dma_buf_export_sync_file` and `struct dma_buf_import_sync_file`
/// at `arg`: the flags, which must be read, write or both, and the
/// descriptor.
fn sync_file_argument(process: &Process, arg: u64) -> Result<(u32, i32), Errno> {
    let mut bytes = [0u8; 8];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let [a, b, c, d, e, f, g, h] = bytes;
    let flags = u32::from_ne_bytes([a, b, c, d]);
    if u64::from(flags) & !SYNC_RW != 0 || u64::from(flags) & SYNC_RW == 0 {
        return Err(Errno::EINVAL);
    }
    Ok((flags, i32::from_ne_bytes([e, f, g, h])))
}

/// `EXPORT_SYNC_FILE`: a `sync_file` of what the slot holds now, or of a
/// fence signalled already when it holds nothing, as a new close-on-exec
/// descriptor written back into the argument.
fn export(process: &Process, dmabuf: &Dmabuf, arg: u64) -> Result<usize, Errno> {
    let (_flags, _) = sync_file_argument(process, arg)?;
    // A clone under the leaf lock, and nothing more.
    let held = dmabuf.fence.lock().clone();
    let fence = match held {
        Some(fence) => fence,
        None => super::sync::signalled()?,
    };
    let fd = super::sync::descriptor(process, fence, true)?;
    if uaccess::copy_to_user(process.space(), arg + 4, &fd.to_ne_bytes()).is_err() {
        // Nobody was told of it: taken back, and closed unlocked.
        let taken = process.files().lock().remove(fd);
        drop(taken);
        return Err(Errno::EFAULT);
    }
    Ok(0)
}

/// `IMPORT_SYNC_FILE`, by the running program.
fn import(process: &Process, dmabuf: &Dmabuf, arg: u64) -> Result<usize, Errno> {
    let client = crate::syscall::process::current().ok_or(Errno::ENODEV)?;
    if !core::ptr::eq(Arc::as_ptr(&client), process) {
        return Err(Errno::ENODEV);
    }
    import_as(&client, dmabuf, arg)
}

/// `IMPORT_SYNC_FILE` by `client`: passed to the dmabuf's driver as data,
/// and answered with the driver's answer; nothing in the kernel waits on
/// the fence.
pub(super) fn import_as(client: &Arc<Process>, dmabuf: &Dmabuf, arg: u64) -> Result<usize, Errno> {
    use ferrix_chardevctl::message::{IMPORT_FOREIGN, IMPORT_OWN, IMPORT_SIGNALLED, Op};

    let process: &Process = client;
    let (flags, fd) = sync_file_argument(process, arg)?;
    let fence = crate::syscall::fd::file(process, fd)
        .ok()
        .and_then(|file| super::sync::of(file.io()))
        .ok_or(Errno::EINVAL)?;
    let control = dmabuf.control.upgrade().ok_or(Errno::ENODEV)?;
    if control.is_gone() {
        return Err(Errno::ENODEV);
    }
    let (kind, file) = match fence.status(crate::timer::now_nanos()) {
        #[expect(
            clippy::cast_sign_loss,
            reason = "the status travels as its two's complement"
        )]
        Some((code, _)) => (IMPORT_SIGNALLED, i64::from(code) as u64),
        None if fence.made_by(&control) => (IMPORT_OWN, fence.cookie()),
        None => (IMPORT_FOREIGN, 0),
    };
    drop(fence);
    let ask = super::Ask {
        op: Op::DmabufImport,
        file,
        minor: 0,
        cmd: flags,
        arg: dmabuf.cookie,
        pages: kind,
    };
    super::call(&control, client, ask).map(|_| 0)
}

/// `chardev_dmabuf_fence(control, dmabuf, cookie, deadline_ms, flags)`: a
/// new fence of the control's own in the slot of the control's own live
/// dmabuf `dmabuf`, the one there before given up (K5).
pub(crate) fn fence(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let [handle, cookie, fence_cookie, asked, flags, unused] = *registers;
    if flags != 0 || unused != 0 {
        return Err(status::INVALID_ARGS);
    }
    let control = super::control_of(caller, handle)?;
    let dmabuf = live(&control, cookie).ok_or(status::BAD_STATE)?;
    let fence = super::sync::made_watched(&control, fence_cookie, super::sync::deadline_ms(asked))?;
    let before = dmabuf.fence.lock().replace(fence);
    // Unlocked: a fence's drop takes its control's table.
    drop(before);
    drop(dmabuf);
    Ok(0)
}

/// `control`'s live dmabuf `cookie`, if it has one. Its table's lock is not
/// held when the reference is handed back: were it the last, its drop takes
/// that lock.
fn live(control: &Control, cookie: u64) -> Option<Arc<Dmabuf>> {
    let table = control.dmabufs.lock();
    let found = table
        .iter()
        .filter(|(held, _)| *held == cookie)
        .find_map(|(_, dmabuf)| dmabuf.upgrade());
    drop(table);
    found
}

/// The fence `dmabuf`'s slot holds now, for the self-check.
pub(crate) fn slot(dmabuf: &Dmabuf) -> Option<Arc<SyncFile>> {
    dmabuf.fence.lock().clone()
}

/// `chardev_dmabuf_install(control, request, vmo, cookie, flags, size)`: a
/// dmabuf over the whole of `vmo`, or with [`DMABUF_NAME_ONLY`] a name-only
/// one of `size` bytes, as a new descriptor in the waiting program.
pub(crate) fn install(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let [handle, id, vmo_handle, cookie, flags, size] = *registers;
    if flags & !DMABUF_FLAGS != 0 {
        return Err(status::INVALID_ARGS);
    }
    let writable = flags & DMABUF_WRITABLE != 0;
    let control = super::control_of(caller, handle)?;
    // Every register judged before anything is looked up or held (N1).
    let backing = if flags & DMABUF_NAME_ONLY != 0 {
        // No handle, so no `WRITE` to decide a writable descriptor by
        // (ledger 724, V3).
        if vmo_handle != 0 || writable || !name_size_valid(size) {
            return Err(status::INVALID_ARGS);
        }
        Backing::Name { size }
    } else {
        if size != 0 {
            return Err(status::INVALID_ARGS);
        }
        Backing::Vmo(handed_vmo(caller, vmo_handle, writable)?)
    };
    let request = super::outstanding(&control, id)?;
    super::begin_copy(&request)?;
    let installed = install_for(&control, &request.client, backing, cookie, flags);
    super::copy_done(&request);
    let (descriptor, made) = installed?;
    let descriptor = usize::try_from(descriptor).map_err(|_| status::INVALID_ARGS)?;
    Ok(if made && flags & DMABUF_TELL_MADE != 0 {
        descriptor | DMABUF_MADE
    } else {
        descriptor
    })
}

/// Whether a name-only dmabuf may say it is `size` bytes: not 0, whole
/// pages, at most [`DMABUF_NAME_MAX`] (N1).
fn name_size_valid(size: u64) -> bool {
    size != 0 && size.is_multiple_of(4096) && size <= DMABUF_NAME_MAX
}

/// The VMO `handle` names in `caller`'s table, if it may become a dmabuf:
/// `READ` and `TRANSFER`, `WRITE` too for a writable one, and plain
/// anonymous memory (the consultant's (2), B1).
fn handed_vmo(caller: &dyn Host, handle: u64, writable: bool) -> Result<Arc<Vmo>, Errno> {
    let needed =
        Rights(Rights::READ.0 | Rights::TRANSFER.0 | if writable { Rights::WRITE.0 } else { 0 });
    let vmo = caller.core().with_handles(|table| {
        let (object, rights) = table
            .get(Handle::from_register(handle))
            .map_err(|_| status::BAD_HANDLE)?;
        let Object::Vmo(vmo) = object else {
            return Err(status::WRONG_TYPE);
        };
        if !rights.contains(needed) {
            return Err(status::ACCESS_DENIED);
        }
        Ok(Arc::clone(vmo))
    })?;
    if !vmo.is_anonymous() || vmo.len_pages() == 0 {
        return Err(status::INVALID_ARGS);
    }
    Ok(vmo)
}

/// Find or make the dmabuf for `cookie` and give `client` a descriptor of
/// it: the descriptor, and whether this install is the one the driver
/// hears it was made by.
fn install_for(
    control: &Arc<Control>,
    client: &Arc<Process>,
    backing: Backing,
    cookie: u64,
    flags: u64,
) -> Result<(i32, bool), Errno> {
    let dmabuf = find_or_make(control, backing, cookie)?;
    let writable = flags & DMABUF_WRITABLE != 0;
    let inode: Arc<dyn Inode> = Arc::clone(&dmabuf) as _;
    let open =
        crate::fs::anon::open_mode(inode, NAME, false, writable).map_err(|_| status::NO_MEMORY)?;
    let descriptor = client
        .files()
        .lock()
        .insert(open, flags & DMABUF_CLOEXEC != 0)
        .map_err(|_| status::LIMIT_REACHED)?;
    // The first install to reach a program is the one that made it, as far
    // as the driver can tell: its release follows from here.
    let made = !dmabuf.announced.swap(true, Ordering::AcqRel);
    Ok((descriptor, made))
}

/// The live dmabuf for `cookie`, which must be over the same backing (the
/// same VMO, or a name of the same size), or a new one.
///
/// # Errors
///
/// `BAD_STATE` once the driver is gone; `SHOULD_WAIT` while the cookie's
/// last object is going; `ALREADY_BOUND` for a live cookie over another
/// backing; `LIMIT_REACHED` at [`MAX_DMABUFS`]; `NO_MEMORY`.
pub(super) fn find_or_make(
    control: &Arc<Control>,
    backing: Backing,
    cookie: u64,
) -> Result<Arc<Dmabuf>, Errno> {
    let mut dmabufs = control.dmabufs.lock();
    // Gone is read under the table's lock, and `finish` stores it before it
    // takes this lock: no object is made for a dead control (the
    // consultant's B3, ledger 726).
    if control.is_gone() {
        return Err(status::BAD_STATE);
    }
    let mut going = false;
    let mut live = None;
    for (_, held) in dmabufs.iter().filter(|(held, _)| *held == cookie) {
        match held.upgrade() {
            Some(dmabuf) => live = Some(dmabuf),
            None => going = true,
        }
    }
    if let Some(dmabuf) = live {
        // Unlocked before `dmabuf` can drop: were it the last reference,
        // its drop takes this lock.
        drop(dmabufs);
        return if dmabuf.backing.same(&backing) {
            Ok(dmabuf)
        } else {
            Err(status::ALREADY_BOUND)
        };
    }
    // The cookie's last object is in its drop, which has not yet taken
    // this lock to leave the table and queue its release. A new object now
    // would be a second for the cookie, and the driver would hear it made
    // and then the old one's release (the consultant's B2, ledger 726). The
    // drop takes nothing but this lock and the queue's, so the driver's
    // retry finds the entry gone.
    if going {
        return Err(status::SHOULD_WAIT);
    }
    if dmabufs.len() >= MAX_DMABUFS {
        return Err(status::LIMIT_REACHED);
    }
    dmabufs.try_reserve(1).map_err(|_| status::NO_MEMORY)?;
    // The release's slot in the queue to the driver, held from now (B4).
    super::hold_release(control).map_err(|_| status::NO_MEMORY)?;
    // Cyclic only for its failure, which builds nothing: a dmabuf dropped
    // here would take this lock in its drop and give the slot back twice.
    let made = crate::fallible::try_arc_cyclic(|_| Dmabuf {
        backing,
        cookie,
        control: Arc::downgrade(control),
        announced: AtomicBool::new(false),
        fence: SpinLock::new(None),
    });
    let Ok(dmabuf) = made else {
        drop(dmabufs);
        super::unhold_release(control);
        return Err(status::NO_MEMORY);
    };
    // NOALLOC: reserved above.
    dmabufs.push((cookie, Arc::downgrade(&dmabuf)));
    Ok(dmabuf)
}

/// `chardev_dmabuf_resolve(control, request, descriptor, cookie)`: the
/// cookie of the dmabuf the waiting program's descriptor names, written to
/// `cookie` in the driver, if this same control made it (B3).
pub(crate) fn resolve(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let [handle, id, descriptor, out, ..] = *registers;
    let control = super::control_of(caller, handle)?;
    let request = super::outstanding(&control, id)?;
    super::begin_copy(&request)?;
    let found = crate::syscall::fd::file(&request.client, crate::syscall::fd::arg(descriptor))
        .ok()
        .and_then(|file| of(file.io()))
        .filter(|dmabuf| dmabuf.made_by(&control))
        .map(|dmabuf| dmabuf.cookie());
    super::copy_done(&request);
    let cookie = found.ok_or(status::BAD_HANDLE)?;
    uaccess::copy_to_user(caller.core().space(), out, &cookie.to_ne_bytes())
        .map_err(|_| status::FAULT)?;
    Ok(0)
}

/// How many dmabufs `control` has alive, for the self-check.
pub(crate) fn alive(control: &Control) -> usize {
    control
        .dmabufs
        .lock()
        .iter()
        .filter(|(_, held)| held.strong_count() > 0)
        .count()
}

/// The table type a control keeps: each live dmabuf by its cookie.
pub(crate) type Table = Vec<(u64, Weak<Dmabuf>)>;
