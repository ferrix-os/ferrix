//! An `AF_NETLINK` socket, as the inode its open file reads and writes
//! through.
//!
//! A netlink socket is not a socket in the net core: nothing about it is on a
//! wire. It is a request channel into the kernel's own tables, and what is
//! here is the shell around that — the inode a descriptor points at, the port
//! identifier the kernel hands out, the queue of replies waiting to be read,
//! and the waiting a blocking receive does.
//!
//! # A send is answered before it returns
//!
//! `sendmsg` hands over a buffer of requests. This module walks them, answers
//! each from the net core, and queues the replies on the socket that asked,
//! all before the call returns; `recvmsg` then takes them one message at a
//! time. That is what makes a netlink exchange work for a single-threaded
//! program that sends and then reads with no timeout, and it is what Linux
//! does for `NETLINK_ROUTE`, where every answer is synchronous.
//!
//! # `NETLINK_KOBJECT_UEVENT` hears nothing yet
//!
//! A uevent socket opens, binds to its groups and waits, and nothing is ever
//! queued on it: this kernel makes its devices at boot and broadcasts no
//! event for them, so a listener hears what it would hear on Linux between
//! hotplugs. That is enough for libudev, whose monitor otherwise prints a
//! line for every socket it fails to make, and Steam's client makes one
//! over and over. A send is refused without `CAP_NET_ADMIN`, as Linux
//! refuses a send to a group from an unprivileged socket, and is taken and
//! dropped with it: no socket here hears another's broadcast.
//!
//! # One message per receive
//!
//! Each reply is queued as a datagram of its own rather than packed with its
//! siblings into one. A netlink datagram that does not fit the buffer offered
//! is truncated and the rest of it dropped — `MSG_TRUNC` says how much was
//! lost — so packing a whole dump into one datagram would turn a reader with a
//! small buffer into a reader that silently loses interfaces. One message per
//! datagram cannot lose more than the message a reader had no room for.
//!
//! # Nothing is built inside the net core's lock
//!
//! The reply buffer is allocated before the lock is taken and the replies are
//! copied out of it afterwards; inside the lock there is nothing but reading
//! the stack's tables and writing bytes, which is the rule
//! [`crate::net::NetCore::with`] states.

pub(crate) mod check;
pub(crate) mod link;
mod route;

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicU32, Ordering};

use ferrix_kmem::{Charge, arc_footprint, buffer_footprint, reserve_deque};
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::netlink::{
    NETLINK_ADD_MEMBERSHIP, NETLINK_DROP_MEMBERSHIP, NETLINK_KOBJECT_UEVENT, NETLINK_ROUTE,
    NetlinkAddress, NlMsgHdr, SOL_NETLINK,
};
use ferrix_linux_abi::socket::{
    AF_NETLINK, MSG_DONTWAIT, MSG_PEEK, SO_DOMAIN, SO_ERROR, SO_PROTOCOL, SO_RCVBUF,
    SO_RCVTIMEO_NEW, SO_RCVTIMEO_OLD, SO_SNDBUF, SO_SNDTIMEO_NEW, SO_SNDTIMEO_OLD, SO_TYPE,
    SOCKET_BUFFER_DEFAULT, SOL_SOCKET, Width,
};
use ferrix_netlink::Messages;
use ferrix_vfs::{Inode, Metadata, OpenFile, Readiness};

use super::socket::Received;
use crate::fs;
use crate::net::NetNamespace;
use crate::sync::SpinLock;

/// The most one send answers with.
///
/// A dump of every link, address, route and neighbour this host has is a few
/// kilobytes; this is room for two orders of magnitude more, and a dump that
/// fills it ends where it is with its `NLMSG_DONE` rather than running on.
const MAX_REPLY: usize = 32 * 1024;

/// How many bytes of replies wait on one socket before a send is refused.
///
/// A program that asks and never reads must not be able to grow the kernel's
/// heap by asking again, which is what a queue with no ceiling would let it
/// do. Linux's answer to the same situation is `ENOBUFS`, and so is this one.
const MAX_QUEUED: usize = 256 * 1024;

/// The next port identifier to hand out.
///
/// Linux gives an auto-bound socket its thread's identifier and falls back to
/// a free number when that is taken. A counter is used here instead, because
/// the identifier's only job is to be different from every other socket's --
/// a program compares the one in a reply with the one `getsockname` gave it --
/// and a process identifier that is reused the moment a process exits would be
/// the one thing it must not be.
static NEXT_PORT: AtomicU32 = AtomicU32::new(1);

