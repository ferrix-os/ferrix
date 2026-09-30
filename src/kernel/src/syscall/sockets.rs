//! The socket calls: `AF_UNIX` sockets, and an honest refusal for every other
//! family.
//!
//! # What there is
//!
//! `socket` and `socketpair` make `AF_UNIX` stream, sequenced-packet and
//! datagram sockets (`crate::fs::socket`). Every other family is refused with
//! `EAFNOSUPPORT`, the answer Linux gives for a family it was built without,
//! and the one every program already handles. The calls on a socket send and
//! receive, shut it down, name it unnamed, and ask and set its options.
//!
//! An `AF_UNIX` socket is named with `bind`, `listen`, `connect` and `accept`,
//! and passes descriptors with `SCM_RIGHTS`. Credentials (`SCM_CREDENTIALS`)
//! and a datagram sent to a name are `EOPNOTSUPP` until they land. On a
//! descriptor that is not a socket every call is `ENOTSOCK`, and on a closed
//! one `EBADF`, exactly as Linux answers.
//!
//! # Passing descriptors
//!
//! `sendmsg` turns an `SCM_RIGHTS` message's descriptors into references to
//! their open files before anything is sent, and the socket queues them with
//! the first byte (`crate::fs::socket::Passed`). `recvmsg` installs the ones
//! that came with the bytes it took as new descriptors, as many as its control
//! buffer has room for, and writes one `SCM_RIGHTS` message saying which;
//! what does not fit is closed and the message flagged `MSG_CTRUNC`, as
//! `scm_detach_fds` does. A file is only ever dropped with the descriptor
//! table unlocked: the table is given a clone, so the last reference to a file
//! that could not be installed is the one dropped afterwards.
//!
//! # Linux's order
//!
//! The checks Linux makes before it looks at the descriptor are made first, in
//! its order -- a bad flag is `EINVAL`, a buffer outside the user half `EFAULT`
//! -- so a program sees the same first error it would on Linux.
//!
//! # One copy in, one copy out
//!
//! A send gathers the program's buffers into one kernel buffer and a receive
//! scatters one out, so a record is queued and taken whole whatever iovecs
//! carried it. One call holds at most [`MAX_TRANSFER`] in the kernel: a stream
//! send past it is a short send, as a full buffer makes one anyway, and a record
//! that large is larger than any socket buffer and refused regardless.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_bootinfo::is_user_address;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::inet::{
    IPPROTO_ICMP, IPPROTO_ICMPV6, IPPROTO_MAX, IPPROTO_TCP, IPPROTO_UDP, SOCKADDR_STORAGE_SIZE,
};
use ferrix_linux_abi::netlink::NETLINK_ROUTE;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::socket::{
    AF_INET, AF_INET6, AF_MAX, AF_NETLINK, AF_PACKET, AF_UNIX, CmsgHdr, ControlMessages,
    MSG_CMSG_CLOEXEC, MSG_CMSG_COMPAT, MSG_CTRUNC, MSG_OOB, MSG_TRUNC, MsgHdr, SCM_CREDENTIALS,
    SCM_MAX_FD, SCM_RIGHTS, SOCK_CLOEXEC, SOCK_DGRAM, SOCK_NONBLOCK, SOCK_RAW, SOCK_STREAM,
    SOCK_TYPE_MASK, SOL_SOCKET, Ucred, UnixAddress, Width, cmsg_align, cmsg_len, cmsg_space,
};
use ferrix_net::packet::PacketKind;
use ferrix_net::socket::Family;
use ferrix_vfs::OpenFile;

use crate::fs;
use crate::fs::socket::{Passed, Received, Socket, SocketType};
use crate::net::netlink::{self as netlink, NetlinkSocket};
use crate::net::packet::PacketSocket;
use crate::net::socket::{self as inet, InetKind, InetSocket};
use crate::syscall::attributes::int;
use crate::syscall::credentials;
use crate::syscall::fd;
use crate::syscall::process::Process;
use crate::syscall::uaccess;
use crate::trap::Abi;

/// One past the last socket type: `SOCK_MAX` in `linux/net.h`.
const SOCK_MAX: u32 = 11;

/// This build's pointer width, which is the width of every `msghdr`,
/// `cmsghdr` and `timeval` a program hands it.
const NATIVE: Width = if size_of::<usize>() == 8 {
    Width::Bits64
} else {
    Width::Bits32
};

/// Linux's `MAX_RW_COUNT`: no transfer reports more, so the count fits a
/// 32-bit return register.
const MAX_RW_COUNT: usize = 0x7FFF_F000;

/// `UIO_MAXIOV`: the most iovecs one message may have.
const UIO_MAXIOV: u64 = 1024;

/// The most one send or receive holds in the kernel at once.
const MAX_TRANSFER: usize = 16 << 20;

/// The longest control buffer a send takes: Linux's default `optmem_max`.
const MAX_CONTROL: usize = 20_480;

/// The longest option value `setsockopt` reads.
const MAX_OPTION: usize = 64;

/// Answer `call` if it is one of this module's.
pub(crate) fn dispatch(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
    abi: Abi,
) -> Option<Result<usize, Errno>> {
    let descriptor = fd::arg(a[0]);
    // An i386 program's `msghdr`, `cmsghdr`, `iovec` and `timeval` are made of
    // 32-bit words on this 64-bit kernel, as every program's are on ARMv7-A.
    let width = if abi == Abi::Compat {
        Width::Bits32
    } else {
        NATIVE
    };
    let answer = match call {
        Syscall::Socket => sys_socket(process, int(a[0]), a[1] as u32, int(a[2])),
        Syscall::Socketpair => sys_socketpair(process, int(a[0]), a[1] as u32, int(a[2]), a[3]),
        Syscall::Bind => sys_bind(process, descriptor, a[1], a[2]),
        Syscall::Listen => sys_listen(process, descriptor, int(a[1])),
        Syscall::Connect => sys_connect(process, descriptor, a[1], a[2]),
        Syscall::Accept => sys_accept(process, descriptor, a[1], a[2], 0),
        Syscall::Accept4 => sys_accept(process, descriptor, a[1], a[2], a[3] as u32),
        Syscall::Getsockname => sys_getname(process, descriptor, a[1], a[2], false),
        Syscall::Getpeername => sys_getname(process, descriptor, a[1], a[2], true),
        Syscall::Shutdown => sys_shutdown(process, descriptor, a[1] as u32),
        Syscall::Sendto => sys_sendto(
            process,
            descriptor,
            a[1],
            a[2],
            a[3] as u32,
            a[4],
            name_length(a[4], a[5]),
        ),
        Syscall::Recvfrom => sys_recvfrom(process, descriptor, a[1], a[2], a[3] as u32, a[4], a[5]),
        Syscall::Sendmsg => sys_sendmsg(process, descriptor, a[1], a[2] as u32, width),
        Syscall::Recvmsg => sys_recvmsg(process, descriptor, a[1], a[2] as u32, width),
        Syscall::Getsockopt => {
            let option = (int(a[1]), int(a[2]));
            sys_getsockopt(process, descriptor, option, a[3], a[4], width)
        }
        Syscall::Setsockopt => {
            let option = (int(a[1]), int(a[2]));
            sys_setsockopt(process, descriptor, option, a[3], int(a[4]), width)
        }
        _ => return None,
    };
    Some(answer)
}

