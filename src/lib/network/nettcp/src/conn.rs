//! A connection: its counters, its buffers, and what a program does to it.
//!
//! The segment-by-segment behaviour is in `crate::input` and
//! `crate::output`; this module holds the state those two work on, the
//! constructors, and the calls a socket layer makes -- write, read, close,
//! abort -- none of which touch the wire.

use crate::congestion::Control;
use crate::ring::{ByteQueue, Reassembly};
use crate::rtt::{Estimator, Millis};
use crate::seq::SeqNumber;
use crate::state::{Failure, State};

/// The default segment size for IPv4 when the peer names none: RFC 9293's 536.
pub const DEFAULT_MSS: u16 = 536;

/// The largest segment this implementation will send, whatever a peer offers.
///
/// A peer may advertise an MSS larger than any path will carry; the standard
/// lets the sender clamp it, and this is the clamp. 1460 is Ethernet's payload
/// less the two headers, which is what every path Ferrix will meet supports.
pub const MAX_MSS: u16 = 1460;

/// The most a window may be scaled by: RFC 7323's ceiling of 14.
pub const MAX_WINDOW_SCALE: u8 = 14;

/// How long a connection stays in `TIME-WAIT`, which is twice the segment
/// lifetime Linux assumes.
pub const TIME_WAIT: Millis = 60_000;

/// How long an acknowledgment may be held back in the hope of carrying data
/// with it. Linux's `TCP_DELACK_MIN`.
pub const DELAYED_ACK: Millis = 40;

/// How long `FIN-WAIT-2` waits for a peer that has stopped closing, which is
/// Linux's `tcp_fin_timeout`.
pub const FIN_WAIT_2_TIMEOUT: Millis = 60_000;

/// How many times a segment is retransmitted before the connection is given
/// up, which is Linux's `tcp_retries2`.
pub const MAX_RETRANSMITS: u32 = 15;

/// How many times a connection request is retransmitted before it is given up,
/// Linux's `tcp_syn_retries`.
pub const MAX_SYN_RETRANSMITS: u32 = 6;

/// What a connection is built with.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// How many bytes may wait to be sent.
    pub send_capacity: usize,
    /// How many bytes may wait to be read, which is also the window this end
    /// advertises.
    ///
    /// 1 MiB by default, up from 64 KiB on 2026-10-15: a program that reads
    /// in bursts, busy elsewhere between them as a download written to a
    /// disk is, leaves the sender stopped once this is full, and 64 KiB is
    /// under a millisecond of the gateway's rate. A burst reader on
    /// `x86_64` under KVM (1 MiB read, then 15 ms away, 32 MiB) took a
    /// median 2.0 s at 64 KiB (best 1.10), 1.9 s at 256 KiB (best 1.72, a
    /// plateau not explained yet) and 1.05 s at 1 MiB (best 0.84), 20 to 24
    /// runs each on a loaded host. The buffer grows only as
    /// data waits, so an idle connection costs what it did. The worst case
    /// per connection is this for the receive queue and this again for the
    /// reassembly list, both charged to the connection's job (F-37).
    pub receive_capacity: usize,
    /// The largest segment this end will accept, advertised in the handshake.
    pub segment_size: u16,
    /// The scale this end asks for on its own window.
    pub window_scale: u8,
    /// Whether to offer selective acknowledgment.
    pub selective_ack: bool,
    /// Whether to send data as soon as there is any, rather than waiting for a
    /// full segment or an acknowledgment. This is `TCP_NODELAY`.
    pub no_delay: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            send_capacity: 64 * 1024,
            receive_capacity: 1024 * 1024,
            segment_size: MAX_MSS,
            window_scale: 7,
            selective_ack: true,
            no_delay: false,
        }
    }
}

/// What changed for a program waiting on this connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Bytes became readable, or the peer closed and a read will now answer.
    pub readable: bool,
    /// Room to write appeared, or the connection finished opening.
    pub writable: bool,
    /// The connection reached a state from which nothing more will happen.
    pub closed: bool,
}

impl Progress {
    /// Whether anything at all changed.
    #[must_use]
    pub const fn is_anything(self) -> bool {
        self.readable || self.writable || self.closed
    }

    /// The two progresses together.
    #[must_use]
    pub const fn or(self, other: Progress) -> Progress {
        Progress {
            readable: self.readable || other.readable,
            writable: self.writable || other.writable,
            closed: self.closed || other.closed,
        }
    }
}

/// One TCP connection.
#[derive(Debug)]
pub struct Connection {
    /// Where the connection is in its life.
    pub(crate) state: State,
    /// Why it stopped, once it has.
    pub(crate) failure: Option<Failure>,
    /// This end's port.
    pub(crate) local_port: u16,
    /// The peer's port.
    pub(crate) remote_port: u16,

