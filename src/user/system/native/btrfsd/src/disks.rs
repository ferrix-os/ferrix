//! The disks this server has: each driver's ring, taken from `devmgr`'s `CORE_OPEN`
//! (`docs/microkernel/BTRFSD.md` §2), and the volume's device over them.
//!
//! One thread, and a disk's requests wait for it in turn. A driver that dies parks
//! its disk: its requests wait for the next driver, up to [`PARK_PATIENCE_NANOS`],
//! and then answer `EIO`, the kernel core's rule. A mount keeps a disk's index, so
//! it goes on over the next driver.

use alloc::vec;
use alloc::vec::Vec;
use core::cell::UnsafeCell;

use ferrix_blkclient::{Disk, Failure, accept};
use ferrix_blkring::control::CONTROL_RIGHTS;
use ferrix_btrfs::BtrfsError;
use ferrix_btrfs::volume::{Device as BtrfsDevice, ReadKind};
use ferrix_btrfs_write::Error as WriteError;
use ferrix_devmgr_proto::{CORE_BYTES, CoreOpen, CoreOpened, CorePublished};
use ferrix_fsproto::wire::DiskUp;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::signals::Signals;
use ferrix_rt::Kernel;
use ferrix_rt::native::channel::{self, Channel};
use ferrix_rt::native::handle::{Deadline, Object as _, OwnedHandle};
use ferrix_rt::native::job::{Job, Resource};
use ferrix_rt::native::space::Space;

/// How long a driver has between its `CORE_OPEN` and its HELLO.
pub(crate) const HELLO_PATIENCE_NANOS: u64 = 10_000_000_000;
/// How long one request waits on a silent driver.
const REQUEST_PATIENCE_NANOS: u64 = 30_000_000_000;
/// How long a parked disk's requests wait for the next driver.
const PARK_PATIENCE_NANOS: u64 = 30_000_000_000;
/// The most disks and the most drivers on their way.
const MOST_DISKS: usize = 16;
/// How long one look at the cores' channel waits, while a disk is parked.
const PARKED_LOOK_NANOS: u64 = 10_000_000;

/// The nanoseconds since boot.
pub(crate) fn now() -> u64 {
    ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
}

/// A driver that was given its channel and has not said HELLO.
pub(crate) struct Pending {
    pub(crate) channel: Channel<Kernel>,
    location: u32,
    until: u64,
}

/// A disk, its driver alive or parked.
pub(crate) struct Entry {
    pub(crate) name: [u8; 8],
    location: u32,
    pub(crate) disk: Option<Disk<Kernel>>,
    parked_at: u64,
    pub(crate) sectors: u64,
    pub(crate) sector_size: u32,
    pub(crate) filesystem: Option<alloc::sync::Arc<dyn ferrix_vfs::FileSystem>>,
    pub(crate) writable: bool,
}

/// Everything of the block side.
pub(crate) struct Disks {
    pub(crate) cores: Channel<Kernel>,
    pub(crate) entries: Vec<Entry>,
    pub(crate) pending: Vec<Pending>,
    pub(crate) events: Option<Channel<Kernel>>,
    /// Told about disks that came up before the client said HELLO.
    told: usize,
    space: Space<Kernel>,
    job: Job<Kernel>,
}

struct One(UnsafeCell<Option<Disks>>);

// SAFETY: btrfsd runs one thread.
unsafe impl Sync for One {}

static DISKS: One = One(UnsafeCell::new(None));

/// Set the disks up, once, before anything reads them.
pub(crate) fn install(cores: Channel<Kernel>, space: Space<Kernel>, job: Job<Kernel>) {
    // SAFETY: one thread, before any other use.
    unsafe {
        *DISKS.0.get() = Some(Disks {
            cores,
            entries: Vec::new(),
            pending: Vec::new(),
            events: None,
            told: 0,
            space,
            job,
        });
    }
}

/// The one set of disks.
pub(crate) fn disks() -> &'static mut Disks {
    #[expect(
        clippy::expect_used,
        reason = "AUDIT: install runs first thing in main"
    )]
    // SAFETY: one thread, and no caller holds this across a call that takes it
    // again: `Device` below goes through here only inside its own calls, and the
    // main loop's reference ends before it serves a request.
    unsafe {
        (*DISKS.0.get()).as_mut().expect("installed")
    }
}

