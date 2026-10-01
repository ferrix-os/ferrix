//! An `AF_INET` or `AF_INET6` socket, as the inode its open file reads and
//! writes through.
//!
//! The socket itself lives in the stack and is named by a [`SocketId`]. What
//! is here is the shell around it: the inode a descriptor points at, the
//! waiting a blocking call does, the timeouts and options a program sets, and
//! the translation between Linux's `sockaddr_in` and the stack's endpoints.
//!
//! # A socket owns its place in the stack
//!
//! Dropping this closes the stack's socket. There is no other path: a
//! descriptor closed, a process exiting and a failed `accept` all end in the
//! same `Drop`, which is why a leaked connection is not possible without a
//! leaked `Arc`.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;

use ferrix_kmem::{Charge, arc_footprint};
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::inet::{
    ICMP_FILTER, ICMPV6_FILTER, IP_HDRINCL, IP_TOS, IP_TTL, IPPROTO_ICMP, IPPROTO_ICMPV6,
    IPPROTO_TCP, IPPROTO_UDP, IPV6_2292HOPLIMIT, IPV6_CHECKSUM, IPV6_HOPLIMIT, IPV6_RECVHOPLIMIT,
    IPV6_UNICAST_HOPS, IPV6_V6ONLY, InetAddress, SOCKADDR_STORAGE_SIZE, SOL_ICMPV6, SOL_IP,
    SOL_IPV6, SOL_RAW, SOL_TCP, TCP_MAXSEG, TCP_NODELAY,
};
use ferrix_linux_abi::socket::{
    AF_INET, AF_INET6, MSG_DONTWAIT, MSG_PEEK, SHUT_RD, SHUT_RDWR, SHUT_WR, SO_ACCEPTCONN,
    SO_BROADCAST, SO_DOMAIN, SO_ERROR, SO_KEEPALIVE, SO_PROTOCOL, SO_RCVBUF, SO_RCVTIMEO_NEW,
    SO_RCVTIMEO_OLD, SO_REUSEADDR, SO_SNDBUF, SO_SNDTIMEO_NEW, SO_SNDTIMEO_OLD, SO_TYPE,
    SOCK_DGRAM, SOCK_RAW, SOCK_STREAM, SOCKET_BUFFER_DEFAULT, SOL_SOCKET, Width,
};
use ferrix_net::socket::{Error, Family, Shutdown, Socket as NetSocket};
use ferrix_net::{Endpoint, IpAddress, Ipv4, Ipv6, SocketId, to_v6};
use ferrix_vfs::{Inode, Metadata, OpenFile, Readiness};

use crate::fs;
use crate::net::NetNamespace;
use crate::sync::SpinLock;

/// What kind of `AF_INET` socket this is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InetKind {
    /// `SOCK_STREAM`: TCP.
    Stream,
    /// `SOCK_DGRAM` at `IPPROTO_UDP`: UDP.
    Datagram,
    /// `SOCK_DGRAM` at `IPPROTO_ICMP`: the echo socket `ping` uses without
    /// privilege.
    Echo,
    /// `SOCK_RAW` at a protocol: every packet of it, header and all, which
    /// needs privilege.
    Raw {
        /// The protocol number it was opened with.
        protocol: u8,
    },
}

impl InetKind {
    /// The type `SO_TYPE` reports.
    const fn linux(self) -> u32 {
        match self {
            InetKind::Stream => SOCK_STREAM,
            InetKind::Datagram | InetKind::Echo => SOCK_DGRAM,
            InetKind::Raw { .. } => SOCK_RAW,
        }
    }

    /// The protocol `SO_PROTOCOL` reports, for a socket of this family.
    const fn protocol(self, family: Family) -> i32 {
        match (self, family) {
            (InetKind::Stream, _) => IPPROTO_TCP,
            (InetKind::Datagram, _) => IPPROTO_UDP,
            (InetKind::Echo, Family::V4) => IPPROTO_ICMP,
            (InetKind::Echo, Family::V6) => IPPROTO_ICMPV6,
            (InetKind::Raw { protocol }, _) => protocol as i32,
        }
    }

    /// Whether it carries a stream rather than records.
    const fn is_stream(self) -> bool {
        matches!(self, InetKind::Stream)
    }
}

/// What a program has set that the stack does not keep.
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
    /// `SO_KEEPALIVE`, kept and reported; nothing probes yet.
    keepalive: bool,
    /// `IPV6_RECVHOPLIMIT`: each datagram comes with an `IPV6_HOPLIMIT`
    /// control message.
    hop_limit_messages: bool,
    /// `IPV6_2292HOPLIMIT`: the same, in RFC 2292's type.
    hop_limit_messages_2292: bool,
}

/// An `AF_INET` or `AF_INET6` socket.
pub(crate) struct InetSocket {
    /// The network namespace it was made in, for life: the stack its id
    /// names a socket of.
    ns: Arc<NetNamespace>,
    /// Which socket in the stack.
    id: SocketId,
    /// What kind it is.
    kind: InetKind,
    /// Which family it was opened as.
    family: Family,
    /// What `stat` reports through it.
    metadata: Metadata,
    /// What a program has set.
    options: SpinLock<Options>,
    /// Its heap and its entry in the stack's table, charged to the job that
    /// opened or accepted it (certification finding F-37). A connection's
    /// own state and queues are charged in the stack.
    _charge: Charge,
}

