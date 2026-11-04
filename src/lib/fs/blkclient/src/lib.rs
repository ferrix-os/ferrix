//! The block ring's other side, in user space (`docs/microkernel/BTRFSD.md` §2,
//! `docs/MICROKERNEL.md` §3's *Retired calls*): what `interfaces/block_ring` does
//! in a hybrid kernel, on channels, VMOs and ports. The driver is unchanged.
//!
//! The ring's rules are `ferrix_blkring::kernel`'s, the same code the kernel's core
//! runs: the driver's indices are hostile, and a bad one ends the ring. The driver
//! can write these pages at any moment, so every read of them is a volatile load,
//! into memory only this side can reach, and what the caller checks it checks
//! there. Both VMOs are held by the kernel first (`vmo_hold`), so a driver's
//! decommit, or its job's limit, cannot fault this side.
//!
//! One request at a time: a caller that wants more makes it wait, and the bound on
//! each wait is what turns a hung driver into `Disk::Gone`.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use core::mem::ManuallyDrop;
use core::sync::atomic::{Ordering, compiler_fence, fence};

pub use ferrix_blkring::RingMemory;
use ferrix_blkring::bell::{BELL_SUBMIT, Wait};
use ferrix_blkring::control::{Hello, Message, PORT_RIGHTS, Refusal, VMO_RIGHTS};
use ferrix_blkring::geometry::{Device, DeviceFlags};
use ferrix_blkring::kernel::{Completed, Ending, KernelSide, Slot};
use ferrix_blkring::layout::{Status, Submission};
use ferrix_native::call::Syscall;
use ferrix_native::channel::Channel;
use ferrix_native::handle::{Deadline, Object, OwnedHandle};
use ferrix_native::pending::Protection;
use ferrix_native::port::{self, Port};
use ferrix_native::vmo::Vmo;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::signals::Signals;

/// The most bytes of a control message.
const MESSAGE_BYTES: usize = ferrix_blkring::control::MAX_BYTES;

/// The key of the control channel's closing, on this side's port.
const KEY_CONTROL: u64 = 0xC0;

/// The most bytes of a ring: a standard one's page, with room to grow.
const MOST_RING_BYTES: u64 = 1024 * 1024;

/// How the disk failed a request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Failure {
    /// The device or driver answered an error.
    Io,
    /// Past the disk's end, or not whole sectors.
    Range,
    /// The driver is gone or hung, or broke the ring: every request fails.
    Gone,
}

/// A pair of bytes the driver can also write, reached only through volatile
/// accesses: never a Rust reference.
#[derive(Debug)]
pub struct Shared {
    at: usize,
    bytes: usize,
}

impl Shared {
    /// Bytes at `at`, for a side that mapped them itself.
    ///
    /// # Safety
    ///
    /// `at..at + bytes` is mapped, read-write, for as long as this lives.
    #[must_use]
    pub const unsafe fn from_raw(at: usize, bytes: usize) -> Shared {
        Shared { at, bytes }
    }

    fn address(&self, offset: usize, len: usize) -> Option<usize> {
        let end = offset.checked_add(len)?;
        (end <= self.bytes).then_some(self.at.wrapping_add(offset))
    }

    /// Copy bytes out, by volatile loads, into this side's memory. False past the
    /// end.
    pub fn copy_out(&self, offset: usize, out: &mut [u8]) -> bool {
        let Some(from) = self.address(offset, out.len()) else {
            return false;
        };
        for (index, slot) in out.iter_mut().enumerate() {
            // SAFETY: `from + index` is inside the mapping `vmo_hold` keeps
            // backed for as long as this lives; a byte, volatile, since the
            // driver may be writing it. Whatever is read is taken as data.
            *slot = unsafe { core::ptr::read_volatile((from + index) as *const u8) };
        }
        compiler_fence(Ordering::SeqCst);
        true
    }

    /// Copy bytes in, by volatile stores. False past the end.
    pub fn copy_in(&self, offset: usize, bytes: &[u8]) -> bool {
        let Some(to) = self.address(offset, bytes.len()) else {
            return false;
        };
        for (index, byte) in bytes.iter().enumerate() {
            // SAFETY: as in `copy_out`, written.
            unsafe { core::ptr::write_volatile((to + index) as *mut u8, *byte) };
        }
        compiler_fence(Ordering::SeqCst);
        true
    }
}

