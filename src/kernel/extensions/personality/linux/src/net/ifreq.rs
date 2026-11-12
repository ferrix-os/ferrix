//! The `ifreq` ioctls: the interface interface every program already knows.
//!
//! rtnetlink is how an interface is configured today, and `src/kernel/src/net/
//! netlink` is where that lives. These calls are the older way, and they have
//! not gone anywhere: `if_nametoindex`, which POSIX.1-2024 specifies and which
//! every program that names an interface goes through, is `SIOCGIFINDEX`;
//! `getifaddrs` falls back to `SIOCGIFCONF`; `ifconfig` knows nothing else at
//! all; and busybox's `ip link set eth0 up`, which otherwise speaks netlink,
//! reads and writes the flags this way. A stack that answers only netlink is a
//! stack where `ip` cannot find a device that is right there.
//!
//! # The structure
//!
//! Every call but one takes a pointer to a `struct ifreq`: sixteen bytes of
//! name, NUL-padded, and then a union. Nothing here reads past the first
//! sixteen bytes of that union, so the structure's total size -- which differs
//! between a 32-bit and a 64-bit target -- matters only to `SIOCGIFCONF`,
//! which walks an array of them.
//!
//! The name identifies the interface for every call except `SIOCGIFNAME`,
//! which goes the other way: an index in the union, and the name written back.
//!
//! # What is refused
//!
//! An interface that is not there is `ENODEV`, as Linux answers. A getter for
//! an address the interface has not got is `EADDRNOTAVAIL`. A setter from a
//! process that is not privileged is `EPERM`, checked before anything is read,
//! so a refused call cannot have changed anything. A request this module does
//! not know is `ENOTTY`, which is what lets the socket's own ioctls be tried
//! first.

use alloc::vec;
use alloc::vec::Vec;

use ferrix_linux_abi::inet::InetAddress;
use ferrix_linux_abi::netlink::{ARPHRD_ETHER, ARPHRD_LOOPBACK, IFF_ALLMULTI, IFF_PROMISC};
use ferrix_linux_abi::socket::{
    AF_INET, IFNAMSIZ, IFREQ_BYTES, IFREQ_UNION, SIOCDIFADDR, SIOCGIFADDR, SIOCGIFBRDADDR,
    SIOCGIFCONF, SIOCGIFCOUNT, SIOCGIFDSTADDR, SIOCGIFFLAGS, SIOCGIFHWADDR, SIOCGIFINDEX,
    SIOCGIFMETRIC, SIOCGIFMTU, SIOCGIFNAME, SIOCGIFNETMASK, SIOCGIFTXQLEN, SIOCSIFADDR,
    SIOCSIFBRDADDR, SIOCSIFDSTADDR, SIOCSIFFLAGS, SIOCSIFMETRIC, SIOCSIFMTU, SIOCSIFNETMASK,
    SIOCSIFTXQLEN,
};
use ferrix_net::addr::{IpAddress, IpCidr, Ipv4};
use ferrix_net::iface::{Address, IFF_MULTICAST, IFF_NOARP, IFF_POINTOPOINT, IFF_UP, Medium};
use ferrix_vfs::Errno;

use crate::net::NetNamespace;
use crate::net::namespace;
use crate::net::netlink::link;
use crate::syscall::process::Process;
use crate::syscall::uaccess;

/// The smallest MTU an interface may be given, from Linux's `dev_set_mtu`.
const MIN_MTU: u32 = 68;

/// The largest, which is `IP_MAX_MTU`.
const MAX_MTU: u32 = 0xFFFF - 20;

/// What `SIOCGIFTXQLEN` reports. Nothing queues on the way out here -- the
/// ring's producer is the queue -- so the number is Linux's default rather
/// than a measurement, and a program reading it learns only that there is
/// one.
const TXQLEN: u32 = 1000;

/// The flags a program may set, from Linux's `IFF_VOLATILE` complement: every
/// other bit is the driver's to report and is kept whatever a program writes.
const SETTABLE: u32 = IFF_UP | IFF_NOARP | IFF_PROMISC | IFF_ALLMULTI | IFF_MULTICAST;

