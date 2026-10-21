//! The log core: the kernel log, read by a ring-3 driver over a control
//! channel.
//!
//! The kernel log (`console::log`) keeps every byte the console sends. On a
//! Pixel 7 booted natively the console is a record in RAM that Android reads
//! back after the run; the phone's USB serial port, driven from ring 3 by
//! `usbdev`, is how the boot's lines and `ferrix-statd`'s output reach the
//! host while it runs (`docs/vendor/google/pixel7/USB-HANDOVER.md`, phase 4). So the driver
//! asks for the log with `log_control_create` on its device, and this module
//! answers its READs with DATA from the log, `src/lib/proto/logctl`'s protocol.
//!
//! # Who may read
//!
//! The log holds every program's console output, which is the program's own,
//! so reading it is a capability, as `syslog(2)` makes it a privilege: a
//! driver holding `MANAGE` on a device whose binding carries the log off the
//! machine for its owner (`DeviceNode::reads_log`, only the Pixel 7's USB
//! device controller), and one at a time. The claim is this module's, not the
//! device's: there is one log, and two readers would each see half of it
//! only if they shared a cursor, which nothing asks for. It ends when the
//! channel closes, which the driver's process ending does, or when the driver
//! breaks the protocol.
//!
//! # Never woken by the console
//!
//! Recording a byte is all the console asks of the log, from any context, so
//! nothing here is woken when a line is printed. A task per claim serves the
//! channel and, while a READ is outstanding and the log has nothing new,
//! looks again every [`POLL_NANOS`]. The reader's cursor starts at the log's
//! beginning, so its first DATA is the oldest byte still kept, with what the
//! ring had already lost counted in it: a driver that starts late still gets
//! the boot.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_linux_abi::errno::Errno;
use ferrix_logctl::message::{DATA_HEADER_BYTES, MAX_BYTES, Message, REFUSED_BYTES, Refusal};
use ferrix_logctl::session::Session;
use ferrix_native_abi::nr::NativeCall;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;
use ferrix_native_abi::types::CHANNEL_MAX_HANDLES;

use crate::console::log;
use crate::hooks::Full;
use crate::object::Transfer;
use crate::object::channel::{ChannelMessage, Endpoint, ReadError, Sender};
use crate::object::process::Host;
use crate::sched;
use crate::sync::SpinLock;
use crate::syscall::native;
use crate::timer;

pub(crate) mod check;

/// How often a READ waiting for the log looks at it again: often enough that
/// a line reaches the host while it is still the newest, and seldom enough to
/// cost nothing between lines.
pub(crate) const POLL_NANOS: u64 = 15_000_000;

/// How long the task sleeps with no READ outstanding before looking at the
/// channel of its own accord.
const IDLE_NANOS: u64 = 1_000_000_000;

/// Whether a reader holds the log.
static CLAIMED: AtomicBool = AtomicBool::new(false);

/// The kernel's end of a claim's channel, waiting for its task.
static STARTING: SpinLock<Option<Arc<Endpoint>>> = SpinLock::new(None);

/// Why a claim could not be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CreateError {
    /// Another reader holds the log.
    InUse,
    /// No memory for the channel, or no stack for the task.
    NoMemory,
}

/// Answer `log_control_create` with the log core.
///
/// Called once from `main.rs`'s `register_load`: the native ABI is the
/// item's and names no subsystem above it, so this registers into it.
///
/// # Errors
///
/// [`Full`] when the item has no room for the registration.
pub(crate) fn install() -> Result<(), Full> {
    native::serve(NativeCall::LogControlCreate, control_create)
}

/// `log_control_create`: the device handle and its `MANAGE` right are the
/// item's to check (`native::control_channel`), the binding and the claim
/// this module's.
fn control_create(caller: &dyn Host, registers: &[u64; 6]) -> Result<usize, Errno> {
    let device = registers.first().copied().unwrap_or(0);
    native::control_channel(
        caller,
        device,
        ferrix_blkring::control::CONTROL_RIGHTS,
        |node| {
            if !node.reads_log() {
                return Err(status::ACCESS_DENIED);
            }
            claim().map_err(|why| match why {
                CreateError::InUse => status::ALREADY_BOUND,
                CreateError::NoMemory => status::NO_MEMORY,
            })
        },
    )
}

/// Claim the log for a reader, start the task that serves it, and answer the
/// reader's end of the channel.
///
/// # Errors
///
/// [`CreateError::InUse`] while another reader holds the log, and
/// [`CreateError::NoMemory`] when the channel or the task could not be made.
pub(crate) fn claim() -> Result<Arc<Endpoint>, CreateError> {
    let (kernel_end, driver_end) = Endpoint::pair().map_err(|_| CreateError::NoMemory)?;
    if CLAIMED.swap(true, Ordering::AcqRel) {
        return Err(CreateError::InUse);
    }
    *STARTING.lock() = Some(kernel_end);
    if sched::spawn("logctl", run, 0, ferrix_sched::NICE_0_WEIGHT).is_err() {
        let _ = STARTING.lock().take();
        CLAIMED.store(false, Ordering::Release);
        return Err(CreateError::NoMemory);
    }
    Ok(driver_end)
}

