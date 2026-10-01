//! `NETLINK_ROUTE`: the requests `ip` makes, answered from the net core.
//!
//! One function reaches this module — [`answer`] — and it is called with the
//! stack locked, so nothing here allocates, waits or touches a program's
//! memory. It reads a buffer of requests that a program wrote and writes the
//! replies into a buffer the caller already owns, which is what makes it safe
//! to run inside [`crate::net::NetCore::with`].
//!
//! # What is answered
//!
//! Dumps of links, addresses, routes and neighbours, and the four calls that
//! change something: a link's flags and MTU, an address added or removed, a
//! route added or removed. Anything else is `EOPNOTSUPP`, a message too short
//! for the fixed header its type implies is `EINVAL`, and a change asked for
//! without `NLM_F_REQUEST` is `EINVAL` — a notification is what the kernel
//! sends, not what it takes.
//!
//! `RTM_NEWLINK` on an interface that exists is treated as `RTM_SETLINK`,
//! because that is the message `ip link set dev eth0 up` actually sends;
//! creating an interface has nowhere to go and is refused.
//!
//! # A get is a dump
//!
//! Every `RTM_GET*` answers with the whole table and its `NLMSG_DONE`, whether
//! or not `NLM_F_DUMP` was set, because there are no filters here yet: the
//! attributes that narrow a dump to one link or one family are read by nobody.
//! A reader that asked for one entry therefore has one more message to walk
//! than it expected, which every netlink reader can do -- it is the same shape
//! it would get from a kernel whose table holds one row. Filters belong with
//! the first program that sends one.

use alloc::sync::Arc;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::netlink::{
    ARPHRD_ETHER, ARPHRD_LOOPBACK, IFA_ADDRESS, IFA_F_PERMANENT, IFA_LABEL, IFA_LOCAL, IFF_UP,
    IFLA_ADDRESS, IFLA_IFNAME, IFLA_LINKINFO, IFLA_MTU, IfAddrMsg, IfInfoMsg, NDA_DST, NDA_LLADDR,
    NLM_F_ACK, NLM_F_MULTI, NLM_F_REQUEST, NLMSG_MIN_TYPE, NdMsg, NlMsgHdr, RT_SCOPE_HOST,
    RT_SCOPE_LINK, RT_SCOPE_UNIVERSE, RT_TABLE_MAIN, RTA_DST, RTA_GATEWAY, RTA_OIF, RTA_PRIORITY,
    RTM_DELADDR, RTM_DELLINK, RTM_DELNEIGH, RTM_DELROUTE, RTM_GETADDR, RTM_GETLINK, RTM_GETNEIGH,
    RTM_GETROUTE, RTM_NEWADDR, RTM_NEWLINK, RTM_NEWNEIGH, RTM_NEWROUTE, RTM_SETLINK, RTN_UNICAST,
    RtMsg,
};
use ferrix_linux_abi::socket::{AF_INET, AF_INET6, AF_UNSPEC};
use ferrix_net::iface::{Address as IfaceAddress, Medium};
use ferrix_net::route::{Origin, Route};
use ferrix_net::{IpAddress, IpCidr, Ipv4, Ipv6, Stack};
use ferrix_netlink::{Address, Attr, Attributes, Message, Messages, Value, Writer};

use super::link;
use crate::net::NetNamespace;
use crate::net::namespace::{MAX_ADDRESSES, MAX_ROUTES};
use crate::syscall::process::Process;

/// `IFLA_INFO_KIND` inside `IFLA_LINKINFO` for a virtual pair: `veth`, nul
/// terminated, padded as a nested attribute is.
const VETH_KIND: [u8; 12] = [9, 0, 1, 0, b'v', b'e', b't', b'h', 0, 0, 0, 0];

/// The most attributes any reply here carries.
const MAX_ATTRS: usize = 4;

/// A short list of attributes, built where there is nothing to allocate with.
#[derive(Debug)]
struct Attrs<'a> {
    /// The attributes, of which the first `count` are set.
    items: [Attr<'a>; MAX_ATTRS],
    /// How many are set.
    count: usize,
}