/// Answer one interface ioctl.
///
/// # Errors
///
/// `ENOTTY` for a request this module does not know, so that the caller can
/// go on to its own; and otherwise what the call refuses with.
pub(crate) fn ioctl(process: &Process, request: u32, arg: u64) -> Result<usize, Errno> {
    // The interfaces a program sees are its network namespace's.
    let namespace = process.net_ns();
    let ns = &namespace;
    match request {
        SIOCGIFCONF => return config(ns, process, arg),
        SIOCGIFCOUNT => {
            let count = ns.core().with(|stack, _| stack.interfaces().len());
            uaccess::put_u32(
                process.space(),
                arg,
                u32::try_from(count).unwrap_or(u32::MAX),
            )?;
            return Ok(0);
        }
        SIOCGIFNAME => return name_of_index(ns, process, arg),
        _ => {}
    }
    // Changing one takes `CAP_NET_ADMIN` over the owner of the namespace.
    if is_setter(request) && !link::net_admin(Some(process), ns) {
        return Err(Errno::EPERM);
    }
    let name = read_name(process, arg)?;
    let index = ns
        .core()
        .with(|stack, _| stack.interface_by_name(&name).map(|found| found.index))
        .ok_or(Errno::ENODEV)?;
    match request {
        SIOCGIFINDEX => uaccess::put_u32(process.space(), union_at(arg), index).map(|()| 0),
        SIOCGIFFLAGS => {
            let flags = with_interface(ns, index, |interface| interface.flags)?;
            // `ifr_flags` is a `short`: sixteen bits, and the rest of the
            // union is the program's to have left as it was.
            put_u16(process, union_at(arg), flags as u16).map(|()| 0)
        }
        SIOCSIFFLAGS => set_flags(ns, process, index, arg),
        SIOCGIFMTU => {
            let mtu = with_interface(ns, index, |interface| interface.mtu)?;
            uaccess::put_u32(process.space(), union_at(arg), mtu).map(|()| 0)
        }
        SIOCSIFMTU => set_mtu(ns, process, index, arg),
        SIOCGIFTXQLEN => uaccess::put_u32(process.space(), union_at(arg), TXQLEN).map(|()| 0),
        // Linux takes a queue length and does nothing a stack with no queue
        // could show; refusing would fail `ifconfig eth0 txqueuelen 1000`.
        SIOCSIFTXQLEN => Ok(0),
        // Always zero, as Linux's `dev_ifsioc` answers, and always refused to
        // set, as it refuses.
        SIOCGIFMETRIC => uaccess::put_u32(process.space(), union_at(arg), 0).map(|()| 0),
        SIOCSIFMETRIC => Err(Errno::EOPNOTSUPP),
        SIOCGIFHWADDR => hardware(ns, process, index, arg),
        SIOCGIFADDR | SIOCGIFDSTADDR | SIOCGIFBRDADDR | SIOCGIFNETMASK => {
            get_address(ns, process, index, request, arg)
        }
        SIOCSIFADDR | SIOCSIFDSTADDR | SIOCSIFBRDADDR | SIOCSIFNETMASK => {
            set_address(ns, process, index, request, arg)
        }
        SIOCDIFADDR => delete_address(ns, process, index, arg),
        _ => Err(Errno::ENOTTY),
    }
}

/// Whether a request changes something, and so needs privilege.
fn is_setter(request: u32) -> bool {
    matches!(
        request,
        SIOCSIFFLAGS
            | SIOCSIFMTU
            | SIOCSIFTXQLEN
            | SIOCSIFMETRIC
            | SIOCSIFADDR
            | SIOCSIFDSTADDR
            | SIOCSIFBRDADDR
            | SIOCSIFNETMASK
            | SIOCDIFADDR
    )
}

/// Where the union after the name begins.
fn union_at(arg: u64) -> u64 {
    arg.wrapping_add(IFREQ_UNION as u64)
}

