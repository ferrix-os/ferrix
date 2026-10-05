//! A user-mode network backend for the guest: a NAT gateway on a UDP socket.
//!
//! # Why this exists rather than `-netdev user`
//!
//! QEMU's own user-mode network is slirp, and slirp is an optional build-time
//! dependency. The QEMU on the machine this was written for was built without
//! it, so the backend simply is not there:
//!
//! ```text
//! qemu-system-x86_64: -netdev user,id=n0: network backend 'user' is not
//! compiled into this binary
//! ```
//!
//! The two alternatives both want privilege this build tool does not have and
//! should not ask for. `-netdev tap` needs `CAP_NET_ADMIN` or a setuid helper,
//! and the usual way round that — a user namespace with a `tap` inside it — is
//! refused outright on this host, where `AppArmor` blocks unprivileged user
//! namespaces. Requiring either would mean `cargo xtask run --net` works on one
//! developer's machine and asks for a password on the next.
//!
//! What is always available is a datagram socket:
//!
//! ```text
//! -netdev dgram,id=net0,local.type=inet,local.host=127.0.0.1,local.port=0,
//!                       remote.type=inet,remote.host=127.0.0.1,remote.port=P
//! -device virtio-net-pci,netdev=net0,mac=...
//! ```
//!
//! This module binds a UDP socket on the loopback, at a port `P` the host's
//! kernel chooses, and QEMU sends every Ethernet frame the guest transmits to it
//! as one datagram. QEMU's own end is bound to port 0, so it too gets a free
//! port, and the gateway learns it from the first frame that arrives: the guest
//! always speaks first, with a DHCP discover or an ARP request, so there is
//! never an answer with nowhere to go. From then on only that one address is
//! listened to. Nothing here needs a raw socket, a tun device or a capability:
//! outside the guest network it speaks through ordinary host `UdpSocket`s and
//! `TcpStream`s, the same ones any program gets.
//!
//! UDP on the loopback rather than a UNIX datagram socket, which this was first
//! written on, because it is the one datagram socket every host has: Windows'
//! `std` has no `UnixDatagram`, and Windows has no datagram flavour of `AF_UNIX`
//! for it to wrap. What the change costs is that a loopback datagram can be
//! dropped when a receive buffer is full, where a UNIX one blocks its sender;
//! `tcp` retransmits on a timer, and every other protocol here is one the guest
//! already retries.
//!
//! # The network it presents
//!
//! Deliberately the same numbers slirp uses, so that habits and every piece of
//! QEMU documentation carry over unchanged:
//!
//! | Address       | What it is                                  |
//! |---------------|---------------------------------------------|
//! | `10.0.2.0/24` | the guest network                           |
//! | `10.0.2.2`    | this gateway, at MAC `52:55:0a:00:02:02`    |
//! | `10.0.2.3`    | the DNS forwarder                           |
//! | `10.0.2.15`   | where the guest is expected, and what DHCP offers |
//!
//! # What it does, and what it deliberately does not
//!
//! * **ARP** — answers requests for `10.0.2.2` and `10.0.2.3`, and learns the
//!   guest's MAC from anything it sends.
//! * **DHCP** — a minimal server, enough for `udhcpc` to configure `eth0`.
//! * **ICMP echo** — answered here for `10.0.2.2` and `10.0.2.3`, and for any
//!   other address carried out by the host's own `ping`, as [`icmp`] explains:
//!   `ping 10.0.2.2` and `ping 1.1.1.1` both work.
//! * **UDP** — one host socket per guest flow, with an idle timeout. A datagram
//!   to `10.0.2.3:53` goes to the host's own resolver: the first in
//!   `/etc/resolv.conf`, or on Windows the first the network configuration
//!   names; see [`host_resolver`].
//! * **TCP** — terminated here and re-opened as an ordinary host `TcpStream`,
//!   with the payload relayed between the two; see [`tcp`].
//! * **Forwards** — a `--forward` port on the host's loopback, each connection
//!   to it opened to the guest from `10.0.2.2` and relayed the same way; see
//!   [`Forward`].
//! * **Fragments** — refused, in both directions. The MTU is 1500 and nothing
//!   here fragments; an over-long relayed datagram is dropped and counted.
//! * **IPv6** — not offered. The guest has no stack pointed at it yet, and a
//!   half-answered IPv6 is worse than none: a guest that gets a router
//!   advertisement will prefer the address in it.

