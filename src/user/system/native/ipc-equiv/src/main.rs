//! `ipc-equiv`: the cases `channel_write_read` must answer alike on the
//! general path and on the fast path (`docs/OPAQUE-KERNEL.md` §9.7, part 6).
//!
//! Run from a shell with no bootstrap handle it is the launcher: it starts
//! copies of itself as echo servers and runs each case against one, printing
//! one transcript line per case, `ipc-equiv case <n> <name>: <what came
//! back>`, and last `ipc-equiv: exit <status>`. Started with a bootstrap
//! channel it is a server, which answers each request by its first byte (see
//! [`serve`]). `cargo xtask test-ipc-equiv` boots it with `ferrix.fastpath=off`
//! and, on x86-64, again with `on`, and requires the transcripts to match.
//!
//! No fast path exists yet, so both boots take the general path, and the
//! cases are written against what the general path answers. Every line
//! states a result that does not depend on timing: where a case races, the
//! line names the set of results the general path allows and which of them
//! came, so a fast path that gives another fails the comparison.
//!
//! The cases of part 6 built here: 1 (echo of every length), 2 (a
//! receive-only first call and 1,000 trips in order), 3 (a message already
//! waiting), 5 (the peer closed before the call and during the wait), 6 (the
//! waiting process killed, and the peer ending during a trip), 8 (a port
//! observing the server's end), 12 (the refusals: a count of 25, a closed
//! handle, a VMO, no WRITE, no READ) and 16 (a receive-only call ended by
//! the peer's close). The rest need what a native program cannot do today
//! or the fast path itself, and are listed in [`OWED`].

#![no_std]
#![no_main]

use core::fmt::Write as _;

use ferrix_native_abi::rights::{Requested, Rights};
use ferrix_native_abi::types::{PROCESS_KILLED, PROCESS_RUNNING};
use ferrix_rt::linux::{self, numbers};
use ferrix_rt::native::channel::{self, Channel};
use ferrix_rt::native::job::{Job, for_cgroup};
use ferrix_rt::native::pending::{Process, create_process};
use ferrix_rt::native::port;
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::native::{Deadline, Error, Object, OwnedHandle, Signals};
use ferrix_rt::{Bootstrap, Kernel};

/// The most bytes `channel_write_read` carries: three of this processor's
/// words.
const MOST: usize = ferrix_native_abi::nr::CHANNEL_WRITE_READ_BYTES;

ferrix_rt::entry!(main);

/// Where this program is, to start a copy of it.
const SELF: &[u8] = b"/sbin/ipc-equiv\0";
/// The cgroup whose job the servers run in, which the runner's script makes.
const CGROUP: &[u8] = b"/sys/fs/cgroup/ipc-equiv\0";
/// `AT_FDCWD`.
const AT_FDCWD: usize = -100_isize as usize;
/// `O_RDONLY | O_CLOEXEC`.
const O_READ: usize = 0o2_000_000;
/// `O_DIRECTORY | O_RDONLY | O_CLOEXEC`.
const O_DIR: usize = 0o2_000_000 | 0o200_000;
/// `SEEK_END`.
const SEEK_END: usize = 2;
/// Trips in case 2.
const TRIPS: u64 = 1_000;
/// How long a server waits before it closes, or the launcher before it
/// kills: long enough that the other side is waiting by then.
const SETTLE_NANOS: u64 = 20_000_000;

/// The cases of part 6 not built here, and why: printed, so the transcript
/// says what it does not cover.
const OWED: &[(&str, &str)] = &[
    (
        "4",
        "two threads of one process reading one end: native programs have no threads yet",
    ),
    (
        "5c",
        "a close by another thread of the peer's process: as case 4",
    ),
    ("6c", "an execve by another thread: as case 4"),
    (
        "7",
        "a signal with a handler: a native program has no signals",
    ),
    (
        "9",
        "the peer pinned to another processor: no affinity call in the native ABI",
    ),
    ("10", "a spinner pinned beside the trip: as case 9"),
    (
        "11",
        "a filtered process: only stage 9's probe filters, a kernel check's",
    ),
    (
        "13",
        "one domain and two, with the barrier counters: the counters are the kernel's",
    ),
    (
        "14",
        "an end in T13's window: the hook is the fast path's, a kernel check's",
    ),
    (
        "15",
        "vector state across a general resume: needs step 3a's contract",
    ),
];

