//! The gateway's own TCP endpoint: the server half, on this side of the wire.
//!
//! A gateway with no privilege cannot forward TCP. It has no way to put a
//! segment on the host's network with the guest's address on it, and no way to
//! see the reply if it did. So it does not forward: it *terminates*. The guest's
//! connection ends here, in this file, and a second connection — an ordinary
//! host [`TcpStream`], the kind any program opens — is made to where the guest
//! was trying to go, with the payload relayed between the two. That is what
//! slirp does, and it is why a user-mode network is a pair of connections
//! rather than a router.
//!
//! Terminating means implementing the half of TCP a server does: accept a SYN,
//! answer it once the host connection is up, number the bytes in both
//! directions, acknowledge what arrives, retransmit what is not acknowledged,
//! and close each direction separately.
//!
//! # What this is not
//!
//! It is not `src/lib/network/nettcp`. That crate is the *guest's* stack — the thing on
//! the other end of this wire — and using it here would mean testing Ferrix's
//! TCP against itself, where a shared misreading of RFC 9293 cancels out and
//! both sides agree on something no other host does.
//!
//! It is also not fast, on purpose:
//!
//! * a fixed receive window, reduced by whatever has not yet been written to
//!   the host, which is real flow control and no more;
//! * retransmission of everything unacknowledged on a fixed timer, with no
//!   round-trip estimate, and of one segment on three duplicate
//!   acknowledgments;
//! * no congestion control, no SACK, no window scaling and no timestamps.
//!
//! The path between the two ends is a loopback socket to this machine's own
//! kernel and a virtio-net device: it does not reorder packets and has no
//! bandwidth-delay product worth a congestion window. It does lose them: more
//! segments than the receive buffers the guest's driver has posted are
//! dropped between QEMU and the guest. Three things answer that, and nothing
//! more:
//!
//! * the gateway has at most [`MAX_IN_FLIGHT`] segments unacknowledged, which
//!   the guest can take, over all its connections together, shared out a
//!   segment at a time (`Core::service_tcp`);
//! * three duplicate acknowledgments send the first missing segment again at
//!   once, and only that one: the guest keeps what arrived after the hole;
//! * [`RETRANSMIT`] is short, because a round trip to the guest is a few
//!   milliseconds and a lost segment's timer is the connection standing still.
//!
//! What each is worth, in `cargo xtask run-compositor --everything` under WHPX
//! on 2026-09-30 (with the driver's 16 receive slots), a 4 MB download in the guest: the whole window in flight and
//! every duplicate acknowledgment resending all of it, 0.33 MB/s, having sent
//! five times the bytes; sixteen in flight, 0.21 MB/s, a quarter of the time
//! on the timer; eight, 3.4 to 3.8 MB/s with the timer at 200 ms and 4.3 to
//! 4.4 MB/s at 20 ms. The host fetched the same file at 23 MB/s. With the
//! driver's ring and queue at 128 entries (2026-10-01, `x86_64` under KVM, three
//! runs each of a 32 MiB download): 8 in flight 3.6 to 12.4 MB/s, 16 5.5 to
//! 14.3, 24 10.8 to 15.5, and 44 (the guest's whole window) collapses again.
//!
//! # Forwarded connections
//!
//! A `--forward` port turns that around for connections the host opens. The
//! gateway accepts on the host's loopback and then does the half of TCP a
//! client does, once: it sends the guest a SYN from `10.0.2.2`, and a SYN-ACK
//! of it opens the connection. From there it is the same connection as any
//! other — the same relay, the same timers, the same close — because once both
//! ends are open TCP has no idea which of them spoke first.
//!
//! # Out-of-order data
//!
//! Segments that do not start at `rcv_nxt` are dropped rather than queued, and
//! the acknowledgment the guest gets back asks for `rcv_nxt` again. On a lossy
//! path that would be a performance disaster; here the only way it happens is a
//! guest that reordered its own transmissions, and the recovery is the one TCP
//! already specifies.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream};
use std::ops::Range;
use std::time::{Duration, Instant};

use ferrix_netwire::checksum::Pseudo;
use ferrix_netwire::ipv4;
use ferrix_netwire::tcp::{self as wire, Flags};

use super::{Core, Forward, GATEWAY_IP, GUEST_IP, MTU, Result, bump};
use crate::Error;

/// The most payload one segment carries: the MTU less the IPv4 and TCP headers,
/// neither of which this endpoint ever puts options in after the handshake.
const MAX_SEGMENT: usize = MTU - ipv4::MIN_HEADER_LEN - wire::MIN_HEADER_LEN;

/// The smallest maximum segment size a peer may usefully ask for (RFC 9293
/// section 3.7.1). A SYN offering less is taken to mean this.
const MIN_SEGMENT: usize = 536;

/// What the gateway advertises it can receive, before subtracting what it is
/// still holding. Sixteen bytes short of a power of two for no reason but that
/// it fits a `u16` with room to be reduced.
const RECEIVE_WINDOW: u16 = 32_768;