mod dhcp;
mod icmp;
mod tcp;
mod udp;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use ferrix_netwire::ethernet::{self, Mac, ethertype};
use ferrix_netwire::{arp, icmpv4, ipv4};

use crate::{Error, Result};

/// The gateway's own address, which is the guest's default route.
pub(crate) const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);

/// The address the DNS forwarder answers on.
pub(crate) const DNS_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);

/// The address DHCP offers the guest, and the only one this gateway routes for.
pub(crate) const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);

/// The guest network's mask.
const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);

/// Its broadcast address.
const BROADCAST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 255);

/// Where an address the guest used actually goes on the host.
///
/// [`GATEWAY_IP`] is this gateway, and slirp's convention -- which every piece
/// of QEMU documentation assumes -- is that it is the host as well: a guest
/// that opens `10.0.2.2:8080` reaches whatever is listening on the host's
/// `127.0.0.1:8080`. That is also what lets a test be hermetic, because the
/// server the guest fetches from can be the test itself. Every other address
/// is left alone, because this is a gateway to the real network and not a set
/// of services pretending to be one.
pub(crate) fn host_of(seen: SocketAddrV4) -> SocketAddrV4 {
    if *seen.ip() == GATEWAY_IP {
        SocketAddrV4::new(Ipv4Addr::LOCALHOST, seen.port())
    } else {
        seen
    }
}

/// A port on the host that leads to a port on the guest: slirp's `hostfwd`,
/// for TCP.
///
/// The gateway listens on `127.0.0.1:host`, and for each connection it accepts
/// it opens one to `GUEST_IP:guest` from `GATEWAY_IP`, with the payload relayed
/// between the two exactly as it is for a connection the guest opened. The
/// loopback and not every interface, for the reason `--vnc` gives: what the
/// guest serves is for this machine unless somebody tunnels it further.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Forward {
    /// The port listened on, on the host's loopback.
    pub(crate) host: u16,
    /// The port connected to, on the guest.
    pub(crate) guest: u16,
}

impl Forward {
    /// `<host>:<guest>`, both ports other than zero.
    pub(crate) fn parse(raw: &str) -> Result<Forward> {
        raw.split_once(':')
            .and_then(|(host, guest)| host.parse().ok().zip(guest.parse().ok()))
            .filter(|&(host, guest): &(u16, u16)| host > 0 && guest > 0)
            .map(|(host, guest)| Forward { host, guest })
            .ok_or_else(|| {
                Error::new(format!(
                    "--forward wants <host port>:<guest port>, got `{raw}`"
                ))
            })
    }
}

/// The gateway's MAC address. Locally administered, and slirp's.
pub(crate) const GATEWAY_MAC: Mac = [0x52, 0x55, 0x0A, 0x00, 0x02, 0x02];

/// The link's MTU: an IP packet may be this long, header included.
pub(crate) const MTU: usize = 1500;

/// The longest frame the gateway sends or accepts.
const MAX_FRAME: usize = ethernet::HEADER_LEN + MTU;

/// The shortest frame Ethernet carries. Shorter ones are padded, as a real
/// adapter's transmit path pads them, so that a guest driver counting on it is
/// not the thing that breaks first.
const MIN_FRAME: usize = 60;

/// The time to live the gateway stamps on what it originates.
const TTL: u8 = 64;

/// How long the serving thread waits for a frame before turning to its timers.
///
/// Every host socket is polled once per turn, so this is also the worst-case
/// latency the loop adds on the way back from the host. The path is a
/// loopback socket to this machine's own kernel; five milliseconds of it is nothing
/// beside the guest's own emulated interrupt latency.
const TURN: Duration = Duration::from_millis(5);

