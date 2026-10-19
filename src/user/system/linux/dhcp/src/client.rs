//! The client's state machine (RFC 2131 §4.4, figure 5), with no clock and
//! no socket of its own.
//!
//! The caller passes the time in -- a [`Duration`] since the client started
//! -- and gets back what to do: messages to send, a lease to apply, a lease
//! to take away, a status to exit with. [`Client::wake`] says when the next
//! timer is due, so the caller waits on its socket until then and calls
//! [`Client::tick`], or [`Client::receive`] when a reply comes first. That
//! is what lets the tests walk a day-long lease through T1 and T2 without
//! waiting a day.
//!
//! ```text
//!  Selecting --OFFER--> Requesting --ACK--> Bound --T1--> Renewing --T2--> Rebinding
//!      ^                    |  NAK            ^              |  ACK           |  ACK
//!      |                    v                 +--------------+----------------+
//!      +---- NAK, or the lease runs out, from Renewing or Rebinding -----------+
//! ```

use std::net::Ipv4Addr;
use std::time::Duration;

use crate::message::{Reply, Request, kind};
use crate::{Lease, prefix_of};

/// The prefix length used when a server sends no subnet mask, as busybox's
/// lease script did.
const DEFAULT_PREFIX: u8 = 24;

/// The shortest wait between retransmissions while renewing or rebinding
/// (RFC 2131 §4.4.5: "down to a minimum of 60 seconds").
const RENEW_RETRY_FLOOR: Duration = Duration::from_secs(60);

/// What the command line asked of the client.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// The interface's hardware address.
    pub mac: [u8; 6],
    /// How many DISCOVERs, and then REQUESTs, to send before giving up a
    /// round (`-t`).
    pub tries: u32,
    /// How long to wait for an answer to each (`-T`).
    pub timeout: Duration,
    /// How long to wait after a round with no lease before the next (`-A`).
    pub pause: Duration,
    /// Exit with status 1 after a round with no lease, rather than pause and
    /// try again (`-n`).
    pub exit_without_lease: bool,
    /// Exit with status 0 once a lease is applied, rather than stay to renew
    /// it (`-q`).
    pub quit_with_lease: bool,
}

/// Where a message goes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Destination {
    /// To every host on the link, from `0.0.0.0` or the address held.
    Broadcast {
        /// The IP source address.
        from: Ipv4Addr,
    },
    /// To the server that gave the lease, from the address it gave.
    Server {
        /// The IP source address: the lease's.
        from: Ipv4Addr,
        /// The server.
        to: Ipv4Addr,
    },
}

/// What the caller has to do.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    /// Send `message` (a DHCP message, without its IP and UDP headers).
    Send {
        /// What it is, for a progress line.
        kind: u8,
        /// The message.
        message: Vec<u8>,
        /// Where it goes.
        to: Destination,
    },
    /// A lease to apply, where none was.
    Bound(Lease),
    /// The lease was extended, and is now `new`. Applied again only where it
    /// differs from `old` ([`Lease::same_configuration`]).
    Renewed {
        /// The lease held until now.
        old: Lease,
        /// The lease as extended.
        new: Lease,
    },
    /// The lease is gone -- refused by its server or run out -- and its
    /// address and route are to be taken away.
    Lost(Lease),
    /// No answer came after a round of tries; the client pauses before the
    /// next, or exits if [`Action::Exit`] follows.
    NoLease,
    /// Exit with this status.
    Exit(u8),
}

/// An offer being requested.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Offer {
    /// The address offered.
    address: Ipv4Addr,
    /// The server that offered it.
    server: Ipv4Addr,
}