impl RingMemory for Shared {
    fn read_u8(&self, offset: usize) -> u8 {
        self.address(offset, 1).map_or(0, |at| {
            // SAFETY: as in `copy_out`.
            unsafe { core::ptr::read_volatile(at as *const u8) }
        })
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        if let Some(at) = self.address(offset, 1) {
            // SAFETY: as in `copy_in`.
            unsafe { core::ptr::write_volatile(at as *mut u8, value) };
        }
    }

    fn barrier(&self) {
        fence(Ordering::SeqCst);
    }

    fn read_u16(&self, offset: usize) -> u16 {
        match self.address(offset, 2) {
            Some(at) if at.is_multiple_of(2) => {
                // SAFETY: aligned, inside the mapping, one access: as in `copy_out`.
                let raw = unsafe { core::ptr::read_volatile(at as *const u16) };
                u16::from_le(raw)
            }
            _ => u16::from_le_bytes([self.read_u8(offset), self.read_u8(offset.wrapping_add(1))]),
        }
    }

    fn read_u32(&self, offset: usize) -> u32 {
        match self.address(offset, 4) {
            Some(at) if at.is_multiple_of(4) => {
                // SAFETY: as in `read_u16`.
                let raw = unsafe { core::ptr::read_volatile(at as *const u32) };
                u32::from_le(raw)
            }
            _ => u32::from_le_bytes([
                self.read_u8(offset),
                self.read_u8(offset.wrapping_add(1)),
                self.read_u8(offset.wrapping_add(2)),
                self.read_u8(offset.wrapping_add(3)),
            ]),
        }
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        match self.address(offset, 2) {
            // SAFETY: as in `read_u16`, written.
            Some(at) if at.is_multiple_of(2) => unsafe {
                core::ptr::write_volatile(at as *mut u16, value.to_le());
            },
            _ => {
                for (index, byte) in value.to_le_bytes().into_iter().enumerate() {
                    self.write_u8(offset.wrapping_add(index), byte);
                }
            }
        }
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        match self.address(offset, 4) {
            // SAFETY: as in `read_u16`, written.
            Some(at) if at.is_multiple_of(4) => unsafe {
                core::ptr::write_volatile(at as *mut u32, value.to_le());
            },
            _ => {
                for (index, byte) in value.to_le_bytes().into_iter().enumerate() {
                    self.write_u8(offset.wrapping_add(index), byte);
                }
            }
        }
    }
}

/// A block device a driver serves, from this side of the ring.
pub struct Disk<S: Syscall + Copy> {
    side: ManuallyDrop<KernelSide<'static, Shared>>,
    slots: *mut [Slot],
    mapped: [Mapped<S>; 2],
    unmap: fn(usize, usize),
    data: Shared,
    device: Device,
    driver_port: Port<S>,
    port: Port<S>,
    control: Channel<S>,
    patience_nanos: u64,
    clock: fn() -> u64,
    next_id: u64,
    gone: bool,
    name: [u8; 8],
    location: u32,
}

impl<S: Syscall + Copy> core::fmt::Debug for Disk<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Disk")
            .field("device", &self.device)
            .field("gone", &self.gone)
            .finish_non_exhaustive()
    }
}

/// Why a driver's HELLO was not taken up: REFUSED has gone back to it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NotTaken(pub Refusal);

/// Wait for HELLO on `control`, up to `deadline`, take the ring up and answer
/// READY: the core's `take_up`, here. A driver that says nothing in time, or
/// something else, is refused.
///
/// # Errors
///
/// [`NotTaken`].
pub fn accept<S: Syscall + Copy>(
    sys: S,
    control: Channel<S>,
    deadline: Deadline,
    patience_nanos: u64,
    clock: fn() -> u64,
    unmap: fn(usize, usize),
) -> Result<Disk<S>, NotTaken> {
    match take_up(sys, &control, deadline, unmap) {
        Ok(ring) => Ok(Disk {
            side: ManuallyDrop::new(ring.side),
            slots: ring.slots,
            mapped: ring.mapped,
            unmap,
            data: ring.data,
            device: ring.device,
            driver_port: ring.driver_port,
            port: ring.port,
            control,
            patience_nanos,
            clock,
            next_id: 1,
            gone: false,
            name: ring.name,
            location: ring.location,
        }),
        Err(refusal) => {
            let reply = Message::Refused(refusal.raw()).encode();
            let _ = control.write(reply.as_bytes());
            Err(NotTaken(refusal))
        }
    }
}