fn unmap(at: usize, bytes: usize) {
    if let Err(why) = disks().space.unmap(at, bytes) {
        ferrix_rt::notice!("btrfsd   a dead driver's ring would not unmap: {why:?}");
    }
}

impl Disks {
    /// One message from `devmgr`, if there is one.
    pub(crate) fn serve_core(&mut self) {
        let mut bytes = [0_u8; CORE_BYTES];
        let mut handles = [Handle::INVALID; 1];
        let Ok(got) = self.cores.read(&mut bytes, &mut handles) else {
            return;
        };
        let answer = self.open(
            bytes.get(..got.bytes).unwrap_or(&[]),
            handles[0],
            got.handles,
        );
        let reply = CoreOpened {
            status: answer.as_ref().err().map_or(0, |errno| u32::from(errno.0)),
        }
        .encode();
        if let Err(why) = answer.as_ref() {
            ferrix_rt::notice!("btrfsd   CORE_OPEN refused: errno {}", why.0);
        }
        let sent = match answer {
            Ok(end) => self.cores.write_with(&reply, [end]).is_ok(),
            Err(_) => self.cores.write(&reply).is_ok(),
        };
        if !sent {
            ferrix_rt::notice!("btrfsd   devmgr's channel would not take an answer");
        }
    }

    /// `CORE_OPEN`, checked: the device says where it is, the call is the block
    /// ring's and no other, and a live driver has it already is refused (CO.1, CO.2).
    fn open(
        &mut self,
        bytes: &[u8],
        device: Handle,
        count: usize,
    ) -> Result<OwnedHandle<Kernel>, Errno> {
        // No device handle comes with it: this server drives nothing, and the word
        // is compared with what its driver's HELLO says (CO.1, CO.2).
        if count != 0 {
            drop(OwnedHandle::from_raw(Kernel, device));
            return Err(Errno::EINVAL);
        }
        let open = CoreOpen::decode(bytes).ok_or(Errno::EINVAL)?;
        if open.call as usize != nr::BLOCK_RING_CREATE {
            return Err(Errno::ENOSYS);
        }
        let live = self.entries.iter().any(|entry| {
            entry.location == open.location
                && entry.disk.as_ref().is_some_and(|disk| !disk.is_gone())
        });
        if live
            || self
                .pending
                .iter()
                .any(|pending| pending.location == open.location)
        {
            return Err(Errno::EBUSY);
        }
        if self.pending.len() >= MOST_DISKS {
            return Err(Errno::ENOSPC);
        }
        let (near, far) = channel::create(Kernel).map_err(|_| Errno::ENOMEM)?;
        let far = far
            .into_owned()
            .replace(Requested::Exactly(CONTROL_RIGHTS))
            .map_err(|_| Errno::ENOMEM)?;
        self.pending.push(Pending {
            channel: near,
            location: open.location,
            until: now().saturating_add(HELLO_PATIENCE_NANOS),
        });
        Ok(far)
    }

