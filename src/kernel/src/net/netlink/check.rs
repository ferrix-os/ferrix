//! The netlink self-check, run at boot beside the net core's.
//!
//! It uses the loopback and nothing else, so it passes on a machine with no
//! network device, and it exercises the path `ip` takes rather than the pieces
//! it is made of: an `AF_NETLINK` socket is opened and bound, requests are
//! sent through it, and the replies are walked back with the same crate a
//! program would use.
//!
//! Five things are required:
//!
//! * a dump of the links answers, and the loopback is in it with its name and
//!   its `IFF_UP | IFF_LOOPBACK` flags;
//! * an address added through `RTM_NEWADDR` appears in the next `RTM_GETADDR`
//!   dump, and is gone from the one after `RTM_DELADDR`;
//! * a route added through `RTM_NEWROUTE` appears in the next `RTM_GETROUTE`
//!   dump, and is gone after `RTM_DELROUTE`;
//! * a request nothing answers earns `NLMSG_ERROR` with `EOPNOTSUPP`, and a
//!   message too short for its fixed header earns `EINVAL`;
//! * every reply is addressed to the port `getsockname` reported, which is
//!   what `libnetlink` checks before it believes a word of it.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::netlink::{
    IFA_LOCAL, IFF_LOOPBACK, IFF_UP, IFLA_IFNAME, IfAddrMsg, IfInfoMsg, NLM_F_ACK, NLM_F_CREATE,
    NLM_F_DUMP, NLM_F_EXCL, NLM_F_REQUEST, NLMSG_DONE, NLMSG_ERROR, NetlinkAddress, NlMsgErr,
    NlMsgHdr, RT_SCOPE_UNIVERSE, RT_TABLE_MAIN, RTA_DST, RTA_OIF, RTM_DELADDR, RTM_DELROUTE,
    RTM_GETADDR, RTM_GETLINK, RTM_GETROUTE, RTM_NEWADDR, RTM_NEWROUTE, RTN_UNICAST, RTPROT_BOOT,
    RtMsg,
};
use ferrix_linux_abi::socket::{AF_INET, SOCK_DGRAM};
use ferrix_netlink::{Address, Attr, Messages, Value, Writer};

use super::{NetlinkSocket, of};

/// What the check saw.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Why it did not run, if it did not.
    pub(crate) skipped: Option<&'static str>,
    /// How many links the dump answered with.
    pub(crate) links: usize,
    /// How many addresses the dump answered with, before one was added.
    pub(crate) addresses: usize,
    /// How many routes the table held, before one was added.
    pub(crate) routes: usize,
    /// How many requests were refused exactly as specified.
    pub(crate) refusals: usize,
}

/// The address the check adds and takes away again.
const ADDRESS: [u8; 4] = [10, 99, 0, 1];

/// The prefix the check's route covers.
const PREFIX: [u8; 4] = [10, 99, 7, 0];

/// How long a prefix both of those carry.
const PREFIX_LEN: u8 = 24;

/// A message type in the routing family's range that nothing answers:
/// `RTM_NEWQDISC`, which belongs to traffic control.
const UNANSWERED: u16 = 36;

/// The longest reply this check reads.
const REPLY: usize = 4096;

/// Run the check.
///
/// # Errors
///
/// A string naming what did not hold, which the caller turns into a panic.
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    let mut netlink = Netlink::open()?;

    let loopback = links(&mut netlink, &mut report)?;
    addresses(&mut netlink, &mut report, loopback)?;
    routes(&mut netlink, &mut report, loopback)?;
    refusals(&mut netlink, &mut report)?;
    Ok(report)
}

