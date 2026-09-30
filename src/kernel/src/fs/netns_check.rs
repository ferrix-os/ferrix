//! Network namespaces, proved at boot (`docs/NETNS.md` section 8): the rules
//! NN1 to NN17 attempted and refused or accepted, through the system-call
//! layer where a rule is about a call and through the sockets and the netlink
//! socket of a namespace where it is about what crosses it.
//!
//! A process that is root makes a network namespace and finds only a loopback
//! that is down (NN1); brings it up by netlink and by `ioctl` and then owns
//! `127.0.0.1` and `::1` (NN2); shares no port and no socket with another
//! (NN3), and keeps a socket made before it moved (NN4). A process that is
//! uid 1000 is refused one (NN5) until it makes a user namespace with it, and
//! is then refused the first namespace's tables and allowed its own (NN6), and
//! a raw socket the same way (NN7). Two namespaces are joined by a veth pair
//! that carries a datagram and a stream, a pair in a third namespace hears
//! nothing of it (NN8, NN16), an end is moved only with authority over both
//! owners (NN9), a physical device moves, is served where it went, and goes
//! home when the namespace ends (NN10), as a pair's ends vanish (NN13).
//! Abstract `AF_UNIX` names are per namespace (NN11), `/proc/<pid>/ns/net` and
//! `/proc/net` show the reader's (NN12), and a namespace's tables stop at
//! their ceilings (NN17). NN14's charge is `kmem_check`'s.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_linux_abi::inet::InetAddress;
use ferrix_linux_abi::netlink::{
    IFA_ADDRESS, IFA_LOCAL, IFF_UP, IFLA_IFNAME, IFLA_INFO_DATA, IFLA_INFO_KIND, IFLA_LINKINFO,
    IFLA_NET_NS_FD, IFLA_NET_NS_PID, IfAddrMsg, IfInfoMsg, NLM_F_ACK, NLM_F_CREATE, NLM_F_REQUEST,
    NLMSG_ERROR, NlMsgHdr, RT_SCOPE_UNIVERSE, RT_TABLE_MAIN, RTA_DST, RTA_OIF, RTM_DELLINK,
    RTM_NEWADDR, RTM_NEWLINK, RTM_NEWROUTE, RTM_SETLINK, RTN_UNICAST, RTPROT_STATIC, RtMsg,
    VETH_INFO_PEER,
};
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::socket::{
    AF_INET, AF_UNIX, IFNAMSIZ, SIOCGIFFLAGS, SIOCSIFFLAGS, SOCK_DGRAM, SOCK_RAW, SOCK_STREAM,
};
use ferrix_net::socket::Family;
use ferrix_netlink::{Address, Attr, Messages, Value, Writer};
use ferrix_vfs::Errno;

use crate::fs::mount_check::{Page, Report as Counts, Tally, by_number, close, page_for};
use crate::fs::namespace_check::{read_file, read_link, unshare};
use crate::net::netlink::NetlinkSocket;
use crate::net::socket::{InetKind, InetSocket, of as inet_of};
use crate::net::{self, NetNamespace, device, namespace, veth};
use crate::syscall::family;
use crate::syscall::namespace::CLONE_NEWUSER;
use crate::syscall::process::{self, Process};
use crate::syscall::userns;

/// `CLONE_NEWNET`.
const CLONE_NEWNET: u64 = namespace::CLONE_NEWNET;
/// `CLONE_THREAD`.
const CLONE_THREAD: u64 = ferrix_linux_abi::types::CLONE_THREAD;

/// The ids the unprivileged process has.
const UID: u32 = 1000;

/// How many bytes a check's netlink request is built in.
const REQUEST: usize = 1024;
/// How many a reply is read into.
const REPLY: usize = 4096;

/// What the check saw, for the boot line.
pub(crate) fn run() -> Result<Counts, &'static str> {
    let mut counts = Counts::default();
    let mut tally = Tally {
        report: &mut counts,
    };
    let mut made = Vec::new();
    let outcome = creation(&mut tally, &mut made)
        .and_then(|()| isolation(&mut tally, &mut made))
        .and_then(|()| loopback(&mut tally, &mut made))
        .and_then(|()| privileges(&mut tally, &mut made))
        .and_then(|()| pairs(&mut tally, &mut made))
        .and_then(|()| devices(&mut tally, &mut made))
        .and_then(|()| abstract_names(&mut tally, &mut made))
        .and_then(|()| proc_views(&mut tally, &mut made))
        .and_then(|()| ceilings(&mut tally, &mut made));
    for each in &made {
        process::kill(each, crate::object::job::KILLED_STATUS);
    }
    outcome.map(|()| counts)
}

// ---------------------------------------------------------------- helpers

/// `call` by number.
fn call(process: &Process, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
    by_number(process, call, args)
}

/// A process the check made, to be ended with the rest.
fn spawn(made: &mut Vec<Arc<Process>>) -> Result<Arc<Process>, &'static str> {
    let one = process::new_for_check().map_err(|_| "could not make a network check's process")?;
    made.push(Arc::clone(&one));
    Ok(one)
}

/// A process that is uid and gid 1000.
fn unprivileged(made: &mut Vec<Arc<Process>>) -> Result<Arc<Process>, &'static str> {
    let one = spawn(made)?;
    for (call_to, what) in [
        (Syscall::Setgid, "could not become gid 1000"),
        (Syscall::Setuid, "could not become uid 1000"),
    ] {
        let _ = call(&one, call_to, [u64::from(UID), 0, 0, 0, 0, 0]).map_err(|_| what)?;
    }
    Ok(one)
}

/// A root process in a network namespace of its own.
fn in_own_namespace(
    tally: &mut Tally<'_>,
    made: &mut Vec<Arc<Process>>,
) -> Result<(Arc<Process>, Arc<NetNamespace>), &'static str> {
    let one = spawn(made)?;
    tally.ok(
        unshare(&one, CLONE_NEWNET),
        "root was refused a network namespace",
    )?;
    let ns = one.net_ns();
    if ns.same(net::first()) {
        return Err("unshare(CLONE_NEWNET) left the process in the first namespace");
    }
    Ok((one, ns))
}

/// A `sockaddr_in`.
fn addr4(octets: [u8; 4], port: u16) -> Vec<u8> {
    let mut bytes = alloc::vec![0_u8; ferrix_linux_abi::inet::SOCKADDR_STORAGE_SIZE];
    let written = InetAddress::V4 {
        port,
        address: octets,
    }
    .encode(&mut bytes)
    .unwrap_or(0);
    bytes.truncate(written);
    bytes
}

/// A socket of `kind`, made in `ns`.
fn socket_in(ns: &Arc<NetNamespace>, kind: InetKind) -> Result<Arc<InetSocket>, &'static str> {
    let file = InetSocket::open(ns, Family::V4, kind, false, (0, 0))
        .map_err(|_| "a socket could not be opened in a network namespace")?;
    inet_of(&file).ok_or("a socket's open file does not hold a socket")
}

/// The body the data-plane checks send.
const BODY: &[u8] = b"across a namespace, and not beyond it";

/// A datagram from `from` (bound anywhere) to `to_addr` received by `server`,
/// whole and from `source` if that is given.
fn datagram(
    from: &Arc<NetNamespace>,
    server: &Arc<InetSocket>,
    to: &[u8],
    source: Option<[u8; 4]>,
    what: &'static str,
) -> Result<(), &'static str> {
    let client = socket_in(from, InetKind::Datagram)?;
    let _ = client.send(BODY, 0, false, Some(to)).map_err(|_| what)?;
    let mut out = [0_u8; 128];
    let (received, sender) = server.recv(&mut out, 0, true).map_err(|_| what)?;
    if out.get(..received.bytes) != Some(BODY) {
        return Err(what);
    }
    if let Some(octets) = source {
        let sender = sender.ok_or(what)?;
        if sender.get(4..8) != Some(octets.as_slice()) {
            return Err(what);
        }
    }
    Ok(())
}

