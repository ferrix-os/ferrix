//! Driving the stack: what it wants to send, and what the clock owes it.
//!
//! Three calls, and a kernel that makes them in a loop is a working host:
//! [`Stack::poll_transmit`] until it answers nothing, [`Stack::on_timer`] when
//! [`Stack::poll_at`] has passed, and [`Stack::receive`] when a frame arrives.
//!
//! Nothing here blocks and nothing allocates a thread. A packet looped back to
//! this host is delivered inside `poll_transmit`, bounded so that a route that
//! points at itself is a dropped packet rather than a wedged processor.

use alloc::vec::Vec;

use ferrix_netwire::tcp;

use crate::input::pseudo_of;
use crate::socket::{Socket, SocketId};
use crate::stack::{Millis, Outgoing, Stack};

/// How many times one call will loop a packet back to this host.
const LOOPBACK_TURNS: usize = 64;

/// The largest segment payload copied out of a connection in one go.
const SEGMENT_BUFFER: usize = 2048;

impl Stack {
    /// The next frame to put on an interface, if there is one.
    pub fn poll_transmit(&mut self, now: Millis) -> Option<Outgoing> {
        for _ in 0..LOOPBACK_TURNS {
            if self.egress.is_empty() {
                self.pump(now);
            }
            let outgoing = self.egress.pop_front()?;
            if self.is_loopback(outgoing.interface) {
                self.receive(outgoing.interface, &outgoing.frame, now);
                continue;
            }
            return Some(outgoing);
        }
        None
    }

    /// Ask every connection for a segment, and fill the egress queue.
    fn pump(&mut self, now: Millis) {
        let ids: Vec<u32> = self
            .sockets
            .iter()
            .filter(|(_, socket)| matches!(socket, Socket::Stream(_)))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.pump_stream(id, now);
        }
        self.reap();
    }

    /// Take every segment one connection wants to send.
    fn pump_stream(&mut self, id: u32, now: Millis) {
        // On the heap, as the segment below is: an array here was 2 KiB of a
        // kernel stack under every frame `poll_transmit` reaches, a packet
        // looped back into `receive` and a new connection's insert among them
        // (F-107).
        // With no room, nothing is sent this time; the connection's segment
        // waits for the next poll.
        let mut payload = Vec::new();
        if payload.try_reserve_exact(SEGMENT_BUFFER).is_err() {
            return;
        }
        // NOALLOC: room for all of it was had above.
        payload.resize(SEGMENT_BUFFER, 0_u8);
        loop {
            // The socket comes out of the table while its segment is built,
            // because building it goes through the routing table and the
            // neighbour cache, which are the stack's and not the socket's.
            let Some(Socket::Stream(mut stream)) = self.sockets.remove(&id) else {
                return;
            };
            let transmit = stream.connection.poll_transmit(now, &mut payload);
            let local = stream.local;
            let remote = stream.remote;
            let hop_limit = stream.options.hop_limit;
            let device = stream.options.device;
            let _ = self.sockets.insert(id, Socket::Stream(stream));
            let Some(transmit) = transmit else {
                return;
            };
            let body = payload.get(..transmit.payload_len).unwrap_or_default();
            let pseudo = pseudo_of(local.address, remote.address);
            let length = tcp::MIN_HEADER_LEN + transmit.header.options.len() + 4 + body.len();
            let mut segment = alloc::vec![0_u8; length];
            let Ok(written) = transmit.header.emit(body, pseudo, &mut segment) else {
                return;
            };
            segment.truncate(written);
            let sent = self.send_ip(
                local.address,
                remote.address,
                tcp::PROTOCOL,
                &segment,
                hop_limit,
                device,
                now,
            );
            if sent.is_err() {
                if let Some(Socket::Stream(stream)) = self.sockets.get_mut(&id) {
                    stream.error = sent.err();
                }
                return;
            }
        }
    }

    /// Let the clock reach `now`.
    pub fn on_timer(&mut self, now: Millis) {
        for (interface, address) in self.neighbors.expire(now) {
            self.solicit(interface, address, now);
        }
        self.reassembly.expire(now);
        let ids: Vec<u32> = self
            .sockets
            .iter()
            .filter(|(_, socket)| matches!(socket, Socket::Stream(_)))
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            if let Some(Socket::Stream(stream)) = self.sockets.get_mut(id) {
                let _ = stream.connection.on_timer(now);
            }
        }
        for id in ids {
            self.promote(SocketId(id));
        }
        self.reap();
    }

    /// When [`Stack::on_timer`] next has something to do.
    #[must_use]
    pub fn poll_at(&self) -> Option<Millis> {
        let connections = self.sockets.values().filter_map(|socket| match socket {
            Socket::Stream(stream) => stream.connection.poll_at(),
            _ => None,
        });
        connections
            .chain(self.neighbors.poll_at())
            .chain(self.reassembly.poll_at())
            .min()
    }

    /// Forget connections that have finished and that no program still holds.
    ///
    /// A connection the program has closed stays in the table until its own
    /// close has been acknowledged, so the peer sees a close rather than
    /// silence. This is where it finally goes.
    pub(crate) fn reap(&mut self) {
        let dead: Vec<u32> = self
            .sockets
            .iter()
            .filter_map(|(id, socket)| match socket {
                Socket::Stream(stream)
                    if stream.connection.is_finished()
                        && stream.closing
                        && stream.listener.is_none() =>
                {
                    Some(*id)
                }
                _ => None,
            })
            .collect();
        for id in dead {
            let _ = self.sockets.remove(&id);
        }
    }
}
