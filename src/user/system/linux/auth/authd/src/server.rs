//! The socket (`docs/AUTH.md` §3.2): one `SOCK_SEQPACKET` listener, one
//! conversation per connection, and one `poll` for all of them.
//!
//! A conversation can stay open for hours, since the lock screen asks for
//! its first prompt when it locks, so connections are served side by side.
//! Hashing is not: a check runs to its end on this one thread, which is what
//! keeps `authd`'s memory to one hash's and makes a thousand connections
//! guess no faster than one (§3.5). A reply held for a failure's delay waits
//! in its connection's queue while the others are served.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ferrix_auth_client::{send_packet, socklen};
use ferrix_auth_proto::{MAX_RECORD, Record};

use crate::audit::say;
use crate::engine::{Conversation, Engine, Peer, Reply};
use crate::seat::Seat;

/// The most connections held at once; one more is closed at once.
const MAX_CONNECTIONS: usize = 64;

/// How much longer than its delay a held reply waits.
const HELD_MARGIN: Duration = Duration::from_millis(20);

/// The longest `poll` waits, so a stop is seen even if its signal is lost.
const TICK: Duration = Duration::from_secs(1);

struct Connection {
    fd: OwnedFd,
    conversation: Conversation,
    /// Replies not yet sent, each with when it may go.
    pending: VecDeque<(Instant, Reply)>,
    /// Whether a final reply is queued: read nothing more, and close once
    /// the queue is empty.
    closing: bool,
}

/// Milliseconds since 1970, which the store's throttles are kept in.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Serve `listener`, and the seat channel when there is one, until `stop`
/// is set.
pub(crate) fn serve(
    listener: &OwnedFd,
    engine: &mut Engine,
    mut seat: Option<&mut Seat>,
    stop: &AtomicBool,
    notify: &mut crate::notify::Notify,
) {
    let mut connections: Vec<Connection> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        // The service watchdog's ping, from the loop itself.
        notify.ping(now);
        for connection in &mut connections {
            flush(connection, now);
        }
        connections.retain(|c| !(c.closing && c.pending.is_empty()));
        let wake = connections
            .iter()
            .filter_map(|c| c.pending.front().map(|(at, _)| *at))
            .min()
            .map_or(TICK, |at| at.saturating_duration_since(now).min(TICK));
        let mut polled = vec![libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        polled.extend(connections.iter().map(|c| libc::pollfd {
            fd: c.fd.as_raw_fd(),
            events: if c.closing { 0 } else { libc::POLLIN },
            revents: 0,
        }));
        // The seat's port last, so the connections stay at 1.. in step with
        // `connections`; its place is kept, since `accept` may add to them.
        let seat_at = polled.len();
        if let Some(seat) = seat.as_deref() {
            polled.push(libc::pollfd {
                fd: seat.fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let timeout = i32::try_from(wake.as_millis()).unwrap_or(i32::MAX).max(1);
        let count = libc::nfds_t::try_from(polled.len()).unwrap_or(0);
        // SAFETY: `polled` is valid for reads and writes of `count` entries.
        let ready = unsafe { libc::poll(polled.as_mut_ptr(), count, timeout) };
        if ready < 0 {
            continue;
        }
        if polled
            .first()
            .is_some_and(|p| p.revents & libc::POLLIN != 0)
        {
            accept(listener, &mut connections);
        }
        let mut dead = Vec::new();
        for (index, (connection, entry)) in connections
            .iter_mut()
            .zip(polled.get(1..seat_at).unwrap_or_default())
            .enumerate()
        {
            if entry.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0
                || connection.closing
            {
                continue;
            }
            if !receive(connection, engine) {
                dead.push(index);
            }
        }
        let seat_ready = seat.is_some()
            && polled
                .get(seat_at)
                .is_some_and(|p| p.revents & libc::POLLIN != 0);
        for index in dead.into_iter().rev() {
            let _ = connections.remove(index);
        }
        if let Some(seat) = seat.as_deref_mut() {
            if seat_ready {
                seat.drain(engine);
            }
            // After the connections: a grant made by an acceptance just now
            // goes at once, as its ACCEPTED does.
            seat.send(engine);
        }
    }
}

/// Take one connection off the listener.
fn accept(listener: &OwnedFd, connections: &mut Vec<Connection>) {
    // SAFETY: null address and length ask for no address back.
    let fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
        )
    };
    if fd < 0 {
        return;
    }
    // SAFETY: `accept4` just returned `fd`, and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if connections.len() >= MAX_CONNECTIONS {
        say("authd: too many connections; one was closed unanswered");
        return;
    }
    let Some(peer) = peer_of(&fd) else {
        say("authd: a connection's peer could not be named; it was closed");
        return;
    };
    connections.push(Connection {
        fd,
        conversation: Conversation::new(peer),
        pending: VecDeque::new(),
        closing: false,
    });
}