/// Where the client is.
#[derive(Clone, PartialEq, Eq, Debug)]
enum State {
    /// Sending DISCOVERs: `sent` of them so far, the next at `next`.
    Selecting { sent: u32, next: Duration },
    /// Sending REQUESTs for `offer`.
    Requesting {
        offer: Offer,
        sent: u32,
        next: Duration,
    },
    /// Holding `lease`, given at `since`.
    Bound { lease: Lease, since: Duration },
    /// Asking the lease's server to extend it; the next try at `next`.
    Renewing {
        lease: Lease,
        since: Duration,
        next: Duration,
    },
    /// Asking any server to extend it; the next try at `next`.
    Rebinding {
        lease: Lease,
        since: Duration,
        next: Duration,
    },
    /// Waiting out `-A` after a round with no lease.
    Paused { until: Duration },
    /// Done: [`Action::Exit`] has been returned.
    Done,
}

/// The client.
#[derive(Clone, Debug)]
pub struct Client {
    /// What the command line asked.
    config: Config,
    /// Where it is.
    state: State,
    /// The transaction under way.
    xid: u32,
    /// Where the next transaction identifier comes from: xorshift32, seeded
    /// by the caller.
    random: u32,
    /// When the current transaction began, for `secs`.
    began: Duration,
}

impl Client {
    /// A client that sends its first DISCOVER at the first [`Client::tick`].
    /// `seed` is any random word, which transaction identifiers are drawn
    /// from.
    #[must_use]
    pub fn new(config: Config, seed: u32) -> Client {
        let mut client = Client {
            config,
            state: State::Selecting {
                sent: 0,
                next: Duration::ZERO,
            },
            xid: 0,
            random: seed | 1,
            began: Duration::ZERO,
        };
        client.new_transaction(Duration::ZERO);
        client
    }

    /// When the next timer is due, or `None` when nothing is ever due again:
    /// the client is done, or holds a lease that never ends.
    #[must_use]
    pub fn wake(&self) -> Option<Duration> {
        match &self.state {
            State::Selecting { next, .. }
            | State::Requesting { next, .. }
            | State::Renewing { next, .. }
            | State::Rebinding { next, .. } => Some(*next),
            State::Bound { lease, since } => {
                (lease.seconds != u32::MAX).then(|| *since + seconds(lease.renew))
            }
            State::Paused { until } => Some(*until),
            State::Done => None,
        }
    }

    /// The lease held, if any.
    #[must_use]
    pub fn lease(&self) -> Option<&Lease> {
        match &self.state {
            State::Bound { lease, .. }
            | State::Renewing { lease, .. }
            | State::Rebinding { lease, .. } => Some(lease),
            _ => None,
        }
    }

    /// Whether a lease is held and nothing is being asked: between the ACK
    /// and T1, when no reply is awaited.
    #[must_use]
    pub fn is_bound(&self) -> bool {
        matches!(self.state, State::Bound { .. })
    }

    /// Whether the client has said to exit.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }

    /// Act on the time being `now`: whatever timer is due.
    pub fn tick(&mut self, now: Duration) -> Vec<Action> {
        if self.wake().is_none_or(|due| now < due) {
            return Vec::new();
        }
        match self.state.clone() {
            State::Selecting { sent, .. } if sent < self.config.tries => {
                self.state = State::Selecting {
                    sent: sent + 1,
                    next: now + self.config.timeout,
                };
                vec![self.discover(now)]
            }
            State::Selecting { .. } => self.no_lease(now),
            State::Requesting { offer, sent, .. } if sent < self.config.tries => {
                self.state = State::Requesting {
                    offer,
                    sent: sent + 1,
                    next: now + self.config.timeout,
                };
                vec![self.request(now, offer)]
            }
            // The offer went unanswered: ask again from the start.
            State::Requesting { .. } => self.select(now),
            State::Bound { lease, since } => {
                self.new_transaction(now);
                self.renew(now, lease, since)
            }
            State::Renewing { lease, since, .. } => self.renew(now, lease, since),
            State::Rebinding { lease, since, .. } => self.rebind(now, lease, since),
            State::Paused { .. } => self.select(now),
            State::Done => Vec::new(),
        }
    }