/// How much host data may wait to go to the guest before the host socket stops
/// being read, which is what pushes back on a server sending faster than the
/// guest reads.
const SEND_CAPACITY: usize = 64 * 1024;

/// How long to wait for an acknowledgment before sending everything
/// unacknowledged again: a few of the guest's round trips, which are a few
/// milliseconds, and no more, because every lost segment the duplicate
/// acknowledgments did not recover waits this long. The serving thread's
/// turn bounds how finely it is kept (about 15 ms on Windows).
pub(super) const RETRANSMIT: Duration = Duration::from_millis(20);

/// How long a forwarded connection's SYN waits for the guest's answer before
/// it is sent again. A guest still booting answers nothing for seconds, and
/// resending at [`RETRANSMIT`]'s pace would only fill its log.
const REDIAL: Duration = Duration::from_millis(200);

/// The most segments the gateway has unacknowledged at once, over all its
/// connections together: what the guest can hold. Ferrix's network driver
/// keeps 64 receive slots posted for every connection at once, since
/// 2026-10-01 (it was 16); more than this in flight and the rest are dropped
/// before the guest sees them, which the numbers at the top of this file
/// measure.
pub(super) const MAX_IN_FLIGHT: usize = 24;

/// How many acknowledgments of the same byte, with nothing new in them, mean a
/// segment was lost: RFC 5681's three.
const DUPLICATE_ACKS: u32 = 3;

/// How long a connect to the real destination may take before it is called
/// refused. Without a bound the thread making it outlives the boot.
pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a closed connection is kept, so that a retransmitted FIN is
/// acknowledged rather than reset.
const LINGER: Duration = Duration::from_secs(10);

/// How long a connection may say nothing before it is abandoned.
const IDLE: Duration = Duration::from_secs(300);

/// How many reads one connection may take from its host socket in one turn.
const READS_PER_TURN: usize = 16;

/// The distance between one connection's initial sequence number and the next's.
const ISS_STRIDE: u32 = 64_000;

/// The ports on `10.0.2.2` a forwarded connection comes from, taken in turn.
/// Linux's own ephemeral range, so the guest sees what a real peer would send.
pub(super) const FORWARD_PORTS: Range<u16> = 32_768..61_000;

/// A `--forward` port: the host's listener, and where on the guest what it
/// accepts goes.
#[derive(Debug)]
pub(super) struct Listener {
    /// Bound to the host's loopback, and non-blocking, so the serving thread
    /// can ask it every turn.
    socket: TcpListener,
    /// The guest's port.
    guest: u16,
}

impl Listener {
    /// Listen on the host's side of `forward`.
    pub(super) fn bind(forward: Forward) -> Result<Listener> {
        let socket = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, forward.host))
            .map_err(|error| {
                Error::new(format!(
                    "--forward {}:{}: could not listen on 127.0.0.1:{}: {error}",
                    forward.host, forward.guest, forward.host
                ))
            })?;
        socket.set_nonblocking(true)?;
        Ok(Listener {
            socket,
            guest: forward.guest,
        })
    }
}

/// What identifies a guest's TCP connection.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) struct Key {
    /// Where the guest connected from.
    guest: SocketAddrV4,
    /// Where it connected to, which is also where the host connection goes.
    seen: SocketAddrV4,
}

/// How far a connection has got.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// The guest's SYN has been seen and the host connection is being made.
    Connecting,
    /// A forwarded connection: the host's is accepted, and the SYN to the
    /// guest has gone without a SYN-ACK yet.
    Dialing,
    /// The SYN-ACK has gone out; the guest has not acknowledged it yet.
    Handshaking,
    /// Both ends are open.
    Open,
    /// Finished, and kept only so that a retransmission is answered.
    Closed,
}

/// The outcome of one connect attempt, sent back by the thread that made it.
#[derive(Debug)]
pub(super) struct Connected {
    /// Which connection it belongs to.
    key: Key,
    /// The stream, or why there is none.
    stream: std::io::Result<TcpStream>,
}

/// A segment the endpoint has decided to send.
#[derive(Debug)]
struct Outgoing {
    /// Its control flags.
    flags: Flags,
    /// Its sequence number.
    sequence: u32,
    /// What it acknowledges.
    acknowledgment: u32,
    /// What the gateway can still receive.
    window: u16,
    /// The maximum segment size option, on the SYN-ACK only.
    mss: Option<u16>,
    /// Its data.
    payload: Vec<u8>,
}

