//! `dhcpc`: Ferrix's DHCPv4 client, in place of busybox's `udhcpc`.
//!
//! ```text
//! dhcpc -i IFACE [-n] [-q] [-t TRIES] [-T SECONDS] [-A SECONDS] [-f]
//! ```
//!
//! It asks for a lease on `IFACE` -- DISCOVER, OFFER, REQUEST, ACK (RFC
//! 2131) -- over an `AF_PACKET` socket, since the interface has no address
//! to send from yet, and applies the lease itself over rtnetlink: the
//! interface up, its address and prefix, the default route through the first
//! router, and `/etc/resolv.conf`. There is no lease script, so nothing it
//! does needs a shell, `ip` or `sed`. Each lease, renewal and loss is one
//! line on standard output:
//!
//! ```text
//! eth0: 10.0.2.15/24 by DHCP, router 10.0.2.2, DNS 10.0.2.3, lease 86400 s
//! ```
//!
//! and progress and refusals go to standard error, `dhcpc: ` first.
//!
//! The command line is `udhcpc`'s, as far as Ferrix's callers use it: `-i`
//! the interface, `-t` how many DISCOVERs (and REQUESTs) to send, `-T` how
//! long to wait for each answer, `-n` to exit with status 1 rather than try
//! again after `-A` seconds when none came, and `-q` to exit with status 0
//! once the lease is applied. Without `-q` the client stays: it renews with
//! the server that gave the lease at T1, asks any server at T2, and takes the
//! address away again if the lease runs out (RFC 2131 §4.4.5). `-f`, stay in
//! the foreground, is accepted and is what it always does.
//!
//! # Where things are
//!
//! Everything that decides is here and runs on the host: [`message`], the
//! DHCP message both ways; [`frame`], the IPv4 and UDP around it, by the
//! kernel's own `ferrix-netwire`; [`client`], the state machine, driven by a
//! clock the caller passes in, so a test walks a day-long lease in
//! microseconds; [`rtnl`], the rtnetlink requests and the replies' walk,
//! by the kernel's own `ferrix-netlink`; and [`args`], the command line.
//! `src/main.rs` and its `sys` module are the sockets and the loop.

#![forbid(unsafe_code)]

pub mod args;
pub mod client;
pub mod frame;
pub mod message;
pub mod rtnl;

#[cfg(test)]
mod tests;

use std::net::Ipv4Addr;

/// A lease as the client holds it: what was given, and for how long.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Lease {
    /// The address.
    pub address: Ipv4Addr,
    /// Its prefix length, from the subnet mask; 24 when no mask was sent, as
    /// `udhcpc`'s lease script assumed.
    pub prefix: u8,
    /// The server that gave it, which a renewal is sent to.
    pub server: Ipv4Addr,
    /// The routers, first preferred.
    pub routers: Vec<Ipv4Addr>,
    /// The name servers, in order.
    pub dns: Vec<Ipv4Addr>,
    /// The domain name, for `search` in `/etc/resolv.conf`.
    pub domain: Option<String>,
    /// How long it lasts, in seconds; `u32::MAX` is for ever.
    pub seconds: u32,
    /// When to renew, in seconds after it was given.
    pub renew: u32,
    /// When to ask any server, in seconds after it was given.
    pub rebind: u32,
}

impl Lease {
    /// The line said when the lease is applied, without the interface.
    #[must_use]
    pub fn describe(&self) -> String {
        let list = |addresses: &[Ipv4Addr]| {
            if addresses.is_empty() {
                "none".to_owned()
            } else {
                addresses
                    .iter()
                    .map(Ipv4Addr::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        };
        let lasts = if self.seconds == u32::MAX {
            "for ever".to_owned()
        } else {
            format!("{} s", self.seconds)
        };
        format!(
            "{}/{} by DHCP, router {}, DNS {}, lease {lasts}",
            self.address,
            self.prefix,
            list(self.routers.get(..1).unwrap_or_default()),
            list(&self.dns),
        )
    }

    /// `/etc/resolv.conf` for this lease, or `None` when it named no server
    /// and the file is better left as it is.
    #[must_use]
    pub fn resolv_conf(&self) -> Option<String> {
        if self.dns.is_empty() {
            return None;
        }
        let mut text = String::from("# Written by dhcpc from its lease.\n");
        if let Some(domain) = &self.domain {
            text.push_str(&format!("search {domain}\n"));
        }
        for server in &self.dns {
            text.push_str(&format!("nameserver {server}\n"));
        }
        Some(text)
    }

    /// Whether `other` puts anything different on the interface or in the
    /// routing table than this does: a renewal that changes nothing there is
    /// not applied again.
    #[must_use]
    pub fn same_configuration(&self, other: &Lease) -> bool {
        self.address == other.address
            && self.prefix == other.prefix
            && self.routers.first() == other.routers.first()
    }
}

/// The prefix length a subnet mask spells, or `None` for a mask whose ones
/// are not contiguous.
#[must_use]
pub fn prefix_of(mask: Ipv4Addr) -> Option<u8> {
    let bits = u32::from(mask);
    let ones = bits.leading_ones();
    if bits.checked_shl(ones).unwrap_or(0) != 0 {
        return None;
    }
    u8::try_from(ones).ok()
}

/// The broadcast address of `address`'s subnet of `prefix` bits.
#[must_use]
pub fn broadcast_of(address: Ipv4Addr, prefix: u8) -> Ipv4Addr {
    let host = u32::MAX.checked_shr(u32::from(prefix)).unwrap_or(0);
    Ipv4Addr::from(u32::from(address) | host)
}