    /// Whatever drivers said HELLO, or ran out of time.
    pub(crate) fn serve_pending(&mut self) {
        let at = now();
        let mut index = 0;
        while index < self.pending.len() {
            let Some(pending) = self.pending.get(index) else {
                break;
            };
            let signals = pending
                .channel
                .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::At(0))
                .unwrap_or(Signals::NONE);
            if signals.intersects(Signals::READABLE | Signals::PEER_CLOSED) {
                let pending = self.pending.remove(index);
                self.take_up(pending);
                continue;
            }
            if at > pending.until {
                let late = self.pending.remove(index);
                ferrix_rt::notice!("btrfsd   {:#x}: no HELLO in time", late.location);
                continue;
            }
            index += 1;
        }
    }

    fn take_up(&mut self, pending: Pending) {
        let location = pending.location;
        let disk = match accept(
            Kernel,
            pending.channel,
            Deadline::At(0),
            REQUEST_PATIENCE_NANOS,
            now,
            unmap,
        ) {
            Ok(disk) => disk,
            Err(refused) => {
                ferrix_rt::notice!("btrfsd   {location:#x}: refused: {:?}", refused.0);
                return;
            }
        };
        if disk.location() != location {
            ferrix_rt::notice!("btrfsd   {location:#x}: its HELLO names another place");
            return;
        }
        let mut name = [0_u8; 8];
        for (slot, byte) in name.iter_mut().zip(disk.name()) {
            *slot = *byte;
        }
        let up = DiskUp {
            name,
            sectors: disk.capacity(),
            sector_size: disk.block_size(),
            flags: disk.flags(),
            location,
            max_sectors: disk.max_sectors(),
        };
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.location == location)
        {
            entry.disk = Some(disk);
            ferrix_rt::notice!("btrfsd   {} is back", shown(&name));
        } else {
            if self.entries.len() >= MOST_DISKS {
                ferrix_rt::notice!("btrfsd   {location:#x}: too many disks");
                return;
            }
            self.entries.push(Entry {
                name,
                location,
                sectors: up.sectors,
                sector_size: up.sector_size,
                disk: Some(disk),
                parked_at: 0,
                filesystem: None,
                writable: false,
            });
            // What a driver's sparse VMO cost this server: nothing (H.1).
            let holds = self
                .job
                .quota(Resource::Memory)
                .map_or(0, |quota| quota.used / 1024);
            ferrix_rt::notice!(
                "btrfsd   {} is up; this server holds {holds} KiB",
                shown(&name)
            );
        }
        // Told first: `linuxd` has every disk by the time `devmgr` reports.
        self.tell();
        let published = CorePublished { location }.encode();
        if self.cores.write(&published).is_err() {
            ferrix_rt::notice!("btrfsd   devmgr's channel would not take PUBLISHED");
        }
    }

    /// The disks the client has not been told of, if it is there to be told.
    pub(crate) fn tell(&mut self) {
        let Some(events) = self.events.as_ref() else {
            return;
        };
        while let Some(entry) = self.entries.get(self.told) {
            let up = DiskUp {
                name: entry.name,
                sectors: entry.sectors,
                sector_size: entry.sector_size,
                flags: entry.disk.as_ref().map_or(0, Disk::flags),
                location: entry.location,
                max_sectors: entry.disk.as_ref().map_or(1, Disk::max_sectors),
            };
            if events.write(&up.encode()).is_err() {
                return;
            }
            self.told += 1;
        }
    }

    /// The entry named `name`.
    pub(crate) fn named(&self, name: &[u8]) -> Option<usize> {
        self.entries
            .iter()
            .position(|entry| entry.name.split(|&byte| byte == 0).next() == Some(name))
    }

    fn drive<R>(
        &mut self,
        index: usize,
        mut request: impl FnMut(&mut Disk<Kernel>) -> Result<R, Failure>,
    ) -> Result<R, Errno> {
        loop {
            let entry = self.entries.get_mut(index).ok_or(Errno::ENXIO)?;
            if let Some(disk) = entry.disk.as_mut() {
                match request(disk) {
                    Ok(done) => return Ok(done),
                    Err(Failure::Gone) => {
                        // The driver's memory goes now; the next one is a new
                        // accept (H.4).
                        entry.disk = None;
                        entry.parked_at = now();
                        ferrix_rt::notice!("btrfsd   {} parked", shown(&entry.name));
                    }
                    Err(Failure::Io) => {
                        ferrix_rt::notice!("btrfsd   {}: an I/O error", shown(&entry.name));
                        return Err(Errno::EIO);
                    }
                    Err(Failure::Range) => return Err(Errno::EINVAL),
                }
            }
            let parked_at = self.entries.get(index).ok_or(Errno::ENXIO)?.parked_at;
            if now().saturating_sub(parked_at) > PARK_PATIENCE_NANOS {
                return Err(Errno::EIO);
            }
            self.look(PARKED_LOOK_NANOS);
        }
    }

    /// Give `devmgr` and the drivers a moment.
    fn look(&mut self, nanos: u64) {
        let until = now().saturating_add(nanos);
        let signals = self
            .cores
            .wait_one(Signals::READABLE, Deadline::At(until))
            .unwrap_or(Signals::NONE);
        if signals.intersects(Signals::READABLE) {
            self.serve_core();
        }
        self.serve_pending();
    }

    /// Read sectors.
    pub(crate) fn read(&mut self, index: usize, sector: u64, out: &mut [u8]) -> Result<(), Errno> {
        self.drive(index, |disk| disk.read(sector, out))
    }

    /// Write sectors.
    pub(crate) fn write(&mut self, index: usize, sector: u64, bytes: &[u8]) -> Result<(), Errno> {
        self.drive(index, |disk| disk.write(sector, bytes, false))
    }

    /// Flush.
    pub(crate) fn flush(&mut self, index: usize) -> Result<(), Errno> {
        self.drive(index, Disk::flush)
    }

    /// A driver's end closed: what it held goes at once, in case nothing asks of
    /// its disk again.
    pub(crate) fn notice_ends(&mut self) {
        for entry in &mut self.entries {
            let ended = entry.disk.as_ref().is_some_and(Disk::driver_closed);
            if ended {
                entry.disk = None;
                entry.parked_at = now();
                ferrix_rt::notice!("btrfsd   {} parked: its driver ended", shown(&entry.name));
            }
        }
    }
}