struct Ring<S: Syscall + Copy> {
    side: KernelSide<'static, Shared>,
    slots: *mut [Slot],
    mapped: [Mapped<S>; 2],
    data: Shared,
    device: Device,
    driver_port: Port<S>,
    port: Port<S>,
    name: [u8; 8],
    location: u32,
}

/// One of the driver's VMOs: held, mapped, and let go of when the disk ends (H.4).
#[derive(Debug)]
pub struct Mapped<S: Syscall + Copy> {
    _vmo: Vmo<S>,
    _hold: OwnedHandle<S>,
    at: usize,
    bytes: usize,
}

fn take_up<S: Syscall + Copy>(
    sys: S,
    control: &Channel<S>,
    deadline: Deadline,
    unmap: fn(usize, usize),
) -> Result<Ring<S>, Refusal> {
    let _ = control
        .wait_one(Signals::READABLE | Signals::PEER_CLOSED, deadline)
        .map_err(|_| Refusal::Malformed)?;
    let mut bytes = [0_u8; MESSAGE_BYTES];
    let mut handles = [Handle::INVALID; 3];
    let got = control
        .read(&mut bytes, &mut handles)
        .map_err(|_| Refusal::Malformed)?;
    let owned = |handle| OwnedHandle::from_raw(sys, handle);
    // Owned first, so that any refusal below closes all three.
    let [ring, data, driver_port] = handles.map(owned);
    let hello: Hello = match Message::decode(bytes.get(..got.bytes).unwrap_or(&[])) {
        Ok(Message::Hello(hello)) => hello,
        _ => return Err(Refusal::Malformed),
    };
    if got.handles != 3 {
        return Err(Refusal::Rights);
    }
    let accepted = hello.validate_message()?;
    let device = accepted.device;
    // The rule of a core in user space (H.3): not fewer than these, and from
    // here on exactly these.
    let exactly = |handle: OwnedHandle<S>, rights| {
        handle
            .replace(Requested::Exactly(rights))
            .map_err(|_| Refusal::Rights)
    };
    let ring_vmo = Vmo::from_owned(exactly(ring, VMO_RIGHTS)?);
    let data_vmo = Vmo::from_owned(exactly(data, VMO_RIGHTS)?);
    let driver_port = Port::from_owned(exactly(driver_port, PORT_RIGHTS)?);
    // Size before anything is held: past the ring's most, or short of what the
    // driver declared, costs nobody anything. A wrong object fails here.
    let ring_bytes = ring_vmo.size().map_err(|_| Refusal::Rights)?;
    let data_bytes = data_vmo.size().map_err(|_| Refusal::Rights)?;
    if ring_bytes > MOST_RING_BYTES {
        return Err(Refusal::Header);
    }
    if data_bytes < device.data_vmo_size() {
        return Err(Refusal::Device);
    }
    let (ring_map, ring) = held_mapping(ring_vmo).ok_or(Refusal::Header)?;
    let Some((data_map, data)) = held_mapping(data_vmo) else {
        unmap(ring_map.at, ring_map.bytes);
        return Err(Refusal::Device);
    };
    let mapped = [ring_map, data_map];
    let unmap_both = |mapped: &[Mapped<S>; 2]| {
        for one in mapped {
            unmap(one.at, one.bytes);
        }
    };
    let slots: *mut [Slot] = Box::into_raw(vec![Slot::EMPTY; 4].into_boxed_slice());
    // SAFETY: just made and owned here; `Disk`'s drop takes it back after the
    // side that borrows it, and every refusal below does too.
    let storage: &'static mut [Slot] = unsafe { &mut *slots };
    let refuse = |why: Refusal, mapped: &[Mapped<S>; 2]| {
        unmap_both(mapped);
        // SAFETY: the side that borrowed it is gone, or never was.
        drop(unsafe { Box::from_raw(slots) });
        why
    };
    let Ok(side) = KernelSide::attach(ring, ring_bytes, device, storage) else {
        return Err(refuse(Refusal::Header, &mapped));
    };
    // The port's type, which the rights above cannot show: a bell the driver may
    // get at any moment, on a port it gave us, and a wrong object refuses it.
    if matches!(
        driver_port.queue(BELL_SUBMIT, [0, 0]),
        Err(ferrix_native::error::Error::WrongType)
    ) {
        return Err(refuse(Refusal::Rights, &mapped));
    }
    let made = port::create(sys)
        .map_err(|_| Refusal::Device)
        .and_then(|port| {
            // READY's right, and the one that lets it travel: a handle goes on a
            // channel from user space only with `TRANSFER`, which costs nothing
            // here since the driver's own `HELLO` port has the same two.
            let given = port
                .duplicate(Requested::Exactly(PORT_RIGHTS))
                .map_err(|_| Refusal::Device)?;
            let ready = Message::Ready.encode();
            control
                .write_with(ready.as_bytes(), [given.into_owned()])
                .map_err(|_| Refusal::Malformed)?;
            Ok(port)
        });
    let port = match made {
        Ok(port) => port,
        Err(why) => return Err(refuse(why, &mapped)),
    };
    Ok(Ring {
        side,
        slots,
        mapped,
        data,
        device,
        driver_port,
        port,
        name: *accepted.identity.name.as_bytes(),
        location: accepted.identity.location.0,
    })
}

