//! The render node, served: the chardev control (`docs/NVIDIA.md` §4.4),
//! the front end of `ferrix-intel-xelp` over a back end, and one event loop
//! on a port.
//!
//! The driver asks for its device's chardev control, says HELLO listing the
//! render node alone (K3: `/dev/dri/renderD<N>`, numbered by the kernel), and
//! waits for READY. The kernel refuses that HELLO while the GPU's DMA is not
//! translated (`L.object.192`, `docs/INTEL-GPU.md` §4.4 (a)); the driver
//! then says so and serves nothing. From READY on, every REQUEST goes to the
//! front end, and the front end's deadlines and the back end's interrupt
//! packets wake the loop.
//!
//! Three drivers use this file: `intel`, over the GT back end (M2, which
//! adds it to its module tree), the keeper (`keeper/main.rs`), over a
//! journal and a worker process (`docs/INTEL-GPU.md` §12), and `intel-fake`
//! (`../../intel-fake`), over the fake back end only `test-intel-uapi`'s
//! image carries (consultant C2, ledger 2352).

use core::fmt;
use ferrix_rt::log::Level;

use ferrix_chardevctl::message::{
    self, Hello, MAP_VMO, MAP_WRITE_COMBINING, MAX_NODES, Message, Op, Request, VERSION_INTERRUPTS,
};
use ferrix_chardevctl::node::RENDER_MINOR;
use ferrix_intel_xelp::frontend::{Backend, Frontend, Host, Installed};
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{DMABUF_CLOEXEC, DMABUF_WRITABLE, SYNC_CLOEXEC};
use ferrix_rt::Kernel;
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::chardev;
use ferrix_rt::native::device::Device;
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object};
use ferrix_rt::native::port::Port;
use ferrix_rt::native::vmo::Vmo;

/// The port key of the control channel's signals.
pub(crate) const KEY_CONTROL: u64 = 1;
/// The port key the back end binds its interrupt to: a packet with it is
/// an interrupt, and the back end is serviced with `interrupt` set.
pub(crate) const KEY_INTERRUPT: u64 = 2;
/// The port key of a back end in another process (the keeper's worker,
/// `docs/INTEL-GPU.md` §12): a packet with it services the back end as a
/// deadline does. The display card's is 3.
pub(crate) const KEY_BACKEND: u64 = 4;

/// The key a wake-up asked for through [`WAKE_AT`] arrives under: 'HP', the
/// display card's hotplug poll (`docs/DESKTOP.md` §3.4), clear of every
/// other key.
pub(crate) const KEY_WAKE: u64 = 0x4850;

/// A wake-up the display card asks for, at this `CLOCK_MONOTONIC` time in
/// nanoseconds; 0 for none. The loop holds no reference to the card, so
/// the card sets this and the loop queues a packet under [`KEY_WAKE`] when
/// it is due, which reaches the card as its other packets do.
pub(crate) static WAKE_AT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// `deadline` with [`WAKE_AT`] folded in: the earlier of the two, and the
/// wake-up queued on `port` once it is due.
fn with_wake(port: &Port<Kernel>, deadline: Option<u64>) -> Option<u64> {
    use core::sync::atomic::Ordering;
    let due = WAKE_AT.load(Ordering::Relaxed);
    if due == 0 {
        return deadline;
    }
    if ferrix_rt::linux::monotonic_nanos().unwrap_or(0) >= due {
        WAKE_AT.store(0, Ordering::Relaxed);
        let _ = port.queue(KEY_WAKE, [0, 0]);
        return deadline;
    }
    Some(deadline.map_or(due, |deadline| deadline.min(due)))
}

/// A line of the driver's log at `level` (`ferrix_rt::log`).
pub(crate) fn say(level: Level, arguments: fmt::Arguments<'_>) {
    ferrix_rt::log::line(level, arguments);
}

/// A back end's object memory, as the chardev core needs it: the VMO that
/// mmaps and dmabufs hand out.
pub(crate) trait AsVmo {
    /// The VMO.
    fn vmo(&self) -> &Vmo<Kernel>;

