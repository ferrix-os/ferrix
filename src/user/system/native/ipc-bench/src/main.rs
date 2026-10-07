//! `ipc-bench`: how long a message takes to go to another process and back.
//!
//! The figure an IPC design is quoted by -- Zircon's "about a microsecond" is
//! this -- and the one `docs/OPAQUE-KERNEL.md`'s trip cannot give, because a
//! trip there includes a disk. Run from a shell, with no bootstrap handle, it
//! is the client: it starts a copy of itself as a native process in the root
//! cgroup's job, giving it one end of a channel, and then times
//! [`ROUNDS`] round trips of an eight-byte message, each a `channel_write`,
//! an `object_wait_one` for `READABLE` and a `channel_read`. Started with a
//! bootstrap channel it is the server, and echoes every message back until
//! the client closes its end.
//!
//! Then the same trip again with `channel_write_read`, one call a side and
//! the message in registers (`call`).
//!
//! Before the round trips it times [`ROUNDS`] `object_wait_one`s on a signal
//! already asserted, so the floor -- a native call that does not sleep -- is
//! printed beside the trip. Times are the processor's counter (`ferrix_rt::counter`),
//! converted to nanoseconds by the monotonic clock read around the run.
//! Each line is `ipc-bench <what> n=<count> min=<ns> p50=<ns> p90=<ns>
//! p99=<ns> mean=<ns>`, the percentiles to an eighth of a power of two.
//! Exit 0 is a run that finished; any other status names the step that did
//! not.

#![no_std]
#![no_main]

use core::fmt::Write as _;

use ferrix_native_abi::rights::{Requested, Rights};
use ferrix_rt::linux::{self, numbers};
use ferrix_rt::native::channel::{self, Channel, ReadError};
use ferrix_rt::native::job::{Job, for_cgroup};
use ferrix_rt::native::pending::{Protection, create_process};
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::native::{Deadline, Error, Handle, Object, OwnedHandle, Signals};
use ferrix_rt::{Bootstrap, Kernel};

ferrix_rt::entry!(main);

/// Round trips timed, after [`WARMUP`] untimed ones.
const ROUNDS: u32 = 2_000_000; // EXPERIMENT ONLY (po9-user)
/// Round trips run first and not timed: caches, first faults, the server's
/// first sleep.
const WARMUP: u32 = 1_000;
/// Where this program is, to start a copy of it.
const SELF: &[u8] = b"/sbin/ipc-bench\0";
/// The cgroup whose job the server runs in, which `bench-ipc`'s script makes.
const CGROUP: &[u8] = b"/sys/fs/cgroup/ipc-bench\0";
/// `AT_FDCWD`.
const AT_FDCWD: usize = -100_isize as usize;
/// `O_RDONLY | O_CLOEXEC`.
const O_READ: usize = 0o2_000_000;
/// `O_DIRECTORY | O_RDONLY | O_CLOEXEC`.
const O_DIR: usize = 0o2_000_000 | 0o200_000;
/// `SEEK_END`.
const SEEK_END: usize = 2;

/// The launcher with no bootstrap handle; with one, a server, or the client
/// of the domain run if its first message carries a channel.
fn main(bootstrap: Bootstrap) -> i32 {
    match bootstrap {
        Some(channel) => started(&channel),
        None => match client() {
            Ok(()) => 0,
            Err(step) => {
                say(format_args!("ipc-bench failed at step {step}"));
                step
            }
        },
    }
}

/// A process the launcher started: the domain run's client if its first
/// message carries the channel to time trips on, and otherwise a server,
/// which that first message was the first request to.
fn started(bootstrap: &Channel<Kernel>) -> i32 {
    let mut bytes = [0_u8; 64];
    let mut handles = [Handle::INVALID; 1];
    let received = loop {
        match bootstrap.read(&mut bytes, &mut handles) {
            Ok(received) => break received,
            Err(ReadError::Failed(Error::ShouldWait)) => {
                if bootstrap
                    .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
                    .is_err()
                {
                    return 36;
                }
            }
            Err(ReadError::Failed(Error::PeerClosed)) => return 0,
            Err(_) => return 37,
        }
    };
    let message = bytes.get(..received.bytes).unwrap_or_default();
    if received.handles == 1
        && let Some(&handle) = handles.first()
    {
        let server = Channel::from_owned(OwnedHandle::from_raw(Kernel, handle));
        return match domain_client(bootstrap, &server) {
            Ok(()) => 0,
            Err(step) => step,
        };
    }
    if message == FAST {
        return serve_fast(bootstrap);
    }
    if bootstrap.write(message).is_err() {
        return 31;
    }
    serve(bootstrap)
}