/// What a receive took, the encoded address it came from if the socket names
/// one, and the control messages that come with it.
type Taken = (Received, Option<Vec<u8>>, Vec<inet::Control>);

/// A socket reached through a descriptor, whatever family it is.
///
/// The syscall layer is one set of checks in Linux's order, and only the last
/// step of each call differs between the families. This is that step: every
/// call above takes an `Any` and asks it, so the order of the checks cannot
/// drift apart between `AF_UNIX` and `AF_INET`.
#[derive(Debug)]
enum Any {
    /// An `AF_UNIX` socket.
    Unix(Arc<Socket>),
    /// An `AF_INET` or `AF_INET6` socket.
    Inet(Arc<InetSocket>),
    /// An `AF_NETLINK` socket.
    Netlink(Arc<NetlinkSocket>),
    /// An `AF_PACKET` socket.
    Packet(Arc<PacketSocket>),
}

impl Any {
    /// Its type, as `SO_TYPE` reports it.
    fn kind(&self) -> SocketType {
        match self {
            Any::Unix(socket) => socket.kind(),
            Any::Inet(socket) => match socket.kind() {
                InetKind::Stream => SocketType::Stream,
                InetKind::Datagram | InetKind::Echo | InetKind::Raw { .. } => SocketType::Datagram,
            },
            // `SOCK_RAW` and `SOCK_DGRAM` are the same socket on netlink, and
            // both carry records; a packet socket carries frames either way.
            Any::Netlink(_) | Any::Packet(_) => SocketType::Datagram,
        }
    }

    /// Whether it has a peer.
    fn is_connected(&self) -> bool {
        match self {
            Any::Unix(socket) => socket.is_connected(),
            Any::Inet(socket) => socket.is_connected(),
            // A netlink socket has no peer: it talks to the kernel, which is
            // not a socket `getpeername` can name. Neither has a packet socket.
            Any::Netlink(_) | Any::Packet(_) => false,
        }
    }

    /// Stop one or both directions.
    fn shutdown(&self, how: u32) -> Result<(), Errno> {
        match self {
            Any::Unix(socket) => socket.shutdown(how),
            Any::Inet(socket) => socket.shutdown(how),
            Any::Netlink(socket) => socket.shutdown(how),
            // `packet_ops` has `sock_no_shutdown`.
            Any::Packet(_) => Err(Errno::EOPNOTSUPP),
        }
    }

    /// Send, to `to` if the call named a destination.
    fn send(
        &self,
        data: &[u8],
        flags: u32,
        nonblock: bool,
        to: Option<&[u8]>,
    ) -> Result<usize, Errno> {
        match self {
            Any::Unix(socket) => socket.send(data, flags, nonblock),
            Any::Inet(socket) => socket.send(data, flags, nonblock, to),
            // Every netlink request is answered as it is made, so neither the
            // flags that ask not to wait nor the descriptor's own change
            // anything.
            Any::Netlink(socket) => socket.send(data, to),
            // A frame goes out at once or not at all: there is nothing to wait
            // for.
            Any::Packet(socket) => socket.send(data, to),
        }
    }

    /// Receive, and say where it came from and what control messages come
    /// with it.
    fn recv(&self, out: &mut [u8], flags: u32, nonblock: bool) -> Result<Taken, Errno> {
        match self {
            Any::Unix(socket) => socket
                .recv(out, flags, nonblock)
                .map(|taken| (taken, None, Vec::new())),
            Any::Inet(socket) => socket.recv(out, flags, nonblock).map(|(taken, from)| {
                let control = socket.control_messages(&taken);
                (
                    Received {
                        bytes: taken.bytes,
                        full: taken.full,
                    },
                    from,
                    control,
                )
            }),
            Any::Netlink(socket) => socket.recv(out, flags, nonblock).map(|(taken, from)| {
                (
                    Received {
                        bytes: taken.bytes,
                        full: taken.full,
                    },
                    from,
                    Vec::new(),
                )
            }),
            Any::Packet(socket) => socket.recv(out, flags, nonblock).map(|(taken, from)| {
                (
                    Received {
                        bytes: taken.bytes,
                        full: taken.full,
                    },
                    from,
                    Vec::new(),
                )
            }),
        }
    }

    /// An option's value.
    fn get_option(&self, level: i32, name: i32, width: Width) -> Result<Vec<u8>, Errno> {
        match self {
            Any::Unix(socket) => socket.get_option(level, name, width),
            Any::Inet(socket) => socket.get_option(level, name, width),
            Any::Netlink(socket) => socket.get_option(level, name, width),
            Any::Packet(socket) => socket.get_option(level, name, width),
        }
    }

    /// Set an option.
    fn set_option(&self, level: i32, name: i32, value: &[u8], width: Width) -> Result<(), Errno> {
        match self {
            Any::Unix(socket) => socket.set_option(level, name, value, width),
            Any::Inet(socket) => socket.set_option(level, name, value, width),
            Any::Netlink(socket) => socket.set_option(level, name, value, width),
            Any::Packet(socket) => socket.set_option(level, name, value, width),
        }
    }

    /// The name it is bound to, as a `sockaddr`.
    ///
    /// An `AF_UNIX` socket without a name is `AF_UNIX` alone, two bytes long,
    /// which is what Linux reports for one.
    fn local_name(&self) -> Result<Vec<u8>, Errno> {
        match self {
            Any::Unix(socket) => Ok(socket.sock_name()),
            Any::Inet(socket) => Ok(socket.local_name()),
            Any::Netlink(socket) => Ok(socket.local_name()),
            Any::Packet(socket) => Ok(socket.local_name()),
        }
    }

    /// The name of its peer.
    fn peer_name(&self) -> Result<Vec<u8>, Errno> {
        match self {
            Any::Unix(socket) => socket.peer_sock_name(),
            Any::Inet(socket) => socket.peer_name().ok_or(Errno::ENOTCONN),
            Any::Netlink(_) => Err(Errno::ENOTCONN),
            // `packet_getname` refuses a peer.
            Any::Packet(_) => Err(Errno::EOPNOTSUPP),
        }
    }

    /// Give it a name.
    fn bind(&self, process: &Process, raw: &[u8]) -> Result<(), Errno> {
        match self {
            Any::Unix(socket) => {
                let address = unix_address(raw)?;
                socket.bind(&crate::syscall::path::context(process), &address)
            }
            Any::Inet(socket) => socket.bind(raw),
            Any::Netlink(socket) => socket.bind(raw),
            Any::Packet(socket) => socket.bind(raw),
        }
    }