/// The resolver used when the host names none that can be read.
const FALLBACK_RESOLVER: Ipv4Addr = Ipv4Addr::new(1, 1, 1, 1);

/// Where the host's resolvers are listed.
const RESOLV_CONF: &str = "/etc/resolv.conf";

/// What the gateway saw, so that a failing test says something more useful
/// than that the guest is quiet.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    /// Frames the guest sent.
    frames_in: AtomicU64,
    /// Frames sent to the guest.
    frames_out: AtomicU64,
    /// ARP requests answered.
    arp: AtomicU64,
    /// ICMP echoes answered.
    icmp: AtomicU64,
    /// ICMP echoes to the outside that the host's `ping` saw answered.
    icmp_forwarded: AtomicU64,
    /// DHCP offers and acknowledgments sent.
    dhcp: AtomicU64,
    /// UDP flows opened towards the host.
    udp_flows: AtomicU64,
    /// UDP datagrams relayed to the host.
    udp_out: AtomicU64,
    /// UDP datagrams relayed back to the guest.
    udp_in: AtomicU64,
    /// TCP connections opened towards the host.
    tcp_opened: AtomicU64,
    /// TCP connections the host refused, and which the guest saw reset.
    tcp_refused: AtomicU64,
    /// TCP connections accepted on a `--forward` port and opened to the guest.
    tcp_forwarded: AtomicU64,
    /// Packets dropped because relaying them would have exceeded the MTU.
    oversize: AtomicU64,
    /// Packets of a protocol the gateway does not speak.
    unsupported: AtomicU64,
    /// Packets that did not parse: a bad checksum, a truncated header.
    malformed: AtomicU64,
}

/// Add one to a counter, discarding the previous value.
fn bump(counter: &AtomicU64) {
    let _ = counter.fetch_add(1, Ordering::Relaxed);
}

impl Counters {
    /// One line naming everything that happened, for the boot log.
    pub(crate) fn report(&self) -> String {
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        format!(
            "frames {}/{} in/out, arp {}, icmp {} ({} forwarded), dhcp {}, udp {} flows {}/{} out/in, \
             tcp {} opened {} refused {} forwarded, dropped {} oversize {} unsupported {} malformed",
            get(&self.frames_in),
            get(&self.frames_out),
            get(&self.arp),
            get(&self.icmp),
            get(&self.icmp_forwarded),
            get(&self.dhcp),
            get(&self.udp_flows),
            get(&self.udp_out),
            get(&self.udp_in),
            get(&self.tcp_opened),
            get(&self.tcp_refused),
            get(&self.tcp_forwarded),
            get(&self.oversize),
            get(&self.unsupported),
            get(&self.malformed),
        )
    }

    /// How many frames the guest sent, which is the first question a failing
    /// network test asks.
    pub(crate) fn frames_in(&self) -> u64 {
        self.frames_in.load(Ordering::Relaxed)
    }
}

impl From<ferrix_netwire::Error> for Error {
    fn from(error: ferrix_netwire::Error) -> Self {
        Error::new(error.to_string())
    }
}

/// A running gateway: the thread, the socket it owns, and the counters.
///
/// Dropping it stops the thread and closes the socket. The QEMU process it
/// serves must therefore be waited for, or killed, before the handle goes.
#[derive(Debug)]
pub(crate) struct Gateway {
    /// Set to stop the serving thread at the end of its next turn.
    stop: Arc<AtomicBool>,
    /// The thread, taken and joined by [`Gateway::drop`].
    thread: Option<JoinHandle<()>>,
    /// Where the gateway's socket is bound, and QEMU sends to.
    address: SocketAddrV4,
    /// What the thread saw.
    counters: Arc<Counters>,
}