    /// Act on `reply`, which arrived at `now`. A reply to another
    /// transaction or another client changes nothing.
    pub fn receive(&mut self, now: Duration, reply: &Reply) -> Vec<Action> {
        if reply.xid != self.xid || reply.mac != self.config.mac {
            return Vec::new();
        }
        match (self.state.clone(), reply.kind) {
            (State::Selecting { .. }, kind::OFFER) => {
                let (Some(server), false) = (reply.server, reply.yiaddr.is_unspecified()) else {
                    return Vec::new();
                };
                let offer = Offer {
                    address: reply.yiaddr,
                    server,
                };
                self.state = State::Requesting {
                    offer,
                    sent: 1,
                    next: now + self.config.timeout,
                };
                vec![self.request(now, offer)]
            }
            (State::Requesting { offer, .. }, kind::ACK) if reply.yiaddr == offer.address => {
                let lease = lease_of(reply, offer.server);
                self.bind(now, lease.clone(), Action::Bound(lease))
            }
            (State::Requesting { offer, .. }, kind::NAK)
                if reply.server.is_none_or(|server| server == offer.server) =>
            {
                self.select(now)
            }
            (State::Renewing { lease, .. } | State::Rebinding { lease, .. }, kind::ACK)
                if !reply.yiaddr.is_unspecified() =>
            {
                let new = lease_of(reply, reply.server.unwrap_or(lease.server));
                self.bind(
                    now,
                    new.clone(),
                    Action::Renewed {
                        old: lease,
                        new: new.clone(),
                    },
                )
            }
            (State::Renewing { lease, .. } | State::Rebinding { lease, .. }, kind::NAK) => {
                let mut actions = vec![Action::Lost(lease)];
                actions.extend(self.select(now));
                actions
            }
            _ => Vec::new(),
        }
    }

    /// Start a new transaction at `now`.
    fn new_transaction(&mut self, now: Duration) {
        let mut x = self.random;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.random = x;
        self.xid = x;
        self.began = now;
    }

    /// `secs` for a message sent at `now`.
    fn secs(&self, now: Duration) -> u16 {
        u16::try_from(now.saturating_sub(self.began).as_secs()).unwrap_or(u16::MAX)
    }

    /// Back to the start: a new transaction, its first DISCOVER now.
    fn select(&mut self, now: Duration) -> Vec<Action> {
        self.new_transaction(now);
        self.state = State::Selecting { sent: 0, next: now };
        self.tick(now)
    }

    /// A round with no lease: exit, or pause and start again.
    fn no_lease(&mut self, now: Duration) -> Vec<Action> {
        if self.config.exit_without_lease {
            self.state = State::Done;
            return vec![Action::NoLease, Action::Exit(1)];
        }
        self.state = State::Paused {
            until: now + self.config.pause,
        };
        vec![Action::NoLease]
    }

    /// Hold `lease` from `now`, saying `said`, and exit if `-q` asked to.
    fn bind(&mut self, now: Duration, lease: Lease, said: Action) -> Vec<Action> {
        if self.config.quit_with_lease {
            self.state = State::Done;
            return vec![said, Action::Exit(0)];
        }
        self.state = State::Bound { lease, since: now };
        vec![said]
    }

    /// Renewing at `now` the lease given at `since`: a REQUEST to its server,
    /// or on to rebinding at T2.
    fn renew(&mut self, now: Duration, lease: Lease, since: Duration) -> Vec<Action> {
        let rebind_at = since + seconds(lease.rebind);
        if now >= rebind_at {
            return self.rebind(now, lease, since);
        }
        let message = Request {
            kind: kind::REQUEST,
            xid: self.xid,
            secs: self.secs(now),
            mac: self.config.mac,
            ciaddr: lease.address,
            requested: None,
            server: None,
        }
        .encode();
        let to = Destination::Server {
            from: lease.address,
            to: lease.server,
        };
        self.state = State::Renewing {
            next: retry_at(now, rebind_at),
            lease,
            since,
        };
        vec![Action::Send {
            kind: kind::REQUEST,
            message,
            to,
        }]
    }