    /// Start listening.
    fn listen(&self, process: &Process, backlog: i32) -> Result<(), Errno> {
        match self {
            Any::Unix(socket) => socket.listen(backlog, fs::socket::credentials_of(process)),
            Any::Netlink(_) | Any::Packet(_) => Err(Errno::EOPNOTSUPP),
            Any::Inet(socket) => socket.listen(backlog),
        }
    }

    /// Connect to a peer.
    fn connect(&self, process: &Process, raw: &[u8], nonblock: bool) -> Result<(), Errno> {
        match self {
            Any::Unix(socket) => {
                let address = unix_address(raw)?;
                socket.connect(
                    &crate::syscall::path::context(process),
                    &address,
                    nonblock,
                    fs::socket::credentials_of(process),
                )
            }
            Any::Netlink(_) | Any::Packet(_) => Err(Errno::EOPNOTSUPP),
            Any::Inet(socket) => socket.connect(raw, nonblock),
        }
    }

    /// Take a connection, and say who made it: waiting unless `nonblock`,
    /// the listener's flag, and the new socket non-blocking only when
    /// `accepted_nonblock`, `accept4`'s.
    fn accept(
        &self,
        nonblock: bool,
        accepted_nonblock: bool,
        owner: (u32, u32),
    ) -> Result<(Arc<OpenFile>, Vec<u8>), Errno> {
        match self {
            Any::Unix(socket) => socket.accept(nonblock, accepted_nonblock),
            Any::Netlink(_) | Any::Packet(_) => Err(Errno::EOPNOTSUPP),
            Any::Inet(socket) => socket.accept(nonblock, accepted_nonblock, owner),
        }
    }
}

/// The `AF_UNIX` address in the bytes a program passed, which Linux refuses
/// with `EINVAL` however it is wrong.
fn unix_address(raw: &[u8]) -> Result<UnixAddress<'_>, Errno> {
    UnixAddress::parse(raw, raw.len()).map_err(|_| Errno::EINVAL)
}

/// What a `socket` call asked for.
#[derive(Clone, Copy, Debug)]
enum Opened {
    /// An `AF_UNIX` socket of this type.
    Unix(SocketType),
    /// An `AF_INET` or `AF_INET6` socket of this kind.
    Inet(Family, InetKind),
    /// An `AF_NETLINK` socket of this type.
    Netlink(u32),
    /// An `AF_PACKET` socket: the type `socket` was given, not yet checked,
    /// and the protocol as it was given, in network byte order.
    Packet(u32, u16),
}

/// Whether `socket` may open what `socket_type` named: a raw socket needs
/// `CAP_NET_RAW` over the user namespace that owns the network namespace it
/// is made in (`docs/NETNS.md` section 2.3).
///
/// Asked after the type and protocol are known to exist, as `inet_create`
/// asks it after its protocol lookup, so a raw socket at protocol zero is
/// `EPROTONOSUPPORT` for everyone.
fn permitted(process: &Process, opened: &Opened) -> Result<(), Errno> {
    let raw = matches!(
        opened,
        Opened::Inet(_, InetKind::Raw { .. }) | Opened::Packet(..)
    );
    if raw {
        let owner = process.net_ns();
        let held = process.with_credentials(|ids| {
            crate::syscall::userns::capable_over(
                ids,
                owner.owner(),
                crate::syscall::userns::CAP_NET_RAW,
            )
        });
        if !held {
            return Err(Errno::EPERM);
        }
    }
    Ok(())
}

