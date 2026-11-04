//! A volume `btrfsd` serves, as the VFS sees it (`docs/microkernel/BTRFSD.md` §1,
//! §3), in place of `fs/btrfs.rs` under `fs=server`: the same module's calls, so
//! that no caller knows. The crates themselves are not in this process.
//!
//! A disk comes up by an event from the server and is registered as any disk is,
//! its `/dev` node and all, with reads through the server. A mount opens a channel
//! of its own. One request at a time on a channel, under a sleeping turn; every
//! wait has a bound past the server's own, so a dead or hung server is `EIO`.
//!
//! The page cache of a served file is here, as it is for any file: the server keeps
//! none. Its source asks the server, so mapped files fault through the same path
//! (`MICROKERNEL.md` §6.3's first step; K3's pager is the next).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ferrix_fsproto::wire::{
    ANSWER_BYTES, Answer, DiskUp, EVENT_BYTES, FIRST_NODE, Op, REQUEST_MAX, Request, SLOT_BYTES,
    STATFS_BYTES, decode_dirent, decode_statfs, encode_set_attr,
};
use ferrix_native::channel::{self, Channel};
use ferrix_native::handle::{Deadline, Object as _, OwnedHandle};
use ferrix_native::pending::Protection;
use ferrix_native::vmo::{self, Vmo};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_rt::Kernel;
use ferrix_vfs::tmpfs::{PAGE_SIZE, PageSource, Pages, Storage as _};
use ferrix_vfs::{
    DirEntry, Errno, FileSystem, FileType, Inode, Metadata, NewNode, SetAttributes, StatFs,
};

use super::block::BlockDevice;
use super::devfs::{self, BlockRegistration, Origin, register_block_from};
use super::pages::VmoStorage;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;

/// Longer than anything the server's own bounds add up to (a request's patience
/// and a parked disk's), so that the answer is the server's when it is alive.
const CALL_PATIENCE_NANOS: u64 = 120_000_000_000;
/// The most bytes of a block request in one slot.
const SLOT: usize = SLOT_BYTES;
/// virtio-blk's major, which every disk of the block ring has.
const DISK_MAJOR: u32 = 254;

/// A channel to the server and its slot, one request at a time.
struct Client {
    channel: Channel<Kernel>,
    /// Where the slot is mapped here.
    slot: usize,
    busy: AtomicBool,
    turn: WaitQueue,
}

// SAFETY: the channel is a handle's number, and the slot is reached only in a turn.
unsafe impl Send for Client {}
// SAFETY: as above.
unsafe impl Sync for Client {}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("served channel")
    }
}

/// A new slot VMO, mapped here, and a handle to give the server.
fn new_slot() -> Result<(usize, OwnedHandle<Kernel>), Errno> {
    let made: Vmo<Kernel> = vmo::create(Kernel, SLOT).map_err(|_| Errno::ENOMEM)?;
    let (_, at) = ferrix_kernel_core::mm::foreign_window((SLOT as u64).div_ceil(PAGE_SIZE))
        .ok_or(Errno::ENOMEM)?;
    let _ = made
        .map(Some(at as usize), SLOT, Protection::ReadWrite, 0)
        .map_err(|_| Errno::ENOMEM)?;
    let given = made.into_owned();
    Ok((at as usize, given))
}

impl Client {
    fn new(channel: Channel<Kernel>, slot: usize) -> Client {
        Client {
            channel,
            slot,
            busy: AtomicBool::new(false),
            turn: WaitQueue::new(),
        }
    }

