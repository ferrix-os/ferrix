//! The net core: everything a host needs between a socket and an interface.
//!
//! One value holds the interfaces, the routes, the neighbour cache and every
//! socket. It has no clock, no lock and no device: frames and a time go in,
//! frames come out, and the kernel is what gives it a lock, a timer and a
//! driver. That is the same shape `src/lib/network/nettcp` has and for the same reason --
//! it is the shape `cargo test` can drive.
//!
//! # The loopback is inside
//!
//! A packet routed to the loopback interface is not handed out; it goes back
//! into the input path on the next turn of [`Stack::poll_transmit`]. A caller
//! that never asks for a frame therefore never delivers one to itself, which
//! is why the poll loop is the thing to drive rather than the receive path.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

use ferrix_kmem::{Charge, boxed_footprint};
use ferrix_nettcp::Connection;

use crate::addr::{Endpoint, IpAddress, Ipv4, Ipv6};
use crate::iface::{Address, Interface, Medium};
use crate::neighbor::Neighbors;
use crate::ports;
use crate::rand::Random;
use crate::reassembly::Reassembler;
use crate::route::{Origin, Route, Routes};
use crate::socket::{
    DatagramSocket, Error, Family, ListenSocket, RawSocket, Readiness, Shutdown, Socket, SocketId,
    StreamSocket,
};

/// Milliseconds on the stack's clock.
pub type Millis = u64;

/// A frame the stack wants put on an interface.
#[derive(Clone, Debug)]
pub struct Outgoing {
    /// Which interface it goes out of.
    pub interface: u32,
    /// The bytes: a complete Ethernet frame, or a bare IP packet on a
    /// loopback.
    pub frame: Vec<u8>,
}

/// What the stack is built with.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// How many bytes of payload a datagram socket holds by default.
    pub datagram_capacity: usize,
    /// What a TCP connection is built with.
    pub tcp: ferrix_nettcp::Config,
    /// The hop limit outgoing packets carry.
    pub hop_limit: u8,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            datagram_capacity: 212_992,
            tcp: ferrix_nettcp::Config::default(),
            hop_limit: 64,
        }
    }
}

/// What the whole stack has seen, for `/proc/net/snmp` and for a boot check.
#[derive(Clone, Copy, Debug, Default)]
pub struct Counters {
    /// Packets that arrived and were understood.
    pub delivered: u64,
    /// Packets dropped because nothing was listening.
    pub no_socket: u64,
    /// Packets dropped because they were malformed.
    pub malformed: u64,
    /// Packets dropped because they were not for this host.
    pub not_ours: u64,
    /// Fragments that arrived.
    pub fragments: u64,
    /// Datagrams put back together.
    pub reassembled: u64,
    /// Connections accepted.
    pub accepted: u64,
    /// Connections this host started.
    pub connected: u64,
    /// Resets sent to a segment with nowhere to go.
    pub resets_sent: u64,
    /// Unreachable messages sent for a datagram with nowhere to go.
    pub unreachable_sent: u64,
    /// Errors an unreachable message was carried back to a socket by.
    pub errors_reported: u64,
}

/// The net core.
#[derive(Debug)]
pub struct Stack {
    /// The interfaces, in the order they were added.
    pub(crate) interfaces: Vec<Interface>,
    /// The routing table.
    pub(crate) routes: Routes,
    /// The neighbour cache.
    pub(crate) neighbors: Neighbors,
    /// Every socket, by identifier.
    pub(crate) sockets: BTreeMap<u32, Socket>,
    /// The packet sockets, which share identifiers with [`Stack::sockets`]
    /// and are kept apart because they have no IP address to be found by.
    pub(crate) packets: BTreeMap<u32, crate::packet::PacketSocket>,
    /// The next identifier to hand out.
    next_id: u32,
    /// The unpredictability initial sequence numbers and ports come from.
    pub(crate) random: Random,
    /// Frames waiting to go out.
    pub(crate) egress: VecDeque<Outgoing>,
    /// Fragments waiting for their siblings.
    pub(crate) reassembly: Reassembler,
    /// What the stack was built with.
    pub(crate) config: Config,
    /// The identification field the next IPv4 datagram carries.
    pub(crate) ip_id: u16,
    /// What has been seen.
    pub(crate) counters: Counters,
}

