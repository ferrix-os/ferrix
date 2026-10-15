//! `xlatency grab [--seconds S] [--idle S] [--step P]`: an X game's mouse,
//! the pattern that needs the compositor's pointer lock (docs/GAME-PATH.md
//! §4.7).
//!
//! It grabs the pointer to its window, confined there and shown as a cursor
//! with nothing in it, warps the pointer to the window's centre, and after
//! each core `MotionNotify` reads how far the pointer is from the centre --
//! what the game takes as the mouse's movement -- and warps it back, as SDL's
//! warp mode and many games do. It counts `XInput2` raw motion as well.
//!
//! The injector (`vptr`) moves the mouse by `P` pixels a motion across. On a
//! server that has the compositor lock the pointer, each motion is read as
//! `P` across and 0 down; on one that does not, the compositor's pointer
//! goes on moving where X's was warped back from, and the reading is
//! wherever the compositor's pointer is, less the centre. It says, once
//! `S` seconds are up or none has come for the idle seconds after one,
//!
//! `xlatency: grab motions <m> raw <r> as-the-mouse <k> step <P> echoes <e> first <offsets>`
//!
//! the motions read away from the centre, the raw motions, how many of the
//! first read exactly `P` across and 0 down, the motions at the centre
//! (each warp's own), and the first few offsets read.

use std::time::{Duration, Instant};

use crate::say;
use crate::x11::{Display, Packet};

/// What the command line asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Options {
    seconds: u64,
    idle: u64,
    step: i16,
}

const USAGE: &str = "usage: xlatency grab [--seconds S] [--idle S] [--step P]";

pub(crate) fn options(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        seconds: 60,
        idle: 0,
        step: 9,
    };
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or(USAGE)?;
        let number: u64 = value.parse().map_err(|_| USAGE.to_owned())?;
        match flag.as_str() {
            "--seconds" => options.seconds = number,
            "--idle" => options.idle = number,
            "--step" => options.step = i16::try_from(number).map_err(|_| USAGE)?,
            _ => return Err(USAGE.to_owned()),
        }
    }
    Ok(options)
}

/// How many offsets the report lists.
const FIRST: usize = 8;

/// What the game has read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Reading {
    centre: (i16, i16),
    step: i16,
    motions: usize,
    raw: usize,
    as_mouse: usize,
    echoes: usize,
    first: Vec<(i16, i16)>,
}

impl Reading {
    fn new(step: i16) -> Self {
        Self {
            step,
            ..Self::default()
        }
    }

    /// The window is `width` × `height`; its centre is where the pointer
    /// is warped to.
    fn sized(&mut self, width: u16, height: u16) {
        let half = |side: u16| i16::try_from(side / 2).unwrap_or(i16::MAX);
        self.centre = (half(width), half(height));
    }

    /// A core motion to (`x`, `y`) on the window: whether the game warps
    /// the pointer back to the centre after it.
    fn motion(&mut self, x: i16, y: i16) -> bool {
        if (x, y) == self.centre {
            self.echoes += 1;
            return false;
        }
        let offset = (
            x.saturating_sub(self.centre.0),
            y.saturating_sub(self.centre.1),
        );
        self.motions += 1;
        if offset.0.unsigned_abs() == self.step.unsigned_abs() && offset.1 == 0 {
            self.as_mouse += 1;
        }
        if self.first.len() < FIRST {
            self.first.push(offset);
        }
        true
    }

    fn line(&self) -> String {
        let first: Vec<String> = self.first.iter().map(|(x, y)| format!("{x},{y}")).collect();
        format!(
            "xlatency: grab motions {} raw {} as-the-mouse {} step {} echoes {} first {}",
            self.motions,
            self.raw,
            self.as_mouse,
            self.step,
            self.echoes,
            if first.is_empty() {
                "-".to_owned()
            } else {
                first.join(" ")
            }
        )
    }
}

/// Grab, warp and read until the time is up.
pub(crate) fn run(options: Options) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(options.seconds);
    let display = std::env::var("DISPLAY").map_err(|_| "DISPLAY is not set")?;
    let socket = crate::x11::socket_of(&display)
        .ok_or_else(|| format!("{display} is not a local display"))?;
    let mut x = Display::open(&socket)?;
    let window = x.window("xgrab")?;
    let mut reading = Reading::new(options.step);
    reading.sized(x.screen.width, x.screen.height);
    x.map(window)?;
    loop {
        if Instant::now() >= deadline {
            return Err("the window was never mapped".to_owned());
        }
        match x.next(Some(Duration::from_millis(100)))? {
            Some(Packet::Mapped) => break,
            Some(Packet::Configured { width, height }) => reading.sized(width, height),
            _ => {}
        }
    }
    let cursor = x.blank_cursor(window)?;
    // The window is viewable once the server has it on the compositor; a
    // grab before that is refused, and asked again.
    let mut status = u8::MAX;
    while Instant::now() < deadline {
        status = x.grab_pointer(window, cursor)?;
        if status == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if status != 0 {
        return Err(format!("GrabPointer said {status} until the end"));
    }
    x.warp_pointer(window, reading.centre.0, reading.centre.1)?;
    say(&format!(
        "xlatency: grab ready centre {},{}",
        reading.centre.0, reading.centre.1
    ));
    let mut last = None;
    while Instant::now() < deadline {
        let idle = last.is_some_and(|at: Instant| at.elapsed().as_secs() >= options.idle);
        if options.idle > 0 && idle {
            break;
        }
        match x.next(Some(Duration::from_millis(50)))? {
            Some(Packet::Motion { x: at_x, y: at_y }) => {
                last = Some(Instant::now());
                if reading.motion(at_x, at_y) {
                    x.warp_pointer(window, reading.centre.0, reading.centre.1)?;
                }
            }
            Some(Packet::RawMotion { .. }) => {
                last = Some(Instant::now());
                reading.raw += 1;
            }
            Some(Packet::Configured { width, height }) => {
                reading.sized(width, height);
                x.warp_pointer(window, reading.centre.0, reading.centre.1)?;
            }
            Some(Packet::Error(code, major, minor)) => {
                say(&format!(
                    "xlatency: grab: X error {code} for request {major}.{minor}"
                ));
            }
            _ => {}
        }
    }
    say(&reading.line());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_motion_is_read_from_the_centre_and_the_warps_own_is_not() {
        let mut reading = Reading::new(9);
        reading.sized(1001, 700);
        assert_eq!(reading.centre, (500, 350));
        assert!(reading.motion(509, 350));
        assert!(!reading.motion(500, 350), "the warp's own motion");
        assert!(reading.motion(491, 350));
        assert!(reading.motion(620, 340), "the compositor's pointer");
        assert_eq!(reading.motions, 3);
        assert_eq!(reading.as_mouse, 2);
        assert_eq!(reading.echoes, 1);
        assert_eq!(
            reading.line(),
            "xlatency: grab motions 3 raw 0 as-the-mouse 2 step 9 echoes 1 \
             first 9,0 -9,0 120,-10"
        );
    }

    #[test]
    fn the_options_have_defaults_and_take_numbers() {
        assert_eq!(
            options(&[]),
            Ok(Options {
                seconds: 60,
                idle: 0,
                step: 9
            })
        );
        let args: Vec<String> = ["--step", "12", "--idle", "3"]
            .iter()
            .map(|arg| (*arg).to_owned())
            .collect();
        assert_eq!(
            options(&args),
            Ok(Options {
                seconds: 60,
                idle: 3,
                step: 12
            })
        );
        assert!(options(&["--step".to_owned(), "x".to_owned()]).is_err());
    }
}