    #[allow(
        dead_code,
        reason = "only the Intel driver's card0 asks; the keeper and intel-fake share this file"
    )]
    /// The random tag in the page past the object's end, for card0 to find
    /// the object behind a buffer it is handed (`platform::Memory`); `None`
    /// for an object without one.
    fn tag(&self) -> Option<u64> {
        None
    }

    #[allow(
        dead_code,
        reason = "only the Intel driver's card0 asks; the keeper and intel-fake share this file"
    )]
    /// The object's own bytes, without a tag page.
    fn bytes(&self) -> usize {
        0
    }
}

/// Why serving stopped.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Stopped {
    /// No chardev control for the device.
    Control,
    /// HELLO could not be said, or no READY came.
    Hello,
    /// The kernel refused the HELLO; it said why.
    Refused,
    /// The control channel closed: the kernel took the nodes back.
    Closed,
    /// The port failed.
    Port,
}

/// The errno a native call's failure stands for, toward a program.
fn errno(error: Error) -> Errno {
    match error {
        Error::Fault => Errno::EFAULT,
        Error::BadHandle => Errno::EBADF,
        Error::NoMemory => Errno::ENOMEM,
        Error::LimitReached | Error::NoHandles => Errno::EMFILE,
        Error::ShouldWait => Errno::EAGAIN,
        Error::AlreadyBound => Errno::EBUSY,
        Error::InvalidArgs => Errno::EINVAL,
        Error::AccessDenied => Errno::EACCES,
        Error::Unsupported => Errno::EOPNOTSUPP,
        _ => Errno::EIO,
    }
}

/// The chardev core, as the front end's [`Host`].
pub(crate) struct ChardevHost {
    control: Channel<Kernel>,
    /// The effective user id of the request being answered.
    euid: u32,
    /// The ioctl counts, when `ferrix.intelgpu.ioctlstat` asks for them.
    stats: Option<IoctlStats>,
}

/// How often [`IoctlStats`] are said, in nanoseconds.
const STATS_EVERY_NS: u64 = 10_000_000_000;

/// One ioctl number's counts over a report's span.
#[derive(Clone, Copy, Default)]
struct IoctlSlot {
    /// Calls.
    calls: u32,
    /// Calls the front end held, answered later (a wait, a busy binding).
    held: u32,
    /// The driver's time in the call, from the request read to its reply
    /// sent or its hold, with the copies in and out it made.
    ns: u64,
    /// The longest such time.
    max_ns: u64,
    /// The part of `ns` before the reply went: what the program waited for
    /// in the driver. The rest (settling fences, the counts) is after it.
    reply_ns: u64,
}

/// The render node's ioctls counted by number (`ferrix.intelgpu.ioctlstat`,
/// `docs/INTEL-GPU.md` §13): calls, the driver's time in each, the copies
/// and replies they cost, and the loop's busy time, said every 10 s. A
/// call's time as its program sees it is this plus the kernel's forwarding,
/// which a program measures by itself.
pub(crate) struct IoctlStats {
    slots: [IoctlSlot; 256],
    /// `copy_in` and `copy_out` calls, and their bytes.
    copies: u32,
    copy_bytes: u64,
    /// Replies sent, at once or later.
    replies: u32,
    /// The request being answered: whether it was replied to in the call.
    current: u64,
    replied: bool,
    /// Its command and when its call began.
    current_cmd: u32,
    began: u64,
    /// The last ioctl the loop answered, for [`IoctlStats::awake_long`].
    last_cmd: u32,
    /// Time the loop spent awake, not in `port_wait`.
    busy_ns: u64,
    /// When the loop last woke.
    awake_at: u64,
    /// The report's span began.
    since: u64,
}

impl IoctlStats {
    fn new(now: u64) -> Self {
        Self {
            slots: [IoctlSlot::default(); 256],
            copies: 0,
            copy_bytes: 0,
            replies: 0,
            current: u64::MAX,
            replied: false,
            current_cmd: 0,
            began: now,
            last_cmd: 0,
            busy_ns: 0,
            awake_at: now,
            since: now,
        }
    }

    /// The loop goes to wait at `now`: the time since it woke was busy,
    /// and the counts are said if due.
    fn asleep(&mut self, now: u64) {
        self.busy_ns += now.saturating_sub(self.awake_at);
        self.awake_at = now;
        self.report(now);
    }

