//! memkeep, free memory kept above a mark (`docs/MEMKEEP.md`).
//!
//! Started by init as `memkeep.service` (`Type=native`). It says READY on
//! its bootstrap channel, reads its marks once from
//! `/etc/ferrix/memkeep.conf`, then once a second reads `MemFree`; while it
//! is under `low`, it asks the top-level slice holding the most clean file
//! cache for `chunk` bytes of it through that slice's `memory.reclaim`, one
//! ask a second at most. A slice whose ask left free memory where it was
//! (it rose by less than a quarter of `chunk` and did not fall) is left
//! alone for ten seconds. The reclaim, and every
//! bound on it, is the kernel's; only the pace is this program's. It is a
//! pacer, not a promise: nothing in the kernel rests on it. When init closes
//! the channel it exits 0; any other status names the step that went wrong.

#![no_std]
#![no_main]

use ferrix_native_abi::directory::{MAX_MESSAGE, Message};
use ferrix_rt::Bootstrap;
use ferrix_rt::linux::{call, numbers};
use ferrix_rt::native::{Deadline, Error, Object, Signals};

ferrix_rt::entry!(main);

/// How a run ended, as its exit status.
#[derive(Debug, Clone, Copy)]
#[repr(i32)]
enum Step {
    NoBootstrap = 1,
    SayReady = 2,
    Wait = 3,
}

/// A top-level slice init makes: its name, its `memory.stat` and its
/// `memory.reclaim`, each path NUL-terminated.
#[derive(Debug, Clone, Copy)]
struct Slice {
    name: &'static [u8],
    stat: &'static [u8],
    reclaim: &'static [u8],
}

/// The top-level cgroups init makes (`ferrix_svc::kind`: `SYSTEM_SLICE`,
/// `DRIVERS_SLICE`, `INIT_SCOPE`, and `user.slice` for the users' sessions).
/// Fixed, not configured: these are the only ones that hold the machine's
/// cache, and a slice that is not there is skipped.
const SLICES: [Slice; 4] = [
    Slice {
        name: b"system.slice",
        stat: b"/sys/fs/cgroup/system.slice/memory.stat\0",
        reclaim: b"/sys/fs/cgroup/system.slice/memory.reclaim\0",
    },
    Slice {
        name: b"user.slice",
        stat: b"/sys/fs/cgroup/user.slice/memory.stat\0",
        reclaim: b"/sys/fs/cgroup/user.slice/memory.reclaim\0",
    },
    Slice {
        name: b"drivers.slice",
        stat: b"/sys/fs/cgroup/drivers.slice/memory.stat\0",
        reclaim: b"/sys/fs/cgroup/drivers.slice/memory.reclaim\0",
    },
    Slice {
        name: b"init.scope",
        stat: b"/sys/fs/cgroup/init.scope/memory.stat\0",
        reclaim: b"/sys/fs/cgroup/init.scope/memory.reclaim\0",
    },
];

/// The marks, `/etc/ferrix/memkeep.conf`.
#[derive(Debug, Clone, Copy)]
struct Config {
    /// KiB of `MemFree` below which it asks.
    low: u64,
    /// Bytes asked of a slice in one ask, whole pages.
    chunk: u64,
}

/// A page, the unit `memory.reclaim` gives back in.
const PAGE: u64 = 4096;

/// The least `low` the defaults give, in KiB: the kernel's own watermark.
const LEAST_LOW_KIB: u64 = 32 * 1024;

/// The least `chunk` the defaults give, in bytes.
const LEAST_CHUNK: u64 = 1024 * 1024;

/// One second, and how long a slice that gave too little is left alone.
const SECOND_NS: u64 = 1_000_000_000;
const QUIET_NS: u64 = 10 * SECOND_NS;

/// The least time between two lines that sum its asks.
const SAY_EVERY_NS: u64 = 10 * SECOND_NS;