/// The socket a process holds as descriptor `fd`.
fn socket_at(
    who: &Process,
    fd: usize,
    what: &'static str,
) -> Result<Arc<InetSocket>, &'static str> {
    let file = who
        .files()
        .lock()
        .get(i32::try_from(fd).unwrap_or(-1))
        .map(Arc::clone)
        .map_err(|_| what)?;
    inet_of(&file).ok_or(what)
}

/// A netlink socket in `ns`, spoken through by `who`.
struct Nl {
    socket: Arc<NetlinkSocket>,
    who: Arc<Process>,
    port: u32,
    seq: u32,
}

impl Nl {
    /// Open and bind a socket of `who`'s in `ns`.
    fn open(who: &Arc<Process>, ns: &Arc<NetNamespace>) -> Result<Nl, &'static str> {
        let file = NetlinkSocket::open(ns, SOCK_DGRAM, false, (0, 0))
            .map_err(|_| "a netlink socket could not be opened")?;
        let socket = net::netlink::of(&file).ok_or("a netlink socket is not one")?;
        socket
            .bind(&ferrix_linux_abi::netlink::NetlinkAddress::default().to_bytes())
            .map_err(|_| "a netlink socket could not be bound")?;
        let name = socket.local_name();
        let port = ferrix_linux_abi::netlink::NetlinkAddress::parse(&name, name.len())
            .map_err(|_| "a netlink socket's name was not a sockaddr_nl")?
            .pid;
        Ok(Nl {
            socket,
            who: Arc::clone(who),
            port,
            seq: 0,
        })
    }

    /// Send a request that asks for an acknowledgement and answer the errno in
    /// it: zero for success.
    fn ask(
        &mut self,
        kind: u16,
        flags: u16,
        body: &[u8],
        attributes: &[Attr<'_>],
    ) -> Result<i32, &'static str> {
        self.seq += 1;
        let mut buffer = [0_u8; REQUEST];
        let mut writer = Writer::new(&mut buffer);
        let header = NlMsgHdr {
            len: 0,
            kind,
            flags: NLM_F_REQUEST | NLM_F_ACK | flags,
            seq: self.seq,
            pid: self.port,
        };
        let _ = writer
            .message(header, body, attributes)
            .map_err(|_| "a netlink request did not fit the check's buffer")?;
        let request = writer.written().to_vec();
        let who = Arc::clone(&self.who);
        let _ = userns::acting_as(&who, || self.socket.send(&request, None))?
            .map_err(|_| "a netlink request was refused by the socket")?;
        let mut out = [0_u8; REPLY];
        let (received, _) = self
            .socket
            .recv(&mut out, 0, true)
            .map_err(|_| "a netlink request earned no reply")?;
        let reply = out.get(..received.bytes).unwrap_or_default();
        let message = Messages::new(reply)
            .next()
            .ok_or("a netlink reply was empty")?
            .map_err(|_| "a netlink reply could not be read")?;
        if message.header.kind != NLMSG_ERROR {
            return Err("a netlink request was not answered with an acknowledgement");
        }
        let code = message
            .payload
            .first_chunk::<4>()
            .map(|bytes| i32::from_le_bytes(*bytes))
            .ok_or("a netlink acknowledgement had no error code")?;
        Ok(-code)
    }

    /// Require an acknowledgement of success.
    fn must(
        &mut self,
        kind: u16,
        flags: u16,
        body: &[u8],
        attributes: &[Attr<'_>],
        what: &'static str,
        tally: &mut Tally<'_>,
    ) -> Result<(), &'static str> {
        tally.report.calls += 1;
        match self.ask(kind, flags, body, attributes)? {
            0 => Ok(()),
            _ => Err(what),
        }
    }

    /// Require a refusal with `errno`.
    fn refused(
        &mut self,
        (kind, flags): (u16, u16),
        body: &[u8],
        attributes: &[Attr<'_>],
        errno: Errno,
        what: &'static str,
        tally: &mut Tally<'_>,
    ) -> Result<(), &'static str> {
        tally.report.calls += 1;
        if self.ask(kind, flags, body, attributes)? != i32::from(errno.0) {
            return Err(what);
        }
        tally.report.refusals += 1;
        Ok(())
    }

    /// The names of the links a dump of the namespace lists.
    fn link_names(&mut self) -> Result<Vec<Vec<u8>>, &'static str> {
        self.seq += 1;
        let mut buffer = [0_u8; 64];
        let mut writer = Writer::new(&mut buffer);
        let header = NlMsgHdr {
            len: 0,
            kind: ferrix_linux_abi::netlink::RTM_GETLINK,
            flags: NLM_F_REQUEST | ferrix_linux_abi::netlink::NLM_F_DUMP,
            seq: self.seq,
            pid: self.port,
        };
        let _ = writer
            .message(header, &IfInfoMsg::default().to_bytes(), &[])
            .map_err(|_| "a dump request did not fit")?;
        let request = writer.written().to_vec();
        let who = Arc::clone(&self.who);
        let _ = userns::acting_as(&who, || self.socket.send(&request, None))?
            .map_err(|_| "a dump request was refused")?;
        let mut names = Vec::new();
        let mut out = [0_u8; REPLY];
        while let Ok((received, _)) = self.socket.recv(&mut out, 0, true) {
            let reply = out.get(..received.bytes).unwrap_or_default();
            let Some(Ok(message)) = Messages::new(reply).next() else {
                break;
            };
            if message.header.kind == ferrix_linux_abi::netlink::NLMSG_DONE {
                break;
            }
            if let Some(name) = message.attributes(IfInfoMsg::SIZE).find(IFLA_IFNAME) {
                names.push(name.as_name().to_vec());
            }
        }
        Ok(names)
    }
}

/// An `ifinfomsg` for a link by index, with the flags a request changes.
fn link_body(index: i32, flags: u32, change: u32) -> [u8; IfInfoMsg::SIZE] {
    IfInfoMsg {
        family: 0,
        kind: 0,
        index,
        flags,
        change,
    }
    .to_bytes()
}

/// Bring a link up by netlink.
fn link_up(nl: &mut Nl, index: i32, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    nl.must(
        RTM_NEWLINK,
        0,
        &link_body(index, IFF_UP, IFF_UP),
        &[],
        "a link could not be brought up by its owner",
        tally,
    )
}

/// Give a link an IPv4 address by netlink.
fn address_add(
    nl: &mut Nl,
    index: u32,
    octets: [u8; 4],
    prefix: u8,
    tally: &mut Tally<'_>,
) -> Result<(), &'static str> {
    let body = IfAddrMsg {
        family: AF_INET as u8,
        prefix_len: prefix,
        flags: 0,
        scope: 0,
        index,
    }
    .to_bytes();
    nl.must(
        RTM_NEWADDR,
        NLM_F_CREATE,
        &body,
        &[
            Attr::new(IFA_LOCAL, Value::Address(Address::V4(octets))),
            Attr::new(IFA_ADDRESS, Value::Address(Address::V4(octets))),
        ],
        "an address could not be added by the owner of a namespace",
        tally,
    )
}

/// The index of the interface called `name` in `ns`.
fn index_of(ns: &NetNamespace, name: &[u8]) -> Option<u32> {
    ns.core()
        .look(|stack| stack.interface_by_name(name).map(|each| each.index))
}

/// An attribute with a payload, laid out by hand, for the nested ones the
/// writer has no shape for.
fn nested(kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&u16::try_from(4 + payload.len()).unwrap_or(0).to_le_bytes());
    bytes.extend_from_slice(&kind.to_le_bytes());
    bytes.extend_from_slice(payload);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
    bytes
}