/// Only `SOCK_NONBLOCK` and `SOCK_CLOEXEC` may accompany a type, or be given
/// to `accept4`.
fn known_flags(flags: u32) -> Result<(), Errno> {
    if flags & !(SOCK_CLOEXEC | SOCK_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

/// What `socket`'s arguments name: `__sys_socket`, `__sock_create` and the
/// family's own create function, in their order.
fn socket_type(family: i32, kind: u32, protocol: i32) -> Result<Opened, Errno> {
    known_flags(kind & !SOCK_TYPE_MASK)?;
    if !(0..i32::from(AF_MAX)).contains(&family) {
        return Err(Errno::EAFNOSUPPORT);
    }
    if kind & SOCK_TYPE_MASK >= SOCK_MAX {
        return Err(Errno::EINVAL);
    }
    let kind = kind & SOCK_TYPE_MASK;
    if family == i32::from(AF_INET) {
        return inet_type(Family::V4, kind, protocol);
    }
    if family == i32::from(AF_INET6) {
        return inet_type(Family::V6, kind, protocol);
    }
    if family == i32::from(AF_NETLINK) {
        return netlink_type(kind, protocol);
    }
    if family == i32::from(AF_PACKET) {
        // `packet_create` asks for `CAP_NET_RAW` before it looks at the type,
        // so the type is checked after [`permitted`], in [`packet_kind`]. The
        // protocol is taken as its low 16 bits, which is all a `__be16` holds.
        return Ok(Opened::Packet(kind, protocol as u16));
    }
    if family != i32::from(AF_UNIX) {
        return Err(Errno::EAFNOSUPPORT);
    }
    // `PF_UNIX`, which is `AF_UNIX`, is the one protocol besides zero.
    if protocol != 0 && protocol != i32::from(AF_UNIX) {
        return Err(Errno::EPROTONOSUPPORT);
    }
    SocketType::from_linux(kind)
        .map(Opened::Unix)
        .ok_or(Errno::ESOCKTNOSUPPORT)
}

/// The `AF_INET` or `AF_INET6` socket a type and a protocol name.
///
/// `SOCK_RAW` takes any protocol but zero, which `inet_create`'s lookup
/// finds no match for, and `IPPROTO_MAX` and above, which it refuses first.
/// Whether the caller may have one is [`permitted`]'s question. An `AF_INET6`
/// raw socket is not implemented yet and is `EPROTONOSUPPORT` to a caller
/// who could otherwise have had it.
fn inet_type(family: Family, kind: u32, protocol: i32) -> Result<Opened, Errno> {
    if !(0..IPPROTO_MAX).contains(&protocol) {
        return Err(Errno::EINVAL);
    }
    let echo = match family {
        Family::V4 => IPPROTO_ICMP,
        Family::V6 => IPPROTO_ICMPV6,
    };
    match (kind, protocol) {
        (SOCK_STREAM, 0 | IPPROTO_TCP) => Ok(Opened::Inet(family, InetKind::Stream)),
        (SOCK_DGRAM, 0 | IPPROTO_UDP) => Ok(Opened::Inet(family, InetKind::Datagram)),
        (SOCK_DGRAM, given) if given == echo => Ok(Opened::Inet(family, InetKind::Echo)),
        (SOCK_RAW, 0) => Err(Errno::EPROTONOSUPPORT),
        (SOCK_RAW, given) => match u8::try_from(given) {
            Ok(protocol) => Ok(Opened::Inet(family, InetKind::Raw { protocol })),
            Err(_) => Err(Errno::EPROTONOSUPPORT),
        },
        (SOCK_STREAM | SOCK_DGRAM, _) => Err(Errno::EPROTONOSUPPORT),
        _ => Err(Errno::ESOCKTNOSUPPORT),
    }
}

/// The kind of packet socket a type names: `SOCK_RAW` or `SOCK_DGRAM`.
/// `SOCK_PACKET`, the interface before `AF_PACKET`, is not offered, and is
/// refused with every other type.
fn packet_kind(kind: u32) -> Result<PacketKind, Errno> {
    match kind {
        SOCK_RAW => Ok(PacketKind::Raw),
        SOCK_DGRAM => Ok(PacketKind::Datagram),
        _ => Err(Errno::ESOCKTNOSUPPORT),
    }
}

/// The `AF_NETLINK` socket a type and a protocol name.
///
/// `netlink_create` takes `SOCK_RAW` and `SOCK_DGRAM` and nothing else, and
/// makes no distinction between them: a netlink socket carries records
/// whichever was asked for. `NETLINK_ROUTE` is the one protocol this kernel
/// has; the others are `EPROTONOSUPPORT`, which is what Linux answers for a
/// family built without them.
fn netlink_type(kind: u32, protocol: i32) -> Result<Opened, Errno> {
    if kind != SOCK_DGRAM && kind != SOCK_RAW {
        return Err(Errno::ESOCKTNOSUPPORT);
    }
    if protocol != NETLINK_ROUTE {
        return Err(Errno::EPROTONOSUPPORT);
    }
    Ok(Opened::Netlink(kind))
}

/// `socket`.
pub(crate) fn sys_socket(
    process: &Process,
    family: i32,
    kind: u32,
    protocol: i32,
) -> Result<usize, Errno> {
    let opened = socket_type(family, kind, protocol)?;
    permitted(process, &opened)?;
    let owner = crate::syscall::path::creator_ids(process);
    let nonblock = kind & SOCK_NONBLOCK != 0;
    let file = match opened {
        Opened::Unix(socket_type) => fs::socket::new_socket(socket_type, nonblock, process)?,
        Opened::Inet(family, kind) => {
            InetSocket::open(&process.net_ns(), family, kind, nonblock, owner)?
        }
        Opened::Netlink(kind) => NetlinkSocket::open(&process.net_ns(), kind, nonblock, owner)?,
        Opened::Packet(kind, protocol) => PacketSocket::open(
            &process.net_ns(),
            packet_kind(kind)?,
            protocol,
            nonblock,
            owner,
        )?,
    };
    let descriptor = process
        .files()
        .lock()
        .insert(file, kind & SOCK_CLOEXEC != 0)?;
    usize::try_from(descriptor).map_err(|_| Errno::EMFILE)
}

/// `socketpair`: two sockets connected to each other, written into `pair`.
///
/// Linux reserves the two descriptors and writes their numbers before it
/// creates the sockets, so an unwritable pointer is `EFAULT` ahead of the
/// family's refusal. The pointer's range is checked here; a mapped-but-
/// unwritable page inside it is the one case answered after the sockets are
/// made, and then both descriptors are closed again.
pub(crate) fn sys_socketpair(
    process: &Process,
    family: i32,
    kind: u32,
    protocol: i32,
    pair: u64,
) -> Result<usize, Errno> {
    known_flags(kind & !SOCK_TYPE_MASK)?;
    user_buffer(pair, 8)?;
    let opened = socket_type(family, kind, protocol)?;
    permitted(process, &opened)?;
    let socket_type = match opened {
        Opened::Unix(socket_type) => socket_type,
        // `inet_socketpair` is `sock_no_socketpair`: the internet families
        // have no way to make two connected sockets without a listener, and
        // `netlink_ops` leaves the call at the same refusal.
        Opened::Inet(_, _) | Opened::Netlink(_) => return Err(Errno::EOPNOTSUPP),
        // `packet_ops` has `sock_no_socketpair`, once the type is one it has.
        Opened::Packet(kind, _) => {
            let _ = packet_kind(kind)?;
            return Err(Errno::EOPNOTSUPP);
        }
    };
    let (one, other) = fs::socket::new_pair(socket_type, kind & SOCK_NONBLOCK != 0, process)?;
    let (first, second) = install_pair(process, one, other, kind & SOCK_CLOEXEC != 0)?;
    let numbers: Vec<u8> = first
        .to_le_bytes()
        .into_iter()
        .chain(second.to_le_bytes())
        .collect();
    if uaccess::copy_to_user(process.space(), pair, &numbers).is_err() {
        // Taken out under the lock, dropped after it: see `fd`.
        let taken = {
            let mut files = process.files().lock();
            (files.remove(first), files.remove(second))
        };
        drop(taken);
        return Err(Errno::EFAULT);
    }
    Ok(0)
}

/// Put both sockets of a pair in the table in one hold of its lock, or
/// neither.
///
/// A socket a full table refuses is dropped inside the lock, which is bounded:
/// it closes its directions and wakes their queues.
fn install_pair(
    process: &Process,
    one: Arc<OpenFile>,
    other: Arc<OpenFile>,
    cloexec: bool,
) -> Result<(i32, i32), Errno> {
    let mut files = process.files().lock();
    let first = files.insert(one, cloexec)?;
    match files.insert(other, cloexec) {
        Ok(second) => Ok((first, second)),
        Err(errno) => {
            let displaced = files.remove(first);
            drop(files);
            drop(displaced);
            Err(errno)
        }
    }
}

/// `access_ok`: a buffer must lie in the user half. Only the range is checked,
/// not whether it is mapped, which is all `import_ubuf` checks before the
/// descriptor is looked up.
fn user_buffer(at: u64, len: u64) -> Result<(), Errno> {
    let len = len as usize as u64;
    if len == 0 {
        return Ok(());
    }
    let last = at.checked_add(len - 1).ok_or(Errno::EFAULT)?;
    if is_user_address(at) && is_user_address(last) {
        Ok(())
    } else {
        Err(Errno::EFAULT)
    }
}

/// The socket behind `descriptor`, with the open file it was reached through:
/// `EBADF` if nothing is open there, and `ENOTSOCK` if something that is not a
/// socket is.
fn socket_of(process: &Process, descriptor: i32) -> Result<(Arc<OpenFile>, Any), Errno> {
    let file = fd::file(process, descriptor)?;
    if let Some(socket) = fs::socket::of(&file) {
        return Ok((file, Any::Unix(socket)));
    }
    if let Some(socket) = inet::of(&file) {
        return Ok((file, Any::Inet(socket)));
    }
    if let Some(socket) = crate::net::packet::of(&file) {
        return Ok((file, Any::Packet(socket)));
    }
    let socket = netlink::of(&file).ok_or(Errno::ENOTSOCK)?;
    Ok((file, Any::Netlink(socket)))
}

/// A length a program gave for a buffer it passes by pointer: `EINVAL` if it
/// is negative, as Linux reads it as an `int`.
fn buffer_length(process: &Process, at: u64) -> Result<usize, Errno> {
    let length = uaccess::get_u32(process.space(), at)?;
    let length = length.cast_signed();
    usize::try_from(length).map_err(|_| Errno::EINVAL)
}

/// `getsockname`, and `getpeername` with `peer`.
///
/// An `AF_UNIX` socket without a name is reported as `AF_UNIX` alone, two
/// bytes long, as Linux reports it; an internet socket reports the address and
/// port it is bound or connected to.
fn sys_getname(
    process: &Process,
    descriptor: i32,
    address: u64,
    length: u64,
    peer: bool,
) -> Result<usize, Errno> {
    let (_file, socket) = socket_of(process, descriptor)?;
    if peer && !socket.is_connected() {
        return Err(Errno::ENOTCONN);
    }
    let capacity = buffer_length(process, length)?;
    let encoded = if peer {
        socket.peer_name()?
    } else {
        socket.local_name()?
    };
    write_address(process, address, length, &encoded, capacity)?;
    Ok(0)
}

/// Write a `sockaddr` back to a program, cut to the room it offered, with the
/// length it would have taken.
///
/// That is Linux's `move_addr_to_user`: the length reported is the address's
/// own, not the number of bytes written, so a program that gave a short buffer
/// can tell it was cut.
fn write_address(
    process: &Process,
    address: u64,
    length: u64,
    encoded: &[u8],
    capacity: usize,
) -> Result<(), Errno> {
    if address != 0 {
        let shown = encoded
            .get(..capacity.min(encoded.len()))
            .unwrap_or_default();
        uaccess::copy_to_user(process.space(), address, shown).map_err(|_| Errno::EFAULT)?;
    }
    if length != 0 {
        uaccess::put_u32(
            process.space(),
            length,
            u32::try_from(encoded.len()).unwrap_or(u32::MAX),
        )?;
    }
    Ok(())
}

/// A `sockaddr` a program passed by pointer and length.
fn read_address(process: &Process, address: u64, length: u64) -> Result<Vec<u8>, Errno> {
    let length = length & u64::from(u32::MAX);
    if (length as u32).cast_signed() < 0 {
        return Err(Errno::EINVAL);
    }
    let length = usize::try_from(length).map_err(|_| Errno::EINVAL)?;
    if length > SOCKADDR_STORAGE_SIZE {
        return Err(Errno::EINVAL);
    }
    // `move_addr_to_kernel` takes a length of zero and copies nothing, leaving
    // the family to refuse an address it cannot read. Refusing here instead
    // would answer `EINVAL` where Linux answers whatever the family answers.
    if length == 0 {
        return Ok(Vec::new());
    }
    copy_in(process, address, length)
}

/// `bind`.
fn sys_bind(process: &Process, descriptor: i32, address: u64, length: u64) -> Result<usize, Errno> {
    let (_file, socket) = socket_of(process, descriptor)?;
    let raw = read_address(process, address, length)?;
    socket.bind(process, &raw)?;
    Ok(0)
}

/// `listen`.
fn sys_listen(process: &Process, descriptor: i32, backlog: i32) -> Result<usize, Errno> {
    let (_file, socket) = socket_of(process, descriptor)?;
    socket.listen(process, backlog)?;
    Ok(0)
}

/// `connect`.
fn sys_connect(
    process: &Process,
    descriptor: i32,
    address: u64,
    length: u64,
) -> Result<usize, Errno> {
    let (file, socket) = socket_of(process, descriptor)?;
    let raw = read_address(process, address, length)?;
    socket.connect(process, &raw, file.status().nonblock)?;
    Ok(0)
}

/// `accept` and `accept4`: the connection is installed as a new descriptor and
/// the peer's address written back.
///
/// The new descriptor's non-blocking and close-on-exec flags come from
/// `accept4`'s own flags and are not inherited from the listener, which is
/// what Linux does and what a program that forgets to set them relies on.
fn sys_accept(
    process: &Process,
    descriptor: i32,
    address: u64,
    length: u64,
    flags: u32,
) -> Result<usize, Errno> {
    known_flags(flags)?;
    let (file, socket) = socket_of(process, descriptor)?;
    let owner = crate::syscall::path::creator_ids(process);
    let (accepted, peer) =
        socket.accept(file.status().nonblock, flags & SOCK_NONBLOCK != 0, owner)?;
    let taken = process
        .files()
        .lock()
        .insert(accepted, flags & SOCK_CLOEXEC != 0)?;
    // The address is written last, as `__sys_accept4` writes it: a program
    // that passed an unreadable length still gets its connection taken, and
    // closing the descriptor again is this call's job rather than its
    // caller's.
    if address != 0 {
        let written = buffer_length(process, length)
            .and_then(|capacity| write_address(process, address, length, &peer, capacity));
        if let Err(errno) = written {
            let displaced = process.files().lock().remove(taken);
            drop(displaced);
            return Err(errno);
        }
    }
    usize::try_from(taken).map_err(|_| Errno::EMFILE)
}

/// `shutdown`.
fn sys_shutdown(process: &Process, descriptor: i32, how: u32) -> Result<usize, Errno> {
    let (_file, socket) = socket_of(process, descriptor)?;
    socket.shutdown(how)?;
    Ok(0)
}

/// Whether a send may name a destination of `length` bytes: a connected
/// stream socket refuses one with `EISCONN` and an unconnected one with
/// `EOPNOTSUPP`, as `unix_stream_sendmsg` does; a sequenced-packet socket
/// ignores it, as `unix_seqpacket_sendmsg` does; and a datagram to a name
/// waits for names.
fn check_destination(socket: &Any, length: u64) -> Result<(), Errno> {
    if length == 0 {
        return Ok(());
    }
    if let Any::Netlink(_) | Any::Packet(_) = socket {
        // Every netlink send may name the kernel, whatever the socket's type;
        // `netlink_sendmsg` reads the address and refuses only what it holds.
        // Every packet send may name a link address, which `packet_sendmsg`
        // reads the same way.
        return Ok(());
    }
    if let Any::Inet(_) = socket {
        // An internet datagram socket takes a destination on every send; a
        // connected stream one refuses it, as `tcp_sendmsg` does.
        return match socket.kind() {
            SocketType::Stream => Err(Errno::EISCONN),
            SocketType::Datagram | SocketType::SeqPacket => Ok(()),
        };
    }
    match socket.kind() {
        SocketType::Stream if socket.is_connected() => Err(Errno::EISCONN),
        SocketType::Stream | SocketType::Datagram => Err(Errno::EOPNOTSUPP),
        SocketType::SeqPacket => Ok(()),
    }
}

/// A transfer length clamped as Linux clamps it, and to what one call holds
/// in the kernel.
fn clamped(length: u64) -> usize {
    usize::try_from(length)
        .unwrap_or(usize::MAX)
        .min(MAX_RW_COUNT)
        .min(MAX_TRANSFER)
}

/// A zeroed kernel buffer of `length` bytes, or `ENOMEM`.
fn zeroed(length: usize) -> Result<Vec<u8>, Errno> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(|_| Errno::ENOMEM)?;
    bytes.resize(length, 0);
    Ok(bytes)
}

/// `length` bytes of the caller's memory.
fn copy_in(process: &Process, at: u64, length: usize) -> Result<Vec<u8>, Errno> {
    let mut bytes = zeroed(length)?;
    if length > 0 {
        uaccess::copy_from_user(process.space(), at, &mut bytes).map_err(|_| Errno::EFAULT)?;
    }
    Ok(bytes)
}

/// What a receive answers: a record's whole length under `MSG_TRUNC`, and the
/// bytes copied otherwise.
fn received_count(socket: &Any, received: Received, flags: u32) -> usize {
    if flags & MSG_TRUNC != 0 && socket.kind() != SocketType::Stream {
        received.full
    } else {
        received.bytes
    }
}

/// The destination length `sendto` was given: none without an address,
/// whatever the length says, since `__sys_sendto` reads one only with one.
fn name_length(address: u64, length: u64) -> u64 {
    if address == 0 { 0 } else { length }
}

/// A message's name length as `__copy_msghdr` reads it: none without a name,
/// and `EINVAL` for a negative one.
fn name_length_of(message: &MsgHdr) -> Result<u64, Errno> {
    if message.name == 0 {
        return Ok(0);
    }
    u64::try_from(message.name_len.cast_signed()).map_err(|_| Errno::EINVAL)
}

/// `sendto`. With no destination, what `send` and `write` do.
fn sys_sendto(
    process: &Process,
    descriptor: i32,
    buffer: u64,
    length: u64,
    flags: u32,
    a_address: u64,
    address_length: u64,
) -> Result<usize, Errno> {
    user_buffer(buffer, length)?;
    let (file, socket) = socket_of(process, descriptor)?;
    let address_length = address_length & u64::from(u32::MAX);
    // `move_addr_to_kernel`'s refusal, made before the send looks at the name.
    if address_length != 0 && (address_length as u32).cast_signed() < 0 {
        return Err(Errno::EINVAL);
    }
    check_destination(&socket, address_length)?;
    if flags & MSG_OOB != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let destination = if address_length == 0 {
        None
    } else {
        Some(read_address(process, a_address, address_length)?)
    };
    let data = copy_in(process, buffer, clamped(length))?;
    match &socket {
        Any::Unix(unix) => unix.send_from(process, &data, flags, file.status().nonblock, None),
        _ => socket.send(&data, flags, file.status().nonblock, destination.as_deref()),
    }
}

/// `recvfrom`. A peer without a name reports an address of length zero, as
/// `unix_copy_addr` leaves it.
fn sys_recvfrom(
    process: &Process,
    descriptor: i32,
    buffer: u64,
    length: u64,
    flags: u32,
    address: u64,
    address_length: u64,
) -> Result<usize, Errno> {
    user_buffer(buffer, length)?;
    let (file, socket) = socket_of(process, descriptor)?;
    if flags & MSG_OOB != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let capacity = if address == 0 {
        0
    } else {
        buffer_length(process, address_length)?
    };
    let mut data = zeroed(clamped(length))?;
    let (received, from, _control) = socket.recv(&mut data, flags, file.status().nonblock)?;
    let taken = data.get(..received.bytes).unwrap_or_default();
    uaccess::copy_to_user(process.space(), buffer, taken).map_err(|_| Errno::EFAULT)?;
    if address != 0 {
        let encoded = from.unwrap_or_default();
        write_address(process, address, address_length, &encoded, capacity)?;
    }
    Ok(received_count(&socket, received, flags))
}

/// A program's `struct msghdr`, of `width`.
fn read_header(process: &Process, at: u64, width: Width) -> Result<MsgHdr, Errno> {
    let mut bytes = [0_u8; MsgHdr::size(Width::Bits64)];
    let header = bytes.get_mut(..MsgHdr::size(width)).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), at, header).map_err(|_| Errno::EFAULT)?;
    MsgHdr::decode(header, width).ok_or(Errno::EINVAL)
}