    /// The first sequence number this end chose.
    pub(crate) iss: SeqNumber,
    /// The oldest byte sent and not acknowledged.
    pub(crate) snd_una: SeqNumber,
    /// The next byte to send.
    pub(crate) snd_nxt: SeqNumber,
    /// The highest sequence number ever sent, which retransmission rewinds
    /// behind and fast recovery ends at.
    pub(crate) snd_max: SeqNumber,
    /// What the peer says it has room for, already unscaled.
    pub(crate) snd_wnd: u32,
    /// The sequence number of the segment that last updated the send window.
    pub(crate) snd_wl1: SeqNumber,
    /// The acknowledgment of the segment that last updated the send window.
    pub(crate) snd_wl2: SeqNumber,
    /// The largest segment this end will send.
    pub(crate) snd_mss: u16,
    /// The scale the peer asked for on its window.
    pub(crate) snd_wscale: u8,

    /// The peer's first sequence number.
    pub(crate) irs: SeqNumber,
    /// The next byte expected from the peer.
    pub(crate) rcv_nxt: SeqNumber,
    /// The scale this end asked for on its own window.
    pub(crate) rcv_wscale: u8,
    /// The largest segment this end advertised it would accept.
    pub(crate) rcv_mss: u16,

    /// Bytes written by the program and not yet acknowledged.
    pub(crate) send: ByteQueue,
    /// Bytes received in order and not yet read.
    pub(crate) recv: ByteQueue,
    /// Bytes received out of order.
    pub(crate) holes: Reassembly,

    /// The round-trip estimate and the timeout it gives.
    pub(crate) rtt: Estimator,
    /// The congestion window.
    pub(crate) congestion: Control,
    /// When the segment being timed was sent, and what it ended at.
    pub(crate) timed: Option<(Millis, SeqNumber)>,

    /// When to retransmit, if anything is outstanding.
    pub(crate) retransmit_at: Option<Millis>,
    /// When to send an acknowledgment that is being held back.
    pub(crate) delayed_ack_at: Option<Millis>,
    /// When `TIME-WAIT` or `FIN-WAIT-2` ends.
    pub(crate) linger_at: Option<Millis>,
    /// When to probe a window the peer has closed.
    pub(crate) persist_at: Option<Millis>,

    /// Whether a `FIN` has been queued behind the unsent data.
    pub(crate) fin_queued: bool,
    /// Whether the `FIN` has been sent, and the sequence number it took.
    pub(crate) fin_seq: Option<SeqNumber>,
    /// Whether the peer's `FIN` has been received.
    pub(crate) fin_received: bool,
    /// Whether a reset is owed to the peer.
    pub(crate) reset_pending: bool,
    /// Whether an acknowledgment must be sent at the next opportunity rather
    /// than held back.
    pub(crate) ack_immediately: bool,
    /// Segments of data taken in order since the last acknowledgment went.
    /// The second one is acknowledged at once: RFC 9293 section 3.8.6.3 says
    /// a receiver SHOULD acknowledge at least every second full-sized segment,
    /// and holding every acknowledgment for [`DELAYED_ACK`] leaves a sender
    /// waiting on a window it has already filled.
    pub(crate) unacknowledged_segments: u32,
    /// Whether a window probe is owed because the peer's window is shut.
    pub(crate) probe_pending: bool,
    /// How many probes have gone into a shut window without it opening.
    pub(crate) probes: u32,

    /// Whether the peer offered selective acknowledgment.
    pub(crate) sack_permitted: bool,
    /// What the connection was built with.
    pub(crate) config: Config,
}

impl Connection {
    /// The shared part of every constructor.
    fn blank(config: Config, local_port: u16, remote_port: u16, iss: SeqNumber) -> Connection {
        let segment = u32::from(config.segment_size.clamp(1, MAX_MSS));
        Connection {
            state: State::Closed,
            failure: None,
            local_port,
            remote_port,
            iss,
            snd_una: iss,
            snd_nxt: iss,
            snd_max: iss,
            snd_wnd: 0,
            snd_wl1: iss,
            snd_wl2: iss,
            snd_mss: DEFAULT_MSS,
            snd_wscale: 0,
            irs: SeqNumber(0),
            rcv_nxt: SeqNumber(0),
            rcv_wscale: config.window_scale.min(MAX_WINDOW_SCALE),
            rcv_mss: config.segment_size.clamp(1, MAX_MSS),
            send: ByteQueue::with_capacity(config.send_capacity),
            recv: ByteQueue::with_capacity(config.receive_capacity),
            holes: Reassembly::with_capacity(config.receive_capacity),
            rtt: Estimator::new(),
            congestion: Control::new(segment),
            timed: None,
            retransmit_at: None,
            delayed_ack_at: None,
            linger_at: None,
            persist_at: None,
            fin_queued: false,
            fin_seq: None,
            fin_received: false,
            reset_pending: false,
            ack_immediately: false,
            unacknowledged_segments: 0,
            probe_pending: false,
            probes: 0,
            sack_permitted: false,
            config,
        }
    }