/// The name in an `ifreq`, cut at its first NUL.
fn read_name(process: &Process, arg: u64) -> Result<Vec<u8>, Errno> {
    let mut bytes = vec![0_u8; IFNAMSIZ];
    uaccess::copy_from_user(process.space(), arg, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    bytes.truncate(end);
    if bytes.is_empty() {
        return Err(Errno::ENODEV);
    }
    Ok(bytes)
}

/// Write a name into an `ifreq`, NUL-padded to `IFNAMSIZ`.
fn write_name(process: &Process, arg: u64, name: &[u8]) -> Result<(), Errno> {
    let mut bytes = [0_u8; IFNAMSIZ];
    // One short of the field, so the name is always terminated.
    let room = IFNAMSIZ - 1;
    for (slot, byte) in bytes.iter_mut().zip(name.iter().take(room)) {
        *slot = *byte;
    }
    uaccess::copy_to_user(process.space(), arg, &bytes).map_err(|_| Errno::EFAULT)
}

/// Write a `short`, which is what `ifr_flags` is.
fn put_u16(process: &Process, at: u64, value: u16) -> Result<(), Errno> {
    uaccess::copy_to_user(process.space(), at, &value.to_ne_bytes()).map_err(|_| Errno::EFAULT)
}

/// Read one.
fn get_u16(process: &Process, at: u64) -> Result<u16, Errno> {
    let mut bytes = [0_u8; 2];
    uaccess::copy_from_user(process.space(), at, &mut bytes).map_err(|_| Errno::EFAULT)?;
    Ok(u16::from_ne_bytes(bytes))
}

/// Read something out of an interface, or `ENODEV` if it went away between
/// the lookup and here.
fn with_interface<T>(
    ns: &NetNamespace,
    index: u32,
    read: impl FnOnce(&ferrix_net::iface::Interface) -> T,
) -> Result<T, Errno> {
    ns.core()
        .with(|stack, _| stack.interface(index).map(read))
        .ok_or(Errno::ENODEV)
}

/// `SIOCGIFNAME`: the index is in the union and the name comes back.
fn name_of_index(ns: &NetNamespace, process: &Process, arg: u64) -> Result<usize, Errno> {
    let index = uaccess::get_u32(process.space(), union_at(arg))?;
    let name = ns
        .core()
        .with(|stack, _| stack.interface(index).map(|found| found.name))
        .ok_or(Errno::ENODEV)?;
    write_name(process, arg, name.as_bytes())?;
    Ok(0)
}

/// `SIOCSIFFLAGS`.
fn set_flags(ns: &NetNamespace, process: &Process, index: u32, arg: u64) -> Result<usize, Errno> {
    let wanted = u32::from(get_u16(process, union_at(arg))?);
    let up = wanted & IFF_UP != 0;
    let changed: Result<(), Errno> = ns.core().with(|stack, _| {
        stack.set_up(index, up).map_err(|_| Errno::ENODEV)?;
        let interface = stack.interface_mut(index).ok_or(Errno::ENODEV)?;
        // Everything else the program may set, and nothing it may not:
        // the driver's own bits -- `IFF_BROADCAST`, `IFF_LOOPBACK`,
        // `IFF_RUNNING`, `IFF_LOWER_UP` -- are facts, not requests.
        let settable = SETTABLE & !IFF_UP;
        interface.flags = (interface.flags & !settable) | (wanted & settable);
        Ok(())
    });
    // A virtual pair runs when both its ends are up.
    ns.refresh_carriers();
    let _ = ns.fit(0);
    changed.map(|()| 0)
}

/// `SIOCSIFMTU`.
fn set_mtu(ns: &NetNamespace, process: &Process, index: u32, arg: u64) -> Result<usize, Errno> {
    let mtu = uaccess::get_u32(process.space(), union_at(arg))?;
    if !(MIN_MTU..=MAX_MTU).contains(&mtu) {
        return Err(Errno::EINVAL);
    }
    ns.core().with(|stack, _| {
        let interface = stack.interface_mut(index).ok_or(Errno::ENODEV)?;
        interface.mtu = mtu;
        Ok(0)
    })
}

/// `SIOCGIFHWADDR`: a `sockaddr` whose family is the `ARPHRD_` kind and whose
/// data is the address.
fn hardware(ns: &NetNamespace, process: &Process, index: u32, arg: u64) -> Result<usize, Errno> {
    let (kind, mac) = with_interface(ns, index, |interface| {
        let kind = match interface.medium {
            Medium::Ethernet => ARPHRD_ETHER,
            Medium::Loopback => ARPHRD_LOOPBACK,
        };
        (kind, interface.hardware)
    })?;
    let mut bytes = [0_u8; 16];
    let family = kind.to_ne_bytes();
    for (slot, byte) in bytes.iter_mut().zip(family.iter().chain(mac.iter())) {
        *slot = *byte;
    }
    uaccess::copy_to_user(process.space(), union_at(arg), &bytes).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// The IPv4 address an interface is configured with, for the getters.
fn first_v4(ns: &NetNamespace, index: u32) -> Result<Address, Errno> {
    let found = with_interface(ns, index, |interface| {
        interface
            .addresses
            .iter()
            .find(|address| address.cidr.address().is_v4())
            .copied()
    })?;
    found.ok_or(Errno::EADDRNOTAVAIL)
}

/// The mask a prefix length stands for.
fn mask_of(prefix_len: u8) -> Ipv4 {
    let host_bits = 32_u32.saturating_sub(u32::from(prefix_len));
    let bits = if host_bits >= 32 {
        0
    } else {
        u32::MAX << host_bits
    };
    Ipv4::from_bits(bits)
}

/// How many leading bits of a mask are set, stopping at the first zero: an
/// `ifconfig` that passes a mask with a hole in it gets the prefix before it.
fn prefix_of(mask: Ipv4) -> u8 {
    let bits = mask.to_bits();
    let ones = bits.leading_ones();
    u8::try_from(ones).unwrap_or(32)
}

/// `SIOCGIFADDR`, `SIOCGIFDSTADDR`, `SIOCGIFBRDADDR` and `SIOCGIFNETMASK`.
fn get_address(
    ns: &NetNamespace,
    process: &Process,
    index: u32,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    let address = first_v4(ns, index)?;
    let IpAddress::V4(four) = address.cidr.address() else {
        return Err(Errno::EADDRNOTAVAIL);
    };
    let answer = match request {
        SIOCGIFADDR => four,
        SIOCGIFNETMASK => mask_of(address.cidr.prefix_len()),
        SIOCGIFBRDADDR => address.cidr.broadcast().ok_or(Errno::EADDRNOTAVAIL)?,
        // A link with no peer answers with its own address, which is what
        // Linux does for an interface that is not point-to-point.
        SIOCGIFDSTADDR => match address.peer {
            Some(IpAddress::V4(peer)) => peer,
            _ => four,
        },
        _ => return Err(Errno::ENOTTY),
    };
    write_v4(process, union_at(arg), answer)?;
    Ok(0)
}

/// Write a `sockaddr_in` with no port into an `ifreq`'s union.
fn write_v4(process: &Process, at: u64, address: Ipv4) -> Result<(), Errno> {
    let mut bytes = [0_u8; 16];
    let written = InetAddress::V4 {
        port: 0,
        address: address.octets(),
    }
    .encode(&mut bytes)
    .ok_or(Errno::EINVAL)?;
    uaccess::copy_to_user(
        process.space(),
        at,
        bytes.get(..written).unwrap_or_default(),
    )
    .map_err(|_| Errno::EFAULT)
}

/// Read one, refusing anything that is not `AF_INET`.
fn read_v4(process: &Process, at: u64) -> Result<Ipv4, Errno> {
    let mut bytes = [0_u8; 16];
    uaccess::copy_from_user(process.space(), at, &mut bytes).map_err(|_| Errno::EFAULT)?;
    let family = u16::from_ne_bytes([*bytes.first().unwrap_or(&0), *bytes.get(1).unwrap_or(&0)]);
    if family != AF_INET {
        return Err(Errno::EINVAL);
    }
    match InetAddress::parse(&bytes, bytes.len()) {
        Ok(InetAddress::V4 { address, .. }) => Ok(Ipv4::new(address)),
        _ => Err(Errno::EINVAL),
    }
}

/// `SIOCSIFADDR`, `SIOCSIFDSTADDR`, `SIOCSIFBRDADDR` and `SIOCSIFNETMASK`.
///
/// Linux keeps one primary address per interface behind these calls, and each
/// of them rewrites that one: setting the address keeps the mask the
/// interface had, and setting the mask keeps the address. That is what
/// `ifconfig eth0 10.0.2.15 netmask 255.255.255.0` means, two calls in a row.
fn set_address(
    ns: &NetNamespace,
    process: &Process,
    index: u32,
    request: u32,
    arg: u64,
) -> Result<usize, Errno> {
    let given = read_v4(process, union_at(arg))?;
    let held = first_v4(ns, index).ok();
    let (address, prefix_len, peer) = match request {
        SIOCSIFADDR => {
            let prefix = held.map_or_else(|| classful(given), |had| had.cidr.prefix_len());
            (given, prefix, held.and_then(|had| had.peer))
        }
        SIOCSIFNETMASK => {
            let had = held.ok_or(Errno::EADDRNOTAVAIL)?;
            let IpAddress::V4(four) = had.cidr.address() else {
                return Err(Errno::EADDRNOTAVAIL);
            };
            (four, prefix_of(given), had.peer)
        }
        SIOCSIFDSTADDR => {
            let had = held.ok_or(Errno::EADDRNOTAVAIL)?;
            let IpAddress::V4(four) = had.cidr.address() else {
                return Err(Errno::EADDRNOTAVAIL);
            };
            (four, had.cidr.prefix_len(), Some(IpAddress::V4(given)))
        }
        // A broadcast address is the prefix's every host bit set, and nothing
        // here stores one that is not. Linux takes the call and a program
        // that reads it back gets the prefix's, so taking it and keeping the
        // prefix is the same answer with less to go wrong.
        SIOCSIFBRDADDR => return Ok(0),
        _ => return Err(Errno::ENOTTY),
    };
    ns.admit()?;
    let done = ns.core().with(|stack, _| {
        if held.is_none() && stack.address_count() >= namespace::MAX_ADDRESSES {
            return Err(Errno::ENOSPC);
        }
        if let Some(had) = held {
            let _ = stack.remove_address(index, had.cidr.address());
        }
        stack
            .add_address(
                index,
                Address {
                    cidr: IpCidr::new(IpAddress::V4(address), prefix_len),
                    peer,
                },
            )
            .map_err(|_| Errno::ENODEV)?;
        if request == SIOCSIFDSTADDR
            && let Some(interface) = stack.interface_mut(index)
        {
            interface.flags |= IFF_POINTOPOINT;
        }
        Ok(())
    });
    let _ = ns.fit(0);
    done?;
    Ok(0)
}

/// `SIOCDIFADDR`.
fn delete_address(
    ns: &NetNamespace,
    process: &Process,
    index: u32,
    arg: u64,
) -> Result<usize, Errno> {
    let address = read_v4(process, union_at(arg))?;
    ns.core().with(|stack, _| {
        stack
            .remove_address(index, IpAddress::V4(address))
            .map_err(|_| Errno::EADDRNOTAVAIL)
    })?;
    Ok(0)
}

/// The prefix length an address gets when it is given one without a mask:
/// the classful one, which is what Linux's `inet_insert_ifa` falls back to.
fn classful(address: Ipv4) -> u8 {
    match address.octets().first().copied().unwrap_or(0) {
        0..=127 => 8,
        128..=191 => 16,
        _ => 24,
    }
}

/// `SIOCGIFCONF`: every interface with an IPv4 address, as `ifreq`s.
///
/// The argument is a `struct ifconf`: a length and a pointer, which on a
/// 64-bit target are four bytes, four of padding, and eight, and on a 32-bit
/// one four and four. A length with a null pointer, or a length of zero, asks
/// how many bytes the answer needs; anything else is filled to the length and
/// the length is written back with what was used.
fn config(ns: &NetNamespace, process: &Process, arg: u64) -> Result<usize, Errno> {
    let word = size_of::<usize>() as u64;
    let length = uaccess::get_u32(process.space(), arg)? as usize;
    let buffer = uaccess::get_word(process.space(), arg.wrapping_add(word))?;

    let entries: Vec<(ferrix_net::iface::Name, Ipv4)> = ns.core().with(|stack, _| {
        stack
            .interfaces()
            .iter()
            .filter_map(|interface| {
                let address = interface.addresses.iter().find_map(|address| {
                    match address.cidr.address() {
                        IpAddress::V4(four) => Some(four),
                        IpAddress::V6(_) => None,
                    }
                })?;
                Some((interface.name, address))
            })
            .collect()
    });

    let needed = entries.len().saturating_mul(IFREQ_BYTES);
    if buffer == 0 {
        uaccess::put_u32(
            process.space(),
            arg,
            u32::try_from(needed).unwrap_or(u32::MAX),
        )?;
        return Ok(0);
    }
    let room = length / IFREQ_BYTES;
    let mut written = 0_usize;
    for (name, address) in entries.iter().take(room) {
        let at = buffer.wrapping_add(written as u64);
        write_name(process, at, name.as_bytes())?;
        write_v4(process, at.wrapping_add(IFREQ_UNION as u64), *address)?;
        written += IFREQ_BYTES;
    }
    uaccess::put_u32(
        process.space(),
        arg,
        u32::try_from(written).unwrap_or(u32::MAX),
    )?;
    Ok(0)
}
