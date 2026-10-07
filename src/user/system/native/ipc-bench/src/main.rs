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
//!
//! MEASUREMENT ONLY (branch `os76/p0-measure`, never lands), three additions:
//!
//! - `null-entry` (x86-64) and `null-general`: the bare cost of a native
//!   call. `null-entry` is [`NULL_ENTRY`], which the kernel's `SYSCALL`
//!   entry answers right after the fast path's T1 test, before the filter,
//!   the decode, the interrupts opened and the way-out look; `null-general`
//!   is [`NULL_GENERAL`], a gap in the native range that goes the general
//!   path to `native::dispatch` and is refused there by its decode
//!   (`ENOSYS`). Each sample is [`NULL_BATCH`] calls back to back, printed
//!   per call, with `batch=` in the line.
//! - `ipc-bench.long=<seconds>` on the kernel command line (read from
//!   `/proc/cmdline`, so no rebuild): after the stock `domain-call` line the
//!   domain client keeps going in blocks of [`BLOCK`] trips until that many
//!   seconds have passed, printing per block `po9blk <i> med=<ticks>
//!   min=<ticks> p90=<ticks> p99=<ticks> ns=<median ns> span=<block us>
//!   tsc=<counter>` (the prefix and the first fields are po9-user's, so its
//!   scripts read these), and at the end `ipc-bench long ...`, the spread of
//!   the block medians in ns. Without the option the run prints what it
//!   always did, plus the two null lines.

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
const ROUNDS: u32 = 20_000;
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

/// MEASUREMENT ONLY: the native number the x86-64 `SYSCALL` entry answers
/// with 0 at once (`src/kernel/src/arch/x86_64/syscall.rs`, beside T1).
const NULL_ENTRY: usize = 0x1FF0;
/// MEASUREMENT ONLY: a gap in the native range, which goes the whole general
/// path and is refused `ENOSYS` by `native::dispatch`'s decode.
const NULL_GENERAL: usize = 0x1FF1;
/// Null calls per timed sample: the counter steps in about 10 ns under KVM,
/// which is the order of the call itself.
const NULL_BATCH: u64 = 16;