fn main(bootstrap: Bootstrap) -> i32 {
    match serve(bootstrap) {
        Ok(()) => 0,
        Err(step) => step as i32,
    }
}

/// What the asks since the last line came to.
#[derive(Debug, Clone, Copy, Default)]
struct Tally {
    /// Asks made, per slice of [`SLICES`].
    asks: [u64; 4],
    /// Asks that lifted `MemFree` by less than a quarter of a chunk.
    slow: u64,
    /// Asks the write refused (`EAGAIN` after sixteen empty rounds, say).
    refused: u64,
    /// `MemFree` before the first ask, and after the last, in KiB.
    before: u64,
    after: u64,
}

impl Tally {
    fn total(&self) -> u64 {
        self.asks.iter().sum()
    }
}

/// Say READY, read the marks, and keep free memory above `low` until init
/// lets go.
fn serve(bootstrap: Bootstrap) -> Result<(), Step> {
    let channel = bootstrap.ok_or(Step::NoBootstrap)?;
    let mut out = [0_u8; MAX_MESSAGE];
    let len = Message::ready().encode(&mut out).ok_or(Step::SayReady)?;
    channel
        .write(out.get(..len).unwrap_or_default())
        .map_err(|_| Step::SayReady)?;
    let config = config(meminfo().map_or(0, |info| info.0));
    say(&[
        b"memkeep: keeping MemFree at or above ",
        number(config.low).as_slice(),
        b" kB, ",
        number(config.chunk).as_slice(),
        b" bytes an ask of the top-level slice with the most file cache\n",
    ]);
    let mut quiet_until = [0_u64; 4];
    let mut tally = Tally::default();
    let mut said_at: Option<u64> = None;
    loop {
        let deadline = Deadline::At(now_ns().saturating_add(SECOND_NS));
        match channel.wait_one(Signals::PEER_CLOSED, deadline) {
            Ok(seen) if seen.intersects(Signals::PEER_CLOSED) => return Ok(()),
            Ok(_) | Err(Error::TimedOut) => {}
            Err(_) => return Err(Step::Wait),
        }
        keep(&config, &mut quiet_until, &mut tally);
        let now = now_ns();
        let due = said_at.is_none_or(|at| now.saturating_sub(at) >= SAY_EVERY_NS);
        if tally.total() > 0 && due {
            say_tally(&tally, config.chunk);
            tally = Tally::default();
            said_at = Some(now);
        }
    }
}

/// One second's work: if `MemFree` is under `low`, one ask of the slice
/// with the most file cache that has at least two chunks of it and is not
/// being left alone.
fn keep(config: &Config, quiet_until: &mut [u64; 4], tally: &mut Tally) {
    let Some((_, free)) = meminfo() else {
        return;
    };
    if free >= config.low {
        return;
    }
    let now = now_ns();
    let mut best: Option<(usize, u64)> = None;
    for (at, slice) in SLICES.iter().enumerate() {
        if quiet_until.get(at).is_some_and(|until| now < *until) {
            continue;
        }
        let file = file_cache(slice);
        let enough = file >= config.chunk.saturating_mul(2);
        if enough && best.is_none_or(|(_, most)| file > most) {
            best = Some((at, file));
        }
    }
    let Some((at, _)) = best else {
        return;
    };
    let Some(slice) = SLICES.get(at) else {
        return;
    };
    let given = ask(slice, config.chunk);
    let after = meminfo().map_or(free, |info| info.1);
    let rose = after.saturating_sub(free).saturating_mul(1024);
    if tally.total() == 0 {
        tally.before = free;
    }
    tally.after = after;
    if let Some(count) = tally.asks.get_mut(at) {
        *count += 1;
    }
    if !given {
        tally.refused += 1;
    }
    // An ask the kernel took while `MemFree` fell is not one without
    // progress: something took more than the ask gave, which is when the
    // next ask matters most (a game entering its world took 256 MiB in a
    // second on the N150). Without progress is an ask that was taken and
    // left `MemFree` where it was.
    let fell = after < free;
    if !given || (!fell && rose < config.chunk / 4) {
        tally.slow += u64::from(given);
        if let Some(until) = quiet_until.get_mut(at) {
            *until = now_ns().saturating_add(QUIET_NS);
        }
    }
}