impl<'a> Attrs<'a> {
    /// An empty list.
    fn new() -> Attrs<'a> {
        Attrs {
            items: [Attr::new(0, Value::U32(0)); MAX_ATTRS],
            count: 0,
        }
    }

    /// Add one, or drop it if the list is full. A reply with one attribute
    /// missing is better than a reply that is not sent, and the ceiling is
    /// this module's own.
    fn push(&mut self, attribute: Attr<'a>) {
        if let Some(slot) = self.items.get_mut(self.count) {
            *slot = attribute;
            self.count += 1;
        }
    }

    /// What was added, in order.
    fn as_slice(&self) -> &[Attr<'a>] {
        self.items.get(..self.count).unwrap_or_default()
    }
}

/// Answer every request in `request`, writing the replies into `out`.
///
/// Each request is answered with its own namespace's stack locked, so nothing
/// it does allocates, waits or touches a program's memory -- except the few
/// that touch two namespaces or allocate, which [`link`] answers with no lock
/// held. Answers how many bytes of `out` were written. A request that changes
/// something is refused `EPERM` unless `privileged`: the tables are the
/// namespace's owner's, and one that adds to them first asks the namespace's
/// charge for room.
pub(super) fn answer(
    ns: &Arc<NetNamespace>,
    actor: Option<&Process>,
    port: u32,
    request: &[u8],
    out: &mut [u8],
    privileged: bool,
) -> usize {
    let mut writer = Writer::new(out);
    let mut changed = false;
    for message in Messages::new(request) {
        match message {
            Ok(message) => {
                let header = message.header;
                let asked = header.flags & NLM_F_REQUEST != 0;
                if asked
                    && privileged
                    && adds(header.kind)
                    && let Err(errno) = ns.admit()
                {
                    let _ = writer.error(header, i32::from(errno.0), port);
                    continue;
                }
                if link::wants(&message) && asked {
                    link::answer(ns, actor, port, &message, &mut writer, privileged);
                } else {
                    ns.core()
                        .with(|stack, _| one(stack, port, &message, &mut writer, privileged));
                }
                changed |= asked && privileged && changes(header.kind);
            }
            Err(_) => {
                // A length that cannot be read leaves no header to echo, so
                // the error names the request it answers as far as it can.
                let _ = writer.error(NlMsgHdr::default(), einval(), port);
                break;
            }
        }
    }
    if changed {
        // What the tables hold now, and the carrier of any pair an end of
        // which came up or went down.
        let _ = ns.fit(0);
        ns.refresh_carriers();
    }
    writer.len()
}

/// Whether a message of this type may add to the tables.
const fn adds(kind: u16) -> bool {
    matches!(kind, RTM_NEWLINK | RTM_NEWADDR | RTM_NEWROUTE)
}

/// `EINVAL` as an `NLMSG_ERROR` carries it.
fn einval() -> i32 {
    i32::from(Errno::EINVAL.0)
}

/// Answer one request.
fn one(
    stack: &mut Stack,
    port: u32,
    message: &Message<'_>,
    writer: &mut Writer<'_>,
    privileged: bool,
) {
    let header = message.header;
    let acknowledge = header.flags & NLM_F_ACK != 0;
    // Netlink's own numbers -- NOOP, ERROR, DONE -- are not requests to a
    // family, as `netlink_rcv_skb` skips them.
    if header.kind < NLMSG_MIN_TYPE {
        if acknowledge {
            let _ = writer.error(header, 0, port);
        }
        return;
    }
    if header.flags & NLM_F_REQUEST == 0 {
        // Linux ignores a message that is not a request. One that would change
        // something is refused instead: a program that sent a notification to
        // the kernel is confused about which direction it is talking in, and
        // silence would leave it believing the change happened.
        if changes(header.kind) {
            let _ = writer.error(header, einval(), port);
        } else if acknowledge {
            let _ = writer.error(header, 0, port);
        }
        return;
    }
    if changes(header.kind) && !privileged {
        let _ = writer.error(header, i32::from(Errno::EPERM.0), port);
        return;
    }
    let outcome = match header.kind {
        RTM_GETLINK => dump_links(stack, port, &header, writer),
        RTM_GETADDR => dump_addresses(stack, port, &header, writer),
        RTM_GETROUTE => dump_routes(stack, port, &header, writer),
        RTM_GETNEIGH => dump_neighbours(stack, port, &header, writer),
        RTM_SETLINK | RTM_NEWLINK => set_link(stack, message),
        RTM_NEWADDR | RTM_DELADDR => change_address(stack, message, header.kind == RTM_NEWADDR),
        RTM_NEWROUTE | RTM_DELROUTE => change_route(stack, message, header.kind == RTM_NEWROUTE),
        _ => Err(Errno::EOPNOTSUPP),
    };
    match outcome {
        // A dump ends with the `NLMSG_DONE` that tells a reader to stop.
        Ok(true) => {
            let _ = writer.done(header, port);
        }
        Ok(false) => {
            if acknowledge {
                let _ = writer.error(header, 0, port);
            }
        }
        Err(errno) => {
            let _ = writer.error(header, i32::from(errno.0), port);
        }
    }
}

