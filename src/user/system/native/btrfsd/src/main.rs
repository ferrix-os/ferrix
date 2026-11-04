//! btrfsd, the filesystem server (`docs/MICROKERNEL.md` §6.3,
//! `docs/microkernel/BTRFSD.md`): `ferrix-btrfs` and `ferrix-btrfs-vfs`, byte for
//! byte, in a process of their own, over the block ring in user space. The partition
//! manager starts it with a job of its own, measured.
//!
//! One instance serves one partition's disks to one client: BD.4's choice, so a
//! client cannot starve another's and no state of the crates is shared across
//! partitions. One thread: a request is answered before the next is read, and every
//! wait inside one has a bound.

#![no_std]
#![no_main]

extern crate alloc;

mod disks;
mod heap;
mod pass;

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_blkclient::{Mapped, Shared, held_mapping};
use ferrix_btrfs_vfs::Btrfs;
use ferrix_btrfs_vfs::rw::RwBtrfs;
use ferrix_fsproto::server::{Slot, Volume};
use ferrix_fsproto::wire::{ANSWER_BYTES, Answer, Op, REQUEST_MAX, Request, SLOT_BYTES};
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_rt::native::channel::{self, Channel, ReadError};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object as _, OwnedHandle};
use ferrix_rt::native::job::Job;
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::space::Space;
use ferrix_rt::native::vmo::Vmo;
use ferrix_rt::{Bootstrap, Kernel};
use ferrix_sync::SpinParker;
use ferrix_vfs::tmpfs::HeapStorage;
use ferrix_vfs::{Clock, Timespec};

use crate::disks::{disks, install, now};

ferrix_rt::entry!(main);

/// The start's message: the handles after the debug log.
const START_HANDLES: usize = 5;
/// Port keys.
const KEY_CORES: u64 = 1;
const KEY_CONTROL: u64 = 2;
const KEY_VOLUME: u64 = 0x1000;
/// The most volumes one client holds open.
const MOST_VOLUMES: usize = 32;
/// How often the loop looks at drivers on their way, and at ended ones.
const LOOK_NANOS: u64 = 100_000_000;

/// A channel's slot: bytes either side can write at any moment, copied.
struct SharedSlot {
    _mapped: Mapped<Kernel>,
    shared: Shared,
}

impl Slot for SharedSlot {
    fn read(&self, offset: usize, out: &mut [u8]) -> bool {
        self.shared.copy_out(offset, out)
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) -> bool {
        self.shared.copy_in(offset, bytes)
    }
}

/// The slot from a client's VMO, of exactly the size the protocol says.
fn slot_of(vmo: Vmo<Kernel>) -> Result<SharedSlot, Errno> {
    if vmo.size().map_err(|_| Errno::EINVAL)? != SLOT_BYTES as u64 {
        return Err(Errno::EINVAL);
    }
    let (mapped, shared) = held_mapping(vmo).ok_or(Errno::EINVAL)?;
    Ok(SharedSlot {
        _mapped: mapped,
        shared,
    })
}

struct Open {
    disk: usize,
    channel: Channel<Kernel>,
    volume: Volume,
    slot: SharedSlot,
}

struct Client {
    control: Channel<Kernel>,
    slot: Option<SharedSlot>,
    open: Vec<Option<Open>>,
    port: Port<Kernel>,
    /// A scratch of the slot's size, so that a block op reads it once.
    scratch: Vec<u8>,
    clock: Arc<WallClock>,
}

/// What a writable mount's times come from: the wall clock the client gave at
/// HELLO, and the monotonic clock since.
#[derive(Debug)]
struct WallClock {
    at_hello_sec: i64,
    at_hello_nsec: i64,
    mono_at_hello: u64,
}

impl Clock for WallClock {
    fn now(&self) -> Timespec {
        let passed = now().saturating_sub(self.mono_at_hello);
        let nanos = self
            .at_hello_nsec
            .saturating_add((passed % 1_000_000_000) as i64);
        Timespec {
            tv_sec: self
                .at_hello_sec
                .saturating_add((passed / 1_000_000_000) as i64)
                .saturating_add(nanos / 1_000_000_000),
            tv_nsec: nanos % 1_000_000_000,
        }
    }
}

/// A file's most bytes on a writable mount, as the kernel's store says.
const MAX_FILE_SIZE: u64 = 1 << 46;