impl Default for Stack {
    fn default() -> Stack {
        Stack::new(Config::default())
    }
}

impl Stack {
    /// A stack with a loopback interface, up, and its two routes.
    #[must_use]
    pub fn new(config: Config) -> Stack {
        let mut stack = Stack {
            interfaces: Vec::new(),
            routes: Routes::new(),
            neighbors: Neighbors::new(),
            sockets: BTreeMap::new(),
            packets: BTreeMap::new(),
            next_id: 1,
            random: Random::new(0),
            egress: VecDeque::new(),
            reassembly: Reassembler::new(),
            config,
            ip_id: 1,
            counters: Counters::default(),
        };
        let loopback = Interface::loopback(1);
        stack.interfaces.push(loopback);
        stack.add_local_routes(1);
        stack
    }

    /// The stack of a network namespace made after the first: its loopback is
    /// there and down, with no address and no route, and it reassembles at
    /// most [`crate::reassembly::NAMESPACE_BYTES`] (`docs/NETNS.md` 2.2).
    #[must_use]
    pub fn new_namespace(config: Config) -> Stack {
        let mut stack = Stack::new(config);
        stack.interfaces.clear();
        stack.routes = Routes::new();
        stack.interfaces.push(Interface::loopback_down(1));
        stack.reassembly = Reassembler::with_limit(crate::reassembly::NAMESPACE_BYTES);
        stack
    }

    /// Seed the generator that chooses initial sequence numbers and ephemeral
    /// ports. A stack that is never seeded is deterministic and says so in
    /// [`crate::rand`].
    pub fn seed(&mut self, seed: u64) {
        self.random.reseed(seed);
    }

    /// What has been seen.
    #[must_use]
    pub const fn counters(&self) -> Counters {
        self.counters
    }

    /// How many bytes of half-arrived datagrams are being held, which a fuzz
    /// target holds to its ceiling.
    #[must_use]
    pub const fn reassembly_held(&self) -> usize {
        self.reassembly.held()
    }

    // ---------------------------------------------------------------- links

    /// Add an interface and answer the index it was given.
    pub fn add_interface(&mut self, mut interface: Interface) -> u32 {
        let index = self
            .interfaces
            .iter()
            .map(|existing| existing.index)
            .max()
            .unwrap_or(0)
            + 1;
        interface.index = index;
        self.interfaces.push(interface);
        index
    }

    /// Every interface.
    #[must_use]
    pub fn interfaces(&self) -> &[Interface] {
        &self.interfaces
    }

    /// The interface with that index.
    #[must_use]
    pub fn interface(&self, index: u32) -> Option<&Interface> {
        self.interfaces
            .iter()
            .find(|interface| interface.index == index)
    }

    /// The interface with that index, to change.
    pub fn interface_mut(&mut self, index: u32) -> Option<&mut Interface> {
        self.interfaces
            .iter_mut()
            .find(|interface| interface.index == index)
    }

    /// Take an interface away, with its routes and everything the neighbour
    /// cache learned over it.
    ///
    /// Answers whether there was one. A socket bound to one of its addresses
    /// is left alone: it keeps its name and stops receiving, which is what a
    /// program sees when a cable is pulled.
    pub fn remove_interface(&mut self, index: u32) -> bool {
        self.detach_interface(index).is_some()
    }

    /// Take an interface out of the stack and hand it back, with its routes
    /// and what the neighbour cache learned over it gone: the first half of
    /// moving it to another stack ([`Stack::attach_interface`]). Its addresses
    /// stay on it.
    pub fn detach_interface(&mut self, index: u32) -> Option<Interface> {
        let at = self
            .interfaces
            .iter()
            .position(|interface| interface.index == index)?;
        let interface = self.interfaces.remove(at);
        self.routes.remove_interface(index);
        self.neighbors.remove_interface(index);
        self.egress.retain(|outgoing| outgoing.interface != index);
        Some(interface)
    }

