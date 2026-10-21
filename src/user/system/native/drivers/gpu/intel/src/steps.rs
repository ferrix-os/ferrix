//! Finding where the box hangs (2026-10-19). Booting with
//! `ferrix.intelgpu` hard-hung the N150 box twice, with no panic, no timed
//! reset and nobody at the screen, so the RAM log was lost to the power
//! cycle. Three words on the kernel command line, read from `/proc/cmdline`,
//! let one boot show where:
//!
//! * `ferrix.intelgpu.stop=N` stops the driver before step `N` of [`At`]:
//!   it says so and stays, touching nothing more (it never exits, since
//!   devmgr would then quiesce the function under the lit screen). `stop=1`
//!   starts the driver and touches nothing, which splits a hang in the
//!   kernel's part from one in the driver's.
//! * `ferrix.intelgpu.trace` says every register access before it is made:
//!   `intel: about to read 0x...` and `intel: about to write 0x... <- 0x...`,
//!   GGTT entries written included. The kernel draws a program's console
//!   lines on the screen under `ferrix.fbcon` before the write returns, with
//!   the write-combining buffers drained (`console::screen`), so a photograph
//!   of a hung box shows the last access asked for.
//!
//! * `ferrix.intelgpu.reset=S` restarts the machine S seconds after boot
//!   (`reboot(2)`, the disks committed first), whatever step the driver is
//!   in: a box whose init never resets it, or a driver waiting forever,
//!   comes back on its own. Only a processor stuck in a register access
//!   misses it.
//!
//! Each step's line is said before the step whether or not any word is
//! given: `intel: step N (Name): what`.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_intel_xelp::options::{self, Options, Words};

use crate::say;

/// The steps, in the order the driver takes them. The number is what
/// `ferrix.intelgpu.stop` names; the box's run plan cites them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub(crate) enum At {
    /// BAR 0 mapped: the kernel's `io_mapping`, no device access.
    MapBar0 = 1,
    /// Configuration space read through the kernel: `GMCH_CTRL`, `BSM`.
    Config = 2,
    /// Forcewake of the render and GT domains.
    Forcewake = 3,
    /// `GuC`'s `WOPCM` registers (GT domain).
    Wopcm = 4,
    /// The slice, DSS, EU and media fuses (GT domain).
    Fuses = 5,
    /// The command streamer's timestamp frequency.
    Clock = 6,
    /// `RP0`, `RP1` and `RPn`, through the MCHBAR mirror.
    RpCaps = 7,
    /// Pipe A's plane 1 (display registers).
    Plane = 8,
    /// The GGTT's entries, read only, for at most 200 ms.
    GgttWalk = 9,
    /// M2: forcewake of every domain the fuses leave, media included.
    ForcewakeAll = 10,
    /// M2: the pool pinned (the kernel turns bus mastering on).
    Pin = 11,
    /// M2: the PAT written and read back.
    Pat = 12,
    /// M2: the GT workarounds, the `WOPCM` check and the MOCS.
    GtInit = 13,
    /// M2: the GT's interrupt banks reset.
    IrqReset = 14,
    /// M2: the MSI claimed and bound (the kernel writes its capability).
    Msi = 15,
    /// M2: RCS0's status page bound, the engine sanitized and resumed.
    Rcs0 = 16,
    /// M2: BCS0's, the same.
    Bcs0 = 17,
    /// M2: the GT's interrupts and the master enable on.
    IrqOn = 18,
    /// M2: a PPGTT mapping made and walked back (memory only).
    Ppgtt = 19,
    /// M2, checkpoint 2: the default images and `MI_STORE_DWORD_IMM`: the
    /// first commands the GPU runs.
    Commands = 20,
    /// M2, checkpoint 3: the blits and the PPGTT batch.
    Blits = 21,
    /// M2, checkpoint 4: a hung batch reset and banned.
    Hang = 22,
    /// M3: the GT under the back end, and the render node served.
    Serve = 23,
}