fn say_btrfs(why: core::fmt::Arguments<'_>) {
    ferrix_rt::notice!("btrfs    {why}");
}

/// The volume on the disk at `index`, mounted once whoever asks again, and as
/// asked: one disk is never two volumes in one process, a writer and a reader.
fn volume_on(
    index: usize,
    dev_no: u64,
    writable: bool,
    clock: &Arc<WallClock>,
) -> Result<Arc<dyn ferrix_vfs::FileSystem>, Errno> {
    let entry = disks().entries.get(index).ok_or(Errno::ENXIO)?;
    if let Some(mounted) = entry.filesystem.as_ref() {
        if entry.writable != writable {
            return Err(Errno::EBUSY);
        }
        return Ok(Arc::clone(mounted));
    }
    let device = disks::Volume {
        index,
        sector: u64::from(entry.sector_size),
    };
    let mounted: Arc<dyn ferrix_vfs::FileSystem> = if writable {
        RwBtrfs::mount(
            device,
            dev_no,
            Arc::new(HeapStorage::new(MAX_FILE_SIZE)),
            Arc::clone(clock) as Arc<dyn Clock>,
            &SpinParker,
            say_btrfs,
        )?
    } else {
        Btrfs::mount(device, dev_no, Arc::new(pass::PassStorage))?
    };
    if let Some(entry) = disks().entries.get_mut(index) {
        entry.filesystem = Some(Arc::clone(&mounted));
        entry.writable = writable;
    }
    Ok(mounted)
}

impl Client {
    /// A request on the control channel: its answer, and a handle to go with it.
    fn control_request(
        &mut self,
        bytes: &[u8],
        handles: &[Handle],
    ) -> (Answer, Option<OwnedHandle<Kernel>>) {
        let owned: Vec<OwnedHandle<Kernel>> = handles
            .iter()
            .map(|&handle| OwnedHandle::from_raw(Kernel, handle))
            .collect();
        let Ok(request) = Request::decode(bytes) else {
            return (Answer::refused(Errno::EINVAL), None);
        };
        if request.op == Op::Hello {
            return match self.hello(&request, owned) {
                Ok(events) => (Answer::OK, Some(events)),
                Err(errno) => (Answer::refused(errno), None),
            };
        }
        match self.answer_control(&request, owned) {
            Ok(done) => (done, None),
            Err(errno) => {
                ferrix_rt::notice!("btrfsd   {:?} refused: errno {}", request.op, errno.0);
                (Answer::refused(errno), None)
            }
        }
    }

    /// The first request: its slot, the wall clock, and the far end of the events
    /// channel in the answer.
    fn hello(
        &mut self,
        request: &Request<'_>,
        mut handles: Vec<OwnedHandle<Kernel>>,
    ) -> Result<OwnedHandle<Kernel>, Errno> {
        if self.slot.is_some() || handles.len() != 1 {
            return Err(Errno::EINVAL);
        }
        let vmo = handles.pop().ok_or(Errno::EINVAL)?;
        self.slot = Some(slot_of(Vmo::from_owned(vmo))?);
        self.clock = Arc::new(WallClock {
            at_hello_sec: request.a as i64,
            at_hello_nsec: (request.b % 1_000_000_000) as i64,
            mono_at_hello: now(),
        });
        let (near, far) = channel::create(Kernel).map_err(|_| Errno::ENOMEM)?;
        disks().events = Some(near);
        disks().tell();
        Ok(far.into_owned())
    }