    fn slot_write(&self, bytes: &[u8]) {
        // SAFETY: `bytes` fits the slot, as every caller checks: it is mapped for
        // the channel's life, and this thread has the turn. A server's concurrent
        // write would only change file data it serves anyway.
        unsafe {
            core::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.slot as *mut u8,
                bytes.len().min(SLOT),
            );
        }
    }

    fn slot_read(&self, out: &mut [u8]) {
        // SAFETY: as in `slot_write`.
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.slot as *const u8,
                out.as_mut_ptr(),
                out.len().min(SLOT),
            );
        }
    }

    /// One request; `give` goes with it, and what `answered` is called with is the
    /// slot's in the same turn. An errno in the answer is the error.
    fn call<R>(
        &self,
        request: &Request<'_>,
        give: Option<[OwnedHandle<Kernel>; 2]>,
        before: impl FnOnce(&Client),
        answered: impl FnOnce(&Client, &Answer) -> Result<R, Errno>,
    ) -> Result<R, Errno> {
        let mut form = [0_u8; REQUEST_MAX];
        let length = request.encode(&mut form).ok_or(Errno::ENAMETOOLONG)?;
        while self
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            let _ = self.turn.wait_until_deadline(
                || !self.busy.load(Ordering::Acquire),
                ferrix_kernel_core::timer::now_nanos().saturating_add(CALL_PATIENCE_NANOS),
            );
        }
        before(self);
        let outcome = self
            .exchange(form.get(..length).unwrap_or(&[]), give)
            .and_then(|answer| {
                if answer.status != 0 {
                    return Err(Errno(answer.status as u16));
                }
                answered(self, &answer)
            });
        self.busy.store(false, Ordering::Release);
        self.turn.wake_all();
        outcome
    }

    fn exchange(
        &self,
        request: &[u8],
        give: Option<[OwnedHandle<Kernel>; 2]>,
    ) -> Result<Answer, Errno> {
        match give {
            Some(handles) => self
                .channel
                .write_with(request, handles)
                .map_err(|_| Errno::EIO)?,
            None => self.channel.write(request).map_err(|_| Errno::EIO)?,
        }
        let until = ferrix_kernel_core::timer::now_nanos().saturating_add(CALL_PATIENCE_NANOS);
        let signals = self
            .channel
            .wait_one(
                Signals::READABLE | Signals::PEER_CLOSED,
                Deadline::At(until),
            )
            .map_err(|_| Errno::EIO)?;
        if !signals.intersects(Signals::READABLE) {
            return Err(Errno::EIO);
        }
        let mut bytes = [0_u8; ANSWER_BYTES];
        let got = self
            .channel
            .read(&mut bytes, &mut [])
            .map_err(|_| Errno::EIO)?;
        Answer::decode(bytes.get(..got.bytes).unwrap_or(&[])).ok_or(Errno::EIO)
    }
}

/// The control channel, once `start` has run.
static CONTROL: SpinLock<Option<Arc<Client>>> = SpinLock::new(None);
/// The events channel's handle.
static EVENTS: AtomicU32 = AtomicU32::new(0);
/// The registered disks, kept for as long as the process lives.
static REGISTERED: SpinLock<Vec<BlockRegistration>> = SpinLock::new(Vec::new());

fn control() -> Result<Arc<Client>, Errno> {
    CONTROL.lock().as_ref().map(Arc::clone).ok_or(Errno::ENXIO)
}

/// Say HELLO on the channel the kernel gave this process.
pub(crate) fn start(files: u32) {
    let channel = Channel::from_owned(OwnedHandle::from_raw(Kernel, Handle(files)));
    let Ok((slot, vmo)) = new_slot() else {
        crate::println!("  btrfsd   no memory for its slot");
        return;
    };
    let client = Client::new(channel, slot);
    let mut form = [0_u8; REQUEST_MAX];
    let wall = super::clock().now();
    let hello = Request {
        a: wall.tv_sec as u64,
        b: wall.tv_nsec as u64,
        ..Request::new(Op::Hello, 0)
    };
    let Some(length) = hello.encode(&mut form) else {
        return;
    };
    let sent = client
        .channel
        .write_with(form.get(..length).unwrap_or(&[]), [vmo]);
    if sent.is_err() {
        crate::println!("  btrfsd   its channel would not take HELLO");
        return;
    }
    let until = ferrix_kernel_core::timer::now_nanos().saturating_add(CALL_PATIENCE_NANOS);
    let mut bytes = [0_u8; ANSWER_BYTES];
    // The answer carries the events channel, a channel's handle: so this read goes by
    // the kernel's topology path, the one a region's thread once could not.
    let mut events = [Handle::INVALID; 1];
    let answered = client
        .channel
        .wait_one(
            Signals::READABLE | Signals::PEER_CLOSED,
            Deadline::At(until),
        )
        .is_ok_and(|signals| signals.intersects(Signals::READABLE))
        && client
            .channel
            .read(&mut bytes, &mut events)
            .is_ok_and(|got| {
                got.handles == 1
                    && Answer::decode(bytes.get(..got.bytes).unwrap_or(&[]))
                        .is_some_and(|a| a.status == 0)
            });
    if !answered {
        crate::println!("  btrfsd   did not answer HELLO");
        return;
    }
    EVENTS.store(events[0].0, Ordering::Release);
    *CONTROL.lock() = Some(Arc::new(client));
    if crate::sched::spawn("served", watch_events, 0, 0).is_err() {
        crate::println!("  btrfsd   no thread for its events");
    }
}

