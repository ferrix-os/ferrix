//! An `AF_PACKET` socket, as the inode its open file reads and writes
//! through.
//!
//! The socket lives in the stack, as an internet socket does, and is named by
//! a [`SocketId`]; what is here is the shell: `sockaddr_ll` both ways, the
//! waiting a blocking receive does, and the options a program sets. See
//! `ferrix_net::packet` for what the socket itself does and does not do yet.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::socket::{
    AF_PACKET, MSG_DONTWAIT, MSG_PEEK, SO_DOMAIN, SO_ERROR, SO_PROTOCOL, SO_RCVBUF,
    SO_RCVTIMEO_NEW, SO_RCVTIMEO_OLD, SO_SNDBUF, SO_SNDTIMEO_NEW, SO_SNDTIMEO_OLD, SO_TYPE,
    SOCK_DGRAM, SOCK_RAW, SOCKADDR_LL_ADDR_OFFSET, SOCKADDR_LL_SIZE, SOCKET_BUFFER_DEFAULT,
    SOL_SOCKET, Width,
};
use ferrix_net::SocketId;
use ferrix_net::packet::{LinkAddress, PacketKind};
use ferrix_vfs::{Inode, Metadata, OpenFile, Readiness};

use crate::fs;
use crate::net::NetNamespace;
use crate::net::socket::Received;
use crate::sync::SpinLock;

/// `ARPHRD_ETHER`, the hardware type every interface that carries frames has
/// here.
const ARPHRD_ETHER: u16 = 1;

/// How long an Ethernet hardware address is.
const ETHERNET_ADDRESS: u8 = 6;

/// What a program has set.
#[derive(Clone, Copy, Debug)]
struct Options {
    /// `SO_RCVTIMEO`, in nanoseconds; zero waits forever.
    receive_timeout: u64,
    /// `SO_SNDTIMEO`, kept and reported: a send never waits.
    send_timeout: u64,
    /// `SO_SNDBUF` as set, reported back.
    send_buffer: usize,
    /// `SO_RCVBUF` as set, reported back.
    receive_buffer: usize,
}

/// An `AF_PACKET` socket.
pub(crate) struct PacketSocket {
    /// The network namespace it was made in, for life.
    ns: Arc<NetNamespace>,
    /// Which socket in the stack.
    id: SocketId,
    /// Whole frames or payloads.
    kind: PacketKind,
    /// The protocol `socket` named, as it was given: in network byte order.
    opened_protocol: u16,
    /// What `stat` reports through it.
    metadata: Metadata,
    /// What a program has set.
    options: SpinLock<Options>,
}

impl fmt::Debug for PacketSocket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PacketSocket")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl Drop for PacketSocket {
    fn drop(&mut self) {
        self.ns.core().with(|stack, _| stack.close(self.id));
    }
}