/// A name as an attribute carries it.
fn name_attr(kind: u16, name: &[u8]) -> Vec<u8> {
    let mut text = name.to_vec();
    text.push(0);
    nested(kind, &text)
}

/// A `u32` as an attribute carries it.
fn u32_attr(kind: u16, value: u32) -> Vec<u8> {
    nested(kind, &value.to_le_bytes())
}

/// The payload of `RTM_NEWLINK`'s `IFLA_LINKINFO` for a veth pair whose other
/// end is called `peer` and is placed by `peer_place` (an attribute).
fn veth_info(peer: &[u8], peer_place: &[u8]) -> Vec<u8> {
    let mut block = IfInfoMsg::default().to_bytes().to_vec();
    block.extend_from_slice(&name_attr(IFLA_IFNAME, peer));
    block.extend_from_slice(peer_place);
    let data = nested(VETH_INFO_PEER, &block);
    let mut info = name_attr(IFLA_INFO_KIND, b"veth");
    info.extend_from_slice(&nested(IFLA_INFO_DATA, &data));
    info
}

// ------------------------------------------------------------- the rules

/// NN5 and NN15: who may make a network namespace, and with what.
fn creation(tally: &mut Tally<'_>, made: &mut Vec<Arc<Process>>) -> Result<(), &'static str> {
    let user = unprivileged(made)?;
    tally.refused(
        unshare(&user, CLONE_NEWNET),
        Errno::EPERM,
        "an unprivileged process made a network namespace",
    )?;
    let asked = |who: &Process, flags: u64| family::namespaces_asked(who, flags).map(|()| 0);
    tally.refused(
        asked(&user, CLONE_NEWNET),
        Errno::EPERM,
        "clone with CLONE_NEWNET was not refused EPERM to an unprivileged process",
    )?;
    tally.ok(
        asked(&user, CLONE_NEWNET | CLONE_NEWUSER),
        "clone with CLONE_NEWNET and CLONE_NEWUSER was refused to an unprivileged process",
    )?;
    let root = spawn(made)?;
    tally.ok(
        asked(&root, CLONE_NEWNET),
        "clone with CLONE_NEWNET was refused to root",
    )?;
    tally.refused(
        asked(&root, CLONE_NEWNET | CLONE_THREAD),
        Errno::EINVAL,
        "CLONE_NEWNET with CLONE_THREAD was not refused EINVAL",
    )?;
    // With a user namespace it is allowed, and that namespace owns it.
    tally.ok(
        unshare(&user, CLONE_NEWUSER | CLONE_NEWNET),
        "an unprivileged process was refused a user and a network namespace",
    )?;
    let own = user.net_ns();
    let owner = user.with_credentials(|held| Arc::clone(&held.user_ns));
    if own.same(net::first()) || !own.owner().same(&owner) || owner.is_first() {
        return Err("a network namespace made with a user namespace was not owned by it");
    }
    Ok(())
}

/// NN1 and NN2: a new namespace has a down loopback and nothing else, and
/// `ip link set lo up` gives it its addresses, by netlink and by `ioctl`.
fn loopback(tally: &mut Tally<'_>, made: &mut Vec<Arc<Process>>) -> Result<(), &'static str> {
    let (root, ns) = in_own_namespace(tally, made)?;
    let (links, addresses, routes) = ns.core().look(|stack| {
        let lo = stack.interface(1);
        (
            stack.interfaces().len(),
            lo.map_or(usize::MAX, |each| each.addresses.len()),
            stack.routes().entries().len(),
        )
    });
    let lo_up = ns
        .core()
        .look(|stack| stack.interface(1).is_some_and(ferrix_net::Interface::is_up));
    if links != 1 || lo_up {
        return Err("a new network namespace did not start with a loopback that is down");
    }
    if addresses != 0 || routes != 0 {
        return Err("a new network namespace's loopback owned an address or a route");
    }
    let probe = socket_in(&ns, InetKind::Datagram)?;
    tally.refused(
        probe.bind(&addr4([127, 0, 0, 1], 7_000)).map(|()| 0),
        Errno::EADDRNOTAVAIL,
        "127.0.0.1 could be bound in a namespace whose loopback is down",
    )?;
    tally.refused(
        probe
            .connect(&addr4([127, 0, 0, 1], 7_000), false)
            .map(|()| 0),
        Errno::ENETUNREACH,
        "127.0.0.1 could be reached in a namespace whose loopback is down",
    )?;

    // NN2: up by netlink.
    let mut nl = Nl::open(&root, &ns)?;
    link_up(&mut nl, 1, tally)?;
    let owned = ns.core().look(|stack| {
        stack
            .interface(1)
            .is_some_and(|lo| lo.is_up() && lo.addresses.len() == 2)
    });
    if !owned {
        return Err("bringing a namespace's loopback up did not give it 127.0.0.1 and ::1");
    }
    let server = socket_in(&ns, InetKind::Datagram)?;
    server
        .bind(&addr4([127, 0, 0, 1], 7_000))
        .map_err(|_| "127.0.0.1 could not be bound once the loopback was up")?;
    datagram(
        &ns,
        &server,
        &addr4([127, 0, 0, 1], 7_000),
        None,
        "a datagram did not cross a namespace's own loopback",
    )?;
    stream_over(&ns, &ns, [127, 0, 0, 1], 7_001)?;
    // Down takes the addresses again.
    nl.must(
        RTM_NEWLINK,
        0,
        &link_body(1, 0, IFF_UP),
        &[],
        "a loopback could not be taken down by its owner",
        tally,
    )?;
    let gone = ns
        .core()
        .look(|stack| stack.interface(1).is_some_and(|lo| lo.addresses.is_empty()));
    if !gone {
        return Err("taking a namespace's loopback down left its addresses");
    }

    // The same by `ioctl(SIOCSIFFLAGS)` from a process, in a fresh namespace.
    let (other, fresh) = in_own_namespace(tally, made)?;
    let mut page = page_for(&other)?;
    set_flags_by_ioctl(&mut page, b"lo", IFF_UP as u16, tally)?;
    if !fresh.core().look(|stack| {
        stack
            .interface(1)
            .is_some_and(|lo| lo.is_up() && !lo.addresses.is_empty())
    }) {
        return Err("SIOCSIFFLAGS did not bring a namespace's loopback up with its addresses");
    }
    let flags = get_flags_by_ioctl(&mut page, b"lo")?;
    if flags & IFF_UP as u16 == 0 {
        return Err("SIOCGIFFLAGS did not report the loopback up");
    }
    Ok(())
}

/// A socket of the caller's for `ioctl`, and the `ifreq` for `name`.
fn ifreq_call(
    page: &mut Page<'_>,
    request: u32,
    name: &[u8],
    flags: u16,
) -> Result<Result<Vec<u8>, Errno>, &'static str> {
    page.reset();
    let fd = call(
        page.process,
        Syscall::Socket,
        [u64::from(AF_INET), u64::from(SOCK_DGRAM), 0, 0, 0, 0],
    )
    .map_err(|_| "a socket for an ioctl could not be made")?;
    let mut request_bytes = alloc::vec![0_u8; ferrix_linux_abi::socket::IFREQ_BYTES];
    for (slot, byte) in request_bytes.iter_mut().zip(name.iter().take(IFNAMSIZ - 1)) {
        *slot = *byte;
    }
    if let Some(field) = request_bytes.get_mut(IFNAMSIZ..IFNAMSIZ + 2) {
        field.copy_from_slice(&flags.to_ne_bytes());
    }
    let at = page.put_bytes(&request_bytes)?;
    let answer = call(
        page.process,
        Syscall::Ioctl,
        [fd as u64, u64::from(request), at, 0, 0, 0],
    );
    let mut back = alloc::vec![0_u8; request_bytes.len()];
    let _ = crate::syscall::uaccess::copy_from_user(page.process.space(), at, &mut back);
    close(page.process, fd);
    Ok(answer.map(|_| back))
}

/// `SIOCSIFFLAGS`, required to succeed.
fn set_flags_by_ioctl(
    page: &mut Page<'_>,
    name: &[u8],
    flags: u16,
    tally: &mut Tally<'_>,
) -> Result<(), &'static str> {
    let done = ifreq_call(page, SIOCSIFFLAGS, name, flags)?;
    tally.ok(
        done.map(|_| 0),
        "the owner of a namespace was refused SIOCSIFFLAGS",
    )
}