/// A message's iovecs, as (address, length) pairs: `EMSGSIZE` past
/// `UIO_MAXIOV`, `EINVAL` for a length that would make the total negative,
/// `EFAULT` for a buffer outside the user half, and the lengths clamped so the
/// total stays within `MAX_RW_COUNT` -- `import_iovec`'s rules. A length is
/// negative by the program's `ssize_t`, which `width` says the size of.
fn iovecs(process: &Process, message: &MsgHdr, width: Width) -> Result<Vec<(u64, usize)>, Errno> {
    if message.iov_len > UIO_MAXIOV {
        return Err(Errno::EMSGSIZE);
    }
    let count = usize::try_from(message.iov_len).map_err(|_| Errno::EMSGSIZE)?;
    let word = width.bytes();
    let most = if width == Width::Bits32 {
        i32::MAX as u64
    } else {
        i64::MAX as u64
    };
    let raw = copy_in(process, message.iov, count * word * 2)?;
    let mut segments = Vec::new();
    segments
        .try_reserve_exact(count)
        .map_err(|_| Errno::ENOMEM)?;
    let mut total = 0_usize;
    for index in 0..count {
        let at = index * word * 2;
        let base = width.word(&raw, at).ok_or(Errno::EINVAL)?;
        let length = width.word(&raw, at + word).ok_or(Errno::EINVAL)?;
        if length > most {
            return Err(Errno::EINVAL);
        }
        let length = usize::try_from(length)
            .map_err(|_| Errno::EINVAL)?
            .min(MAX_RW_COUNT - total);
        user_buffer(base, length as u64)?;
        total += length;
        segments.push((base, length));
    }
    Ok(segments)
}

