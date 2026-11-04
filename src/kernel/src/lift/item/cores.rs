//! The device cores' end of devmgr (docs/microkernel/M5-LIFT.md §11): under
//! `personality=user` the cores are linuxd's, and devmgr opens them over the
//! channel the kernel gave both, with the same request a driver's call made of the
//! hybrid kernel, and the device named by location alone. No device authority
//! reaches linuxd: it is told where, and holds a handle with `TRANSFER` at most.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ferrix_devmgr_proto::{CORE_BYTES, CoreOpen, CoreOpened, Message};
use ferrix_linux_abi::errno::Errno;
use ferrix_native::channel::Channel;
use ferrix_native::handle::{Deadline, Object as _, OwnedHandle};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr::NativeCall;
use ferrix_native_abi::signals::Signals;
use ferrix_rt::Kernel;

use crate::device::{self, DeviceNode};
use crate::object::channel::Endpoint;
use crate::object::process::Host;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::native::Handler;

static HANDLERS: SpinLock<Vec<(u32, Handler)>> = SpinLock::new(Vec::new());
static NODES: SpinLock<Vec<Arc<DeviceNode>>> = SpinLock::new(Vec::new());
/// The driver's end of the channel being opened, between `hand_over` and the reply.
static HANDED: SpinLock<Option<Handle>> = SpinLock::new(None);
static MESSAGES: AtomicU32 = AtomicU32::new(0);
static REPORTED: AtomicBool = AtomicBool::new(false);
static REPORTED_QUEUE: WaitQueue = WaitQueue::new();
/// How long a restart waits out a dead driver's claim.
const STILL_CLAIMED_NANOS: u64 = 2_000_000_000;

pub fn serve(call: NativeCall, handler: Handler) {
    HANDLERS
        .lock()
        .push((ferrix_native_abi::nr::number(call) as u32, handler));
}

static SERVERS: SpinLock<Vec<&'static crate::syscall::native::Server>> = SpinLock::new(Vec::new());
/// A core's quiesce, which the kernel's `DEVICE_QUIESCE` runs in the hybrid.
pub fn note_server(server: &'static crate::syscall::native::Server) {
    SERVERS.lock().push(server);
}
/// A driver's next start finds the device claimed: what the hybrid's quiesce does
/// before it, for each core no driver of it is still served by.
fn quiesced(location: u32) {
    let Some(node) = node_at(location) else {
        return;
    };
    let servers: Vec<_> = SERVERS.lock().iter().copied().collect();
    for server in servers {
        if (server.wait_until_unserved)(&node, &|| true).is_ok()
            && let Some(release) = server.release
        {
            release(&node);
        }
    }
}

/// A native call that exited to linuxd, by its handler; `None` if it is no call
/// a handler serves, a Linux number.
pub fn answer_native(args: &crate::trap::SyscallArgs) -> Option<isize> {
    let number = u32::try_from(args.number).ok()?;
    let handler = HANDLERS
        .lock()
        .iter()
        .find(|(call, _)| *call == number)
        .map(|(_, h)| *h)?;
    let task = crate::sched::current()?;
    let caller = task.thread()?.process();
    Some(match handler(caller, &args.args) {
        Ok(value) => value as isize,
        Err(errno) => errno.as_return_value(),
    })
}

fn location_from(raw: u32) -> Option<device::Location> {
    let location = ferrix_blkring::Location(raw);
    ferrix_pci::Address::new(
        location.segment(),
        location.bus(),
        location.device(),
        location.function(),
    )
    .map(device::Location::Pci)
}

/// One node per place, so a core's claims compare as they do in the kernel.
fn node_at(raw: u32) -> Option<Arc<DeviceNode>> {
    let mut nodes = NODES.lock();
    let location = location_from(raw)?;
    if let Some(known) = device::devices()
        .iter()
        .find(|node| node.location() == location)
    {
        return Some(Arc::clone(known));
    }
    if let Some(known) = nodes.iter().find(|node| node.location() == location) {
        return Some(Arc::clone(known));
    }
    let node = Arc::new(DeviceNode::empty(location));
    nodes.push(Arc::clone(&node));
    Some(node)
}

/// The node `device` names: `serve_cores` passes the raw location as the handle.
pub fn node_of(device: u64) -> Option<Arc<DeviceNode>> {
    node_at(u32::try_from(device).ok()?)
}

/// `native::control_channel`'s last step: the driver's end across the edge.
pub fn hand_over(driver_end: Arc<Endpoint>) -> Result<usize, Errno> {
    let handle = ferrix_kernel_core::bridge::export(
        crate::object::Object::Channel(driver_end),
        ferrix_native_abi::rights::Rights::CHANNEL,
    )
    .ok_or(Errno::ENOMEM)?;
    *HANDED.lock() = Some(handle);
    Ok(handle.0 as usize)
}

/// Serve `cores` and `messages` until devmgr ends: two threads.
pub fn start(cores: u32, messages: u32) {
    MESSAGES.store(messages, Ordering::Release);
    if cores != 0 && crate::sched::spawn("cores", serve_cores, cores as usize, 0).is_err() {
        crate::println!("linuxd: no thread for the cores");
    }
    if messages != 0 && crate::sched::spawn("devmgr", serve_messages, messages as usize, 0).is_err()
    {
        crate::println!("linuxd: no thread for devmgr's messages");
    }
}