    /// Put an interface taken from a stack into this one, under the next free
    /// index, and answer the index. Its name must be free here. Its routes
    /// are not restored: a route named an index of the other stack, and what
    /// an address implies is added.
    ///
    /// # Errors
    ///
    /// [`Error::AddressInUse`] when an interface of this stack has its name.
    pub fn attach_interface(&mut self, interface: Interface) -> Result<u32, Error> {
        if self.interface_by_name(interface.name.as_bytes()).is_some() {
            return Err(Error::AddressInUse);
        }
        let up = interface.is_up();
        let index = self.add_interface(interface);
        if up {
            self.add_local_routes(index);
        }
        Ok(index)
    }

    /// Give an interface another name.
    ///
    /// # Errors
    ///
    /// [`Error::NoDevice`] for an index that is not here,
    /// [`Error::AddressInUse`] for a name another interface has.
    pub fn rename_interface(&mut self, index: u32, name: &[u8]) -> Result<(), Error> {
        let name = crate::iface::Name::new(name);
        if self
            .interfaces
            .iter()
            .any(|each| each.index != index && each.name == name)
        {
            return Err(Error::AddressInUse);
        }
        self.interface_mut(index).ok_or(Error::NoDevice)?.name = name;
        Ok(())
    }

    /// How many addresses the interfaces hold between them.
    #[must_use]
    pub fn address_count(&self) -> usize {
        self.interfaces
            .iter()
            .map(|interface| interface.addresses.len())
            .sum()
    }

    /// Forget the fragments held: a namespace that cannot pay for them drops
    /// them, as a host under memory pressure does.
    pub fn flush_reassembly(&mut self) {
        self.reassembly.flush();
    }

    /// The interface with that name.
    #[must_use]
    pub fn interface_by_name(&self, name: &[u8]) -> Option<&Interface> {
        self.interfaces
            .iter()
            .find(|interface| interface.name.as_bytes() == name)
    }

    /// Give an interface an address, and add the on-link route that follows.
    pub fn add_address(&mut self, index: u32, address: Address) -> Result<(), Error> {
        let interface = self
            .interface_mut(index)
            .ok_or(Error::AddressNotAvailable)?;
        interface.add_address(address);
        self.add_local_routes(index);
        Ok(())
    }

    /// Take an address away, and the routes that came with it.
    pub fn remove_address(&mut self, index: u32, address: IpAddress) -> Result<(), Error> {
        let interface = self
            .interface_mut(index)
            .ok_or(Error::AddressNotAvailable)?;
        if !interface.remove_address(address) {
            return Err(Error::AddressNotAvailable);
        }
        self.routes.remove_interface(index);
        self.add_local_routes(index);
        Ok(())
    }

    /// Bring an interface up or down. Taking one down drops its routes and
    /// everything the neighbour cache learned over it.
    ///
    /// A loopback owns its addresses only while it is up, as Linux's does:
    /// bringing one up that has none gives it `127.0.0.1/8` and `::1`, and
    /// taking it down takes them away. A virtual Ethernet end is not running
    /// until its peer is up too, which the kernel, who can see both ends, says.
    pub fn set_up(&mut self, index: u32, up: bool) -> Result<(), Error> {
        let interface = self
            .interface_mut(index)
            .ok_or(Error::AddressNotAvailable)?;
        let loopback = interface.medium == Medium::Loopback;
        let running = if matches!(interface.backing, crate::iface::Backing::Veth { .. }) {
            0
        } else {
            crate::iface::IFF_RUNNING
        };
        if up {
            interface.flags |= crate::iface::IFF_UP | running;
            if loopback {
                interface.flags |= crate::iface::IFF_RUNNING | crate::iface::IFF_LOWER_UP;
                if interface.addresses.is_empty() {
                    interface
                        .addresses
                        .extend_from_slice(&Interface::loopback_addresses());
                }
            }
        } else {
            interface.flags &= !(crate::iface::IFF_UP | crate::iface::IFF_RUNNING);
            if loopback {
                interface.flags &= !crate::iface::IFF_LOWER_UP;
                interface.addresses.clear();
            }
        }
        if up {
            self.add_local_routes(index);
        } else {
            self.routes.remove_interface(index);
            self.neighbors.remove_interface(index);
        }
        Ok(())
    }

