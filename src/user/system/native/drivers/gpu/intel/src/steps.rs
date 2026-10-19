//! Finding where the box hangs (2026-10-19). Booting with
//! `ferrix.intelgpu` hard-hung the N150 box twice, with no panic, no timed
//! reset and nobody at the screen, so the RAM log was lost to the power
//! cycle. Two words on the kernel command line, read from `/proc/cmdline`,
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
//! Each step's line is said before the step whether or not either word is
//! given: `intel: step N: <what>`.

use core::fmt;

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
}

/// What the command line asked for.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Steps {
    /// Stop before this step.
    pub(crate) stop: Option<u32>,
    /// Say every register access before it is made.
    pub(crate) trace: bool,
}

/// The driver stopped before a step, as asked.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stopped;

/// The words, as the kernel's command line says them.
const STOP: &[u8] = b"ferrix.intelgpu.stop=";
const TRACE: &[u8] = b"ferrix.intelgpu.trace";

impl Steps {
    /// Read `/proc/cmdline`. Unreadable, it is said, and nothing is asked.
    pub(crate) fn from_command_line() -> Steps {
        let mut line = [0_u8; 2048];
        let steps = read_cmdline(&mut line).map_or_else(Steps::default, |len| {
            parse(line.get(..len).unwrap_or_default())
        });
        say(format_args!(
            "intel: ferrix.intelgpu.stop {}, trace {}",
            Show(steps.stop),
            if steps.trace { "on" } else { "off" }
        ));
        steps
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

/// The words of `line` this driver reads; a `stop=` that is not a number
/// is taken as 1, so a misspelt value touches nothing rather than all.
pub(crate) fn parse(line: &[u8]) -> Steps {
    let mut steps = Steps::default();
    for word in line.split(u8::is_ascii_whitespace) {
        if word == TRACE {
            steps.trace = true;
        } else if let Some(value) = word.strip_prefix(STOP) {
            steps.stop = Some(number(value).unwrap_or(1).max(1));
        }
    }
    steps
}

fn number(digits: &[u8]) -> Option<u32> {
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    digits.iter().try_fold(0_u32, |n, &byte| {
        byte.is_ascii_digit()
            .then(|| n * 10 + u32::from(byte - b'0'))
    })
}

/// `/proc/cmdline` into `line`: its length, or `None`, said.
fn read_cmdline(line: &mut [u8]) -> Option<usize> {
    use ferrix_rt::linux::{self, numbers};
    const PATH: &[u8] = b"/proc/cmdline\0";
    const AT_FDCWD: usize = (-100_isize) as usize;
    const O_RDONLY: usize = 0;
    // SAFETY: `PATH` is NUL-terminated and borrowed for the call.
    let fd = match unsafe {
        linux::call(
            numbers::OPENAT,
            [AT_FDCWD, PATH.as_ptr().addr(), O_RDONLY, 0, 0, 0],
        )
    } {
        Ok(fd) => fd,
        Err(errno) => {
            unreadable(errno);
            return None;
        }
    };
    let mut len = 0;
    while let Some(rest) = line.get_mut(len..) {
        if rest.is_empty() {
            break;
        }
        match linux::read(fd, rest) {
            Ok(0) => break,
            Ok(got) => len += got,
            Err(errno) => {
                let _ = linux::close(fd);
                unreadable(errno);
                return None;
            }
        }
    }
    let _ = linux::close(fd);
    Some(len)
}

fn unreadable(errno: impl fmt::Debug) {
    say(format_args!(
        "intel: /proc/cmdline unreadable ({errno:?}): no ferrix.intelgpu.stop, no trace"
    ));
}
