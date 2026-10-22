//! `power`: the x86-64 power driver (`docs/POWERCTL.md` §8).
//!
//! `devmgr` starts it on the kernel's power node, which exists only under
//! `ferrix.powerctl` on a processor the kernel's model table names (or under
//! the test-only `ferrix.powerctl.fake`). It claims the kernel's power core
//! with `power_control_create`, then once a second reads the kernel's
//! readings with `power_read` -- which is also what keeps its lease -- and
//! decides, by `ferrix_x86_power`'s policy: the next step of the plan its
//! boot words give, or a step down when the hottest reading reaches the soft
//! limit, or firmware's value for good after three. It writes with
//! `power_set`; the kernel holds the bounds, the raise floor, its own
//! backstop and every restore.
//!
//! Its lines keep `ferrix.vrcurrent`'s shapes, so the box's scripts read
//! them as before: `msr: vrcurrent set N A (was M A)` for each write the
//! kernel took, `msr: vrcurrent N A, core C C, package P C` every five
//! seconds, with the ratio, the limit reasons and the package's power.
//! The kernel says its own writes and restores in raw hex (`power: ...`).
//!
//! Without `ferrix.power.vr` it writes nothing and only says what it reads.
//! `ferrix.power.test=hang@S` and `exit@S` stop renewing, or exit, S seconds
//! after its first write, for `cargo xtask test-power`; with either it first
//! checks the claim's own refusals (a second claim, a copy of the claim, the
//! node's memory, mapping and interrupt, a quiesce under it) and says each.

#![no_std]
#![no_main]

use core::fmt::{self, Write as _};

use ferrix_blkring::control::{Message as StartMessage, START_BYTES};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{IoMappingSpec, POWER_READ_WORDS};
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::device::Device;
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::pin::PinAccess;
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo;
use ferrix_rt::{Bootstrap, Kernel};
use ferrix_x86_power::decode::{self, Amps, Reading};
use ferrix_x86_power::options::{self, Options, TestMode, Words};
use ferrix_x86_power::policy::{Action, Policy, Why};

ferrix_rt::entry!(main);

/// The boot words the driver reads (`docs/POWERCTL.md` §9): ring 3's,
/// not the kernel's, read from `/proc/cmdline`.
const WORDS: Words<'static> = Words {
    vr: b"ferrix.power.vr",
    hot: b"ferrix.power.hot",
    test: b"ferrix.power.test",
};

/// A second, in nanoseconds.
const SECOND: u64 = 1_000_000_000;
/// Seconds between temperature lines.
const LOG_EVERY: u64 = 5;

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Step {
    /// No bootstrap channel, or no START.
    Start = 1,
    /// The kernel refused the claim.
    Claim = 2,
    /// The kernel ended the claim.
    Ended = 3,
    /// `ferrix.power.test=exit@S`.
    TestExit = 4,
    /// No port to sleep on.
    Port = 5,
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return Step::Start as i32;
    };
    match run(&boot) {
        Ok(()) => 0,
        Err(step) => {
            say(format_args!("power: stopped at {step:?}"));
            step as i32
        }
    }
}

/// The device START names.
fn started(boot: &Channel<Kernel>) -> Result<Device<Kernel>, Step> {
    let _ = boot
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Step::Start)?;
    let mut bytes = [0_u8; START_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let received = boot
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Start)?;
    if received.handles != 1 {
        return Err(Step::Start);
    }
    let device = Device::from_owned(OwnedHandle::from_raw(Kernel, handles[0]));
    match StartMessage::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(StartMessage::Start(_)) => Ok(device),
        _ => Err(Step::Start),
    }
}

fn run(boot: &Channel<Kernel>) -> Result<(), Step> {
    let device = started(boot)?;
    let options = read_options();
    let claim = device.power_control().map_err(|error| {
        say(format_args!(
            "power: the kernel refused the claim: {error:?}"
        ));
        Step::Claim
    })?;
    if options.test.is_some() {
        refusals(&device, &claim);
    }
    let idle = port::create(Kernel).map_err(|_| Step::Port)?;
    let first = claim.power_read().map_err(|_| Step::Ended)?;
    let reading = decode::reading(&first);
    say(format_args!(
        "power: claimed: firmware {} A, ceiling {} A, back off at {} C",
        Amps(reading.firmware_eighths),
        Amps(reading.ceiling_eighths),
        options.hot
    ));
    let Some(plan) = options.plan else {
        say(format_args!(
            "power: no ferrix.power.vr plan: writing nothing"
        ));
        return watch(&claim, &idle, None, &options);
    };
    let Some((plan, dropped)) = plan.below(u32::try_from(reading.ceiling_eighths / 8).unwrap_or(0))
    else {
        say(format_args!(
            "power: every step of the plan is above the kernel's ceiling: writing nothing"
        ));
        return watch(&claim, &idle, None, &options);
    };
    if dropped > 0 {
        say(format_args!(
            "power: {dropped} steps above the kernel's ceiling dropped"
        ));
    }
    watch(
        &claim,
        &idle,
        Some(Policy::new(plan, options.hot)),
        &options,
    )
}