/// One guest connection and the host connection standing in for it.
#[derive(Debug)]
pub(super) struct Connection {
    /// How far it has got.
    state: State,
    /// The host connection, once there is one.
    stream: Option<TcpStream>,
    /// The initial send sequence number, so the handshake's ACK is recognised.
    iss: u32,
    /// The oldest byte the guest has not acknowledged.
    snd_una: u32,
    /// The next sequence number to send.
    snd_nxt: u32,
    /// Everything from `snd_una` onwards that the guest has not acknowledged,
    /// whether or not it has been sent yet.
    pending: Vec<u8>,
    /// What the guest says it can receive.
    snd_wnd: u16,
    /// The sequence number of this end's FIN, once the host has closed.
    fin: Option<u32>,
    /// Whether the SYN-ACK is still unacknowledged. Like the FIN, its sequence
    /// number is one the send buffer does not hold a byte for.
    syn_pending: bool,
    /// The host connection has no more to send.
    host_eof: bool,
    /// The host connection failed rather than closed, so the guest gets a reset.
    aborted: bool,
    /// The next sequence number expected from the guest.
    rcv_nxt: u32,
    /// The guest has sent its FIN.
    guest_fin: bool,
    /// Whether the host socket has been shut down for writing in consequence.
    host_shutdown: bool,
    /// Guest data not yet written to the host.
    inbound: Vec<u8>,
    /// The most this end puts in one segment.
    mss: usize,
    /// When something was last sent, for the retransmission timer.
    sent_at: Instant,
    /// How long the timer runs: [`RETRANSMIT`], from the gateway's `Core`.
    retransmit: Duration,
    /// How many acknowledgments in a row repeated `snd_una`.
    duplicate_acks: u32,
    /// The segment at `snd_una` is owed again, and only that one: three
    /// duplicate acknowledgments said it was lost, and the guest kept what
    /// came after it.
    resend_first: bool,
    /// When the guest was last heard from.
    heard_at: Instant,
}

impl Connection {
    /// A connection in the state a SYN leaves it: numbered, with no host
    /// connection yet.
    fn new(segment: &wire::Segment<'_>, iss: u32, retransmit: Duration) -> Connection {
        let offered = segment.header.options.mss.map_or(MIN_SEGMENT, usize::from);
        let now = Instant::now();
        Connection {
            state: State::Connecting,
            stream: None,
            iss,
            snd_una: iss,
            snd_nxt: iss,
            pending: Vec::new(),
            snd_wnd: segment.header.window,
            fin: None,
            syn_pending: false,
            host_eof: false,
            aborted: false,
            rcv_nxt: segment.header.sequence.wrapping_add(1),
            guest_fin: false,
            host_shutdown: false,
            inbound: Vec::new(),
            mss: offered.clamp(MIN_SEGMENT, MAX_SEGMENT),
            sent_at: now,
            retransmit,
            duplicate_acks: 0,
            resend_first: false,
            heard_at: now,
        }
    }

    /// A forwarded connection: the host's `stream` accepted, and the SYN that
    /// opens the guest's half put in `out`.
    fn dialing(
        stream: TcpStream,
        iss: u32,
        retransmit: Duration,
        out: &mut Vec<Outgoing>,
    ) -> Connection {
        let now = Instant::now();
        let mut connection = Connection {
            state: State::Dialing,
            stream: Some(stream),
            iss,
            snd_una: iss,
            snd_nxt: iss,
            pending: Vec::new(),
            snd_wnd: 0,
            fin: None,
            syn_pending: true,
            host_eof: false,
            aborted: false,
            rcv_nxt: 0,
            guest_fin: false,
            host_shutdown: false,
            inbound: Vec::new(),
            mss: MIN_SEGMENT,
            sent_at: now,
            retransmit,
            duplicate_acks: 0,
            resend_first: false,
            heard_at: now,
        };
        connection.send_syn(out);
        connection
    }

    /// The SYN of a forwarded connection, first or again.
    fn send_syn(&mut self, out: &mut Vec<Outgoing>) {
        let mss = u16::try_from(MAX_SEGMENT).unwrap_or(u16::MAX);
        out.push(Outgoing {
            flags: Flags::SYN,
            sequence: self.iss,
            acknowledgment: 0,
            window: self.window(),
            mss: Some(mss),
            payload: Vec::new(),
        });
        self.snd_nxt = self.iss.wrapping_add(1);
        self.sent_at = Instant::now();
    }

    /// What this end can still receive: the fixed window less what is still
    /// waiting to go to the host.
    fn window(&self) -> u16 {
        let held = u16::try_from(self.inbound.len()).unwrap_or(u16::MAX);
        RECEIVE_WINDOW.saturating_sub(held)
    }

    /// A segment carrying only an acknowledgment.
    fn ack(&self) -> Outgoing {
        Outgoing {
            flags: Flags::ACK,
            sequence: self.snd_nxt,
            acknowledgment: self.rcv_nxt,
            window: self.window(),
            mss: None,
            payload: Vec::new(),
        }
    }

    /// The host connection is up: answer the guest's SYN.
    fn accepted(&mut self, stream: TcpStream, out: &mut Vec<Outgoing>) {
        self.stream = Some(stream);
        self.state = State::Handshaking;
        self.syn_pending = true;
        let mss = u16::try_from(MAX_SEGMENT).unwrap_or(u16::MAX);
        out.push(Outgoing {
            flags: Flags::SYN.union(Flags::ACK),
            sequence: self.iss,
            acknowledgment: self.rcv_nxt,
            window: self.window(),
            mss: Some(mss),
            payload: Vec::new(),
        });
        self.snd_nxt = self.iss.wrapping_add(1);
        self.sent_at = Instant::now();
    }

