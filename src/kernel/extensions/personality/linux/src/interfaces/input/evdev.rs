//! `/dev/input/event<N>`: one open of an input device.
//!
//! `docs/INPUT.md` §3.3 fixes the subset. Where it leaves a rule to Linux --
//! which errors, which lengths, what a read returns -- the rule is
//! `drivers/input/evdev.c`, and `ferrix-inputctl`'s queue is where the
//! reading and the dropping live. This is the file object over them: a read,
//! a poll, and the `EVIOC*` requests.
//!
//! # Lengths follow the caller's width
//!
//! Linux copies a bitmap in whole `long`s of the caller's width and returns
//! the byte count. The requests that carry a length encode it in the request
//! number, so this decodes direction, type `'E'`, number and size rather than
//! matching whole numbers -- `EVIOCGNAME(16)` and `EVIOCGNAME(64)` are
//! different numbers for one request.

use alloc::sync::Arc;

use alloc::vec::Vec;
use core::any::Any;

use ferrix_inputctl::message::RawEvent;
use ferrix_inputctl::queue::{Clock, ReadError, ReadFlags};
use ferrix_inputctl::session::GrabError;
use ferrix_linux_abi::input::{
    self, EV_ABS, EV_CNT, EV_LED, EV_REP, EV_SYN, EV_VERSION, EVIOCGID, EVIOCGRAB, EVIOCGREP,
    EVIOCGVERSION, EVIOCREVOKE, EVIOCSCLOCKID, EVIOCSREP, Event,
};
use ferrix_linux_abi::layout::{Field, Layout};
use ferrix_linux_abi::socket::Width;
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{
    DirEntry, Errno, FIRST_CURSOR, FileType, Inode, Metadata, Readiness, Result as VfsResult,
    Timespec,
};

use crate::sync::SpinLock;
use crate::syscall::process::{self, Process};
use crate::syscall::uaccess;

use super::{EVDEV_MINOR_BASE, INPUT_MAJOR, InputDevice, Open};

/// One open of an `event<N>` node.
pub(crate) struct EventFile {
    device: Arc<InputDevice>,
    open: Arc<Open>,
    /// Whether this open has been revoked by `EVIOCREVOKE`.
    revoked: SpinLock<bool>,
}

impl EventFile {
    /// Open `device`, giving the open its own queue.
    ///
    /// # Errors
    ///
    /// `ENOMEM` when the queue could not be made.
    pub(crate) fn open(device: Arc<InputDevice>) -> VfsResult<Arc<Self>> {
        let open = device.open().ok_or(Errno::ENOMEM)?;
        Ok(Arc::new(Self {
            device,
            open,
            revoked: SpinLock::new(false),
        }))
    }

    /// Whether reads are over: the driver is gone, or this open was revoked.
    fn finished(&self) -> bool {
        self.device.is_gone() || *self.revoked.lock()
    }
}

impl Drop for EventFile {
    fn drop(&mut self) {
        // Closing releases the grab, as `evdev_release` does.
        self.device.close(&self.open);
        self.device.changed.wake_all();
    }
}

impl core::fmt::Debug for EventFile {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("EventFile")
            .field("event", &self.device.index)
            .finish()
    }
}

impl Inode for EventFile {
    fn metadata(&self) -> Metadata {
        metadata(self.device.index)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    /// Linux's event device has no `splice_read`: `sendfile` and `splice`
    /// from it are `EINVAL`, into a pipe as into anything.
    fn splices_out(&self) -> bool {
        false
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.read_stream(buf, false)
    }

    /// Readable with a whole report to read, and hung up once the driver is
    /// gone or the open is revoked, which is what `evdev_poll` reports.
    fn poll(&self) -> Readiness {
        let gone = self.finished();
        let poll = self.open.queue.lock().poll(gone);
        Readiness {
            readable: poll.readable,
            writable: false,
            hangup: poll.hangup,
            // A queue reports no error of its own: `evdev_poll` sets
            // `EPOLLERR` only beside `EPOLLHUP`, for a device that is gone.
            error: poll.hangup,
            priority: false,
        }
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(crate::fs::wake::shared(&self.device.changed));
        true
    }

    /// The same queues, and the same answer, for its whole life.
    fn poll_queues_fixed(&self) -> bool {
        true
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(self.device.changed.wakes())
    }

    /// A program's events for the device, as `evdev_write` takes them: whole
    /// `input_event`s, a buffer too small for one being `EINVAL`, and what is
    /// left past the last whole one not written. The device's LEDs change
    /// in its state (`EVIOCGLED`) and those that changed go to its driver,
    /// which lights them; `SYN_REPORT` is taken and does nothing. Any other
    /// type is `EINVAL`, where Linux injects it as though the device sent
    /// it: `docs/INPUT.md` §3.3.
    fn write_stream(&self, data: &[u8], _nonblock: bool) -> VfsResult<usize> {
        if self.finished() {
            return Err(Errno::ENODEV);
        }
        let width = caller_width();
        let size = Event::size(width);
        if data.len() < size {
            return Err(Errno::EINVAL);
        }
        let whole = data.len() - data.len() % size;
        let mut leds = Vec::new();
        for chunk in data.get(..whole).unwrap_or(&[]).chunks_exact(size) {
            let event = Event::read(width, chunk).ok_or(Errno::EINVAL)?;
            match event.r#type {
                EV_LED => leds.push(RawEvent::new(EV_LED, event.code, event.value)),
                EV_SYN => {}
                _ => return Err(Errno::EINVAL),
            }
        }
        self.device.write_leds(&leds);
        Ok(whole)
    }