/// A send's control messages, as `__scm_send` reads them: the files an
/// `SCM_RIGHTS` message names, gathered across every such message, and the
/// credentials an `SCM_CREDENTIALS` message names, for an `AF_UNIX` socket to
/// pass. A malformed buffer is `EINVAL`, and so is an unknown `SOL_SOCKET`
/// message or more than `SCM_MAX_FD` descriptors; a descriptor that is not
/// open is `EBADF`; credentials that are not the sender's own are `EPERM`,
/// unless the sender is root, as `scm_check_creds` has it; messages for other
/// levels are ignored. Either on any other family is `EOPNOTSUPP`.
///
/// The files are cloned out of the descriptor table under its lock and dropped,
/// if the send is refused, after it.
fn control(
    process: &Process,
    message: &MsgHdr,
    socket: &Any,
    width: Width,
) -> Result<Option<Passed>, Errno> {
    if message.control_len == 0 {
        return Ok(None);
    }
    let length = usize::try_from(message.control_len)
        .ok()
        .filter(|&length| length <= MAX_CONTROL)
        .ok_or(Errno::ENOBUFS)?;
    let control = copy_in(process, message.control, length)?;
    let mut descriptors: Vec<i32> = Vec::new();
    let mut rights = false;
    let mut credentials: Option<Ucred> = None;
    for entry in ControlMessages::new(&control, width) {
        let entry = entry.map_err(|_| Errno::EINVAL)?;
        if entry.level != SOL_SOCKET {
            continue;
        }
        match entry.kind {
            SCM_RIGHTS if matches!(socket, Any::Unix(_)) => {
                rights = true;
                let count = entry.data.len() / size_of::<i32>();
                if descriptors.len().saturating_add(count) > SCM_MAX_FD {
                    return Err(Errno::EINVAL);
                }
                descriptors.try_reserve(count).map_err(|_| Errno::ENOMEM)?;
                descriptors.extend(
                    entry
                        .data
                        .chunks_exact(size_of::<i32>())
                        .filter_map(|bytes| <[u8; 4]>::try_from(bytes).ok())
                        .map(i32::from_le_bytes),
                );
            }
            SCM_CREDENTIALS if matches!(socket, Any::Unix(_)) => {
                credentials = Some(named_credentials(process, entry.data)?);
            }
            SCM_RIGHTS | SCM_CREDENTIALS => return Err(Errno::EOPNOTSUPP),
            _ => return Err(Errno::EINVAL),
        }
    }
    let stamp = |passed: Passed| match credentials {
        Some(credentials) => passed.with_credentials(credentials),
        None => passed,
    };
    if !rights || descriptors.is_empty() {
        return credentials
            .map(|_| Passed::new(Vec::new()).map(stamp))
            .transpose();
    }
    let mut files = Vec::new();
    files
        .try_reserve_exact(descriptors.len())
        .map_err(|_| Errno::ENOMEM)?;
    let table = process.files().lock();
    for descriptor in descriptors {
        match table.get(descriptor) {
            Ok(file) => files.push(Arc::clone(file)),
            Err(errno) => {
                drop(table);
                drop(files);
                return Err(errno);
            }
        }
    }
    drop(table);
    Passed::new(files).map(|passed| Some(stamp(passed)))
}

