//! The net ring's self-check: the whole kernel side of a network interface,
//! with no network device anywhere.
//!
//! The check plays the driver. It makes the two VMOs and the port a driver
//! makes, writes the ring header, sends HELLO over the control channel it was
//! given, and then answers submissions by hand. That proves every part of the
//! path a real driver will use -- the control handshake, the rights at
//! handoff, the ring's arithmetic, the interface appearing in the net core,
//! frames going out of it and frames coming in -- on a machine with no network
//! adapter at all.
//!
//! What is required:
//!
//! * a HELLO whose handles carry the wrong rights is refused, and one as
//!   specified is answered with READY carrying the completion port;
//! * the interface appears in the net core with the name, hardware address and
//!   MTU the HELLO gave it;
//! * the kernel posts every free slot for the driver to fill, because a
//!   receive queue with no buffers drops every packet silently;
//! * an ARP request for the interface's address, written into a slot and
//!   completed, is answered with an ARP reply in a slot the kernel submits --
//!   which is a frame in and a frame out, through the whole stack;
//! * a packet socket bound to the interface for ARP reads that request, with
//!   the `sockaddr_ll` of its sender, and a frame it sends is the next one the
//!   kernel submits, addressed as the socket said;
//! * closing the driver's end parks the interface: its carrier goes, its index
//!   and address stay;
//! * a ring made for the same device again, with the same HELLO, takes the
//!   parked interface up again under its old index, address and all -- what a
//!   driver `devmgr` starts again relies on.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::convert::Infallible;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES};
use ferrix_net::addr::{IpAddress, IpCidr, Ipv4};
use ferrix_net::iface::Address;
use ferrix_netring::control::{Hello, Interface, InterfaceFlags, MAX_MESSAGE};
use ferrix_netring::driver::DriverSide;
use ferrix_netring::layout::{Op, Status, Submission, VERSION};
use ferrix_netwire::arp;
use ferrix_netwire::ethernet::{self, ethertype};

use super::Pages;
use crate::device::{self, DeviceNode};
use crate::net;
use crate::object::channel::{Endpoint, ReadError};
use crate::object::port::Port;
use crate::object::{Object, Transfer};
use crate::sched;
use crate::timer;
use crate::user::vmo::Vmo;

/// How many entries the check's ring has.
const ENTRIES: u32 = 8;

/// How many bytes a slot holds.
const SLOT: u32 = 2048;

/// The interface's address, which the check configures as `ip` would.
const OURS: Ipv4 = Ipv4::new([192, 0, 2, 1]);

/// The address the pretend peer asks from.
const THEIRS: Ipv4 = Ipv4::new([192, 0, 2, 2]);

/// The peer's hardware address.
const THEIR_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0xAA, 0xBB, 0xCC];

/// The interface's hardware address.
const OUR_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x11, 0x22, 0x33];

/// How long the check waits for the ring's task to answer.
const PATIENCE_NANOS: u64 = 5_000_000_000;

/// What the check saw.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Why it did not run, if it did not.
    pub(crate) skipped: Option<&'static str>,
    /// How many HELLOs were refused exactly as specified.
    pub(crate) refusals: usize,
    /// How many slots the kernel posted for the driver to fill.
    pub(crate) posted: u32,
    /// How many frames the check handed up the stack.
    pub(crate) received: u32,
    /// How many frames the kernel asked to have sent.
    pub(crate) sent: u32,
    /// How many frames went through a packet socket, in and out.
    pub(crate) packet_frames: u32,
}

/// Run the check.
///
/// # Errors
///
/// A string naming what did not hold.
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    let Some(node) = device::devices().first().map(Arc::clone) else {
        report.skipped = Some("this machine has no device to hang a ring on");
        return Ok(report);
    };
    refuse_a_bad_hello(&node, &mut report)?;
    serve_a_ring(&node, &mut report)?;
    Ok(report)
}