/// The netlink protocols a socket may be opened for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Protocol {
    /// `NETLINK_ROUTE`: requests about links, addresses and routes.
    Route,
    /// `NETLINK_KOBJECT_UEVENT`: device events, of which there are none yet.
    Uevent,
}

impl Protocol {
    /// The protocol `socket`'s third argument names, if this kernel has it.
    pub(crate) const fn from_linux(protocol: i32) -> Option<Protocol> {
        match protocol {
            NETLINK_ROUTE => Some(Protocol::Route),
            NETLINK_KOBJECT_UEVENT => Some(Protocol::Uevent),
            _ => None,
        }
    }

    /// Its number, as `SO_PROTOCOL` reports it.
    const fn to_linux(self) -> i32 {
        match self {
            Protocol::Route => NETLINK_ROUTE,
            Protocol::Uevent => NETLINK_KOBJECT_UEVENT,
        }
    }
}

/// What a program has set that nothing else keeps.
#[derive(Clone, Copy, Debug)]
struct Options {
    /// `SO_RCVTIMEO`, in nanoseconds; zero waits forever.
    receive_timeout: u64,
    /// `SO_SNDTIMEO`, likewise.
    send_timeout: u64,
    /// `SO_SNDBUF` as set, reported back.
    send_buffer: usize,
    /// `SO_RCVBUF` as set, reported back.
    receive_buffer: usize,
}

/// Everything about the socket that changes.
#[derive(Debug)]
struct State {
    /// `nl_pid`: the port identifier, zero until the socket is bound.
    port: u32,
    /// `nl_groups`: the multicast groups it asked to join. Nothing multicasts
    /// yet; they are kept so `getsockname` reports what was bound.
    groups: u32,
    /// Replies waiting to be read, one message each.
    queue: VecDeque<Vec<u8>>,
    /// How many bytes those replies hold.
    queued: usize,
    /// What a program has set.
    options: Options,
    /// The room `queue` holds, charged to the job that opened the socket
    /// (certification finding F-37).
    room: Charge,
    /// The replies in it, likewise.
    replies: Charge,
}

/// An `AF_NETLINK` socket.
pub(crate) struct NetlinkSocket {
    /// The network namespace it was made in, for life: the tables its
    /// requests read and change.
    ns: Arc<NetNamespace>,
    /// `SOCK_DGRAM` or `SOCK_RAW`, as `SO_TYPE` reports it. Netlink treats
    /// them the same and so does this.
    kind: u32,
    /// What it was opened for.
    protocol: Protocol,
    /// What `stat` reports through it.
    metadata: Metadata,
    /// Everything that changes.
    state: SpinLock<State>,
    /// Its own heap, charged to the job that opened it.
    _charge: Charge,
}

impl fmt::Debug for NetlinkSocket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NetlinkSocket")
            .field("ino", &self.metadata.ino)
            .field("port", &self.state.lock().port)
            .finish_non_exhaustive()
    }
}

impl NetlinkSocket {
    /// Open a socket of `kind` for `protocol`.
    ///
    /// # Errors
    ///
    /// Whatever [`OpenFile::new`] refuses, which for a socket is nothing.
    pub(crate) fn open(
        ns: &Arc<NetNamespace>,
        kind: u32,
        protocol: Protocol,
        nonblock: bool,
        owner: (u32, u32),
    ) -> Result<Arc<OpenFile>, Errno> {
        let refused = |_| Errno::ENOMEM;
        let charge = Charge::bytes(arc_footprint::<NetlinkSocket>()).map_err(refused)?;
        // Nothing is never refused; these grow with the replies.
        let (room, replies) = (
            Charge::bytes(0).map_err(refused)?,
            Charge::bytes(0).map_err(refused)?,
        );
        let ino = fs::socket::next_ino();
        let socket = Arc::new(NetlinkSocket {
            ns: Arc::clone(ns),
            kind,
            protocol,
            metadata: fs::socket::socket_metadata(ino, owner),
            state: SpinLock::new(State {
                port: 0,
                groups: 0,
                queue: VecDeque::new(),
                queued: 0,
                options: Options {
                    receive_timeout: 0,
                    send_timeout: 0,
                    send_buffer: SOCKET_BUFFER_DEFAULT,
                    receive_buffer: SOCKET_BUFFER_DEFAULT,
                },
                room,
                replies,
            }),
            _charge: charge,
        });
        fs::socket::open_on_sockfs(socket, ino, nonblock)
    }