    /// Start an outgoing connection: the `SYN` is sent by the next
    /// [`Connection::poll_transmit`].
    #[must_use]
    pub fn connect(
        config: Config,
        local_port: u16,
        remote_port: u16,
        iss: SeqNumber,
    ) -> Connection {
        let mut connection = Connection::blank(config, local_port, remote_port, iss);
        connection.state = State::SynSent;
        connection
    }

    /// Answer a connection request: the `SYN, ACK` is sent by the next
    /// [`Connection::poll_transmit`].
    ///
    /// `peer_seq` is the request's sequence number, and the four options are
    /// what it offered. A listener parses them once and hands them here rather
    /// than keeping the segment.
    #[must_use]
    pub fn accept(
        config: Config,
        local_port: u16,
        remote_port: u16,
        iss: SeqNumber,
        request: &Request,
    ) -> Connection {
        let mut connection = Connection::blank(config, local_port, remote_port, iss);
        connection.state = State::SynReceived;
        connection.irs = request.sequence;
        connection.rcv_nxt = request.sequence.advance(1);
        connection.snd_wnd = u32::from(request.window);
        connection.snd_wl1 = request.sequence;
        connection.adopt_options(request.mss, request.window_scale, request.selective_ack);
        connection
    }

    /// Take on what a peer's `SYN` offered.
    pub(crate) fn adopt_options(
        &mut self,
        mss: Option<u16>,
        window_scale: Option<u8>,
        selective_ack: bool,
    ) {
        self.snd_mss = mss.unwrap_or(DEFAULT_MSS).clamp(1, MAX_MSS);
        self.congestion.set_segment(u32::from(self.snd_mss));
        match window_scale {
            Some(scale) => {
                self.snd_wscale = scale.min(MAX_WINDOW_SCALE);
            }
            // RFC 7323: a window scale is offered only in answer to one, so if
            // the peer did not send the option neither end scales.
            None => {
                self.snd_wscale = 0;
                self.rcv_wscale = 0;
            }
        }
        self.sack_permitted = selective_ack && self.config.selective_ack;
    }

    /// Where the connection is.
    #[must_use]
    pub const fn state(&self) -> State {
        self.state
    }

    /// Why it stopped, if it stopped badly.
    #[must_use]
    pub const fn failure(&self) -> Option<Failure> {
        self.failure
    }

    /// This end's port.
    #[must_use]
    pub const fn local_port(&self) -> u16 {
        self.local_port
    }

    /// The peer's port.
    #[must_use]
    pub const fn remote_port(&self) -> u16 {
        self.remote_port
    }

    /// How many bytes are waiting to be read.
    #[must_use]
    pub fn receive_queued(&self) -> usize {
        self.recv.len()
    }

    /// How many bytes the program has written that are not yet acknowledged.
    #[must_use]
    pub fn send_queued(&self) -> usize {
        self.send.len()
    }

    /// How many bytes have been sent and not acknowledged.
    #[must_use]
    pub fn in_flight(&self) -> u32 {
        self.snd_nxt.distance_from(self.snd_una)
    }

    /// The largest segment this end sends.
    #[must_use]
    pub const fn segment_size(&self) -> u16 {
        self.snd_mss
    }

    /// What the connection was built with.
    #[must_use]
    pub const fn config(&self) -> Config {
        self.config
    }

    /// Turn Nagle's algorithm off or on, which is `TCP_NODELAY`.
    pub const fn set_no_delay(&mut self, off: bool) {
        self.config.no_delay = off;
    }

    /// The congestion window, for `/proc/net/tcp` and for tests.
    #[must_use]
    pub const fn congestion_window(&self) -> u32 {
        self.congestion.window()
    }

    /// The smoothed round-trip time, once one has been measured.
    #[must_use]
    pub const fn round_trip(&self) -> Option<Millis> {
        self.rtt.smoothed()
    }

    /// Whether a read would answer at once: there are bytes, or there will
    /// never be any.
    #[must_use]
    pub fn is_readable(&self) -> bool {
        !self.recv.is_empty() || self.fin_received || !self.state.can_receive()
    }