impl PacketSocket {
    /// Open a socket of `kind` for the protocol `socket` named, which arrives
    /// in network byte order, as the open file `socket` installs.
    ///
    /// # Errors
    ///
    /// Whatever [`OpenFile::new`] refuses, which for a socket is nothing.
    pub(crate) fn open(
        ns: &Arc<NetNamespace>,
        kind: PacketKind,
        protocol: u16,
        nonblock: bool,
        owner: (u32, u32),
    ) -> Result<Arc<OpenFile>, Errno> {
        let id = ns
            .core()
            .with(|stack, _| stack.open_packet(kind, u16::from_be(protocol)));
        let ino = fs::socket::next_ino();
        let socket = Arc::new(PacketSocket {
            ns: Arc::clone(ns),
            id,
            kind,
            opened_protocol: protocol,
            metadata: fs::socket::socket_metadata(ino, owner),
            options: SpinLock::new(Options {
                receive_timeout: 0,
                send_timeout: 0,
                send_buffer: SOCKET_BUFFER_DEFAULT,
                receive_buffer: SOCKET_BUFFER_DEFAULT,
            }),
        });
        fs::socket::open_on_sockfs(socket, ino, nonblock)
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

    /// `bind`: to the protocol and the interface a `sockaddr_ll` names, as
    /// `packet_bind` reads one -- the whole structure, of family `AF_PACKET`.
    /// A protocol of zero keeps the socket's, and an interface of zero is
    /// every interface.
    pub(crate) fn bind(&self, raw: &[u8]) -> Result<(), Errno> {
        if raw.len() < SOCKADDR_LL_SIZE || family_of(raw) != Some(AF_PACKET) {
            return Err(Errno::EINVAL);
        }
        let protocol = be16_at(raw, 2).ok_or(Errno::EINVAL)?;
        let interface = interface_at(raw).ok_or(Errno::ENODEV)?;
        self.ns
            .core()
            .with(|stack, _| stack.bind_packet(self.id, u16::from_be(protocol), interface))
            .map_err(|error| match error {
                ferrix_net::Error::NoDevice => Errno::ENODEV,
                other => errno(other),
            })
    }

    /// Send `data`, to the link address `to` names if it is given.
    ///
    /// `packet_snd`'s checks on the address: long enough for its own
    /// `sll_halen`, and on a `SOCK_DGRAM` socket long enough for an Ethernet
    /// address, and of family `AF_PACKET`.
    pub(crate) fn send(&self, data: &[u8], to: Option<&[u8]>) -> Result<usize, Errno> {
        let address = match to {
            None => None,
            Some(raw) => Some(self.link_address(raw)?),
        };
        self.ns
            .core()
            .with(|stack, _| stack.send_packet(self.id, data, address))
            .map_err(errno)
    }

    /// Read a `sockaddr_ll` a send was given.
    fn link_address(&self, raw: &[u8]) -> Result<LinkAddress, Errno> {
        let length = raw.get(11).copied().ok_or(Errno::EINVAL)?;
        if raw.len() < SOCKADDR_LL_ADDR_OFFSET + usize::from(length)
            || family_of(raw) != Some(AF_PACKET)
        {
            return Err(Errno::EINVAL);
        }
        if self.kind == PacketKind::Datagram
            && raw.len() < SOCKADDR_LL_ADDR_OFFSET + usize::from(ETHERNET_ADDRESS)
        {
            return Err(Errno::EINVAL);
        }
        let mut address = [0_u8; 6];
        if let Some(given) = raw.get(SOCKADDR_LL_ADDR_OFFSET..SOCKADDR_LL_ADDR_OFFSET + 6) {
            address.copy_from_slice(given);
        }
        Ok(LinkAddress {
            interface: interface_at(raw).ok_or(Errno::ENXIO)?,
            protocol: u16::from_be(be16_at(raw, 2).ok_or(Errno::EINVAL)?),
            address,
        })
    }

    /// Take the next frame, and the `sockaddr_ll` it came from.
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
            fs::socket::deadline_after(self.options.lock().receive_timeout)
        };
        loop {
            let taken = self
                .ns
                .core()
                .with(|stack, _| stack.recv_packet(self.id, out, peek));
            match taken {
                Ok(frame) => {
                    let mut name = vec![0_u8; SOCKADDR_LL_SIZE];
                    put_link_name(
                        &mut name,
                        frame.protocol,
                        frame.interface,
                        frame.kind.linux(),
                        Some(frame.source),
                    );
                    return Ok((
                        Received {
                            bytes: frame.bytes,
                            full: frame.bytes + frame.truncated,
                            hop_limit: None,
                        },
                        Some(name),
                    ));
                }
                Err(ferrix_net::Error::WouldBlock) if nonblock => return Err(Errno::EAGAIN),
                Err(ferrix_net::Error::WouldBlock) => {}
                Err(other) => return Err(errno(other)),
            }
            fs::socket::wait_on(
                self.ns.core().progress(),
                || self.readiness().readable,
                deadline,
            )?;
        }
    }

    /// `getsockname`, as `packet_getname` answers it: the protocol and the
    /// bound interface, with that interface's hardware address when there is
    /// one, and as long as the address it holds.
    pub(crate) fn local_name(&self) -> Vec<u8> {
        let (protocol, interface, hardware) = self.ns.core().with(|stack, _| {
            let Some(socket) = stack.packet_socket(self.id) else {
                return (0, 0, None);
            };
            let bound = socket.interface.unwrap_or(0);
            let hardware = socket
                .interface
                .and_then(|index| stack.interface(index))
                .filter(|link| matches!(link.medium, ferrix_net::Medium::Ethernet))
                .map(|link| link.hardware);
            (socket.protocol, bound, hardware)
        });
        let mut name = vec![0_u8; SOCKADDR_LL_SIZE];
        put_link_name(&mut name, protocol, interface, 0, hardware);
        let length = SOCKADDR_LL_ADDR_OFFSET + if hardware.is_some() { 6 } else { 0 };
        name.truncate(length);
        name
    }

    /// `getsockopt`.
    ///
    /// # Errors
    ///
    /// `ENOPROTOOPT` for an option a packet socket does not have, which is
    /// every `SOL_PACKET` one so far.
    pub(crate) fn get_option(&self, level: i32, name: i32, width: Width) -> Result<Vec<u8>, Errno> {
        let options = *self.options.lock();
        let number: i32 = match (level, name) {
            (SOL_SOCKET, SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW) => {
                return Ok(fs::socket::timeval(options.receive_timeout, width));
            }
            (SOL_SOCKET, SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW) => {
                return Ok(fs::socket::timeval(options.send_timeout, width));
            }
            (SOL_SOCKET, SO_TYPE) => match self.kind {
                PacketKind::Raw => SOCK_RAW.cast_signed(),
                PacketKind::Datagram => SOCK_DGRAM.cast_signed(),
            },
            (SOL_SOCKET, SO_DOMAIN) => i32::from(AF_PACKET),
            // `sk_protocol` holds what `socket` was given, byte order and all.
            (SOL_SOCKET, SO_PROTOCOL) => i32::from(self.opened_protocol),
            (SOL_SOCKET, SO_ERROR) => 0,
            (SOL_SOCKET, SO_SNDBUF) => i32::try_from(options.send_buffer).unwrap_or(i32::MAX),
            (SOL_SOCKET, SO_RCVBUF) => i32::try_from(options.receive_buffer).unwrap_or(i32::MAX),
            _ => return Err(Errno::ENOPROTOOPT),
        };
        Ok(number.to_le_bytes().to_vec())
    }

    /// `setsockopt`.
    ///
    /// # Errors
    ///
    /// `ENOPROTOOPT` at `SOL_PACKET`, which `udhcpc` asks for
    /// `PACKET_AUXDATA` and takes quietly, and at any level but `SOL_SOCKET`;
    /// `EINVAL` for a value of the wrong size.
    pub(crate) fn set_option(
        &self,
        level: i32,
        name: i32,
        value: &[u8],
        width: Width,
    ) -> Result<(), Errno> {
        if level != SOL_SOCKET {
            return Err(Errno::ENOPROTOOPT);
        }
        match name {
            SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW => {
                self.options.lock().receive_timeout = fs::socket::read_timeval(value, width)?;
            }
            SO_SNDTIMEO_OLD | SO_SNDTIMEO_NEW => {
                self.options.lock().send_timeout = fs::socket::read_timeval(value, width)?;
            }
            SO_SNDBUF | SO_RCVBUF => {
                let bytes = value.first_chunk::<4>().ok_or(Errno::EINVAL)?;
                let wanted = usize::try_from(i32::from_ne_bytes(*bytes).max(0))
                    .unwrap_or(0)
                    .saturating_mul(2)
                    .clamp(
                        ferrix_linux_abi::socket::SOCKET_BUFFER_MIN,
                        ferrix_linux_abi::socket::SOCKET_BUFFER_MAX,
                    );
                let mut options = self.options.lock();
                if name == SO_SNDBUF {
                    options.send_buffer = wanted;
                } else {
                    options.receive_buffer = wanted;
                }
            }
            // An option that changes nothing here is accepted, as the internet
            // families accept theirs.
            _ => {}
        }
        Ok(())
    }

    /// `SIOCINQ`: the length of the frame at the front, as `packet_ioctl`
    /// reports it. Everything else goes on to the interface requests.
    ///
    /// # Errors
    ///
    /// `ENOTTY` for another request.
    pub(crate) fn ioctl(
        &self,
        process: &crate::syscall::process::Process,
        request: u32,
        arg: u64,
    ) -> Result<usize, Errno> {
        if request != ferrix_linux_abi::socket::SIOCINQ {
            return Err(Errno::ENOTTY);
        }
        let waiting = self.ns.core().with(|stack, _| {
            stack
                .packet_socket(self.id)
                .and_then(|socket| socket.peek())
                .map_or(0, |frame| frame.bytes.len())
        });
        crate::syscall::uaccess::put_u32(
            process.space(),
            arg,
            u32::try_from(waiting).unwrap_or(u32::MAX),
        )?;
        Ok(0)
    }
}