    /// The on-link route each of an interface's addresses implies.
    fn add_local_routes(&mut self, index: u32) {
        let Some(interface) = self.interface(index) else {
            return;
        };
        let prefixes: Vec<_> = interface
            .addresses
            .iter()
            .map(|address| address.cidr)
            .collect();
        for cidr in prefixes {
            self.routes.add(Route {
                destination: cidr,
                gateway: None,
                interface: index,
                metric: 0,
                origin: Origin::Kernel,
            });
        }
    }

    /// The routing table.
    #[must_use]
    pub const fn routes(&self) -> &Routes {
        &self.routes
    }

    /// The routing table, to change.
    pub const fn routes_mut(&mut self) -> &mut Routes {
        &mut self.routes
    }

    /// The neighbour cache.
    #[must_use]
    pub const fn neighbors(&self) -> &Neighbors {
        &self.neighbors
    }

    /// The neighbour cache, to change.
    pub const fn neighbors_mut(&mut self) -> &mut Neighbors {
        &mut self.neighbors
    }

    // -------------------------------------------------------------- sockets

    /// Open a UDP socket.
    pub fn open_udp(&mut self, family: Family) -> SocketId {
        let socket = DatagramSocket::new(family, self.config.datagram_capacity);
        self.install_socket(Socket::Udp(socket))
    }

    /// Open an ICMP echo socket, which answers `ping` without privilege.
    pub fn open_icmp(&mut self, family: Family) -> SocketId {
        let socket = DatagramSocket::new(family, self.config.datagram_capacity);
        self.install_socket(Socket::Icmp(socket))
    }

    /// Open a raw socket for `protocol`, which receives a copy of every packet
    /// of that protocol this host takes in and sends packets of it.
    ///
    /// Whether the caller may is not the stack's question: Linux keeps raw
    /// sockets behind `CAP_NET_RAW`, and the kernel asks that before this.
    pub fn open_raw(&mut self, family: Family, protocol: u8) -> SocketId {
        let socket = RawSocket::new(family, protocol, self.config.datagram_capacity);
        self.install_socket(Socket::Raw(socket))
    }

    /// Open a TCP socket, which is neither connected nor listening yet.
    ///
    /// It is held as a listener with a backlog of zero until `connect` or
    /// `listen` decides which it is, because that is the one state a TCP
    /// socket has before either.
    pub fn open_tcp(&mut self, family: Family) -> SocketId {
        let address = unspecified(family);
        self.install_socket(Socket::Listen(ListenSocket {
            family,
            local: Endpoint::new(address, 0),
            backlog: 0,
            ready: VecDeque::new(),
            pending: Vec::new(),
            options: crate::socket::Options::default(),
            owner: ferrix_kmem::current(),
        }))
    }

    /// Put a socket in the table and answer its identifier.
    pub(crate) fn install_socket(&mut self, socket: Socket) -> SocketId {
        let id = self.allocate_id();
        let _ = self.sockets.insert(id.0, socket);
        id
    }

    /// The next identifier no socket of either table holds.
    pub(crate) fn allocate_id(&mut self) -> SocketId {
        loop {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            if !self.sockets.contains_key(&id) && !self.packets.contains_key(&id) {
                return SocketId(id);
            }
        }
    }

    /// The socket, if it is there.
    #[must_use]
    pub fn socket(&self, id: SocketId) -> Option<&Socket> {
        self.sockets.get(&id.0)
    }

    /// The socket, to change.
    pub fn socket_mut(&mut self, id: SocketId) -> Option<&mut Socket> {
        self.sockets.get_mut(&id.0)
    }