/// The domain run's client: time the `channel_write_read` trip to `server`,
/// a process of the same speculation domain, and send the line back to the
/// launcher, which has the console.
fn domain_client(launcher: &Channel<Kernel>, server: &Channel<Kernel>) -> Result<(), i32> {
    let message = 0x5EED_u64.to_ne_bytes();
    server.write(FAST).map_err(|_| 40)?;
    let mut call = Histogram::new();
    for _ in 0..WARMUP {
        call_trip(server, &message)?;
    }
    let clock = Clock::start();
    for _ in 0..ROUNDS {
        let before = ferrix_rt::counter().unwrap_or(0);
        call_trip(server, &message)?;
        call.add(ferrix_rt::counter().unwrap_or(0).wrapping_sub(before));
    }
    let scale = clock.stop();
    let line = call.line("domain-call", scale);
    launcher
        .write(line.bytes.get(..line.len).unwrap_or_default())
        .map_err(|_| 41)
}

/// Echo every message until the client lets go.
fn serve(channel: &Channel<Kernel>) -> i32 {
    let mut bytes = [0_u8; 64];
    let mut handles = [Handle::INVALID; 1];
    loop {
        match channel.read(&mut bytes, &mut handles) {
            Ok(received) => {
                let len = received.bytes.min(bytes.len());
                let message = bytes.get(..len).unwrap_or_default();
                if message == FAST {
                    return serve_fast(channel);
                }
                if channel.write(message).is_err() {
                    return 31;
                }
            }
            Err(ReadError::Failed(Error::ShouldWait)) => {
                if channel
                    .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
                    .is_err()
                {
                    return 32;
                }
            }
            Err(ReadError::Failed(Error::PeerClosed)) => return 0,
            Err(_) => return 33,
        }
    }
}

/// What the client sends to move the server to `channel_write_read`.
const FAST: &[u8] = b"FAST";

/// Echo with `channel_write_read`: one call per request, the answer to the
/// last going out as the next comes in.
fn serve_fast(channel: &Channel<Kernel>) -> i32 {
    let mut received = match channel.write_read(None) {
        Ok(words) => words,
        Err(Error::PeerClosed) => return 0,
        Err(_) => return 34,
    };
    loop {
        let bytes = received.bytes();
        let answer = bytes.get(..received.len).unwrap_or_default();
        received = match channel.write_read(Some(answer)) {
            Ok(words) => words,
            Err(Error::PeerClosed) => return 0,
            Err(_) => return 35,
        };
    }
}

/// Start the server, time the floor and the trips, print them; then the
/// same `channel_write_read` trip between two processes of one speculation
/// domain.
fn client() -> Result<(), i32> {
    let (image, job) = prepare()?;
    let mine = spawn(&job, &image, "ipc-echo")?;

    let mut floor = Histogram::new();
    let clock = Clock::start();
    for _ in 0..ROUNDS {
        let before = ferrix_rt::counter().unwrap_or(0);
        let _ = mine
            .wait_one(Signals::WRITABLE, Deadline::Never)
            .map_err(|_| 10)?;
        floor.add(ferrix_rt::counter().unwrap_or(0).wrapping_sub(before));
    }
    let scale = clock.stop();
    floor.print("floor", scale);

    let message = 0x5EED_u64.to_ne_bytes();
    let mut back = [0_u8; 64];
    let mut handles = [Handle::INVALID; 1];
    let mut trip = Histogram::new();
    for _ in 0..WARMUP {
        round_trip(&mine, &message, &mut back, &mut handles)?;
    }
    let clock = Clock::start();
    for _ in 0..ROUNDS {
        let before = ferrix_rt::counter().unwrap_or(0);
        round_trip(&mine, &message, &mut back, &mut handles)?;
        trip.add(ferrix_rt::counter().unwrap_or(0).wrapping_sub(before));
    }
    let scale = clock.stop();
    trip.print("trip", scale);

    // The same trip as one `channel_write_read` a side.
    mine.write(FAST).map_err(|_| 23)?;
    let mut call = Histogram::new();
    for _ in 0..WARMUP {
        call_trip(&mine, &message)?;
    }
    let clock = Clock::start();
    for _ in 0..ROUNDS {
        let before = ferrix_rt::counter().unwrap_or(0);
        call_trip(&mine, &message)?;
        call.add(ferrix_rt::counter().unwrap_or(0).wrapping_sub(before));
    }
    let scale = clock.stop();
    call.print("call", scale);
    drop(mine);
    domain_run(&job, &image)
}