    /// Rebinding at `now` the lease given at `since`: a broadcast REQUEST, or
    /// the lease lost when it has run out.
    fn rebind(&mut self, now: Duration, lease: Lease, since: Duration) -> Vec<Action> {
        let ends = since + seconds(lease.seconds);
        if now >= ends {
            let mut actions = vec![Action::Lost(lease)];
            actions.extend(self.select(now));
            return actions;
        }
        let message = Request {
            kind: kind::REQUEST,
            xid: self.xid,
            secs: self.secs(now),
            mac: self.config.mac,
            ciaddr: lease.address,
            requested: None,
            server: None,
        }
        .encode();
        let to = Destination::Broadcast {
            from: lease.address,
        };
        self.state = State::Rebinding {
            next: retry_at(now, ends),
            lease,
            since,
        };
        vec![Action::Send {
            kind: kind::REQUEST,
            message,
            to,
        }]
    }

    /// A DISCOVER, sent at `now`.
    fn discover(&self, now: Duration) -> Action {
        Action::Send {
            kind: kind::DISCOVER,
            message: Request {
                kind: kind::DISCOVER,
                xid: self.xid,
                secs: self.secs(now),
                mac: self.config.mac,
                ciaddr: Ipv4Addr::UNSPECIFIED,
                requested: None,
                server: None,
            }
            .encode(),
            to: Destination::Broadcast {
                from: Ipv4Addr::UNSPECIFIED,
            },
        }
    }

    /// A REQUEST for `offer`, sent at `now`.
    fn request(&self, now: Duration, offer: Offer) -> Action {
        Action::Send {
            kind: kind::REQUEST,
            message: Request {
                kind: kind::REQUEST,
                xid: self.xid,
                secs: self.secs(now),
                mac: self.config.mac,
                ciaddr: Ipv4Addr::UNSPECIFIED,
                requested: Some(offer.address),
                server: Some(offer.server),
            }
            .encode(),
            to: Destination::Broadcast {
                from: Ipv4Addr::UNSPECIFIED,
            },
        }
    }
}

/// `n` seconds.
fn seconds(n: u32) -> Duration {
    Duration::from_secs(u64::from(n))
}

/// When to try again, at `now`, before `deadline`: half the time left, but
/// not sooner than a minute, and not after the deadline itself.
fn retry_at(now: Duration, deadline: Duration) -> Duration {
    let half = deadline.saturating_sub(now) / 2;
    (now + half.max(RENEW_RETRY_FLOOR)).min(deadline)
}

/// The lease an ACK gives, from `server`.
///
/// T1 and T2 default to half and seven eighths of the lease (RFC 2131
/// §4.4.5), and are taken from the server only when they are in order: T1
/// before T2 before the end.
#[must_use]
pub fn lease_of(reply: &Reply, server: Ipv4Addr) -> Lease {
    // A server that names no length gives what RFC 2131 calls the default; an
    // hour is what most servers' default is.
    let total = reply.lease.unwrap_or(3600);
    let (renew, rebind) = if total == u32::MAX {
        (u32::MAX, u32::MAX)
    } else {
        let renew = reply.renew.unwrap_or(total / 2);
        let rebind = reply
            .rebind
            .unwrap_or(u32::try_from(u64::from(total) * 7 / 8).unwrap_or(total));
        if renew <= rebind && rebind <= total {
            (renew, rebind)
        } else {
            (
                total / 2,
                u32::try_from(u64::from(total) * 7 / 8).unwrap_or(total),
            )
        }
    };
    Lease {
        address: reply.yiaddr,
        prefix: reply.netmask.and_then(prefix_of).unwrap_or(DEFAULT_PREFIX),
        server,
        routers: reply.routers.clone(),
        dns: reply.dns.clone(),
        domain: reply.domain.clone(),
        seconds: total,
        renew,
        rebind,
    }
}