/// What an open socket holds of the heap beside what the stack charges: the
/// socket, and its entry in the stack's table, B-tree nodes being at least
/// half full.
fn socket_charge() -> Result<Charge, Errno> {
    Charge::bytes(
        arc_footprint::<InetSocket>()
            .saturating_add(2 * size_of::<(u32, NetSocket)>())
            .saturating_add(2 * size_of::<((u64, SocketId), FileOf)>()),
    )
    .map_err(|_| Errno::ENOMEM)
}

/// The file a stack socket is open as: its inode number on sockfs and the
/// user that owns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileOf {
    /// What `/proc/<pid>/fd` spells as `socket:[ino]`.
    pub(crate) ino: u64,
    /// Who opened or accepted it.
    pub(crate) uid: u32,
}

/// Each stack socket that has a file, by its id.
///
/// `/proc/net/tcp` and its siblings print these, because a program that asks
/// who owns a connection -- `lsof`, `ss -p`, Steam's check of the websocket
/// its web helper opens -- finds the socket's row there and looks for the
/// same inode among the processes' descriptors. The stack's own id is not
/// that number. A socket still in a listener's backlog has no file yet, and
/// one lingering in `TIME_WAIT` has none any more; neither has an entry, and
/// Linux prints both inodes as 0 too.
///
/// Never taken with the net core held, and the net core is never taken with
/// this held.
static FILES: SpinLock<BTreeMap<(u64, SocketId), FileOf>> = SpinLock::new(BTreeMap::new());

/// The file socket `id` of network namespace `namespace` is open as, if it has
/// one.
pub(crate) fn file_of(namespace: u64, id: SocketId) -> Option<FileOf> {
    FILES.lock().get(&(namespace, id)).copied()
}

impl fmt::Debug for InetSocket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InetSocket")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("ino", &self.metadata.ino)
            .finish_non_exhaustive()
    }
}

impl Drop for InetSocket {
    fn drop(&mut self) {
        let _ = FILES.lock().remove(&(self.ns.id(), self.id));
        self.ns.core().with(|stack, _| stack.close(self.id));
    }
}

/// What a receive answered.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Received {
    /// Bytes copied into the caller's buffer.
    pub(crate) bytes: usize,
    /// What the whole record held, which `MSG_TRUNC` answers with.
    pub(crate) full: usize,
    /// The hop limit a datagram arrived with.
    pub(crate) hop_limit: Option<u8>,
}

/// One control message a receive hands back, before it is laid out in the
/// program's buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Control {
    /// `cmsg_level`.
    pub(crate) level: i32,
    /// `cmsg_type`.
    pub(crate) kind: i32,
    /// What follows the header.
    pub(crate) data: Vec<u8>,
}

impl InetSocket {
    /// Open a socket of `kind` in `family`, as the open file `socket`
    /// installs.
    ///
    /// # Errors
    ///
    /// Whatever [`OpenFile::new`] refuses, which for a socket is nothing.
    pub(crate) fn open(
        ns: &Arc<NetNamespace>,
        family: Family,
        kind: InetKind,
        nonblock: bool,
        owner: (u32, u32),
    ) -> Result<Arc<OpenFile>, Errno> {
        // Before the stack has a socket to leak if it is refused.
        let charge = socket_charge()?;
        let id = ns.core().with(|stack, _| match kind {
            InetKind::Stream => stack.open_tcp(family),
            InetKind::Datagram => stack.open_udp(family),
            InetKind::Echo => stack.open_icmp(family),
            InetKind::Raw { protocol } => stack.open_raw(family, protocol),
        });
        Self::wrap(ns, id, family, kind, nonblock, owner, charge)
    }

    /// Put an existing stack socket behind an open file, which is what
    /// `accept` does with the connection it takes.
    fn wrap(
        ns: &Arc<NetNamespace>,
        id: SocketId,
        family: Family,
        kind: InetKind,
        nonblock: bool,
        owner: (u32, u32),
        charge: Charge,
    ) -> Result<Arc<OpenFile>, Errno> {
        let ino = fs::socket::next_ino();
        let socket = Arc::new(InetSocket {
            ns: Arc::clone(ns),
            id,
            kind,
            family,
            metadata: fs::socket::socket_metadata(ino, owner),
            options: SpinLock::new(Options {
                receive_timeout: 0,
                send_timeout: 0,
                send_buffer: SOCKET_BUFFER_DEFAULT,
                receive_buffer: SOCKET_BUFFER_DEFAULT,
                keepalive: false,
                hop_limit_messages: false,
                hop_limit_messages_2292: false,
            }),
            _charge: charge,
        });
        let _ = FILES
            .lock()
            .insert((ns.id(), id), FileOf { ino, uid: owner.0 });
        fs::socket::open_on_sockfs(socket, ino, nonblock)
    }