impl ferrix_btrfs_write::WriteDevice for Volume {
    fn write_at(&mut self, physical: u64, data: &[u8]) -> Result<(), WriteError> {
        let failed = WriteError::DeviceWrite { physical };
        if data.is_empty() {
            return Ok(());
        }
        let len = data.len() as u64;
        let end = physical.checked_add(len).ok_or(failed)?;
        let first = physical / self.sector;
        if physical.is_multiple_of(self.sector) && len.is_multiple_of(self.sector) {
            return disks().write(self.index, first, data).map_err(|_| failed);
        }
        let sectors = end.div_ceil(self.sector).saturating_sub(first);
        let bytes = usize::try_from(sectors.saturating_mul(self.sector)).map_err(|_| failed)?;
        let mut bounce = vec![0_u8; bytes];
        disks()
            .read(self.index, first, &mut bounce)
            .map_err(|_| failed)?;
        let skip = usize::try_from(physical - first * self.sector).map_err(|_| failed)?;
        let patch = bounce.get_mut(skip..skip + data.len()).ok_or(failed)?;
        patch.copy_from_slice(data);
        disks()
            .write(self.index, first, &bounce)
            .map_err(|_| failed)
    }

    fn flush(&mut self) -> Result<(), WriteError> {
        disks().flush(self.index).map_err(|_| WriteError::Flush)
    }
}

/// A NUL-padded name, to print.
pub(crate) fn shown(name: &[u8]) -> &str {
    core::str::from_utf8(name.split(|&byte| byte == 0).next().unwrap_or(&[])).unwrap_or("?")
}

/// A disk as a btrfs volume reads it: bytes at physical offsets, in whole sectors.
/// The same arithmetic as the kernel's glue, in `fs/btrfs.rs`.
#[derive(Clone, Debug)]
pub(crate) struct Volume {
    pub(crate) index: usize,
    pub(crate) sector: u64,
}

impl BtrfsDevice for Volume {
    fn read_at(
        &mut self,
        physical: u64,
        buf: &mut [u8],
        _kind: ReadKind,
    ) -> Result<(), BtrfsError> {
        let failed = BtrfsError::DeviceRead { physical };
        if buf.is_empty() {
            return Ok(());
        }
        let len = buf.len() as u64;
        let end = physical.checked_add(len).ok_or(failed)?;
        let first = physical / self.sector;
        if physical.is_multiple_of(self.sector) && len.is_multiple_of(self.sector) {
            return disks().read(self.index, first, buf).map_err(|_| failed);
        }
        let sectors = end.div_ceil(self.sector).saturating_sub(first);
        let bytes = usize::try_from(sectors.saturating_mul(self.sector)).map_err(|_| failed)?;
        let mut bounce = vec![0_u8; bytes];
        disks()
            .read(self.index, first, &mut bounce)
            .map_err(|_| failed)?;
        let skip = usize::try_from(physical - first * self.sector).map_err(|_| failed)?;
        let taken = bounce.get(skip..skip + buf.len()).ok_or(failed)?;
        buf.copy_from_slice(taken);
        Ok(())
    }
}