fn events() -> core::mem::ManuallyDrop<Channel<Kernel>> {
    core::mem::ManuallyDrop::new(Channel::from_owned(OwnedHandle::from_raw(
        Kernel,
        Handle(EVENTS.load(Ordering::Acquire)),
    )))
}

fn watch_events(_: usize) {
    loop {
        let signals = events().wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never);
        settle();
        if !signals.is_ok_and(|signals| signals.intersects(Signals::READABLE)) {
            return;
        }
    }
}

/// Every disk the server has said is up, registered. The server tells before it
/// publishes to `devmgr`, so after `devmgr`'s report this has every disk it started.
pub(crate) fn settle() {
    let Ok(client) = control() else {
        return;
    };
    let channel = events();
    let mut bytes = [0_u8; EVENT_BYTES];
    while let Ok(got) = channel.read(&mut bytes, &mut []) {
        if let Some(up) = DiskUp::decode(bytes.get(..got.bytes).unwrap_or(&[])) {
            register(&client, &up);
        }
    }
}

fn register(client: &Arc<Client>, up: &DiskUp) {
    let end = up
        .name
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(up.name.len());
    let name = up.name.get(..end).unwrap_or(&[]);
    let Some(disk_name) = ferrix_blkring::DiskName::new(up.name) else {
        return;
    };
    let device: Arc<dyn BlockDevice> = Arc::new(ServedDisk {
        client: Arc::clone(client),
        name: up.name,
        sectors: up.sectors,
        sector_size: up.sector_size,
        read_only: up.flags & ferrix_blkring::geometry::DeviceFlags::READ_ONLY.0 != 0,
    });
    let minor = disk_name.minor();
    let accounted = super::blkio::account_disk(device, DISK_MAJOR, minor);
    match register_block_from(name, DISK_MAJOR, minor, accounted, Origin::default()) {
        Ok(registration) => REGISTERED.lock().push(registration),
        // A disk the server has told of again: its driver came back.
        Err(_) => {}
    }
}

/// A disk through the server.
struct ServedDisk {
    client: Arc<Client>,
    name: [u8; 8],
    sectors: u64,
    sector_size: u32,
    read_only: bool,
}

impl fmt::Debug for ServedDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServedDisk")
            .field("sectors", &self.sectors)
            .finish_non_exhaustive()
    }
}

impl ServedDisk {
    fn name(&self) -> &[u8] {
        let end = self
            .name
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(self.name.len());
        self.name.get(..end).unwrap_or(&[])
    }

    fn whole_sectors_of_slot(&self) -> usize {
        let size = self.sector_size.max(1) as usize;
        (SLOT / size).max(1) * size
    }
}

impl BlockDevice for ServedDisk {
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), Errno> {
        let size = u64::from(self.sector_size);
        let mut at = sector;
        for piece in buf.chunks_mut(self.whole_sectors_of_slot()) {
            let count = piece.len() as u64 / size.max(1);
            let request = Request {
                a: at,
                b: count,
                name: self.name(),
                ..Request::new(Op::BlockRead, 0)
            };
            self.client.call(
                &request,
                None,
                |_| {},
                |client, _| {
                    client.slot_read(piece);
                    Ok(())
                },
            )?;
            at = at.saturating_add(count);
        }
        Ok(())
    }

    fn write(&self, sector: u64, buf: &[u8]) -> Result<(), Errno> {
        let size = u64::from(self.sector_size);
        let mut at = sector;
        for piece in buf.chunks(self.whole_sectors_of_slot()) {
            let count = piece.len() as u64 / size.max(1);
            let request = Request {
                a: at,
                b: count,
                name: self.name(),
                ..Request::new(Op::BlockWrite, 0)
            };
            self.client.call(
                &request,
                None,
                |client| client.slot_write(piece),
                |_, _| Ok(()),
            )?;
            at = at.saturating_add(count);
        }
        Ok(())
    }

    fn flush(&self) -> Result<(), Errno> {
        let request = Request {
            name: self.name(),
            ..Request::new(Op::BlockFlush, 0)
        };
        self.client.call(&request, None, |_| {}, |_, _| Ok(()))
    }

    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn read_only(&self) -> bool {
        self.read_only
    }
}

/// The disk `rdev` names, as the server knows it.
fn disk_name(rdev: u64) -> Option<[u8; 8]> {
    let mut found = None;
    devfs::for_each_block(|name, major, minor, _| {
        if ferrix_vfs::initramfs::makedev(major, minor) == rdev {
            let mut stored = [0_u8; 8];
            for (slot, byte) in stored.iter_mut().zip(name) {
                *slot = *byte;
            }
            found = Some(stored);
        }
    });
    found
}