/// A client and a server born in one new speculation domain, so that their
/// switches skip the predictor barrier (`docs/OPAQUE-KERNEL.md` §9.2): the
/// client is told the server's channel in its first message and sends its
/// line back. Both must be made in the job, which this process is not.
fn domain_run(job: &Job<Kernel>, image: &Vmo<Kernel>) -> Result<(), i32> {
    let domain = job.create_speculation_domain().map_err(|error| {
        say(format_args!("ipc-bench: speculation domain: {error:?}"));
        50
    })?;
    let to_server = spawn(&domain, image, "ipc-echo")?;
    let to_client = spawn(&domain, image, "ipc-client")?;
    to_client
        .write_with(b"CLIENT", [to_server.into_owned()])
        .map_err(|_| 51)?;
    let mut bytes = [0_u8; 160];
    let mut handles = [Handle::INVALID; 1];
    loop {
        match to_client.read(&mut bytes, &mut handles) {
            Ok(received) => {
                let line = bytes.get(..received.bytes).unwrap_or_default();
                let _ = linux::write(1, line);
                return Ok(());
            }
            Err(ReadError::Failed(Error::ShouldWait)) => {
                let _ = to_client
                    .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
                    .map_err(|_| 52)?;
            }
            Err(_) => return Err(53),
        }
    }
}

/// One message there and back by `channel_write_read`, checked.
fn call_trip(channel: &Channel<Kernel>, message: &[u8]) -> Result<(), i32> {
    let back = channel.write_read(Some(message)).map_err(|_| 24)?;
    if back.bytes().get(..back.len) != Some(message) {
        return Err(25);
    }
    Ok(())
}

/// One message there and back.
fn round_trip(
    channel: &Channel<Kernel>,
    message: &[u8],
    back: &mut [u8],
    handles: &mut [Handle],
) -> Result<(), i32> {
    channel.write(message).map_err(|_| 20)?;
    loop {
        match channel.read(back, handles) {
            Ok(_) => return Ok(()),
            Err(ReadError::Failed(Error::ShouldWait)) => {
                let _ = channel
                    .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
                    .map_err(|_| 21)?;
            }
            Err(_) => return Err(22),
        }
    }
}

/// Start a copy of this program, named `name`, in `job`, with one end of a
/// new channel as its bootstrap, and keep the other.
fn spawn(job: &Job<Kernel>, image: &Vmo<Kernel>, name: &str) -> Result<Channel<Kernel>, i32> {
    let (mine, theirs) = channel::create(Kernel).map_err(|_| 8)?;
    let process = create_process(job, image, name).map_err(|_| 9)?;
    process.start(theirs.into_owned()).map_err(|_| 9)?;
    // The process handle goes; the process lives until its channel does.
    drop(process);
    Ok(mine)
}

/// Load this program into a VMO, and find the job of a cgroup of its own.
fn prepare() -> Result<(Vmo<Kernel>, Job<Kernel>), i32> {
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
    // Back to the start for the reads, which then go through it in order.
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

    // Mounted and made here, what is there already being as good: the shell
    // running this may have no `mount` or `mkdir`.
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
    .map_err(|errno| {
        say(format_args!(
            "ipc-bench: opening the cgroup: errno {}",
            errno.0
        ));
        6
    })?;
    let job = for_cgroup(
        Kernel,
        i32::try_from(dir).unwrap_or(-1),
        Requested::Exactly(Rights::MANAGE),
    )
    .map_err(|error| {
        say(format_args!("ipc-bench: job_for_cgroup: {error:?}"));
        7
    })?;
    let _ = linux::close(dir);
    Ok((image, job))
}

/// The counter against the monotonic clock, over one run.
struct Clock {
    /// The clock at the start, in nanoseconds.
    nanos: u64,
    /// The counter at the start.
    ticks: u64,
}