    /// Every socket, for `/proc/net`.
    pub fn sockets(&self) -> impl Iterator<Item = (SocketId, &Socket)> {
        self.sockets
            .iter()
            .map(|(id, socket)| (SocketId(*id), socket))
    }

    /// Close a socket. A connected stream is reset unless it was closed
    /// cleanly first, which is what Linux does to an unread socket.
    pub fn close(&mut self, id: SocketId) {
        if self.packets.remove(&id.0).is_some() {
            return;
        }
        let Some(socket) = self.sockets.remove(&id.0) else {
            return;
        };
        match socket {
            Socket::Stream(mut stream) => {
                stream.closing = true;
                let unread = stream.connection.receive_queued() > 0;
                if unread {
                    stream.connection.abort();
                } else {
                    stream.connection.close();
                }
                // Keep it until its close has been acknowledged, so the peer
                // sees a close rather than a connection that stopped
                // answering. `reap` takes it away when it reaches CLOSED.
                let _ = self.sockets.insert(id.0, Socket::Stream(stream));
                self.detach_from_listeners(id);
            }
            Socket::Listen(listener) => {
                // Nobody will accept them now: reset each, and let `reap`
                // take it once it has closed, as it takes one a program let
                // go of. Left with its listener named and not closing, it
                // stayed in the table for good.
                for waiting in listener.ready.iter().chain(listener.pending.iter()) {
                    if let Some(Socket::Stream(stream)) = self.sockets.get_mut(&waiting.0) {
                        stream.connection.abort();
                        stream.closing = true;
                        stream.listener = None;
                    }
                }
            }
            Socket::Udp(_) | Socket::Icmp(_) | Socket::Raw(_) => {}
        }
    }

    /// Stop any listener from still counting `id` among its connections.
    fn detach_from_listeners(&mut self, id: SocketId) {
        for socket in self.sockets.values_mut() {
            if let Socket::Listen(listener) = socket {
                listener.ready.retain(|waiting| *waiting != id);
                listener.pending.retain(|waiting| *waiting != id);
            }
        }
    }

    /// What the socket can do right now.
    #[must_use]
    pub fn readiness(&self, id: SocketId) -> Readiness {
        if let Some(packet) = self.packets.get(&id.0) {
            return packet.readiness();
        }
        self.sockets
            .get(&id.0)
            .map_or(Readiness::default(), Socket::readiness)
    }

    /// What the socket is bound to.
    #[must_use]
    pub fn local_endpoint(&self, id: SocketId) -> Option<Endpoint> {
        self.sockets.get(&id.0).map(Socket::local)
    }

    /// What the socket is connected to.
    #[must_use]
    pub fn remote_endpoint(&self, id: SocketId) -> Option<Endpoint> {
        self.sockets.get(&id.0).and_then(Socket::remote)
    }

    /// Bind a socket to an address and a port.
    pub fn bind(&mut self, id: SocketId, requested: Endpoint) -> Result<(), Error> {
        let family = self.sockets.get(&id.0).ok_or(Error::NoSocket)?.family();
        self.check_bind_address(family, requested.address)?;
        // A raw socket is bound to an address and never to a port, and may be
        // bound again: `raw_bind` only records the address.
        if let Some(Socket::Raw(raw)) = self.sockets.get_mut(&id.0) {
            raw.datagram.local = Endpoint::new(requested.address, 0);
            return Ok(());
        }
        let port = if requested.port == 0 {
            self.choose_port(id, family)?
        } else {
            if self.port_taken(id, family, requested) {
                return Err(Error::AddressInUse);
            }
            requested.port
        };
        let endpoint = Endpoint::new(requested.address, port);
        match self.sockets.get_mut(&id.0).ok_or(Error::NoSocket)? {
            Socket::Udp(socket) | Socket::Icmp(socket) => {
                if socket.local.port != 0 {
                    return Err(Error::AlreadyDone);
                }
                socket.local = endpoint;
            }
            Socket::Listen(listener) => {
                if listener.local.port != 0 {
                    return Err(Error::AlreadyDone);
                }
                listener.local = endpoint;
            }
            Socket::Stream(_) | Socket::Raw(_) => return Err(Error::AlreadyDone),
        }
        Ok(())
    }