    /// Its type.
    pub(crate) const fn kind(&self) -> InetKind {
        self.kind
    }

    /// Whether it has a peer.
    pub(crate) fn is_connected(&self) -> bool {
        self.ns
            .core()
            .with(|stack, _| stack.remote_endpoint(self.id).is_some())
    }

    /// What it can do right now.
    pub(crate) fn readiness(&self) -> Readiness {
        let ready = self.ns.core().look(|stack| stack.readiness(self.id));
        Readiness {
            readable: ready.readable,
            writable: ready.writable,
            hangup: ready.hangup,
            error: ready.error,
            priority: false,
        }
    }

    /// Give it a name.
    pub(crate) fn bind(&self, raw: &[u8]) -> Result<(), Errno> {
        let endpoint = self.endpoint_of(raw)?;
        self.ns
            .core()
            .with(|stack, _| stack.bind(self.id, endpoint))
            .map_err(errno)
    }

    /// Start listening.
    pub(crate) fn listen(&self, backlog: i32) -> Result<(), Errno> {
        if !self.kind.is_stream() {
            return Err(Errno::EOPNOTSUPP);
        }
        // At most `SOMAXCONN`, as Linux caps it: connections waiting to be
        // accepted are charged to this socket's job, but a backlog of a
        // billion is still a program's mistake.
        let backlog = usize::try_from(backlog.max(0))
            .unwrap_or(0)
            .min(ferrix_linux_abi::socket::SOMAXCONN);
        self.ns
            .core()
            .with(|stack, _| stack.listen(self.id, backlog))
            .map_err(errno)
    }

    /// Take a connection that finished its handshake.
    ///
    /// `nonblock` is the listener's `O_NONBLOCK`, which decides only whether
    /// to wait; the new socket is non-blocking when `accepted_nonblock`
    /// (`accept4`'s `SOCK_NONBLOCK`) says so, as on Linux.
    pub(crate) fn accept(
        &self,
        nonblock: bool,
        accepted_nonblock: bool,
        owner: (u32, u32),
    ) -> Result<(Arc<OpenFile>, Vec<u8>), Errno> {
        if !self.kind.is_stream() {
            return Err(Errno::EOPNOTSUPP);
        }
        let deadline = self.deadline(self.options.lock().receive_timeout, nonblock);
        loop {
            // Before a connection is taken, which a refusal would leak.
            let charge = socket_charge()?;
            let taken = self.ns.core().with(|stack, _| stack.accept(self.id));
            match taken {
                Ok(id) => {
                    let peer = self
                        .ns
                        .core()
                        .with(|stack, _| stack.remote_endpoint(id))
                        .unwrap_or(Endpoint::new(self.unspecified(), 0));
                    let file = Self::wrap(
                        &self.ns,
                        id,
                        self.family,
                        self.kind,
                        accepted_nonblock,
                        owner,
                        charge,
                    )?;
                    return Ok((file, self.encode(peer)));
                }
                Err(Error::WouldBlock) if nonblock => return Err(Errno::EAGAIN),
                Err(Error::WouldBlock) => {}
                Err(other) => return Err(errno(other)),
            }
            self.wait(|| self.readiness().readable, deadline)?;
        }
    }

    /// Connect to a peer.
    ///
    /// A stream socket's connect blocks until the handshake finishes, unless
    /// the descriptor is non-blocking, in which case it answers `EINPROGRESS`
    /// and the program waits with `poll`, as Linux does.
    pub(crate) fn connect(&self, raw: &[u8], nonblock: bool) -> Result<(), Errno> {
        let endpoint = self.endpoint_of(raw)?;
        let started = self
            .ns
            .core()
            .with(|stack, at| stack.connect(self.id, endpoint, at));
        match started {
            Ok(()) => {}
            Err(Error::AlreadyDone) if self.is_connected() => return Err(Errno::EISCONN),
            Err(Error::AlreadyDone) => return Err(Errno::EALREADY),
            Err(other) => return Err(errno(other)),
        }
        if !self.kind.is_stream() {
            return Ok(());
        }
        if nonblock {
            return Err(Errno::EINPROGRESS);
        }
        let deadline = self.deadline(self.options.lock().send_timeout, false);
        loop {
            let ready = self.readiness();
            if ready.error {
                let error = self.ns.core().with(|stack, _| stack.take_error(self.id));
                return Err(error.map_or(Errno::ECONNREFUSED, errno));
            }
            if ready.writable {
                return Ok(());
            }
            self.wait(
                || {
                    let ready = self.readiness();
                    ready.writable || ready.error
                },
                deadline,
            )?;
        }
    }

    /// Stop one or both directions.
    pub(crate) fn shutdown(&self, how: u32) -> Result<(), Errno> {
        let how = match how {
            SHUT_RD => Shutdown::Read,
            SHUT_WR => Shutdown::Write,
            SHUT_RDWR => Shutdown::Both,
            _ => return Err(Errno::EINVAL),
        };
        if self.kind.is_stream() && !self.is_connected() {
            return Err(Errno::ENOTCONN);
        }
        self.ns
            .core()
            .with(|stack, _| stack.shutdown(self.id, how))
            .map_err(errno)
    }

