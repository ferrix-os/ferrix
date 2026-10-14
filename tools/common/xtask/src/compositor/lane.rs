//! `test-compositor --boot lane`: a locked pointer's motion reaches its
//! client while the compositor is in a frame (stage 24's I1,
//! `docs/GAME-PATH.md` §4.7).
//!
//! hyprix reads its devices on a thread of its own and writes a locked
//! pointer's relative motion to the client from there. This boot holds
//! every frame [`HOLD_MS`] milliseconds (`HYPRIX_HOLD_FRAME_MS`, a test
//! knob), has `vlock` -- a window that locks the pointer and draws at every
//! frame callback, so the compositor is always in a frame -- and moves
//! QEMU's virtio mouse [`MOTIONS`] times. `vlock` says how old each motion
//! was when it arrived, from the input core's stamp, which hyprix sends as
//! the event's time; hyprix says how old each was when it wrote it
//! (`HYPRIX_INPUT_AGES`). With the thread the median age is a small part of
//! a held frame; read on the loop, a motion waits for the frame it arrived
//! in, half a held frame at the median. The negative control is the thread
//! removed (`Front::new` asked for none), which must fail on
//! [`WAITED`].

use std::time::{Duration, Instant};

use super::boot::{build_image, say_the_marker, undithered};
use super::{Carried, EITHER, FAILED, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{Qmp, free_port};
use crate::paths::Arch;
use crate::qemu::Watching;
use crate::{Error, Result};

/// How long every frame is held.
const HOLD_MS: u64 = 100;

/// The motions moved under the lock.
const MOTIONS: usize = 40;

/// The fewest of them that must arrive under the lock.
const FEWEST: usize = 30;

/// What the judge says when the motions waited for the frames.
const WAITED: &str = "a locked pointer's motion waited for the frame";

/// The compositor's configuration: the window that locks the pointer.
const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --boot lane`.
exec-once = /bin/vlock --seconds 600
";

/// The compositor's unit, with the ages said and every frame held.
fn drop_in() -> String {
    format!(
        "# Carried by `cargo xtask test-compositor --boot lane`.\n\
         [Service]\n\
         Environment=HYPRIX_INPUT_AGES=1\n\
         Environment=HYPRIX_HOLD_FRAME_MS={HOLD_MS}\n"
    )
}

/// A relative movement, as QMP's `input-send-event` takes it.
fn relative(axis: &str, value: i32) -> String {
    format!("{{\"type\":\"rel\",\"data\":{{\"axis\":\"{axis}\",\"value\":{value}}}}}")
}

/// The boot.
pub(super) fn test_lane(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let vlock = super::build(arch, "compositor-vptr", "vlock")?;
    let carried = Carried {
        ports: vec![
            crate::ports::File {
                path: "bin/vlock".to_owned(),
                mode: 0o755,
                content: crate::ports::Content::Bytes(std::fs::read(&vlock).map_err(|error| {
                    Error::new(format!("reading {}: {error}", vlock.display()))
                })?),
            },
            crate::ports::File {
                path: "etc/ferrix/units/hyprix.service.d/lane.conf".to_owned(),
                mode: 0o644,
                content: crate::ports::Content::Bytes(drop_in().into_bytes()),
            },
        ],
        ..Carried::none()
    };
    let (image, kernel) = build_image(arch, programs, &undithered(CONFIG), carried, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.mouse = true;
    qemu_args.qmp_port = Some(port);
    let mut said = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let up = watching.read_more(Instant::now() + SETTLE, |lines| {
            lines.iter().any(|line| line.contains(MARKER))
        })?;
        if let Some(line) = watching.lines().iter().find(|line| line.contains(FAILED)) {
            return Err(Error::new(format!("{arch}: {}", line.trim())));
        }
        if !up && !watching.lines().iter().any(|line| line.contains(MARKER)) {
            return Err(Error::new(format!(
                "{arch}: the compositor never printed `{MARKER}`"
            )));
        }
        say_the_marker(watching, arch);
        let seen = |watching: &Watching<'_>, what: &str| {
            watching
                .lines()
                .iter()
                .chain(watching.after())
                .any(|line| line.contains(what))
        };
        let _ = watching.read_more(Instant::now() + SETTLE, |lines| {
            lines.iter().any(|line| line.contains("vlock: ready"))
        })?;
        // The pointer onto the window, which puts the lock in force.
        for _ in 0..20 {
            if seen(watching, "vlock: locked") {
                break;
            }
            qmp.input_send_event(&[relative("x", 5)])?;
            let _ = watching.read_more(Instant::now() + Duration::from_millis(300), |lines| {
                lines.iter().any(|line| line.contains("vlock: locked"))
            })?;
        }
        if !seen(watching, "vlock: locked") {
            return Err(Error::new(format!(
                "{arch}: vlock's lock was never in force"
            )));
        }
        // vlock draws at every frame once the lock has been quiet a while
        // (its `QUIET`); the motions go under those frames.
        let _ = watching.read_more(Instant::now() + Duration::from_secs(5), |lines| {
            lines.iter().any(|line| line.contains("vlock: drawing"))
        })?;
        if !seen(watching, "vlock: drawing") {
            return Err(Error::new(format!(
                "{arch}: vlock never began drawing at every frame"
            )));
        }
        // Each at a different phase of the held frames.
        for number in 0..MOTIONS {
            let step = if number % 2 == 0 { 4 } else { -4 };
            qmp.input_send_event(&[relative("x", step)])?;
            let pause = 60 + (number * 37) % 90;
            std::thread::sleep(Duration::from_millis(u64::try_from(pause).unwrap_or(80)));
        }
        let _ = watching.read_more(Instant::now() + Duration::from_secs(3), |_| false)?;
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    judge(arch, &said)
}

/// The ages, in microseconds, of the lines holding `marker`: the number
/// after `age`: a motion vlock had before the lock, and one hyprix wrote
/// on its loop rather than its input thread, left out.
fn ages(lines: &[String], marker: &str) -> Vec<u64> {
    let mut ages: Vec<u64> = lines
        .iter()
        .filter(|line| {
            line.contains(marker)
                && !line.contains(" unlocked")
                && !line.trim_end().ends_with(" loop")
        })
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let _ = words.find(|word| *word == "age")?;
            words.next()?.parse().ok()
        })
        .collect();
    ages.sort_unstable();
    ages
}

/// The `p`th percentile of sorted `values`.
fn percentile(values: &[u64], p: usize) -> Option<u64> {
    let index = (values.len() * p).div_ceil(100).max(1) - 1;
    values.get(index).copied()
}

/// Milliseconds with two places, from microseconds.
fn millis(micros: u64) -> String {
    format!("{:.2}", micros as f64 / 1000.0)
}

/// Pass when the motions arrived under the lock, and at the median well
/// inside a held frame.
fn judge(arch: Arch, lines: &[String]) -> Result<()> {
    if let Some(line) = lines.iter().find(|line| line.contains("FERRIX-PANIC")) {
        return Err(Error::new(format!(
            "{arch}: the kernel stopped while the compositor ran: {}",
            line.trim()
        )));
    }
    let arrived = ages(lines, "vlock: motion ");
    let written = ages(lines, "hyprix: input-age ");
    let lane = lines
        .iter()
        .filter(|line| line.contains("hyprix: input-age ") && line.trim_end().ends_with(" lane"))
        .count();
    let tell = |what: &str, ages: &[u64]| {
        println!(
            "  {arch}: {what}: {} motions, median {} ms, 99th {} ms, most {} ms",
            ages.len(),
            percentile(ages, 50).map_or("-".to_owned(), millis),
            percentile(ages, 99).map_or("-".to_owned(), millis),
            ages.last().copied().map_or("-".to_owned(), millis),
        );
    };
    tell("stamp to the input thread's write", &written);
    tell("stamp to vlock's arrival", &arrived);
    println!("  {arch}: written by the input thread: {lane}; frames held {HOLD_MS} ms");
    if arrived.len() < FEWEST {
        return Err(Error::new(format!(
            "{arch}: {} of {MOTIONS} motions reached vlock under the lock, fewer than {FEWEST}",
            arrived.len()
        )));
    }
    let limit = HOLD_MS * 1000 / 4;
    let median = percentile(&arrived, 50).unwrap_or(u64::MAX);
    if median > limit {
        return Err(Error::new(format!(
            "{arch}: {WAITED}: median age {} ms with frames held {HOLD_MS} ms, over {} ms",
            millis(median),
            millis(limit)
        )));
    }
    if lane < FEWEST {
        return Err(Error::new(format!(
            "{arch}: the input thread wrote {lane} motions, fewer than {FEWEST}"
        )));
    }
    println!(
        "  {arch}: a locked pointer's motion reached its client inside a held frame: median {} \
         ms of {HOLD_MS}",
        millis(median)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn ages_are_read_after_the_word_age_and_unlocked_ones_left_out() {
        let said = lines(&[
            "vlock: motion 1 stamp 5 age 900 unlocked",
            "vlock: motion 2 stamp 5 age 300",
            "  12.0 | hyprix: input-age stamp 5 age 70 lane",
            "  12.0 | hyprix: input-age stamp 4 age 90000 loop",
            "vlock: motion 3 stamp 5 age 100",
        ]);
        assert_eq!(ages(&said, "vlock: motion "), [100, 300]);
        assert_eq!(ages(&said, "hyprix: input-age "), [70]);
    }

    #[test]
    fn motions_that_waited_for_the_frames_fail_and_quick_ones_pass() {
        let mut quick = Vec::new();
        let mut slow = Vec::new();
        for n in 0..MOTIONS {
            quick.push(format!("hyprix: input-age stamp {n} age 200 lane"));
            quick.push(format!("vlock: motion {n} stamp {n} age 900"));
            slow.push(format!("hyprix: input-age stamp {n} age 50000 loop"));
            slow.push(format!("vlock: motion {n} stamp {n} age 50500"));
        }
        assert!(judge(Arch::X86_64, &quick).is_ok());
        let failed = judge(Arch::X86_64, &slow).expect_err("held");
        assert!(failed.to_string().contains(WAITED));
        assert!(judge(Arch::X86_64, &quick[..10]).is_err());
    }
}