/// `SIOCGIFFLAGS`.
fn get_flags_by_ioctl(page: &mut Page<'_>, name: &[u8]) -> Result<u16, &'static str> {
    let bytes = ifreq_call(page, SIOCGIFFLAGS, name, 0)?.map_err(|_| "SIOCGIFFLAGS was refused")?;
    bytes
        .get(IFNAMSIZ..IFNAMSIZ + 2)
        .and_then(|field| field.first_chunk::<2>())
        .map(|field| u16::from_ne_bytes(*field))
        .ok_or("SIOCGIFFLAGS wrote nothing")
}

/// A TCP connection from a socket of `from` to a listener of `to` on
/// `address`, carrying bytes both ways.
fn stream_over(
    from: &Arc<NetNamespace>,
    to: &Arc<NetNamespace>,
    address: [u8; 4],
    port: u16,
) -> Result<(), &'static str> {
    let listener = socket_in(to, InetKind::Stream)?;
    listener
        .bind(&addr4(address, port))
        .map_err(|_| "a stream listener could not bind")?;
    listener
        .listen(2)
        .map_err(|_| "a stream listener could not listen")?;
    let client = socket_in(from, InetKind::Stream)?;
    client
        .connect(&addr4(address, port), false)
        .map_err(|_| "a stream connection across namespaces was refused")?;
    let (accepted, _) = listener
        .accept(false, false, (0, 0))
        .map_err(|_| "a stream connection that was made was not accepted")?;
    let server = inet_of(&accepted).ok_or("an accepted connection is not a socket")?;
    for (source, sink) in [(&client, &server), (&server, &client)] {
        let mut written = 0;
        while written < BODY.len() {
            let rest = BODY.get(written..).unwrap_or_default();
            written += source
                .send(rest, 0, false, None)
                .map_err(|_| "a stream across namespaces would not take bytes")?;
        }
        let mut got = Vec::new();
        let mut chunk = [0_u8; 64];
        while got.len() < BODY.len() {
            let (received, _) = sink
                .recv(&mut chunk, 0, false)
                .map_err(|_| "a stream across namespaces lost bytes")?;
            if received.bytes == 0 {
                return Err("a stream across namespaces ended early");
            }
            got.extend(chunk.iter().take(received.bytes).copied());
        }
        if got.as_slice() != BODY {
            return Err("a stream across namespaces garbled its bytes");
        }
    }
    Ok(())
}