    /// The host refused, or the connection has to be torn down: reset the guest.
    fn refuse(&mut self, out: &mut Vec<Outgoing>) {
        out.push(Outgoing {
            flags: Flags::RST.union(Flags::ACK),
            sequence: self.snd_nxt,
            acknowledgment: self.rcv_nxt,
            window: 0,
            mss: None,
            payload: Vec::new(),
        });
        self.close();
    }

    /// Forget the host connection and stop sending.
    fn close(&mut self) {
        self.stream = None;
        self.state = State::Closed;
        self.inbound.clear();
        self.pending.clear();
    }
}

/// The sequence number `a` comes after `b`, in the modulo-2³² order RFC 9293
/// numbers a connection in.
fn after(a: u32, b: u32) -> bool {
    let distance = a.wrapping_sub(b);
    distance != 0 && distance < 0x8000_0000
}

/// `length` as a sequence-space distance.
fn span(length: usize) -> u32 {
    u32::try_from(length).unwrap_or(u32::MAX)
}

impl Connection {
    /// Take one segment from the guest.
    fn on_segment(&mut self, segment: &wire::Segment<'_>, out: &mut Vec<Outgoing>) {
        let header = &segment.header;
        self.heard_at = Instant::now();
        if header.flags.contains(Flags::RST) {
            // The guest has given up on this connection. Dropping the host
            // stream closes it, which is what the far end should be told.
            self.close();
            return;
        }
        if self.state == State::Dialing {
            self.on_dial_answer(segment, out);
            return;
        }
        if header.flags.contains(Flags::SYN) && self.state == State::Connecting {
            // The SYN again, because the host connect has not finished. There
            // is nothing to answer with yet, and answering late is correct.
            return;
        }
        if header.flags.contains(Flags::ACK) {
            self.on_ack(header.acknowledgment, !segment.payload.is_empty());
        }
        self.snd_wnd = header.window;
        if self.state == State::Handshaking && self.snd_una != self.iss {
            self.state = State::Open;
        }
        self.receive(segment);
        if header.sequence_len(segment.payload.len()) > 0 {
            // Anything occupying sequence space is acknowledged, in order or
            // not: an acknowledgment of `rcv_nxt` is exactly the request to
            // send the missing piece again.
            out.push(self.ack());
        }
    }

    /// The guest's answer to a forwarded connection's SYN.
    ///
    /// A SYN-ACK of this end's SYN opens the connection, and is acknowledged.
    /// Anything else is not an answer: the SYN goes again on its timer, and a
    /// guest with nothing listening has already said so with a reset, which
    /// [`Connection::on_segment`] handles before this.
    fn on_dial_answer(&mut self, segment: &wire::Segment<'_>, out: &mut Vec<Outgoing>) {
        let header = &segment.header;
        let answers = header.flags.contains(Flags::SYN.union(Flags::ACK))
            && header.acknowledgment == self.iss.wrapping_add(1);
        if !answers {
            return;
        }
        self.rcv_nxt = header.sequence.wrapping_add(1);
        self.snd_wnd = header.window;
        self.mss = header
            .options
            .mss
            .map_or(MIN_SEGMENT, usize::from)
            .clamp(MIN_SEGMENT, MAX_SEGMENT);
        // Retires the SYN's sequence number, which is `syn_pending`'s job.
        self.on_ack(header.acknowledgment, false);
        self.state = State::Open;
        out.push(self.ack());
    }

    /// Act on an acknowledgment: release what it covers and restart the timer.
    ///
    /// An acknowledgment of something never sent, or of something already
    /// acknowledged, is ignored rather than trusted: `snd_una` must only ever
    /// move forwards, and only as far as `snd_nxt`.
    ///
    /// An acknowledgment of `snd_una` again, carrying no data, while something
    /// is outstanding is a duplicate; the third in a row resends from
    /// `snd_una` straight away, as fast retransmit does.
    fn on_ack(&mut self, acknowledgment: u32, carries_data: bool) {
        let acked = acknowledgment.wrapping_sub(self.snd_una);
        let outstanding = self.snd_nxt.wrapping_sub(self.snd_una);
        if acked == 0 {
            if outstanding > 0 && !carries_data {
                self.duplicate_acks += 1;
                if self.duplicate_acks == DUPLICATE_ACKS {
                    self.resend_first = true;
                    self.sent_at = Instant::now();
                }
            }
            return;
        }
        if acked > outstanding {
            return;
        }
        self.duplicate_acks = 0;
        let mut covered = usize::try_from(acked).unwrap_or(0);
        if self.syn_pending {
            // The SYN-ACK's sequence number is the first one this end ever
            // sends, and there is no byte in the buffer behind it. Counting it
            // as data would throw the first byte of the response away, and the
            // guest would never see it: it is acknowledged, so it is never
            // sent again.
            covered = covered.saturating_sub(1);
            self.syn_pending = false;
        }
        // The FIN's sequence number is not in the buffer either, which is why
        // this is a minimum rather than an assertion.
        let bytes = covered.min(self.pending.len());
        let _ = self.pending.drain(..bytes);
        self.snd_una = acknowledgment;
        self.sent_at = Instant::now();
    }

