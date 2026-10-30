//! memscrub, the RAM scrubber's pace (`docs/RAM-SCRUBBER.md` §5).
//!
//! Started by init as `memscrub.service` (`Type=native`), with the memory
//! scrub token waiting first on its bootstrap channel (init's GRANT). It
//! says READY, then once a second reads how idle the processors were; while
//! they were at least `idle` percent idle it asks the kernel to test frames,
//! `batch` a call, as many as sweeping the free memory once a `period` takes.
//! Every bound -- budget, watermark, contiguity, the retired set's share, one
//! batch at a time -- is the kernel's; only the pace is this program's. It
//! says each sweep's end. When init closes the channel it exits 0; any other
//! status names the step that went wrong.

#![no_std]
#![no_main]

use ferrix_native_abi::directory::{self, Kind, MAX_MESSAGE, Message};
use ferrix_rt::linux::{call, numbers};
use ferrix_rt::native::channel::{Channel, ReadError};
use ferrix_rt::native::pending;
use ferrix_rt::native::{Deadline, Error, Handle, Object, OwnedHandle, Requested, Rights, Signals};
use ferrix_rt::{Bootstrap, Kernel};

ferrix_rt::entry!(main);

/// How a run ended, as its exit status.
#[derive(Debug, Clone, Copy)]
#[repr(i32)]
enum Step {
    NoBootstrap = 1,
    NoToken = 2,
    SayReady = 3,
    Wait = 4,
}

/// The configuration, `/etc/ferrix/memscrub.conf`.
#[derive(Debug, Clone, Copy)]
struct Config {
    /// Seconds for one sweep of free memory.
    period: u64,
    /// Percent of the last second the processors must have been idle.
    idle: u64,
    /// Frames a call, 1 to 64.
    batch: u64,
}

/// The defaults (§5).
const DEFAULT: Config = Config {
    period: 86_400,
    idle: 90,
    batch: 16,
};

/// The most calls in one second, so a short period cannot keep a processor
/// busy scrubbing.
const CALLS_A_SECOND: u64 = 64;

fn main(bootstrap: Bootstrap) -> i32 {
    match serve(bootstrap) {
        Ok(()) => 0,
        Err(step) => step as i32,
    }
}

/// Take the token, say READY, and pace the scrubber until init lets go.
fn serve(bootstrap: Bootstrap) -> Result<(), Step> {
    let channel = bootstrap.ok_or(Step::NoBootstrap)?;
    let token = take_token(&channel).ok_or(Step::NoToken)?;
    let mut out = [0_u8; MAX_MESSAGE];
    let len = Message::ready().encode(&mut out).ok_or(Step::SayReady)?;
    channel
        .write(out.get(..len).unwrap_or_default())
        .map_err(|_| Step::SayReady)?;
    let config = config();
    let mut last = cpu_times();
    let mut owed: u64 = 0;
    let mut sweeps: u64 = 0;
    let mut tested: u64 = 0;
    let mut said_down = false;
    loop {
        let deadline = Deadline::At(now_ns().saturating_add(1_000_000_000));
        match channel.wait_one(Signals::PEER_CLOSED, deadline) {
            Ok(seen) if seen.intersects(Signals::PEER_CLOSED) => return Ok(()),
            Ok(_) | Err(Error::TimedOut) => {}
            Err(_) => return Err(Step::Wait),
        }
        let now = cpu_times();
        let idle = idle_percent(last, now);
        last = now;
        if idle < config.idle {
            continue;
        }
        // Frames owed this second: the managed frames over the period, at
        // least one batch.
        owed = owed.saturating_add((managed() / config.period.max(1)).max(config.batch));
        let mut calls = 0;
        while owed > 0 && calls < CALLS_A_SECOND {
            calls += 1;
            match pending::memory_scrub(Kernel, &token, config.batch as usize) {
                Ok(batch) => {
                    owed = owed.saturating_sub(config.batch);
                    tested += batch.tested;
                    if batch.sweeps > 0 {
                        sweeps += batch.sweeps;
                        say(&[
                            b"memscrub: sweep ",
                            number(sweeps).as_slice(),
                            b" done, ",
                            number(tested).as_slice(),
                            b" frames tested\n",
                        ]);
                        tested = 0;
                    }
                    if batch.stopped {
                        owed = 0;
                    }
                }
                Err(Error::WrongType) => {
                    if !said_down {
                        say(&[b"memscrub: the scrubber stands down; nothing to pace\n"]);
                        said_down = true;
                    }
                    owed = 0;
                }
                Err(_) => owed = 0,
            }
        }
    }
}