/// A HELLO whose handles carry the wrong rights is refused.
fn refuse_a_bad_hello(node: &Arc<DeviceNode>, report: &mut Report) -> Result<(), &'static str> {
    let (id, control) = super::create(node).map_err(|_| "a ring could not be made for a device")?;
    let driver = Driver::new()?;
    // The VMO handles carry `DUPLICATE`, which the protocol refuses: a
    // duplicable VMO is one the kernel's own handle could be copied from.
    let too_many = Rights(
        Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0 | Rights::TRANSFER.0 | Rights::DUPLICATE.0,
    );
    driver.send_hello(&control, too_many)?;
    match driver.read(&control)? {
        (ferrix_netring::Message::Refused(ferrix_netring::Refusal::Handles), _) => {
            report.refusals += 1;
        }
        _ => return Err("a HELLO with duplicable VMOs was not refused for its handles"),
    }
    drop(control);
    wait_for_task(id)
}

/// A HELLO as specified is taken up, and the interface works.
fn serve_a_ring(node: &Arc<DeviceNode>, report: &mut Report) -> Result<(), &'static str> {
    let (id, control) = super::create(node).map_err(|_| "a second ring could not be made")?;
    let mut driver = Driver::new()?;
    driver.send_hello(&control, vmo_rights())?;
    match driver.read(&control)? {
        (ferrix_netring::Message::Ready, Some(port)) => driver.completion_port = Some(port),
        (ferrix_netring::Message::Ready, None) => {
            return Err("READY came without the completion port it must carry");
        }
        _ => return Err("a HELLO as specified was not answered with READY"),
    }

    let index = wait_for_interface()?;
    check_the_interface(index)?;
    give_it_an_address(index)?;
    report.posted = driver.take_posted()?;
    if report.posted == 0 {
        return Err("the kernel posted no slot for the driver to fill");
    }
    let listener = packet_socket(index)?;
    report.received = driver.deliver_arp_request()?;
    report.sent = driver.expect_transmission(check_arp_reply)?;
    report.packet_frames += read_the_request(&listener, index)?;
    report.packet_frames += send_a_frame(&listener, index, &mut driver)?;

    drop(control);
    wait_for_task(id)?;
    check_parked(index)?;
    take_it_up_again(node, index)?;

    // Nobody serves this device's interface after the check.
    super::forget_device(node);
    if net::core().look(|stack| stack.interface(index).is_some()) {
        return Err("a parked interface outlived its device's forgetting");
    }
    Ok(())
}

/// The interface of a ring whose driver went is still there under its index,
/// with the address it was given, and without carrier.
fn check_parked(index: u32) -> Result<(), &'static str> {
    net::core().look(|stack| {
        let Some(interface) = stack.interface(index) else {
            return Err("the interface did not outlive the driver that brought it");
        };
        if interface.flags & ferrix_net::iface::IFF_RUNNING != 0 {
            return Err("a parked interface kept its carrier");
        }
        if !interface.owns(IpAddress::V4(OURS)) {
            return Err("a parked interface lost its address");
        }
        Ok(())
    })
}

/// A second driver for the same device, saying the same HELLO, is given the
/// parked interface back rather than a new one, carrier and all.
fn take_it_up_again(node: &Arc<DeviceNode>, index: u32) -> Result<(), &'static str> {
    let (id, control) = super::create(node)
        .map_err(|_| "a ring could not be made again for a device whose driver went")?;
    let driver = Driver::new()?;
    driver.send_hello(&control, vmo_rights())?;
    match driver.read(&control)? {
        (ferrix_netring::Message::Ready, Some(_)) => {}
        _ => return Err("a HELLO for a parked interface was not answered with READY"),
    }
    if wait_for_interface()? != index {
        return Err("a driver started again was given a new interface, not its old one");
    }
    net::core().look(|stack| {
        let Some(interface) = stack.interface(index) else {
            return Err("the interface taken up again went away");
        };
        if interface.flags & ferrix_net::iface::IFF_RUNNING == 0 {
            return Err("an interface taken up again did not get its carrier back");
        }
        if !interface.owns(IpAddress::V4(OURS)) {
            return Err("an interface taken up again lost its address");
        }
        Ok(())
    })?;
    drop(control);
    wait_for_task(id)
}

/// The Ethernet protocol the check's own frame names, one of the two IEEE
/// set aside for local experiments.
const EXPERIMENTAL_ETHERTYPE: u16 = 0x88B5;

/// What the check's packet socket sends.
const PACKET_BODY: &[u8] = b"a frame a packet socket wrote";

