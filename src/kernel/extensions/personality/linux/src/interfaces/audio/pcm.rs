//! `/dev/snd/controlC<N>` and `/dev/snd/pcmC<N>D0p`: the ALSA subset
//! `docs/AUDIO.md` §3.4 answers.
//!
//! The stream's rules are `ferrix_sndctl::pcm`'s. What is here is reading a
//! request's argument from the program, calling the stream with its lock
//! held, and writing the answer back; copying a program's samples into the
//! buffer with no lock held; and waiting, for room to write and for a drain,
//! on the card's wait queue. Each request's errno for each state is the
//! stream's, which follows `sound/core/pcm_native.c`.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::Ordering;

use ferrix_linux_abi::layout::{Field, Layout};
use ferrix_linux_abi::sound::{
    CLASS_GENERIC, CTL_VERSION, Ctl, CtlCardInfo, CtlElemList, HwParams, PCM_VERSION, Pcm, PcmInfo,
    STATE_DRAINING, STREAM_CAPTURE, STREAM_PLAYBACK, SUBCLASS_GENERIC_MIX, Status, SwParams,
    SyncPtr, Xferi,
};
use ferrix_sndctl::pcm::{Drain, Stream};
use ferrix_vfs::initramfs::makedev;
use ferrix_vfs::{Errno, FileType, Inode, Metadata, Readiness, Result as VfsResult, Timespec};

use crate::syscall::process::{self, Process};
use crate::syscall::uaccess;
use crate::timer;

use super::{Card, SOUND_MAJOR, control_minor, playback_minor, width};

/// The name the card's driver goes by in `CARD_INFO`: one alsa-lib has no
/// `cards/<driver>.conf` for, so `default` is `plug` over `hw`
/// (`docs/AUDIO.md` §2.2).
const DRIVER: &[u8] = b"Ferrix";
/// The card's id, which alsa-lib's name hints name it by: a word.
const ID: &[u8] = b"Ferrix";
/// Its short name.
const NAME: &[u8] = b"VirtIO SoundCard";

/// Where the playback nodes' inode numbers start, and the control nodes'.
pub(crate) const PCM_INO_BASE: u64 = 1 << 43;
/// Where the control nodes' inode numbers start.
pub(crate) const CONTROL_INO_BASE: u64 = 1 << 44;

fn zero() -> Timespec {
    Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    }
}

fn node_metadata(ino: u64, minor: u32) -> Metadata {
    Metadata {
        ino,
        kind: FileType::CharDevice,
        // 0660 and root's, as `card0` and `event0` are.
        permissions: 0o660,
        nlink: 1,
        uid: 0,
        gid: 0,
        size: 0,
        rdev: makedev(SOUND_MAJOR, minor),
        blocks: 0,
        block_size: 4096,
        atime: zero(),
        mtime: zero(),
        ctime: zero(),
    }
}

/// `pcmC<card>D0p`'s metadata.
pub(crate) fn pcm_metadata(card: u32) -> Metadata {
    node_metadata(PCM_INO_BASE + u64::from(card), playback_minor(card))
}

/// `controlC<card>`'s metadata.
pub(crate) fn control_metadata(card: u32) -> Metadata {
    node_metadata(CONTROL_INO_BASE + u64::from(card), control_minor(card))
}

/// Copy `text` into a NUL-terminated field.
fn text<const N: usize>(text: &[u8]) -> [u8; N] {
    let mut field = [0_u8; N];
    for (slot, byte) in field.iter_mut().zip(text.iter().take(N - 1)) {
        *slot = *byte;
    }
    field
}

fn fault<T>(_: T) -> Errno {
    Errno::EFAULT
}

/// Read `size` bytes of a request's argument.
fn read_arg(process: &Process, arg: u64, size: usize) -> Result<Vec<u8>, Errno> {
    let mut bytes = alloc::vec![0_u8; size];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(fault)?;
    Ok(bytes)
}

fn write_arg(process: &Process, arg: u64, bytes: &[u8]) -> Result<(), Errno> {
    uaccess::copy_to_user(process.space(), arg, bytes).map_err(fault)
}

fn read_int(process: &Process, arg: u64) -> Result<i32, Errno> {
    let bytes = read_arg(process, arg, 4)?;
    i32::get(&bytes, 0).ok_or(Errno::EFAULT)
}

fn write_int(process: &Process, arg: u64, value: i32) -> Result<(), Errno> {
    write_arg(process, arg, &value.to_le_bytes())
}