fn trimmed(name: &[u8; 8]) -> &[u8] {
    let end = name
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(name.len());
    name.get(..end).unwrap_or(&[])
}

/// Mount the volume on the disk `rdev` names, read-only.
///
/// # Errors
///
/// `ENXIO` with no such disk, and what the server answers.
pub(crate) fn mount(rdev: u64) -> Result<Arc<dyn FileSystem>, Errno> {
    open(rdev, false)
}

/// Mount it for writing.
///
/// # Errors
///
/// `EROFS` for a disk that takes no writes, and as [`mount`].
pub(crate) fn mount_rw(rdev: u64) -> Result<Arc<dyn FileSystem>, Errno> {
    if devfs::block_device(rdev).ok_or(Errno::ENXIO)?.read_only() {
        return Err(Errno::EROFS);
    }
    open(rdev, true)
}

fn open(rdev: u64, writable: bool) -> Result<Arc<dyn FileSystem>, Errno> {
    let until = ferrix_kernel_core::timer::now_nanos().saturating_add(CALL_PATIENCE_NANOS);
    if !CLOSED.wait_until_deadline(|| CLOSING.load(Ordering::Acquire) == 0, until) {
        return Err(Errno::EBUSY);
    }
    let name = disk_name(rdev).ok_or(Errno::ENXIO)?;
    let (slot, vmo) = new_slot()?;
    let request = Request {
        a: u64::from(writable),
        b: rdev,
        name: trimmed(&name),
        ..Request::new(Op::Open, 0)
    };
    // This side makes the channel, and gives the server its end: a read that takes
    // a channel's handle is one more thing the arena's stacks would have to be
    // ready for, and nothing is gained by the server making it.
    let (near, far) = channel::create(Kernel).map_err(|_| Errno::ENOMEM)?;
    let answer = control()?.call(
        &request,
        Some([vmo, far.into_owned()]),
        |_| {},
        |_, answer| Ok(*answer),
    )?;
    let attr = answer.attr.ok_or(Errno::EIO)?;
    let volume = Arc::new(ServedVolume {
        client: Arc::new(Client::new(near, slot)),
        rdev,
        writable,
        root: SpinLock::new(None),
        nodes: SpinLock::new(BTreeMap::new()),
    });
    let root = volume.node_for(answer.node, attr);
    *volume.root.lock() = Some(root);
    Ok(volume)
}

/// The label on the disk `rdev` names; `None` for no volume, or none.
pub(crate) fn label(rdev: u64) -> Option<Vec<u8>> {
    let name = disk_name(rdev)?;
    let request = Request {
        name: trimmed(&name),
        ..Request::new(Op::Label, 0)
    };
    control()
        .ok()?
        .call(
            &request,
            None,
            |_| {},
            |client, answer| {
                let mut label = vec![0_u8; (answer.value as usize).min(256)];
                client.slot_read(&mut label);
                Ok(label)
            },
        )
        .ok()
        .filter(|label| !label.is_empty())
}

/// The server's own memory is its own: nothing here to give back.
pub(crate) fn drop_clean(_want: u64) -> u64 {
    0
}

/// As [`drop_clean`].
pub(crate) fn clean_bytes() -> u64 {
    0
}

/// One mount.
struct ServedVolume {
    client: Arc<Client>,
    rdev: u64,
    writable: bool,
    root: SpinLock<Option<Arc<ServedNode>>>,
    /// Each inode's one object here, so that what one name's change does shows
    /// through every other. Weak: the dentry cache decides how long one lives.
    nodes: SpinLock<BTreeMap<u64, Weak<ServedNode>>>,
}

/// Mounts being closed, waited out by the next open: the server is to have let go.
static CLOSING: AtomicU32 = AtomicU32::new(0);
static CLOSED: WaitQueue = WaitQueue::new();