fn main(bootstrap: Bootstrap) -> i32 {
    match bootstrap {
        Some(channel) => serve(&channel),
        None => {
            let status = match launch() {
                Ok(()) => 0,
                Err(step) => {
                    say(format_args!("ipc-equiv failed at step {step}"));
                    step
                }
            };
            say(format_args!("ipc-equiv: exit {status}"));
            status
        }
    }
}

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

/// Answer requests by their first byte, each by `channel_write_read`: the
/// answer to one goes out as the next comes in.
/// - `E`, or an empty message: echo it.
/// - `W`: write `X` first, then answer `w`, so `X` is what the caller's call
///   receives and `w` waits on its end (case 3).
/// - `C`: close without answering (case 5).
/// - `Z`: end the process without answering (case 6).
/// - `P`: observe this end for `READABLE` on a port, answer `a`; the next
///   request's answer is `p1` if the port got its packet, `p0` if not
///   (case 8).
/// - `Q`: answer `q`, wait a little, and close (case 16).
fn serve(channel: &Channel<Kernel>) -> i32 {
    let mut port = None;
    let mut received = match channel.write_read(None) {
        Ok(words) => words,
        Err(Error::PeerClosed) => return 0,
        Err(_) => return 60,
    };
    loop {
        let bytes = received.bytes();
        let request = bytes.get(..received.len).unwrap_or_default();
        let observed = port.take().map(|port: port::Port<Kernel>| {
            if port.wait(Deadline::At(0)).is_ok() {
                b"p1"
            } else {
                b"p0"
            }
        });
        let answer: &[u8] = match (observed, request.first()) {
            (Some(observed), _) => observed,
            (None, None | Some(b'E')) => request,
            (None, Some(b'W')) => {
                if channel.write(b"X").is_err() {
                    return 61;
                }
                b"w"
            }
            (None, Some(b'C')) => return 0,
            (None, Some(b'Z')) => linux_exit(7),
            (None, Some(b'P')) => {
                let Ok(made) = port::create(Kernel) else {
                    return 62;
                };
                if channel.wait_async(&made, Signals::READABLE, 1).is_err() {
                    return 63;
                }
                port = Some(made);
                b"a"
            }
            (None, Some(b'Q')) => {
                let _ = channel.write(b"q");
                sleep(SETTLE_NANOS);
                return 0;
            }
            (None, Some(_)) => b"?",
        };
        received = match channel.write_read(Some(answer)) {
            Ok(words) => words,
            Err(Error::PeerClosed) => return 0,
            Err(_) => return 64,
        };
    }
}

/// End this process with `status`, now.
fn linux_exit(status: i32) -> ! {
    ferrix_rt::exit(status)
}

/// Sleep about `nanos`, on a port nothing posts to.
fn sleep(nanos: u64) {
    let now = linux::monotonic_nanos().unwrap_or(0);
    if let Ok(port) = port::create(Kernel) {
        let _ = port.wait(Deadline::At(now.saturating_add(nanos)));
    }
}

// ---------------------------------------------------------------------------
// The launcher and the cases
// ---------------------------------------------------------------------------

/// The image and the job every server is started from.
struct Place {
    image: Vmo<Kernel>,
    job: Job<Kernel>,
}

/// A server started for one case: the launcher's end, and the process.
struct Server {
    channel: Channel<Kernel>,
    process: Process<Kernel>,
}

impl Place {
    /// A server in `job`, or this place's job.
    fn server_in(&self, job: Option<&Job<Kernel>>) -> Result<Server, i32> {
        let (mine, theirs) = channel::create(Kernel).map_err(|_| 8)?;
        let process = create_process(job.unwrap_or(&self.job), &self.image, "ipc-equiv-server")
            .map_err(|_| 9)?;
        process.start(theirs.into_owned()).map_err(|_| 9)?;
        Ok(Server {
            channel: mine,
            process,
        })
    }
}

fn launch() -> Result<(), i32> {
    let place = prepare()?;
    echo_every_length(&place)?;
    trips_in_order(&place)?;
    a_message_already_waiting(&place)?;
    the_peer_closed(&place)?;
    an_end(&place)?;
    a_port_observing(&place)?;
    the_refusals()?;
    a_receive_ended_by_a_close(&place)?;
    for (case, why) in OWED {
        say(format_args!("ipc-equiv case {case} owed: {why}"));
    }
    Ok(())
}