impl Inode for PacketSocket {
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
        self.send(data, None)
    }
}

/// The socket an open file reads and writes through, if it is a packet one.
pub(crate) fn of(file: &OpenFile) -> Option<Arc<PacketSocket>> {
    Arc::clone(file.io())
        .into_any()
        .downcast::<PacketSocket>()
        .ok()
}

/// Which errno a packet socket's failure is, as `packet_snd` and
/// `packet_do_bind` answer them.
const fn errno(error: ferrix_net::Error) -> Errno {
    match error {
        ferrix_net::Error::NoDevice => Errno::ENXIO,
        ferrix_net::Error::NetworkDown => Errno::ENETDOWN,
        other => crate::net::socket::errno(other),
    }
}

/// A `sockaddr`'s family.
fn family_of(raw: &[u8]) -> Option<u16> {
    raw.first_chunk::<2>()
        .map(|bytes| u16::from_ne_bytes(*bytes))
}

/// Two bytes at `at`, as they lie: a network-order field read without turning.
fn be16_at(raw: &[u8], at: usize) -> Option<u16> {
    raw.get(at..at + 2)
        .and_then(|bytes| <[u8; 2]>::try_from(bytes).ok())
        .map(u16::from_ne_bytes)
}

/// `sll_ifindex`, which must not be negative.
fn interface_at(raw: &[u8]) -> Option<u32> {
    let bytes = raw
        .get(4..8)
        .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())?;
    u32::try_from(i32::from_ne_bytes(bytes)).ok()
}

/// Write a `sockaddr_ll` into `name`, which is [`SOCKADDR_LL_SIZE`] bytes.
fn put_link_name(
    name: &mut [u8],
    protocol: u16,
    interface: u32,
    kind: u8,
    hardware: Option<[u8; 6]>,
) {
    let (hatype, length, address) = match hardware {
        Some(address) => (ARPHRD_ETHER, ETHERNET_ADDRESS, address),
        None => (0, 0, [0; 6]),
    };
    let fields: [(usize, &[u8]); 7] = [
        (0, &AF_PACKET.to_ne_bytes()),
        (2, &protocol.to_be_bytes()),
        (4, &interface.to_ne_bytes()),
        (8, &hatype.to_ne_bytes()),
        (10, &[kind]),
        (11, &[length]),
        (SOCKADDR_LL_ADDR_OFFSET, &address),
    ];
    for (at, field) in fields {
        if let Some(slot) = name.get_mut(at..at + field.len()) {
            slot.copy_from_slice(field);
        }
    }
}