impl Clock {
    /// Read both now.
    fn start() -> Clock {
        Clock {
            nanos: linux::monotonic_nanos().unwrap_or(0),
            ticks: ferrix_rt::counter().unwrap_or(0),
        }
    }

    /// Nanoseconds per thousand ticks since [`Clock::start`].
    fn stop(&self) -> u64 {
        let nanos = linux::monotonic_nanos()
            .unwrap_or(0)
            .saturating_sub(self.nanos);
        let ticks = ferrix_rt::counter()
            .unwrap_or(0)
            .saturating_sub(self.ticks)
            .max(1);
        nanos.saturating_mul(1000) / ticks
    }
}

/// Durations in ticks, every one kept, sorted when a line is made: the
/// percentiles are the samples themselves, not a histogram's buckets, which
/// read up to an eighth low and moved the trip's p50 in steps of about
/// 233 ns (`docs/OPAQUE-KERNEL.md` §9.5 step 0).
///
/// The samples live in a VMO mapped for them, since a native program has no
/// heap and [`ROUNDS`] of them are too many for its stack.
struct Histogram {
    /// The samples, the first `total` of them filled.
    samples: &'static mut [u64],
    /// How many in all.
    total: usize,
    /// Their sum, for the mean.
    sum: u64,
}

/// Room for one run's samples, page-rounded.
const SAMPLE_BYTES: usize = (ROUNDS as usize * size_of::<u64>()).next_multiple_of(4096);

impl Histogram {
    /// Nothing counted, in fresh memory of its own.
    fn new() -> Histogram {
        let samples = vmo::create(Kernel, SAMPLE_BYTES)
            .and_then(|room| room.map(None, SAMPLE_BYTES, Protection::ReadWrite, 0))
            .map_or(&mut [][..], |at| {
                // SAFETY: `vmo_map` answered `SAMPLE_BYTES` of fresh, zeroed,
                // writable memory at `at`, page-aligned, which nothing else
                // in this process names and which stays mapped when the VMO's
                // handle goes; `u64` is valid for any bytes.
                unsafe {
                    core::slice::from_raw_parts_mut(
                        core::ptr::with_exposed_provenance_mut::<u64>(at),
                        SAMPLE_BYTES / size_of::<u64>(),
                    )
                }
            });
        Histogram {
            samples,
            total: 0,
            sum: 0,
        }
    }

    /// Count one.
    fn add(&mut self, ticks: u64) {
        if let Some(slot) = self.samples.get_mut(self.total) {
            *slot = ticks;
            self.total += 1;
            self.sum = self.sum.saturating_add(ticks);
        }
    }

    /// One line on standard output: see [`Histogram::line`].
    fn print(&mut self, what: &str, scale: u64) {
        let line = self.line(what, scale);
        let _ = linux::write(1, line.bytes.get(..line.len).unwrap_or_default());
    }

    /// One line, in nanoseconds, `scale` being nanoseconds per thousand
    /// ticks: the least, the samples at the 50th, 90th and 99th percentiles
    /// of the sorted run, and the mean.
    fn line(&mut self, what: &str, scale: u64) -> Line {
        let ns = |ticks: u64| ticks.saturating_mul(scale) / 1000;
        let sorted = self.samples.get_mut(..self.total).unwrap_or_default();
        sorted.sort_unstable();
        let at = |per_mille: usize| {
            sorted
                .get(sorted.len() * per_mille / 1000)
                .copied()
                .unwrap_or(0)
        };
        let mean = self.sum / (self.total.max(1) as u64);
        format_line(format_args!(
            "ipc-bench {what} n={} min={} p50={} p90={} p99={} mean={}",
            self.total,
            ns(at(0)),
            ns(at(500)),
            ns(at(900)),
            ns(at(990)),
            ns(mean),
        ))
    }
}

/// A line on standard output.
fn say(line: core::fmt::Arguments<'_>) {
    let out = format_line(line);
    let _ = linux::write(1, out.bytes.get(..out.len).unwrap_or_default());
}

/// `line` formatted, with its newline.
fn format_line(line: core::fmt::Arguments<'_>) -> Line {
    let mut out = Line {
        bytes: [0; 160],
        len: 0,
    };
    let _ = out.write_fmt(line);
    let _ = out.write_str("\n");
    out
}

/// A line being formatted, cut short rather than overflowing.
struct Line {
    /// Its bytes.
    bytes: [u8; 160],
    /// How many are used.
    len: usize,
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