    /// Take a segment's data and its FIN, if they are the ones expected next.
    fn receive(&mut self, segment: &wire::Segment<'_>) {
        if self.state == State::Connecting || self.state == State::Closed {
            return;
        }
        let sequence = segment.header.sequence;
        if !segment.payload.is_empty() && sequence == self.rcv_nxt {
            self.inbound.extend_from_slice(segment.payload);
            self.rcv_nxt = self.rcv_nxt.wrapping_add(span(segment.payload.len()));
        }
        let after_data = sequence.wrapping_add(span(segment.payload.len()));
        if segment.header.flags.contains(Flags::FIN) && after_data == self.rcv_nxt {
            self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
            self.guest_fin = true;
        }
    }

    /// The first half of a turn: move bytes each way, run the timers, and
    /// send what owes nothing to the gateway's budget of segments in flight
    /// -- a reset, a SYN again, the one segment three duplicate
    /// acknowledgments asked for, which is in flight already.
    fn prepare(&mut self, out: &mut Vec<Outgoing>) {
        if self.state == State::Dialing {
            self.redial(out);
            return;
        }
        self.write_to_host();
        self.read_from_host();
        self.expire_retransmit();
        if self.state != State::Open {
            return;
        }
        if self.aborted {
            self.refuse(out);
            return;
        }
        if self.host_eof && self.fin.is_none() {
            self.fin = Some(self.snd_una.wrapping_add(span(self.pending.len())));
        }
        if std::mem::take(&mut self.resend_first) {
            self.resend_una(out);
        }
    }

    /// How many segments this connection has unacknowledged on the wire.
    fn in_flight(&self) -> usize {
        let outstanding = usize::try_from(self.snd_nxt.wrapping_sub(self.snd_una)).unwrap_or(0);
        outstanding.div_ceil(self.mss.max(1))
    }

    /// Send the next segment of data the guest's window has room for, and
    /// say whether there was one.
    fn push_next(&mut self, out: &mut Vec<Outgoing>) -> bool {
        if self.state != State::Open || self.aborted {
            return false;
        }
        self.push_one(self.send_window(), out)
    }

    /// The last of a turn: the FIN, once every byte before it has gone.
    fn finish_turn(&mut self, out: &mut Vec<Outgoing>) {
        if self.state != State::Open || self.aborted {
            return;
        }
        self.push_fin(self.send_window(), out);
    }

    /// What may be outstanding to the guest: its window, and no more than
    /// [`MAX_IN_FLIGHT`] segments of it.
    ///
    /// A window of zero stops everything, which is what it is for. The
    /// retransmission timer doubles as the persist timer: whatever is
    /// outstanding goes again, and its acknowledgment carries the guest's
    /// new window. With nothing outstanding there is nothing to probe with
    /// and nothing to lose by waiting for the guest's own window update.
    /// The guest's window says what its socket has room for; what its
    /// driver can take off the wire is less, and is the bound here.
    fn send_window(&self) -> usize {
        usize::from(self.snd_wnd).min(MAX_IN_FLIGHT * self.mss)
    }

    /// Send a forwarded connection's SYN again if it has gone unanswered for
    /// [`REDIAL`], and give up after [`CONNECT_TIMEOUT`]: a guest that is
    /// still booting answers nothing at all, and closing the host's connection
    /// is how its client finds that out.
    fn redial(&mut self, out: &mut Vec<Outgoing>) {
        if self.heard_at.elapsed() > CONNECT_TIMEOUT {
            self.close();
        } else if self.sent_at.elapsed() >= REDIAL {
            self.send_syn(out);
        }
    }

    /// Give the host whatever the guest has sent, and close that direction when
    /// the guest has closed it.
    fn write_to_host(&mut self) {
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        while !self.inbound.is_empty() {
            match stream.write(&self.inbound) {
                Ok(0) => break,
                Ok(written) => {
                    let _ = self.inbound.drain(..written);
                }
                Err(_) => break,
            }
        }
        if self.guest_fin && self.inbound.is_empty() && !self.host_shutdown {
            let _ = stream.shutdown(Shutdown::Write);
            self.host_shutdown = true;
        }
    }

    /// Take whatever the host has for the guest, up to [`SEND_CAPACITY`].
    ///
    /// Stopping there is the flow control in the guest's direction: an unread
    /// host socket fills, and the server on the far end of it blocks, rather
    /// than this process growing a buffer until it is the problem.
    fn read_from_host(&mut self) {
        if self.host_eof || self.pending.len() >= SEND_CAPACITY {
            return;
        }
        let Some(stream) = self.stream.as_mut() else {
            return;
        };
        let mut buffer = [0_u8; MAX_SEGMENT];
        for _ in 0..READS_PER_TURN {
            match stream.read(&mut buffer) {
                Ok(0) => {
                    self.host_eof = true;
                    break;
                }
                Ok(len) => match buffer.get(..len) {
                    Some(data) => self.pending.extend_from_slice(data),
                    None => break,
                },
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                // A reset from the far end, or anything else that is not "no
                // data yet". The guest is told the truth: a reset, not a clean
                // close it would read as a complete response.
                Err(_) => {
                    self.aborted = true;
                    break;
                }
            }
            if self.pending.len() >= SEND_CAPACITY {
                break;
            }
        }
    }