/// What the command line asked for.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Steps {
    /// Stop before this step.
    pub(crate) stop: Option<u32>,
    /// Say every register access before it is made.
    pub(crate) trace: bool,
    /// Run M2's checks before serving (`ferrix.intelgpu.m2`).
    pub(crate) selftest: bool,
    /// A fixed GT frequency request in MHz (`ferrix.intelgpu.freq=MHz`).
    pub(crate) freq_mhz: Option<u32>,
    /// The per-second frequency log (`ferrix.intelgpu.freqlog`).
    pub(crate) freqlog: bool,
    /// The render node's trace (`ferrix.intelgpu.uapitrace`): held waits,
    /// their answers, failed ioctls, and a wait unanswered for 2 s named
    /// with its fences, without `trace`'s every register access.
    pub(crate) uapitrace: bool,
    /// The box's frequency checkpoint (`ferrix.intelgpu.freqtest`).
    pub(crate) freqtest: bool,
    /// Hold render and GT forcewake for the driver's life
    /// (`ferrix.intelgpu.fwhold`), as before forcewake was taken per access.
    pub(crate) fwhold: bool,
}

/// The command-line words, as the kernel's command line says them.
const WORDS: Words<'static> = Words {
    stop: b"ferrix.intelgpu.stop=",
    trace: b"ferrix.intelgpu.trace",
    reset: b"ferrix.intelgpu.reset=",
    selftest: b"ferrix.intelgpu.m2",
    freq: b"ferrix.intelgpu.freq=",
    freqlog: b"ferrix.intelgpu.freqlog",
    freqtest: b"ferrix.intelgpu.freqtest",
    uapitrace: b"ferrix.intelgpu.uapitrace",
    fwhold: b"ferrix.intelgpu.fwhold",
};

/// The driver stopped before a step, as asked.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stopped;

impl Steps {
    /// Read `/proc/cmdline` (`ferrix_intel_xelp::options`), and arm the
    /// restart it asks for. Unreadable, it is said, and nothing is asked.
    pub(crate) fn from_command_line() -> Steps {
        let mut line = [0_u8; 2048];
        let options = read_cmdline(&mut line).map_or_else(Options::default, |len| {
            options::parse(line.get(..len).unwrap_or_default(), &WORDS)
        });
        say(format_args!(
            "intel: ferrix.intelgpu.stop {}, trace {}, reset {}, m2 {}, freq {}, freqlog {}, freqtest {}, uapitrace {}, fwhold {}",
            Show(options.stop),
            if options.trace { "on" } else { "off" },
            Show(options.reset_seconds),
            if options.selftest { "on" } else { "off" },
            Show(options.freq_mhz),
            if options.freqlog { "on" } else { "off" },
            if options.freqtest { "on" } else { "off" },
            if options.uapitrace { "on" } else { "off" },
            if options.fwhold { "on" } else { "off" },
        ));
        if options.reset_refused {
            say(format_args!(
                "intel: ferrix.intelgpu.reset refused: seconds 1 to {} after boot; no restart",
                options::RESET_MAX_SECONDS
            ));
        }
        if let Some(seconds) = options.reset_seconds {
            RESET_AT_NANOS.store(u64::from(seconds) * 1_000_000_000, Ordering::Relaxed);
            say(format_args!(
                "intel: the machine restarts {seconds} s after boot (ferrix.intelgpu.reset), whatever this driver is doing then"
            ));
        }
        Steps {
            stop: options.stop,
            trace: options.trace,
            selftest: options.selftest,
            freq_mhz: options.freq_mhz,
            freqlog: options.freqlog,
            freqtest: options.freqtest,
            uapitrace: options.uapitrace,
            fwhold: options.fwhold,
        }
    }