    /// The network namespace it was made in.
    pub(crate) const fn namespace(&self) -> &Arc<NetNamespace> {
        &self.ns
    }

    /// Give it a port identifier and the groups it asked for.
    ///
    /// A `nl_pid` of zero asks the kernel to choose, which is what every
    /// program does; one that names a number keeps it, and a second bind that
    /// names a different one is `EINVAL`, as Linux answers.
    pub(crate) fn bind(&self, raw: &[u8]) -> Result<(), Errno> {
        let address = NetlinkAddress::parse(raw, raw.len()).map_err(|_| Errno::EINVAL)?;
        let mut state = self.state.lock();
        if state.port != 0 && address.pid != 0 && address.pid != state.port {
            return Err(Errno::EINVAL);
        }
        if state.port == 0 {
            state.port = if address.pid == 0 {
                next_port()
            } else {
                address.pid
            };
        }
        state.groups = address.groups;
        Ok(())
    }

    /// The address it is bound to, as a `sockaddr_nl`.
    pub(crate) fn local_name(&self) -> Vec<u8> {
        let state = self.state.lock();
        NetlinkAddress {
            pid: state.port,
            groups: state.groups,
        }
        .to_bytes()
        .to_vec()
    }

    /// What it can do right now: read when a reply is waiting, and write
    /// always, since a request is answered as it is made.
    pub(crate) fn readiness(&self) -> Readiness {
        Readiness {
            readable: !self.state.lock().queue.is_empty(),
            writable: true,
            hangup: false,
            error: false,
            priority: false,
        }
    }

    /// `shutdown`, which netlink does not have: `netlink_ops` leaves it as
    /// `sock_no_shutdown`, which refuses.
    pub(crate) const fn shutdown(&self, _how: u32) -> Result<(), Errno> {
        Err(Errno::EOPNOTSUPP)
    }

    /// Send a buffer of requests, and queue what they are answered with.
    ///
    /// A destination may be given and must be the kernel: a message addressed
    /// to another socket's port is `ECONNREFUSED`, since nothing here carries
    /// one program's netlink message to another.
    pub(crate) fn send(&self, data: &[u8], to: Option<&[u8]>) -> Result<usize, Errno> {
        if let Some(raw) = to {
            let address = NetlinkAddress::parse(raw, raw.len()).map_err(|_| Errno::EINVAL)?;
            if address.pid != 0 {
                return Err(Errno::ECONNREFUSED);
            }
        }
        let port = self.port();
        // Changing addresses, routes and links takes `CAP_NET_ADMIN` over the
        // user namespace that owns this socket's network namespace, as Linux's
        // `rtnetlink_rcv_msg` asks; the kernel's own checks, which are no
        // process, may.
        let actor = crate::syscall::userns::acting();
        let privileged = link::net_admin(actor.as_deref(), &self.ns);
        if self.protocol == Protocol::Uevent {
            // Nothing hears it: see the module's documentation.
            return if privileged {
                Ok(data.len())
            } else {
                Err(Errno::EPERM)
            };
        }
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(MAX_REPLY)
            .map_err(|_| Errno::ENOMEM)?;
        buffer.resize(MAX_REPLY, 0);
        let written = route::answer(
            &self.ns,
            actor.as_deref(),
            port,
            data,
            &mut buffer,
            privileged,
        );
        buffer.truncate(written);
        self.queue(&buffer)?;
        // The queue is this socket's, not the stack's, so the wake the net
        // core made while it was locked came too early for it. Waking again
        // costs a walk of an empty queue in the common case and is the
        // difference between a blocked reader waking now and waking on the
        // driving task's next tick.
        self.ns.core().progress().wake_all();
        Ok(data.len())
    }