    /// Whether an address may be bound: it has to be one of this host's, or
    /// the unspecified one.
    fn check_bind_address(&self, family: Family, address: IpAddress) -> Result<(), Error> {
        if address.is_unspecified() {
            return Ok(());
        }
        if address.is_v4() != matches!(family, Family::V4) {
            // An `AF_INET6` socket binding an IPv4-mapped address is the one
            // case where the families may differ, and `Endpoint` has already
            // been given the mapped form.
            if !matches!(family, Family::V6) {
                return Err(Error::Invalid);
            }
        }
        let owned = self
            .interfaces
            .iter()
            .any(|interface| interface.owns(address));
        if owned {
            Ok(())
        } else {
            Err(Error::AddressNotAvailable)
        }
    }

    /// Whether another socket of the same protocol already holds that port.
    ///
    /// `SO_REUSEADDR` on both the socket binding and the one already there
    /// lifts the conflict, which is what lets a server bind a particular
    /// address on a port a wildcard socket also holds -- unless the one there
    /// is listening. Linux's `inet_csk_bind_conflict` keeps a listening
    /// socket's port to it whatever `SO_REUSEADDR` says: a second listener on
    /// an overlapping address could take connections meant for the first.
    /// (`SO_REUSEPORT`, which would share it on purpose, is not offered.)
    fn port_taken(&self, id: SocketId, family: Family, endpoint: Endpoint) -> bool {
        let this = self.sockets.get(&id.0);
        let stream = matches!(this, Some(Socket::Listen(_) | Socket::Stream(_)));
        let icmp = matches!(this, Some(Socket::Icmp(_)));
        let reusing = this.is_some_and(|socket| socket.options().reuse_address);
        self.sockets.iter().any(|(other, socket)| {
            if *other == id.0 {
                return false;
            }
            let same_protocol = match socket {
                Socket::Listen(_) | Socket::Stream(_) => stream,
                Socket::Icmp(_) => icmp,
                Socket::Udp(_) => !stream && !icmp,
                // A raw socket holds no port.
                Socket::Raw(_) => false,
            };
            if !same_protocol || socket.family() != family {
                return false;
            }
            let held = socket.local();
            if held.port != endpoint.port {
                return false;
            }
            let overlaps = held.address.is_unspecified()
                || endpoint.address.is_unspecified()
                || held.address == endpoint.address;
            if !overlaps {
                return false;
            }
            is_listening(socket) || !(reusing && socket.options().reuse_address)
        })
    }

    /// Whether another socket is listening on a port and address overlapping
    /// `endpoint`: what [`Stack::listen`] refuses, as Linux's
    /// `inet_csk_listen_start` does, for two sockets bound side by side under
    /// `SO_REUSEADDR` before either listened.
    fn listener_on(&self, id: SocketId, family: Family, endpoint: Endpoint) -> bool {
        self.sockets.iter().any(|(other, socket)| {
            let held = socket.local();
            *other != id.0
                && is_listening(socket)
                && socket.family() == family
                && held.port == endpoint.port
                && (held.address.is_unspecified()
                    || endpoint.address.is_unspecified()
                    || held.address == endpoint.address)
        })
    }

    /// A free ephemeral port for this socket.
    fn choose_port(&mut self, id: SocketId, family: Family) -> Result<u16, Error> {
        let mut random = self.random;
        let address = unspecified(family);
        let chosen = ports::choose(&mut random, |candidate| {
            self.port_taken(id, family, Endpoint::new(address, candidate))
        });
        self.random = random;
        chosen.ok_or(Error::AddressInUse)
    }