/// NN3 and NN4: two namespaces share no port and no socket, and a socket
/// stays in the namespace it was made in when its maker moves.
fn isolation(tally: &mut Tally<'_>, made: &mut Vec<Arc<Process>>) -> Result<(), &'static str> {
    let (one, first) = in_own_namespace(tally, made)?;
    let (two, second) = in_own_namespace(tally, made)?;
    for (who, ns) in [(&one, &first), (&two, &second)] {
        link_up(&mut Nl::open(who, ns)?, 1, tally)?;
    }
    let held_first = socket_in(&first, InetKind::Datagram)?;
    held_first
        .bind(&addr4([127, 0, 0, 1], 7_100))
        .map_err(|_| "a port could not be bound in a namespace")?;
    let held_second = socket_in(&second, InetKind::Datagram)?;
    held_second
        .bind(&addr4([127, 0, 0, 1], 7_100))
        .map_err(|_| "the same port could not be bound in two namespaces")?;
    // A listener in one is not reachable from the other over 127.0.0.1.
    let listener = socket_in(&first, InetKind::Stream)?;
    listener
        .bind(&addr4([127, 0, 0, 1], 7_101))
        .map_err(|_| "a listener could not bind")?;
    listener
        .listen(2)
        .map_err(|_| "a listener could not listen")?;
    let stranger = socket_in(&second, InetKind::Stream)?;
    tally.refused(
        stranger
            .connect(&addr4([127, 0, 0, 1], 7_101), false)
            .map(|()| 0),
        Errno::ECONNREFUSED,
        "a namespace reached a listener of another over its own loopback",
    )?;
    // A datagram sent in the second to the first's port arrives in the second.
    datagram(
        &second,
        &held_second,
        &addr4([127, 0, 0, 1], 7_100),
        None,
        "a datagram did not reach the socket of its own namespace",
    )?;
    let mut out = [0_u8; 8];
    if held_first.recv(&mut out, 0, true).is_ok() {
        return Err("a datagram crossed from one namespace to another over the loopback");
    }
    // The netlink view and the tables are each namespace's own.
    let mut dumper = Nl::open(&one, &first)?;
    let names = dumper.link_names()?;
    if names.as_slice() != [b"lo".to_vec()] {
        return Err("a netlink dump in a new namespace showed another namespace's links");
    }

    // NN4: a socket made before the maker moves stays where it was made.
    // A socket made in one namespace by a process that then moves.
    let traveller = spawn(made)?;
    tally.ok(
        unshare(&traveller, CLONE_NEWNET),
        "no namespace to start in",
    )?;
    let start = traveller.net_ns();
    link_up(&mut Nl::open(&traveller, &start)?, 1, tally)?;
    let before = call(
        &traveller,
        Syscall::Socket,
        [u64::from(AF_INET), u64::from(SOCK_DGRAM), 0, 0, 0, 0],
    )
    .map_err(|_| "a socket could not be made before moving")?;
    let kept = socket_at(&traveller, before, "a socket made before moving")?;
    let sink = socket_in(&start, InetKind::Datagram)?;
    sink.bind(&addr4([127, 0, 0, 1], 7_102))
        .map_err(|_| "the old namespace's sink could not bind")?;
    tally.ok(
        unshare(&traveller, CLONE_NEWNET),
        "a process could not move to another namespace",
    )?;
    let after = call(
        &traveller,
        Syscall::Socket,
        [u64::from(AF_INET), u64::from(SOCK_DGRAM), 0, 0, 0, 0],
    )
    .map_err(|_| "a socket could not be made after moving")?;
    let moved = socket_at(&traveller, after, "a socket made after moving")?;
    let _ = kept
        .send(BODY, 0, false, Some(&addr4([127, 0, 0, 1], 7_102)))
        .map_err(|_| "a socket made before its maker moved no longer reached its namespace")?;
    if sink.recv(&mut out, 0, true).is_err() {
        return Err("a socket made before its maker moved did not stay in its namespace");
    }
    tally.refused(
        moved
            .send(BODY, 0, false, Some(&addr4([127, 0, 0, 1], 7_102)))
            .map(|_| 0),
        Errno::ENETUNREACH,
        "a socket made after its maker moved was in the old namespace",
    )?;
    Ok(())
}

/// NN6 and NN7: `CAP_NET_ADMIN` and `CAP_NET_RAW` over the user namespace
/// that owns the network namespace.
fn privileges(tally: &mut Tally<'_>, made: &mut Vec<Arc<Process>>) -> Result<(), &'static str> {
    let first = Arc::clone(net::first());
    // An unprivileged process, in the first user namespace, changes nothing in
    // the first network namespace.
    let user = unprivileged(made)?;
    let mut nl = Nl::open(&user, &first)?;
    nl.refused(
        (RTM_NEWLINK, 0),
        &link_body(1, IFF_UP, IFF_UP),
        &[],
        Errno::EPERM,
        "an unprivileged process changed the first network namespace",
        tally,
    )?;
    let mut page = page_for(&user)?;
    tally.refused(
        ifreq_call(&mut page, SIOCSIFFLAGS, b"lo", IFF_UP as u16)?.map(|_| 0),
        Errno::EPERM,
        "an unprivileged process set flags by ioctl in the first network namespace",
    )?;
    // Fake root: a user namespace's creator holds every capability there and
    // none over a network namespace the first user namespace owns.
    let fake = unprivileged(made)?;
    let held = Nl::open(&fake, &first)?;
    tally.ok(
        unshare(&fake, CLONE_NEWUSER | CLONE_NEWNET),
        "an unprivileged process was refused a user and a network namespace",
    )?;
    let mut elsewhere = held;
    elsewhere.refused(
        (RTM_NEWLINK, 0),
        &link_body(1, IFF_UP, IFF_UP),
        &[],
        Errno::EPERM,
        "fake root in a user namespace changed the first network namespace",
        tally,
    )?;
    // And the same process configures the namespace its user namespace owns.
    let own = fake.net_ns();
    let mut inside = Nl::open(&fake, &own)?;
    link_up(&mut inside, 1, tally)?;
    let mut page = page_for(&fake)?;
    set_flags_by_ioctl(&mut page, b"lo", IFF_UP as u16, tally)?;

    // NN7: raw sockets.
    tally.refused(
        call(
            &user,
            Syscall::Socket,
            [u64::from(AF_INET), u64::from(SOCK_RAW), 1, 0, 0, 0],
        ),
        Errno::EPERM,
        "an unprivileged process opened a raw socket",
    )?;
    tally.ok(
        call(
            &fake,
            Syscall::Socket,
            [u64::from(AF_INET), u64::from(SOCK_RAW), 1, 0, 0, 0],
        ),
        "the owner of a network namespace could not open a raw socket there",
    )
}

/// One side of the pairs the check makes: a root process, the namespace it is
/// in, and a netlink socket it speaks to that namespace through.
struct Side {
    root: Arc<Process>,
    ns: Arc<NetNamespace>,
    nl: Nl,
}

impl Side {
    /// A root process in a namespace of its own.
    fn new(tally: &mut Tally<'_>, made: &mut Vec<Arc<Process>>) -> Result<Side, &'static str> {
        let (root, ns) = in_own_namespace(tally, made)?;
        let nl = Nl::open(&root, &ns)?;
        Ok(Side { root, ns, nl })
    }

    /// Make a veth pair by netlink, this end named `near` here and the other
    /// `far` in the namespace of the process `far_pid`, and answer their
    /// indexes if the request is accepted.
    fn make_pair(
        &mut self,
        (near, far): (&[u8], &[u8]),
        far_pid: u32,
        tally: &mut Tally<'_>,
        what: &'static str,
    ) -> Result<(), &'static str> {
        let info = veth_info(far, &u32_attr(IFLA_NET_NS_PID, far_pid));
        self.nl.must(
            RTM_NEWLINK,
            NLM_F_CREATE,
            &link_body(0, 0, 0),
            &[
                Attr::new(IFLA_IFNAME, Value::Name(near)),
                Attr::new(IFLA_LINKINFO, Value::Bytes(&info)),
            ],
            what,
            tally,
        )
    }

    /// The index of the interface called `name` here.
    fn index(&self, name: &[u8], what: &'static str) -> Result<u32, &'static str> {
        index_of(&self.ns, name).ok_or(what)
    }

    /// Give it an address and bring it up.
    fn configure(
        &mut self,
        index: u32,
        octets: [u8; 4],
        tally: &mut Tally<'_>,
    ) -> Result<(), &'static str> {
        address_add(&mut self.nl, index, octets, 24, tally)?;
        link_up(&mut self.nl, index as i32, tally)
    }

    /// Whether the interface `index` has `flag` set.
    fn has(&self, index: u32, flag: u32) -> bool {
        self.ns
            .core()
            .look(|s| s.interface(index).is_some_and(|e| e.flags & flag != 0))
    }
}

/// NN8, NN9 and NN16: a veth pair joins two namespaces and only them, and an
/// end moves only with authority over both owners.
fn pairs(tally: &mut Tally<'_>, made: &mut Vec<Arc<Process>>) -> Result<(), &'static str> {
    let mut a = Side::new(tally, made)?;
    let mut b = Side::new(tally, made)?;
    let mut c = Side::new(tally, made)?;
    let (ia, ib) = first_pair(&mut a, &mut b, tally)?;
    second_pair(&mut a, &mut c, &b, tally)?;
    authority(&a, tally, made)?;
    moved_and_deleted(&mut a, &mut b, &c, (ia, ib), tally)
}

/// NN8: a pair made by netlink, not running until both ends are up, carries a
/// datagram and a stream.
fn first_pair(
    a: &mut Side,
    b: &mut Side,
    tally: &mut Tally<'_>,
) -> Result<(u32, u32), &'static str> {
    let before = veth::count();
    a.make_pair(
        (b"veth-a", b"veth-b"),
        b.root.pid(),
        tally,
        "a veth pair could not be made by netlink",
    )?;
    if veth::count() != before + 1 {
        return Err("making a veth pair did not add one to the table");
    }
    let ia = a.index(
        b"veth-a",
        "the first end of a veth pair is not in its namespace",
    )?;
    let ib = b.index(b"veth-b", "the peer of a veth pair is not in its namespace")?;
    a.configure(ia, [10, 9, 0, 1], tally)?;
    if a.has(ia, ferrix_net::iface::IFF_RUNNING) {
        return Err("a veth end ran before its peer was up");
    }
    b.configure(ib, [10, 9, 0, 2], tally)?;
    if !a.has(ia, ferrix_net::iface::IFF_LOWER_UP) {
        return Err("a veth end did not gain carrier when its peer came up");
    }
    let server = socket_in(&b.ns, InetKind::Datagram)?;
    server
        .bind(&addr4([10, 9, 0, 2], 5_000))
        .map_err(|_| "the far end of a veth pair would not take its address")?;
    datagram(
        &a.ns,
        &server,
        &addr4([10, 9, 0, 2], 5_000),
        Some([10, 9, 0, 1]),
        "a datagram did not cross a veth pair",
    )?;
    stream_over(&a.ns, &b.ns, [10, 9, 0, 2], 5_001)?;
    Ok((ia, ib))
}

/// What a namespace has taken in over all its interfaces and what its stack
/// has delivered: unchanged while traffic it is no party to goes by.
fn watched_frames(side: &Side) -> (u64, u64, u64) {
    side.ns.core().look(|stack| {
        let counters = stack.counters();
        let received = stack
            .interfaces()
            .iter()
            .map(|each| each.counters.received)
            .sum();
        (received, counters.delivered, counters.not_ours)
    })
}

/// NN16: a second pair, between `a` and `c`, carries its own traffic; what
/// crosses one never arrives at the other's peer, which is in `watched`.
fn second_pair(
    a: &mut Side,
    c: &mut Side,
    watched: &Side,
    tally: &mut Tally<'_>,
) -> Result<(), &'static str> {
    a.make_pair(
        (b"veth-c", b"veth-d"),
        c.root.pid(),
        tally,
        "a second veth pair could not be made",
    )?;
    let ic = a.index(b"veth-c", "the second pair's end is not in its namespace")?;
    let id = c.index(b"veth-d", "the second pair's peer is not in its namespace")?;
    a.configure(ic, [10, 8, 0, 1], tally)?;
    c.configure(id, [10, 8, 0, 2], tally)?;
    let before = watched_frames(watched);
    let far = socket_in(&c.ns, InetKind::Datagram)?;
    far.bind(&addr4([10, 8, 0, 2], 5_000))
        .map_err(|_| "the second pair's far end would not take its address")?;
    let client = socket_in(&a.ns, InetKind::Datagram)?;
    let _ = client
        .send(BODY, 0, false, Some(&addr4([10, 8, 0, 2], 5_000)))
        .map_err(|_| "a datagram to the second pair was refused")?;
    let after = watched_frames(watched);
    if after != before {
        return Err("a frame sent on one veth pair arrived at the peer of another");
    }
    let mut out = [0_u8; 128];
    let (got, _) = far
        .recv(&mut out, 0, true)
        .map_err(|_| "a datagram did not cross the second veth pair")?;
    if out.get(..got.bytes) != Some(BODY) {
        return Err("a datagram crossed the second veth pair as something else");
    }
    Ok(())
}

/// NN9: authority over both owners. Fake root owns a namespace of its own and
/// makes a pair there; pushing an end into a namespace the first user
/// namespace owns, or making the peer there, is refused.
fn authority(
    outside: &Side,
    tally: &mut Tally<'_>,
    made: &mut Vec<Arc<Process>>,
) -> Result<(), &'static str> {
    let fake = unprivileged(made)?;
    tally.ok(
        unshare(&fake, CLONE_NEWUSER | CLONE_NEWNET),
        "an unprivileged process was refused a user and a network namespace",
    )?;
    let own = fake.net_ns();
    let mut inside = Side {
        nl: Nl::open(&fake, &own)?,
        root: Arc::clone(&fake),
        ns: own,
    };
    inside.make_pair(
        (b"mine-a", b"mine-b"),
        fake.pid(),
        tally,
        "the owner of a namespace could not make a veth pair in it",
    )?;
    let mine = inside.index(b"mine-b", "the owner's veth end is not in its namespace")?;
    inside.nl.refused(
        (RTM_SETLINK, 0),
        &link_body(mine as i32, 0, 0),
        &[Attr::new(IFLA_NET_NS_PID, Value::U32(outside.root.pid()))],
        Errno::EPERM,
        "an unprivileged user namespace pushed a veth end into the first user namespace's",
        tally,
    )?;
    let info = veth_info(b"leak-b", &u32_attr(IFLA_NET_NS_PID, outside.root.pid()));
    inside.nl.refused(
        (RTM_NEWLINK, NLM_F_CREATE),
        &link_body(0, 0, 0),
        &[
            Attr::new(IFLA_IFNAME, Value::Name(b"leak-a")),
            Attr::new(IFLA_LINKINFO, Value::Bytes(&info)),
        ],
        Errno::EPERM,
        "an unprivileged user namespace made the peer of a veth pair in another's namespace",
        tally,
    )?;
    if index_of(&inside.ns, b"leak-a").is_some() || index_of(&outside.ns, b"leak-b").is_some() {
        return Err("a refused veth pair left an end behind");
    }
    Ok(())
}