    /// Send `data`, to `to` if it is given.
    pub(crate) fn send(
        &self,
        data: &[u8],
        flags: u32,
        nonblock: bool,
        to: Option<&[u8]>,
    ) -> Result<usize, Errno> {
        let destination = match to {
            Some(raw) => Some(self.endpoint_of(raw)?),
            None => None,
        };
        let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
        let deadline = self.deadline(self.options.lock().send_timeout, nonblock);
        loop {
            let sent = self
                .ns
                .core()
                .with(|stack, at| stack.send(self.id, data, destination, at));
            match sent {
                Ok(count) => return Ok(count),
                Err(Error::WouldBlock) if nonblock => return Err(Errno::EAGAIN),
                Err(Error::WouldBlock) => {}
                Err(Error::InProgress) if nonblock => return Err(Errno::EAGAIN),
                Err(Error::InProgress) => {}
                Err(other) => return Err(errno(other)),
            }
            self.wait(
                || {
                    let ready = self.readiness();
                    ready.writable || ready.error || ready.hangup
                },
                deadline,
            )?;
        }
    }

    /// Take received bytes, and the address they came from.
    pub(crate) fn recv(
        &self,
        out: &mut [u8],
        flags: u32,
        nonblock: bool,
    ) -> Result<(Received, Option<Vec<u8>>), Errno> {
        let peek = flags & MSG_PEEK != 0;
        let nonblock = nonblock || flags & MSG_DONTWAIT != 0;
        let deadline = self.deadline(self.options.lock().receive_timeout, nonblock);
        loop {
            let taken = self
                .ns
                .core()
                .with(|stack, _| stack.recv(self.id, out, peek));
            match taken {
                Ok(received) => {
                    let full = received.bytes + received.truncated;
                    let from = received.remote.map(|endpoint| self.encode(endpoint));
                    return Ok((
                        Received {
                            bytes: received.bytes,
                            full,
                            hop_limit: received.hop_limit,
                        },
                        from,
                    ));
                }
                Err(Error::WouldBlock) if nonblock => return Err(Errno::EAGAIN),
                Err(Error::WouldBlock) => {}
                Err(other) => return Err(errno(other)),
            }
            self.wait(
                || {
                    let ready = self.readiness();
                    ready.readable || ready.error || ready.hangup
                },
                deadline,
            )?;
        }
    }

    /// The control messages a receive that answered `received` hands back,
    /// in the order `ip6_datagram_recv_specific_ctl` puts them: RFC 3542's
    /// hop limit, then RFC 2292's. Each is an `int`.
    pub(crate) fn control_messages(&self, received: &Received) -> Vec<Control> {
        let options = *self.options.lock();
        let mut messages = Vec::new();
        let Some(hop_limit) = received.hop_limit else {
            return messages;
        };
        if matches!(self.family, Family::V4) || self.kind.is_stream() {
            return messages;
        }
        let data = i32::from(hop_limit).to_ne_bytes().to_vec();
        if options.hop_limit_messages {
            messages.push(Control {
                level: SOL_IPV6,
                kind: IPV6_HOPLIMIT,
                data: data.clone(),
            });
        }
        if options.hop_limit_messages_2292 {
            messages.push(Control {
                level: SOL_IPV6,
                kind: IPV6_2292HOPLIMIT,
                data,
            });
        }
        messages
    }

    /// The name it is bound to, encoded as a `sockaddr`.
    pub(crate) fn local_name(&self) -> Vec<u8> {
        let endpoint = self
            .ns
            .core()
            .with(|stack, _| stack.local_endpoint(self.id))
            .unwrap_or(Endpoint::new(self.unspecified(), 0));
        self.encode(endpoint)
    }

    /// The name of its peer, if it has one.
    pub(crate) fn peer_name(&self) -> Option<Vec<u8>> {
        let endpoint = self
            .ns
            .core()
            .with(|stack, _| stack.remote_endpoint(self.id))?;
        Some(self.encode(endpoint))
    }

    /// The unspecified address of this socket's family.
    fn unspecified(&self) -> IpAddress {
        match self.family {
            Family::V4 => IpAddress::V4(Ipv4::UNSPECIFIED),
            Family::V6 => IpAddress::V6(Ipv6::UNSPECIFIED),
        }
    }

    /// Read a `sockaddr` a program passed, and refuse one of the wrong family.
    fn endpoint_of(&self, raw: &[u8]) -> Result<Endpoint, Errno> {
        let address = InetAddress::parse(raw, raw.len()).map_err(|_| Errno::EINVAL)?;
        let wanted = match self.family {
            Family::V4 => AF_INET,
            Family::V6 => AF_INET6,
        };
        if address.family() != wanted {
            return Err(Errno::EAFNOSUPPORT);
        }
        Ok(match address {
            InetAddress::V4 { port, address } => {
                Endpoint::new(IpAddress::V4(Ipv4::new(address)), port)
            }
            InetAddress::V6 { port, address, .. } => {
                let six = Ipv6::new(address);
                match six.v4_mapped() {
                    Some(four) => Endpoint::new(IpAddress::V4(four), port),
                    None => Endpoint::new(IpAddress::V6(six), port),
                }
            }
        })
    }