    /// Take the next reply, and say it came from the kernel.
    pub(crate) fn recv(
        &self,
        out: &mut [u8],
        flags: u32,
        nonblock: bool,
    ) -> Result<(Received, Option<Vec<u8>>), Errno> {
        let peek = flags & MSG_PEEK != 0;
        let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
        let deadline = if nonblock {
            0
        } else {
            fs::socket::deadline_after(self.state.lock().options.receive_timeout)
        };
        loop {
            if let Some(received) = self.take(out, peek) {
                return Ok((received, Some(kernel_address())));
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            fs::socket::wait_on(
                self.ns.core().progress(),
                || !self.state.lock().queue.is_empty(),
                deadline,
            )?;
        }
    }

    /// Take the message at the front of the queue, if there is one.
    fn take(&self, out: &mut [u8], peek: bool) -> Option<Received> {
        let mut state = self.state.lock();
        let front = state.queue.front()?;
        let full = front.len();
        let taken = full.min(out.len());
        let source = front.get(..taken).unwrap_or_default();
        out.get_mut(..taken)?.copy_from_slice(source);
        if !peek && let Some(taken) = state.queue.pop_front() {
            state.queued = state.queued.saturating_sub(full);
            state
                .replies
                .shrink(buffer_footprint::<u8>(taken.capacity()));
        }
        Some(Received {
            bytes: taken,
            full,
            hop_limit: None,
        })
    }

    /// Queue each message of `replies` as a datagram of its own.
    fn queue(&self, replies: &[u8]) -> Result<(), Errno> {
        let mut state = self.state.lock();
        for message in Messages::new(replies) {
            // Everything here was written by `src/lib/network/netlink`'s builder, so a
            // refusal is this kernel's bug rather than a program's; there is
            // nothing to tell the program about it, and the rest of the
            // buffer cannot be read past it either way.
            let Ok(message) = message else {
                break;
            };
            let length = NlMsgHdr::SIZE.saturating_add(message.payload.len());
            if state.queued.saturating_add(length) > MAX_QUEUED {
                return Err(Errno::ENOBUFS);
            }
            let State {
                queue,
                room,
                replies,
                ..
            } = &mut *state;
            reserve_deque(queue, 1, room).map_err(|_| Errno::ENOMEM)?;
            let mut datagram = Vec::new();
            datagram
                .try_reserve_exact(length)
                .map_err(|_| Errno::ENOMEM)?;
            replies
                .grow(buffer_footprint::<u8>(datagram.capacity()))
                .map_err(|_| Errno::ENOMEM)?;
            datagram.extend_from_slice(&message.header.to_bytes());
            datagram.extend_from_slice(message.payload);
            state.queue.push_back(datagram);
            state.queued = state.queued.saturating_add(length);
        }
        Ok(())
    }

    /// The socket's port identifier, binding it if nothing has.
    ///
    /// Linux auto-binds a netlink socket on its first send, so a program that
    /// never calls `bind` still has an identifier its replies are addressed
    /// to.
    fn port(&self) -> u32 {
        let mut state = self.state.lock();
        if state.port == 0 {
            state.port = next_port();
        }
        state.port
    }

    /// `getsockopt`.
    ///
    /// # Errors
    ///
    /// `ENOPROTOOPT` for an option this socket does not have.
    pub(crate) fn get_option(&self, level: i32, name: i32, width: Width) -> Result<Vec<u8>, Errno> {
        let options = self.state.lock().options;
        match (level, name) {
            (SOL_SOCKET, SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW) => {
                Ok(fs::socket::timeval(options.receive_timeout, width))
            }
            (SOL_SOCKET, SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW) => {
                Ok(fs::socket::timeval(options.send_timeout, width))
            }
            _ => Ok(self.option_value(level, name)?.to_le_bytes().to_vec()),
        }
    }

    /// The number an option reads as.
    fn option_value(&self, level: i32, name: i32) -> Result<i32, Errno> {
        let options = self.state.lock().options;
        match (level, name) {
            (SOL_SOCKET, SO_TYPE) => Ok(self.kind.cast_signed()),
            (SOL_SOCKET, SO_DOMAIN) => Ok(i32::from(AF_NETLINK)),
            (SOL_SOCKET, SO_PROTOCOL) => Ok(self.protocol.to_linux()),
            (SOL_SOCKET, SO_ERROR) => Ok(0),
            (SOL_SOCKET, SO_SNDBUF) => Ok(i32::try_from(options.send_buffer).unwrap_or(i32::MAX)),
            (SOL_SOCKET, SO_RCVBUF) => {
                Ok(i32::try_from(options.receive_buffer).unwrap_or(i32::MAX))
            }
            _ => Err(Errno::ENOPROTOOPT),
        }
    }

    /// `setsockopt`.
    ///
    /// # Errors
    ///
    /// `ENOPROTOOPT` for a level this socket does not have, `EINVAL` for a
    /// value that is the wrong size.
    pub(crate) fn set_option(
        &self,
        level: i32,
        name: i32,
        value: &[u8],
        width: Width,
    ) -> Result<(), Errno> {
        if matches!(
            (level, name),
            (
                SOL_SOCKET,
                SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW | SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW
            )
        ) {
            let nanos = fs::socket::read_timeval(value, width)?;
            let mut state = self.state.lock();
            if name == SO_RCVTIMEO_OLD || name == SO_RCVTIMEO_NEW {
                state.options.receive_timeout = nanos;
            } else {
                state.options.send_timeout = nanos;
            }
            return Ok(());
        }
        let number = read_int(value)?;
        match (level, name) {
            (SOL_SOCKET, SO_SNDBUF) => {
                self.state.lock().options.send_buffer = clamp_buffer(number);
                Ok(())
            }
            (SOL_SOCKET, SO_RCVBUF) => {
                self.state.lock().options.receive_buffer = clamp_buffer(number);
                Ok(())
            }
            (SOL_NETLINK, NETLINK_ADD_MEMBERSHIP | NETLINK_DROP_MEMBERSHIP) => {
                // A group beyond the 32 `nl_groups` names. Nothing multicasts
                // yet, so joining one is remembered by not being refused: a
                // program that listens for link changes hears nothing either
                // way, and refusing would make it exit instead.
                let group = u32::try_from(number).map_err(|_| Errno::EINVAL)?;
                let bit = 1_u32.checked_shl(group.saturating_sub(1)).unwrap_or(0);
                let mut state = self.state.lock();
                if name == NETLINK_ADD_MEMBERSHIP {
                    state.groups |= bit;
                } else {
                    state.groups &= !bit;
                }
                Ok(())
            }
            // An option a program sets for luck changes nothing here and is
            // accepted rather than refused, as `AF_INET`'s are.
            (SOL_SOCKET | SOL_NETLINK, _) => Ok(()),
            _ => Err(Errno::ENOPROTOOPT),
        }
    }
}

impl Inode for NetlinkSocket {
    fn metadata(&self) -> Metadata {
        self.metadata
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn poll(&self) -> Readiness {
        self.readiness()
    }

    /// The net core's progress queue, which every change a socket's
    /// readiness reads wakes.
    fn poll_changes(&self) -> Option<u64> {
        Some(self.ns.core().progress().wakes())
    }

    fn poll_queues(&self, visit: &mut dyn FnMut(ferrix_vfs::WakeSource)) -> bool {
        visit(self.ns.core().progress_source());
        true
    }

    fn read_stream(&self, buf: &mut [u8], nonblock: bool) -> ferrix_vfs::Result<usize> {
        self.recv(buf, 0, nonblock)
            .map(|(received, _from)| received.bytes)
    }

    fn write_stream(&self, data: &[u8], _nonblock: bool) -> ferrix_vfs::Result<usize> {
        // A request is answered as it is made, so a send never waits and the
        // descriptor's non-blocking flag has nothing to change.
        self.send(data, None)
    }
}

/// The socket an open file reads and writes through, if it is a netlink one.
pub(crate) fn of(file: &OpenFile) -> Option<Arc<NetlinkSocket>> {
    Arc::clone(file.io())
        .into_any()
        .downcast::<NetlinkSocket>()
        .ok()
}

/// The next port identifier, never zero, which means "the kernel".
fn next_port() -> u32 {
    // Never zero, which is the kernel's own. Four billion sockets would have
    // to be opened before the counter wraps back onto a number in use.
    NEXT_PORT.fetch_add(1, Ordering::Relaxed).max(1)
}

/// The address every reply comes from: the kernel, which is port zero in no
/// group.
fn kernel_address() -> Vec<u8> {
    NetlinkAddress::default().to_bytes().to_vec()
}

/// An option's value as the `int` most of them are.
fn read_int(value: &[u8]) -> Result<i32, Errno> {
    let bytes = value.first_chunk::<4>().ok_or(Errno::EINVAL)?;
    Ok(i32::from_ne_bytes(*bytes))
}

/// A buffer size as Linux stores it: doubled, and held between its bounds.
fn clamp_buffer(requested: i32) -> usize {
    let wanted = usize::try_from(requested.max(0))
        .unwrap_or(0)
        .saturating_mul(2);
    wanted.clamp(
        ferrix_linux_abi::socket::SOCKET_BUFFER_MIN,
        ferrix_linux_abi::socket::SOCKET_BUFFER_MAX,
    )
}
