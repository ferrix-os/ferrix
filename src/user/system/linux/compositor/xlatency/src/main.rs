//! `xlatency [--events N] [--seconds S] [--idle S] [--delay-us D]`: stage 24's
//! input-to-scan-out probe (docs/GAME-PATH.md §2.4, R2).
//!
//! An X client that takes `XInput2` raw motion as a game with a relative
//! mouse does, and draws as a vsynced game draws: one frame in flight, the
//! next presented with Present for the refresh after the last one shown.
//! Each frame is filled with a colour that encodes how many events it
//! carries, so a camera or a screen dump can tell the frames apart. For
//! every event it prints, once a completion shows it,
//!
//! `xlatency: shown <n> arrival <ns> swap <ns> ust <us> msc <m> frames <f> in <ms> xtime <ms>`
//!
//! the event's number from 1, when it arrived and when the frame that
//! carried it was presented (`CLOCK_MONOTONIC`), that frame's
//! `CompleteNotify` (`ust`, the start of the scan-out, in microseconds of
//! the same clock, and its `msc`), the refreshes from the last completion
//! before the event to that one, the milliseconds from arrival to the
//! scan-out, and the event's X time, which pairs it with the server's log
//! of the same input (`docs/GAME-PATH.md` §4.7: yserver gives an input the
//! time the device stamped it, and logs both). It ends with a summary line after `N` events, after `S`
//! seconds, or, with `--idle`, once every event is shown and none has come
//! for that many seconds.
//!
//! `--delay-us D` holds every event out of the frames drawn less than `D`
//! after it arrived, as a game that took that long to use it would: the
//! negative control, whose 16700 must move the median by one refresh.
//!
//! `xlatency summarize SENDS REPORT` pairs the report with the injector's
//! log (`vptr --log`) and gives the latency from each motion's send.
//!
//! `xlatency grab` is a game's mouse instead: a grab, a hidden cursor and a
//! warp to the centre after each motion ([`grab`]).

mod grab;
mod record;
mod x11;

use std::io::Write as _;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use record::Tracker;
use x11::{Display, Packet};

const USAGE: &str = "usage: xlatency [--events N] [--seconds S] [--idle S] [--delay-us D]\n\
    \x20      xlatency summarize SENDS REPORT\n\
    \x20      xlatency grab [--seconds S] [--idle S] [--step P]";

/// Frames in turn: one shown, one in flight, one drawn into.
const PIXMAPS: usize = 3;

/// How long without a completion before the probe says it is waiting.
const STALL: Duration = Duration::from_secs(2);

/// What the command line asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Options {
    events: usize,
    seconds: u64,
    delay: u64,
    idle: u64,
}

fn options(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        events: 0,
        seconds: 60,
        delay: 0,
        idle: 0,
    };
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or(USAGE)?;
        let number: u64 = value.parse().map_err(|_| USAGE.to_owned())?;
        match flag.as_str() {
            "--events" => options.events = usize::try_from(number).map_err(|_| USAGE)?,
            "--seconds" => options.seconds = number,
            "--delay-us" => options.delay = number.saturating_mul(1000),
            "--idle" => options.idle = number,
            _ => return Err(USAGE.to_owned()),
        }
    }
    Ok(options)
}

/// `CLOCK_MONOTONIC` in nanoseconds.
fn monotonic() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid, writable timespec for the call's duration.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    let seconds = u64::try_from(now.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(now.tv_nsec).unwrap_or(0);
    seconds.saturating_mul(1_000_000_000).saturating_add(nanos)
}

/// Say a line, at once.
pub(crate) fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// The colour of a frame carrying `count` events: far apart for
/// neighbouring counts.
fn colour(count: usize) -> u32 {
    let count = u32::try_from(count).unwrap_or(0);
    count.wrapping_mul(0x9E37_79B9) & 0x00FF_FFFF
}

/// The window, its frames and the record.
#[derive(Debug)]
struct Probe {
    x: Display,
    window: u32,
    gc: u32,
    pixmaps: Vec<u32>,
    serial: u32,
    tracker: Tracker,
    delay: u64,
}

impl Probe {
    fn open() -> Result<Self, String> {
        let display = std::env::var("DISPLAY").map_err(|_| "DISPLAY is not set")?;
        let socket =
            x11::socket_of(&display).ok_or_else(|| format!("{display} is not a local display"))?;
        let mut x = Display::open(&socket)?;
        let window = x.window("xlatency")?;
        let mut pixmaps = Vec::new();
        for _ in 0..PIXMAPS {
            pixmaps.push(x.pixmap(window)?);
        }
        let gc = x.gc(window)?;
        x.map(window)?;
        Ok(Self {
            x,
            window,
            gc,
            pixmaps,
            serial: 0,
            tracker: Tracker::default(),
            delay: 0,
        })
    }