/// A move by root, by the descriptor of the destination's namespace: the end
/// comes down, loses its address, is found in its new namespace and its peer
/// follows it; then deleting either end deletes both.
fn moved_and_deleted(
    a: &mut Side,
    b: &mut Side,
    c: &Side,
    (ia, ib): (u32, u32),
    tally: &mut Tally<'_>,
) -> Result<(), &'static str> {
    let mut page = page_for(&b.root)?;
    let fd = open_ns_file(&b.root, &mut page, c.root.pid())?;
    let before = c.ns.core().look(|stack| stack.interfaces().len());
    b.nl.must(
        RTM_SETLINK,
        0,
        &link_body(ib as i32, 0, 0),
        &[Attr::new(IFLA_NET_NS_FD, Value::U32(fd as u32))],
        "root could not move a veth end by the descriptor of a namespace",
        tally,
    )?;
    close(&b.root, fd);
    if index_of(&b.ns, b"veth-b").is_some()
        || c.ns.core().look(|stack| stack.interfaces().len()) != before + 1
    {
        return Err("a moved veth end was not in its new namespace only");
    }
    let landed = c.index(b"veth-b", "a moved veth end lost its name")?;
    if c.ns.core().look(|stack| {
        stack
            .interface(landed)
            .is_none_or(|end| end.is_up() || !end.addresses.is_empty())
    }) {
        return Err("a moved veth end kept its address or stayed up");
    }
    let backing =
        a.ns.core()
            .look(|stack| stack.interface(ia).map(|end| end.backing));
    let followed = backing
        .and_then(veth::end_of)
        .and_then(|(pair, end)| veth::peer(pair, end))
        .is_some_and(|(peer, index)| peer.same(&c.ns) && index == landed);
    if !followed {
        return Err("the peer of a moved veth end did not follow it");
    }
    a.nl.must(
        RTM_DELLINK,
        0,
        &link_body(ia as i32, 0, 0),
        &[],
        "a veth end could not be deleted",
        tally,
    )?;
    if index_of(&a.ns, b"veth-a").is_some() || index_of(&c.ns, b"veth-b").is_some() {
        return Err("deleting one end of a veth pair left the other");
    }
    Ok(())
}

/// `open("/proc/<pid>/ns/net")`, as the page's process.
fn open_ns_file(who: &Arc<Process>, page: &mut Page<'_>, pid: u32) -> Result<usize, &'static str> {
    page.reset();
    let path = format!("/proc/{pid}/ns/net");
    let at = page.put(path.as_bytes())?;
    userns::acting_as(who, || {
        call(
            page.process,
            Syscall::Openat,
            [
                ferrix_linux_abi::types::AT_FDCWD as u64,
                at,
                u64::from(ferrix_linux_abi::types::O_RDONLY),
                0,
                0,
                0,
            ],
        )
    })?
    .map_err(|_| "/proc/<pid>/ns/net would not open as a file")
}

/// A device the way a ring adds it to the first namespace: backed by a key,
/// served by a port. Answers the key, its index and the port.
fn add_device(
    first: &Arc<NetNamespace>,
) -> Result<(u32, u32, Arc<crate::object::port::Port>), &'static str> {
    let key = device::new_key();
    let mut nic = ferrix_net::Interface::ethernet(0, b"eth-check", [0x02, 0, 0, 0, 0x77, 1], 1500);
    nic.backing = ferrix_net::iface::Backing::Device(key);
    nic.flags |= ferrix_net::iface::IFF_UP | ferrix_net::iface::IFF_LOWER_UP;
    let home = first.core().add_interface(nic);
    device::place(key, first, home);
    let port = crate::object::port::Port::new().map_err(|_| "no port for a device's bell")?;
    device::wake_on_transmit(key, &port);
    Ok((key, home, port))
}

/// An ARP reply from 192.168.77.9 to the check's device, as its ring would
/// receive it.
fn arp_reply() -> Vec<u8> {
    let mut reply = alloc::vec![0_u8; 42];
    let parts: [(core::ops::Range<usize>, &[u8]); 3] = [
        (0..6, &[0x02, 0, 0, 0, 0x77, 1]),
        (6..12, &[0x02, 0, 0, 0, 0x77, 9]),
        (12..14, &[0x08, 0x06]),
    ];
    for (range, bytes) in parts {
        if let Some(field) = reply.get_mut(range) {
            field.copy_from_slice(bytes);
        }
    }
    // htype 1, ptype 0x0800, hlen 6, plen 4, op 2 (reply), sha, spa, tha, tpa.
    let body: [u8; 28] = [
        0, 1, 8, 0, 6, 4, 0, 2, 0x02, 0, 0, 0, 0x77, 9, 192, 168, 77, 9, 0x02, 0, 0, 0, 0x77, 1,
        192, 168, 77, 2,
    ];
    if let Some(field) = reply.get_mut(14..42) {
        field.copy_from_slice(&body);
    }
    reply
}