/// The links dump, which must hold the loopback with its name and flags.
///
/// Answers the loopback's index, which everything after it is done on.
fn links(netlink: &mut Netlink, report: &mut Report) -> Result<u32, &'static str> {
    let body = IfInfoMsg::default();
    let replies = netlink.talk(
        RTM_GETLINK,
        NLM_F_REQUEST | NLM_F_DUMP,
        &body.to_bytes(),
        &[],
    )?;
    report.links = replies.len();
    if replies.is_empty() {
        return Err("a dump of the links answered with nothing");
    }
    let mut loopback = None;
    for reply in &replies {
        let message = first(reply)?;
        let body = IfInfoMsg::from_bytes(message.body(IfInfoMsg::SIZE).unwrap_or_default())
            .ok_or("a link reply carried no ifinfomsg")?;
        let name = message
            .attributes(IfInfoMsg::SIZE)
            .find(IFLA_IFNAME)
            .ok_or("a link reply carried no name")?;
        if name.as_name() != b"lo" {
            continue;
        }
        if body.flags & (IFF_UP | IFF_LOOPBACK) != IFF_UP | IFF_LOOPBACK {
            return Err("the loopback was dumped without its up and loopback flags");
        }
        loopback = u32::try_from(body.index).ok();
    }
    loopback.ok_or("a dump of the links did not hold the loopback")
}

/// An address added, seen, and taken away again.
fn addresses(
    netlink: &mut Netlink,
    report: &mut Report,
    interface: u32,
) -> Result<(), &'static str> {
    report.addresses = dump_addresses(netlink)?.len();
    let body = IfAddrMsg {
        family: u8::try_from(AF_INET).unwrap_or(0),
        prefix_len: PREFIX_LEN,
        flags: 0,
        scope: RT_SCOPE_UNIVERSE,
        index: interface,
    };
    let attributes = [Attr::new(IFA_LOCAL, Value::Address(Address::V4(ADDRESS)))];
    let added = netlink.talk(
        RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        &body.to_bytes(),
        &attributes,
    )?;
    acknowledged(&added, "adding an address through netlink was refused")?;
    if !dump_addresses(netlink)?.contains(&ADDRESS) {
        return Err("an address added through netlink was not in the next dump");
    }
    let removed = netlink.talk(
        RTM_DELADDR,
        NLM_F_REQUEST | NLM_F_ACK,
        &body.to_bytes(),
        &attributes,
    )?;
    acknowledged(&removed, "removing an address through netlink was refused")?;
    if dump_addresses(netlink)?.contains(&ADDRESS) {
        return Err("an address removed through netlink was still in the next dump");
    }
    Ok(())
}

/// Every IPv4 address a dump answers with.
fn dump_addresses(netlink: &mut Netlink) -> Result<Vec<[u8; 4]>, &'static str> {
    let body = IfAddrMsg::default();
    let replies = netlink.talk(
        RTM_GETADDR,
        NLM_F_REQUEST | NLM_F_DUMP,
        &body.to_bytes(),
        &[],
    )?;
    let mut found = Vec::new();
    for reply in &replies {
        let message = first(reply)?;
        let carried = message
            .attributes(IfAddrMsg::SIZE)
            .find(IFA_LOCAL)
            .and_then(|attribute| attribute.as_address());
        if let Some(Address::V4(four)) = carried {
            found.push(four);
        }
    }
    Ok(found)
}