/// Whether a message of this type asks for a change rather than a report.
const fn changes(kind: u16) -> bool {
    matches!(
        kind,
        RTM_NEWLINK
            | RTM_DELLINK
            | RTM_SETLINK
            | RTM_NEWADDR
            | RTM_DELADDR
            | RTM_NEWROUTE
            | RTM_DELROUTE
            | RTM_NEWNEIGH
            | RTM_DELNEIGH
    )
}

/// The header every message of a dump carries.
fn part(kind: u16, request: &NlMsgHdr, port: u32) -> NlMsgHdr {
    NlMsgHdr {
        len: 0,
        kind,
        flags: NLM_F_MULTI,
        seq: request.seq,
        pid: port,
    }
}

/// `RTM_GETLINK`: one `RTM_NEWLINK` per interface.
fn dump_links(
    stack: &Stack,
    port: u32,
    request: &NlMsgHdr,
    writer: &mut Writer<'_>,
) -> Result<bool, Errno> {
    for interface in stack.interfaces() {
        let body = IfInfoMsg {
            family: u8::try_from(AF_UNSPEC).unwrap_or(0),
            kind: match interface.medium {
                Medium::Ethernet => ARPHRD_ETHER,
                Medium::Loopback => ARPHRD_LOOPBACK,
            },
            index: i32::try_from(interface.index).unwrap_or(0),
            flags: interface.flags,
            change: 0,
        };
        let mut attributes = Attrs::new();
        attributes.push(Attr::new(
            IFLA_IFNAME,
            Value::Name(interface.name.as_bytes()),
        ));
        attributes.push(Attr::new(
            IFLA_ADDRESS,
            Value::Bytes(interface.hardware.as_slice()),
        ));
        attributes.push(Attr::new(IFLA_MTU, Value::U32(interface.mtu)));
        // A virtual pair says what it is, as `ip -d link` reads it.
        if matches!(interface.backing, ferrix_net::iface::Backing::Veth { .. }) {
            attributes.push(Attr::new(IFLA_LINKINFO, Value::Bytes(&VETH_KIND)));
        }
        if writer
            .message(
                part(RTM_NEWLINK, request, port),
                &body.to_bytes(),
                attributes.as_slice(),
            )
            .is_err()
        {
            // The buffer is full. The dump ends where it is, with the `DONE`
            // the caller writes next, rather than half a message.
            break;
        }
    }
    Ok(true)
}

/// `RTM_GETADDR`: one `RTM_NEWADDR` per address on each interface.
fn dump_addresses(
    stack: &Stack,
    port: u32,
    request: &NlMsgHdr,
    writer: &mut Writer<'_>,
) -> Result<bool, Errno> {
    for interface in stack.interfaces() {
        for configured in &interface.addresses {
            let address = wire_address(configured.cidr.address());
            let body = IfAddrMsg {
                family: family_of(configured.cidr.address()),
                prefix_len: configured.cidr.prefix_len(),
                flags: u8::try_from(IFA_F_PERMANENT).unwrap_or(0),
                scope: scope_of(configured.cidr.address()),
                index: interface.index,
            };
            let peer = configured.peer.map_or(address, wire_address);
            let mut attributes = Attrs::new();
            // `IFA_ADDRESS` is the peer's address on a point-to-point link and
            // the address itself everywhere else; `IFA_LOCAL` is always this
            // host's. A reader that knows only one of them reads the right
            // thing either way, which is why Linux sends both.
            attributes.push(Attr::new(IFA_ADDRESS, Value::Address(peer)));
            attributes.push(Attr::new(IFA_LOCAL, Value::Address(address)));
            attributes.push(Attr::new(IFA_LABEL, Value::Name(interface.name.as_bytes())));
            if writer
                .message(
                    part(RTM_NEWADDR, request, port),
                    &body.to_bytes(),
                    attributes.as_slice(),
                )
                .is_err()
            {
                return Ok(true);
            }
        }
    }
    Ok(true)
}

