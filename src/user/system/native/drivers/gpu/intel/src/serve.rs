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

use core::fmt::{self, Write as _};

use ferrix_chardevctl::message::{
    self, Hello, MAP_VMO, MAP_WRITE_COMBINING, MAX_NODES, Message, Op, Request, VERSION,
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

/// A line on standard error, which a native process has open on the
/// console.
pub(crate) fn say(arguments: fmt::Arguments<'_>) {
    let mut line = Line::default();
    let _ = line.write_fmt(arguments);
    let _ = line.write_str("\n");
    let _ = ferrix_rt::linux::write(2, line.as_bytes());
}

/// One console line.
struct Line {
    bytes: [u8; 256],
    len: usize,
}

impl Default for Line {
    fn default() -> Self {
        Line {
            bytes: [0; 256],
            len: 0,
        }
    }
}

impl Line {
    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for &byte in text.as_bytes() {
            if let Some(slot) = self.bytes.get_mut(self.len) {
                *slot = byte;
                self.len += 1;
            }
        }
        Ok(())
    }
}

/// A back end's object memory, as the chardev core needs it: the VMO that
/// mmaps and dmabufs hand out.
pub(crate) trait AsVmo {
    /// The VMO.
    fn vmo(&self) -> &Vmo<Kernel>;
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
}

impl<M: AsVmo> Host<M> for ChardevHost {
    fn copy_in(&mut self, request: u64, address: u64, out: &mut [u8]) -> Result<(), Errno> {
        chardev::copy_in(&self.control, request, address, out).map_err(|_| Errno::EFAULT)
    }

    fn copy_out(&mut self, request: u64, address: u64, bytes: &[u8]) -> Result<(), Errno> {
        chardev::copy_out(&self.control, request, address, bytes).map_err(|_| Errno::EFAULT)
    }

    fn reply(&mut self, request: u64, status: i32) {
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

    fn say(&mut self, line: fmt::Arguments<'_>) {
        say(line);
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
        say(format_args!("intel: no chardev control ({error:?})"));
        Stopped::Control
    })?;
    let mut minors = [0; MAX_NODES];
    if let Some(first) = minors.first_mut() {
        *first = RENDER_MINOR;
    }
    let hello = Hello {
        version: VERSION,
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
            say(format_args!("intel: render node published ({count} node)"));
            Ok(ChardevHost { control, euid: 0 })
        }
        Ok(Message::Refused(refusal)) => {
            say(format_args!("intel: render node refused: {refusal}"));
            Err(Stopped::Refused)
        }
        _ => Err(Stopped::Hello),
    }
}

/// Serve the render node until the kernel takes it back. `port` is the
/// loop's: the control's signals arrive on it with [`KEY_CONTROL`], the
/// back end's interrupt, if it bound one, with [`KEY_INTERRUPT`], and
/// every other key goes to `other` (the Intel driver's display card).
///
/// # Errors
///
/// [`Stopped::Closed`] when the control channel closes, [`Stopped::Port`]
/// when the port fails.
pub(crate) fn serve<B: Backend>(
    host: &mut ChardevHost,
    port: &Port<Kernel>,
    frontend: &mut Frontend<B>,
    other: &mut dyn FnMut(u64, &Port<Kernel>),
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
        match port.wait(deadline) {
            Ok(packet) if packet.key == KEY_CONTROL => {
                if Signals(packet.signals).intersects(Signals::PEER_CLOSED) {
                    return Err(Stopped::Closed);
                }
                drain(host, frontend)?;
                arm(host)?;
            }
            Ok(packet) if packet.key == KEY_INTERRUPT => frontend.service(host, true),
            Ok(packet) if packet.key == KEY_BACKEND => frontend.service(host, false),
            Ok(packet) => other(packet.key, port),
            Err(Error::TimedOut) => frontend.service(host, false),
            Err(_) => return Err(Stopped::Port),
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
            _ => say(format_args!(
                "intel: a control message that is not a REQUEST"
            )),
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
        Op::Ioctl => frontend.ioctl(host, request.id, request.file, request.cmd, request.arg),
        Op::Release => frontend.release(host, request.file),
        Op::Mmap => frontend.mmap(host, request.id, request.file, request.arg, request.pages),
        Op::DmabufRelease => frontend.dmabuf_release(host, request.arg),
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