/// A route added, seen, and taken away again.
fn routes(netlink: &mut Netlink, report: &mut Report, interface: u32) -> Result<(), &'static str> {
    report.routes = dump_routes(netlink)?.len();
    let body = RtMsg {
        family: u8::try_from(AF_INET).unwrap_or(0),
        dst_len: PREFIX_LEN,
        src_len: 0,
        tos: 0,
        table: RT_TABLE_MAIN,
        protocol: RTPROT_BOOT,
        scope: RT_SCOPE_UNIVERSE,
        kind: RTN_UNICAST,
        flags: 0,
    };
    let attributes = [
        Attr::new(RTA_DST, Value::Address(Address::V4(PREFIX))),
        Attr::new(RTA_OIF, Value::U32(interface)),
    ];
    let added = netlink.talk(
        RTM_NEWROUTE,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE,
        &body.to_bytes(),
        &attributes,
    )?;
    acknowledged(&added, "adding a route through netlink was refused")?;
    if !dump_routes(netlink)?.contains(&PREFIX) {
        return Err("a route added through netlink was not in the next dump");
    }
    // The delete names only the destination, as `ip route del` sends one,
    // with no interface and no gateway: what it leaves out matches anything.
    let destination_only = [Attr::new(RTA_DST, Value::Address(Address::V4(PREFIX)))];
    let removed = netlink.talk(
        RTM_DELROUTE,
        NLM_F_REQUEST | NLM_F_ACK,
        &body.to_bytes(),
        &destination_only,
    )?;
    acknowledged(
        &removed,
        "removing a route by its destination alone through netlink was refused",
    )?;
    if dump_routes(netlink)?.contains(&PREFIX) {
        return Err("a route removed through netlink was still in the next dump");
    }
    // And a second delete finds nothing, which is ESRCH.
    let again = netlink.talk(
        RTM_DELROUTE,
        NLM_F_REQUEST | NLM_F_ACK,
        &body.to_bytes(),
        &destination_only,
    )?;
    match error_of(&again) {
        Ok(code) if code == -i32::from(Errno::ESRCH.0) => report.refusals += 1,
        _ => return Err("deleting a route that is gone was not refused with ESRCH"),
    }
    Ok(())
}

/// The destination of every IPv4 route a dump answers with.
fn dump_routes(netlink: &mut Netlink) -> Result<Vec<[u8; 4]>, &'static str> {
    let body = RtMsg::default();
    let replies = netlink.talk(
        RTM_GETROUTE,
        NLM_F_REQUEST | NLM_F_DUMP,
        &body.to_bytes(),
        &[],
    )?;
    let mut found = Vec::new();
    for reply in &replies {
        let message = first(reply)?;
        let carried = message
            .attributes(RtMsg::SIZE)
            .find(RTA_DST)
            .and_then(|attribute| attribute.as_address());
        if let Some(Address::V4(four)) = carried {
            found.push(four);
        }
    }
    Ok(found)
}

/// The two refusals: a type nothing answers, and a message too short for the
/// fixed header its type implies.
fn refusals(netlink: &mut Netlink, report: &mut Report) -> Result<(), &'static str> {
    let unanswered = netlink.talk(UNANSWERED, NLM_F_REQUEST | NLM_F_ACK, &[0_u8; 16], &[])?;
    match error_of(&unanswered)? {
        code if code == -i32::from(Errno::EOPNOTSUPP.0) => report.refusals += 1,
        _ => return Err("a request nothing answers was not refused with EOPNOTSUPP"),
    }
    // An `ifaddrmsg` is eight bytes; four of them is a message no family can
    // read.
    let short = netlink.talk(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, &[0_u8; 4], &[])?;
    match error_of(&short)? {
        code if code == -i32::from(Errno::EINVAL.0) => report.refusals += 1,
        _ => return Err("a message too short for its header was not refused with EINVAL"),
    }
    Ok(())
}

/// The first message of a reply.
fn first(reply: &[u8]) -> Result<ferrix_netlink::Message<'_>, &'static str> {
    Messages::new(reply)
        .next()
        .ok_or("a reply held no message at all")?
        .map_err(|_| "a reply the kernel wrote could not be walked back")
}

/// The `NLMSG_ERROR` a reply carries, whatever its number.
fn error_of(replies: &[Vec<u8>]) -> Result<i32, &'static str> {
    let reply = replies
        .first()
        .ok_or("a request was answered with nothing")?;
    let message = first(reply)?;
    if message.header.kind != NLMSG_ERROR {
        return Err("a request that should have been refused was answered");
    }
    NlMsgErr::from_bytes(message.payload)
        .map(|body| body.error)
        .ok_or("an error reply carried no nlmsgerr")
}