/// What a call answered, as a transcript names it.
fn answer(result: Result<channel::Words, Error>) -> Line {
    match result {
        Ok(words) => {
            let bytes = words.bytes();
            let message = bytes.get(..words.len).unwrap_or_default();
            format_line(format_args!("{:?}", Printable(message)))
        }
        Err(error) => format_line(format_args!("{error:?}")),
    }
}

/// Bytes as a transcript shows them: printable ASCII as itself, the rest as
/// hex.
struct Printable<'a>(&'a [u8]);

impl core::fmt::Debug for Printable<'_> {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        out.write_str("\"")?;
        for &byte in self.0 {
            if byte.is_ascii_graphic() {
                out.write_char(char::from(byte))?;
            } else {
                write!(out, "\\x{byte:02x}")?;
            }
        }
        out.write_str("\"")
    }
}

/// Case 1: an echo of every length from 0 to [`MOST`] bytes -- 24 on a
/// 64-bit processor, 12 on a 32-bit one -- comes back as sent, with the
/// bytes past its length zero.
fn echo_every_length(place: &Place) -> Result<(), i32> {
    let server = place.server_in(None)?;
    let mut sent = [0_u8; MOST];
    for (at, byte) in sent.iter_mut().enumerate() {
        *byte = if at == 0 { b'E' } else { 0x80 | at as u8 };
    }
    let mut good = 0;
    for len in 0..=MOST {
        let message = sent.get(..len).unwrap_or_default();
        let back = server.channel.write_read(Some(message)).map_err(|_| 10)?;
        let bytes = back.bytes();
        let tail_zero = bytes
            .get(back.len..)
            .is_some_and(|tail| tail.iter().all(|&b| b == 0));
        if back.len == len && bytes.get(..len) == Some(message) && tail_zero {
            good += 1;
        }
    }
    say(format_args!(
        "ipc-equiv case 1 echo of every length 0 to {MOST}: {good} of {} came back as sent, \
         the rest of the words zero",
        MOST + 1
    ));
    Ok(())
}

/// Case 2: after the server's receive-only first call, 1,000 trips carrying
/// sequence numbers come back in order.
fn trips_in_order(place: &Place) -> Result<(), i32> {
    let server = place.server_in(None)?;
    let mut in_order = 0_u64;
    for sequence in 0..TRIPS {
        let mut message = [0_u8; 9];
        message[0] = b'E';
        if let Some(rest) = message.get_mut(1..) {
            rest.copy_from_slice(&sequence.to_le_bytes());
        }
        let back = server.channel.write_read(Some(&message)).map_err(|_| 11)?;
        if back.bytes().get(..back.len) == Some(&message[..]) {
            in_order += 1;
        }
    }
    say(format_args!(
        "ipc-equiv case 2 receive-only first, then {TRIPS} trips: {in_order} in order"
    ));
    Ok(())
}

/// Case 3: a message already waiting on the caller's end (T6) is the answer,
/// and the caller's own message still reaches the server.
fn a_message_already_waiting(place: &Place) -> Result<(), i32> {
    let server = place.server_in(None)?;
    let first = answer(server.channel.write_read(Some(b"W")));
    // `w` waits on this end now; this call's answer is it, at once.
    let second = answer(server.channel.write_read(Some(b"E1")));
    // And the echo of `E1` follows.
    let third = answer(server.channel.write_read(None));
    say(format_args!(
        "ipc-equiv case 3 a message waiting: W got {}, E1 got {}, then {}",
        first.text(),
        second.text(),
        third.text()
    ));
    Ok(())
}

/// Case 5: the peer closed before the call (T8), on a send and on a
/// receive-only call; and during the wait.
fn the_peer_closed(place: &Place) -> Result<(), i32> {
    let (mine, theirs) = channel::create(Kernel).map_err(|_| 12)?;
    drop(theirs);
    let sending = answer(mine.write_read(Some(b"E")));
    let receiving = answer(mine.write_read(None));
    let server = place.server_in(None)?;
    let waiting = answer(server.channel.write_read(Some(b"C")));
    say(format_args!(
        "ipc-equiv case 5 the peer closed: before a send {}, before a receive {}, during the \
         wait {}",
        sending.text(),
        receiving.text(),
        waiting.text()
    ));
    Ok(())
}