impl Gateway {
    /// Bind the socket and start serving.
    ///
    /// The port is the host kernel's choice, so two gateways in one process —
    /// `--arch all` boots three machines in turn — never collide, and neither
    /// does one left behind by a run that was killed.
    ///
    /// `resolver` is where `10.0.2.3:53` forwards to, or the host's own when
    /// it is `None`. A test that must answer a name the same way on every
    /// machine, with or without a network, passes its own.
    ///
    /// `forwards` are listened on before this returns, so a port something
    /// else holds is an error the run stops on, rather than a forward that
    /// silently leads nowhere.
    pub(crate) fn start(resolver: Option<SocketAddrV4>, forwards: &[Forward]) -> Result<Gateway> {
        Gateway::start_retransmitting(resolver, forwards, tcp::RETRANSMIT)
    }

    /// [`Gateway::start`], with TCP's retransmission timer at `retransmit`
    /// rather than [`tcp::RETRANSMIT`]. A test of what the duplicate
    /// acknowledgments alone send puts the timer out of its way with this:
    /// at 20 ms it runs out whenever the test's thread or this one goes
    /// unscheduled that long, and sends everything in flight again.
    fn start_retransmitting(
        resolver: Option<SocketAddrV4>,
        forwards: &[Forward],
        retransmit: Duration,
    ) -> Result<Gateway> {
        let core = Core::bind(resolver, forwards, retransmit)?;
        let address = match core.socket.local_addr()? {
            SocketAddr::V4(address) => address,
            SocketAddr::V6(_) => {
                return Err(Error::new(
                    "the gateway's socket, bound to 127.0.0.1, reports an IPv6 address",
                ));
            }
        };
        let counters = Arc::clone(&core.counters);
        let stop = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("ferrix-net-gateway".to_owned())
            .spawn(move || serve(core, &signal))
            .map_err(|error| Error::new(format!("could not start the gateway thread: {error}")))?;
        Ok(Gateway {
            stop,
            thread: Some(thread),
            address,
            counters,
        })
    }

    /// The address QEMU is told to send to, as its `remote.host` and
    /// `remote.port`.
    pub(crate) fn address(&self) -> SocketAddrV4 {
        self.address
    }