/// Require that `replies` is the acknowledgement `NLM_F_ACK` asked for, and
/// answer `refused` if it is anything else.
fn acknowledged(replies: &[Vec<u8>], refused: &'static str) -> Result<(), &'static str> {
    match error_of(replies) {
        Ok(0) => Ok(()),
        Ok(_) | Err(_) => Err(refused),
    }
}

/// An `AF_NETLINK` socket, bound, with the sequence numbers a talker keeps.
#[derive(Debug)]
struct Netlink {
    /// The socket itself.
    socket: Arc<NetlinkSocket>,
    /// The port identifier `getsockname` reported, which every reply must be
    /// addressed to.
    port: u32,
    /// The sequence number of the last request.
    seq: u32,
}

impl Netlink {
    /// Open a socket and bind it, as `rtnl_open` does.
    fn open() -> Result<Netlink, &'static str> {
        let file = NetlinkSocket::open(crate::net::first(), SOCK_DGRAM, false, (0, 0))
            .map_err(|_| "a netlink socket could not be opened")?;
        let socket = of(&file).ok_or("a netlink socket's open file does not hold one")?;
        socket
            .bind(&NetlinkAddress::default().to_bytes())
            .map_err(|_| "a netlink socket could not be bound")?;
        let name = socket.local_name();
        let bound = NetlinkAddress::parse(&name, name.len())
            .map_err(|_| "getsockname answered something that is not a sockaddr_nl")?;
        if bound.pid == 0 {
            return Err("a bound netlink socket was given no port identifier");
        }
        Ok(Netlink {
            socket,
            port: bound.pid,
            seq: 0,
        })
    }

    /// Send one request and take every reply to it.
    fn talk(
        &mut self,
        kind: u16,
        flags: u16,
        body: &[u8],
        attributes: &[Attr<'_>],
    ) -> Result<Vec<Vec<u8>>, &'static str> {
        self.seq += 1;
        let mut buffer = [0_u8; 256];
        let mut writer = Writer::new(&mut buffer);
        let header = NlMsgHdr {
            len: 0,
            kind,
            flags,
            seq: self.seq,
            pid: self.port,
        };
        let _written = writer
            .message(header, body, attributes)
            .map_err(|_| "a request did not fit the buffer the check builds it in")?;
        let request = writer.written().to_vec();
        let _sent = self
            .socket
            .send(&request, None)
            .map_err(|_| "a netlink request was refused by the socket")?;
        self.take()
    }

    /// Take replies until the dump ends, the socket is empty, or one of them
    /// is an error.
    fn take(&self) -> Result<Vec<Vec<u8>>, &'static str> {
        let mut replies = Vec::new();
        let mut out = [0_u8; REPLY];
        loop {
            let taken = match self.socket.recv(&mut out, 0, true) {
                Ok(taken) => taken,
                Err(Errno::EAGAIN) => return Ok(replies),
                Err(_) => return Err("a netlink reply could not be taken"),
            };
            let (received, from) = taken;
            if received.full > received.bytes {
                return Err("a netlink reply was longer than the check's buffer");
            }
            let from = from.ok_or("a netlink reply arrived without an address")?;
            let from = NetlinkAddress::parse(&from, from.len())
                .map_err(|_| "a netlink reply came from something that is not a sockaddr_nl")?;
            if from.pid != 0 {
                return Err("a netlink reply did not come from the kernel");
            }
            let reply = out.get(..received.bytes).unwrap_or_default().to_vec();
            let message = first(&reply)?;
            if message.header.pid != self.port {
                return Err("a netlink reply was addressed to another socket's port");
            }
            if message.header.seq != self.seq {
                return Err("a netlink reply answered another request's sequence number");
            }
            let done = message.header.kind == NLMSG_DONE;
            replies.push(reply);
            if done {
                // The `NLMSG_DONE` is the end of the dump and not part of it.
                let _ = replies.pop();
                return Ok(replies);
            }
        }
    }
}