    /// Send everything unacknowledged again, if the timer has run out.
    ///
    /// Winding `snd_nxt` back to `snd_una` is the whole of it: the next
    /// [`Connection::push_next`] then walks the same bytes, and the FIN after
    /// them, in the same order and with the same sequence numbers.
    fn expire_retransmit(&mut self) {
        if self.snd_nxt == self.snd_una || self.sent_at.elapsed() < self.retransmit {
            return;
        }
        self.snd_nxt = self.snd_una;
        self.sent_at = Instant::now();
    }

    /// Send the first unacknowledged segment again, and nothing after it.
    fn resend_una(&self, out: &mut Vec<Outgoing>) {
        let outstanding = usize::try_from(self.snd_nxt.wrapping_sub(self.snd_una)).unwrap_or(0);
        let len = self.pending.len().min(self.mss).min(outstanding);
        let Some(payload) = self.pending.get(..len).filter(|bytes| !bytes.is_empty()) else {
            return;
        };
        out.push(Outgoing {
            flags: Flags::ACK.union(Flags::PSH),
            sequence: self.snd_una,
            acknowledgment: self.rcv_nxt,
            window: self.window(),
            mss: None,
            payload: payload.to_vec(),
        });
    }

    /// Send one segment of data, and say whether there was one to send.
    fn push_one(&mut self, window: usize, out: &mut Vec<Outgoing>) -> bool {
        let sent = usize::try_from(self.snd_nxt.wrapping_sub(self.snd_una)).unwrap_or(0);
        let Some(room) = window.checked_sub(sent).filter(|room| *room > 0) else {
            return false;
        };
        let Some(unsent) = self.pending.get(sent..).filter(|rest| !rest.is_empty()) else {
            return false;
        };
        let len = unsent.len().min(self.mss).min(room);
        let Some(payload) = unsent.get(..len) else {
            return false;
        };
        out.push(Outgoing {
            // PSH on every one: there is nothing to gain by making the guest
            // wait, and a receiver that ignores it loses nothing either.
            flags: Flags::ACK.union(Flags::PSH),
            sequence: self.snd_nxt,
            acknowledgment: self.rcv_nxt,
            window: self.window(),
            mss: None,
            payload: payload.to_vec(),
        });
        self.snd_nxt = self.snd_nxt.wrapping_add(span(len));
        self.sent_at = Instant::now();
        true
    }

    /// Send the FIN, once every byte before it has gone.
    fn push_fin(&mut self, window: usize, out: &mut Vec<Outgoing>) {
        let sent = usize::try_from(self.snd_nxt.wrapping_sub(self.snd_una)).unwrap_or(0);
        if self.fin != Some(self.snd_nxt) || sent >= window {
            return;
        }
        out.push(Outgoing {
            flags: Flags::ACK.union(Flags::FIN),
            sequence: self.snd_nxt,
            acknowledgment: self.rcv_nxt,
            window: self.window(),
            mss: None,
            payload: Vec::new(),
        });
        self.snd_nxt = self.snd_nxt.wrapping_add(1);
        self.sent_at = Instant::now();
    }

    /// Close the connection once both directions have finished, and say whether
    /// the entry can now be forgotten.
    fn settle(&mut self) -> bool {
        let ours_acked = self.fin.is_some_and(|fin| after(self.snd_una, fin));
        if self.state != State::Closed && self.guest_fin && ours_acked {
            self.close();
        }
        match self.state {
            State::Closed => self.heard_at.elapsed() > LINGER,
            _ => self.heard_at.elapsed() > IDLE,
        }
    }
}

impl Core {
    /// Take one TCP segment from the guest.
    pub(super) fn on_tcp(
        &mut self,
        source: Ipv4Addr,
        destination: Ipv4Addr,
        bytes: &[u8],
    ) -> Result<()> {
        let pseudo = Pseudo::V4 {
            source: source.octets(),
            destination: destination.octets(),
        };
        let segment = wire::Header::parse(bytes, pseudo)?;
        let key = Key {
            guest: SocketAddrV4::new(source, segment.header.source_port),
            seen: SocketAddrV4::new(destination, segment.header.destination_port),
        };
        let mut out = Vec::new();
        match self.tcp.get_mut(&key) {
            Some(connection) => connection.on_segment(&segment, &mut out),
            None => self.open(key, &segment, &mut out),
        }
        for reply in &out {
            self.send_outgoing(&key, reply)?;
        }
        Ok(())
    }