/// NN10: a device is in the first namespace until a privileged caller moves
/// it, is served from the namespace it went to, and arrives down and bare.
/// Answers the namespace it went to, the netlink socket to speak to it with,
/// and the device's key.
fn device_moves(
    tally: &mut Tally<'_>,
    made: &mut Vec<Arc<Process>>,
    holder: &Arc<Process>,
) -> Result<(u32, Arc<NetNamespace>, Nl), &'static str> {
    let first = Arc::clone(net::first());
    let root = spawn(made)?;
    let (key, home, _port) = add_device(&first)?;
    let away = namespace::create(Arc::clone(first.owner()))
        .map_err(|_| "no namespace for a device to go to")?;
    if device::find(key).is_none_or(|(ns, index)| !ns.same(&first) || index != home) {
        return Err("a device was not in the first namespace to begin with");
    }
    holder.set_net_ns(Arc::clone(&away));
    let mut nl = Nl::open(&root, &first)?;
    nl.must(
        RTM_SETLINK,
        0,
        &link_body(home as i32, 0, 0),
        &[Attr::new(IFLA_NET_NS_PID, Value::U32(holder.pid()))],
        "root could not move a device to another namespace",
        tally,
    )?;
    let Some((now_in, index)) = device::find(key) else {
        return Err("a moved device is nowhere");
    };
    if !now_in.same(&away) || index_of(&first, b"eth-check").is_some() {
        return Err("the registry still named the old namespace after a device moved");
    }
    if away.core().look(|stack| {
        stack
            .interface(index)
            .is_none_or(ferrix_net::Interface::is_up)
    }) {
        return Err("a device came up in the namespace it moved to");
    }
    // Served where it went: its frames are taken from that namespace's queue,
    // and what its ring receives goes into that namespace's stack.
    let mut inside = Nl::open(holder, &away)?;
    address_add(&mut inside, index, [192, 168, 77, 2], 24, tally)?;
    link_up(&mut inside, index as i32, tally)?;
    let probe = socket_in(&away, InetKind::Datagram)?;
    let _ = probe.send(BODY, 0, false, Some(&addr4([192, 168, 77, 9], 9)));
    let asked = device::take_outgoing(key, 4)
        .iter()
        .any(|frame| frame.get(12..14) == Some(&[0x08, 0x06]));
    if !asked {
        return Err("a device that moved was not served its frames from where it went");
    }
    device::receive(key, &arp_reply());
    let learned = away.core().look(|stack| {
        stack
            .neighbors()
            .entries()
            .iter()
            .any(|entry| entry.mac.is_some())
    });
    if !learned {
        return Err("a frame the ring of a moved device received did not reach its namespace");
    }
    Ok((key, away, inside))
}

