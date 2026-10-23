//! `devmgr-events`: a driver's death and restart, published to programs
//! (`docs/DESKTOP.md` §4.2).
//!
//! `devmgr` sees every driver die and come back, and tells the kernel,
//! which prints it; nothing a program can read held it. This helper is how
//! `devmgr` tells programs instead, with no new kernel interface: `devmgr`
//! starts it after REPORT, as it starts `drvupdated`, with a channel whose
//! other end it keeps, and writes each event to that channel
//! (`ferrix_devmgr_events_proto::Event`). The helper listens on the
//! abstract `AF_UNIX` name `ferrix.devmgr.events` and writes each event as
//! one line to every connected reader, a reader that connects late getting
//! the last [`KEPT`] first. Readers only read: anything they send is never
//! read. Any uid may read, since a line says no more than the kernel's log
//! does.
//!
//! It never holds `devmgr`: `devmgr` writes and goes on, and a full channel
//! loses an event, not a death's handling. Nor does a reader hold it: one
//! that does not take its line at once is dropped.
//!
//! One loop waits on the listening socket and on a port, through the port's
//! descriptor, with `ppoll`.
//!
//! The exit status names the step that failed (see [`Step`]).

#![no_std]
#![no_main]

use ferrix_devmgr_events_proto::{EVENT_BYTES, Event, LINE_BYTES, SOCKET_NAME};
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::{Deadline, Error, Handle, Object, Signals};
use ferrix_rt::{Bootstrap, Kernel, linux};

ferrix_rt::entry!(main);

/// Where the helper gave up, as its exit status.
#[repr(i32)]
enum Step {
    /// Started with no channel to `devmgr`.
    NoBootstrap = 1,
    /// The socket could not be made, bound or listened on.
    Socket = 2,
    /// The port, its watch or its descriptor could not be made.
    Port = 3,
    /// `ppoll` failed.
    Poll = 4,
    /// `devmgr` closed its end.
    Closed = 5,
}

/// The port key `devmgr`'s channel is watched under.
const KEY_DEVMGR: u64 = 1;

/// Readers at once; the oldest is dropped for a new one past this.
const READERS: usize = 16;

/// Events kept for a reader that connects late.
const KEPT: usize = 32;

/// `POLLIN`.
const POLLIN: i16 = 1;

/// `MSG_DONTWAIT | MSG_NOSIGNAL`: a reader that cannot take a line at once
/// is dropped, and a gone one raises no `SIGPIPE`.
const SEND_FLAGS: usize = 0x40 | 0x4000;

/// One `struct pollfd`.
#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

/// The last [`KEPT`] lines, oldest first.
struct Kept {
    lines: [([u8; LINE_BYTES], usize); KEPT],
    next: usize,
    count: usize,
}

impl Kept {
    const fn new() -> Self {
        Self {
            lines: [([0; LINE_BYTES], 0); KEPT],
            next: 0,
            count: 0,
        }
    }

    fn keep(&mut self, line: [u8; LINE_BYTES], length: usize) {
        if let Some(slot) = self.lines.get_mut(self.next) {
            *slot = (line, length);
        }
        self.next = (self.next + 1) % KEPT;
        self.count = (self.count + 1).min(KEPT);
    }

