//! `dhcpc`: take an IPv4 lease for an interface and apply it (the library's
//! documentation, `src/lib.rs`, says what and how).
//!
//! A command line it does not take is the usage and status 2; a failure is
//! one line, `dhcpc: ` and why, on standard error, and status 1 -- `-n`'s
//! round with no lease among them.

mod sys;

use std::io::Write as _;
use std::net::Ipv4Addr;
use std::process::ExitCode;
use std::time::Instant;

use dhcp::args::{self, Args, USAGE};
use dhcp::client::{Action, Client, Config, Destination};
use dhcp::message::{Reply, kind};
use dhcp::{Lease, frame, rtnl};

use sys::{Netlink, Packet};

/// The link's broadcast address.
const BROADCAST_MAC: [u8; 6] = [0xFF; 6];

/// `EEXIST`, `ESRCH` and `EADDRNOTAVAIL`: what a change that was already
/// made, or a removal of what is not there, is refused with.
const EEXIST: i32 = 17;
const ESRCH: i32 = 3;
const EADDRNOTAVAIL: i32 = 99;

/// Where the resolver's servers go.
const RESOLV_CONF: &str = "/etc/resolv.conf";

fn main() -> ExitCode {
    let args = match args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "dhcpc: {error}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(status) => ExitCode::from(status),
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "dhcpc: {}: {error}", args.interface);
            ExitCode::FAILURE
        }
    }
}

/// A line on standard error, `dhcpc: IFACE: ` first.
fn progress(interface: &str, line: &str) {
    let _ = writeln!(std::io::stderr(), "dhcpc: {interface}: {line}");
}

/// A line on standard output, `IFACE: ` first: what was configured.
fn said(interface: &str, line: &str) {
    let _ = writeln!(std::io::stdout(), "{interface}: {line}");
}

/// What the loop keeps besides the client.
struct Run<'a> {
    args: &'a Args,
    netlink: Netlink,
    index: u32,
    /// The packet socket, open while the client is asking: closed while a
    /// lease is held, since it is handed a copy of every IPv4 frame the
    /// interface takes in.
    packet: Option<Packet>,
    /// The hardware address the last ACK came from, which a renewal is sent
    /// to: the server's, or its relay's.
    server_mac: [u8; 6],
}

fn run(args: &Args) -> Result<u8, String> {
    let mut netlink = Netlink::open().map_err(|error| error.to_string())?;
    let link = find_link(&mut netlink, &args.interface)?;
    let mac = link
        .mac
        .ok_or_else(|| "it has no Ethernet address".to_owned())?;
    refused(
        netlink.call(|seq| rtnl::set_up(seq, link.index), |_, _| {}),
        "bringing it up",
    )?;
    let mut run = Run {
        args,
        netlink,
        index: link.index,
        packet: None,
        server_mac: BROADCAST_MAC,
    };
    // What busybox's lease script did on `deconfig`: whatever IPv4 address
    // the interface had goes, so the lease's is the only one.
    run.deconfigure()?;
    let mut client = Client::new(
        Config {
            mac,
            tries: args.tries,
            timeout: args.timeout,
            pause: args.pause,
            exit_without_lease: args.exit_without_lease,
            quit_with_lease: args.quit_with_lease,
        },
        sys::seed(),
    );
    let start = Instant::now();
    loop {
        let actions = client.tick(start.elapsed());
        if let Some(status) = run.perform(actions, None)? {
            return Ok(status);
        }
        // Open whenever the client is asking; a held lease needs no socket
        // until its timer is due, and the first send opens it again.
        if !client.is_bound() && run.packet.is_none() {
            run.packet = Some(Packet::open(run.index).map_err(|error| error.to_string())?);
        }
        let timeout = client.wake().map(|due| due.saturating_sub(start.elapsed()));
        let Some(packet) = &run.packet else {
            // A lease held: nothing to read until its timer is due.
            match timeout {
                Some(timeout) => std::thread::sleep(timeout),
                None => loop {
                    // A lease for ever, and nothing left to do.
                    std::thread::park();
                },
            }
            continue;
        };
        if !packet.wait(timeout).map_err(|error| error.to_string())? {
            continue;
        }
        let (bytes, from) = packet.receive().map_err(|error| error.to_string())?;
        let Some((_, message)) = frame::unwrap(&bytes) else {
            continue;
        };
        let Ok(reply) = Reply::parse(message) else {
            continue;
        };
        let actions = client.receive(start.elapsed(), &reply);
        if let Some(status) = run.perform(actions, Some(from))? {
            return Ok(status);
        }
    }
}