/// The kernel option that asks for the long run, its value in seconds.
const LONG_OPTION: &[u8] = b"ipc-bench.long=";
/// The longest long run asked for is cut to this, in seconds.
const LONG_MOST: u64 = 3_600;
/// Trips a long run's block holds.
const BLOCK: usize = 50_000;
/// The most blocks a long run keeps medians for: more than an hour's.
const MOST_BLOCKS: usize = 65_536;
/// What the launcher sends the domain client, before the long run's seconds.
const CLIENT: &[u8] = b"CLIENT";
/// `/proc/cmdline`.
const CMDLINE: &[u8] = b"/proc/cmdline\0";

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
        // "CLIENT", and after it the long run's seconds when one was asked.
        let long = message
            .strip_prefix(CLIENT)
            .and_then(|rest| <[u8; 8]>::try_from(rest).ok())
            .map_or(0, u64::from_le_bytes);
        return match domain_client(bootstrap, &server, long) {
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
/// launcher, which has the console. Then, with `long` seconds asked for
/// (`ipc-bench.long`), the long run.
fn domain_client(
    launcher: &Channel<Kernel>,
    server: &Channel<Kernel>,
    long: u64,
) -> Result<(), i32> {
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
        .map_err(|_| 41)?;
    if long > 0 {
        long_run(launcher, server, &message, long)?;
    }
    Ok(())
}

/// MEASUREMENT ONLY: the long run. The same trip in blocks of [`BLOCK`],
/// each block's line sent to the launcher as it ends, until `seconds` have
/// passed; then the spread of the block medians. A block's line is made and
/// sent outside its timed trips, and the launcher's print runs between
/// blocks or inside the next one, which its median does not notice.
fn long_run(
    launcher: &Channel<Kernel>,
    server: &Channel<Kernel>,
    message: &[u8],
    seconds: u64,
) -> Result<(), i32> {
    let mut block = Histogram::with_room(BLOCK);
    let mut medians = Histogram::with_room(MOST_BLOCKS);
    let start = linux::monotonic_nanos().unwrap_or(0);
    let end = start.saturating_add(seconds.saturating_mul(1_000_000_000));
    let mut index = 0_usize;
    loop {
        block.clear();
        let clock = Clock::start();
        for _ in 0..BLOCK {
            let before = ferrix_rt::counter().unwrap_or(0);
            call_trip(server, message)?;
            block.add(ferrix_rt::counter().unwrap_or(0).wrapping_sub(before));
        }
        let (scale, span) = clock.stop_spanned();
        let tsc = ferrix_rt::counter().unwrap_or(0);
        let [min, median, p90, p99] = block.points();
        let median_ns = median.saturating_mul(scale) / 1000;
        medians.add(median_ns);
        let line = format_line(format_args!(
            "po9blk {index} med={median} min={min} p90={p90} p99={p99} ns={median_ns} span={} tsc={tsc}",
            span / 1000
        ));
        launcher
            .write(line.bytes.get(..line.len).unwrap_or_default())
            .map_err(|_| 42)?;
        index += 1;
        if index >= MOST_BLOCKS || linux::monotonic_nanos().unwrap_or(u64::MAX) >= end {
            break;
        }
    }
    let took = linux::monotonic_nanos()
        .unwrap_or(0)
        .saturating_sub(start)
        / 1_000_000;
    let sorted = medians.sorted();
    let at = |per_mille: usize| {
        sorted
            .get(sorted.len() * per_mille / 1000)
            .copied()
            .unwrap_or(0)
    };
    let line = format_line(format_args!(
        "ipc-bench long blocks={index} trips={} ms={took} block-median-ns min={} p10={} p50={} p90={} max={}",
        index.saturating_mul(BLOCK),
        at(0),
        at(100),
        at(500),
        at(900),
        sorted.last().copied().unwrap_or(0),
    ));
    launcher
        .write(line.bytes.get(..line.len).unwrap_or_default())
        .map_err(|_| 43)
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
    let long = long_seconds();
    if long > 0 {
        say(format_args!(
            "ipc-bench long: {long} s of {BLOCK}-trip domain-call blocks after the stock lines (ipc-bench.long)"
        ));
    }
    let mine = spawn(&job, &image, "ipc-echo")?;

    // MEASUREMENT ONLY: the bare call, at the entry and through the general
    // path. The first only where the entry answers it.
    if cfg!(target_arch = "x86_64") {
        null_calls("null-entry", NULL_ENTRY);
    }
    null_calls("null-general", NULL_GENERAL);

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
    domain_run(&job, &image, long)
}

/// MEASUREMENT ONLY: time `number`, a native call that answers at once, in
/// samples of [`NULL_BATCH`] calls, and print the line per call.
fn null_calls(what: &str, number: usize) {
    let null = || {
        // SAFETY: no pointer arguments; the kernel answers the number
        // without reading any of them.
        let _ = unsafe { linux::call(number, [0; 6]) };
    };
    for _ in 0..WARMUP {
        null();
    }
    let mut calls = Histogram::new();
    let clock = Clock::start();
    for _ in 0..ROUNDS {
        let before = ferrix_rt::counter().unwrap_or(0);
        for _ in 0..NULL_BATCH {
            null();
        }
        calls.add(ferrix_rt::counter().unwrap_or(0).wrapping_sub(before));
    }
    let scale = clock.stop();
    let line = calls.line_per(what, scale, NULL_BATCH);
    let _ = linux::write(1, line.bytes.get(..line.len).unwrap_or_default());
}

/// MEASUREMENT ONLY: the seconds `ipc-bench.long=` asks for on the kernel
/// command line, cut to [`LONG_MOST`]; 0 when it is not there or
/// `/proc/cmdline` cannot be read. `/proc` is mounted here if it is not.
fn long_seconds() -> u64 {
    let open = || {
        // SAFETY: `CMDLINE` is NUL-terminated and borrowed for the call.
        unsafe {
            linux::call(
                numbers::OPENAT,
                [AT_FDCWD, CMDLINE.as_ptr().addr(), O_READ, 0, 0, 0],
            )
        }
    };
    let fd = match open() {
        Ok(fd) => fd,
        Err(_) => {
            let proc_dir = b"/proc\0";
            // SAFETY: the strings are NUL-terminated and borrowed for the
            // calls; no data argument.
            let _ = unsafe {
                linux::call(
                    numbers::MKDIRAT,
                    [AT_FDCWD, proc_dir.as_ptr().addr(), 0o555, 0, 0, 0],
                )
            };
            // SAFETY: as above.
            let _ = unsafe {
                linux::call(
                    numbers::MOUNT,
                    [
                        b"proc\0".as_ptr().addr(),
                        proc_dir.as_ptr().addr(),
                        b"proc\0".as_ptr().addr(),
                        0,
                        0,
                        0,
                    ],
                )
            };
            match open() {
                Ok(fd) => fd,
                Err(_) => return 0,
            }
        }
    };
    let mut line = [0_u8; 4096];
    let got = linux::read(fd, &mut line).unwrap_or(0);
    let _ = linux::close(fd);
    line.get(..got)
        .unwrap_or_default()
        .split(u8::is_ascii_whitespace)
        .find_map(|word| word.strip_prefix(LONG_OPTION))
        .and_then(|digits| core::str::from_utf8(digits).ok()?.parse::<u64>().ok())
        .map_or(0, |seconds| seconds.min(LONG_MOST))
}

/// A client and a server born in one new speculation domain, so that their
/// switches skip the predictor barrier (`docs/OPAQUE-KERNEL.md` §9.2): the
/// client is told the server's channel in its first message and sends its
/// line back. Both must be made in the job, which this process is not.
/// With `long` seconds asked for, the client is told them too, and its block
/// lines are printed as they come until its `ipc-bench long` line.
fn domain_run(job: &Job<Kernel>, image: &Vmo<Kernel>, long: u64) -> Result<(), i32> {
    let domain = job.create_speculation_domain().map_err(|error| {
        say(format_args!("ipc-bench: speculation domain: {error:?}"));
        50
    })?;
    let to_server = spawn(&domain, image, "ipc-echo")?;
    let to_client = spawn(&domain, image, "ipc-client")?;
    let mut asked = [0_u8; 14];
    let hello: &[u8] = if long == 0 {
        CLIENT
    } else {
        let (name, seconds) = asked.split_at_mut(CLIENT.len());
        name.copy_from_slice(CLIENT);
        seconds.copy_from_slice(&long.to_le_bytes());
        &asked
    };
    to_client
        .write_with(hello, [to_server.into_owned()])
        .map_err(|_| 51)?;
    let mut bytes = [0_u8; 160];
    let mut handles = [Handle::INVALID; 1];
    loop {
        match to_client.read(&mut bytes, &mut handles) {
            Ok(received) => {
                let line = bytes.get(..received.bytes).unwrap_or_default();
                let _ = linux::write(1, line);
                if long == 0 || line.starts_with(b"ipc-bench long ") {
                    return Ok(());
                }
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
        self.stop_spanned().0
    }

    /// [`Clock::stop`], and the nanoseconds since [`Clock::start`].
    fn stop_spanned(&self) -> (u64, u64) {
        let nanos = linux::monotonic_nanos()
            .unwrap_or(0)
            .saturating_sub(self.nanos);
        let ticks = ferrix_rt::counter()
            .unwrap_or(0)
            .saturating_sub(self.ticks)
            .max(1);
        (nanos.saturating_mul(1000) / ticks, nanos)
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

impl Histogram {
    /// Nothing counted, in fresh memory of its own: room for one run.
    fn new() -> Histogram {
        Histogram::with_room(ROUNDS as usize)
    }

    /// Nothing counted, with room for `count` samples in fresh memory of its
    /// own.
    fn with_room(count: usize) -> Histogram {
        let bytes = (count * size_of::<u64>()).next_multiple_of(4096);
        let samples = vmo::create(Kernel, bytes)
            .and_then(|room| room.map(None, bytes, Protection::ReadWrite, 0))
            .map_or(&mut [][..], |at| {
                // SAFETY: `vmo_map` answered `bytes` of fresh, zeroed,
                // writable memory at `at`, page-aligned, which nothing else
                // in this process names and which stays mapped when the VMO's
                // handle goes; `u64` is valid for any bytes.
                unsafe {
                    core::slice::from_raw_parts_mut(
                        core::ptr::with_exposed_provenance_mut::<u64>(at),
                        bytes / size_of::<u64>(),
                    )
                }
            });
        Histogram {
            samples,
            total: 0,
            sum: 0,
        }
    }

    /// Forget what was counted, keeping the room.
    fn clear(&mut self) {
        self.total = 0;
        self.sum = 0;
    }

    /// The samples counted, sorted.
    fn sorted(&mut self) -> &[u64] {
        let sorted = self.samples.get_mut(..self.total).unwrap_or_default();
        sorted.sort_unstable();
        sorted
    }

    /// The least sample, and those at the 50th, 90th and 99th percentiles,
    /// in ticks.
    fn points(&mut self) -> [u64; 4] {
        let sorted = self.sorted();
        let at = |per_mille: usize| {
            sorted
                .get(sorted.len() * per_mille / 1000)
                .copied()
                .unwrap_or(0)
        };
        [at(0), at(500), at(900), at(990)]
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
        self.line_per(what, scale, 1)
    }

    /// [`Histogram::line`] for samples that each timed `per` calls, printed
    /// per call; with `batch=<per>` after the count when `per` is not 1.
    fn line_per(&mut self, what: &str, scale: u64, per: u64) -> Line {
        let per = per.max(1);
        let ns = |ticks: u64| ticks.saturating_mul(scale) / (1000 * per);
        let total = self.total;
        let mean = self.sum / (total.max(1) as u64);
        let [min, p50, p90, p99] = self.points();
        if per == 1 {
            format_line(format_args!(
                "ipc-bench {what} n={total} min={} p50={} p90={} p99={} mean={}",
                ns(min),
                ns(p50),
                ns(p90),
                ns(p99),
                ns(mean),
            ))
        } else {
            format_line(format_args!(
                "ipc-bench {what} n={total} batch={per} min={} p50={} p90={} p99={} mean={}",
                ns(min),
                ns(p50),
                ns(p90),
                ns(p99),
                ns(mean),
            ))
        }
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