    /// An ioctl's call is answered from `request`'s read.
    fn begin(&mut self, request: u64, cmd: u32) -> u64 {
        self.current = request;
        self.current_cmd = cmd;
        self.last_cmd = cmd;
        self.replied = false;
        self.began = now_ns();
        self.began
    }

    /// The call being answered replied at `now`.
    fn replied_at(&mut self, now: u64) {
        self.replied = true;
        let began = self.began;
        if let Some(slot) = self.slots.get_mut((self.current_cmd & 0xff) as usize) {
            slot.reply_ns += now.saturating_sub(began);
        }
    }

    /// The loop was awake from `awake_at` to `now` on `what`: said when that
    /// was over 100 ms, which leaves the screen's flips and every program
    /// waiting (2026-10-24: 65 s on the N150 with the freqlog stopped).
    fn awake_long(&self, now: u64, what: &str) {
        let ms = now.saturating_sub(self.awake_at) / 1_000_000;
        if ms >= 100 {
            ferrix_rt::warn!(
                "intel: ioctlstat the loop was awake {ms} ms on {what} (last ioctl {:#04x} {})",
                self.last_cmd & 0xff,
                ioctl_name((self.last_cmd & 0xff) as u8)
            );
        }
    }

    /// The call of ioctl `cmd` that began at `began` returned.
    fn end(&mut self, cmd: u32, began: u64) {
        let ns = now_ns().saturating_sub(began);
        let held = u32::from(!self.replied);
        if let Some(slot) = self.slots.get_mut((cmd & 0xff) as usize) {
            slot.calls += 1;
            slot.held += held;
            slot.ns += ns;
            slot.max_ns = slot.max_ns.max(ns);
        }
        self.current = u64::MAX;
    }

    /// Say the counts and begin a new span, if one is due at `now`.
    fn report(&mut self, now: u64) {
        let span = now.saturating_sub(self.since);
        if span < STATS_EVERY_NS {
            return;
        }
        let calls: u64 = self.slots.iter().map(|slot| u64::from(slot.calls)).sum();
        let ns: u64 = self.slots.iter().map(|slot| slot.ns).sum();
        let span_ms = (span / 1_000_000).max(1);
        ferrix_rt::info!(
            "intel: ioctlstat {} ms: {} ioctls ({}/s), driver {} us in them ({} per mille of a core), loop busy {} per mille, {} copies ({} KiB), {} replies",
            span_ms,
            calls,
            calls * 1_000 / span_ms,
            ns / 1_000,
            ns / 1_000 / span_ms,
            self.busy_ns / 1_000 / span_ms,
            self.copies,
            self.copy_bytes / 1_024,
            self.replies,
        );
        for (number, slot) in self.slots.iter().enumerate() {
            if slot.calls == 0 {
                continue;
            }
            ferrix_rt::info!(
                "intel: ioctlstat {:#04x} {}: {} calls, {} held, avg {} ns ({} to the reply), max {} us",
                number,
                ioctl_name(number as u8),
                slot.calls,
                slot.held,
                slot.ns / u64::from(slot.calls),
                slot.reply_ns / u64::from(slot.calls),
                slot.max_ns / 1_000,
            );
        }
        *self = Self::new(now);
    }
}