/// Read the boot words.
fn read_options() -> Options {
    let mut line = [0_u8; 1024];
    match read_file(b"/proc/cmdline\0", &mut line) {
        Ok(len) => {
            let options = options::parse(line.get(..len).unwrap_or_default(), &WORDS);
            if options.plan_refused.is_some() || options.hot_refused || options.test_refused {
                say(format_args!(
                    "power: a ferrix.power word did not parse and is ignored"
                ));
            }
            options
        }
        Err(errno) => {
            say(format_args!(
                "power: /proc/cmdline unreadable ({errno:?}): no plan"
            ));
            Options::default()
        }
    }
}

/// The loop: a reading a second, the policy's action, a line every five.
fn watch(
    claim: &Channel<Kernel>,
    idle: &Port<Kernel>,
    mut policy: Option<Policy>,
    options: &Options,
) -> Result<(), Step> {
    let start = now();
    let mut energy: Option<((u32, u32), u64)> = None;
    let mut first_write: Option<u64> = None;
    let mut elapsed = 0_u64;
    loop {
        let words = read(claim)?;
        let reading = decode::reading(&words);
        if let Some(policy) = policy.as_mut() {
            act(claim, policy, elapsed, &reading)?;
            if first_write.is_none() && policy.current().is_some() {
                first_write = Some(elapsed);
            }
        }
        if let (Some((mode, after)), Some(at)) = (options.test, first_write)
            && elapsed >= at + u64::from(after)
        {
            match mode {
                TestMode::Hang => {
                    say(format_args!("power: test: renewing no more"));
                    loop {
                        let _ = idle.wait(Deadline::Never);
                    }
                }
                TestMode::Exit => {
                    say(format_args!("power: test: exiting"));
                    return Err(Step::TestExit);
                }
            }
        }
        if elapsed.is_multiple_of(LOG_EVERY) {
            energy = report(&reading, energy);
        }
        elapsed += 1;
        let _ = idle.wait(Deadline::At(start.saturating_add(elapsed * SECOND)));
    }
}

/// `power_read`, or the claim's end said.
fn read(claim: &Channel<Kernel>) -> Result<[u64; POWER_READ_WORDS], Step> {
    claim.power_read().map_err(|error| {
        let closed = claim
            .wait_one(Signals::PEER_CLOSED, Deadline::At(0))
            .is_ok();
        say(format_args!(
            "power: the kernel ended the claim ({error:?}, closed {closed})"
        ));
        Step::Ended
    })
}

/// Do what the policy says this second.
fn act(
    claim: &Channel<Kernel>,
    policy: &mut Policy,
    elapsed: u64,
    reading: &Reading,
) -> Result<(), Step> {
    let was = policy.current();
    let (amps, why) = match policy.step(elapsed, reading) {
        Action::Hold => return Ok(()),
        Action::Set { amps, why } => (amps, Some(why)),
        Action::GiveUp { celsius } => {
            say(format_args!(
                "power: giving up at {celsius} C: firmware's limit for good"
            ));
            (
                u32::try_from(reading.firmware_eighths / 8).unwrap_or(0),
                None,
            )
        }
    };
    let eighths = u64::from(amps) * 8;
    let before = reading.vr_eighths.unwrap_or(0);
    match claim.power_set(eighths) {
        Ok(back) => {
            if let Some(Why::BackOff { celsius }) = why {
                say(format_args!("power: backing off to {amps} A: {celsius} C"));
            }
            say(format_args!(
                "msr: vrcurrent set {} A (was {} A)",
                Amps(back & 0x1fff),
                Amps(before)
            ));
            Ok(())
        }
        Err(Error::ShouldWait | Error::LimitReached | Error::InvalidArgs) => {
            say(format_args!("power: the kernel refused {amps} A for now"));
            policy.refused(was);
            Ok(())
        }
        Err(error) => {
            say(format_args!(
                "power: the kernel ended the claim at {amps} A ({error:?})"
            ));
            Err(Step::Ended)
        }
    }
}