    /// Start listening. A backlog of zero is taken as one, as Linux does.
    pub fn listen(&mut self, id: SocketId, backlog: usize) -> Result<(), Error> {
        let family = self.sockets.get(&id.0).ok_or(Error::NoSocket)?.family();
        let needs_port = matches!(
            self.sockets.get(&id.0),
            Some(Socket::Listen(listener)) if listener.local.port == 0
        );
        if needs_port {
            let port = self.choose_port(id, family)?;
            if let Some(Socket::Listen(listener)) = self.sockets.get_mut(&id.0) {
                listener.local.port = port;
            }
        } else if let Some(socket @ Socket::Listen(_)) = self.sockets.get(&id.0)
            && !is_listening(socket)
            && self.listener_on(id, family, socket.local())
        {
            return Err(Error::AddressInUse);
        }
        match self.sockets.get_mut(&id.0).ok_or(Error::NoSocket)? {
            Socket::Listen(listener) => {
                listener.backlog = backlog.max(1);
                Ok(())
            }
            _ => Err(Error::AlreadyDone),
        }
    }

    /// Take the oldest connection that finished its handshake.
    pub fn accept(&mut self, id: SocketId) -> Result<SocketId, Error> {
        match self.sockets.get_mut(&id.0).ok_or(Error::NoSocket)? {
            Socket::Listen(listener) if listener.backlog > 0 => {
                listener.ready.pop_front().ok_or(Error::WouldBlock)
            }
            Socket::Listen(_) => Err(Error::Invalid),
            _ => Err(Error::WrongKind),
        }
    }

    /// Shut a socket down in one or both directions.
    pub fn shutdown(&mut self, id: SocketId, how: Shutdown) -> Result<(), Error> {
        match self.sockets.get_mut(&id.0).ok_or(Error::NoSocket)? {
            Socket::Udp(socket)
            | Socket::Icmp(socket)
            | Socket::Raw(RawSocket {
                datagram: socket, ..
            }) => {
                if matches!(how, Shutdown::Read | Shutdown::Both) {
                    socket.read_shut = true;
                }
                if matches!(how, Shutdown::Write | Shutdown::Both) {
                    socket.write_shut = true;
                }
                Ok(())
            }
            Socket::Stream(stream) => {
                if matches!(how, Shutdown::Read | Shutdown::Both) {
                    stream.read_shut = true;
                }
                if matches!(how, Shutdown::Write | Shutdown::Both) {
                    stream.connection.close();
                }
                Ok(())
            }
            Socket::Listen(_) => Err(Error::NotConnected),
        }
    }

    /// Take and clear the error a socket is holding, as `SO_ERROR` does.
    pub fn take_error(&mut self, id: SocketId) -> Option<Error> {
        match self.sockets.get_mut(&id.0)? {
            Socket::Udp(socket) | Socket::Icmp(socket) => socket.take_error(),
            Socket::Raw(raw) => raw.datagram.take_error(),
            Socket::Stream(stream) => stream.error.take().or_else(|| {
                stream.connection.failure().map(|failure| match failure {
                    ferrix_nettcp::Failure::Refused => Error::Refused,
                    ferrix_nettcp::Failure::TimedOut => Error::TimedOut,
                    ferrix_nettcp::Failure::Reset | ferrix_nettcp::Failure::Protocol => {
                        Error::Reset
                    }
                })
            }),
            Socket::Listen(_) => None,
        }
    }

    /// Turn a stream socket into a connection to `remote`.
    ///
    /// The handshake has not started when this answers: the `SYN` goes out on
    /// the next [`Stack::poll_transmit`], and the caller waits on
    /// [`Stack::readiness`].
    pub fn connect(&mut self, id: SocketId, remote: Endpoint, now: Millis) -> Result<(), Error> {
        match self.sockets.get(&id.0).ok_or(Error::NoSocket)? {
            Socket::Udp(_) | Socket::Icmp(_) => self.connect_datagram(id, remote),
            Socket::Raw(_) => self.connect_raw(id, remote),
            Socket::Listen(listener) if listener.backlog == 0 => {
                self.connect_stream(id, remote, now)
            }
            Socket::Listen(_) => Err(Error::AlreadyDone),
            Socket::Stream(_) => Err(Error::AlreadyDone),
        }
    }