/// `RTM_GETROUTE`: one `RTM_NEWROUTE` per route, in the order a lookup walks
/// them.
fn dump_routes(
    stack: &Stack,
    port: u32,
    request: &NlMsgHdr,
    writer: &mut Writer<'_>,
) -> Result<bool, Errno> {
    for route in stack.routes().entries() {
        let destination = route.destination;
        let body = RtMsg {
            family: family_of(destination.address()),
            dst_len: destination.prefix_len(),
            src_len: 0,
            tos: 0,
            table: RT_TABLE_MAIN,
            protocol: route.origin.code(),
            scope: if route.gateway.is_some() {
                RT_SCOPE_UNIVERSE
            } else {
                RT_SCOPE_LINK
            },
            kind: RTN_UNICAST,
            flags: 0,
        };
        let destination_bytes = wire_address(destination.address());
        let gateway = route.gateway.map(wire_address);
        let mut attributes = Attrs::new();
        // A default route carries no `RTA_DST`: that is how a reader tells it
        // from a route to the address `0.0.0.0` itself.
        if destination.prefix_len() != 0 {
            attributes.push(Attr::new(RTA_DST, Value::Address(destination_bytes)));
        }
        if let Some(gateway) = gateway {
            attributes.push(Attr::new(RTA_GATEWAY, Value::Address(gateway)));
        }
        attributes.push(Attr::new(RTA_OIF, Value::U32(route.interface)));
        attributes.push(Attr::new(RTA_PRIORITY, Value::U32(route.metric)));
        if writer
            .message(
                part(RTM_NEWROUTE, request, port),
                &body.to_bytes(),
                attributes.as_slice(),
            )
            .is_err()
        {
            break;
        }
    }
    Ok(true)
}

/// `RTM_GETNEIGH`: one `RTM_NEWNEIGH` per entry in the neighbour cache.
fn dump_neighbours(
    stack: &Stack,
    port: u32,
    request: &NlMsgHdr,
    writer: &mut Writer<'_>,
) -> Result<bool, Errno> {
    for entry in stack.neighbors().entries() {
        let body = NdMsg {
            family: family_of(entry.address),
            ifindex: i32::try_from(entry.interface).unwrap_or(0),
            state: entry.state.nud(),
            flags: 0,
            kind: RTN_UNICAST,
        };
        let address = wire_address(entry.address);
        let mut attributes = Attrs::new();
        attributes.push(Attr::new(NDA_DST, Value::Address(address)));
        if let Some(mac) = entry.mac.as_ref() {
            attributes.push(Attr::new(NDA_LLADDR, Value::Bytes(mac.as_slice())));
        }
        if writer
            .message(
                part(RTM_NEWNEIGH, request, port),
                &body.to_bytes(),
                attributes.as_slice(),
            )
            .is_err()
        {
            break;
        }
    }
    Ok(true)
}

/// `RTM_SETLINK`, and the `RTM_NEWLINK` that `ip link set` actually sends:
/// bring an interface up or down, and take an MTU if one is given.
fn set_link(stack: &mut Stack, message: &Message<'_>) -> Result<bool, Errno> {
    let body = fixed::<IfInfoMsg>(message, IfInfoMsg::SIZE, IfInfoMsg::from_bytes)?;
    let attributes = message.attributes(IfInfoMsg::SIZE);
    let index = link_index(stack, body.index, &attributes)?;
    if let Some(mtu) = attributes.find(IFLA_MTU).and_then(|found| found.as_u32()) {
        let interface = stack.interface_mut(index).ok_or(Errno::ENODEV)?;
        interface.mtu = mtu;
    }
    // A name for an interface that is there: only while it is down, as
    // `dev_change_name` has it (`EBUSY`), and never one another has.
    if body.index > 0
        && let Some(name) = attributes.find(IFLA_IFNAME).map(|found| found.as_name())
    {
        let current = stack.interface(index).ok_or(Errno::ENODEV)?;
        if current.name.as_bytes() != name {
            if current.is_up() {
                return Err(Errno::EBUSY);
            }
            stack
                .rename_interface(index, name)
                .map_err(|error| match error {
                    ferrix_net::Error::AddressInUse => Errno::EEXIST,
                    _ => Errno::ENODEV,
                })?;
        }
    }
    // `ifi_change` says which flags the message is about and `ifi_flags` what
    // they should become, so a request that leaves `change` empty changes
    // nothing however its flags are set. Only `IFF_UP` is ours to act on; the
    // rest describe the link rather than ask anything of it.
    if body.change & IFF_UP != 0 {
        stack
            .set_up(index, body.flags & IFF_UP != 0)
            .map_err(|_| Errno::ENODEV)?;
    }
    Ok(false)
}