impl<S: Syscall + Copy> Mapped<S> {
    /// Where it is mapped.
    #[must_use]
    pub const fn address(&self) -> usize {
        self.at
    }

    /// How many bytes.
    #[must_use]
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Hold `vmo` in the kernel (BD.2, on its payer's account, H.1), then map it
/// read-write. The caller unmaps when it lets it go.
pub fn held_mapping<S: Syscall + Copy>(vmo: Vmo<S>) -> Option<(Mapped<S>, Shared)> {
    let hold = vmo.hold_whole().ok()?;
    let bytes = usize::try_from(vmo.size().ok()?).ok()?;
    let at = vmo.map(None, bytes, Protection::ReadWrite, 0).ok()?;
    Some((
        Mapped {
            _vmo: vmo,
            _hold: hold,
            at,
            bytes,
        },
        Shared { at, bytes },
    ))
}

impl<S: Syscall + Copy> Drop for Disk<S> {
    /// A dead driver's ring goes with its disk (H.4): the mappings, the holds and
    /// the handles, and the slots. A restarted driver is a new [`accept`], so
    /// what a crash loop leaves behind stays what it was. Requests are `&mut`, so
    /// none is in flight here.
    fn drop(&mut self) {
        // SAFETY: not used again; dropped once, here, before what it borrows.
        unsafe { ManuallyDrop::drop(&mut self.side) };
        // SAFETY: `take_up` made it with `Box::into_raw`; its one borrower went
        // on the line above.
        drop(unsafe { Box::from_raw(self.slots) });
        for one in &self.mapped {
            (self.unmap)(one.at, one.bytes);
        }
    }
}

impl<S: Syscall + Copy> Disk<S> {
    /// The disk's size in sectors.
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.device.capacity()
    }

    /// The bytes of a sector.
    #[must_use]
    pub fn block_size(&self) -> u32 {
        self.device.block_size()
    }

    /// The most sectors of one request.
    #[must_use]
    pub fn max_sectors(&self) -> u32 {
        self.device.max_sectors()
    }

    /// Where the device is, as its HELLO said.
    #[must_use]
    pub const fn location(&self) -> u32 {
        self.location
    }

    /// `ferrix_blkring`'s device flags.
    #[must_use]
    pub const fn flags(&self) -> u32 {
        self.device.flags().0
    }

    /// Whether a request has already found its driver gone.
    #[must_use]
    pub const fn is_gone(&self) -> bool {
        self.gone
    }

    /// Whether the driver's end of the control channel has closed: it ended.
    #[must_use]
    pub fn driver_closed(&self) -> bool {
        self.control
            .wait_one(Signals::PEER_CLOSED, Deadline::At(0))
            .is_ok_and(|signals| signals.intersects(Signals::PEER_CLOSED))
    }

    /// The name the driver was told to give it, `vda` and so on.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        let end = self
            .name
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(self.name.len());
        self.name.get(..end).unwrap_or(&[])
    }