fn serve_cores(handle: usize) {
    let channel = core::mem::ManuallyDrop::new(Channel::from_owned(OwnedHandle::from_raw(
        Kernel,
        Handle(handle as u32),
    )));
    let mut bytes = [0_u8; CORE_BYTES];
    let mut handles = [Handle::INVALID; 1];
    loop {
        if channel
            .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
            .is_err()
        {
            return;
        }
        let Ok(got) = channel.read(&mut bytes, &mut handles) else {
            return;
        };
        // What devmgr told us the device is by: nothing is done with it.
        if got.handles > 0 {
            crate::sys::handle_close(handles[0].0);
        }
        let Some(open) = CoreOpen::decode(bytes.get(..got.bytes).unwrap_or(&[])) else {
            continue;
        };
        let handler = HANDLERS
            .lock()
            .iter()
            .find(|(call, _)| *call == open.call)
            .map(|(_, h)| *h);
        let answered = match handler {
            Some(handler) => {
                let registers = [u64::from(open.location), 0, 0, 0, 0, 0];
                // A dead driver's claim goes when its bridge has seen the channel
                // close, which the kernel's quiesce does not wait for here: a restart
                // may come first. A bound, inside devmgr's own patience.
                let until = crate::timer::now_nanos().saturating_add(STILL_CLAIMED_NANOS);
                loop {
                    let answer = handler(no_caller(), &registers);
                    match answer {
                        Err(ferrix_native_abi::status::ALREADY_BOUND)
                            if crate::timer::now_nanos() < until =>
                        {
                            quiesced(open.location);
                            crate::sched::sleep_for(10_000_000);
                        }
                        other => break other.map_err(|errno| u32::from(errno.0)),
                    }
                }
            }
            None => Err(u32::from(ferrix_linux_abi::errno::Errno::ENOSYS.0)),
        };
        let status = answered.err().unwrap_or(0);
        let handed = HANDED.lock().take();
        let reply = CoreOpened { status }.encode();
        let sent = match handed {
            Some(end) if status == 0 => {
                channel.write_slots(&reply, &mut [Some(OwnedHandle::from_raw(Kernel, end))])
            }
            _ => channel.write(&reply),
        };
        if sent.is_err() {
            return;
        }
    }
}

/// The caller a core's entry is given: nobody, since it asks nothing of it here.
fn no_caller() -> &'static dyn Host {
    struct Nobody;
    impl core::fmt::Debug for Nobody {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("devmgr")
        }
    }
    impl Host for Nobody {
        fn core(&self) -> &crate::object::process::Process {
            panic!("linuxd: a core asked a caller's process of devmgr's open")
        }
        fn core_arc(&self) -> &Arc<crate::object::process::Process> {
            panic!("linuxd: a core asked a caller's process of devmgr's open")
        }
        fn kill(&self, _status: i32) {}
        fn thread_starting(&self) {}
        fn thread_gone(&self, _ended: bool) {}
        fn wait_interrupted(&self) -> bool {
            false
        }
    }
    static NOBODY: Nobody = Nobody;
    &NOBODY
}

fn serve_messages(handle: usize) {
    let channel = core::mem::ManuallyDrop::new(Channel::from_owned(OwnedHandle::from_raw(
        Kernel,
        Handle(handle as u32),
    )));
    let mut bytes = [0_u8; 4096];
    let mut handles = [Handle::INVALID; 8];
    loop {
        if channel
            .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
            .is_err()
        {
            return;
        }
        let Ok(got) = channel.read(&mut bytes, &mut handles) else {
            crate::println!("  devmgr   its channel closed");
            return;
        };
        match Message::decode(bytes.get(..got.bytes).unwrap_or(&[])) {
            Ok(Message::Report { started, failed }) => {
                crate::println!("  devmgr   {started} started, {failed} failed");
                REPORTED.store(true, Ordering::Release);
                REPORTED_QUEUE.wake_all();
            }
            Ok(Message::Died { location, status }) => crate::println!(
                "  devmgr   the driver of {location:#010x} ended with status {status}; the device is quiesced"
            ),
            Ok(Message::Restarted { location, restarts }) => crate::println!(
                "  devmgr   the driver of {location:#010x} was started again and published (restart {restarts})"
            ),
            Ok(other) => {
                let _ = crate::discovery::devmgr::record(other);
            }
            Err(_) => {}
        }
    }
}

/// Bytes on devmgr's channel.
pub fn tell_devmgr(bytes: &[u8]) -> bool {
    let handle = MESSAGES.load(Ordering::Acquire);
    if handle == 0 {
        return false;
    }
    let channel = core::mem::ManuallyDrop::new(Channel::from_owned(OwnedHandle::from_raw(
        Kernel,
        Handle(handle),
    )));
    channel.write(bytes).is_ok()
}

pub fn wait_reported(deadline: u64) -> bool {
    REPORTED_QUEUE.wait_until_deadline(|| REPORTED.load(Ordering::Acquire), deadline)
}

/// A disk or a card a driver serves is up: devmgr may go on.
pub fn published(location: u32) {
    let handle = MESSAGES.load(Ordering::Acquire);
    if handle == 0 {
        return;
    }
    let channel = core::mem::ManuallyDrop::new(Channel::from_owned(OwnedHandle::from_raw(
        Kernel,
        Handle(handle),
    )));
    let _ = channel.write(&Message::Published { location }.encode());
}