    /// Say step `at` before it is taken; [`Stopped`] if the command line
    /// asked to stop before it.
    pub(crate) fn before(&self, at: At, what: fmt::Arguments<'_>) -> Result<(), Stopped> {
        let n = at as u32;
        if self.stop.is_some_and(|stop| stop <= n) {
            say(format_args!(
                "intel: stopped before step {n} ({at:?}) as ferrix.intelgpu.stop asks; the driver now waits and touches nothing"
            ));
            return Err(Stopped);
        }
        say(format_args!("intel: step {n} ({at:?}): {what}"));
        Ok(())
    }
}

/// When `ferrix.intelgpu.reset` restarts the machine, in nanoseconds after
/// boot on the monotonic clock; 0 for never.
static RESET_AT_NANOS: AtomicU64 = AtomicU64::new(0);

/// When the restart is due, if one is armed.
pub(crate) fn reset_at_nanos() -> Option<u64> {
    Some(RESET_AT_NANOS.load(Ordering::Relaxed)).filter(|&at| at != 0)
}

/// Restart the machine if `now` (monotonic nanoseconds) is past the armed
/// time: `reboot(2)` `RESTART`, which commits the disks first, as usbdev's
/// restart does. Every wait and every clock read of the driver comes here,
/// so the restart comes whatever step the driver is in, unless its
/// processor is stuck in a register access. Refused, it is said once and
/// disarmed.
pub(crate) fn reset_if_due(now: u64) {
    let Some(at) = reset_at_nanos() else {
        return;
    };
    if now < at {
        return;
    }
    RESET_AT_NANOS.store(0, Ordering::Relaxed);
    say(format_args!(
        "intel: restarting the machine, {} s after boot, as ferrix.intelgpu.reset asks",
        at / 1_000_000_000
    ));
    if let Err(error) = ferrix_rt::linux::reboot_restart() {
        say(format_args!("intel: the restart was refused: {error:?}"));
    }
}

/// `Option<u32>` said as a number or `none`.
struct Show(Option<u32>);

impl fmt::Display for Show {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(n) => write!(f, "{n}"),
            None => f.write_str("none"),
        }
    }
}

/// `/proc/cmdline` into `line`: its length, or `None`, said.
fn read_cmdline(line: &mut [u8]) -> Option<usize> {
    read_file(b"/proc/cmdline\0", line).map_err(unreadable).ok()
}

/// System memory in bytes, from `/proc/meminfo`'s `MemTotal` (in kB): what
/// the uAPI's `MEMORY_REGIONS` tells Mesa, which sizes its heaps from it.
pub(crate) fn system_memory_bytes() -> Option<u64> {
    let mut text = [0_u8; 2048];
    let len = read_file(b"/proc/meminfo\0", &mut text).ok()?;
    let line = text
        .get(..len)?
        .split(|&b| b == b'\n')
        .find(|l| l.starts_with(b"MemTotal:"))?;
    let kb = line
        .iter()
        .filter(|b| b.is_ascii_digit())
        .fold(0_u64, |n, &d| {
            n.saturating_mul(10).saturating_add(u64::from(d - b'0'))
        });
    (kb > 0).then_some(kb.saturating_mul(1024))
}

/// A small file, from its start, into `buf` (`path` NUL-terminated): its
/// length.
fn read_file(path: &[u8], buf: &mut [u8]) -> Result<usize, ferrix_linux_abi::errno::Errno> {
    use ferrix_rt::linux::{self, numbers};
    const AT_FDCWD: usize = (-100_isize) as usize;
    const O_RDONLY: usize = 0;
    if path.last() != Some(&0) {
        return Err(ferrix_linux_abi::errno::Errno::EINVAL);
    }
    // SAFETY: `path` is NUL-terminated and borrowed for the call.
    let fd = unsafe {
        linux::call(
            numbers::OPENAT,
            [AT_FDCWD, path.as_ptr().addr(), O_RDONLY, 0, 0, 0],
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

fn unreadable(errno: impl fmt::Debug) {
    say(format_args!(
        "intel: /proc/cmdline unreadable ({errno:?}): no ferrix.intelgpu.stop, trace or reset"
    ));
}