    /// Write an endpoint as the `sockaddr` this socket's family uses.
    ///
    /// An `AF_INET6` socket reports an IPv4 peer as `::ffff:a.b.c.d`, which is
    /// what a program that opened one expects to read back.
    fn encode(&self, endpoint: Endpoint) -> Vec<u8> {
        let address = match self.family {
            Family::V4 => endpoint.address,
            Family::V6 => to_v6(endpoint.address),
        };
        let inet = match address {
            IpAddress::V4(four) => InetAddress::V4 {
                port: endpoint.port,
                address: four.octets(),
            },
            IpAddress::V6(six) => InetAddress::V6 {
                port: endpoint.port,
                flow_info: 0,
                address: six.octets(),
                scope_id: 0,
            },
        };
        let mut bytes = vec![0_u8; SOCKADDR_STORAGE_SIZE];
        let written = inet.encode(&mut bytes).unwrap_or(0);
        bytes.truncate(written);
        bytes
    }

    /// When a wait must give up.
    fn deadline(&self, timeout: u64, nonblock: bool) -> u64 {
        if nonblock {
            return 0;
        }
        fs::socket::deadline_after(timeout)
    }

    /// Wait for the stack to move, or for a signal or the deadline.
    fn wait(&self, ready: impl FnMut() -> bool, deadline: u64) -> Result<(), Errno> {
        fs::socket::wait_on(self.ns.core().progress(), ready, deadline)
    }
}

/// What a socket option reads or writes.
impl InetSocket {
    /// `getsockopt`: the option's value, in the layout a program reads.
    ///
    /// # Errors
    ///
    /// `ENOPROTOOPT` for an option this socket does not have.
    pub(crate) fn get_option(&self, level: i32, name: i32, width: Width) -> Result<Vec<u8>, Errno> {
        let options = *self.options.lock();
        match (level, name) {
            (SOL_SOCKET, SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW) => {
                Ok(fs::socket::timeval(options.receive_timeout, width))
            }
            (SOL_SOCKET, SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW) => {
                Ok(fs::socket::timeval(options.send_timeout, width))
            }
            (SOL_ICMPV6, ICMPV6_FILTER) => self
                .icmp6_filter(|filter| filter.iter().flat_map(|word| word.to_ne_bytes()).collect()),
            _ => Ok(self.option_value(level, name)?.to_le_bytes().to_vec()),
        }
    }

    /// The number an option reads as.
    fn option_value(&self, level: i32, name: i32) -> Result<i32, Errno> {
        let options = *self.options.lock();
        match (level, name) {
            (SOL_SOCKET, SO_TYPE) => Ok(self.kind.linux().cast_signed()),
            (SOL_SOCKET, SO_DOMAIN) => Ok(match self.family {
                Family::V4 => i32::from(AF_INET),
                Family::V6 => i32::from(AF_INET6),
            }),
            (SOL_SOCKET, SO_PROTOCOL) => Ok(self.kind.protocol(self.family)),
            (SOL_SOCKET, SO_ACCEPTCONN) => Ok(i32::from(self.is_listening())),
            (SOL_SOCKET, SO_ERROR) => {
                let error = self.ns.core().with(|stack, _| stack.take_error(self.id));
                Ok(error.map_or(0, |error| i32::from(errno(error).0)))
            }
            (SOL_SOCKET, SO_SNDBUF) => Ok(i32::try_from(options.send_buffer).unwrap_or(i32::MAX)),
            (SOL_SOCKET, SO_RCVBUF) => {
                Ok(i32::try_from(options.receive_buffer).unwrap_or(i32::MAX))
            }
            (SOL_SOCKET, SO_KEEPALIVE) => Ok(i32::from(options.keepalive)),
            (SOL_SOCKET, SO_REUSEADDR) => Ok(i32::from(self.stack_options().reuse_address)),
            (SOL_SOCKET, SO_BROADCAST) => Ok(i32::from(self.stack_options().broadcast)),
            (SOL_IP, IP_TTL) | (SOL_IPV6, IPV6_UNICAST_HOPS) => {
                Ok(i32::from(self.stack_options().hop_limit))
            }
            (SOL_IP, IP_TOS) => Ok(i32::from(self.stack_options().traffic_class)),
            (SOL_IP, IP_HDRINCL) => self.with_raw(|raw| i32::from(raw.header_included)),
            (SOL_RAW, ICMP_FILTER) => self.icmp_filter().map(u32::cast_signed),
            (SOL_IPV6, IPV6_V6ONLY) => Ok(i32::from(self.stack_options().v6_only)),
            (SOL_IPV6, IPV6_RECVHOPLIMIT) if matches!(self.family, Family::V6) => {
                Ok(i32::from(options.hop_limit_messages))
            }
            (SOL_IPV6, IPV6_2292HOPLIMIT) if matches!(self.family, Family::V6) => {
                Ok(i32::from(options.hop_limit_messages_2292))
            }
            (SOL_RAW | SOL_IPV6, IPV6_CHECKSUM) => self.checksum_offset(level),
            (SOL_TCP, TCP_NODELAY) => Ok(i32::from(self.nodelay())),
            (SOL_TCP, TCP_MAXSEG) => Ok(self.segment_size()),
            _ => Err(Errno::ENOPROTOOPT),
        }
    }