    /// Whole `input_event`s, never half a report.
    ///
    /// A buffer too small for one event is `EINVAL`, as `evdev_read` answers
    /// it; an empty one reads nothing and is not an error.
    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> VfsResult<usize> {
        let width = caller_width();
        let mut take = |nonblocking: bool| -> Result<usize, ReadError> {
            let clocks = InputDevice::clocks();
            self.open.queue.lock().read(
                buf,
                ReadFlags {
                    width,
                    nonblocking,
                    gone: self.finished(),
                },
                &clocks,
            )
        };
        if nonblock {
            return take(true).map_err(read_error);
        }
        let caller = process::current();
        let killed = || {
            caller
                .as_ref()
                .is_some_and(|process| process.signal_pending())
        };
        let mut answer = take(true);
        while matches!(answer, Err(ReadError::Empty | ReadError::WouldBlock)) && !killed() {
            let _ = self.device.changed.wait_until_deadline(
                || self.open.queue.lock().has_packet() || self.finished() || killed(),
                u64::MAX,
            );
            answer = take(true);
        }
        match answer {
            Ok(read) => Ok(read),
            Err(ReadError::Empty | ReadError::WouldBlock) if killed() => Err(Errno::ERESTARTSYS),
            Err(error) => Err(read_error(error)),
        }
    }
}

/// What a queue's refusal is as an errno, following `evdev_read`.
fn read_error(error: ReadError) -> Errno {
    match error {
        // "The device is gone" is `ENODEV` however much is queued, which is
        // `evdev_read`'s first check.
        ReadError::Gone => Errno::ENODEV,
        ReadError::TooSmall => Errno::EINVAL,
        ReadError::WouldBlock | ReadError::Empty => Errno::EAGAIN,
    }
}

/// The `event<index>` node's metadata.
/// An event node's inode number is this plus its number.
///
/// A range of its own, as the cards have one at `1 << 40` and the disks one
/// at `1 << 32`: two nodes of one file system may not share an inode number,
/// and `event0` at 1 was the number `/dev` itself has.
pub(crate) const EVENT_INO_BASE: u64 = 1 << 41;

pub(crate) fn metadata(index: u32) -> Metadata {
    let zero = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    Metadata {
        ino: EVENT_INO_BASE + u64::from(index),
        kind: FileType::CharDevice,
        // 0660 and root's, as `card0` is: Ferrix has no `input` group.
        permissions: 0o660,
        nlink: 1,
        uid: 0,
        gid: 0,
        size: 0,
        rdev: makedev(INPUT_MAJOR, EVDEV_MINOR_BASE.saturating_add(index)),
        blocks: 0,
        block_size: 4096,
        atime: zero,
        mtime: zero,
        ctime: zero,
    }
}

/// The open device `file` is, if it is one.
pub(crate) fn of(io: &Arc<dyn Inode>) -> Option<Arc<EventFile>> {
    Arc::clone(io).into_any().downcast::<EventFile>().ok()
}

