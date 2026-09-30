//! The link requests that cannot be answered inside one stack's lock
//! (`docs/NETNS.md` sections 3.3 and 4): making a virtual pair, deleting one,
//! and moving an interface to another network namespace.
//!
//! They touch two namespaces, or allocate, or both, and the route code's rule
//! is that nothing it does may: [`super::route::answer`] hands these here
//! before it takes any lock. Everything a request asks is checked first --
//! which namespaces it names, and the caller's capability over the owner of
//! each -- and nothing changes if any part is refused.

use alloc::sync::Arc;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::netlink::{
    IFLA_IFNAME, IFLA_INFO_DATA, IFLA_INFO_KIND, IFLA_LINKINFO, IFLA_NET_NS_FD, IFLA_NET_NS_PID,
    IfInfoMsg, NLM_F_ACK, NLM_F_CREATE, RTM_DELLINK, RTM_NEWLINK, RTM_SETLINK, VETH_INFO_PEER,
};
use ferrix_netlink::{Attributes, Message, Writer};

use crate::net::{NetNamespace, namespace, veth};
use crate::syscall::process::Process;
use crate::syscall::userns::{self, CAP_NET_ADMIN};

/// The one kind of link this kernel makes.
const VETH: &[u8] = b"veth";

/// Whether `message` is one of the requests this module answers.
pub(super) fn wants(message: &Message<'_>) -> bool {
    let kind = message.header.kind;
    if kind == RTM_DELLINK {
        return true;
    }
    if kind != RTM_NEWLINK && kind != RTM_SETLINK {
        return false;
    }
    let attributes = message.attributes(IfInfoMsg::SIZE);
    let creates =
        attributes.find(IFLA_LINKINFO).is_some() && message.header.flags & NLM_F_CREATE != 0;
    creates
        || attributes.find(IFLA_NET_NS_PID).is_some()
        || attributes.find(IFLA_NET_NS_FD).is_some()
}

/// Whether `actor` holds `CAP_NET_ADMIN` over the owner of `namespace`. The
/// kernel's own callers, which are no process, may.
pub(crate) fn net_admin(actor: Option<&Process>, namespace: &NetNamespace) -> bool {
    actor.is_none_or(|process| {
        process
            .with_credentials(|held| userns::capable_over(held, namespace.owner(), CAP_NET_ADMIN))
    })
}

/// Answer one of the requests [`wants`] named.
pub(super) fn answer(
    ns: &Arc<NetNamespace>,
    actor: Option<&Process>,
    port: u32,
    message: &Message<'_>,
    writer: &mut Writer<'_>,
    admin: bool,
) {
    let header = message.header;
    let outcome = if !admin {
        Err(Errno::EPERM)
    } else if header.kind == RTM_DELLINK {
        delete(ns, message)
    } else if message
        .attributes(IfInfoMsg::SIZE)
        .find(IFLA_LINKINFO)
        .is_some()
        && header.flags & NLM_F_CREATE != 0
    {
        create(ns, actor, message)
    } else {
        relocate(ns, actor, message)
    };
    match outcome {
        Ok(()) if header.flags & NLM_F_ACK != 0 => {
            let _ = writer.error(header, 0, port);
        }
        Ok(()) => {}
        Err(errno) => {
            let _ = writer.error(header, i32::from(errno.0), port);
        }
    }
}

/// The namespace a message's `IFLA_NET_NS_PID` or `IFLA_NET_NS_FD` names, if
/// it names one, and whether the caller may change it.
fn target(
    attributes: &Attributes<'_>,
    actor: Option<&Process>,
) -> Result<Option<Arc<NetNamespace>>, Errno> {
    let found = if let Some(pid) = attributes
        .find(IFLA_NET_NS_PID)
        .and_then(|attribute| attribute.as_u32())
    {
        let process = crate::syscall::registry::find(pid).ok_or(Errno::ESRCH)?;
        Some(process.net_ns())
    } else if let Some(descriptor) = attributes
        .find(IFLA_NET_NS_FD)
        .and_then(|attribute| attribute.as_u32())
    {
        let caller = actor.ok_or(Errno::EBADF)?;
        let descriptor = i32::try_from(descriptor).map_err(|_| Errno::EBADF)?;
        let file = crate::syscall::fd::file(caller, descriptor)?;
        Some(crate::net::netns_file::of(&file).ok_or(Errno::EINVAL)?)
    } else {
        None
    };
    if let Some(namespace) = &found
        && !net_admin(actor, namespace)
    {
        return Err(Errno::EPERM);
    }
    Ok(found)
}