/// An ioctl number's name (`drm.h`, `i915_drm.h`), for [`IoctlStats`].
fn ioctl_name(number: u8) -> &'static str {
    match number {
        0x00 => "VERSION",
        0x09 => "GEM_CLOSE",
        0x0c => "GET_CAP",
        0x2d => "PRIME_HANDLE_TO_FD",
        0x2e => "PRIME_FD_TO_HANDLE",
        0xbf => "SYNCOBJ_CREATE",
        0xc0 => "SYNCOBJ_DESTROY",
        0xc1 => "SYNCOBJ_HANDLE_TO_FD",
        0xc2 => "SYNCOBJ_FD_TO_HANDLE",
        0xc3 => "SYNCOBJ_WAIT",
        0xc4 => "SYNCOBJ_RESET",
        0xc5 => "SYNCOBJ_SIGNAL",
        0xca => "SYNCOBJ_TIMELINE_WAIT",
        0xcb => "SYNCOBJ_QUERY",
        0xcc => "SYNCOBJ_TRANSFER",
        0xcd => "SYNCOBJ_TIMELINE_SIGNAL",
        0xcf => "SYNCOBJ_EVENTFD",
        0xd1 => "SET_CLIENT_NAME",
        0x46 => "I915_GETPARAM",
        0x57 => "I915_GEM_BUSY",
        0x5b => "I915_GEM_CREATE",
        0x5c => "I915_GEM_PREAD",
        0x5d => "I915_GEM_PWRITE",
        0x5f => "I915_GEM_SET_DOMAIN",
        0x61 => "I915_GEM_SET_TILING",
        0x62 => "I915_GEM_GET_TILING",
        0x64 => "I915_GEM_MMAP_OFFSET",
        0x66 => "I915_GEM_MADVISE",
        0x69 => "I915_GEM_EXECBUFFER2",
        0x6c => "I915_GEM_WAIT",
        0x6d => "I915_GEM_CONTEXT_CREATE",
        0x6e => "I915_GEM_CONTEXT_DESTROY",
        0x6f => "I915_GEM_SET_CACHING",
        0x70 => "I915_GEM_GET_CACHING",
        0x71 => "I915_REG_READ",
        0x72 => "I915_GET_RESET_STATS",
        0x73 => "I915_GEM_USERPTR",
        0x74 => "I915_GEM_CONTEXT_GETPARAM",
        0x75 => "I915_GEM_CONTEXT_SETPARAM",
        0x79 => "I915_QUERY",
        0x7a => "I915_GEM_VM_CREATE",
        0x7b => "I915_GEM_VM_DESTROY",
        0x7c => "I915_GEM_CREATE_EXT",
        _ => "-",
    }
}

impl ChardevHost {
    /// Count the render node's ioctls ([`IoctlStats`]).
    #[allow(dead_code, reason = "the keeper and intel-fake do not count")]
    pub(crate) fn count_ioctls(&mut self) {
        self.stats = Some(IoctlStats::new(now_ns()));
        ferrix_rt::info!("intel: ioctl counts every 10 s (ferrix.intelgpu.ioctlstat)");
    }
}

/// `CLOCK_MONOTONIC` in nanoseconds.
fn now_ns() -> u64 {
    ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
}

impl<M: AsVmo> Host<M> for ChardevHost {
    fn copy_in(&mut self, request: u64, address: u64, out: &mut [u8]) -> Result<(), Errno> {
        if let Some(stats) = self.stats.as_mut() {
            stats.copies += 1;
            stats.copy_bytes += out.len() as u64;
        }
        chardev::copy_in(&self.control, request, address, out).map_err(|_| Errno::EFAULT)
    }

    fn copy_out(&mut self, request: u64, address: u64, bytes: &[u8]) -> Result<(), Errno> {
        if let Some(stats) = self.stats.as_mut() {
            stats.copies += 1;
            stats.copy_bytes += bytes.len() as u64;
        }
        chardev::copy_out(&self.control, request, address, bytes).map_err(|_| Errno::EFAULT)
    }

    fn reply(&mut self, request: u64, status: i32) {
        if let Some(stats) = self.stats.as_mut() {
            stats.replies += 1;
            if request == stats.current {
                stats.replied_at(now_ns());
            }
        }
        // A request the program abandoned answers BAD_STATE (C1, ledger
        // 2352): nothing waits for it any more, and that is fine.
        let _ = chardev::reply(&self.control, request, status, 0);
    }

    fn reply_map(
        &mut self,
        request: u64,
        memory: &M,
        offset: u64,
        write_combining: bool,
    ) -> Result<(), Errno> {
        let kind = if write_combining {
            MAP_VMO | MAP_WRITE_COMBINING
        } else {
            MAP_VMO
        };
        chardev::reply_map(&self.control, request, memory.vmo(), offset, kind).map_err(errno)
    }

    fn dmabuf_install(
        &mut self,
        request: u64,
        memory: &M,
        cookie: u64,
        writable: bool,
        cloexec: bool,
    ) -> Result<Installed, Errno> {
        let mut flags = 0;
        if writable {
            flags |= DMABUF_WRITABLE;
        }
        if cloexec {
            flags |= DMABUF_CLOEXEC;
        }
        chardev::dmabuf_install(&self.control, request, memory.vmo(), cookie, flags)
            .map(|installed| Installed {
                fd: installed.fd,
                made: installed.made,
            })
            .map_err(errno)
    }