/// NN10 and NN13: a physical device moves with its ring, and comes home when
/// the namespace it went to ends; the ends of a pair vanish with theirs.
fn devices(tally: &mut Tally<'_>, made: &mut Vec<Arc<Process>>) -> Result<(), &'static str> {
    let first = Arc::clone(net::first());
    let holder = spawn(made)?;
    let (key, away, inside) = device_moves(tally, made, &holder)?;

    // The namespace ends with its last holder: the device is home, flushed
    // and down, the registry says so, and a pair with an end there is gone.
    let other = namespace::create(Arc::clone(first.owner()))
        .map_err(|_| "no namespace for a veth end to outlive")?;
    let (_, survivor_index) = veth::create((&away, Some(b"doomed")), (&other, Some(b"survivor")))
        .map_err(|_| "a pair could not be made for the end of a namespace")?;
    let pairs_before = veth::count();
    drop(inside);
    holder.set_net_ns(Arc::clone(&first));
    drop(away);
    let Some((back, where_now)) = device::find(key) else {
        return Err("a device was nowhere after the namespace it was in ended");
    };
    if !back.same(&first) {
        return Err("a physical device did not come home to the first namespace");
    }
    if first.core().look(|stack| {
        stack
            .interface(where_now)
            .is_none_or(|nic| nic.is_up() || !nic.addresses.is_empty())
    }) {
        return Err("a device came home up or with an address");
    }
    if veth::count() + 1 != pairs_before {
        return Err("a veth pair outlived the namespace one of its ends was in");
    }
    if other
        .core()
        .look(|stack| stack.interface(survivor_index).is_some())
    {
        return Err("the peer of a veth end outlived it");
    }
    device::remove(key);
    if index_of(&first, b"eth-check").is_some() {
        return Err("a check's device was left in the first namespace");
    }
    Ok(())
}

/// NN11: abstract `AF_UNIX` names are per network namespace.
fn abstract_names(tally: &mut Tally<'_>, made: &mut Vec<Arc<Process>>) -> Result<(), &'static str> {
    let (one, _) = in_own_namespace(tally, made)?;
    let (two, _) = in_own_namespace(tally, made)?;
    let same = spawn(made)?;
    let mut name = alloc::vec![0_u8; 2];
    name.extend_from_slice(b"\0netns-check");
    if let Some(family) = name.get_mut(..2) {
        family.copy_from_slice(&AF_UNIX.to_ne_bytes());
    }
    let listen_on = |who: &Arc<Process>| -> Result<Result<usize, Errno>, &'static str> {
        let mut page = page_for(who)?;
        let fd = call(
            who,
            Syscall::Socket,
            [u64::from(AF_UNIX), u64::from(SOCK_STREAM), 0, 0, 0, 0],
        )
        .map_err(|_| "a unix socket could not be made")?;
        let at = page.put_bytes(&name)?;
        let bound = call(
            who,
            Syscall::Bind,
            [fd as u64, at, name.len() as u64, 0, 0, 0],
        );
        if bound.is_ok() {
            let _ = call(who, Syscall::Listen, [fd as u64, 1, 0, 0, 0, 0]);
        }
        Ok(bound)
    };
    tally.ok(
        listen_on(&one)?,
        "an abstract name could not be bound in a network namespace",
    )?;
    tally.ok(
        listen_on(&two)?,
        "the same abstract name could not be bound in another network namespace",
    )?;
    // A third process in the first namespace sees neither.
    let mut page = page_for(&same)?;
    let fd = call(
        &same,
        Syscall::Socket,
        [u64::from(AF_UNIX), u64::from(SOCK_STREAM), 0, 0, 0, 0],
    )
    .map_err(|_| "a unix socket could not be made")?;
    let at = page.put_bytes(&name)?;
    tally.refused(
        call(
            &same,
            Syscall::Connect,
            [fd as u64, at, name.len() as u64, 0, 0, 0],
        ),
        Errno::ECONNREFUSED,
        "a process reached an abstract name bound in another network namespace",
    )?;
    // And one in the same namespace as the first does.
    let joiner = spawn(made)?;
    joiner.set_net_ns(one.net_ns());
    let mut page = page_for(&joiner)?;
    let fd = call(
        &joiner,
        Syscall::Socket,
        [u64::from(AF_UNIX), u64::from(SOCK_STREAM), 0, 0, 0, 0],
    )
    .map_err(|_| "a unix socket could not be made")?;
    let at = page.put_bytes(&name)?;
    tally.ok(
        call(
            &joiner,
            Syscall::Connect,
            [fd as u64, at, name.len() as u64, 0, 0, 0],
        ),
        "a process could not reach an abstract name in its own network namespace",
    )
}

/// NN12: `/proc/<pid>/ns/net` names the namespace, and `/proc/net` shows the
/// reader's.
fn proc_views(tally: &mut Tally<'_>, made: &mut Vec<Arc<Process>>) -> Result<(), &'static str> {
    let (one, ns) = in_own_namespace(tally, made)?;
    let outside = spawn(made)?;
    let mut page = page_for(&one)?;
    let own = read_link(&mut page, format!("/proc/{}/ns/net", one.pid()).as_bytes())?;
    let mut page_out = page_for(&outside)?;
    let first = read_link(
        &mut page_out,
        format!("/proc/{}/ns/net", outside.pid()).as_bytes(),
    )?;
    if !own.starts_with(b"net:[") || own == first {
        return Err("/proc/<pid>/ns/net did not name a namespace apart from the first");
    }
    if own != format!("net:[{}]", ns.id()).into_bytes() {
        return Err("/proc/<pid>/ns/net did not read the namespace's own number");
    }
    // Following the link opens the namespace as a file.
    let fd = open_ns_file(&outside, &mut page_out, one.pid())?;
    let opened = outside
        .files()
        .lock()
        .get(i32::try_from(fd).unwrap_or(-1))
        .ok()
        .and_then(|file| net::netns_file::of(file));
    close(&outside, fd);
    if !opened.is_some_and(|found| found.same(&ns)) {
        return Err("/proc/<pid>/ns/net did not open as the namespace it names");
    }
    // /proc/net is the reader's: a new namespace lists its loopback alone.
    let devices = userns::acting_as(&one, || read_file(&mut page, b"/proc/net/dev"))??
        .map_err(|_| "/proc/net/dev could not be read in a network namespace")?;
    let rows = devices
        .split(|&byte| byte == b'\n')
        .filter(|line| line.contains(&b':'))
        .count();
    if rows != 1 || !devices.windows(3).any(|window| window == b"lo:") {
        return Err("/proc/net/dev showed another namespace's interfaces");
    }
    let routes = userns::acting_as(&one, || read_file(&mut page, b"/proc/net/route"))??
        .map_err(|_| "/proc/net/route could not be read in a network namespace")?;
    if routes
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .count()
        > 1
    {
        return Err("/proc/net/route showed another namespace's routes");
    }
    tally.report.calls += 3;
    Ok(())
}

/// NN17: a namespace's tables stop at their ceilings.
fn ceilings(tally: &mut Tally<'_>, made: &mut Vec<Arc<Process>>) -> Result<(), &'static str> {
    let (root, ns) = in_own_namespace(tally, made)?;
    let mut nl = Nl::open(&root, &ns)?;
    link_up(&mut nl, 1, tally)?;
    // Addresses: up to the ceiling fit and the next does not.
    let have = ns.core().look(ferrix_net::Stack::address_count);
    for n in 0..namespace::MAX_ADDRESSES.saturating_sub(have) {
        let octets = [10, 100, (n / 250) as u8, (n % 250) as u8 + 1];
        address_add(&mut nl, 1, octets, 32, tally)?;
    }
    nl.refused(
        (RTM_NEWADDR, NLM_F_CREATE),
        &IfAddrMsg {
            family: AF_INET as u8,
            prefix_len: 32,
            flags: 0,
            scope: 0,
            index: 1,
        }
        .to_bytes(),
        &[Attr::new(
            IFA_LOCAL,
            Value::Address(Address::V4([10, 101, 0, 1])),
        )],
        Errno::ENOSPC,
        "a namespace took more addresses than its ceiling",
        tally,
    )?;
    // Routes: up to the ceiling, then refused.
    let held = ns.core().look(|stack| stack.routes().entries().len());
    for n in held..namespace::MAX_ROUTES {
        route_add(&mut nl, n, tally)?;
    }
    nl.refused(
        (RTM_NEWROUTE, NLM_F_CREATE),
        &route_body(),
        &[
            Attr::new(RTA_DST, Value::Address(Address::V4([172, 31, 255, 254]))),
            Attr::new(RTA_OIF, Value::U32(1)),
        ],
        Errno::ENOSPC,
        "a namespace took more routes than its ceiling",
        tally,
    )?;
    // Interfaces: 63 more than the loopback fit, as pairs, and then a pair
    // that would pass the ceiling is refused with nothing of it left.
    let other = namespace::create(Arc::clone(ns.owner()))
        .map_err(|_| "no namespace to fill with interfaces")?;
    let mut pairs_made = 0;
    let refused = loop {
        match veth::create((&ns, None), (&other, None)) {
            Ok(_) => pairs_made += 1,
            Err(errno) => break errno,
        }
        if pairs_made > namespace::MAX_INTERFACES {
            return Err("a namespace took more interfaces than its ceiling");
        }
    };
    tally.report.calls += 1;
    if refused != Errno::ENOSPC {
        return Err("a namespace at its interface ceiling did not answer ENOSPC");
    }
    tally.report.refusals += 1;
    let (here, there) = (
        ns.core().look(|stack| stack.interfaces().len()),
        other.core().look(|stack| stack.interfaces().len()),
    );
    if here > namespace::MAX_INTERFACES || here != there {
        return Err("a refused veth pair left an end behind");
    }
    Ok(())
}

/// `rtmsg` for a unicast static route to a /32 in the main table.
fn route_body() -> [u8; RtMsg::SIZE] {
    RtMsg {
        family: AF_INET as u8,
        dst_len: 32,
        src_len: 0,
        tos: 0,
        table: RT_TABLE_MAIN,
        protocol: RTPROT_STATIC,
        scope: RT_SCOPE_UNIVERSE,
        kind: RTN_UNICAST,
        flags: 0,
    }
    .to_bytes()
}

/// Add the `n`th distinct /32 route through the loopback.
fn route_add(nl: &mut Nl, n: usize, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let octets = [
        172,
        16 + (n / 65_000) as u8,
        (n / 250 % 250) as u8,
        (n % 250) as u8 + 1,
    ];
    nl.must(
        RTM_NEWROUTE,
        NLM_F_CREATE,
        &route_body(),
        &[
            Attr::new(RTA_DST, Value::Address(Address::V4(octets))),
            Attr::new(RTA_OIF, Value::U32(1)),
        ],
        "a route could not be added by the owner of a namespace",
        tally,
    )
}

/// Add the `n`th distinct /32 route to `ns` by a netlink socket opened, used
/// and closed here, for `kmem_check`'s fill: the request is made by the
/// kernel, which is no process and may, and the answer is the acknowledgement's
/// errno.
pub(crate) fn route_for_fill(ns: &Arc<NetNamespace>, n: usize) -> Result<(), Errno> {
    let file = NetlinkSocket::open(ns, SOCK_DGRAM, false, (0, 0))?;
    let socket = net::netlink::of(&file).ok_or(Errno::EINVAL)?;
    let octets = [
        172,
        16 + (n / 65_000) as u8,
        (n / 250 % 250) as u8,
        (n % 250) as u8 + 1,
    ];
    let mut buffer = [0_u8; REQUEST];
    let mut writer = Writer::new(&mut buffer);
    let header = NlMsgHdr {
        len: 0,
        kind: RTM_NEWROUTE,
        flags: NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE,
        seq: 1,
        pid: 0,
    };
    let _ = writer
        .message(
            header,
            &route_body(),
            &[
                Attr::new(RTA_DST, Value::Address(Address::V4(octets))),
                Attr::new(RTA_OIF, Value::U32(1)),
            ],
        )
        .map_err(|_| Errno::EINVAL)?;
    let request = writer.written().to_vec();
    let _ = socket.send(&request, None)?;
    let mut out = [0_u8; REPLY];
    let (received, _) = socket.recv(&mut out, 0, true)?;
    let reply = out.get(..received.bytes).unwrap_or_default();
    let code = Messages::new(reply)
        .next()
        .and_then(Result::ok)
        .and_then(|message| {
            message
                .payload
                .first_chunk::<4>()
                .map(|bytes| i32::from_le_bytes(*bytes))
        })
        .ok_or(Errno::EINVAL)?;
    match code {
        0 => Ok(()),
        negative => Err(Errno(u16::try_from(-negative).unwrap_or(u16::MAX))),
    }
}