    fn fail_all(&mut self, why: Ending) -> Failure {
        if !self.gone {
            self.gone = true;
            let _ = self.side.end(why);
        }
        Failure::Gone
    }

    /// One request, sent and answered, within the patience.
    fn request(&mut self, submission: Submission) -> Result<Completed, Failure> {
        if self.gone {
            return Err(Failure::Gone);
        }
        self.side.submit(submission).map_err(|_| Failure::Gone)?;
        if let Some(bell) = self.side.publish() {
            // A full port already holds a bell the driver has not taken.
            let packet = bell.packet();
            let _ = self.driver_port.queue(bell.key(), packet.data);
        }
        // One bound for the request, however many bells a hung driver rings.
        let until = (self.clock)().saturating_add(self.patience_nanos);
        loop {
            match self.side.poll() {
                Ok(Some(completed)) => {
                    self.side.woke();
                    return Ok(completed);
                }
                Ok(None) => {}
                Err(_) => return Err(self.fail_all(Ending::DriverDied)),
            }
            match self.side.prepare_to_sleep() {
                Ok(Wait::Pending(_)) => continue,
                Ok(Wait::Sleep) => {}
                Err(_) => return Err(self.fail_all(Ending::DriverDied)),
            }
            // The driver's end closing is a death, and wakes this wait at once; so
            // is its silence, for a request's whole patience (`gone_silent`'s rule:
            // the disk goes).
            if self.driver_closed() {
                return Err(self.fail_all(Ending::DriverDied));
            }
            let _ = self
                .control
                .wait_async(&self.port, Signals::PEER_CLOSED, KEY_CONTROL);
            if self.port.wait(Deadline::At(until)).is_err() {
                return Err(self.fail_all(Ending::DriverDied));
            }
            self.side.woke();
        }
    }

    /// Read whole sectors from `sector` into `out`.
    ///
    /// # Errors
    ///
    /// [`Failure`].
    pub fn read(&mut self, mut sector: u64, out: &mut [u8]) -> Result<(), Failure> {
        let size = self.block_size() as usize;
        if size == 0 || !out.len().is_multiple_of(size) {
            return Err(Failure::Range);
        }
        for piece in out.chunks_mut(self.max_sectors().max(1) as usize * size) {
            let count = (piece.len() / size) as u32;
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            let done = self.request(Submission::read(id, sector, count, 0))?;
            if done.status != Status::Ok || done.bytes_done != piece.len() as u64 {
                return Err(Failure::Io);
            }
            if !self.data.copy_out(0, piece) {
                return Err(Failure::Range);
            }
            sector += u64::from(count);
        }
        Ok(())
    }

    /// Write whole sectors at `sector`.
    ///
    /// # Errors
    ///
    /// [`Failure`].
    pub fn write(&mut self, mut sector: u64, bytes: &[u8], durable: bool) -> Result<(), Failure> {
        let size = self.block_size() as usize;
        if size == 0 || !bytes.len().is_multiple_of(size) {
            return Err(Failure::Range);
        }
        for piece in bytes.chunks(self.max_sectors().max(1) as usize * size) {
            let count = (piece.len() / size) as u32;
            if !self.data.copy_in(0, piece) {
                return Err(Failure::Range);
            }
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            let mut submission = Submission::write(id, sector, count, 0);
            if durable && self.device.flags().contains(DeviceFlags::FUA) {
                submission = submission.with_fua();
            }
            let done = self.request(submission)?;
            if done.status != Status::Ok {
                return Err(Failure::Io);
            }
            sector += u64::from(count);
        }
        if durable && !self.device.flags().contains(DeviceFlags::FUA) {
            self.flush()?;
        }
        Ok(())
    }

    /// Make every write that has returned durable.
    ///
    /// # Errors
    ///
    /// [`Failure`].
    pub fn flush(&mut self) -> Result<(), Failure> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let done = self.request(Submission::flush(id))?;
        if done.status == Status::Ok || done.status == Status::Unsupported {
            Ok(())
        } else {
            Err(Failure::Io)
        }
    }
}