/// A `SOCK_DGRAM` packet socket for ARP, bound to the ring's interface, as
/// `udhcpc` binds one for IPv4.
fn packet_socket(index: u32) -> Result<Arc<net::packet::PacketSocket>, &'static str> {
    let file = net::packet::PacketSocket::open(
        net::first(),
        ferrix_net::packet::PacketKind::Datagram,
        ethertype::ARP.to_be(),
        false,
        (0, 0),
    )
    .map_err(|_| "a packet socket could not be opened")?;
    let socket = net::packet::of(&file).ok_or("a packet socket's open file does not hold one")?;
    socket
        .bind(&link_name(index, ethertype::ARP, [0; 6]))
        .map_err(|_| "a packet socket could not be bound to the ring's interface")?;
    Ok(socket)
}

/// A `sockaddr_ll` naming an interface, a protocol and a hardware address.
fn link_name(index: u32, protocol: u16, address: [u8; 6]) -> Vec<u8> {
    let mut name = vec![0_u8; ferrix_linux_abi::socket::SOCKADDR_LL_SIZE];
    let fields: [(usize, &[u8]); 5] = [
        (0, &ferrix_linux_abi::socket::AF_PACKET.to_ne_bytes()),
        (2, &protocol.to_be_bytes()),
        (4, &index.to_ne_bytes()),
        (11, &[6]),
        (12, &address),
    ];
    for (at, field) in fields {
        if let Some(slot) = name.get_mut(at..at + field.len()) {
            slot.copy_from_slice(field);
        }
    }
    name
}

/// The packet socket read the ARP request the driver delivered: its payload
/// without the link header, from the peer's hardware address, to the
/// broadcast, on this interface.
fn read_the_request(
    socket: &Arc<net::packet::PacketSocket>,
    index: u32,
) -> Result<u32, &'static str> {
    let mut out = [0_u8; 128];
    let (received, name) = socket
        .recv(&mut out, 0, true)
        .map_err(|_| "a packet socket bound for ARP did not read the ARP request")?;
    let Some(name) = name else {
        return Err("a packet socket's receive named no sender");
    };
    let request = arp_request();
    if out.get(..received.bytes) != request.get(ethernet::HEADER_LEN..) {
        return Err("a SOCK_DGRAM packet socket read something other than the frame's payload");
    }
    let expected = {
        let mut name = link_name(index, ethertype::ARP, THEIR_MAC);
        let hatype = 1_u16.to_ne_bytes();
        name.get_mut(8..10)
            .ok_or("a sockaddr_ll is too short")?
            .copy_from_slice(&hatype);
        // PACKET_BROADCAST.
        *name.get_mut(10).ok_or("a sockaddr_ll is too short")? = 1;
        name
    };
    if name != expected {
        return Err("a packet socket named the ARP request's sender wrongly");
    }
    Ok(1)
}

/// A frame the packet socket sends to the peer is the next transmission, with
/// the link header the stack put on it.
fn send_a_frame(
    socket: &Arc<net::packet::PacketSocket>,
    index: u32,
    driver: &mut Driver,
) -> Result<u32, &'static str> {
    let sent = socket
        .send(
            PACKET_BODY,
            Some(&link_name(index, EXPERIMENTAL_ETHERTYPE, THEIR_MAC)),
        )
        .map_err(|_| "a packet socket could not send a frame on the ring's interface")?;
    if sent != PACKET_BODY.len() {
        return Err("a packet socket sent its frame short");
    }
    driver.expect_transmission(|frame| {
        let Ok((header, payload)) = ethernet::Header::parse(frame) else {
            return Err("a packet socket's frame is not an Ethernet frame");
        };
        if header.ethertype != EXPERIMENTAL_ETHERTYPE
            || header.source != OUR_MAC
            || header.destination != THEIR_MAC
        {
            return Err("a packet socket's frame has another link header than it asked for");
        }
        if payload.get(..PACKET_BODY.len()) != Some(PACKET_BODY) {
            return Err("a packet socket's frame carries something else");
        }
        Ok(())
    })
}

/// The rights a VMO handle carries at handoff.
fn vmo_rights() -> Rights {
    Rights(Rights::READ.0 | Rights::WRITE.0 | Rights::MAP.0 | Rights::TRANSFER.0)
}

