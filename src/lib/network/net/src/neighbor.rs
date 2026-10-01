//! Which hardware address an IP address is at, and what to do with a packet
//! while nobody knows.
//!
//! One cache for both families. The question ARP asks over IPv4 and the one
//! Neighbor Discovery asks over IPv6 are the same question, the answers have
//! the same lifetime rules, and the packets waiting for either have to be held
//! the same way -- so the difference between them lives in what the stack puts
//! on the wire, not here.
//!
//! # Packets wait, and are dropped rather than held for ever
//!
//! A packet to an unresolved address is queued against its entry. Linux holds
//! three per entry and drops the oldest; this holds the same number for the
//! same reason. When resolution fails the queue is dropped, because the
//! alternative is a burst of traffic to a host that went away minutes ago.

use alloc::vec::Vec;

use ferrix_netwire::ethernet::Mac;

use crate::addr::IpAddress;

/// Milliseconds on the stack's clock.
pub type Millis = u64;

/// How long an answer is trusted before it has to be confirmed.
pub const REACHABLE: Millis = 30_000;

/// How long between solicitations for an address nobody has answered for.
pub const RETRANSMIT: Millis = 1_000;

/// How many solicitations go unanswered before the address is unreachable.
pub const MAX_SOLICITATIONS: u32 = 3;

/// How long a failed entry is remembered, so that a burst of packets to a dead
/// address is one failure rather than one per packet.
pub const FAILED_FOR: Millis = 20_000;

/// How many packets wait for one address.
pub const MAX_QUEUED: usize = 3;

/// How many addresses the cache holds before it forgets the oldest.
pub const MAX_ENTRIES: usize = 256;

/// What is known about an address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// Asked, not yet answered.
    Incomplete,
    /// Answered, and the answer is still fresh.
    Reachable,
    /// Answered, but long enough ago that it should be confirmed when next
    /// used.
    Stale,
    /// Being confirmed: usable, and a solicitation is outstanding.
    Probe,
    /// Configured by hand and never expired.
    Permanent,
    /// Nobody answered.
    Failed,
}

impl State {
    /// Whether a packet may be sent to the address this state describes.
    #[must_use]
    pub const fn is_usable(self) -> bool {
        matches!(
            self,
            State::Reachable | State::Stale | State::Probe | State::Permanent
        )
    }

    /// The `NUD_` bits Linux reports this state as.
    #[must_use]
    pub const fn nud(self) -> u16 {
        match self {
            State::Incomplete => 0x01,
            State::Reachable => 0x02,
            State::Stale => 0x04,
            State::Probe => 0x10,
            State::Permanent => 0x80,
            State::Failed => 0x20,
        }
    }
}

/// One address the cache knows something about.
#[derive(Clone, Debug)]
pub struct Entry {
    /// The address asked about.
    pub address: IpAddress,
    /// The interface it is on.
    pub interface: u32,
    /// Its hardware address, once known.
    pub mac: Option<Mac>,
    /// What is known.
    pub state: State,
    /// When the state stops being true.
    pub expires_at: Millis,
    /// When to send the next solicitation.
    pub solicit_at: Millis,
    /// How many have been sent.
    pub solicitations: u32,
    /// Packets waiting for an answer.
    queued: Vec<Vec<u8>>,
}

/// The cache.
#[derive(Clone, Debug, Default)]
pub struct Neighbors {
    /// The entries, most recently touched last.
    entries: Vec<Entry>,
}

impl Neighbors {
    /// An empty cache.
    #[must_use]
    pub const fn new() -> Neighbors {
        Neighbors {
            entries: Vec::new(),
        }
    }

    /// Every entry, for `/proc/net/arp` and for `ip neigh`.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Where `address` is on `interface`, if it is usable.
    ///
    /// A stale answer is returned and moved to probing, which is what makes
    /// the confirmation happen alongside the traffic rather than instead of
    /// it.
    pub fn lookup(&mut self, interface: u32, address: IpAddress, now: Millis) -> Option<Mac> {
        let entry = self.find_mut(interface, address)?;
        if entry.state == State::Stale {
            entry.state = State::Probe;
            entry.solicitations = 0;
            entry.solicit_at = now;
        }
        if entry.state.is_usable() {
            entry.mac
        } else {
            None
        }
    }

    /// Hold `packet` until `address` is resolved.
    ///
    /// Answers whether a solicitation should go out now, which is true exactly
    /// when this packet created the entry.
    pub fn queue(
        &mut self,
        interface: u32,
        address: IpAddress,
        packet: Vec<u8>,
        now: Millis,
    ) -> bool {
        if let Some(entry) = self.find_mut(interface, address) {
            if entry.state == State::Failed && now >= entry.expires_at {
                entry.state = State::Incomplete;
                entry.solicitations = 0;
                entry.solicit_at = now;
                entry.queued.push(packet);
                return true;
            }
            if entry.state == State::Failed {
                return false;
            }
            if entry.queued.len() >= MAX_QUEUED {
                let _ = entry.queued.remove(0);
            }
            entry.queued.push(packet);
            return false;
        }
        self.insert(Entry {
            address,
            interface,
            mac: None,
            state: State::Incomplete,
            expires_at: now.saturating_add(RETRANSMIT * u64::from(MAX_SOLICITATIONS)),
            solicit_at: now.saturating_add(RETRANSMIT),
            solicitations: 1,
            queued: alloc::vec![packet],
        });
        true
    }