/// Write a `long` of the program's width.
fn write_long(process: &Process, arg: u64, value: i64) -> Result<(), Errno> {
    let mut bytes = [0_u8; 8];
    let len = width().bytes();
    let fits = match width() {
        ferrix_linux_abi::socket::Width::Bits32 => {
            i32::try_from(value).ok().map(|v| v.put(&mut bytes, 0))
        }
        ferrix_linux_abi::socket::Width::Bits64 => Some(value.put(&mut bytes, 0)),
    };
    if fits.flatten().is_none() {
        return Err(Errno::EOVERFLOW);
    }
    write_arg(process, arg, bytes.get(..len).unwrap_or(&[]))
}

/// What `INFO` and the control node's `PCM_INFO` answer.
fn pcm_info(card: &Card) -> PcmInfo {
    let open = card.opened.load(Ordering::Acquire);
    PcmInfo {
        device: 0,
        subdevice: 0,
        stream: STREAM_PLAYBACK,
        // Card numbers are small.
        card: card.index as i32,
        id: text(b"VirtIO PCM"),
        name: text(b"VirtIO PCM"),
        subname: text(b"subdevice #0"),
        dev_class: CLASS_GENERIC,
        dev_subclass: SUBCLASS_GENERIC_MIX,
        subdevices_count: 1,
        subdevices_avail: u32::from(!open),
        pad1: [0; 16],
        reserved: [0; 64],
    }
}

// ---------------------------------------------------------------------------
// The playback node
// ---------------------------------------------------------------------------

/// One open of `pcmC<N>D0p`: the card's one stream, while this open has it.
pub(crate) struct PcmFile {
    card: Arc<Card>,
}

impl PcmFile {
    /// Open the card's stream: `EBUSY` while another open has it, as Linux
    /// answers for a card with one substream.
    pub(crate) fn open(card: Arc<Card>) -> VfsResult<Arc<Self>> {
        if card.is_gone() {
            return Err(Errno::ENODEV);
        }
        if card.opened.swap(true, Ordering::AcqRel) {
            return Err(Errno::EBUSY);
        }
        card.stream.lock().open(width());
        Ok(Arc::new(Self { card }))
    }
}

impl Drop for PcmFile {
    fn drop(&mut self) {
        self.card.with_stream(Stream::close);
        self.card.opened.store(false, Ordering::Release);
        self.card.changed.wake_all();
    }
}

impl core::fmt::Debug for PcmFile {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("PcmFile")
            .field("card", &self.card.index)
            .finish()
    }
}