/// Wait until ring `id`'s task has stopped. Only that ring's: on a machine
/// with a network adapter, the driver's ring runs for the life of the
/// machine and is nothing to do with the check.
fn wait_for_task(id: usize) -> Result<(), &'static str> {
    super::wait_until_task_stopped(id, timer::now_nanos().saturating_add(PATIENCE_NANOS))
}

/// The index of the interface the ring added, once it is there.
fn wait_for_interface() -> Result<u32, &'static str> {
    let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        let found = net::core().with(|stack, _| {
            stack
                .interface_by_name(b"nrc0")
                .map(|interface| interface.index)
        });
        if let Some(index) = found {
            return Ok(index);
        }
        if timer::now_nanos() >= deadline {
            return Err("the ring's interface never appeared in the net core");
        }
        sched::sleep_for(1_000_000);
    }
}

/// The interface is what the HELLO said it was.
fn check_the_interface(index: u32) -> Result<(), &'static str> {
    net::core().with(|stack, _| {
        let Some(interface) = stack.interface(index) else {
            return Err("the interface went away again");
        };
        if interface.hardware != OUR_MAC {
            return Err("the interface has another hardware address than its HELLO gave it");
        }
        if interface.mtu != 1500 {
            return Err("the interface has another MTU than its HELLO gave it");
        }
        if !interface.is_up() {
            return Err("the interface is not up");
        }
        Ok(())
    })
}

/// Configure the address, as `ip` would.
fn give_it_an_address(index: u32) -> Result<(), &'static str> {
    net::core()
        .with(|stack, _| {
            stack.add_address(
                index,
                Address {
                    cidr: IpCidr::new(IpAddress::V4(OURS), 24),
                    peer: None,
                },
            )
        })
        .map_err(|_| "the interface would not take an address")
}

/// The driver the check plays: two VMOs, a port, and the ring's driver end.
struct Driver {
    /// The ring VMO.
    ring_vmo: Arc<Vmo>,
    /// The data VMO.
    data_vmo: Arc<Vmo>,
    /// The port the kernel rings.
    port: Arc<Port>,
    /// The ring, through the direct map.
    ring: Pages,
    /// The slots, through the direct map.
    data: Pages,
    /// The driver's end of the ring.
    side: DriverSide,
    /// Held so the frames stay.
    _ring_held: crate::user::vmo::Held,
    /// Likewise.
    _data_held: crate::user::vmo::Held,
    /// The slots the kernel posted for filling, as they were taken.
    posted: Vec<u32>,
    /// The kernel's completion port, once READY has brought it.
    completion_port: Option<Arc<Port>>,
}