/// The line that sums the asks since the last one.
fn say_tally(tally: &Tally, chunk: u64) {
    let mut line = Line::new();
    line.push(&[
        b"memkeep: asks ",
        number(tally.total()).as_slice(),
        b" of ",
        number(chunk).as_slice(),
        b" bytes (",
    ]);
    let mut gap: &[u8] = b"";
    for (slice, count) in SLICES.iter().zip(tally.asks.iter()) {
        if *count > 0 {
            line.push(&[gap, slice.name, b" ", number(*count).as_slice()]);
            gap = b", ";
        }
    }
    line.push(&[
        b"), ",
        number(tally.slow).as_slice(),
        b" without progress, ",
        number(tally.refused).as_slice(),
        b" refused; MemFree ",
        number(tally.before).as_slice(),
        b" kB before, ",
        number(tally.after).as_slice(),
        b" kB after\n",
    ]);
    line.write();
}

/// The marks, the defaults for anything not given; a file with a key or
/// value it does not take is not used, and that is said once.
fn config(total: u64) -> Config {
    let defaults = |low: Option<u64>| {
        let low = low.unwrap_or_else(|| (total / 8).max(LEAST_LOW_KIB));
        let chunk = (low.saturating_mul(1024) / 8 / PAGE * PAGE).max(LEAST_CHUNK);
        Config { low, chunk }
    };
    let Some((bytes, len)) = read_file(b"/etc/ferrix/memkeep.conf\0") else {
        return defaults(None);
    };
    let text = core::str::from_utf8(bytes.get(..len).unwrap_or_default()).unwrap_or("");
    let mut low = None;
    let mut chunk = None;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().parse::<u64>().ok();
        match (key.trim(), value) {
            ("low", Some(kib)) if (1024..=1 << 40).contains(&kib) => low = Some(kib),
            ("chunk", Some(bytes)) if (PAGE..=1 << 40).contains(&bytes) => {
                chunk = Some(bytes / PAGE * PAGE);
            }
            _ => {
                say(&[
                    b"memkeep: memkeep.conf has a key or value it does not take; the defaults\n",
                ]);
                return defaults(None);
            }
        }
    }
    let mut config = defaults(low);
    if let Some(chunk) = chunk {
        config.chunk = chunk;
    }
    config
}

/// `MemTotal` and `MemFree` from `/proc/meminfo`, in KiB.
fn meminfo() -> Option<(u64, u64)> {
    let (bytes, len) = read_file(b"/proc/meminfo\0")?;
    let text = core::str::from_utf8(bytes.get(..len).unwrap_or_default()).ok()?;
    let field = |label: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(label))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
    };
    Some((field("MemTotal:")?, field("MemFree:")?))
}