    fn dmabuf_resolve(&mut self, request: u64, fd: i32) -> Result<u64, Errno> {
        chardev::dmabuf_resolve(&self.control, request, fd).map_err(errno)
    }

    fn dmabuf_fence(&mut self, dmabuf: u64, fence: u64) -> Result<(), Errno> {
        chardev::dmabuf_fence(&self.control, dmabuf, fence, 0).map_err(errno)
    }

    fn sync_install(&mut self, request: u64, fence: u64, cloexec: bool) -> Result<i32, Errno> {
        let flags = if cloexec { SYNC_CLOEXEC } else { 0 };
        chardev::sync_install(&self.control, request, fence, 0, flags).map_err(errno)
    }

    fn sync_resolve(&mut self, request: u64, fd: i32) -> Result<(u64, bool), Errno> {
        chardev::sync_resolve(&self.control, request, fd).map_err(|_| Errno::EINVAL)
    }

    fn sync_signal(&mut self, fence: u64, status: i32) {
        // A fence past its deadline was signalled with ETIMEDOUT by the
        // kernel already: BAD_STATE, and nothing to do.
        let _ = chardev::sync_signal(&self.control, fence, status);
    }

    fn now_ns(&self) -> u64 {
        ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
    }

    fn privileged(&self, _request: u64) -> bool {
        self.euid == 0
    }

    fn say(&mut self, level: Level, line: fmt::Arguments<'_>) {
        say(level, line);
    }
}

/// Ask for the device's chardev control and publish the render node.
///
/// # Errors
///
/// [`Stopped::Control`], [`Stopped::Hello`], or [`Stopped::Refused`] with
/// the kernel's reason said.
pub(crate) fn publish(device: &Device<Kernel>, location: u32) -> Result<ChardevHost, Stopped> {
    let control = device.chardev_control().map_err(|error| {
        ferrix_rt::err!("intel: no chardev control ({error:?})");
        Stopped::Control
    })?;
    let mut minors = [0; MAX_NODES];
    if let Some(first) = minors.first_mut() {
        *first = RENDER_MINOR;
    }
    let hello = Hello {
        // It takes `Op::Interrupt` ([`answer`]).
        version: VERSION_INTERRUPTS,
        location,
        count: 1,
        minors,
    };
    control
        .write(Message::Hello(hello).encode().as_bytes())
        .map_err(|_| Stopped::Hello)?;
    let _ = control
        .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
        .map_err(|_| Stopped::Hello)?;
    let mut bytes = [0_u8; message::MAX_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let received = control
        .read(&mut bytes, &mut handles)
        .map_err(|_| Stopped::Hello)?;
    match Message::decode(bytes.get(..received.bytes).unwrap_or(&[])) {
        Ok(Message::Ready(count)) => {
            ferrix_rt::info!("intel: render node published ({count} node)");
            Ok(ChardevHost {
                control,
                euid: 0,
                stats: None,
            })
        }
        Ok(Message::Refused(refusal)) => {
            ferrix_rt::err!("intel: render node refused: {refusal}");
            Err(Stopped::Refused)
        }
        _ => Err(Stopped::Hello),
    }
}

/// What the Intel driver's display card reaches of the render node while
/// both are served: the front end, for the card's own work on the GT, and
/// the host its services answer through.
#[allow(
    dead_code,
    reason = "only the Intel driver's card0 reads it; the keeper and intel-fake share this file"
)]
pub(crate) struct Served<'a, B: Backend> {
    /// The render node's host.
    pub(crate) host: &'a mut ChardevHost,
    /// The front end.
    pub(crate) frontend: &'a mut Frontend<B>,
}