    /// What the gateway has seen so far.
    pub(crate) fn counters(&self) -> &Counters {
        &self.counters
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The serving thread: read a frame, answer it, then let every timer run.
///
/// One thread and one turn, rather than a thread per flow: every shared piece
/// of state — the flow tables, the sequence numbers, the guest's MAC — is then
/// owned by one thread and needs no lock, and the counters are the only thing
/// anyone else reads.
fn serve(mut core: Core, stop: &AtomicBool) {
    let mut frame = [0_u8; MAX_FRAME];
    while !stop.load(Ordering::Relaxed) {
        // An error is the turn's timeout, which is how the timers below get
        // to run; a datagram too long for the buffer, which is not a frame
        // this link could ever have carried; or, on Windows, the report that
        // an earlier send found no socket at QEMU's address, which is QEMU
        // having exited and is not this thread's to act on.
        if let Ok((len, from)) = core.socket.recv_from(&mut frame)
            && Some(from) != core.own_address
            && core.is_guest(from)
        {
            bump(&core.counters.frames_in);
            if let Some(bytes) = frame.get(..len) {
                // A frame whose bytes the guest chose. Nothing it can send is
                // allowed to end the thread, so a parse failure is a counter
                // and the next frame is read.
                if core.on_frame(bytes).is_err() {
                    bump(&core.counters.malformed);
                }
            }
        }
        core.poll_udp();
        core.poll_tcp();
        core.poll_icmp();
        core.expire();
    }
}

/// Everything the serving thread owns.
#[derive(Debug)]
struct Core {
    /// Bound to the loopback; QEMU's frames arrive here.
    socket: UdpSocket,
    /// QEMU's address, where answers go: learned from the first frame, and
    /// the only address listened to after it.
    guest_socket: Option<SocketAddr>,
    /// The guest's MAC, learned from the first thing it sends.
    guest_mac: Option<Mac>,
    /// Host sockets standing in for the guest's UDP flows.
    udp: BTreeMap<udp::Key, udp::Flow>,
    /// Host connections standing in for the guest's TCP connections.
    tcp: BTreeMap<tcp::Key, tcp::Connection>,
    /// Which of them is offered room first in the next turn
    /// (`Core::service_tcp`), counted up each turn.
    tcp_turn: usize,
    /// The `--forward` ports, each listening on the host's loopback.
    listeners: Vec<tcp::Listener>,
    /// The gateway-side port the next forwarded connection comes from.
    next_forward_port: u16,
    /// This socket's own address, from which only the wake-ups a helper thread
    /// sends arrive: they end the wait for a frame, and are not frames.
    own_address: Option<SocketAddr>,
    /// Where a host connection's outcome arrives from the thread that made it.
    connected: (
        std::sync::mpsc::Sender<tcp::Connected>,
        std::sync::mpsc::Receiver<tcp::Connected>,
    ),
    /// The resolver `10.0.2.3:53` forwards to, with its port: a test serves
    /// its own answers from a socket the kernel gave a free port, and asking
    /// it on 53 would reach nothing. Found on a thread of its own when nobody
    /// named one, because finding the host's costs a PowerShell start on
    /// Windows, seconds in which this loop would carry nothing at all.
    resolver: Resolver,
    /// Echo requests out to the host's `ping`.
    pings: icmp::Forwarder,
    /// The initial send sequence number the next connection takes.
    next_iss: u32,
    /// How long a TCP connection waits for an acknowledgment before sending
    /// everything unacknowledged again: [`tcp::RETRANSMIT`] but in tests.
    retransmit: Duration,
    /// What has happened.
    counters: Arc<Counters>,
}

impl Core {
    /// Bind the host socket and prepare the tables.
    fn bind(
        resolver: Option<SocketAddrV4>,
        forwards: &[Forward],
        retransmit: Duration,
    ) -> Result<Core> {
        let listeners = forwards
            .iter()
            .map(|&forward| tcp::Listener::bind(forward))
            .collect::<Result<Vec<_>>>()?;
        let resolver = match resolver {
            Some(named) => Resolver::Known(named),
            None => Resolver::find(),
        };
        let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| Error::new(format!("could not bind the gateway socket: {error}")))?;
        socket.set_read_timeout(Some(TURN))?;
        let own_address = socket.local_addr().ok();
        let waker = socket.try_clone().ok().zip(own_address);
        Ok(Core {
            socket,
            own_address,
            guest_socket: None,
            guest_mac: None,
            udp: BTreeMap::new(),
            tcp: BTreeMap::new(),
            tcp_turn: 0,
            listeners,
            next_forward_port: tcp::FORWARD_PORTS.start,
            connected: std::sync::mpsc::channel(),
            resolver,
            pings: icmp::Forwarder::new(waker),
            // Not random, and it does not need to be: this is a NAT on a
            // private wire with one guest on it, where an off-path attacker
            // guessing a sequence number is not a threat that exists. A fixed
            // start also makes one captured run comparable with the next.
            next_iss: 0x1000_0000,
            retransmit,
            counters: Arc::new(Counters::default()),
        })
    }

    /// Whether a datagram from `from` is the guest's.
    ///
    /// The first sender is taken to be QEMU, since nothing else knows the port
    /// before QEMU is started with it; anything from elsewhere afterwards is
    /// some other program on the loopback, and is not a frame at all.
    fn is_guest(&mut self, from: SocketAddr) -> bool {
        *self.guest_socket.get_or_insert(from) == from
    }

    /// Dispatch one frame from the guest.
    fn on_frame(&mut self, frame: &[u8]) -> Result<()> {
        let (header, payload) = ethernet::Header::parse(frame)?;
        if header.source != ethernet::BROADCAST {
            self.guest_mac = Some(header.source);
        }
        match header.ethertype {
            ethertype::ARP => self.on_arp(payload),
            ethertype::IPV4 => self.on_ipv4(payload),
            _ => {
                bump(&self.counters.unsupported);
                Ok(())
            }
        }
    }

    /// Answer an ARP request for an address this gateway owns.
    fn on_arp(&mut self, bytes: &[u8]) -> Result<()> {
        let packet = arp::Packet::parse(bytes)?;
        if packet.operation != arp::Operation::Request {
            return Ok(());
        }
        let target = Ipv4Addr::from(packet.target_ip);
        if target != GATEWAY_IP && target != DNS_IP {
            return Ok(());
        }
        let mut reply = [0_u8; arp::PACKET_LEN];
        let len = packet.reply(GATEWAY_MAC).emit(&mut reply)?;
        bump(&self.counters.arp);
        self.send_frame(
            packet.sender_mac,
            ethertype::ARP,
            reply.get(..len).ok_or(ferrix_netwire::Error::NoSpace)?,
        )
    }

    /// Dispatch one IPv4 packet on its protocol.
    fn on_ipv4(&mut self, bytes: &[u8]) -> Result<()> {
        let packet = ipv4::Header::parse(bytes)?;
        if packet.header.is_fragment() {
            // Reassembly would be a second implementation of what `src/lib/network/net`
            // already has, for a path where nothing this gateway originates is
            // ever fragmented and the MTU is the same on both sides.
            bump(&self.counters.unsupported);
            return Ok(());
        }
        let source = Ipv4Addr::from(packet.header.source);
        let destination = Ipv4Addr::from(packet.header.destination);
        match packet.header.protocol {
            ipv4::protocol::ICMP => self.on_icmp(source, destination, packet.payload),
            ipv4::protocol::UDP => self.on_udp(source, destination, packet.payload),
            ipv4::protocol::TCP => self.on_tcp(source, destination, packet.payload),
            _ => {
                bump(&self.counters.unsupported);
                Ok(())
            }
        }
    }

    /// Answer an echo request: here for the gateway's and the forwarder's own
    /// addresses, and through the host's `ping` for any other one outside the
    /// guest network, which [`Core::poll_icmp`] answers when the host has.
    fn on_icmp(&mut self, source: Ipv4Addr, destination: Ipv4Addr, bytes: &[u8]) -> Result<()> {
        let message = icmpv4::Header::parse(bytes)?;
        let Some((identifier, sequence)) = message.header.echo_fields() else {
            bump(&self.counters.unsupported);
            return Ok(());
        };
        if message.header.kind != icmpv4::kind::ECHO_REQUEST {
            return Ok(());
        }
        if destination == GATEWAY_IP || destination == DNS_IP {
            return self.echo_reply(destination, source, identifier, sequence, message.body);
        }
        // Nothing else on the guest network answers, and a broadcast or a
        // group is not a host the host's `ping` can ask.
        let outside = !on_guest_network(destination)
            && !destination.is_broadcast()
            && !destination.is_multicast()
            && !destination.is_unspecified();
        let taken = outside
            && self.pings.forward(icmp::Answered {
                guest: source,
                destination,
                identifier,
                sequence,
                body: message.body.to_vec(),
            });
        if !taken {
            bump(&self.counters.unsupported);
        }
        Ok(())
    }

    /// Reply to the echoes the host's `ping` saw answered, from the address
    /// the guest asked.
    fn poll_icmp(&mut self) {
        for answered in self.pings.answered() {
            bump(&self.counters.icmp_forwarded);
            // The reply names where the guest sent the request, which for
            // everything outside is the address the host pinged.
            let from = answered.destination;
            if self
                .echo_reply(
                    from,
                    answered.guest,
                    answered.identifier,
                    answered.sequence,
                    &answered.body,
                )
                .is_err()
            {
                bump(&self.counters.malformed);
            }
        }
    }

    /// Send an echo reply carrying `body` from `from` to `to`.
    fn echo_reply(
        &self,
        from: Ipv4Addr,
        to: Ipv4Addr,
        identifier: u16,
        sequence: u16,
        body: &[u8],
    ) -> Result<()> {
        let mut reply = [0_u8; MTU];
        let header = icmpv4::Header::echo(icmpv4::kind::ECHO_REPLY, identifier, sequence);
        let len = header.emit(body, &mut reply)?;
        bump(&self.counters.icmp);
        self.send_ipv4(
            ipv4::protocol::ICMP,
            from,
            to,
            reply.get(..len).ok_or(ferrix_netwire::Error::NoSpace)?,
        )
    }

    /// Send an IPv4 packet to the guest, from `source` to `destination`.
    ///
    /// The identification field is zero and the don't-fragment bit is set, which
    /// RFC 6864 allows precisely because nothing that must not be fragmented
    /// needs an identity to be reassembled by.
    fn send_ipv4(
        &self,
        protocol: u8,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        payload: &[u8],
    ) -> Result<()> {
        let Some(mac) = self.guest_mac else {
            // Nothing has been heard from the guest, so there is no address to
            // send to. Only reachable if a host socket answers before the guest
            // has said anything, which it cannot: the flow began with a frame.
            return Ok(());
        };
        self.send_ipv4_to(mac, protocol, source, destination, payload)
    }

    /// The same, to a named MAC: DHCP answers a guest whose address is still
    /// the one in the request's hardware field.
    fn send_ipv4_to(
        &self,
        mac: Mac,
        protocol: u8,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        payload: &[u8],
    ) -> Result<()> {
        if ipv4::MIN_HEADER_LEN + payload.len() > MTU {
            bump(&self.counters.oversize);
            return Ok(());
        }
        let header = ipv4::Header {
            dscp: 0,
            ecn: 0,
            identification: 0,
            dont_fragment: true,
            more_fragments: false,
            fragment_offset: 0,
            ttl: TTL,
            protocol,
            source: source.octets(),
            destination: destination.octets(),
        };
        let mut packet = [0_u8; MTU];
        let header_len = header.emit(&[], payload.len(), &mut packet)?;
        put(&mut packet, header_len, payload)?;
        let len = header_len + payload.len();
        self.send_frame(
            mac,
            ethertype::IPV4,
            packet.get(..len).ok_or(ferrix_netwire::Error::NoSpace)?,
        )
    }

    /// Put one frame on the wire to QEMU.
    fn send_frame(&self, destination: Mac, ethertype: u16, payload: &[u8]) -> Result<()> {
        let header = ethernet::Header {
            destination,
            source: GATEWAY_MAC,
            vlan: None,
            ethertype,
        };
        let mut frame = [0_u8; MAX_FRAME];
        let at = header.emit(&mut frame)?;
        put(&mut frame, at, payload)?;
        let len = (at + payload.len()).max(MIN_FRAME);
        let bytes = frame.get(..len).ok_or(ferrix_netwire::Error::NoSpace)?;
        // A send that fails is a guest that has gone: QEMU has exited. That is
        // not this thread's to report, and should not stop it serving whatever
        // is still open. With no guest address yet there is nobody to send to,
        // which only a host socket answering before the guest spoke can cause.
        if let Some(guest) = self.guest_socket
            && self.socket.send_to(bytes, guest).is_ok()
        {
            bump(&self.counters.frames_out);
        }
        Ok(())
    }

    /// Drop what has gone idle, and what has finished.
    fn expire(&mut self) {
        self.expire_udp();
        self.expire_tcp();
    }
}

/// Copy `field` into `out` at `at`, refusing rather than indexing past the end.
fn put(out: &mut [u8], at: usize, field: &[u8]) -> Result<()> {
    let end = at
        .checked_add(field.len())
        .ok_or(ferrix_netwire::Error::NoSpace)?;
    out.get_mut(at..end)
        .ok_or(ferrix_netwire::Error::NoSpace)?
        .copy_from_slice(field);
    Ok(())
}

/// Whether `address` is on the guest network, `10.0.2.0/24`, where nothing but
/// the gateway's own addresses answers an echo.
fn on_guest_network(address: Ipv4Addr) -> bool {
    address.octets()[..3] == GATEWAY_IP.octets()[..3]
}

/// The resolver `10.0.2.3:53` forwards to: named, or being found.
#[derive(Debug)]
enum Resolver {
    /// Known.
    Known(SocketAddrV4),
    /// The host's own, which a thread started with the gateway is looking up.
    Finding(std::sync::mpsc::Receiver<SocketAddrV4>),
}

impl Resolver {
    /// Start looking up the host's own on a thread, so the serving loop never
    /// waits for PowerShell. It is started with the gateway, before QEMU is,
    /// so it is found long before a guest has booted far enough to ask.
    fn find() -> Resolver {
        let (sender, receiver) = std::sync::mpsc::channel();
        let started = std::thread::Builder::new()
            .name("ferrix-net-resolver".to_owned())
            .spawn(move || {
                let _ = sender.send(default_resolver());
            });
        match started {
            Ok(_) => Resolver::Finding(receiver),
            Err(_) => Resolver::Known(SocketAddrV4::new(FALLBACK_RESOLVER, udp::DNS_PORT)),
        }
    }