/// `SO_PEERCRED`: who made the socket at the other end.
fn peer_of(fd: &OwnedFd) -> Option<Peer> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = socklen::<libc::ucred>();
    // SAFETY: `credentials` is valid for writes of `len` bytes, which is its
    // size, and `len` for a write of its own.
    let status = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut credentials).cast(),
            &raw mut len,
        )
    };
    (status == 0).then_some(Peer {
        pid: credentials.pid,
        uid: credentials.uid,
    })
}

/// Read one packet and answer it; false when the connection has gone.
fn receive(connection: &mut Connection, engine: &mut Engine) -> bool {
    let mut packet = [0_u8; MAX_RECORD + 1];
    // SAFETY: `packet` is valid for writes of its whole length.
    let got = unsafe {
        libc::recv(
            connection.fd.as_raw_fd(),
            packet.as_mut_ptr().cast(),
            packet.len(),
            0,
        )
    };
    let Ok(got) = usize::try_from(got) else {
        return io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock;
    };
    if got == 0 {
        return false;
    }
    let now = Instant::now();
    let outs = match Record::decode(packet.get(..got).unwrap_or(&[])) {
        Ok(record) => engine.handle(&mut connection.conversation, &record, now_ms()),
        Err(why) => {
            say(&format!(
                "authd: a peer sent a packet that is not a record ({why:?})"
            ));
            vec![crate::engine::Out {
                reply: Reply::Unavailable("that was not a record authd reads".to_owned()),
                after: Duration::ZERO,
            }]
        }
    };
    packet.fill(0);
    let _ = std::hint::black_box(&packet);
    for out in outs {
        connection.closing |= out.reply.is_final();
        // A held reply goes a little after its delay: the engine keeps the
        // account's delay in wall-clock milliseconds, and a client that asks
        // again the moment its answer arrives must find the delay over, not
        // a millisecond of it left to rounding.
        let after = if out.after.is_zero() {
            out.after
        } else {
            out.after + HELD_MARGIN
        };
        connection.pending.push_back((now + after, out.reply));
    }
    // A CANCEL or a record after the end has nothing to send: close.
    if connection.pending.is_empty()
        && matches!(connection.conversation.step, crate::engine::Step::Over)
    {
        connection.closing = true;
    }
    flush(connection, Instant::now());
    true
}

/// Send every reply that is due.
fn flush(connection: &mut Connection, now: Instant) {
    while connection.pending.front().is_some_and(|(at, _)| *at <= now) {
        let Some((_, reply)) = connection.pending.pop_front() else {
            break;
        };
        let mut buffer = [0_u8; MAX_RECORD];
        let sent = reply
            .record()
            .encode(&mut buffer)
            .ok()
            .and_then(|len| buffer.get(..len))
            .map(|packet| send_packet(connection.fd.as_raw_fd(), packet));
        if !matches!(sent, Some(Ok(()))) {
            connection.pending.clear();
            connection.closing = true;
        }
    }
}