    /// Whether a write would take at least one byte.
    #[must_use]
    pub fn is_writable(&self) -> bool {
        self.state.can_send() && self.send.free() > 0
    }

    /// Whether nothing more will ever happen.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.state == State::Closed
    }

    /// Charge what its queues hold, and their growth, to `owner`: the job of
    /// the program that made it, or of the listener it arrived on
    /// (certification finding F-37).
    ///
    /// # Errors
    ///
    /// [`ferrix_kmem::Refused`] past that job's limit.
    pub fn charge_to(&mut self, owner: u32) -> Result<(), ferrix_kmem::Refused> {
        self.send.charge_to(owner)?;
        self.recv.charge_to(owner)?;
        self.holes.charge_to(owner)
    }

    /// Whether the last write stopped short because the send queue's job
    /// could not be charged for it to grow.
    #[must_use]
    pub fn write_refused(&self) -> bool {
        self.send.refused()
    }

    /// Queue `data` to be sent, and say how much was taken.
    ///
    /// Nothing is sent here: the bytes join the send queue and
    /// [`Connection::poll_transmit`] takes them when the windows allow.
    pub fn write(&mut self, data: &[u8]) -> usize {
        if !self.state.can_send() {
            return 0;
        }
        self.send.write(data)
    }

    /// Take received bytes, and say how many were taken.
    ///
    /// Taking bytes opens the receive window, so a caller that reads should
    /// then look at [`Connection::poll_transmit`] for the window update.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let taken = self.recv.read(out);
        if taken > 0 {
            self.ack_immediately = true;
        }
        taken
    }

    /// Look at received bytes without taking them, for `MSG_PEEK`.
    pub fn peek(&self, out: &mut [u8]) -> usize {
        self.recv.peek(0, out)
    }

    /// Close this end: a `FIN` follows the data already queued.
    pub fn close(&mut self) {
        if self.fin_queued {
            return;
        }
        match self.state {
            State::SynSent => {
                // Nothing was ever established, so there is nobody to tell.
                // The timers go with it: a closed connection that still has a
                // retransmission armed will rewind its own sequence numbers
                // when it fires, which the fuzz target caught.
                self.state = State::Closed;
                self.failure = None;
                self.clear_timers();
            }
            State::SynReceived | State::Established => {
                self.fin_queued = true;
                self.state = State::FinWait1;
            }
            State::CloseWait => {
                self.fin_queued = true;
                self.state = State::LastAck;
            }
            _ => {}
        }
    }

    /// End the connection now, with a reset to the peer.
    pub fn abort(&mut self) {
        if self.state.is_synchronised() || self.state == State::SynReceived {
            self.reset_pending = true;
        }
        self.state = State::Closed;
        self.send.clear();
        self.holes.clear();
        self.clear_timers();
    }

    /// Stop every timer, which is what entering `CLOSED` means.
    pub(crate) fn clear_timers(&mut self) {
        self.retransmit_at = None;
        self.delayed_ack_at = None;
        self.persist_at = None;
        self.linger_at = None;
    }

    /// Give up on the connection with a reason, without answering the peer.
    pub(crate) fn fail(&mut self, failure: Failure) {
        self.state = State::Closed;
        self.failure = Some(failure);
        self.clear_timers();
    }

    /// The window to advertise: what the receive queue has room for, shifted.
    ///
    /// Silly-window avoidance lives here. A window smaller than a segment and
    /// smaller than half the buffer is advertised as zero, so a peer is never
    /// invited to send a segment carrying four bytes.
    pub(crate) fn advertised_window(&self) -> u16 {
        let free = self
            .recv
            .free()
            .saturating_sub(self.holes.held())
            .min(u32::MAX as usize) as u32;
        let segment = u32::from(self.rcv_mss);
        let half = (self.recv.capacity() / 2).min(u32::MAX as usize) as u32;
        let usable = if free < segment.min(half) { 0 } else { free };
        (usable >> self.rcv_wscale).min(u32::from(u16::MAX)) as u16
    }
}

/// What a `SYN` offered, taken out of the segment by a listener.
#[derive(Clone, Copy, Debug)]
pub struct Request {
    /// The request's sequence number.
    pub sequence: SeqNumber,
    /// The window it advertised, unscaled as it appeared on the wire.
    pub window: u16,
    /// The largest segment the peer will accept, if it said.
    pub mss: Option<u16>,
    /// The scale the peer asks for on its window, if it asked.
    pub window_scale: Option<u8>,
    /// Whether the peer offered selective acknowledgment.
    pub selective_ack: bool,
}