impl Driver {
    /// Make what a driver makes.
    fn new() -> Result<Driver, &'static str> {
        let ring_bytes = (64 + ENTRIES as usize * 32).next_multiple_of(PAGE_SIZE as usize);
        let data_bytes = (ENTRIES as usize * SLOT as usize).next_multiple_of(PAGE_SIZE as usize);
        let ring_vmo =
            Vmo::new_anonymous(ring_bytes as u64 / PAGE_SIZE).map_err(|_| "no memory for a VMO")?;
        let data_vmo =
            Vmo::new_anonymous(data_bytes as u64 / PAGE_SIZE).map_err(|_| "no memory for a VMO")?;
        let ring_held = ring_vmo
            .hold(0, ring_vmo.len_pages())
            .map_err(|_| "the ring VMO could not be held")?;
        let data_held = data_vmo
            .hold(0, data_vmo.len_pages())
            .map_err(|_| "the data VMO could not be held")?;
        let mut ring = Pages::over(&ring_held, ring_vmo.len_bytes());
        let data = Pages::over(&data_held, data_vmo.len_bytes());
        let side = DriverSide::create(&mut ring, ring_bytes, ENTRIES, SLOT)
            .map_err(|_| "the ring header could not be written")?;
        Ok(Driver {
            ring_vmo,
            data_vmo,
            port: Port::new().map_err(|_| "no memory for a port")?,
            ring,
            data,
            side,
            _ring_held: ring_held,
            _data_held: data_held,
            posted: Vec::new(),
            completion_port: None,
        })
    }

    /// Send HELLO with the three handles, at the rights given.
    fn send_hello(&self, control: &Arc<Endpoint>, rights: Rights) -> Result<(), &'static str> {
        let hello = ferrix_netring::Message::Hello(Hello {
            version: VERSION,
            entries: ENTRIES,
            slot_bytes: SLOT,
            interface: Interface::new(
                b"nrc0",
                OUR_MAC,
                1500,
                InterfaceFlags {
                    carrier: true,
                    broadcast: true,
                    multicast: true,
                },
            ),
        });
        let mut bytes = [0_u8; MAX_MESSAGE];
        let written = hello
            .encode(&mut bytes)
            .map_err(|_| "a HELLO would not encode")?;
        let body = bytes.get(..written).unwrap_or_default().to_vec();
        let handles: Vec<Transfer> = vec![
            (Object::Vmo(Arc::clone(&self.ring_vmo)), rights),
            (Object::Vmo(Arc::clone(&self.data_vmo)), rights),
            (
                Object::Port(Arc::clone(&self.port)),
                Rights(Rights::WRITE.0 | Rights::TRANSFER.0),
            ),
        ];
        control
            .write(body, 3, || Ok::<Vec<Transfer>, Infallible>(handles))
            .map_err(|_| "a HELLO could not be sent")?;
        Ok(())
    }

    /// The next message the kernel sends, waiting for it.
    fn read(
        &self,
        control: &Arc<Endpoint>,
    ) -> Result<(ferrix_netring::Message, Option<Arc<Port>>), &'static str> {
        let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
        loop {
            match control.read(CHANNEL_MAX_BYTES, CHANNEL_MAX_HANDLES, false) {
                Ok(message) => {
                    let port = message.handles.iter().find_map(|(object, _)| match object {
                        Object::Port(port) => Some(Arc::clone(port)),
                        _ => None,
                    });
                    crate::object::dispose(message.handles.into_iter().map(|(object, _)| object));
                    return ferrix_netring::Message::decode(&message.bytes)
                        .map(|decoded| (decoded, port))
                        .map_err(|_| "the kernel sent a message that would not decode");
                }
                Err(ReadError::Empty) => {}
                Err(_) => return Err("the kernel closed the control channel"),
            }
            let ready = control.waiters().wait_until_deadline(
                || {
                    control
                        .signals()
                        .intersects(Signals::READABLE | Signals::PEER_CLOSED)
                },
                deadline,
            );
            if !ready {
                return Err("the kernel said nothing in time");
            }
        }
    }

    /// Take the receive slots the kernel posted, and say how many there were.
    fn take_posted(&mut self) -> Result<u32, &'static str> {
        let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
        loop {
            let mut taken = [Submission {
                slot: 0,
                length: 0,
                op: Op::Receive,
            }; ENTRIES as usize];
            let consumed = self
                .side
                .take(&mut self.ring, &mut taken)
                .map_err(|_| "the submission ring was corrupt")?;
            for entry in taken.iter().take(consumed.taken) {
                if entry.op == Op::Receive {
                    self.posted.push(entry.slot);
                }
            }
            if !self.posted.is_empty() {
                return Ok(self.posted.len() as u32);
            }
            if timer::now_nanos() >= deadline {
                return Err("the kernel posted nothing for the driver to fill");
            }
            sched::sleep_for(1_000_000);
        }
    }

    /// Write an ARP request into a posted slot and complete it, which is a
    /// frame arriving on the interface.
    fn deliver_arp_request(&mut self) -> Result<u32, &'static str> {
        let slot = self
            .posted
            .pop()
            .ok_or("no slot was posted to put a frame in")?;
        let frame = arp_request();
        let offset = self
            .side
            .layout()
            .slot_offset(slot)
            .map_err(|_| "a posted slot is not in the ring")?;
        if !self.data.copy_in(offset as u64, &frame) {
            return Err("a frame would not fit the slot it was posted for");
        }
        let length = frame.len() as u32;
        self.side
            .complete(&mut self.ring, slot, length, Status::Ok)
            .map_err(|_| "the completion ring would not take the frame")?;
        self.ring_the_kernel();
        Ok(1)
    }

    /// Wait for the kernel to submit a frame for sending, and check it with
    /// `check`.
    fn expect_transmission(
        &mut self,
        check: impl Fn(&[u8]) -> Result<(), &'static str>,
    ) -> Result<u32, &'static str> {
        let deadline = timer::now_nanos().saturating_add(PATIENCE_NANOS);
        loop {
            let mut taken = [Submission {
                slot: 0,
                length: 0,
                op: Op::Receive,
            }; ENTRIES as usize];
            let consumed = self
                .side
                .take(&mut self.ring, &mut taken)
                .map_err(|_| "the submission ring was corrupt")?;
            for entry in taken.iter().take(consumed.taken) {
                if entry.op == Op::Receive {
                    self.posted.push(entry.slot);
                    continue;
                }
                return self.check_reply(entry, &check);
            }
            if timer::now_nanos() >= deadline {
                return Err("the kernel never answered the ARP request");
            }
            sched::sleep_for(1_000_000);
        }
    }

    /// The frame the kernel asked to have sent passes `check`.
    fn check_reply(
        &mut self,
        entry: &Submission,
        check: impl Fn(&[u8]) -> Result<(), &'static str>,
    ) -> Result<u32, &'static str> {
        let offset = self
            .side
            .layout()
            .slot_offset(entry.slot)
            .map_err(|_| "a transmission named a slot the ring has not")?;
        let mut frame = vec![0_u8; entry.length as usize];
        if !self.data.copy_out(offset as u64, &mut frame) {
            return Err("a transmission ran past its slot");
        }
        let outcome = check(&frame);
        self.side
            .complete(&mut self.ring, entry.slot, entry.length, Status::Ok)
            .map_err(|_| "the completion ring would not take the transmission")?;
        self.ring_the_kernel();
        outcome?;
        Ok(1)
    }

    /// Publish what is staged and ring the kernel if it asked to be rung,
    /// which is what a driver does and what keeps the check off the recheck
    /// timer.
    fn ring_the_kernel(&mut self) {
        let Some(bell) = self.side.publish(&mut self.ring) else {
            return;
        };
        if let Some(port) = self.completion_port.as_ref() {
            let _ = port.queue_user(bell.key(), [u64::from(bell.tail()), 0]);
        }
    }
}