    /// A segment for a connection that does not exist: start one if it is a
    /// SYN, and reset the guest if it is anything else.
    fn open(&mut self, key: Key, segment: &wire::Segment<'_>, out: &mut Vec<Outgoing>) {
        if !segment.header.flags.contains(Flags::SYN) || segment.header.flags.contains(Flags::ACK) {
            out.push(stray_reset(segment));
            return;
        }
        let iss = self.next_iss;
        self.next_iss = self.next_iss.wrapping_add(ISS_STRIDE);
        let _ = self
            .tcp
            .insert(key, Connection::new(segment, iss, self.retransmit));
        self.start_connect(key);
    }

    /// Open the host connection this one stands in for, on a thread of its own.
    ///
    /// A thread rather than a non-blocking connect, because `std` offers no way
    /// to ask whether one has finished: the answer is a writability poll this
    /// build tool would have to reimplement per platform. A thread that blocks
    /// on a connect for at most [`CONNECT_TIMEOUT`] is the whole cost, and a
    /// guest opening connections faster than that is not a case that exists.
    fn start_connect(&self, key: Key) {
        let sender = self.connected.0.clone();
        let _ = std::thread::Builder::new()
            .name("ferrix-net-connect".to_owned())
            .spawn(move || {
                let target = SocketAddr::V4(super::host_of(key.seen));
                let stream = TcpStream::connect_timeout(&target, CONNECT_TIMEOUT);
                let _ = sender.send(Connected { key, stream });
            });
    }

    /// Answer the connects that have finished since the last turn.
    pub(super) fn poll_tcp(&mut self) {
        self.accept_forwards();
        let mut finished = Vec::new();
        while let Ok(connected) = self.connected.1.try_recv() {
            finished.push(connected);
        }
        for Connected { key, stream } in finished {
            self.settle_connect(key, stream);
        }
        self.service_tcp();
    }

    /// Take every connection waiting on a `--forward` port, and open each to
    /// the guest.
    fn accept_forwards(&mut self) {
        let mut accepted = Vec::new();
        for listener in &self.listeners {
            while let Ok((stream, _)) = listener.socket.accept() {
                accepted.push((stream, listener.guest));
            }
        }
        for (stream, guest) in accepted {
            self.dial(stream, guest);
        }
    }

    /// Open a connection to the guest's `guest` port for one accepted on the
    /// host, from the next free port of [`FORWARD_PORTS`].
    ///
    /// Refused -- the host's connection dropped, which closes it -- when the
    /// guest has sent nothing yet: without its MAC there is nowhere to send a
    /// SYN, and a guest that has not configured its network has no server
    /// listening either.
    fn dial(&mut self, stream: TcpStream, guest: u16) {
        let ready = self.guest_mac.is_some()
            && stream.set_nonblocking(true).is_ok()
            && stream.set_nodelay(true).is_ok();
        let mut free = None;
        if ready {
            for _ in FORWARD_PORTS {
                let key = Key {
                    guest: SocketAddrV4::new(GUEST_IP, guest),
                    seen: SocketAddrV4::new(GATEWAY_IP, self.take_forward_port()),
                };
                if !self.tcp.contains_key(&key) {
                    free = Some(key);
                    break;
                }
            }
        }
        let Some(key) = free else {
            bump(&self.counters.tcp_refused);
            return;
        };
        let iss = self.next_iss;
        self.next_iss = self.next_iss.wrapping_add(ISS_STRIDE);
        let mut out = Vec::new();
        let _ = self.tcp.insert(
            key,
            Connection::dialing(stream, iss, self.retransmit, &mut out),
        );
        bump(&self.counters.tcp_forwarded);
        for segment in &out {
            let _ = self.send_outgoing(&key, segment);
        }
    }

    /// The next port of [`FORWARD_PORTS`], in turn.
    fn take_forward_port(&mut self) -> u16 {
        let port = self.next_forward_port;
        self.next_forward_port = match port.checked_add(1) {
            Some(next) if FORWARD_PORTS.contains(&next) => next,
            _ => FORWARD_PORTS.start,
        };
        port
    }

    /// Hand one finished connect to its connection, and count whether it opened.
    fn settle_connect(&mut self, key: Key, stream: std::io::Result<TcpStream>) {
        let mut out = Vec::new();
        let opened = {
            let Some(connection) = self.tcp.get_mut(&key) else {
                // The guest reset it, or it timed out, while the connect was in
                // flight. Dropping the stream closes it.
                bump(&self.counters.tcp_refused);
                return;
            };
            // Nagle off: this end already batches by the guest's window, and
            // waiting for an acknowledgment before sending a short request is
            // exactly the delay a relay must not add.
            match stream.and_then(|stream| {
                stream.set_nonblocking(true)?;
                stream.set_nodelay(true)?;
                Ok(stream)
            }) {
                Ok(stream) => {
                    connection.accepted(stream, &mut out);
                    true
                }
                Err(_) => {
                    connection.refuse(&mut out);
                    false
                }
            }
        };
        // Counted before the answer goes, so that whoever reads the counters
        // on seeing the SYN-ACK or the reset reads a count that includes it.
        bump(if opened {
            &self.counters.tcp_opened
        } else {
            &self.counters.tcp_refused
        });
        for reply in &out {
            let _ = self.send_outgoing(&key, reply);
        }
    }