/// Case 6: a process killed while it waits in `channel_write_read` ends,
/// killed; and a peer whose process ends during a trip answers the caller
/// `PeerClosed`.
fn an_end(place: &Place) -> Result<(), i32> {
    let job = place.job.create_child().map_err(|_| 13)?;
    let server = place.server_in(Some(&job))?;
    // The server's first call is receive-only: it waits.
    sleep(SETTLE_NANOS);
    job.kill().map_err(|_| 14)?;
    let ended = wait_ended(&server.process)?;
    let ending = place.server_in(None)?;
    let during = answer(ending.channel.write_read(Some(b"Z")));
    say(format_args!(
        "ipc-equiv case 6 an end: a waiter's process killed {ended}; the peer's process ended \
         during a trip {}",
        during.text()
    ));
    Ok(())
}

/// Wait for `process` to end, and say how.
fn wait_ended(process: &Process<Kernel>) -> Result<&'static str, i32> {
    let port = port::create(Kernel).map_err(|_| 15)?;
    process.notify_on_exit(&port, 1).map_err(|_| 15)?;
    let _ = port.wait(Deadline::Never).map_err(|_| 15)?;
    let status = process.status().map_err(|_| 16)?;
    Ok(match status.state {
        PROCESS_KILLED => "ended, killed",
        PROCESS_RUNNING => "still running",
        _ => "ended, not killed",
    })
}

/// Case 8: a port observing the server's end (T10) gets its packet from a
/// message `channel_write_read` sent.
fn a_port_observing(place: &Place) -> Result<(), i32> {
    let server = place.server_in(None)?;
    let armed = answer(server.channel.write_read(Some(b"P")));
    let heard = answer(server.channel.write_read(Some(b"E2")));
    say(format_args!(
        "ipc-equiv case 8 a port observing the peer: armed {}, the packet {}",
        armed.text(),
        heard.text()
    ));
    Ok(())
}

/// Case 12: the calls the general path refuses before any wait (T3 to T5):
/// a count of 25, a closed handle, a VMO, and an end without WRITE or
/// without READ.
fn the_refusals() -> Result<(), i32> {
    let (mine, peer) = channel::create(Kernel).map_err(|_| 17)?;
    let too_many = answer(mine.write_read_unchecked(MOST + 1, [0; 3]));

    let (gone, _other) = channel::create(Kernel).map_err(|_| 17)?;
    let raw = gone.into_owned().into_raw();
    let _ = OwnedHandle::from_raw(Kernel, raw).close();
    let closed_end = Channel::from_owned(OwnedHandle::from_raw(Kernel, raw));
    let closed = answer(closed_end.write_read(Some(b"E")));
    // Its number is closed already: let it go without a second close.
    let _ = closed_end.into_owned().into_raw();

    let made = vmo::create(Kernel, 4096).map_err(|_| 18)?;
    let raw = made.into_owned().into_raw();
    let not_a_channel = Channel::from_owned(OwnedHandle::from_raw(Kernel, raw));
    let wrong = answer(not_a_channel.write_read(Some(b"E")));
    drop(not_a_channel);

    let read_only = mine
        .duplicate(Requested::Exactly(Rights::READ | Rights::WAIT))
        .map_err(|_| 19)?;
    let no_write = answer(read_only.write_read(Some(b"E")));
    let write_only = mine
        .duplicate(Requested::Exactly(Rights::WRITE | Rights::WAIT))
        .map_err(|_| 19)?;
    let no_read = answer(write_only.write_read(Some(b"E")));
    drop(peer);
    say(format_args!(
        "ipc-equiv case 12 refusals: a count of {} {}, a closed handle {}, a VMO {}, without \
         WRITE {}, without READ {}",
        MOST + 1,
        too_many.text(),
        closed.text(),
        wrong.text(),
        no_write.text(),
        no_read.text()
    ));
    Ok(())
}

/// Case 16: a receive-only call waiting when the peer closes answers
/// `PeerClosed`.
fn a_receive_ended_by_a_close(place: &Place) -> Result<(), i32> {
    let server = place.server_in(None)?;
    let first = answer(server.channel.write_read(Some(b"Q")));
    let waiting = answer(server.channel.write_read(None));
    say(format_args!(
        "ipc-equiv case 16 a receive-only call ended by the peer's close: Q got {}, then the \
         wait {}",
        first.text(),
        waiting.text()
    ));
    Ok(())
}

