//! Putting a fragmented IPv4 datagram back together.
//!
//! Fragmentation is rare on the paths Ferrix runs over and is not optional:
//! a DNS answer over a tunnel, or a UDP write larger than the link will
//! carry, arrives in pieces or not at all.
//!
//! # What a stranger can do with this, and what stops them
//!
//! Reassembly is a buffer a remote host fills and this host cannot empty: the
//! classic denial of service is to send the first fragment of a million
//! datagrams and never the rest. Three limits answer it -- a ceiling on the
//! bytes held across all datagrams, a ceiling on the pieces of one, and a
//! deadline after which an incomplete datagram is dropped. RFC 1122 puts the
//! deadline between 60 and 120 seconds; Linux uses 30, and so does this.

use alloc::vec::Vec;

use crate::addr::Ipv4;

/// Milliseconds on the stack's clock.
pub type Millis = u64;

/// How long an incomplete datagram is held.
pub const TIMEOUT: Millis = 30_000;

/// How many bytes are held across every datagram being reassembled.
pub const MAX_BYTES: usize = 256 * 1024;

/// How many bytes a network namespace other than the first holds: what a
/// program that owns one can make a peer of its own fill, charged to it
/// (`docs/NETNS.md` section 5).
pub const NAMESPACE_BYTES: usize = 32 * 1024;

/// How many pieces one datagram may arrive in.
pub const MAX_PIECES: usize = 64;

/// How many datagrams are reassembled at once.
pub const MAX_DATAGRAMS: usize = 16;

/// The largest datagram that can be reassembled, which is the format's own
/// limit.
pub const MAX_DATAGRAM: usize = 65_535;

/// What makes two fragments part of the same datagram: RFC 791's four fields.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Key {
    /// Who sent it.
    pub source: Ipv4,
    /// Who it is for.
    pub destination: Ipv4,
    /// The sender's identifier for this datagram.
    pub identification: u16,
    /// What it carries.
    pub protocol: u8,
}

/// One piece.
#[derive(Clone, Debug)]
struct Piece {
    /// Where it starts in the reassembled payload.
    offset: usize,
    /// The bytes.
    bytes: Vec<u8>,
}

/// One datagram being put together.
#[derive(Clone, Debug)]
struct Pending {
    /// Which datagram.
    key: Key,
    /// The pieces, in the order they arrived.
    pieces: Vec<Piece>,
    /// How long the whole payload is, once the last fragment has arrived.
    total: Option<usize>,
    /// When to give up.
    expires_at: Millis,
    /// How many bytes are held.
    held: usize,
}

/// The datagrams being put together.
#[derive(Clone, Debug)]
pub struct Reassembler {
    /// The datagrams in progress.
    pending: Vec<Pending>,
    /// How many bytes are held across all of them.
    held: usize,
    /// The most bytes it will hold.
    limit: usize,
}

impl Default for Reassembler {
    fn default() -> Reassembler {
        Reassembler::new()
    }
}

impl Reassembler {
    /// An empty reassembler.
    #[must_use]
    pub const fn new() -> Reassembler {
        Reassembler {
            pending: Vec::new(),
            held: 0,
            limit: MAX_BYTES,
        }
    }

    /// An empty reassembler that holds at most `limit` bytes.
    #[must_use]
    pub const fn with_limit(limit: usize) -> Reassembler {
        Reassembler {
            pending: Vec::new(),
            held: 0,
            limit,
        }
    }

    /// Forget everything held: what a charge that cannot be paid does.
    pub fn flush(&mut self) {
        self.pending.clear();
        self.held = 0;
    }

    /// How many bytes are held.
    #[must_use]
    pub const fn held(&self) -> usize {
        self.held
    }

    /// Take a fragment. Answers the whole payload once the last hole closes.
    ///
    /// `offset` is in bytes, already multiplied out of the header's eight-byte
    /// units, and `more` is the header's more-fragments bit.
    pub fn insert(
        &mut self,
        key: Key,
        offset: usize,
        more: bool,
        payload: &[u8],
        now: Millis,
    ) -> Option<Vec<u8>> {
        if offset.saturating_add(payload.len()) > MAX_DATAGRAM {
            return None;
        }
        self.expire(now);
        let index = self.slot(key, now)?;
        let pending = self.pending.get_mut(index)?;
        if pending.pieces.len() >= MAX_PIECES {
            return None;
        }
        if !more {
            pending.total = Some(offset.saturating_add(payload.len()));
        }
        if self.held.saturating_add(payload.len()) > self.limit {
            // Rather than evict somebody else's datagram, refuse this one: an
            // attacker choosing which datagram to displace is worse than a
            // fragment lost under load.
            return None;
        }
        let pending = self.pending.get_mut(index)?;
        pending.held += payload.len();
        self.held += payload.len();
        pending.pieces.push(Piece {
            offset,
            bytes: payload.to_vec(),
        });
        let complete = Self::complete(pending)?;
        self.held = self.held.saturating_sub(pending.held);
        let _ = self.pending.remove(index);
        Some(complete)
    }

    /// The whole payload, if every byte up to the total has arrived.
    fn complete(pending: &Pending) -> Option<Vec<u8>> {
        let total = pending.total?;
        let mut out = alloc::vec![0_u8; total];
        let mut covered = alloc::vec![false; total];
        for piece in &pending.pieces {
            let end = piece.offset.checked_add(piece.bytes.len())?;
            if end > total {
                return None;
            }
            let slot = out.get_mut(piece.offset..end)?;
            slot.copy_from_slice(&piece.bytes);
            for byte in covered.get_mut(piece.offset..end)? {
                *byte = true;
            }
        }
        covered.iter().all(|seen| *seen).then_some(out)
    }

    /// Where this datagram's pieces go, making room if there is none.
    fn slot(&mut self, key: Key, now: Millis) -> Option<usize> {
        if let Some(index) = self.pending.iter().position(|held| held.key == key) {
            return Some(index);
        }
        if self.pending.len() >= MAX_DATAGRAMS {
            let oldest = self
                .pending
                .iter()
                .enumerate()
                .min_by_key(|(_, held)| held.expires_at)
                .map(|(index, _)| index)?;
            self.drop_at(oldest);
        }
        self.pending.push(Pending {
            key,
            pieces: Vec::new(),
            total: None,
            expires_at: now.saturating_add(TIMEOUT),
            held: 0,
        });
        Some(self.pending.len() - 1)
    }

    /// Drop everything whose deadline has passed.
    pub fn expire(&mut self, now: Millis) {
        let mut index = 0;
        while index < self.pending.len() {
            let expired = self
                .pending
                .get(index)
                .is_some_and(|pending| now >= pending.expires_at);
            if expired {
                self.drop_at(index);
            } else {
                index += 1;
            }
        }
    }

    /// When [`Reassembler::expire`] next has something to do.
    #[must_use]
    pub fn poll_at(&self) -> Option<Millis> {
        self.pending.iter().map(|pending| pending.expires_at).min()
    }

    /// Forget the datagram at `index`.
    fn drop_at(&mut self, index: usize) {
        if index >= self.pending.len() {
            return;
        }
        let pending = self.pending.remove(index);
        self.held = self.held.saturating_sub(pending.held);
    }
}