/// The five-second line, `ferrix.vrcurrent`'s shape and more; the energy
/// reading kept for the next.
fn report(reading: &Reading, energy: Option<((u32, u32), u64)>) -> Option<((u32, u32), u64)> {
    let at = now() / 1_000_000;
    let power = energy
        .zip(reading.energy)
        .and_then(|((before, then), after)| {
            decode::milliwatts(before, after, at.saturating_sub(then))
        });
    let mut names = [""; 11];
    let count = decode::reason_names(
        reading.core_reasons.unwrap_or(0) | reading.graphics_reasons.unwrap_or(0),
        &mut names,
    );
    say(format_args!(
        "msr: vrcurrent {} A, core {} C, package {} C, ratio {}, package {} mW, limited by {}",
        Amps(reading.vr_eighths.unwrap_or(0)),
        Shown(reading.core),
        Shown(reading.package),
        Shown(reading.ratio),
        Shown(power),
        Names(names.get(..count).unwrap_or_default())
    ));
    reading.energy.map(|now| (now, at))
}

/// Check the claim's own refusals (`L.device.52`, `L.device.53`) and say
/// each, for `cargo xtask test-power`.
fn refusals(device: &Device<Kernel>, claim: &Channel<Kernel>) {
    let second = device.power_control().err();
    let copy = claim.duplicate(Requested::Same).err();
    let pin = vmo::create(Kernel, 4096)
        .ok()
        .and_then(|memory| device.pin(&memory, 0, 4096, PinAccess::ReadOnly).err());
    let mapping = device
        .io_mapping(IoMappingSpec { phys: 0, len: 4096 })
        .err();
    let interrupt = device.interrupt(0).err();
    let quiesce = device.quiesce().err();
    say(format_args!(
        "power: refusals: second claim {second:?}, copy {copy:?}, pin {pin:?}, mapping {mapping:?}, interrupt {interrupt:?}, quiesce {quiesce:?}"
    ));
}

/// The monotonic clock, in nanoseconds.
fn now() -> u64 {
    ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
}

/// A value, or `-` for one not read.
struct Shown<T>(Option<T>);

impl<T: fmt::Display> fmt::Display for Shown<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(value) => value.fmt(f),
            None => f.write_str("-"),
        }
    }
}

/// Limit reason names, or `none`.
struct Names<'a>(&'a [&'a str]);

impl fmt::Display for Names<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return f.write_str("none");
        }
        for (index, name) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(" ")?;
            }
            f.write_str(name)?;
        }
        Ok(())
    }
}

/// A small file, from its start, into `buf` (`path` NUL-terminated).
fn read_file(path: &[u8], buf: &mut [u8]) -> Result<usize, ferrix_linux_abi::errno::Errno> {
    use ferrix_rt::linux::{self, numbers};
    const AT_FDCWD: usize = (-100_isize) as usize;
    if path.last() != Some(&0) {
        return Err(ferrix_linux_abi::errno::Errno::EINVAL);
    }
    // SAFETY: `path` is NUL-terminated and borrowed for the call.
    let fd = unsafe {
        linux::call(
            numbers::OPENAT,
            [AT_FDCWD, path.as_ptr().addr(), 0, 0, 0, 0],
        )
    }?;
    let mut len = 0;
    while let Some(rest) = buf.get_mut(len..) {
        if rest.is_empty() {
            break;
        }
        match linux::read(fd, rest) {
            Ok(0) => break,
            Ok(got) => len += got,
            Err(errno) => {
                let _ = linux::close(fd);
                return Err(errno);
            }
        }
    }
    let _ = linux::close(fd);
    Ok(len)
}

/// A line on standard error, which a native process has open on the
/// console.
fn say(arguments: fmt::Arguments<'_>) {
    let mut line = Line {
        bytes: [0; 200],
        len: 0,
    };
    let _ = line.write_fmt(arguments);
    let _ = line.write_str("\n");
    let _ = ferrix_rt::linux::write(2, line.bytes.get(..line.len).unwrap_or_default());
}

/// A line, formatted into a fixed buffer and cut at its end.
struct Line {
    bytes: [u8; 200],
    len: usize,
}

impl fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for &byte in text.as_bytes() {
            if let Some(slot) = self.bytes.get_mut(self.len) {
                *slot = byte;
                self.len += 1;
            }
        }
        Ok(())
    }
}