    /// Each kept line, oldest first.
    fn each(&self, mut send: impl FnMut(&[u8]) -> bool) -> bool {
        let first = (self.next + KEPT - self.count) % KEPT;
        for offset in 0..self.count {
            let Some((line, length)) = self.lines.get((first + offset) % KEPT) else {
                continue;
            };
            if !send(line.get(..*length).unwrap_or_default()) {
                return false;
            }
        }
        true
    }
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(devmgr) = bootstrap else {
        return Step::NoBootstrap as i32;
    };
    let Some(listener) = listen() else {
        return Step::Socket as i32;
    };
    let Ok(port) = port::create(Kernel) else {
        return Step::Port as i32;
    };
    if watch(&devmgr, &port).is_err() {
        return Step::Port as i32;
    }
    let Ok(port_fd) = port.descriptor(true) else {
        return Step::Port as i32;
    };
    let mut readers: [Option<usize>; READERS] = [None; READERS];
    let mut kept = Kept::new();
    loop {
        let mut fds = [
            PollFd {
                fd: i32::try_from(listener).unwrap_or(-1),
                events: POLLIN,
                revents: 0,
            },
            PollFd {
                fd: port_fd,
                events: POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a live array of two `pollfd`s for the whole call;
        // no timeout and no signal mask.
        let polled = unsafe {
            linux::call(
                linux::numbers::PPOLL,
                [fds.as_mut_ptr() as usize, fds.len(), 0, 0, 8, 0],
            )
        };
        if polled.is_err() {
            return Step::Poll as i32;
        }
        let [socket, events] = fds;
        if socket.revents != 0 {
            accept_readers(listener, &mut readers, &kept);
        }
        if events.revents != 0 {
            let Ok(packet) = port.wait(Deadline::At(0)) else {
                continue;
            };
            if packet.key == KEY_DEVMGR {
                if !take_events(&devmgr, &mut readers, &mut kept) {
                    return Step::Closed as i32;
                }
                if watch(&devmgr, &port).is_err() {
                    return Step::Port as i32;
                }
            }
        }
    }
}

/// Watch `devmgr`'s channel on `port`.
fn watch(devmgr: &Channel<Kernel>, port: &Port<Kernel>) -> Result<(), Error> {
    devmgr.wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_DEVMGR)
}

/// Bind [`SOCKET_NAME`] and listen on it, without blocking.
fn listen() -> Option<usize> {
    let fd = linux::socket(linux::AF_UNIX, linux::SOCK_STREAM | linux::SOCK_NONBLOCK, 0).ok()?;
    let (address, len) = linux::sockaddr_un_abstract(SOCKET_NAME)?;
    let _bound = linux::bind(fd, &address, len).ok()?;
    let _listening = linux::listen(fd, READERS).ok()?;
    Some(fd)
}

/// Take every waiting reader, send each the kept lines, and keep it; the
/// oldest is dropped for a new one when there are [`READERS`].
fn accept_readers(listener: usize, readers: &mut [Option<usize>; READERS], kept: &Kept) {
    while let Ok(reader) = linux::accept4(listener, linux::SOCK_NONBLOCK) {
        if !kept.each(|line| send(reader, line)) {
            let _ = linux::close(reader);
            continue;
        }
        let slot = readers.iter().position(Option::is_none).unwrap_or_else(|| {
            if let Some(Some(oldest)) = readers.first().copied() {
                let _ = linux::close(oldest);
            }
            readers.rotate_left(1);
            READERS - 1
        });
        if let Some(place) = readers.get_mut(slot) {
            *place = Some(reader);
        }
    }
}

/// Read every event `devmgr` wrote, keep it, and send its line to every
/// reader, dropping one that cannot take it. `false` when `devmgr` closed
/// its end.
fn take_events(
    devmgr: &Channel<Kernel>,
    readers: &mut [Option<usize>; READERS],
    kept: &mut Kept,
) -> bool {
    loop {
        let mut bytes = [0_u8; EVENT_BYTES];
        let mut handles = [Handle::INVALID; 0];
        let event = match devmgr.read(&mut bytes, &mut handles) {
            Ok(got) => Event::decode(bytes.get(..got.bytes).unwrap_or_default()),
            Err(ReadError::Failed(Error::ShouldWait)) => return true,
            Err(_) => return false,
        };
        let Ok(event) = event else {
            continue;
        };
        let mut line = [0_u8; LINE_BYTES];
        let length = event.line(&mut line);
        kept.keep(line, length);
        for place in readers.iter_mut() {
            if let Some(reader) = *place
                && !send(reader, line.get(..length).unwrap_or_default())
            {
                let _ = linux::close(reader);
                *place = None;
            }
        }
    }
}

/// Send all of `line` to `reader` at once, or say it could not be.
fn send(reader: usize, line: &[u8]) -> bool {
    // SAFETY: `line` is live for the call; no address.
    let sent = unsafe {
        linux::call(
            linux::numbers::SENDTO,
            [reader, line.as_ptr() as usize, line.len(), SEND_FLAGS, 0, 0],
        )
    };
    sent == Ok(line.len())
}