    /// Draw the next frame with what has arrived and present it for the
    /// refresh after the last one shown.
    fn present(&mut self) -> Result<(), String> {
        let now = monotonic();
        let carries = self.tracker.applied(now, self.delay);
        self.serial = self.serial.wrapping_add(1);
        let slot = usize::try_from(self.serial).unwrap_or(0) % PIXMAPS;
        let pixmap = self.pixmaps.get(slot).copied().unwrap_or(0);
        self.x.fill(pixmap, self.gc, colour(carries))?;
        let target = self.tracker.last_msc().map_or(0, |msc| msc + 1);
        let request = x11::present_pixmap(
            self.x.opcodes.present,
            self.window,
            pixmap,
            self.serial,
            target,
        );
        self.tracker.presented(self.serial, carries, monotonic());
        self.x.send(&request)
    }

    /// Wait for the window to be mapped.
    fn mapped(&mut self, deadline: Instant) -> Result<(), String> {
        while Instant::now() < deadline {
            match self.x.next(Some(Duration::from_millis(100)))? {
                Some(Packet::Mapped) => return Ok(()),
                Some(Packet::RawMotion { time }) => self.tracker.arrive(monotonic(), time),
                _ => {}
            }
        }
        Err("the window was never mapped".to_owned())
    }
}

/// Run until `events` are shown or the time is up.
fn run(options: Options) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(options.seconds);
    let mut probe = Probe::open()?;
    probe.delay = options.delay;
    probe.mapped(deadline)?;
    probe.present()?;
    let mut heard = Instant::now();
    let mut ready = false;
    let mut last_event = None;
    while Instant::now() < deadline {
        if options.events > 0 && probe.tracker.shown.len() >= options.events {
            break;
        }
        let idle = last_event.is_some_and(|at: Instant| at.elapsed().as_secs() >= options.idle);
        if options.idle > 0 && idle && probe.tracker.shown.len() == probe.tracker.arrived() {
            break;
        }
        let packet = probe.x.next(Some(Duration::from_millis(50)))?;
        match packet {
            Some(Packet::RawMotion { time }) => {
                last_event = Some(Instant::now());
                probe.tracker.arrive(monotonic(), time);
            }
            Some(Packet::Complete {
                serial,
                mode,
                ust,
                msc,
            }) => {
                heard = Instant::now();
                for shown in probe.tracker.complete(serial, mode, ust, msc) {
                    say(&shown.line());
                }
                if !ready {
                    ready = true;
                    say(&format!("xlatency: ready msc {msc} ust {ust} mode {mode}"));
                }
                probe.present()?;
            }
            Some(Packet::Error(code, major, minor)) => {
                say(&format!(
                    "xlatency: X error {code} for request {major}.{minor}"
                ));
            }
            _ => {}
        }
        if heard.elapsed() > STALL {
            heard = Instant::now();
            let msc = probe
                .tracker
                .last_msc()
                .map_or("none".to_owned(), |msc| msc.to_string());
            say(&format!("xlatency: no completion for 2 s; last msc {msc}"));
        }
    }
    say(&record::summary(&probe.tracker));
    Ok(())
}

fn summarize(sends: &str, report: &str) -> Result<(), String> {
    let read = |path: &str| {
        std::fs::read_to_string(path).map_err(|error| format!("reading {path}: {error}"))
    };
    say(&record::summarize(&read(sends)?, &read(report)?));
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.as_slice() {
        [command, sends, report] if command == "summarize" => summarize(sends, report),
        [command, rest @ ..] if command == "grab" => grab::options(rest).and_then(grab::run),
        _ => options(&args).and_then(run),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            say(&format!("xlatency: failed: {error}"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_options_take_numbers_and_the_delay_in_microseconds() {
        let args: Vec<String> = ["--events", "40", "--delay-us", "16700"]
            .iter()
            .map(|arg| (*arg).to_owned())
            .collect();
        assert_eq!(
            options(&args),
            Ok(Options {
                events: 40,
                seconds: 60,
                delay: 16_700_000,
                idle: 0,
            })
        );
        assert!(options(&["--events".to_owned()]).is_err());
        assert!(options(&["--what".to_owned(), "1".to_owned()]).is_err());
    }

    #[test]
    fn neighbouring_counts_have_different_colours() {
        assert_eq!(colour(0), 0);
        assert_ne!(colour(1), colour(2));
        assert_eq!(colour(1) >> 24, 0);
    }
}