    /// Whether it is a listening socket.
    fn is_listening(&self) -> bool {
        self.ns.core().with(|stack, _| {
            matches!(stack.socket(self.id), Some(NetSocket::Listen(listener)) if listener.backlog > 0)
        })
    }

    /// `setsockopt`.
    ///
    /// # Errors
    ///
    /// `ENOPROTOOPT` for a level this socket does not have, `EINVAL` for a
    /// value that is the wrong size or out of range.
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
            let mut options = self.options.lock();
            if name == SO_RCVTIMEO_OLD || name == SO_RCVTIMEO_NEW {
                options.receive_timeout = nanos;
            } else {
                options.send_timeout = nanos;
            }
            return Ok(());
        }
        if (level, name) == (SOL_RAW, ICMP_FILTER) {
            return self.set_icmp_filter(value);
        }
        if (level, name) == (SOL_ICMPV6, ICMPV6_FILTER) {
            return self.set_icmp6_filter(value);
        }
        let number = read_int(value)?;
        match (level, name) {
            (SOL_SOCKET, SO_SNDBUF) => {
                self.options.lock().send_buffer = clamp_buffer(number);
                Ok(())
            }
            (SOL_SOCKET, SO_RCVBUF) => {
                self.options.lock().receive_buffer = clamp_buffer(number);
                Ok(())
            }
            (SOL_SOCKET, SO_KEEPALIVE) => {
                self.options.lock().keepalive = number != 0;
                Ok(())
            }
            (SOL_SOCKET, SO_REUSEADDR) => {
                self.with_options(|options| options.reuse_address = number != 0);
                Ok(())
            }
            (SOL_SOCKET, SO_BROADCAST) => {
                self.with_options(|options| options.broadcast = number != 0);
                Ok(())
            }
            (SOL_IP, IP_TTL) | (SOL_IPV6, IPV6_UNICAST_HOPS) => {
                if !(-1..=255).contains(&number) {
                    return Err(Errno::EINVAL);
                }
                let hops = u8::try_from(number).unwrap_or(64);
                self.with_options(|options| options.hop_limit = hops.max(1));
                Ok(())
            }
            (SOL_IP, IP_TOS) => {
                let class = u8::try_from(number & 0xFF).unwrap_or(0);
                self.with_options(|options| options.traffic_class = class);
                Ok(())
            }
            (SOL_IPV6, IPV6_V6ONLY) => {
                if matches!(self.family, Family::V4) {
                    return Err(Errno::ENOPROTOOPT);
                }
                self.with_options(|options| options.v6_only = number != 0);
                Ok(())
            }
            (SOL_IPV6, IPV6_RECVHOPLIMIT | IPV6_2292HOPLIMIT) => {
                if matches!(self.family, Family::V4) {
                    return Err(Errno::ENOPROTOOPT);
                }
                let mut options = self.options.lock();
                if name == IPV6_RECVHOPLIMIT {
                    options.hop_limit_messages = number != 0;
                } else {
                    options.hop_limit_messages_2292 = number != 0;
                }
                Ok(())
            }
            (SOL_RAW | SOL_IPV6, IPV6_CHECKSUM) => self.set_checksum_offset(level, number),
            (SOL_TCP, TCP_NODELAY) => {
                self.set_nodelay(number != 0);
                Ok(())
            }
            (SOL_IP, IP_HDRINCL) => self.with_raw(|raw| raw.header_included = number != 0),
            // An option a program may set that changes nothing here is
            // accepted rather than refused: refusing one makes a program that
            // sets it for luck fail, and Linux accepts them all.
            (SOL_SOCKET | SOL_IP | SOL_IPV6 | SOL_TCP, _) => Ok(()),
            _ => Err(Errno::ENOPROTOOPT),
        }
    }

    /// The options the stack keeps for this socket.
    fn stack_options(&self) -> ferrix_net::socket::Options {
        self.ns.core().with(|stack, _| {
            stack
                .socket(self.id)
                .map_or(ferrix_net::socket::Options::default(), NetSocket::options)
        })
    }

    /// Change the options the stack keeps.
    fn with_options(&self, body: impl FnOnce(&mut ferrix_net::socket::Options)) {
        self.ns.core().with(|stack, _| {
            if let Some(socket) = stack.socket_mut(self.id) {
                body(socket.options_mut());
            }
        });
    }

    /// Read or change the stack's side of a raw socket.
    ///
    /// # Errors
    ///
    /// `ENOPROTOOPT` on any other socket: `IP_HDRINCL` is `do_ip_setsockopt`'s
    /// and refused there unless the socket is `SOCK_RAW`.
    fn with_raw<T>(
        &self,
        body: impl FnOnce(&mut ferrix_net::socket::RawSocket) -> T,
    ) -> Result<T, Errno> {
        self.ns
            .core()
            .with(|stack, _| match stack.socket_mut(self.id) {
                Some(NetSocket::Raw(raw)) => Ok(body(raw)),
                _ => Err(Errno::ENOPROTOOPT),
            })
    }

    /// `ICMP_FILTER`'s mask, on a raw ICMP socket.
    ///
    /// # Errors
    ///
    /// `EOPNOTSUPP` on a raw socket of another protocol, as `raw_geticmpfilter`
    /// answers, and `ENOPROTOOPT` on a socket that is not raw or not IPv4,
    /// whose `SOL_RAW` has `IPV6_CHECKSUM` alone.
    fn icmp_filter(&self) -> Result<u32, Errno> {
        if matches!(self.family, Family::V6) {
            return Err(Errno::ENOPROTOOPT);
        }
        self.with_raw(|raw| {
            (raw.protocol == ICMP_PROTOCOL)
                .then_some(raw.icmp_filter)
                .ok_or(Errno::EOPNOTSUPP)
        })?
    }

    /// Set `ICMP_FILTER`: at most four bytes are read, as `raw_seticmpfilter`
    /// reads them, and fewer leave the remaining bits zero.
    fn set_icmp_filter(&self, value: &[u8]) -> Result<(), Errno> {
        if matches!(self.family, Family::V6) {
            return Err(Errno::ENOPROTOOPT);
        }
        let mut mask = [0_u8; 4];
        for (slot, byte) in mask.iter_mut().zip(value.iter()) {
            *slot = *byte;
        }
        self.with_raw(|raw| {
            if raw.protocol != ICMP_PROTOCOL {
                return Err(Errno::EOPNOTSUPP);
            }
            raw.icmp_filter = u32::from_ne_bytes(mask);
            Ok(())
        })?
    }

    /// Read `ICMP6_FILTER` through `body`, on a raw ICMPv6 socket.
    ///
    /// # Errors
    ///
    /// `EOPNOTSUPP` on a raw IPv6 socket of another protocol, as
    /// `rawv6_geticmpfilter` answers, and `ENOPROTOOPT` on any other socket,
    /// whose `SOL_ICMPV6` is `ipv6_getsockopt`'s and has nothing.
    fn icmp6_filter<T>(&self, body: impl FnOnce(&[u32; 8]) -> T) -> Result<T, Errno> {
        if matches!(self.family, Family::V4) {
            return Err(Errno::ENOPROTOOPT);
        }
        self.with_raw(|raw| {
            if raw.protocol != ICMPV6_PROTOCOL {
                return Err(Errno::EOPNOTSUPP);
            }
            Ok(body(&raw.icmp6_filter))
        })?
    }

    /// Set `ICMP6_FILTER`: at most its 32 bytes are read, and fewer leave the
    /// rest of the filter as it was, as `rawv6_seticmpfilter` copies them.
    fn set_icmp6_filter(&self, value: &[u8]) -> Result<(), Errno> {
        if matches!(self.family, Family::V4) {
            return Err(Errno::ENOPROTOOPT);
        }
        self.with_raw(|raw| {
            if raw.protocol != ICMPV6_PROTOCOL {
                return Err(Errno::EOPNOTSUPP);
            }
            let mut bytes: Vec<u8> = raw
                .icmp6_filter
                .iter()
                .flat_map(|word| word.to_ne_bytes())
                .collect();
            for (slot, byte) in bytes.iter_mut().zip(value.iter()) {
                *slot = *byte;
            }
            for (word, chunk) in raw.icmp6_filter.iter_mut().zip(bytes.chunks_exact(4)) {
                if let Ok(four) = <[u8; 4]>::try_from(chunk) {
                    *word = u32::from_ne_bytes(four);
                }
            }
            Ok(())
        })?
    }

    /// `IPV6_CHECKSUM` as it reads: the offset, or -1 for none.
    ///
    /// # Errors
    ///
    /// `ENOPROTOOPT` on a socket that is not a raw IPv6 one, and at
    /// `SOL_IPV6` on an ICMPv6 one, which `rawv6_getsockopt` hands to
    /// `ipv6_getsockopt` there.
    fn checksum_offset(&self, level: i32) -> Result<i32, Errno> {
        if matches!(self.family, Family::V4) {
            return Err(Errno::ENOPROTOOPT);
        }
        self.with_raw(|raw| {
            if level == SOL_IPV6 && raw.protocol == ICMPV6_PROTOCOL {
                return Err(Errno::ENOPROTOOPT);
            }
            Ok(raw
                .checksum
                .map_or(-1, |offset| i32::try_from(offset).unwrap_or(i32::MAX)))
        })?
    }

    /// Set `IPV6_CHECKSUM`: a negative offset turns the checksum off, and an
    /// odd one is `EINVAL`, as is the option at `SOL_IPV6` on an ICMPv6
    /// socket, whose checksum RFC 3542 says a program may not turn off.
    fn set_checksum_offset(&self, level: i32, offset: i32) -> Result<(), Errno> {
        if matches!(self.family, Family::V4) {
            return Err(Errno::ENOPROTOOPT);
        }
        self.with_raw(|raw| {
            if level == SOL_IPV6 && raw.protocol == ICMPV6_PROTOCOL {
                return Err(Errno::EINVAL);
            }
            if offset > 0 && offset & 1 != 0 {
                return Err(Errno::EINVAL);
            }
            raw.checksum = usize::try_from(offset).ok();
            Ok(())
        })?
    }

    /// Whether Nagle's algorithm is off.
    fn nodelay(&self) -> bool {
        self.ns.core().with(|stack, _| match stack.socket(self.id) {
            Some(NetSocket::Stream(stream)) => stream.connection.config().no_delay,
            _ => false,
        })
    }

    /// Turn Nagle's algorithm off or on.
    fn set_nodelay(&self, off: bool) {
        self.ns.core().with(|stack, _| {
            if let Some(NetSocket::Stream(stream)) = stack.socket_mut(self.id) {
                stream.connection.set_no_delay(off);
            }
        });
    }

    /// The largest segment the connection sends.
    fn segment_size(&self) -> i32 {
        self.ns.core().with(|stack, _| match stack.socket(self.id) {
            Some(NetSocket::Stream(stream)) => i32::from(stream.connection.segment_size()),
            _ => i32::from(ferrix_nettcp::conn::DEFAULT_MSS),
        })
    }

    /// `SIOCINQ` and `SIOCOUTQ`, which ask what is queued each way.
    ///
    /// # Errors
    ///
    /// `ENOTTY` for any other request, which is what a socket answers.
    pub(crate) fn ioctl(
        &self,
        process: &crate::syscall::process::Process,
        request: u32,
        arg: u64,
    ) -> Result<usize, Errno> {
        let count = match request {
            ferrix_linux_abi::socket::SIOCINQ => self.queued(),
            ferrix_linux_abi::socket::SIOCOUTQ => self.unsent(),
            // Everything else is about an interface rather than about this
            // socket, and `sys_ioctl` sends it on to `net::ifreq` for every
            // socket family alike.
            _ => return Err(Errno::ENOTTY),
        };
        crate::syscall::uaccess::put_u32(
            process.space(),
            arg,
            u32::try_from(count).unwrap_or(u32::MAX),
        )?;
        Ok(0)
    }

    /// How many bytes are waiting to be read, which `SIOCINQ` reports.
    fn queued(&self) -> usize {
        self.ns.core().with(|stack, _| match stack.socket(self.id) {
            Some(NetSocket::Stream(stream)) => stream.connection.receive_queued(),
            Some(NetSocket::Udp(socket) | NetSocket::Icmp(socket)) => {
                socket.peek().map_or(0, |datagram| datagram.payload.len())
            }
            Some(NetSocket::Raw(raw)) => raw
                .datagram
                .peek()
                .map_or(0, |datagram| datagram.payload.len()),
            _ => 0,
        })
    }

    /// How many bytes are waiting to be sent, which `SIOCOUTQ` reports.
    fn unsent(&self) -> usize {
        self.ns.core().with(|stack, _| match stack.socket(self.id) {
            Some(NetSocket::Stream(stream)) => stream.connection.send_queued(),
            _ => 0,
        })
    }
}