impl Drop for ServedVolume {
    /// An unmount: said on a thread that may wait, since a drop can run under locks.
    fn drop(&mut self) {
        let _ = CLOSING.fetch_add(1, Ordering::AcqRel);
        let told = Box::into_raw(Box::new(Arc::clone(&self.client))) as usize;
        let spawned = crate::sched::spawn(
            "unmount",
            |packed| {
                // SAFETY: `packed` is the box this `drop` leaked, its one reader.
                let client = unsafe { Box::from_raw(packed as *mut Arc<Client>) };
                let _ = client.call(&Request::new(Op::Close, 0), None, |_| {}, |_, _| Ok(()));
                let _ = CLOSING.fetch_sub(1, Ordering::AcqRel);
                CLOSED.wake_all();
            },
            told,
            0,
        );
        if spawned.is_err() {
            let _ = CLOSING.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl fmt::Debug for ServedVolume {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServedVolume")
            .field("rdev", &self.rdev)
            .finish_non_exhaustive()
    }
}

impl ServedVolume {
    /// The object for the inode `attr` says, under the number the server just gave:
    /// a second number for an inode this side has goes back by [`Numbered`]'s drop.
    fn node_for(self: &Arc<Self>, node: u32, attr: Metadata) -> Arc<ServedNode> {
        let number = Arc::new(Numbered {
            client: Arc::clone(&self.client),
            node,
        });
        let mut nodes = self.nodes.lock();
        if let Some(known) = nodes.get(&attr.ino).and_then(Weak::upgrade) {
            *known.attr.lock() = attr;
            drop(nodes);
            drop(number);
            return known;
        }
        let made = Arc::new(ServedNode {
            number,
            volume: Arc::downgrade(self),
            client: Arc::clone(&self.client),
            attr: SpinLock::new(attr),
            pages: SpinLock::new(None),
        });
        nodes.retain(|_, node| node.strong_count() > 0);
        let _ = nodes.insert(attr.ino, Arc::downgrade(&made));
        made
    }

    /// Say what an answer says of an inode, if this side holds it.
    fn note(&self, attr: Option<Metadata>) {
        let Some(attr) = attr else {
            return;
        };
        let known = self.nodes.lock().get(&attr.ino).and_then(Weak::upgrade);
        if let Some(known) = known {
            *known.attr.lock() = attr;
        }
    }
}

impl FileSystem for ServedVolume {
    fn root(&self) -> Arc<dyn Inode> {
        #[expect(clippy::expect_used, reason = "AUDIT: mount sets it before answering")]
        let root = self
            .root
            .lock()
            .as_ref()
            .map(Arc::clone)
            .expect("set at mount");
        root
    }

    fn name(&self) -> &'static str {
        "btrfs"
    }

    fn device(&self) -> u64 {
        self.rdev
    }

    fn read_only(&self) -> bool {
        !self.writable
    }

    /// What mappings wrote, and then the volume's commit.
    fn sync(&self) -> Result<(), Errno> {
        let held: Vec<Arc<ServedNode>> = self
            .nodes
            .lock()
            .values()
            .filter_map(Weak::upgrade)
            .collect();
        for node in held {
            node.push_mapped()?;
        }
        self.client
            .call(&Request::new(Op::Sync, 0), None, |_| {}, |_, _| Ok(()))
    }

    fn statfs(&self) -> StatFs {
        let asked = self.client.call(
            &Request::new(Op::StatFs, 0),
            None,
            |_| {},
            |client, _| {
                let mut form = [0_u8; STATFS_BYTES];
                client.slot_read(&mut form);
                Ok(decode_statfs(&form))
            },
        );
        asked.unwrap_or(StatFs {
            magic: 0x9123_683e,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            files: 0,
            files_free: 0,
            name_max: 255,
        })
    }
}

/// A node's number on the server, forgotten when nothing here holds it. A mapping's
/// pages can outlive the inode, so its source holds this too.
struct Numbered {
    client: Arc<Client>,
    node: u32,
}

impl Drop for Numbered {
    /// On a thread that may wait: a drop can run under locks.
    fn drop(&mut self) {
        if self.node != FIRST_NODE {
            let _ = crate::sched::spawn(
                "forget",
                |packed| {
                    // SAFETY: `packed` is the box this `drop` leaked, and this is
                    // its one reader.
                    let told = unsafe { Box::from_raw(packed as *mut (Arc<Client>, u32)) };
                    let _ = told.0.call(
                        &Request::new(Op::Forget, told.1),
                        None,
                        |_| {},
                        |_, _| Ok(()),
                    );
                },
                Box::into_raw(Box::new((Arc::clone(&self.client), self.node))) as usize,
                0,
            );
        }
    }
}

/// A file, directory or link of a served volume.
struct ServedNode {
    number: Arc<Numbered>,
    volume: Weak<ServedVolume>,
    client: Arc<Client>,
    attr: SpinLock<Metadata>,
    pages: SpinLock<Option<Arc<dyn Pages>>>,
}

impl fmt::Debug for ServedNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServedNode")
            .field("node", &self.number.node)
            .finish_non_exhaustive()
    }
}

/// Where a served file's pages come from.
struct Source {
    number: Arc<Numbered>,
    /// Whether what a mapping wrote could be lost by dropping a page: until K3's
    /// pager, a writable mount's pages stay until the file goes.
    droppable: bool,
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Source")
            .field("node", &self.number.node)
            .finish_non_exhaustive()
    }
}