/// Whether a reader holds the log.
pub(crate) fn claimed() -> bool {
    CLAIMED.load(Ordering::Acquire)
}

/// A claim's task: serve the channel until it closes or the driver breaks the
/// protocol, then let the log go.
fn run(_argument: usize) {
    let control = STARTING.lock().take();
    if let Some(control) = control {
        let carried = serve(&control);
        crate::console::println!(
            "  logctl   the kernel log's reader went, {carried} bytes of it carried"
        );
    }
    CLAIMED.store(false, Ordering::Release);
}

/// One reader: its conversation, and its place in the log.
struct Reader {
    session: Session,
    cursor: u64,
    carried: u64,
}

/// What looking at the log for an outstanding READ came to.
enum Answer {
    /// DATA went out.
    Sent,
    /// Nothing to send yet, or no READ to send it for.
    Nothing,
    /// The channel would not take it: the reader is gone.
    Failed,
}

/// Serve one reader until it goes, and answer how many bytes it was sent.
fn serve(control: &Endpoint) -> u64 {
    let mut reader = Reader {
        session: Session::new(),
        cursor: 0,
        carried: 0,
    };
    loop {
        match control.read(MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
            Ok(message) => {
                if reader.take(control, message).is_err() {
                    break;
                }
                continue;
            }
            Err(ReadError::Empty) => {}
            Err(ReadError::TooSmall { .. } | ReadError::NeedsTopology) => {
                refuse(control, Refusal::Protocol);
                break;
            }
            Err(_) => break,
        }
        match reader.answer(control) {
            Answer::Sent => continue,
            Answer::Nothing => {}
            Answer::Failed => break,
        }
        if !wait(control, reader.session.wanted().is_some()) {
            break;
        }
    }
    reader.carried
}

/// Wait for the driver, or for the log if a READ is outstanding. Answers
/// whether the driver is still there.
fn wait(control: &Endpoint, reading: bool) -> bool {
    let signals = control.signals();
    if signals.intersects(Signals::PEER_CLOSED) && !signals.intersects(Signals::READABLE) {
        return false;
    }
    let patience = if reading { POLL_NANOS } else { IDLE_NANOS };
    let deadline = timer::now_nanos().saturating_add(patience);
    let _ = control.waiters().wait_until_deadline(
        || {
            control
                .signals()
                .intersects(Signals::READABLE | Signals::PEER_CLOSED)
        },
        deadline,
    );
    true
}

impl Reader {
    /// Take one message from the driver. Refused, and the claim ended, for
    /// anything but a READ when none is outstanding.
    fn take(&mut self, control: &Endpoint, message: ChannelMessage) -> Result<(), Refusal> {
        let carried_handles = !message.handles.is_empty();
        crate::object::dispose(message.handles.into_iter().map(|(object, _)| object));
        let received = match Message::decode(&message.bytes) {
            Ok(decoded) if !carried_handles => self.session.receive(&decoded),
            _ => Err(Refusal::Protocol),
        };
        if let Err(refusal) = received {
            refuse(control, refusal);
        }
        received
    }

    /// Answer the outstanding READ from the log, if it has a byte for it.
    fn answer(&mut self, control: &Endpoint) -> Answer {
        let Some(room) = self.session.wanted() else {
            return Answer::Nothing;
        };
        let mut data = vec![0u8; DATA_HEADER_BYTES.saturating_add(room)];
        let read = log::read(
            &mut self.cursor,
            data.get_mut(DATA_HEADER_BYTES..).unwrap_or_default(),
        );
        let Some(lost) = self.session.read(read.copied, read.lost) else {
            return Answer::Nothing;
        };
        data.truncate(DATA_HEADER_BYTES.saturating_add(read.copied));
        if ferrix_logctl::message::data_header(&mut data, lost, read.copied).is_err() {
            return Answer::Failed;
        }
        match control.write(Sender::KERNEL, data, 0, || {
            Ok::<Vec<Transfer>, Infallible>(Vec::new())
        }) {
            Ok(()) => {
                self.carried = self.carried.saturating_add(read.copied as u64);
                Answer::Sent
            }
            Err(_) => Answer::Failed,
        }
    }
}

/// Tell the driver why the claim is ending.
fn refuse(control: &Endpoint, refusal: Refusal) {
    let mut bytes = [0u8; REFUSED_BYTES];
    if Message::Refused(refusal).encode_into(&mut bytes).is_ok() {
        let _ = control.write(Sender::KERNEL, bytes.to_vec(), 0, || {
            Ok::<Vec<Transfer>, Infallible>(Vec::new())
        });
    }
}