    /// Record an answer, and give back the packets that were waiting for it.
    pub fn record(
        &mut self,
        interface: u32,
        address: IpAddress,
        mac: Mac,
        now: Millis,
    ) -> Vec<Vec<u8>> {
        match self.find_mut(interface, address) {
            Some(entry) => {
                if entry.state == State::Permanent {
                    return Vec::new();
                }
                entry.mac = Some(mac);
                entry.state = State::Reachable;
                entry.expires_at = now.saturating_add(REACHABLE);
                entry.solicitations = 0;
                core::mem::take(&mut entry.queued)
            }
            None => {
                self.insert(Entry {
                    address,
                    interface,
                    mac: Some(mac),
                    state: State::Reachable,
                    expires_at: now.saturating_add(REACHABLE),
                    solicit_at: Millis::MAX,
                    solicitations: 0,
                    queued: Vec::new(),
                });
                Vec::new()
            }
        }
    }

    /// Configure an answer that never expires.
    pub fn set_permanent(&mut self, interface: u32, address: IpAddress, mac: Mac) {
        if let Some(entry) = self.find_mut(interface, address) {
            entry.mac = Some(mac);
            entry.state = State::Permanent;
            entry.expires_at = Millis::MAX;
            return;
        }
        self.insert(Entry {
            address,
            interface,
            mac: Some(mac),
            state: State::Permanent,
            expires_at: Millis::MAX,
            solicit_at: Millis::MAX,
            solicitations: 0,
            queued: Vec::new(),
        });
    }

    /// The heap the cache holds: its entries and the packets they hold back.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.entries.iter().fold(0_usize, |total, entry| {
            let queued: usize = entry.queued.iter().map(Vec::capacity).sum();
            total
                .saturating_add(size_of::<Entry>())
                .saturating_add(queued)
        })
    }

    /// Forget everything learned, keeping only what was configured by hand:
    /// what a namespace does when it cannot pay for the cache.
    pub fn shed(&mut self) {
        self.entries.retain(|entry| entry.state == State::Permanent);
    }

    /// Forget an address. Answers whether there was one.
    pub fn remove(&mut self, interface: u32, address: IpAddress) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|entry| entry.interface != interface || entry.address != address);
        self.entries.len() != before
    }

    /// Forget everything on an interface, which is what taking it down means.
    pub fn remove_interface(&mut self, interface: u32) {
        self.entries.retain(|entry| entry.interface != interface);
    }

    /// Move the clock on, and answer the addresses that need soliciting.
    ///
    /// An entry that has been solicited enough times without an answer becomes
    /// `Failed` and its packets are dropped here.
    pub fn expire(&mut self, now: Millis) -> Vec<(u32, IpAddress)> {
        let mut solicit = Vec::new();
        for entry in &mut self.entries {
            match entry.state {
                State::Permanent | State::Failed | State::Stale => {}
                State::Reachable if now >= entry.expires_at => entry.state = State::Stale,
                State::Reachable => {}
                State::Incomplete | State::Probe if now >= entry.solicit_at => {
                    if entry.solicitations >= MAX_SOLICITATIONS {
                        entry.state = State::Failed;
                        entry.expires_at = now.saturating_add(FAILED_FOR);
                        entry.queued.clear();
                        entry.mac = None;
                        continue;
                    }
                    entry.solicitations += 1;
                    entry.solicit_at = now.saturating_add(RETRANSMIT);
                    solicit.push((entry.interface, entry.address));
                }
                State::Incomplete | State::Probe => {}
            }
        }
        solicit
    }

    /// When [`Neighbors::expire`] next has something to do.
    #[must_use]
    pub fn poll_at(&self) -> Option<Millis> {
        self.entries
            .iter()
            .filter_map(|entry| match entry.state {
                State::Incomplete | State::Probe => Some(entry.solicit_at),
                State::Reachable => Some(entry.expires_at),
                _ => None,
            })
            .min()
    }

    /// The entry for an address on an interface.
    fn find_mut(&mut self, interface: u32, address: IpAddress) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .find(|entry| entry.interface == interface && entry.address == address)
    }

    /// Add an entry, making room by forgetting the oldest if the cache is
    /// full.
    fn insert(&mut self, entry: Entry) {
        if self.entries.len() >= MAX_ENTRIES {
            let oldest = self
                .entries
                .iter()
                .position(|held| held.state != State::Permanent)
                .unwrap_or(0);
            let _ = self.entries.remove(oldest);
        }
        self.entries.push(entry);
    }
}
