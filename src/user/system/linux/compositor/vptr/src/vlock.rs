//! `vlock [--seconds S] [--motions N]`: a window that locks the pointer as a
//! game does and says how old each relative motion was when it arrived.
//!
//! The receiving end of stage 24's boot check for hyprix's input thread
//! (`docs/GAME-PATH.md` §4.7, I1). It maps a window, draws it again for
//! every frame callback -- so the compositor is always in a frame -- and
//! asks `zwp_pointer_constraints_v1` for a persistent lock on it, and
//! `zwp_relative_pointer_manager_v1` for the seat's relative motion. For
//! each `relative_motion` it prints
//!
//! `vlock: motion <n> stamp <us> age <us>`
//!
//! the motion's number from 1, its time as the compositor sent it (the
//! input core's stamp, microseconds of `CLOCK_MONOTONIC`), and how long
//! before its arrival that was. It ends with
//! `vlock: summary motions <n> locked <m> median <us> 99th <us> most <us>`,
//! over the motions that came while the lock was in force.

use std::io::Write as _;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use compositor_protocol::pointer_constraints::{
    ZWP_LOCKED_POINTER_V1, ZWP_POINTER_CONSTRAINTS_V1, zwp_locked_pointer_v1,
    zwp_pointer_constraints_v1,
};
use compositor_protocol::relative_pointer::{
    ZWP_RELATIVE_POINTER_MANAGER_V1, ZWP_RELATIVE_POINTER_V1, zwp_relative_pointer_manager_v1,
    zwp_relative_pointer_v1,
};
use compositor_toolkit::{Client, Event, ToplevelOptions, Value};

const USAGE: &str = "usage: vlock [--seconds S] [--motions N]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match options(&args).and_then(run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            say(&format!("vlock: failed: {error}"));
            ExitCode::FAILURE
        }
    }
}

/// How long to run, and after how many motions to stop.
fn options(args: &[String]) -> Result<(u64, usize), String> {
    let mut seconds = 60;
    let mut motions = 0;
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or(USAGE)?;
        match flag.as_str() {
            "--seconds" => seconds = value.parse().map_err(|_| USAGE)?,
            "--motions" => motions = value.parse().map_err(|_| USAGE)?,
            _ => return Err(USAGE.to_owned()),
        }
    }
    Ok((seconds, motions))
}

/// `CLOCK_MONOTONIC` in microseconds, the clock the stamps are in.
fn monotonic_us() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid, writable timespec for the call's duration.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    let seconds = u64::try_from(now.tv_sec).unwrap_or(0);
    let micros = u64::try_from(now.tv_nsec).unwrap_or(0) / 1_000;
    seconds.saturating_mul(1_000_000).saturating_add(micros)
}

/// A `relative_motion`'s time: its two halves, high first.
fn stamp_of(args: &[Value]) -> Option<u64> {
    let high = args.first().and_then(Value::as_uint)?;
    let low = args.get(1).and_then(Value::as_uint)?;
    Some((u64::from(high) << 32) | u64::from(low))
}

/// The summary line over the ages, in microseconds.
fn summary(motions: usize, ages: &mut [u64]) -> String {
    ages.sort_unstable();
    let at = |p: usize| {
        let index = (ages.len() * p).div_ceil(100).max(1) - 1;
        ages.get(index)
            .map_or_else(|| "-".to_owned(), ToString::to_string)
    };
    format!(
        "vlock: summary motions {motions} locked {} median {} 99th {} most {}",
        ages.len(),
        at(50),
        at(99),
        ages.last()
            .map_or_else(|| "-".to_owned(), ToString::to_string)
    )
}