    /// Give every connection its turn, then send what they produced.
    ///
    /// [`MAX_IN_FLIGHT`] bounds the whole gateway, not each connection: the
    /// guest's driver has the same few receive buffers posted whichever
    /// connection a frame is for, and Steam downloading through a dozen
    /// connections at once, eight in flight on each, had most dropped and
    /// sent again on the timer (2026-10-01: 0.01 to 0.3 Mbps where one
    /// connection makes 4 MB/s). What room there is goes a segment at a time
    /// to each connection in turn, starting from a different one each turn,
    /// so that none waits behind another.
    fn service_tcp(&mut self) {
        let mut out = Vec::new();
        let keys: Vec<Key> = self.tcp.keys().copied().collect();
        let first = self.tcp_turn % keys.len().max(1);
        self.tcp_turn = self.tcp_turn.wrapping_add(1);
        let order: Vec<Key> = keys
            .iter()
            .cycle()
            .skip(first)
            .take(keys.len())
            .copied()
            .collect();
        for key in &order {
            self.each_connection(*key, &mut out, Connection::prepare);
        }
        let in_flight: usize = self.tcp.values().map(Connection::in_flight).sum();
        let mut room = MAX_IN_FLIGHT.saturating_sub(in_flight);
        while room > 0 && self.one_round(&order, &mut room, &mut out) {}
        for key in &order {
            self.each_connection(*key, &mut out, Connection::finish_turn);
        }
        for (key, segment) in &out {
            let _ = self.send_outgoing(key, segment);
        }
    }

    /// Run `step` on the connection `key`, and keep what it sends in `out`.
    fn each_connection(
        &mut self,
        key: Key,
        out: &mut Vec<(Key, Outgoing)>,
        step: fn(&mut Connection, &mut Vec<Outgoing>),
    ) {
        let Some(connection) = self.tcp.get_mut(&key) else {
            return;
        };
        let mut segments = Vec::new();
        step(connection, &mut segments);
        out.extend(segments.into_iter().map(|segment| (key, segment)));
    }

    /// Offer each connection in `order` one segment of `room`, and say
    /// whether any took one.
    fn one_round(
        &mut self,
        order: &[Key],
        room: &mut usize,
        out: &mut Vec<(Key, Outgoing)>,
    ) -> bool {
        let mut any = false;
        for key in order {
            if *room == 0 {
                break;
            }
            let Some(connection) = self.tcp.get_mut(key) else {
                continue;
            };
            let mut segments = Vec::new();
            if connection.push_next(&mut segments) {
                *room -= 1;
                any = true;
            }
            out.extend(segments.into_iter().map(|segment| (*key, segment)));
        }
        any
    }

    /// Write one segment out as an IPv4 packet to the guest.
    fn send_outgoing(&self, key: &Key, out: &Outgoing) -> Result<()> {
        let source = *key.seen.ip();
        let destination = *key.guest.ip();
        let pseudo = Pseudo::V4 {
            source: source.octets(),
            destination: destination.octets(),
        };
        let header = wire::Header {
            source_port: key.seen.port(),
            destination_port: key.guest.port(),
            sequence: out.sequence,
            acknowledgment: out.acknowledgment,
            flags: out.flags,
            window: out.window,
            urgent_pointer: 0,
            options: wire::Options {
                mss: out.mss,
                ..wire::Options::default()
            },
        };
        let mut bytes = [0_u8; MTU];
        let len = header.emit(&out.payload, pseudo, &mut bytes)?;
        self.send_ipv4(
            ipv4::protocol::TCP,
            source,
            destination,
            bytes.get(..len).ok_or(ferrix_netwire::Error::NoSpace)?,
        )
    }

    /// Close what has finished, and forget what has been closed long enough.
    pub(super) fn expire_tcp(&mut self) {
        self.tcp.retain(|_, connection| !connection.settle());
    }
}

/// The reset RFC 9293 section 3.10.7.1 prescribes for a segment arriving at a
/// closed port: acknowledging one that carried an acknowledgment would be
/// telling the sender its numbering is agreed, so that case answers in its own
/// sequence space instead.
fn stray_reset(segment: &wire::Segment<'_>) -> Outgoing {
    let header = &segment.header;
    if header.flags.contains(Flags::ACK) {
        Outgoing {
            flags: Flags::RST,
            sequence: header.acknowledgment,
            acknowledgment: 0,
            window: 0,
            mss: None,
            payload: Vec::new(),
        }
    } else {
        Outgoing {
            flags: Flags::RST.union(Flags::ACK),
            sequence: 0,
            acknowledgment: header
                .sequence
                .wrapping_add(span(header.sequence_len(segment.payload.len()))),
            window: 0,
            mss: None,
            payload: Vec::new(),
        }
    }
}