/// The token from init's GRANT, which waits first on the channel; reduced
/// to `MANAGE`. Nothing else can be first: init answers only what this
/// program sends.
fn take_token(channel: &Channel<Kernel>) -> Option<OwnedHandle<Kernel>> {
    let mut bytes = [0_u8; MAX_MESSAGE];
    let mut handles = [Handle::INVALID; 1];
    let received = match channel.read(&mut bytes, &mut handles) {
        Ok(received) => received,
        Err(ReadError::Failed(_) | ReadError::TooSmall { .. }) => return None,
    };
    if received.handles != 1 {
        return None;
    }
    let handle = OwnedHandle::from_raw(Kernel, handles[0]);
    let message = Message::decode(bytes.get(..received.bytes).unwrap_or_default())?;
    if message.kind != Kind::Grant || message.name != directory::MEMORY_SCRUB {
        return None;
    }
    handle.replace(Requested::Exactly(Rights::MANAGE)).ok()
}

/// The configuration, the defaults for anything not given; a file with a
/// value out of range is not used, and that is said once.
fn config() -> Config {
    let Some((bytes, len)) = read_file(b"/etc/ferrix/memscrub.conf\0") else {
        return DEFAULT;
    };
    let text = core::str::from_utf8(bytes.get(..len).unwrap_or_default()).unwrap_or("");
    let mut config = DEFAULT;
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Ok(value) = value.trim().parse::<u64>() else {
            say(&[b"memscrub: memscrub.conf has a value that is not a number; the defaults\n"]);
            return DEFAULT;
        };
        let fits = match key.trim() {
            "period" => (10..=31 * 86_400).contains(&value),
            "idle" => value <= 100,
            "batch" => (1..=64).contains(&value),
            _ => false,
        };
        if !fits {
            say(&[b"memscrub: memscrub.conf has a key or value out of range; the defaults\n"]);
            return DEFAULT;
        }
        match key.trim() {
            "period" => config.period = value,
            "idle" => config.idle = value,
            _ => config.batch = value,
        }
    }
    config
}

/// `/proc/stat`'s first line: the processors' time in total and idle, in
/// ticks.
fn cpu_times() -> (u64, u64) {
    let Some((bytes, len)) = read_file(b"/proc/stat\0") else {
        return (0, 0);
    };
    let text = core::str::from_utf8(bytes.get(..len).unwrap_or_default()).unwrap_or("");
    let Some(line) = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("cpu "))
    else {
        return (0, 0);
    };
    let mut total = 0u64;
    let mut idle = 0u64;
    for (at, word) in line.split_whitespace().enumerate() {
        let value = word.parse::<u64>().unwrap_or(0);
        total = total.saturating_add(value);
        // idle and iowait.
        if at == 3 || at == 4 {
            idle = idle.saturating_add(value);
        }
    }
    (total, idle)
}

/// How idle the processors were between two readings, in percent.
fn idle_percent(then: (u64, u64), now: (u64, u64)) -> u64 {
    let total = now.0.saturating_sub(then.0);
    let idle = now.1.saturating_sub(then.1);
    if total == 0 {
        return 0;
    }
    idle.saturating_mul(100) / total
}

/// The frames the allocator manages, from `/proc/ferrix-scrub`.
fn managed() -> u64 {
    let Some((bytes, len)) = read_file(b"/proc/ferrix-scrub\0") else {
        return 0;
    };
    let text = core::str::from_utf8(bytes.get(..len).unwrap_or_default()).unwrap_or("");
    text.lines()
        .find_map(|line| line.strip_prefix("managed "))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

/// A small file's bytes, at most 4 KiB, and how many.
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

/// Write the pieces to standard output, which init hands to the log.
fn say(pieces: &[&[u8]]) {
    for piece in pieces {
        // SAFETY: `piece` is readable for its whole length for the call.
        let _ = unsafe {
            call(
                numbers::WRITE,
                [1, piece.as_ptr() as usize, piece.len(), 0, 0, 0],
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