/// Load this program into a VMO, and find the job of a cgroup of its own,
/// as `ipc-bench` does.
fn prepare() -> Result<Place, i32> {
    // SAFETY: `SELF` is NUL-terminated and borrowed for the call.
    let fd = unsafe {
        linux::call(
            numbers::OPENAT,
            [AT_FDCWD, SELF.as_ptr().addr(), O_READ, 0, 0, 0],
        )
    }
    .map_err(|_| 1)?;
    // SAFETY: no pointer arguments.
    let size = unsafe { linux::call(numbers::LSEEK, [fd, 0, SEEK_END, 0, 0, 0]) }.map_err(|_| 2)?;
    let image = vmo::create(Kernel, size).map_err(|_| 3)?;
    // SAFETY: no pointer arguments.
    let _ = unsafe { linux::call(numbers::LSEEK, [fd, 0, 0, 0, 0, 0]) }.map_err(|_| 2)?;
    let mut chunk = [0_u8; 4096];
    let mut at = 0_usize;
    while at < size {
        let got = linux::read(fd, &mut chunk).map_err(|_| 4)?;
        if got == 0 {
            return Err(4);
        }
        image
            .write(chunk.get(..got).unwrap_or_default(), at as u64)
            .map_err(|_| 5)?;
        at = at.saturating_add(got);
    }
    let _ = linux::close(fd);
    for (source, target, kind) in [
        (&b"sys\0"[..], &b"/sys\0"[..], &b"sysfs\0"[..]),
        (b"cgroup2\0", b"/sys/fs/cgroup\0", b"cgroup2\0"),
    ] {
        // SAFETY: the three strings are NUL-terminated and borrowed for the
        // call; no data argument.
        let _ = unsafe {
            linux::call(
                numbers::MOUNT,
                [
                    source.as_ptr().addr(),
                    target.as_ptr().addr(),
                    kind.as_ptr().addr(),
                    0,
                    0,
                    0,
                ],
            )
        };
    }
    // SAFETY: `CGROUP` is NUL-terminated and borrowed for the call.
    let _ = unsafe {
        linux::call(
            numbers::MKDIRAT,
            [AT_FDCWD, CGROUP.as_ptr().addr(), 0o755, 0, 0, 0],
        )
    };
    // SAFETY: `CGROUP` is NUL-terminated and borrowed for the call.
    let dir = unsafe {
        linux::call(
            numbers::OPENAT,
            [AT_FDCWD, CGROUP.as_ptr().addr(), O_DIR, 0, 0, 0],
        )
    }
    .map_err(|_| 6)?;
    let job = for_cgroup(
        Kernel,
        i32::try_from(dir).unwrap_or(-1),
        Requested::Exactly(Rights::MANAGE),
    )
    .map_err(|_| 7)?;
    let _ = linux::close(dir);
    Ok(Place { image, job })
}

// ---------------------------------------------------------------------------
// Lines
// ---------------------------------------------------------------------------

/// A line on standard output.
fn say(line: core::fmt::Arguments<'_>) {
    let out = format_line(line);
    let _ = linux::write(1, out.bytes.get(..out.len).unwrap_or_default());
    let _ = linux::write(1, b"\n");
}

/// `line` formatted, without its newline.
fn format_line(line: core::fmt::Arguments<'_>) -> Line {
    let mut out = Line {
        bytes: [0; 240],
        len: 0,
    };
    let _ = out.write_fmt(line);
    out
}

/// A line being formatted, cut short rather than overflowing.
struct Line {
    /// Its bytes.
    bytes: [u8; 240],
    /// How many are used.
    len: usize,
}

impl Line {
    /// What has been formatted.
    fn text(&self) -> &str {
        core::str::from_utf8(self.bytes.get(..self.len).unwrap_or_default()).unwrap_or("?")
    }
}

impl core::fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.len.saturating_add(text.len()).min(self.bytes.len());
        let room = end - self.len;
        if let (Some(into), Some(from)) = (
            self.bytes.get_mut(self.len..end),
            text.as_bytes().get(..room),
        ) {
            into.copy_from_slice(from);
        }
        self.len = end;
        Ok(())
    }
}