impl PageSource for Source {
    fn reads_disk(&self) -> bool {
        true
    }

    fn reclaimable(&self) -> bool {
        self.droppable
    }

    fn fill_range(&self, first: u64, pages: &mut [&mut [u8]]) -> Result<usize, Errno> {
        if pages.is_empty() {
            return Err(Errno::EINVAL);
        }
        let offset = first.checked_mul(PAGE_SIZE).ok_or(Errno::EIO)?;
        let page_bytes = PAGE_SIZE as usize;
        let count = pages.len().min(SLOT / page_bytes);
        let request = Request {
            a: offset,
            b: (count * page_bytes) as u64,
            ..Request::new(Op::Read, self.number.node)
        };
        self.number.client.call(
            &request,
            None,
            |_| {},
            |client, answer| {
                // What is past the file's end reads as zeros, as a page cache's does.
                let got = (answer.value as usize).min(count * page_bytes);
                let mut run = vec![0_u8; count * page_bytes];
                if let Some(filled) = run.get_mut(..got) {
                    client.slot_read(filled);
                }
                for (page, bytes) in pages.iter_mut().zip(run.chunks(page_bytes)) {
                    page.copy_from_slice(bytes);
                }
                Ok(count)
            },
        )
    }
}

impl ServedNode {
    fn kind(&self) -> FileType {
        self.attr.lock().kind
    }

    fn writable(&self) -> bool {
        self.volume.upgrade().is_some_and(|volume| volume.writable)
    }

    fn pages(&self) -> Result<Arc<dyn Pages>, Errno> {
        if let Some(pages) = self.pages.lock().as_ref() {
            return Ok(Arc::clone(pages));
        }
        let source: Arc<dyn PageSource> = Arc::new(Source {
            number: Arc::clone(&self.number),
            droppable: !self.writable(),
        });
        let made: Arc<dyn Pages> = Arc::from(VmoStorage.allocate_with(source)?);
        made.resize(self.attr.lock().size);
        let mut slot = self.pages.lock();
        let pages = Arc::clone(slot.get_or_insert_with(|| Arc::clone(&made)));
        drop(slot);
        drop(made);
        Ok(pages)
    }

    fn cached_pages(&self) -> Option<Arc<dyn Pages>> {
        self.pages.lock().as_ref().map(Arc::clone)
    }

    fn require_dir(&self) -> Result<(), Errno> {
        if self.kind() == FileType::Directory {
            Ok(())
        } else {
            Err(Errno::ENOTDIR)
        }
    }

    /// Ask for this node's attributes again, after a change that names it.
    fn refresh(&self) {
        let asked = self.client.call(
            &Request::new(Op::GetAttr, self.number.node),
            None,
            |_| {},
            |_, answer| Ok(answer.attr),
        );
        if let Ok(Some(attr)) = asked {
            *self.attr.lock() = attr;
        }
    }

    /// One page of [`Self::push_mapped`]'s, past the file's end nothing.
    fn push_one_page(&self, pages: &dyn Pages, index: u64, page: &mut [u8]) -> Result<(), Errno> {
        let offset = index.checked_mul(PAGE_SIZE).ok_or(Errno::EIO)?;
        pages.read(offset, page)?;
        let size = self.attr.lock().size;
        let length = size.saturating_sub(offset).min(PAGE_SIZE) as usize;
        let bytes = page.get(..length).unwrap_or(&[]);
        if bytes.is_empty() {
            return Ok(());
        }
        let request = Request {
            a: offset,
            b: length as u64,
            ..Request::new(Op::Write, self.number.node)
        };
        self.client.call(
            &request,
            None,
            |client| client.slot_write(bytes),
            |_, _| Ok(()),
        )?;
        Ok(())
    }

    /// What a shared mapping wrote, which marked nothing here, sent to the server
    /// (`BTRFSD.md` §3: coherent at `msync` and `fsync` until K3's pager).
    fn push_mapped(&self) -> Result<(), Errno> {
        let Some(pages) = self.cached_pages() else {
            return Ok(());
        };
        let (written, _mapped) = pages.mapped_writes()?;
        let page_bytes = PAGE_SIZE as usize;
        let mut page = vec![0_u8; page_bytes];
        let mut failed = None;
        let mut result = Ok(());
        for index in written {
            let sent = self.push_one_page(&*pages, index, &mut page);
            if sent.is_err() {
                // K3b's dirty set: what was not sent stays dirty.
                failed = Some(index);
                result = sent;
                break;
            }
        }
        pages.mapped_writes_done(failed);
        result
    }