    /// The resolver if it is known by now. A query that arrives before it is
    /// is dropped, and the guest's resolver asks again.
    fn now(&mut self) -> Option<SocketAddrV4> {
        if let Resolver::Finding(receiver) = self {
            match receiver.try_recv() {
                Ok(found) => *self = Resolver::Known(found),
                Err(std::sync::mpsc::TryRecvError::Empty) => return None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    *self = Resolver::Known(SocketAddrV4::new(FALLBACK_RESOLVER, udp::DNS_PORT));
                }
            }
        }
        match self {
            Resolver::Known(address) => Some(*address),
            Resolver::Finding(_) => None,
        }
    }
}

/// Where `10.0.2.3:53` forwards to when nobody said: the host's own resolver,
/// on port 53.
fn default_resolver() -> SocketAddrV4 {
    SocketAddrV4::new(host_resolver(), udp::DNS_PORT)
}

/// The host's first IPv4 resolver, or [`FALLBACK_RESOLVER`].
///
/// Read once, at start: from `/etc/resolv.conf`, or on Windows, which has no
/// such file, from the network configuration. A resolver that cannot be found,
/// or only IPv6 ones, is not an error: the fallback is a public resolver, and a
/// gateway that refused to start because of the host's DNS settings would be a
/// build tool that refuses to boot a kernel because the host's DNS is unusual.
fn host_resolver() -> Ipv4Addr {
    let text = if cfg!(windows) {
        windows_resolvers()
    } else {
        std::fs::read_to_string(RESOLV_CONF).ok()
    };
    text.as_deref()
        .and_then(first_nameserver)
        .unwrap_or(FALLBACK_RESOLVER)
}