/// An ARP request for the interface's address, from the pretend peer.
fn arp_request() -> Vec<u8> {
    let packet = arp::Packet {
        operation: arp::Operation::Request,
        sender_mac: THEIR_MAC,
        sender_ip: THEIRS.octets(),
        target_mac: [0; 6],
        target_ip: OURS.octets(),
    };
    let mut body = [0_u8; arp::PACKET_LEN];
    let _ = packet.emit(&mut body);
    let header = ethernet::Header {
        destination: ethernet::BROADCAST,
        source: THEIR_MAC,
        vlan: None,
        ethertype: ethertype::ARP,
    };
    let mut frame = vec![0_u8; ethernet::HEADER_LEN + arp::PACKET_LEN];
    let _ = header.emit(&mut frame);
    if let Some(slot) = frame.get_mut(ethernet::HEADER_LEN..) {
        slot.copy_from_slice(&body);
    }
    frame
}

/// Whether a frame is the ARP reply the request earned.
fn check_arp_reply(frame: &[u8]) -> Result<(), &'static str> {
    let Ok((header, payload)) = ethernet::Header::parse(frame) else {
        return Err("the kernel asked to send something that is not an Ethernet frame");
    };
    if header.ethertype != ethertype::ARP {
        return Err("the kernel answered an ARP request with something else");
    }
    if header.source != OUR_MAC || header.destination != THEIR_MAC {
        return Err("the ARP reply is addressed wrongly");
    }
    let Ok(packet) = arp::Packet::parse(payload) else {
        return Err("the ARP reply is not an ARP packet");
    };
    if packet.operation != arp::Operation::Reply {
        return Err("the kernel answered an ARP request with another request");
    }
    if packet.sender_ip != OURS.octets() || packet.sender_mac != OUR_MAC {
        return Err("the ARP reply claims another address than the interface's");
    }
    if packet.target_ip != THEIRS.octets() {
        return Err("the ARP reply is for another asker");
    }
    Ok(())
}