    /// `other`, which must be of this volume.
    fn number_of(&self, other: &Arc<dyn Inode>) -> Result<(Arc<ServedNode>, u64), Errno> {
        let other = Arc::clone(other)
            .into_any()
            .downcast::<ServedNode>()
            .map_err(|_| Errno::EXDEV)?;
        if !Arc::ptr_eq(&other.client, &self.client) {
            return Err(Errno::EXDEV);
        }
        let number = u64::from(other.number.node);
        Ok((other, number))
    }

    fn after_change(&self, attr: Option<Metadata>) {
        self.refresh();
        if let Some(volume) = self.volume.upgrade() {
            volume.note(attr);
        }
    }
}

impl Inode for ServedNode {
    fn holds_cached_pages(&self) -> bool {
        self.cached_pages()
            .is_some_and(|pages| pages.committed_bytes() > 0)
    }

    fn metadata(&self) -> Metadata {
        *self.attr.lock()
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn set_attributes(&self, change: &SetAttributes) -> Result<(), Errno> {
        let form = encode_set_attr(change);
        let answer = self.client.call(
            &Request::new(Op::SetAttr, self.number.node),
            None,
            |client| client.slot_write(&form),
            |_, answer| Ok(answer.attr),
        )?;
        if let Some(attr) = answer {
            *self.attr.lock() = attr;
        }
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        if self.kind() != FileType::Regular {
            return Err(Errno::EINVAL);
        }
        let size = self.attr.lock().size;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let len = usize::try_from(size - offset).map_or(buf.len(), |rest| rest.min(buf.len()));
        self.pages()?
            .read(offset, buf.get_mut(..len).unwrap_or_default())?;
        Ok(len)
    }

    /// Through to the server a piece at a time, and into the pages held here, so a
    /// read after it reads it.
    fn write_at(&self, offset: u64, data: &[u8], append: bool) -> Result<(usize, u64), Errno> {
        if self.kind() != FileType::Regular {
            return Err(Errno::EINVAL);
        }
        let mut done = 0_usize;
        let mut end = offset;
        for piece in data.chunks(SLOT) {
            let request = Request {
                a: end,
                b: piece.len() as u64,
                c: u32::from(append),
                ..Request::new(Op::Write, self.number.node)
            };
            let answer = self.client.call(
                &request,
                None,
                |client| client.slot_write(piece),
                |_, answer| Ok(*answer),
            );
            let answer = match answer {
                Ok(answer) => answer,
                Err(_) if done > 0 => break,
                Err(errno) => return Err(errno),
            };
            let written = answer.value as usize;
            if let Some(attr) = answer.attr {
                *self.attr.lock() = attr;
            }
            if let Some(pages) = self.cached_pages() {
                pages.resize(self.attr.lock().size);
                let start = answer.end.saturating_sub(written as u64);
                pages.write(start, piece.get(..written).unwrap_or(piece))?;
            }
            done += written;
            end = answer.end;
            if written < piece.len() {
                break;
            }
        }
        Ok((done, end))
    }

    fn set_len(&self, len: u64) -> Result<(), Errno> {
        let request = Request {
            a: len,
            ..Request::new(Op::SetLen, self.number.node)
        };
        let answer = self
            .client
            .call(&request, None, |_| {}, |_, answer| Ok(answer.attr))?;
        if let Some(attr) = answer {
            *self.attr.lock() = attr;
        }
        if let Some(pages) = self.cached_pages() {
            pages.resize(len);
            pages.discard_from(len);
        }
        Ok(())
    }

    fn grow_to(&self, len: u64) -> Result<(), Errno> {
        let request = Request {
            a: len,
            ..Request::new(Op::GrowTo, self.number.node)
        };
        let answer = self
            .client
            .call(&request, None, |_| {}, |_, answer| Ok(answer.attr))?;
        if let Some(attr) = answer {
            *self.attr.lock() = attr;
        }
        if let Some(pages) = self.cached_pages() {
            pages.resize(self.attr.lock().size);
        }
        Ok(())
    }

    fn fsync(&self, data_only: bool) -> Result<(), Errno> {
        self.push_mapped()?;
        let request = Request {
            c: u32::from(data_only),
            ..Request::new(Op::Fsync, self.number.node)
        };
        self.client.call(&request, None, |_| {}, |_, _| Ok(()))
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>, Errno> {
        self.require_dir()?;
        let request = Request {
            name,
            ..Request::new(Op::Lookup, self.number.node)
        };
        let answer = self
            .client
            .call(&request, None, |_| {}, |_, answer| Ok(*answer))?;
        let attr = answer.attr.ok_or(Errno::EIO)?;
        let volume = self.volume.upgrade().ok_or(Errno::EIO)?;
        Ok(volume.node_for(answer.node, attr))
    }

    fn create(
        &self,
        name: &[u8],
        node: NewNode<'_>,
        permissions: u32,
    ) -> Result<Arc<dyn Inode>, Errno> {
        self.require_dir()?;
        let (target, rdev): (&[u8], u64) = match node {
            NewNode::Symlink(target) => (target, 0),
            NewNode::Device { rdev, .. } => (&[], rdev),
            _ => (&[], 0),
        };
        if target.len() > SLOT {
            return Err(Errno::ENAMETOOLONG);
        }
        let request = Request {
            a: u64::from(permissions),
            b: rdev,
            c: node.kind().mode_bits(),
            d: target.len() as u32,
            name,
            ..Request::new(Op::Create, self.number.node)
        };
        let answer = self.client.call(
            &request,
            None,
            |client| client.slot_write(target),
            |_, answer| Ok(*answer),
        )?;
        let attr = answer.attr.ok_or(Errno::EIO)?;
        self.refresh();
        let volume = self.volume.upgrade().ok_or(Errno::EIO)?;
        Ok(volume.node_for(answer.node, attr))
    }

    fn link(&self, name: &[u8], target: &Arc<dyn Inode>) -> Result<(), Errno> {
        let (_held, other) = self.number_of(target)?;
        let request = Request {
            a: other,
            name,
            ..Request::new(Op::Link, self.number.node)
        };
        let attr = self
            .client
            .call(&request, None, |_| {}, |_, answer| Ok(answer.attr))?;
        self.after_change(attr);
        Ok(())
    }

    fn unlink(&self, name: &[u8]) -> Result<(), Errno> {
        let request = Request {
            name,
            ..Request::new(Op::Unlink, self.number.node)
        };
        let attr = self
            .client
            .call(&request, None, |_| {}, |_, answer| Ok(answer.attr))?;
        self.after_change(attr);
        Ok(())
    }

    fn rmdir(&self, name: &[u8]) -> Result<(), Errno> {
        let request = Request {
            name,
            ..Request::new(Op::Rmdir, self.number.node)
        };
        self.client.call(&request, None, |_| {}, |_, _| Ok(()))?;
        self.refresh();
        Ok(())
    }

    fn rename(
        &self,
        old: &[u8],
        new_parent: &Arc<dyn Inode>,
        new: &[u8],
        replace: bool,
    ) -> Result<(), Errno> {
        let (held, other) = self.number_of(new_parent)?;
        let request = Request {
            a: other,
            c: u32::from(replace),
            name: old,
            second: new,
            ..Request::new(Op::Rename, self.number.node)
        };
        let attr = self
            .client
            .call(&request, None, |_| {}, |_, answer| Ok(answer.attr))?;
        self.after_change(attr);
        held.refresh();
        Ok(())
    }

    fn read_dir(
        &self,
        cursor: u64,
        emit: &mut dyn FnMut(DirEntry<'_>) -> bool,
    ) -> Result<(), Errno> {
        self.require_dir()?;
        let mut from = cursor;
        loop {
            let request = Request {
                a: from,
                ..Request::new(Op::ReadDir, self.number.node)
            };
            let bytes = self.client.call(
                &request,
                None,
                |_| {},
                |client, answer| {
                    let mut listed = vec![0_u8; (answer.value as usize).min(SLOT)];
                    client.slot_read(&mut listed);
                    Ok(listed)
                },
            )?;
            if bytes.is_empty() {
                return Ok(());
            }
            let mut rest = bytes.as_slice();
            while !rest.is_empty() {
                let (ino, next, kind, name, took) = decode_dirent(rest).ok_or(Errno::EIO)?;
                if !emit(DirEntry {
                    ino,
                    kind,
                    name,
                    next,
                }) {
                    return Ok(());
                }
                from = next;
                rest = rest.get(took..).unwrap_or(&[]);
            }
        }
    }

    fn read_link(&self) -> Result<Vec<u8>, Errno> {
        let request = Request::new(Op::ReadLink, self.number.node);
        self.client.call(
            &request,
            None,
            |_| {},
            |client, answer| {
                let mut target = vec![0_u8; (answer.value as usize).min(SLOT)];
                client.slot_read(&mut target);
                Ok(target)
            },
        )
    }

    fn mapping(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        if self.kind() != FileType::Regular {
            return None;
        }
        self.pages().ok().and_then(|pages| pages.object())
    }
}