impl Inode for InetSocket {
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

    fn write_stream(&self, data: &[u8], nonblock: bool) -> ferrix_vfs::Result<usize> {
        self.send(data, 0, nonblock, None)
    }
}

/// The socket an open file reads and writes through, if it is an inet one.
pub(crate) fn of(file: &OpenFile) -> Option<Arc<InetSocket>> {
    Arc::clone(file.io())
        .into_any()
        .downcast::<InetSocket>()
        .ok()
}

/// Which errno the net core's answer is.
pub(crate) const fn errno(error: Error) -> Errno {
    match error {
        Error::NoSocket => Errno::EBADF,
        Error::WrongKind => Errno::EOPNOTSUPP,
        Error::AlreadyDone => Errno::EINVAL,
        Error::NotConnected => Errno::EDESTADDRREQ,
        Error::AddressInUse => Errno::EADDRINUSE,
        Error::AddressNotAvailable => Errno::EADDRNOTAVAIL,
        Error::Unreachable => Errno::ENETUNREACH,
        Error::Refused | Error::PortUnreachable => Errno::ECONNREFUSED,
        Error::Reset => Errno::ECONNRESET,
        Error::InProgress => Errno::EAGAIN,
        Error::WouldBlock => Errno::EAGAIN,
        Error::TooLarge => Errno::EMSGSIZE,
        Error::ShutDown => Errno::EPIPE,
        Error::Invalid => Errno::EINVAL,
        Error::NoMemory => Errno::ENOBUFS,
        Error::TimedOut => Errno::ETIMEDOUT,
        Error::NoDevice => Errno::ENODEV,
        Error::NetworkDown => Errno::ENETDOWN,
    }
}

/// ICMP's protocol number, as a raw socket's protocol holds it.
const ICMP_PROTOCOL: u8 = IPPROTO_ICMP as u8;

/// ICMPv6's protocol number as a raw socket holds it.
const ICMPV6_PROTOCOL: u8 = IPPROTO_ICMPV6 as u8;

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