/// Which interface a link message is about: its index, or the name it gave.
fn link_index(stack: &Stack, index: i32, attributes: &Attributes<'_>) -> Result<u32, Errno> {
    if index != 0 {
        let index = u32::try_from(index).map_err(|_| Errno::EINVAL)?;
        return stack
            .interface(index)
            .map(|interface| interface.index)
            .ok_or(Errno::ENODEV);
    }
    let name = attributes
        .find(IFLA_IFNAME)
        .map(|found| found.as_name())
        .ok_or(Errno::EINVAL)?;
    stack
        .interface_by_name(name)
        .map(|interface| interface.index)
        // A link message naming an interface that is not there would create
        // one on Linux. There is nothing here to create: every interface comes
        // from a driver.
        .ok_or(Errno::EOPNOTSUPP)
}

/// `RTM_NEWADDR` and `RTM_DELADDR`.
fn change_address(stack: &mut Stack, message: &Message<'_>, add: bool) -> Result<bool, Errno> {
    let body = fixed::<IfAddrMsg>(message, IfAddrMsg::SIZE, IfAddrMsg::from_bytes)?;
    let attributes = message.attributes(IfAddrMsg::SIZE);
    let carried = attributes
        .find(IFA_LOCAL)
        .or_else(|| attributes.find(IFA_ADDRESS))
        .and_then(|found| found.as_address())
        .ok_or(Errno::EINVAL)?;
    if u16::from(body.family) != carried.family() {
        return Err(Errno::EINVAL);
    }
    let address = core_address(carried);
    let index = stack
        .interface(body.index)
        .map(|interface| interface.index)
        .ok_or(Errno::ENODEV)?;
    if add {
        if stack.address_count() >= MAX_ADDRESSES {
            return Err(Errno::ENOSPC);
        }
        stack
            .add_address(
                index,
                IfaceAddress {
                    cidr: IpCidr::new(address, body.prefix_len),
                    peer: None,
                },
            )
            .map_err(|_| Errno::EADDRNOTAVAIL)?;
    } else {
        stack
            .remove_address(index, address)
            .map_err(|_| Errno::EADDRNOTAVAIL)?;
    }
    Ok(false)
}

/// `RTM_NEWROUTE` and `RTM_DELROUTE`.
fn change_route(stack: &mut Stack, message: &Message<'_>, add: bool) -> Result<bool, Errno> {
    let body = fixed::<RtMsg>(message, RtMsg::SIZE, RtMsg::from_bytes)?;
    let attributes = message.attributes(RtMsg::SIZE);
    let gateway = attributes
        .find(RTA_GATEWAY)
        .and_then(|found| found.as_address())
        .map(core_address);
    let destination = match attributes
        .find(RTA_DST)
        .and_then(|found| found.as_address())
        .map(core_address)
    {
        Some(address) => IpCidr::new(address, body.dst_len),
        // No destination is the default route, and its family comes from the
        // message rather than from an address that is not there.
        None => IpCidr::new(unspecified(body.family)?, body.dst_len),
    };
    let metric = attributes
        .find(RTA_PRIORITY)
        .and_then(|found| found.as_u32());
    if !add {
        // A delete names what it knows and matches anything it leaves out:
        // `ip route del default` sends no interface and no gateway at all.
        let interface = match attributes.find(RTA_OIF).and_then(|found| found.as_u32()) {
            Some(index) => Some(
                stack
                    .interface(index)
                    .map(|found| found.index)
                    .ok_or(Errno::ENODEV)?,
            ),
            None => None,
        };
        if !stack
            .routes_mut()
            .remove_matching(destination, interface, gateway, metric)
        {
            // Linux answers a delete of a route nobody has with ESRCH, and
            // `ip` prints it as "No such process", which is the message every
            // Linux user has already seen from `ip route del`.
            return Err(Errno::ESRCH);
        }
        return Ok(false);
    }
    let interface = route_interface(stack, &attributes, gateway)?;
    if stack.routes().entries().len() >= MAX_ROUTES {
        return Err(Errno::ENOSPC);
    }
    stack.routes_mut().add(Route {
        destination,
        gateway,
        interface,
        metric: metric.unwrap_or(0),
        origin: origin_of(body.protocol),
    });
    Ok(false)
}

