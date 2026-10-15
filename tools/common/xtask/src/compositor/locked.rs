//! `test-compositor --boot locked` and `--boot locked-held`: every distance
//! a locked pointer moves reaches the client that locked it.
//!
//! A game that holds `zwp_locked_pointer_v1` turns its view by the sum of
//! the `zwp_relative_pointer_v1.relative_motion` events it is sent: a
//! motion the compositor drops is a turn the player made and the game never
//! saw. These boots have `vlock` -- a window that locks the pointer and,
//! once the lock has been quiet a second, draws again at every frame
//! callback, so the compositor is always in a frame --
//! and move QEMU's virtio mouse [`MOTIONS`] times by known distances. hyprix
//! says each pass that read a device and each relative motion it carried out
//! (`HYPRIX_INPUT_TRACE`); `vlock` says each motion it was sent and the
//! distance they carried together. `locked` is an ordinary boot, where each
//! motion must arrive as an event of its own; `locked-held` holds every
//! frame [`HOLD_MS`] milliseconds (`HYPRIX_HOLD_FRAME_MS`), so two motions
//! may arrive inside one frame and be read together, gathered into one
//! event: there the distance must still arrive whole.

use std::time::{Duration, Instant};

use super::boot::{build_image, say_the_marker, undithered};
use super::{Carried, EITHER, FAILED, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{Qmp, free_port};
use crate::paths::Arch;
use crate::qemu::Watching;
use crate::{Error, Result};

/// How long every frame is held in `locked-held`.
const HOLD_MS: u64 = 100;

/// The motions moved under the lock.
const MOTIONS: usize = 40;

/// The fewest events they may arrive in when frames are held: two motions
/// inside one held frame are read together and may be one event.
const FEWEST_HELD: usize = 20;

/// What the judge says when a distance went missing.
const LOST: &str = "a locked pointer's motion was lost";

/// The compositor's configuration: the window that locks the pointer.
const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --boot locked`.
exec-once = /bin/vlock --seconds 600
";

/// The compositor's unit: the input traced, and every frame held if `hold`.
fn drop_in(hold: Option<u64>) -> String {
    let mut unit = "# Carried by `cargo xtask test-compositor --boot locked`.\n\
                    [Service]\n\
                    Environment=HYPRIX_INPUT_TRACE=1\n"
        .to_owned();
    if let Some(millis) = hold {
        unit.push_str(&format!("Environment=HYPRIX_HOLD_FRAME_MS={millis}\n"));
    }
    unit
}

/// A relative movement, as QMP's `input-send-event` takes it.
fn relative(axis: &str, value: i32) -> String {
    format!("{{\"type\":\"rel\",\"data\":{{\"axis\":\"{axis}\",\"value\":{value}}}}}")
}

/// The `number`th motion's distance: 1 to 7 pixels, never two the same in a
/// row, so that one dropped or counted twice changes the sum.
const fn step(number: usize) -> i32 {
    (number % 7) as i32 + 1
}

/// The distance all [`MOTIONS`] carry together.
fn total() -> i32 {
    (0..MOTIONS).map(step).sum()
}

/// The ordinary boot: a client drawing every frame, frames as fast as they
/// come.
pub(super) fn test_locked(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    run(arch, programs, args, None)
}

/// The boot with every frame held [`HOLD_MS`].
pub(super) fn test_locked_held(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    run(arch, programs, args, Some(HOLD_MS))
}

fn run(arch: Arch, programs: &Programs, args: &Args, hold: Option<u64>) -> Result<()> {
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
                path: "etc/ferrix/units/hyprix.service.d/locked.conf".to_owned(),
                mode: 0o644,
                content: crate::ports::Content::Bytes(drop_in(hold).into_bytes()),
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
        // The pointer onto the window, which puts the lock in force: down
        // and right from where it starts, which is the screen's middle.
        for _ in 0..20 {
            if seen(watching, "vlock: locked") {
                break;
            }
            qmp.input_send_event(&[relative("y", 5)])?;
            let _ = watching.read_more(Instant::now() + Duration::from_millis(300), |lines| {
                lines.iter().any(|line| line.contains("vlock: locked"))
            })?;
        }
        if !seen(watching, "vlock: locked") {
            return Err(Error::new(format!(
                "{arch}: vlock's lock was never in force"
            )));
        }
        // vlock draws at every frame once its lock has been quiet a second,
        // so that the compositor's last wait before the frames is one only
        // vlock woke: the case where hyprix, before 2026-10-15, read nothing
        // else for as long as the frames went on. The lock-in moved along y
        // and the counted motions move along x, which is how the two are
        // told apart.
        let _ = watching.read_more(Instant::now() + SETTLE, |lines| {
            lines.iter().any(|line| line.contains("vlock: drawing"))
        })?;
        if !seen(watching, "vlock: drawing") {
            return Err(Error::new(format!(
                "{arch}: vlock never began drawing at every frame"
            )));
        }
        let _ = watching.read_more(Instant::now() + Duration::from_millis(500), |_| false)?;
        // Each at a different phase of the frames.
        for number in 0..MOTIONS {
            qmp.input_send_event(&[relative("x", step(number))])?;
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
    judge(arch, &said, hold)
}

/// The number after `word` in `line`.
fn after(line: &str, word: &str) -> Option<f64> {
    let mut words = line.split_whitespace();
    let _ = words.find(|each| *each == word)?;
    words.next()?.parse().ok()
}

/// What arrived: the locked motions along x (the lock-in moved along y),
/// as how many events and the distance they carried together.
fn arrived(lines: &[String], marker: &str) -> (usize, f64) {
    lines
        .iter()
        .filter(|line| line.contains(marker) && !line.contains(" unlocked"))
        .filter_map(|line| after(line, "dx"))
        .filter(|dx| *dx != 0.0)
        .fold((0, 0.0), |(count, sum), dx| (count + 1, sum + dx))
}

/// Pass when the whole distance reached vlock under the lock, as one event
/// a motion in an ordinary boot and in no fewer than [`FEWEST_HELD`] with
/// frames held.
fn judge(arch: Arch, lines: &[String], hold: Option<u64>) -> Result<()> {
    if let Some(line) = lines.iter().find(|line| line.contains("FERRIX-PANIC")) {
        return Err(Error::new(format!(
            "{arch}: the kernel stopped while the compositor ran: {}",
            line.trim()
        )));
    }
    let sent = total();
    let (carried, carried_dx) = arrived(lines, "hyprix: input-trace relative ");
    let (received, received_dx) = arrived(lines, "vlock: motion ");
    let reads = lines
        .iter()
        .filter(|line| line.contains("hyprix: input-trace read "))
        .count();
    let held = hold.map_or_else(
        || "not held".to_owned(),
        |millis| format!("held {millis} ms"),
    );
    println!(
        "  {arch}: {MOTIONS} motions moved {sent} px; hyprix carried out {carried} \
         ({carried_dx} px) in {reads} device reads; vlock was sent {received} ({received_dx} px) \
         under the lock; frames {held}"
    );
    if (received_dx - f64::from(sent)).abs() > f64::EPSILON {
        return Err(Error::new(format!(
            "{arch}: {LOST}: {received_dx} of {sent} px reached vlock under the lock, in \
             {received} of {MOTIONS} motions (hyprix carried out {carried_dx} px); frames {held}"
        )));
    }
    let fewest = if hold.is_some() { FEWEST_HELD } else { MOTIONS };
    if received < fewest {
        return Err(Error::new(format!(
            "{arch}: {received} of {MOTIONS} motions reached vlock as events of their own, fewer \
             than {fewest}; frames {held}"
        )));
    }
    println!(
        "  {arch}: every distance a locked pointer moved reached its client: {sent} px in \
         {received} events; frames {held}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[String]) -> Vec<String> {
        text.to_vec()
    }

    fn motions(steps: &[i32]) -> Vec<String> {
        let mut said = vec!["vlock: motion 1 stamp 5 age 9 dx 0 dy 5 unlocked".to_owned()];
        for (n, dx) in steps.iter().enumerate() {
            said.push(format!(
                "  12.0 | hyprix: input-trace relative dx {dx} dy 0 to 1"
            ));
            said.push(format!(
                "vlock: motion {} stamp 5 age 9 dx {dx} dy 0",
                n + 2
            ));
        }
        said
    }

    #[test]
    fn the_steps_differ_and_sum_to_the_total() {
        assert!((0..MOTIONS - 1).all(|n| step(n) != step(n + 1)));
        assert_eq!(total(), (0..MOTIONS).map(step).sum::<i32>());
        assert!(total() > 0);
    }

    #[test]
    fn every_motion_arrived_passes() {
        let all: Vec<i32> = (0..MOTIONS).map(step).collect();
        assert!(judge(Arch::X86_64, &lines(&motions(&all)), None).is_ok());
        assert!(judge(Arch::X86_64, &lines(&motions(&all)), Some(HOLD_MS)).is_ok());
    }

    #[test]
    fn a_lost_motion_fails_held_or_not() {
        let lost: Vec<i32> = (1..MOTIONS).map(step).collect();
        for hold in [None, Some(HOLD_MS)] {
            let failed = judge(Arch::X86_64, &lines(&motions(&lost)), hold).expect_err("lost");
            assert!(failed.to_string().contains(LOST), "{failed}");
        }
    }

    #[test]
    fn gathered_motions_pass_only_with_frames_held() {
        // Pairs read together: the same distance in half the events.
        let pairs: Vec<i32> = (0..MOTIONS / 2)
            .map(|n| step(2 * n) + step(2 * n + 1))
            .collect();
        assert!(judge(Arch::X86_64, &lines(&motions(&pairs)), Some(HOLD_MS)).is_ok());
        assert!(judge(Arch::X86_64, &lines(&motions(&pairs)), None).is_err());
    }

    #[test]
    fn a_panic_fails() {
        let said = vec!["FERRIX-PANIC: something".to_owned()];
        assert!(judge(Arch::X86_64, &said, None).is_err());
    }
}