impl Inode for PcmFile {
    fn metadata(&self) -> Metadata {
        pcm_metadata(self.card.index)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn splices_out(&self) -> bool {
        false
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// A playback node has no `read`.
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> VfsResult<usize> {
        Err(Errno::EINVAL)
    }

    /// `poll` (`snd_pcm_poll`): writable with `avail_min` free.
    fn poll(&self) -> Readiness {
        let poll = self.card.stream.lock().poll();
        let gone = self.card.is_gone();
        Readiness {
            readable: false,
            writable: poll.writable,
            hangup: gone,
            error: poll.error,
            priority: false,
        }
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(crate::fs::wake::shared(&self.card.changed));
        true
    }

    /// The same queues, and the same answer, for its whole life.
    fn poll_queues_fixed(&self) -> bool {
        true
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(self.card.changed.wakes())
    }
}

/// The open playback node `io` is, if it is one.
pub(crate) fn pcm_of(io: &Arc<dyn Inode>) -> Option<Arc<PcmFile>> {
    Arc::clone(io).into_any().downcast::<PcmFile>().ok()
}

fn killed(caller: Option<&Arc<Process>>) -> bool {
    caller.is_some_and(|process| process.signal_pending())
}

/// A request on the playback node.
///
/// # Errors
///
/// The errno the stream answers the request with in its state, `EFAULT` for
/// an argument that cannot be read or written, and `ENOTTY` for a request
/// the node does not know.
pub(crate) fn pcm_ioctl(
    process: &Process,
    file: &Arc<PcmFile>,
    request: u32,
    arg: u64,
    nonblock: bool,
) -> Result<usize, Errno> {
    let card = &file.card;
    let Some(pcm) = Pcm::from_request(width(), request) else {
        return Err(Errno::ENOTTY);
    };
    card.stream.lock().check_connected()?;
    let now = timer::now_nanos();
    match pcm {
        Pcm::Pversion => write_int(process, arg, PCM_VERSION as i32).map(|()| 0),
        Pcm::Info => {
            let mut bytes = alloc::vec![0_u8; <PcmInfo as Field>::SIZE];
            let _ = pcm_info(card).write(&mut bytes);
            write_arg(process, arg, &bytes).map(|()| 0)
        }
        Pcm::Tstamp => read_int(process, arg).map(|_| 0),
        Pcm::Ttstamp => {
            let clock = read_int(process, arg)?;
            let clock = u32::try_from(clock).map_err(|_| Errno::EINVAL)?;
            card.stream.lock().set_tstamp_type(clock).map(|()| 0)
        }
        Pcm::UserPversion => {
            let version = read_int(process, arg)?;
            card.stream.lock().set_user_version(version as u32);
            Ok(0)
        }
        Pcm::HwRefine | Pcm::HwParams => hw_params(process, card, pcm, arg),
        Pcm::HwFree => card.with_stream(Stream::hw_free).map(|()| 0),
        Pcm::SwParams => sw_params(process, card, arg),
        Pcm::Status | Pcm::StatusExt => status(process, card, arg, now),
        Pcm::Delay => {
            let delay = card.stream.lock().delay()?;
            // At most a buffer.
            write_long(process, arg, delay as i64).map(|()| 0)
        }
        Pcm::Hwsync => card.stream.lock().hwsync().map(|()| 0),
        Pcm::SyncPtr => sync_ptr(process, card, arg),
        Pcm::Prepare => card.with_stream(Stream::prepare).map(|()| 0),
        Pcm::Reset => card.with_stream(Stream::reset).map(|()| 0),
        Pcm::Start => card
            .with_stream(|stream, effects| stream.start(now, effects))
            .map(|()| 0),
        Pcm::Drop => card
            .with_stream(|stream, effects| stream.drop_stream(now, effects))
            .map(|()| 0),
        Pcm::Drain => drain(card, nonblock).map(|()| 0),
        Pcm::Xrun => card
            .with_stream(|stream, effects| stream.xrun(now, effects))
            .map(|()| 0),
        // Nothing queued can be taken back or skipped here: the frames moved
        // are written back into the argument, as Linux does, and are none.
        Pcm::Rewind | Pcm::Forward => write_long(process, arg, 0).map(|()| 0),
        Pcm::WriteiFrames => writei(process, card, arg, nonblock),
        // `INFO_PAUSE` and `INFO_RESUME` are not set.
        Pcm::Pause | Pcm::Resume => Err(Errno::ENOSYS),
        Pcm::ChannelInfo
        | Pcm::ReadiFrames
        | Pcm::WritenFrames
        | Pcm::ReadnFrames
        | Pcm::Link
        | Pcm::Unlink => Err(Errno::EINVAL),
    }
}

fn hw_params(process: &Process, card: &Card, pcm: Pcm, arg: u64) -> Result<usize, Errno> {
    let size = HwParams::size(width());
    let bytes = read_arg(process, arg, size)?;
    let mut params = HwParams::read(width(), &bytes).ok_or(Errno::EFAULT)?;
    if pcm == Pcm::HwRefine {
        card.stream.lock().hw_refine(&mut params)?;
    } else {
        card.with_stream(|stream, effects| stream.hw_params(&mut params, effects))?;
    }
    let mut out = bytes;
    params.write(width(), &mut out).ok_or(Errno::EOVERFLOW)?;
    write_arg(process, arg, &out).map(|()| 0)
}

fn sw_params(process: &Process, card: &Card, arg: u64) -> Result<usize, Errno> {
    let bytes = read_arg(process, arg, SwParams::size(width()))?;
    let mut params = SwParams::read(width(), &bytes).ok_or(Errno::EFAULT)?;
    let result = card.with_stream(|stream, effects| stream.sw_params(&mut params, effects));
    // Written back whatever the answer, as `snd_pcm_sw_params_user` does.
    let mut out = bytes;
    params.write(width(), &mut out).ok_or(Errno::EOVERFLOW)?;
    write_arg(process, arg, &out)?;
    result.map(|()| 0)
}

fn status(process: &Process, card: &Card, arg: u64, now: u64) -> Result<usize, Errno> {
    let status = card.stream.lock().status(now);
    let mut out = alloc::vec![0_u8; Status::size(width())];
    status.write(width(), &mut out).ok_or(Errno::EOVERFLOW)?;
    write_arg(process, arg, &out).map(|()| 0)
}

fn sync_ptr(process: &Process, card: &Card, arg: u64) -> Result<usize, Errno> {
    let bytes = read_arg(process, arg, SyncPtr::size(width()))?;
    let mut sync = SyncPtr::read(width(), &bytes).ok_or(Errno::EFAULT)?;
    let (status, appl_ptr, avail_min) =
        card.stream
            .lock()
            .sync_ptr(sync.flags, sync.control.appl_ptr, sync.control.avail_min)?;
    sync.status = status;
    sync.control.appl_ptr = appl_ptr;
    sync.control.avail_min = avail_min;
    let mut out = alloc::vec![0_u8; SyncPtr::size(width())];
    sync.write(width(), &mut out).ok_or(Errno::EOVERFLOW)?;
    write_arg(process, arg, &out).map(|()| 0)
}

/// `DRAIN`: start the drain, then wait for it to end, giving up after a
/// timeout with no completion, as `snd_pcm_drain` does.
fn drain(card: &Card, nonblock: bool) -> Result<(), Errno> {
    let now = timer::now_nanos();
    let started = card.with_stream(|stream, effects| stream.drain(now, effects))?;
    if started == Drain::Done {
        return Ok(());
    }
    if nonblock {
        return Err(Errno::EAGAIN);
    }
    let caller = process::current();
    loop {
        let (draining, timeout) = {
            let stream = card.stream.lock();
            (stream.state() == STATE_DRAINING, stream.drain_timeout())
        };
        if !draining || card.is_gone() {
            return Ok(());
        }
        if killed(caller.as_ref()) {
            return Err(Errno::ERESTARTSYS);
        }
        let wakes = card.changed.wakes();
        let deadline = timer::now_nanos().saturating_add(timeout);
        let woken = card.changed.wait_until_deadline(
            || {
                card.changed.wakes() != wakes
                    || card.is_gone()
                    || killed(caller.as_ref())
                    || card.stream.lock().state() != STATE_DRAINING
            },
            deadline,
        );
        if !woken && !graced(card, wakes, caller.as_ref()) {
            let now = timer::now_nanos();
            let queued = card.stream.lock().queued();
            crate::console::println!(
                "  audio    card{}: a drain heard no completion for {} ms and gave up with {queued} \
                 frames queued",
                card.index,
                timeout / 1_000_000
            );
            return card.with_stream(|stream, effects| stream.drain_expired(now, effects));
        }
    }
}

/// A period more for a drain whose timeout passed with no completion:
/// whether one came in it.
///
/// The timeout may be the guest's rather than the device's. A host that
/// stops every one of the guest's processors for longer than the timeout
/// lets the guest's clock run on, and when they run again the drain's timer
/// is taken before the driver is, with completions already waiting for it:
/// `test-audio` failed so twice on 2026-09-27 at host load 25-29. A period
/// lets them in. A device that has stopped still fails, a period later.
fn graced(card: &Card, wakes: u64, caller: Option<&Arc<Process>>) -> bool {
    let period = u64::from(card.stream.lock().config().period_time()).saturating_mul(1000);
    let deadline = timer::now_nanos().saturating_add(period);
    card.changed.wait_until_deadline(
        || {
            card.changed.wakes() != wakes
                || card.is_gone()
                || killed(caller)
                || card.stream.lock().state() != STATE_DRAINING
        },
        deadline,
    )
}

/// `WRITEI_FRAMES`: copy frames into the buffer as it has room, starting and
/// feeding the stream, and say in the argument how many went.
fn writei(process: &Process, card: &Card, arg: u64, nonblock: bool) -> Result<usize, Errno> {
    let bytes = read_arg(process, arg, Xferi::size(width()))?;
    let mut xferi = Xferi::read(width(), &bytes).ok_or(Errno::EFAULT)?;
    let frame_bytes = u64::from(card.stream.lock().config().frame_bytes());
    let wanted = xferi.frames;
    let caller = process::current();
    let mut done: u64 = 0;
    let mut error = None;
    let mut scratch = alloc::vec![0_u8; super::PAGE_BYTES];
    while done < wanted {
        let room = match card.stream.lock().room() {
            Ok(room) => room,
            Err(errno) => {
                error = Some(errno);
                break;
            }
        };
        if room.frames == 0 {
            if nonblock {
                if done == 0 {
                    error = Some(Errno::EAGAIN);
                }
                break;
            }
            if killed(caller.as_ref()) {
                if done == 0 {
                    error = Some(Errno::ERESTARTSYS);
                }
                break;
            }
            let _ = card.changed.wait_until_deadline(
                || {
                    card.is_gone()
                        || killed(caller.as_ref())
                        || card.stream.lock().room().is_ok_and(|room| room.frames > 0)
                        || card.stream.lock().room().is_err()
                },
                u64::MAX,
            );
            continue;
        }
        // At most a page at a time, so the copy needs one page of scratch.
        let per_page = (super::PAGE_BYTES as u64 / frame_bytes).max(1);
        let frames = u64::from(room.frames).min(wanted - done).min(per_page);
        let len = (frames * frame_bytes) as usize;
        let from = xferi.buf.saturating_add(done * frame_bytes);
        let slot = scratch.get_mut(..len).ok_or(Errno::EINVAL)?;
        if uaccess::copy_from_user(process.space(), from, slot).is_err() {
            if done == 0 {
                error = Some(Errno::EFAULT);
            }
            break;
        }
        card.fill(room.offset as usize, slot)?;
        let now = timer::now_nanos();
        // Room and frames are a buffer's worth at most.
        if let Err(errno) =
            card.with_stream(|stream, effects| stream.wrote(frames as u32, now, effects))
        {
            if done == 0 {
                error = Some(errno);
            }
            break;
        }
        done += frames;
    }
    xferi.result = match error {
        Some(errno) => -i64::from(errno.0),
        None => done as i64,
    };
    let mut out = bytes;
    xferi.write(width(), &mut out).ok_or(Errno::EOVERFLOW)?;
    write_arg(process, arg, &out)?;
    match error {
        Some(errno) => Err(errno),
        None => Ok(0),
    }
}

// ---------------------------------------------------------------------------
// The control node
// ---------------------------------------------------------------------------

/// One open of `controlC<N>`.
pub(crate) struct ControlFile {
    card: Arc<Card>,
}

impl ControlFile {
    /// Open the card's control node: any number may.
    pub(crate) fn open(card: Arc<Card>) -> VfsResult<Arc<Self>> {
        if card.is_gone() {
            return Err(Errno::ENODEV);
        }
        Ok(Arc::new(Self { card }))
    }
}

impl core::fmt::Debug for ControlFile {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ControlFile")
            .field("card", &self.card.index)
            .finish()
    }
}

impl Inode for ControlFile {
    fn metadata(&self) -> Metadata {
        control_metadata(self.card.index)
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn splices_out(&self) -> bool {
        false
    }

    fn is_stream(&self) -> bool {
        true
    }

    /// There are no elements, so there are never events to read.
    fn read_stream(&self, _buf: &mut [u8], nonblock: bool) -> VfsResult<usize> {
        if nonblock {
            return Err(Errno::EAGAIN);
        }
        let caller = process::current();
        let _ = self
            .card
            .changed
            .wait_until_deadline(|| self.card.is_gone() || killed(caller.as_ref()), u64::MAX);
        if self.card.is_gone() {
            Err(Errno::ENODEV)
        } else {
            Err(Errno::ERESTARTSYS)
        }
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.read_stream(buf, false)
    }

    fn poll(&self) -> Readiness {
        let gone = self.card.is_gone();
        Readiness {
            readable: false,
            writable: false,
            hangup: gone,
            error: gone,
            priority: false,
        }
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(crate::fs::wake::shared(&self.card.changed));
        true
    }

    /// The same queues, and the same answer, for its whole life.
    fn poll_queues_fixed(&self) -> bool {
        true
    }

    fn poll_changes(&self) -> Option<u64> {
        Some(self.card.changed.wakes())
    }
}

/// The open control node `io` is, if it is one.
pub(crate) fn control_of(io: &Arc<dyn Inode>) -> Option<Arc<ControlFile>> {
    Arc::clone(io).into_any().downcast::<ControlFile>().ok()
}

/// A request on the control node (`docs/AUDIO.md` §3.4).
///
/// # Errors
///
/// `ENODEV` once the card is gone, `ENOENT` for an element, of which there
/// are none, `ENXIO` for a PCM device that does not exist, `EFAULT` for an
/// argument that cannot be read or written, and `ENOTTY` for a request the
/// node does not know.
pub(crate) fn control_ioctl(
    process: &Process,
    file: &Arc<ControlFile>,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    let card = &file.card;
    if card.is_gone() {
        return Err(Errno::ENODEV);
    }
    let Some(ctl) = Ctl::from_request(width(), request) else {
        return Err(Errno::ENOTTY);
    };
    match ctl {
        Ctl::Pversion => write_int(process, arg, CTL_VERSION as i32).map(|()| 0),
        Ctl::CardInfo => {
            let mut longname = [0_u8; 80];
            let mut writer = Cursor::new(&mut longname);
            let _ = core::fmt::write(
                &mut writer,
                format_args!("VirtIO SoundCard at {:#x}", card.location),
            );
            let info = CtlCardInfo {
                card: card.index as i32,
                pad: 0,
                id: text(ID),
                driver: text(DRIVER),
                name: text(NAME),
                longname,
                reserved_: [0; 16],
                mixername: [0; 80],
                components: [0; 128],
            };
            let mut bytes = alloc::vec![0_u8; <CtlCardInfo as Field>::SIZE];
            let _ = info.write(&mut bytes);
            write_arg(process, arg, &bytes).map(|()| 0)
        }
        Ctl::PcmNextDevice => {
            let after = read_int(process, arg)?;
            write_int(process, arg, if after < 0 { 0 } else { -1 }).map(|()| 0)
        }
        Ctl::PcmInfo => {
            let bytes = read_arg(process, arg, <PcmInfo as Field>::SIZE)?;
            let asked = PcmInfo::read(&bytes).ok_or(Errno::EFAULT)?;
            if asked.device != 0 || asked.subdevice > 0 && asked.subdevice != u32::MAX {
                return Err(Errno::ENXIO);
            }
            if asked.stream == STREAM_CAPTURE {
                return Err(Errno::ENOENT);
            }
            if asked.stream != STREAM_PLAYBACK {
                return Err(Errno::EINVAL);
            }
            let mut out = bytes;
            let _ = pcm_info(card).write(&mut out);
            write_arg(process, arg, &out).map(|()| 0)
        }
        Ctl::PcmPreferSubdevice | Ctl::RawmidiPreferSubdevice => read_int(process, arg).map(|_| 0),
        Ctl::ElemList => {
            let bytes = read_arg(process, arg, CtlElemList::size(width()))?;
            let mut list = CtlElemList::read(width(), &bytes).ok_or(Errno::EFAULT)?;
            list.used = 0;
            list.count = 0;
            let mut out = bytes;
            list.write(width(), &mut out).ok_or(Errno::EOVERFLOW)?;
            write_arg(process, arg, &out).map(|()| 0)
        }
        // A subscription with nothing to report: taken, and asked back with
        // a negative argument, as `snd_ctl_subscribe_events` answers it.
        Ctl::SubscribeEvents => {
            let asked = read_int(process, arg)?;
            if asked < 0 {
                write_int(process, arg, 1)?;
            }
            Ok(0)
        }
        Ctl::ElemInfo
        | Ctl::ElemRead
        | Ctl::ElemWrite
        | Ctl::ElemLock
        | Ctl::ElemUnlock
        | Ctl::ElemRemove
        | Ctl::TlvRead
        | Ctl::TlvWrite
        | Ctl::TlvCommand => Err(Errno::ENOENT),
        Ctl::ElemAdd | Ctl::ElemReplace => Err(Errno::EPERM),
        Ctl::HwdepNextDevice | Ctl::RawmidiNextDevice | Ctl::UmpNextDevice => {
            write_int(process, arg, -1).map(|()| 0)
        }
        Ctl::HwdepInfo | Ctl::RawmidiInfo => Err(Errno::ENXIO),
        // `SNDRV_CTL_POWER_D0`: always on.
        Ctl::PowerState => write_int(process, arg, 0).map(|()| 0),
        Ctl::Power => Err(Errno::ENOTTY),
    }
}

/// A `core::fmt::Write` over a byte array, keeping the last byte a NUL.
struct Cursor<'a> {
    bytes: &'a mut [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a mut [u8]) -> Self {
        Self { bytes, at: 0 }
    }
}

impl core::fmt::Write for Cursor<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for byte in text.bytes() {
            if self.at + 1 >= self.bytes.len() {
                break;
            }
            if let Some(slot) = self.bytes.get_mut(self.at) {
                *slot = byte;
            }
            self.at += 1;
        }
        Ok(())
    }
}