    fn answer_control(
        &mut self,
        request: &Request<'_>,
        mut handles: Vec<OwnedHandle<Kernel>>,
    ) -> Result<Answer, Errno> {
        let index = disks().named(request.name).ok_or(Errno::ENXIO)?;
        match request.op {
            Op::Open => {
                if handles.len() != 2 {
                    return Err(Errno::EINVAL);
                }
                let free = self
                    .open
                    .iter()
                    .position(Option::is_none)
                    .or_else(|| (self.open.len() < MOST_VOLUMES).then_some(self.open.len()))
                    .ok_or(Errno::EMFILE)?;
                let near: Channel<Kernel> =
                    Channel::from_owned(handles.pop().ok_or(Errno::EINVAL)?);
                let vmo = handles.pop().ok_or(Errno::EINVAL)?;
                let slot = slot_of(Vmo::from_owned(vmo))?;
                let volume = Volume::new(volume_on(index, request.b, request.a == 1, &self.clock)?);
                let answer = volume.root_answer();
                if free == self.open.len() {
                    self.open.push(None);
                }
                if near
                    .wait_async(
                        &self.port,
                        Signals::READABLE | Signals::PEER_CLOSED,
                        KEY_VOLUME + free as u64,
                    )
                    .is_err()
                {
                    return Err(Errno::ENOMEM);
                }
                if let Some(place) = self.open.get_mut(free) {
                    *place = Some(Open {
                        disk: index,
                        channel: near,
                        volume,
                        slot,
                    });
                }
                Ok(answer)
            }
            Op::Label => {
                // The superblock's, as `fs::btrfs::label` reads it.
                const SECTOR_OF_SUPERBLOCK: u64 = 0x1_0000 / 512;
                const MAGIC: core::ops::Range<usize> = 0x40..0x48;
                const LABEL: core::ops::Range<usize> = 0x12b..0x22b;
                let mut block = vec![0_u8; 4096];
                disks().read(index, SECTOR_OF_SUPERBLOCK, &mut block)?;
                if block.get(MAGIC) != Some(&b"_BHRfS_M"[..]) {
                    return Ok(Answer::value(0));
                }
                let field = block.get(LABEL).ok_or(Errno::EIO)?;
                let len = field
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(field.len());
                let name = field.get(..len).ok_or(Errno::EIO)?;
                let slot = self.slot.as_mut().ok_or(Errno::EINVAL)?;
                if !slot.write(0, name) {
                    return Err(Errno::EIO);
                }
                Ok(Answer::value(len as u64))
            }
            Op::BlockRead | Op::BlockWrite => {
                let size = u64::from(disks().entries.get(index).ok_or(Errno::ENXIO)?.sector_size);
                let bytes = request
                    .b
                    .checked_mul(size)
                    .and_then(|bytes| usize::try_from(bytes).ok())
                    .filter(|&bytes| bytes <= SLOT_BYTES)
                    .ok_or(Errno::EINVAL)?;
                let room = self.scratch.get_mut(..bytes).ok_or(Errno::EINVAL)?;
                let slot = self.slot.as_mut().ok_or(Errno::EINVAL)?;
                if request.op == Op::BlockRead {
                    disks().read(index, request.a, room)?;
                    if !slot.write(0, room) {
                        return Err(Errno::EIO);
                    }
                } else {
                    if !slot.read(0, room) {
                        return Err(Errno::EIO);
                    }
                    disks().write(index, request.a, room)?;
                }
                Ok(Answer::OK)
            }
            Op::BlockFlush => {
                disks().flush(index)?;
                Ok(Answer::OK)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    /// Every request the control channel holds. False when its other side has gone.
    fn serve_control(&mut self) -> bool {
        let mut bytes = [0_u8; REQUEST_MAX];
        let mut handles = [Handle::INVALID; 2];
        loop {
            let got = match self.control.read(&mut bytes, &mut handles) {
                Ok(got) => got,
                Err(ReadError::Failed(Error::ShouldWait)) => return true,
                Err(_) => return false,
            };
            let given = handles.get(..got.handles).unwrap_or(&[]);
            let (answer, handle) =
                self.control_request(bytes.get(..got.bytes).unwrap_or(&[]), given);
            let reply: [u8; ANSWER_BYTES] = answer.encode();
            let sent = match handle {
                Some(handle) => self.control.write_with(&reply, [handle]).is_ok(),
                None => self.control.write(&reply).is_ok(),
            };
            if !sent {
                return false;
            }
        }
    }

    /// Every request volume `index`'s channel holds; it goes when its peer does.
    fn serve_volume(&mut self, index: usize) {
        let mut bytes = [0_u8; REQUEST_MAX];
        let mut handles = [Handle::INVALID; 1];
        let Some(place) = self.open.get_mut(index) else {
            return;
        };
        let Some(open) = place.as_mut() else {
            return;
        };
        let keep = loop {
            let got = match open.channel.read(&mut bytes, &mut handles) {
                Ok(got) => got,
                Err(ReadError::Failed(Error::ShouldWait)) => break true,
                Err(_) => break false,
            };
            for handle in handles.iter().take(got.handles) {
                // No op here carries one: closed, not kept.
                drop(OwnedHandle::from_raw(Kernel, *handle));
            }
            // Asked before it is answered, so that the client's next open of the disk
            // finds the mount gone.
            if Request::decode(bytes.get(..got.bytes).unwrap_or(&[]))
                .is_ok_and(|request| request.op == Op::Close)
            {
                let _ = open.channel.write(&Answer::OK.encode());
                break false;
            }
            let answer = open
                .volume
                .serve(bytes.get(..got.bytes).unwrap_or(&[]), &mut open.slot);
            if open.channel.write(&answer.encode()).is_err() {
                break false;
            }
        };
        let armed = keep
            && open
                .channel
                .wait_async(
                    &self.port,
                    Signals::READABLE | Signals::PEER_CLOSED,
                    KEY_VOLUME + index as u64,
                )
                .is_ok();
        if !armed {
            let disk = open.disk;
            *place = None;
            // The last channel of a disk: the volume goes with it, committed by the
            // client's unmount, so nothing of it outlives the mount.
            let others = self.open.iter().flatten().any(|other| other.disk == disk);
            if !others && let Some(entry) = disks().entries.get_mut(disk) {
                entry.filesystem = None;
                entry.writable = false;
            }
        }
    }
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(start): Option<Channel<Kernel>> = bootstrap else {
        return 1;
    };
    if start
        .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
        .is_err()
    {
        return 2;
    }
    let mut bytes = [0_u8; 8];
    let mut handles = [Handle::INVALID; START_HANDLES];
    let Ok(got) = start.read(&mut bytes, &mut handles) else {
        return 3;
    };
    if got.handles != START_HANDLES {
        return 4;
    }
    ferrix_rt::log::use_debug_log(handles[0]);
    let cores: Channel<Kernel> = Channel::from_owned(OwnedHandle::from_raw(Kernel, handles[1]));
    let control: Channel<Kernel> = Channel::from_owned(OwnedHandle::from_raw(Kernel, handles[2]));
    let space = Space::from_owned(OwnedHandle::from_raw(Kernel, handles[3]));
    let Ok(port) = port::create(Kernel) else {
        return 5;
    };
    install(
        cores,
        space,
        Job::from_owned(OwnedHandle::from_raw(Kernel, handles[4])),
    );
    let mut client = Client {
        control,
        slot: None,
        open: Vec::new(),
        port,
        scratch: vec![0; SLOT_BYTES],
        clock: Arc::new(WallClock {
            at_hello_sec: 0,
            at_hello_nsec: 0,
            mono_at_hello: now(),
        }),
    };
    ferrix_rt::notice!("btrfsd   up: serving one partition's disks");
    let wake = Signals::READABLE | Signals::PEER_CLOSED;
    if disks()
        .cores
        .wait_async(&client.port, wake, KEY_CORES)
        .is_err()
        || client
            .control
            .wait_async(&client.port, wake, KEY_CONTROL)
            .is_err()
    {
        return 6;
    }
    loop {
        let until = now().saturating_add(LOOK_NANOS);
        match client.port.wait(Deadline::At(until)) {
            Ok(packet) if packet.key == KEY_CORES => {
                let wait = disks().cores.wait_one(Signals::READABLE, Deadline::At(0));
                if wait.is_ok_and(|signals| signals.intersects(Signals::READABLE)) {
                    disks().serve_core();
                }
                if disks()
                    .cores
                    .wait_async(&client.port, wake, KEY_CORES)
                    .is_err()
                {
                    return 7;
                }
            }
            Ok(packet) if packet.key == KEY_CONTROL => {
                if !client.serve_control() {
                    // Its client is gone: nothing here is anyone's now.
                    ferrix_rt::notice!("btrfsd   its client ended");
                    return 0;
                }
                if client
                    .control
                    .wait_async(&client.port, wake, KEY_CONTROL)
                    .is_err()
                {
                    return 8;
                }
            }
            Ok(packet) if packet.key >= KEY_VOLUME => {
                client.serve_volume((packet.key - KEY_VOLUME) as usize);
            }
            Ok(_) | Err(_) => {}
        }
        disks().serve_pending();
        disks().notice_ends();
    }
}