fn run((seconds, wanted): (u64, usize)) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut client = Client::connect().map_err(|error| error.to_string())?;
    let (manager, _) = client
        .bind(&ZWP_RELATIVE_POINTER_MANAGER_V1, 1, None)
        .map_err(|error| error.to_string())?;
    let (constraints, _) = client
        .bind(&ZWP_POINTER_CONSTRAINTS_V1, 1, None)
        .map_err(|error| error.to_string())?;
    let window = client
        .toplevel(&ToplevelOptions {
            title: "vlock".to_owned(),
            app_id: "vlock".to_owned(),
            size: (640, 480),
            ..ToplevelOptions::default()
        })
        .map_err(|error| error.to_string())?;
    let mut relative = None;
    let mut held = None;
    let mut asked = false;
    let mut locked = false;
    let mut motions = 0;
    let mut ages = Vec::new();
    while Instant::now() < deadline && (wanted == 0 || motions < wanted) {
        let events = client
            .dispatch(Some(Duration::from_millis(50)))
            .map_err(|error| error.to_string())?;
        let arrived = monotonic_us();
        for event in events {
            match event {
                Event::Configure { surface, .. } | Event::Frame { surface, .. }
                    if surface == window =>
                {
                    // Drawn again at every frame: the compositor is always
                    // in one, which is what the check moves the mouse under.
                    let _ = client
                        .draw(window, |_| {})
                        .map_err(|error| error.to_string())?;
                    client.request_frame(window);
                }
                Event::Object {
                    object,
                    opcode,
                    args,
                    ..
                } if Some(object) == relative
                    && opcode == zwp_relative_pointer_v1::event::RELATIVE_MOTION =>
                {
                    motions += 1;
                    let stamp = stamp_of(&args).unwrap_or(0);
                    let age = arrived.saturating_sub(stamp);
                    if locked {
                        ages.push(age);
                    }
                    say(&format!(
                        "vlock: motion {motions} stamp {stamp} age {age}{}",
                        if locked { "" } else { " unlocked" }
                    ));
                }
                Event::Object { object, opcode, .. }
                    if Some(object) == held
                        && opcode == zwp_locked_pointer_v1::event::LOCKED
                        && !locked =>
                {
                    locked = true;
                    say("vlock: locked");
                }
                Event::CloseRequested(surface) if surface == window => {
                    say(&summary(motions, &mut ages));
                    return Ok(());
                }
                _ => {}
            }
        }
        // The lock and the relative pointer, once the seat has a pointer
        // and the window a surface to lock it on.
        if !asked
            && let (Some(pointer), Some(surface)) =
                (client.pointer_object(), client.surface_object(window))
        {
            asked = true;
            let id = client.new_object(&ZWP_RELATIVE_POINTER_V1, 1);
            client
                .request(
                    manager,
                    zwp_relative_pointer_manager_v1::request::GET_RELATIVE_POINTER,
                    &[Value::NewId(id), Value::Object(pointer)],
                )
                .map_err(|error| error.to_string())?;
            relative = Some(id);
            let lock = client.new_object(&ZWP_LOCKED_POINTER_V1, 1);
            held = Some(lock);
            client
                .request(
                    constraints,
                    zwp_pointer_constraints_v1::request::LOCK_POINTER,
                    &[
                        Value::NewId(lock),
                        Value::Object(surface),
                        Value::Object(pointer),
                        Value::Object(compositor_toolkit::ObjectId(0)),
                        Value::Uint(zwp_pointer_constraints_v1::lifetime::PERSISTENT),
                    ],
                )
                .map_err(|error| error.to_string())?;
            client.flush().map_err(|error| error.to_string())?;
            say("vlock: ready");
        }
    }
    say(&summary(motions, &mut ages));
    Ok(())
}

/// A line on the standard output, flushed.
fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_is_its_two_halves() {
        assert_eq!(
            stamp_of(&[Value::Uint(1), Value::Uint(5)]),
            Some((1 << 32) | 5)
        );
        assert_eq!(stamp_of(&[Value::Uint(1)]), None);
    }

    #[test]
    fn the_summary_takes_the_median_and_the_99th() {
        let mut ages = vec![300, 100, 200];
        assert_eq!(
            summary(4, &mut ages),
            "vlock: summary motions 4 locked 3 median 200 99th 300 most 300"
        );
        assert_eq!(
            summary(0, &mut []),
            "vlock: summary motions 0 locked 0 median - 99th - most -"
        );
    }

    #[test]
    fn the_options_are_seconds_and_motions() {
        let args: Vec<String> = ["--motions", "40"].iter().map(|a| (*a).to_owned()).collect();
        assert_eq!(options(&args), Ok((60, 40)));
        assert!(options(&["--what".to_owned(), "1".to_owned()]).is_err());
    }
}