/// The credentials an `SCM_CREDENTIALS` message names, if the sender may
/// name them: its own pid, and ids among its real, effective and saved
/// ones -- anything at all for root.
fn named_credentials(process: &Process, data: &[u8]) -> Result<Ucred, Errno> {
    let field = |at: usize| {
        data.get(at..at + 4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(u32::from_le_bytes)
            .ok_or(Errno::EINVAL)
    };
    // The ids are as the sender's namespace names them; the stamp holds
    // kernel ids, and an id the namespace does not map is refused.
    let named = Ucred {
        pid: field(0)?.cast_signed(),
        uid: credentials::kernel_uid(process, field(4)?)?,
        gid: credentials::kernel_gid(process, field(8)?)?,
    };
    let allowed = process.with_credentials(|credentials| {
        let (user, group) = (&credentials.user, &credentials.group);
        credentials.privileged()
            || (u32::try_from(named.pid).is_ok_and(|pid| pid == process.pid())
                && [user.real, user.effective, user.saved].contains(&named.uid)
                && [group.real, group.effective, group.saved].contains(&named.gid))
    });
    if allowed {
        Ok(named)
    } else {
        Err(Errno::EPERM)
    }
}

/// Install the files a receive took as the receiver's descriptors, as many as
/// `capacity` bytes of control buffer at `at` have room for, and write the
/// `SCM_RIGHTS` message naming them. Answers the control bytes used and whether
/// any file was left out -- no room, or no descriptor free -- which is
/// `MSG_CTRUNC`. Every file left out is dropped when `passed` is, after the
/// table's lock is gone.
fn deliver_files(
    process: &Process,
    passed: &Passed,
    (at, capacity): (u64, usize),
    flags: u32,
    width: Width,
) -> Result<(usize, bool), Errno> {
    let header = CmsgHdr::size(width);
    let room = if at == 0 {
        0
    } else {
        capacity.saturating_sub(header) / size_of::<i32>()
    };
    let wanted = passed.files().len().min(room);
    let mut installed: Vec<i32> = Vec::new();
    installed
        .try_reserve_exact(wanted)
        .map_err(|_| Errno::ENOMEM)?;
    {
        let mut table = process.files().lock();
        for file in passed.files().iter().take(wanted) {
            match table.insert(Arc::clone(file), flags & MSG_CMSG_CLOEXEC != 0) {
                Ok(descriptor) => installed.push(descriptor),
                Err(_) => break,
            }
        }
    }
    let truncated = installed.len() < passed.files().len();
    if installed.is_empty() {
        return Ok((0, truncated));
    }
    let data_len = installed.len() * size_of::<i32>();
    let mut bytes = zeroed(cmsg_len(data_len, width))?;
    CmsgHdr {
        len: cmsg_len(data_len, width) as u64,
        level: SOL_SOCKET,
        kind: SCM_RIGHTS,
    }
    .encode(&mut bytes, width)
    .ok_or(Errno::EINVAL)?;
    for (index, descriptor) in installed.iter().enumerate() {
        let slot = bytes
            .get_mut(header + index * size_of::<i32>()..header + (index + 1) * size_of::<i32>())
            .ok_or(Errno::EINVAL)?;
        slot.copy_from_slice(&descriptor.to_le_bytes());
    }
    uaccess::copy_to_user(process.space(), at, &bytes).map_err(|_| Errno::EFAULT)?;
    Ok((cmsg_space(data_len, width).min(capacity), truncated))
}

/// `sendmsg`: the message's buffers gathered into one send.
fn sys_sendmsg(
    process: &Process,
    descriptor: i32,
    header: u64,
    flags: u32,
    width: Width,
) -> Result<usize, Errno> {
    if flags & MSG_CMSG_COMPAT != 0 {
        return Err(Errno::EINVAL);
    }
    let (file, socket) = socket_of(process, descriptor)?;
    let message = read_header(process, header, width)?;
    let name_length = name_length_of(&message)?;
    check_destination(&socket, name_length)?;
    if flags & MSG_OOB != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let destination = if name_length == 0 {
        None
    } else {
        Some(read_address(process, message.name, name_length)?)
    };
    let segments = iovecs(process, &message, width)?;
    let passed = control(process, &message, &socket, width)?;
    let total = segments
        .iter()
        .map(|&(_, length)| length)
        .sum::<usize>()
        .min(MAX_TRANSFER);
    let mut data = zeroed(total)?;
    let mut filled = 0;
    for (base, length) in segments {
        let take = length.min(total - filled);
        if take == 0 {
            continue;
        }
        let slot = data.get_mut(filled..filled + take).ok_or(Errno::EINVAL)?;
        uaccess::copy_from_user(process.space(), base, slot).map_err(|_| Errno::EFAULT)?;
        filled += take;
    }
    match &socket {
        Any::Unix(unix) => unix.send_from(process, &data, flags, file.status().nonblock, passed),
        _ => socket.send(&data, flags, file.status().nonblock, destination.as_deref()),
    }
}

/// A Unix socket's control messages for `recvmsg`, into the `capacity` bytes
/// at `control`: the sender's credentials if the socket asked for them, then
/// the files that came with the bytes, in the order Linux's `scm_recv` writes
/// them. Answers the bytes used and whether anything was cut.
fn unix_control(
    process: &Process,
    unix: &Socket,
    passed: Option<&Passed>,
    (control, capacity): (u64, usize),
    (flags, width): (u32, Width),
) -> Result<(usize, bool), Errno> {
    let (mut used, mut cut) = if unix.passes_credentials() {
        let sender = fs::socket::sender_of(passed);
        let stamp = inet::Control {
            level: SOL_SOCKET,
            kind: SCM_CREDENTIALS,
            data: fs::socket::as_seen(sender).to_bytes().to_vec(),
        };
        write_control(process, (control, capacity), &[stamp], width)?
    } else {
        (0, false)
    };
    if let Some(passed) = passed.filter(|passed| !passed.files().is_empty()) {
        let at = if control == 0 {
            0
        } else {
            control.saturating_add(used as u64)
        };
        let (more, dropped) = deliver_files(process, passed, (at, capacity - used), flags, width)?;
        used += more;
        cut |= dropped;
    }
    Ok((used, cut))
}

/// `recvmsg`: one receive scattered over the message's buffers, with its
/// length fields and flags written back, and the descriptors that came with
/// the bytes installed. A peer without a name reports an address of length
/// zero.
fn sys_recvmsg(
    process: &Process,
    descriptor: i32,
    header: u64,
    flags: u32,
    width: Width,
) -> Result<usize, Errno> {
    if flags & MSG_CMSG_COMPAT != 0 {
        return Err(Errno::EINVAL);
    }
    let (file, socket) = socket_of(process, descriptor)?;
    let message = read_header(process, header, width)?;
    let _ = name_length_of(&message)?;
    if flags & MSG_OOB != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let segments = iovecs(process, &message, width)?;
    let total = segments
        .iter()
        .map(|&(_, length)| length)
        .sum::<usize>()
        .min(MAX_TRANSFER);
    let capacity = if message.name == 0 {
        0
    } else {
        usize::try_from(message.name_len).unwrap_or(0)
    };
    let mut data = zeroed(total)?;
    let (received, from, passed, control) = match &socket {
        Any::Unix(unix) => {
            let (received, passed) = unix.recv_passing(&mut data, flags, file.status().nonblock)?;
            (received, None, passed, Vec::new())
        }
        _ => {
            let (received, from, control) =
                socket.recv(&mut data, flags, file.status().nonblock)?;
            (received, from, None, control)
        }
    };
    let mut offset = 0;
    for (base, length) in segments {
        if offset >= received.bytes {
            break;
        }
        let take = length.min(received.bytes - offset);
        let piece = data.get(offset..offset + take).ok_or(Errno::EINVAL)?;
        uaccess::copy_to_user(process.space(), base, piece).map_err(|_| Errno::EFAULT)?;
        offset += take;
    }
    let field = |offset: usize| header.saturating_add(offset as u64);
    if message.name != 0 {
        let encoded = from.unwrap_or_default();
        write_address(
            process,
            message.name,
            field(MsgHdr::name_len_offset(width)),
            &encoded,
            capacity,
        )?;
    }
    let control_capacity = usize::try_from(message.control_len).unwrap_or(usize::MAX);
    let (control_used, control_truncated) = match &socket {
        Any::Unix(unix) => unix_control(
            process,
            unix,
            passed.as_ref(),
            (message.control, control_capacity),
            (flags, width),
        )?,
        _ => write_control(
            process,
            (message.control, control_capacity),
            &control,
            width,
        )?,
    };
    // Every file not installed is closed here, with the table unlocked.
    drop(passed);
    let truncated = socket.kind() != SocketType::Stream && received.full > received.bytes;
    let message_flags =
        if truncated { MSG_TRUNC } else { 0 } | if control_truncated { MSG_CTRUNC } else { 0 };
    uaccess::put_u32(
        process.space(),
        field(MsgHdr::flags_offset(width)),
        message_flags,
    )?;
    uaccess::put_word(
        process.space(),
        field(MsgHdr::control_len_offset(width)),
        control_used as u64,
    )?;
    Ok(received_count(&socket, received, flags))
}

/// Lay `messages` out one after another in the program's control buffer at
/// `control`, `capacity` bytes long, as `put_cmsg` does: a message that does
/// not fit whole is cut to what does, and one whose header does not fit is
/// left out, each saying `MSG_CTRUNC`. Answers the bytes used, padding
/// included, which is what `msg_controllen` reads afterwards, and whether any
/// message was cut.
fn write_control(
    process: &Process,
    (control, capacity): (u64, usize),
    messages: &[inet::Control],
    width: Width,
) -> Result<(usize, bool), Errno> {
    let header = CmsgHdr::size(width);
    let mut used = 0_usize;
    let mut cut = false;
    for message in messages {
        let left = capacity.saturating_sub(used);
        if control == 0 || left < header {
            cut = true;
            continue;
        }
        let whole = cmsg_len(message.data.len(), width);
        let length = whole.min(left);
        cut |= length < whole;
        let mut bytes = vec![0_u8; length];
        CmsgHdr {
            len: length as u64,
            level: message.level,
            kind: message.kind,
        }
        .encode(&mut bytes, width)
        .ok_or(Errno::EINVAL)?;
        let data_at = cmsg_align(header, width);
        for (slot, byte) in bytes.iter_mut().skip(data_at).zip(message.data.iter()) {
            *slot = *byte;
        }
        uaccess::copy_to_user(process.space(), control.saturating_add(used as u64), &bytes)
            .map_err(|_| Errno::EFAULT)?;
        used = used.saturating_add(cmsg_space(message.data.len(), width).min(left));
    }
    Ok((used, cut))
}

/// `getsockopt`: the option's value, cut to the length the program offers,
/// with the length written back.
fn sys_getsockopt(
    process: &Process,
    descriptor: i32,
    (level, name): (i32, i32),
    value: u64,
    length: u64,
    width: Width,
) -> Result<usize, Errno> {
    let (_file, socket) = socket_of(process, descriptor)?;
    let capacity = buffer_length(process, length)?;
    let bytes = socket.get_option(level, name, width)?;
    let shown = bytes.get(..capacity.min(bytes.len())).unwrap_or_default();
    uaccess::copy_to_user(process.space(), value, shown).map_err(|_| Errno::EFAULT)?;
    uaccess::put_u32(
        process.space(),
        length,
        u32::try_from(shown.len()).unwrap_or(u32::MAX),
    )?;
    Ok(0)
}

/// `setsockopt`. A negative length is `EINVAL` before the descriptor is
/// looked up, as on Linux.
fn sys_setsockopt(
    process: &Process,
    descriptor: i32,
    (level, name): (i32, i32),
    value: u64,
    length: i32,
    width: Width,
) -> Result<usize, Errno> {
    let length = usize::try_from(length).map_err(|_| Errno::EINVAL)?;
    let (_file, socket) = socket_of(process, descriptor)?;
    let bytes = copy_in(process, value, length.min(MAX_OPTION))?;
    socket.set_option(level, name, &bytes, width)?;
    Ok(0)
}