/// The display card's part of the loop: a packet's key (`None` after any
/// other wake), the port, and what it reaches of the render node.
pub(crate) type Other<'o, B> =
    dyn FnMut(Option<u64>, &Port<Kernel>, Option<&mut Served<'_, B>>) + 'o;

/// Serve the render node until the kernel takes it back. `port` is the
/// loop's: the control's signals arrive on it with [`KEY_CONTROL`], the
/// back end's interrupt, if it bound one, with [`KEY_INTERRUPT`], and
/// every other key goes to `other` (the Intel driver's display card), which
/// is also called with `None` after every other wake, to see what the back
/// end finished of the card's own work.
///
/// # Errors
///
/// [`Stopped::Closed`] when the control channel closes, [`Stopped::Port`]
/// when the port fails.
pub(crate) fn serve<B: Backend>(
    host: &mut ChardevHost,
    port: &Port<Kernel>,
    frontend: &mut Frontend<B>,
    other: &mut Other<'_, B>,
) -> Result<core::convert::Infallible, Stopped>
where
    B::Memory: AsVmo,
{
    let arm = |host: &ChardevHost| {
        host.control
            .wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_CONTROL)
            .map_err(|_| Stopped::Port)
    };
    arm(host)?;
    loop {
        frontend.backend.flush();
        let deadline =
            with_wake(port, frontend.next_deadline()).map_or(Deadline::Never, Deadline::At);
        if let Some(stats) = host.stats.as_mut() {
            stats.asleep(now_ns());
        }
        let waited = port.wait(deadline);
        if let Some(stats) = host.stats.as_mut() {
            stats.awake_at = now_ns();
        }
        let what = match waited {
            Ok(packet) if packet.key == KEY_CONTROL => {
                if Signals(packet.signals).intersects(Signals::PEER_CLOSED) {
                    return Err(Stopped::Closed);
                }
                drain(host, frontend)?;
                arm(host)?;
                "requests"
            }
            Ok(packet) if packet.key == KEY_INTERRUPT => {
                frontend.service(host, true);
                "the interrupt"
            }
            Ok(packet) if packet.key == KEY_BACKEND => {
                frontend.service(host, false);
                "the back end"
            }
            Ok(packet) => {
                other(Some(packet.key), port, Some(&mut Served { host, frontend }));
                "card0"
            }
            Err(Error::TimedOut) => {
                frontend.service(host, false);
                "a deadline"
            }
            Err(_) => return Err(Stopped::Port),
        };
        if what != "card0" {
            other(None, port, Some(&mut Served { host, frontend }));
        }
        if let Some(stats) = host.stats.as_ref() {
            stats.awake_long(now_ns(), what);
        }
    }
}

/// Take every REQUEST queued on the control channel.
fn drain<B: Backend>(host: &mut ChardevHost, frontend: &mut Frontend<B>) -> Result<(), Stopped>
where
    B::Memory: AsVmo,
{
    let mut bytes = [0_u8; message::MAX_BYTES];
    let mut handles = [Handle::INVALID; 1];
    loop {
        let received = match host.control.read(&mut bytes, &mut handles) {
            Ok(received) => received,
            Err(ReadError::Failed(Error::ShouldWait)) => return Ok(()),
            Err(ReadError::Failed(Error::PeerClosed)) => return Err(Stopped::Closed),
            Err(_) => return Err(Stopped::Port),
        };
        match Message::decode(bytes.get(..received.bytes).unwrap_or(&[])) {
            Ok(Message::Request(request)) => answer(host, frontend, &request),
            _ => ferrix_rt::warn!("intel: a control message that is not a REQUEST"),
        }
    }
}

/// One REQUEST, to the front end.
fn answer<B: Backend>(host: &mut ChardevHost, frontend: &mut Frontend<B>, request: &Request)
where
    B::Memory: AsVmo,
{
    host.euid = request.euid;
    match request.op {
        Op::Open => frontend.open(host, request.id, request.file, request.pid),
        Op::Ioctl => {
            let began = host
                .stats
                .as_mut()
                .map(|stats| stats.begin(request.id, request.cmd));
            frontend.ioctl(host, request.id, request.file, request.cmd, request.arg);
            if let (Some(stats), Some(began)) = (host.stats.as_mut(), began) {
                stats.end(request.cmd, began);
            }
        }
        Op::Release => frontend.release(host, request.file),
        Op::Mmap => frontend.mmap(host, request.id, request.file, request.arg, request.pages),
        Op::DmabufRelease => frontend.dmabuf_release(host, request.arg),
        Op::Interrupt => frontend.interrupt(host, request.id),
        Op::DmabufImport => frontend.dmabuf_import(
            host,
            request.id,
            request.arg,
            request.cmd,
            request.pages,
            request.file,
        ),
    }
}