/// `/dev/input`'s entries: one per published device.
pub(crate) fn read_dir(cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> VfsResult<()> {
    // A cursor counts from `FIRST_CURSOR`, not from zero: the two entries
    // before it are `.` and `..`, which the layer above emits. Counting from
    // zero listed nothing at all, because the first call arrives with the
    // cursor already at two.
    let first = usize::try_from(cursor.saturating_sub(FIRST_CURSOR)).unwrap_or(usize::MAX);
    for (at, index) in super::device_indices().into_iter().enumerate().skip(first) {
        let mut name = [0u8; 16];
        let len = write_name(&mut name, index);
        let entry = DirEntry {
            ino: EVENT_INO_BASE + u64::from(index),
            kind: FileType::CharDevice,
            name: name.get(..len).unwrap_or(b"event"),
            next: FIRST_CURSOR.saturating_add(at as u64).saturating_add(1),
        };
        if !emit(entry) {
            return Ok(());
        }
    }
    Ok(())
}

/// `event<index>` into `out`, giving how many bytes it took.
fn write_name(out: &mut [u8; 16], index: u32) -> usize {
    let prefix = b"event";
    let mut at = 0;
    for &byte in prefix {
        if let Some(slot) = out.get_mut(at) {
            *slot = byte;
            at += 1;
        }
    }
    let mut digits = [0u8; 10];
    let mut count = 0;
    let mut value = index;
    loop {
        if let Some(slot) = digits.get_mut(count) {
            *slot = b'0' + (value % 10) as u8;
            count += 1;
        }
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for step in 0..count {
        let digit = digits.get(count - 1 - step).copied().unwrap_or(b'0');
        if let Some(slot) = out.get_mut(at) {
            *slot = digit;
            at += 1;
        }
    }
    at
}

/// The number `event<N>` names, if `name` is one.
pub(crate) fn event_number(name: &[u8]) -> Option<u32> {
    let digits = name.strip_prefix(b"event".as_slice())?;
    if digits.is_empty() || (digits.len() > 1 && digits.first() == Some(&b'0')) {
        return None;
    }
    let mut value: u32 = 0;
    for &byte in digits {
        let digit = byte.checked_sub(b'0').filter(|digit| *digit < 10)?;
        value = value.checked_mul(10)?.checked_add(u32::from(digit))?;
    }
    Some(value)
}

// ---------------------------------------------------------------------------
// The `EVIOC*` requests
//
// `docs/INPUT.md` §3.3 fixes the subset; everything it leaves open is
// `drivers/input/evdev.c`'s `evdev_do_ioctl`. A request that carries a length
// encodes it in its number, so this decodes the number rather than matching
// whole ones: `EVIOCGNAME(16)` and `EVIOCGNAME(64)` are different numbers for
// one request.
// ---------------------------------------------------------------------------

/// Answer one `ioctl` on an open input device.
///
/// # Errors
///
/// `ENOTTY` for a request this is not, and whatever the request's own rule
/// says otherwise.
pub(crate) fn ioctl(
    process: &Process,
    file: &Arc<EventFile>,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    // A revoked open answers nothing, as `evdev_ioctl_handler` checks first.
    if *file.revoked.lock() {
        return Err(Errno::ENODEV);
    }
    match request {
        EVIOCGVERSION => return write_i32(process, arg, EV_VERSION),
        EVIOCGID => return get_id(process, file, arg),
        EVIOCGREP => return get_rep(process, file, arg),
        EVIOCSREP => return set_rep(process, file, arg),
        EVIOCGRAB => return grab(process, file, arg),
        EVIOCREVOKE => return revoke(file),
        EVIOCSCLOCKID => return set_clock(process, file, arg),
        _ => {}
    }
    // The requests with an argument in the number. Direction and type first,
    // so a request of another type is `ENOTTY` rather than being decoded.
    if input::ioc_type(request) != u32::from(b'E') {
        return Err(Errno::ENOTTY);
    }
    let number = input::ioc_nr(request);
    let size = input::ioc_size(request);
    if input::ioc_dir(request) == input::IOC_READ {
        match number {
            // The text is copied out of the session before it is copied to
            // the caller: copying to a program's memory may fault, and a
            // fault may not be resolved with a lock held that disables
            // preemption. `Text` is `Copy`, so this costs a move of the
            // field and nothing else.
            0x06 => {
                let name = *file.device.session.lock().name();
                return text(process, &name, arg, size);
            }
            // A virtio device has no physical path, so `EVIOCGPHYS` is
            // `ENOENT`, which is what Linux answers for a device with none.
            0x07 => return Err(Errno::ENOENT),
            0x08 => {
                let serial = *file.device.session.lock().serial();
                return text(process, &serial, arg, size);
            }
            0x09 => {
                let bits = file.device.session.lock().capabilities().bits.props;
                return bitmap(process, &bits, arg, size);
            }
            0x18 => {
                let bits = file.device.session.lock().state().keys;
                return bitmap(process, &bits, arg, size);
            }
            0x19 => {
                let bits = file.device.session.lock().state().leds;
                return bitmap(process, &bits, arg, size);
            }
            // `EVIOCGSND`: no device here declares `EV_SND`, and the core
            // publishes none, so the answer is an empty bitmap rather than
            // an error -- which is what Linux gives for a device with no
            // sounds.
            0x1a => return bitmap(process, &[0u8; 8], arg, size),
            0x1b => {
                let bits = file.device.session.lock().state().sw;
                return bitmap(process, &bits, arg, size);
            }
            0x20..=0x3F => return get_bit(process, file, (number - 0x20) as u16, arg, size),
            0x40..=0x7F => return get_abs(process, file, (number - 0x40) as u16, arg),
            _ => {}
        }
    }
    // Everything else in the `'E'` space -- the keycode requests, the force
    // feedback ones, `EVIOCSABS`, the masks -- is `EINVAL` in this
    // iteration, which `docs/INPUT.md` §3.3 fixes and §6 has a row for.
    Err(Errno::EINVAL)
}

/// `EVIOCGID`.
fn get_id(process: &Process, file: &Arc<EventFile>, arg: u64) -> Result<usize, Errno> {
    let id = file.device.session.lock().id();
    let value = input::InputId {
        bustype: id.bustype,
        vendor: id.vendor,
        product: id.product,
        version: id.version,
    };
    let mut bytes = [0u8; input::InputId::SIZE];
    value.write(&mut bytes).ok_or(Errno::EINVAL)?;
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `EVIOCGREP`: the stored delay and period.
///
/// Only for a device declaring `EV_REP`, as `evdev_do_ioctl` checks; the core
/// makes no repeat of its own (`docs/INPUT.md` §3.1).
fn get_rep(process: &Process, file: &Arc<EventFile>, arg: u64) -> Result<usize, Errno> {
    let session = file.device.session.lock();
    if !session.capabilities().bits.has_type(EV_REP) {
        return Err(Errno::EINVAL);
    }
    let state = session.state();
    let (delay, period) = (
        state.repeat.first().copied().unwrap_or(0),
        state.repeat.get(1).copied().unwrap_or(0),
    );
    drop(session);
    let mut bytes = [0u8; 8];
    if let Some(slot) = bytes.get_mut(..4) {
        slot.copy_from_slice(&delay.to_le_bytes());
    }
    if let Some(slot) = bytes.get_mut(4..) {
        slot.copy_from_slice(&period.to_le_bytes());
    }
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `EVIOCSREP`.
fn set_rep(process: &Process, file: &Arc<EventFile>, arg: u64) -> Result<usize, Errno> {
    let mut bytes = [0u8; 8];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    // `unsigned int[2]` on the wire, kept as the `i32`s the session holds:
    // `input_enable_softrepeat` takes ints and nothing sends a value past
    // `i32::MAX` milliseconds.
    let read = |at: usize| -> i32 {
        bytes
            .get(at..at + 4)
            .and_then(|slice| slice.try_into().ok())
            .map_or(0, i32::from_le_bytes)
    };
    let mut session = file.device.session.lock();
    if !session.capabilities().bits.has_type(EV_REP) {
        return Err(Errno::EINVAL);
    }
    session.set_repeat(read(0), read(4));
    Ok(0)
}

/// `EVIOCGRAB`: 1 takes the grab, 0 gives it back.
fn grab(process: &Process, file: &Arc<EventFile>, arg: u64) -> Result<usize, Errno> {
    let _ = process;
    // The argument is the value itself, not a pointer: `_IOW('E', 0x90,
    // int)` with `evdev_do_ioctl` reading `p` as an integer.
    let mut session = file.device.session.lock();
    let answer = if arg == 0 {
        session.ungrab(file.open.id)
    } else {
        session.grab(file.open.id)
    };
    match answer {
        Ok(()) => Ok(0),
        // Another open holds it.
        Err(GrabError::Busy) => Err(Errno::EBUSY),
        // Releasing one this open does not hold.
        Err(GrabError::NotHolder) => Err(Errno::EINVAL),
    }
}

/// `EVIOCREVOKE`: this open reads `ENODEV` from now on.
fn revoke(file: &Arc<EventFile>) -> Result<usize, Errno> {
    *file.revoked.lock() = true;
    file.open.queue.lock().revoke();
    // A revoked open loses its grab, as `evdev_revoke` does.
    file.device.session.lock().release(file.open.id);
    file.device.changed.wake_all();
    Ok(0)
}

/// `EVIOCSCLOCKID`.
fn set_clock(process: &Process, file: &Arc<EventFile>, arg: u64) -> Result<usize, Errno> {
    let mut bytes = [0u8; 4];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let id = i32::from_le_bytes(bytes);
    let clock = Clock::from_id(id).ok_or(Errno::EINVAL)?;
    file.open
        .queue
        .lock()
        .set_clock(clock, crate::timer::now_nanos());
    Ok(0)
}

/// `EVIOCGBIT(ev, len)`.
fn get_bit(
    process: &Process,
    file: &Arc<EventFile>,
    kind: u16,
    arg: u64,
    size: u32,
) -> Result<usize, Errno> {
    if kind >= EV_CNT {
        return Err(Errno::EINVAL);
    }
    let session = file.device.session.lock();
    let caps = session.capabilities();
    // `ev` 0 is the event types; every other is that type's codes. A type
    // the device did not declare has an empty bitmap rather than an error,
    // as `evdev_handle_get_val` gives for a type with no codes.
    let bits: Vec<u8> = if kind == 0 {
        caps.bits.types.to_vec()
    } else {
        caps.bits
            .codes(kind)
            .map_or_else(Vec::new, |(bits, _)| bits.to_vec())
    };
    drop(session);
    bitmap(process, &bits, arg, size)
}

/// `EVIOCGABS(abs)`.
fn get_abs(process: &Process, file: &Arc<EventFile>, axis: u16, arg: u64) -> Result<usize, Errno> {
    let session = file.device.session.lock();
    // An axis the core does not publish is `EINVAL`, which
    // `docs/INPUT.md` §3.3 fixes: Linux copies zeros for it, and a program
    // reading zeros for an axis that is not there cannot tell.
    if !session.capabilities().bits.has_code(EV_ABS, axis) {
        return Err(Errno::EINVAL);
    }
    let info = session.abs_info(axis).ok_or(Errno::EINVAL)?;
    drop(session);
    let value = input::AbsInfo {
        value: info.value,
        minimum: info.minimum,
        maximum: info.maximum,
        fuzz: info.fuzz,
        flat: info.flat,
        resolution: info.resolution,
    };
    let mut bytes = [0u8; input::AbsInfo::SIZE];
    value.write(&mut bytes).ok_or(Errno::EINVAL)?;
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// Copy a string out, cut to `size`, and give the bytes copied.
///
/// Linux copies the NUL and counts it, and answers `ENOENT` for a device that
/// gave no such string.
fn text(
    process: &Process,
    value: &ferrix_inputctl::message::Text,
    arg: u64,
    size: u32,
) -> Result<usize, Errno> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err(Errno::ENOENT);
    }
    let room = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
    if room == 0 {
        return Err(Errno::EINVAL);
    }
    let mut out = Vec::with_capacity(room);
    out.extend_from_slice(bytes.get(..bytes.len().min(room - 1)).unwrap_or(&[]));
    out.push(0);
    uaccess::copy_to_user(process.space(), arg, &out).map_err(|_| Errno::EFAULT)?;
    Ok(out.len())
}

/// Copy a bitmap out, cut to `size`, and give the bytes copied.
///
/// A buffer longer than the bitmap gets the bitmap and no more: Linux copies
/// `min(len, sizeof(bits))`.
fn bitmap(process: &Process, bits: &[u8], arg: u64, size: u32) -> Result<usize, Errno> {
    let room = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
    let take = room.min(bits.len());
    if take == 0 {
        return Ok(0);
    }
    let slice = bits.get(..take).ok_or(Errno::EINVAL)?;
    uaccess::copy_to_user(process.space(), arg, slice).map_err(|_| Errno::EFAULT)?;
    Ok(take)
}

/// Write one `int` out.
fn write_i32(process: &Process, arg: u64, value: i32) -> Result<usize, Errno> {
    uaccess::copy_to_user(process.space(), arg, &value.to_le_bytes()).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// The width of the caller, which decides how wide an `input_event` is.
///
/// Every architecture Ferrix builds a compositor for is 64-bit, and ARMv7-A
/// takes no part in the input gates (`docs/INPUT.md` §3.3), so this is the
/// pointer width of the kernel itself. A 32-bit caller on a 64-bit kernel
/// would need the `compat` flag a `personality` brings, which Ferrix does not
/// have.
const fn caller_width() -> Width {
    if usize::BITS == 32 {
        Width::Bits32
    } else {
        Width::Bits64
    }
}