impl Run<'_> {
    /// Do what the client said; `Some` status to exit with.
    fn perform(
        &mut self,
        actions: Vec<Action>,
        from: Option<[u8; 6]>,
    ) -> Result<Option<u8>, String> {
        let interface = &self.args.interface;
        for action in actions {
            match action {
                Action::Send { kind, message, to } => self.send(kind, &message, &to)?,
                Action::Bound(lease) => {
                    if let Some(from) = from {
                        self.server_mac = from;
                    }
                    self.apply(&lease, None)?;
                    said(interface, &lease.describe());
                    // Held: the packet socket is not needed until T1.
                    self.packet = None;
                }
                Action::Renewed { old, new } => {
                    if let Some(from) = from {
                        self.server_mac = from;
                    }
                    if let Err(error) = self.apply(&new, Some(&old)) {
                        progress(
                            interface,
                            &format!("the renewed lease was not applied: {error}"),
                        );
                    }
                    said(
                        interface,
                        &format!(
                            "{}/{} renewed, lease {} s",
                            new.address, new.prefix, new.seconds
                        ),
                    );
                    self.packet = None;
                }
                Action::Lost(lease) => {
                    self.remove(&lease);
                    said(
                        interface,
                        &format!("{}/{} lost; asking again", lease.address, lease.prefix),
                    );
                }
                Action::NoLease => progress(
                    interface,
                    if self.args.exit_without_lease {
                        "no lease"
                    } else {
                        "no lease; asking again after a pause"
                    },
                ),
                Action::Exit(status) => return Ok(Some(status)),
            }
        }
        Ok(None)
    }

    /// Send `message` to `to`.
    fn send(&mut self, kind: u8, message: &[u8], to: &Destination) -> Result<(), String> {
        let (from, address, mac, what) = match *to {
            Destination::Broadcast { from } => {
                (from, Ipv4Addr::BROADCAST, BROADCAST_MAC, "broadcast")
            }
            Destination::Server { from, to } => (from, to, self.server_mac, "to the server"),
        };
        let name = match kind {
            kind::DISCOVER => "DISCOVER",
            kind::REQUEST => "REQUEST",
            _ => "a message",
        };
        progress(
            &self.args.interface,
            &format!("sending {name} ({what} {address})"),
        );
        let packet = frame::wrap(from, address, message).map_err(|error| format!("{error:?}"))?;
        if self.packet.is_none() {
            self.packet = Some(Packet::open(self.index).map_err(|error| error.to_string())?);
        }
        if let Some(socket) = &self.packet {
            // A send the link refused is one more unanswered try, and the
            // client tries again; it is said, not fatal.
            if let Err(error) = socket.send(mac, &packet) {
                progress(&self.args.interface, &error.to_string());
            }
        }
        Ok(())
    }

    /// Every IPv4 address off the interface.
    fn deconfigure(&mut self) -> Result<(), String> {
        let mut held = Vec::new();
        refused(
            self.netlink.call(rtnl::get_addresses, |_, payload| {
                if let Some(address) = rtnl::address_of(payload) {
                    held.push(address);
                }
            }),
            "listing its addresses",
        )?;
        for address in held
            .into_iter()
            .filter(|address| address.index == self.index)
        {
            refused(
                self.netlink.call(
                    |seq| rtnl::delete_address(seq, self.index, address.address, address.prefix),
                    |_, _| {},
                ),
                &format!("removing {}/{}", address.address, address.prefix),
            )?;
        }
        Ok(())
    }

    /// `lease` on the interface: its address, the default route through its
    /// first router, and its name servers. Over `old`, only what changed.
    fn apply(&mut self, lease: &Lease, old: Option<&Lease>) -> Result<(), String> {
        let unchanged = old.is_some_and(|old| old.same_configuration(lease));
        if !unchanged {
            if let Some(old) = old {
                self.remove(old);
            }
            let added = self.netlink.call(
                |seq| rtnl::add_address(seq, self.index, lease.address, lease.prefix),
                |_, _| {},
            );
            if !matches!(added, Ok(Err(EEXIST))) {
                refused(added, &format!("adding {}/{}", lease.address, lease.prefix))?;
            }
            if let Some(&router) = lease.routers.first() {
                // Whoever's default route was on the interface goes first,
                // as the lease script's `ip route del default` did.
                let _ = self
                    .netlink
                    .call(|seq| rtnl::delete_default_route(seq, self.index), |_, _| {});
                refused(
                    self.netlink.call(
                        |seq| rtnl::add_default_route(seq, self.index, router),
                        |_, _| {},
                    ),
                    &format!("adding the default route via {router}"),
                )?;
            }
        }
        if let Some(text) = lease.resolv_conf()
            && old.is_none_or(|old| old.resolv_conf().as_deref() != Some(text.as_str()))
        {
            std::fs::write(RESOLV_CONF, text)
                .map_err(|error| format!("writing {RESOLV_CONF}: {error}"))?;
        }
        Ok(())
    }

    /// `lease`'s route and address off the interface; what is already gone
    /// is not an error, and what cannot be removed is said.
    fn remove(&mut self, lease: &Lease) {
        if !lease.routers.is_empty() {
            let gone = self
                .netlink
                .call(|seq| rtnl::delete_default_route(seq, self.index), |_, _| {});
            if !matches!(gone, Ok(Ok(()) | Err(ESRCH))) {
                let error = refused(gone, "removing the default route").err();
                progress(&self.args.interface, &error.unwrap_or_default());
            }
        }
        let gone = self.netlink.call(
            |seq| rtnl::delete_address(seq, self.index, lease.address, lease.prefix),
            |_, _| {},
        );
        if !matches!(gone, Ok(Ok(()) | Err(EADDRNOTAVAIL))) {
            let error = refused(
                gone,
                &format!("removing {}/{}", lease.address, lease.prefix),
            )
            .err();
            progress(&self.args.interface, &error.unwrap_or_default());
        }
    }
}

/// The interface called `name`.
fn find_link(netlink: &mut Netlink, name: &str) -> Result<rtnl::Link, String> {
    let mut found = None;
    refused(
        netlink.call(rtnl::get_links, |_, payload| {
            if let Some(link) = rtnl::link_of(payload)
                && link.name == name
            {
                found = Some(link);
            }
        }),
        "listing the interfaces",
    )?;
    found.ok_or_else(|| "no such interface".to_owned())
}

/// A netlink call's result as one error line, `doing` saying what it was.
fn refused(result: std::io::Result<Result<(), i32>>, doing: &str) -> Result<(), String> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(errno)) => Err(format!(
            "{doing}: {} (errno {errno})",
            std::io::Error::from_raw_os_error(errno)
        )),
        Err(error) => Err(format!("{doing}: {error}")),
    }
}