/// The first IPv4 `nameserver` line in resolv.conf's syntax.
fn first_nameserver(text: &str) -> Option<Ipv4Addr> {
    text.lines()
        .filter_map(|line| line.split_whitespace().collect::<Vec<_>>().try_into().ok())
        .find_map(|[keyword, address]: [&str; 2]| match keyword {
            "nameserver" => address.parse::<Ipv4Addr>().ok(),
            _ => None,
        })
}

/// Windows' resolvers, written as resolv.conf lines.
///
/// Through PowerShell, for `stty`'s reason in `serial.rs`: the alternative is
/// a binding to the IP Helper API, and this runs once per boot. Only interfaces
/// with a default gateway count, in the order Windows ranks them, so that a
/// virtual adapter nothing routes through — Hyper-V's, WSL's, a VPN that is
/// down — does not come first. Printed one address per line with a keyword in
/// front, so the output is the same whatever the display language.
fn windows_resolvers() -> Option<String> {
    let script = "Get-NetIPConfiguration | Where-Object IPv4DefaultGateway | \
                  Sort-Object { $_.NetIPv4Interface.InterfaceMetric } | \
                  ForEach-Object { $_.DNSServer } | Where-Object AddressFamily -eq 2 | \
                  ForEach-Object { $_.ServerAddresses } | \
                  ForEach-Object { \"nameserver $_\" }";
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}