/// The bytes of file cache charged to `slice` and beneath it, from its
/// `memory.stat` (`file`, the second of its nine lines); 0 for a slice that
/// is not there.
fn file_cache(slice: &Slice) -> u64 {
    let Some((bytes, len)) = read_file(slice.stat) else {
        return 0;
    };
    let text = core::str::from_utf8(bytes.get(..len).unwrap_or_default()).unwrap_or("");
    text.lines()
        .find_map(|line| line.strip_prefix("file "))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

/// Ask `slice` for `chunk` bytes of its clean cache; whether the write was
/// taken. It returns when the kernel has given back what it could.
fn ask(slice: &Slice, chunk: u64) -> bool {
    const O_WRONLY: usize = 1;
    let text = number(chunk);
    let digits = text.as_slice();
    // SAFETY: `slice.reclaim` is NUL-terminated and lives for the call.
    let fd = unsafe {
        call(
            numbers::OPENAT,
            [
                (-100_isize) as usize,
                slice.reclaim.as_ptr() as usize,
                O_WRONLY,
                0,
                0,
                0,
            ],
        )
    };
    let Ok(fd) = fd else {
        return false;
    };
    // SAFETY: `digits` is readable for its whole length for the call.
    let wrote = unsafe {
        call(
            numbers::WRITE,
            [fd, digits.as_ptr() as usize, digits.len(), 0, 0, 0],
        )
    };
    // SAFETY: no pointer arguments.
    let _ = unsafe { call(numbers::CLOSE, [fd, 0, 0, 0, 0, 0]) };
    matches!(wrote, Ok(count) if count == digits.len())
}

/// A small file's bytes, at most 4 KiB, and how many: `/proc/meminfo` is
/// twelve lines and `memory.stat` nine, a few hundred bytes each.
fn read_file(path: &[u8]) -> Option<([u8; 4096], usize)> {
    // SAFETY: `path` is NUL-terminated and lives for the call.
    let fd = unsafe {
        call(
            numbers::OPENAT,
            [(-100_isize) as usize, path.as_ptr() as usize, 0, 0, 0, 0],
        )
    }
    .ok()?;
    let mut buf = [0_u8; 4096];
    // SAFETY: `buf` is writable for its whole length for the call.
    let read = unsafe {
        call(
            numbers::READ,
            [fd, buf.as_mut_ptr() as usize, buf.len(), 0, 0, 0],
        )
    };
    // SAFETY: no pointer arguments.
    let _ = unsafe { call(numbers::CLOSE, [fd, 0, 0, 0, 0, 0]) };
    Some((buf, read.ok()?))
}

/// The monotonic clock, in nanoseconds.
fn now_ns() -> u64 {
    let mut spec = [0_isize; 2];
    // SAFETY: `spec` is a `timespec` of two machine words, writable for the
    // call.
    let read = unsafe {
        call(
            numbers::CLOCK_GETTIME,
            [1, spec.as_mut_ptr() as usize, 0, 0, 0, 0],
        )
    };
    if read.is_err() {
        return 0;
    }
    let [seconds, nanos] = spec;
    (seconds.max(0) as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nanos.max(0) as u64)
}

/// Write the pieces to standard output, which init hands to the log, as
/// one write, so that no other writer's line lands inside it.
fn say(pieces: &[&[u8]]) {
    let mut line = Line::new();
    line.push(pieces);
    line.write();
}

/// One line of output, gathered before it is written; what does not fit is
/// cut.
struct Line {
    bytes: [u8; 512],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Self {
            bytes: [0; 512],
            len: 0,
        }
    }

    fn push(&mut self, pieces: &[&[u8]]) {
        for piece in pieces {
            for byte in *piece {
                if let Some(slot) = self.bytes.get_mut(self.len) {
                    *slot = *byte;
                    self.len += 1;
                }
            }
        }
    }

    fn write(&self) {
        let text = self.bytes.get(..self.len).unwrap_or_default();
        // SAFETY: `text` is readable for its whole length for the call.
        let _ = unsafe {
            call(
                numbers::WRITE,
                [1, text.as_ptr() as usize, text.len(), 0, 0, 0],
            )
        };
    }
}

/// `value` in decimal.
fn number(value: u64) -> NumberText {
    let mut digits = [0_u8; 20];
    let mut at = digits.len();
    let mut left = value;
    loop {
        at -= 1;
        if let Some(slot) = digits.get_mut(at) {
            *slot = b'0' + (left % 10) as u8;
        }
        left /= 10;
        if left == 0 {
            break;
        }
    }
    NumberText { digits, start: at }
}

/// A number's decimal digits.
struct NumberText {
    digits: [u8; 20],
    start: usize,
}

impl NumberText {
    fn as_slice(&self) -> &[u8] {
        self.digits.get(self.start..).unwrap_or_default()
    }
}