    /// A datagram socket's connect only records where sends go.
    fn connect_datagram(&mut self, id: SocketId, remote: Endpoint) -> Result<(), Error> {
        let source = self.source_for(remote.address)?;
        let family = self.sockets.get(&id.0).ok_or(Error::NoSocket)?.family();
        let port = match self.sockets.get(&id.0) {
            Some(socket) if socket.local().port != 0 => socket.local().port,
            _ => self.choose_port(id, family)?,
        };
        let Some(Socket::Udp(socket) | Socket::Icmp(socket)) = self.sockets.get_mut(&id.0) else {
            return Err(Error::WrongKind);
        };
        if socket.local.address.is_unspecified() {
            socket.local.address = source;
        }
        socket.local.port = port;
        socket.remote = Some(remote);
        Ok(())
    }

    /// A raw socket's connect records the peer's address, and this host's
    /// address towards it if the socket was not bound; there are no ports.
    fn connect_raw(&mut self, id: SocketId, remote: Endpoint) -> Result<(), Error> {
        let source = self.source_for(remote.address)?;
        let Some(Socket::Raw(raw)) = self.sockets.get_mut(&id.0) else {
            return Err(Error::WrongKind);
        };
        if raw.datagram.local.address.is_unspecified() {
            raw.datagram.local.address = source;
        }
        raw.datagram.remote = Some(Endpoint::new(remote.address, 0));
        Ok(())
    }

    /// A stream socket's connect starts the handshake.
    fn connect_stream(&mut self, id: SocketId, remote: Endpoint, now: Millis) -> Result<(), Error> {
        let source = self.source_for(remote.address)?;
        let family = self.sockets.get(&id.0).ok_or(Error::NoSocket)?.family();
        let bound = self
            .sockets
            .get(&id.0)
            .map(Socket::local)
            .unwrap_or_else(|| Endpoint::new(unspecified(family), 0));
        let port = if bound.port == 0 {
            self.choose_port(id, family)?
        } else {
            bound.port
        };
        let local = Endpoint::new(
            if bound.address.is_unspecified() {
                source
            } else {
                bound.address
            },
            port,
        );
        let iss = ferrix_nettcp::SeqNumber(self.random.next_u32());
        let options = self
            .sockets
            .get(&id.0)
            .map(Socket::options)
            .unwrap_or_default();
        let mut connection = Connection::connect(self.config.tcp, local.port, remote.port, iss);
        let heap = Charge::bytes(boxed_footprint::<StreamSocket>()).map_err(|_| Error::NoMemory)?;
        connection
            .charge_to(heap.owner())
            .map_err(|_| Error::NoMemory)?;
        let _ = self.sockets.insert(
            id.0,
            Socket::Stream(alloc::boxed::Box::new(StreamSocket {
                family,
                local,
                remote,
                connection,
                options,
                error: None,
                read_shut: false,
                listener: None,
                closing: false,
                heap,
            })),
        );
        self.counters.connected += 1;
        let _ = now;
        Ok(())
    }

    /// An address of this host to send to `destination` from.
    pub(crate) fn source_for(&self, destination: IpAddress) -> Result<IpAddress, Error> {
        let hop = self.routes.lookup(destination).ok_or(Error::Unreachable)?;
        let interface = self.interface(hop.interface).ok_or(Error::Unreachable)?;
        if !interface.is_up() {
            return Err(Error::Unreachable);
        }
        interface
            .source_for(destination)
            .ok_or(Error::AddressNotAvailable)
    }

    /// Whether that interface loops packets back to this host.
    pub(crate) fn is_loopback(&self, index: u32) -> bool {
        self.interface(index)
            .is_some_and(|interface| matches!(interface.medium, Medium::Loopback))
    }
}

/// The unspecified address of a family.
pub(crate) fn unspecified(family: Family) -> IpAddress {
    match family {
        Family::V4 => IpAddress::V4(Ipv4::UNSPECIFIED),
        Family::V6 => IpAddress::V6(Ipv6::UNSPECIFIED),
    }
}

/// Whether `socket` is a TCP socket that has been told to listen.
fn is_listening(socket: &Socket) -> bool {
    matches!(socket, Socket::Listen(listener) if listener.backlog > 0)
}