/// Which interface a link message names: its index, or its name.
fn named(ns: &NetNamespace, index: i32, attributes: &Attributes<'_>) -> Result<u32, Errno> {
    ns.core().look(|stack| {
        if index > 0 {
            let index = u32::try_from(index).map_err(|_| Errno::EINVAL)?;
            return stack
                .interface(index)
                .map(|each| each.index)
                .ok_or(Errno::ENODEV);
        }
        let name = attributes
            .find(IFLA_IFNAME)
            .map(|found| found.as_name())
            .ok_or(Errno::EINVAL)?;
        stack
            .interface_by_name(name)
            .map(|each| each.index)
            .ok_or(Errno::ENODEV)
    })
}

/// `RTM_NEWLINK` with `IFLA_LINKINFO` and `NLM_F_CREATE`: a `veth` pair.
///
/// The link named by the message's own attributes goes to the namespace its
/// own `IFLA_NET_NS_*` names, or this one; its peer, described by
/// `VETH_INFO_PEER` inside `IFLA_INFO_DATA`, to the namespace that block
/// names, or this one. As Linux does, where both default to the caller's.
fn create(
    ns: &Arc<NetNamespace>,
    actor: Option<&Process>,
    message: &Message<'_>,
) -> Result<(), Errno> {
    let outer = message.attributes(IfInfoMsg::SIZE);
    let info = outer.find(IFLA_LINKINFO).ok_or(Errno::EINVAL)?;
    let info = Attributes::new(info.as_bytes());
    let kind = info.find(IFLA_INFO_KIND).ok_or(Errno::EINVAL)?;
    if kind.as_name() != VETH {
        return Err(Errno::EOPNOTSUPP);
    }
    let one_ns = target(&outer, actor)?.unwrap_or_else(|| Arc::clone(ns));
    let one_name = outer.find(IFLA_IFNAME).map(|found| found.as_name());
    // The peer block is an `ifinfomsg` and then attributes, as a message is.
    // A request that describes no peer gets one in the same namespace under a
    // name of the kernel's choosing, as `ip link add type veth` with no `peer`.
    let block = info
        .find(IFLA_INFO_DATA)
        .and_then(|data| Attributes::new(data.as_bytes()).find(VETH_INFO_PEER))
        .map_or(&[][..], |peer| peer.as_bytes());
    let body_end = ferrix_linux_abi::netlink::nlmsg_align(IfInfoMsg::SIZE);
    if !block.is_empty() && block.len() < IfInfoMsg::SIZE {
        return Err(Errno::EINVAL);
    }
    let peer_attributes = Attributes::new(block.get(body_end..).unwrap_or_default());
    let two_ns = target(&peer_attributes, actor)?.unwrap_or_else(|| Arc::clone(ns));
    let two_name = peer_attributes
        .find(IFLA_IFNAME)
        .map(|found| found.as_name());
    // An interface of the caller's own namespace is changed by the caller's
    // authority over it, which `admin` already established.
    let _ = veth::create((&one_ns, one_name), (&two_ns, two_name))?;
    Ok(())
}

/// `RTM_DELLINK`: a virtual pair goes, both ends. Anything else is not
/// deletable, as Linux's loopback and physical links are not.
fn delete(ns: &Arc<NetNamespace>, message: &Message<'_>) -> Result<(), Errno> {
    let body = message
        .body(IfInfoMsg::SIZE)
        .and_then(IfInfoMsg::from_bytes)
        .ok_or(Errno::EINVAL)?;
    let attributes = message.attributes(IfInfoMsg::SIZE);
    let index = named(ns, body.index, &attributes)?;
    let backing = ns
        .core()
        .look(|stack| stack.interface(index).map(|each| each.backing))
        .ok_or(Errno::ENODEV)?;
    match veth::end_of(backing) {
        Some((pair, _)) => {
            veth::destroy(pair);
            Ok(())
        }
        None => Err(Errno::EOPNOTSUPP),
    }
}

/// `RTM_SETLINK` or `RTM_NEWLINK` with `IFLA_NET_NS_PID` or `IFLA_NET_NS_FD`:
/// move the interface, renaming it there if the message gives a name.
fn relocate(
    ns: &Arc<NetNamespace>,
    actor: Option<&Process>,
    message: &Message<'_>,
) -> Result<(), Errno> {
    let body = message
        .body(IfInfoMsg::SIZE)
        .and_then(IfInfoMsg::from_bytes)
        .ok_or(Errno::EINVAL)?;
    let attributes = message.attributes(IfInfoMsg::SIZE);
    let destination = target(&attributes, actor)?.ok_or(Errno::EINVAL)?;
    let index = named(ns, body.index, &attributes)?;
    let rename = if body.index > 0 {
        attributes.find(IFLA_IFNAME).map(|found| found.as_name())
    } else {
        None
    };
    let _ = namespace::transfer(ns, index, &destination, rename)?;
    Ok(())
}