/// Which interface a route goes by: the one it names, or the one the gateway
/// is reached through.
///
/// `ip route add default via 10.0.2.2` sends no `RTA_OIF` at all, so a
/// gateway that is already on-link is what names the interface.
fn route_interface(
    stack: &Stack,
    attributes: &Attributes<'_>,
    gateway: Option<IpAddress>,
) -> Result<u32, Errno> {
    if let Some(index) = attributes.find(RTA_OIF).and_then(|found| found.as_u32()) {
        return stack
            .interface(index)
            .map(|interface| interface.index)
            .ok_or(Errno::ENODEV);
    }
    let gateway = gateway.ok_or(Errno::EINVAL)?;
    stack
        .routes()
        .lookup(gateway)
        .map(|hop| hop.interface)
        .ok_or(Errno::ENETUNREACH)
}

/// The fixed header a message's type implies, or `EINVAL` for a message too
/// short to hold one.
fn fixed<T>(
    message: &Message<'_>,
    size: usize,
    parse: impl Fn(&[u8]) -> Option<T>,
) -> Result<T, Errno> {
    message.body(size).and_then(parse).ok_or(Errno::EINVAL)
}

/// The `AF_` number of an address's family, as a fixed header carries it.
fn family_of(address: IpAddress) -> u8 {
    let family = if address.is_v4() { AF_INET } else { AF_INET6 };
    u8::try_from(family).unwrap_or(0)
}

/// The scope Linux reports for an address: the host for a loopback address,
/// the link for a link-local one, and everywhere for the rest.
fn scope_of(address: IpAddress) -> u8 {
    if address.is_loopback() {
        return RT_SCOPE_HOST;
    }
    let link_local = match address {
        IpAddress::V4(four) => four.is_link_local(),
        IpAddress::V6(six) => six.is_link_local(),
    };
    if link_local {
        RT_SCOPE_LINK
    } else {
        RT_SCOPE_UNIVERSE
    }
}

/// The unspecified address of a family, for a default route with no
/// destination attribute.
fn unspecified(family: u8) -> Result<IpAddress, Errno> {
    match u16::from(family) {
        AF_INET => Ok(IpAddress::V4(Ipv4::UNSPECIFIED)),
        AF_INET6 => Ok(IpAddress::V6(Ipv6::UNSPECIFIED)),
        _ => Err(Errno::EAFNOSUPPORT),
    }
}

/// An address as an attribute carries it.
fn wire_address(address: IpAddress) -> Address {
    match address {
        IpAddress::V4(four) => Address::V4(four.octets()),
        IpAddress::V6(six) => Address::V6(six.octets()),
    }
}

/// An address an attribute carried, as the net core holds it.
fn core_address(address: Address) -> IpAddress {
    match address {
        Address::V4(four) => IpAddress::V4(Ipv4::new(four)),
        Address::V6(six) => IpAddress::V6(Ipv6::new(six)),
    }
}

/// Where a route came from, as its `RTPROT_` number says.
fn origin_of(protocol: u8) -> Origin {
    match protocol {
        code if code == Origin::Kernel.code() => Origin::Kernel,
        code if code == Origin::Static.code() => Origin::Static,
        code if code == Origin::RouterAdvertisement.code() => Origin::RouterAdvertisement,
        code if code == Origin::Dhcp.code() => Origin::Dhcp,
        // `RTPROT_BOOT` is what `ip route add` sends when nobody says
        // otherwise, and what anything unrecognised is closest to.
        _ => Origin::Boot,
    }
}
