//! `test-xlatency`: stage 24's input-to-scan-out probe on Ferrix with no
//! card (docs/GAME-PATH.md §2.4 and §8.1, R2).
//!
//! `test-xwindow`'s boot -- hyprix, and yserver on `:0` as the
//! `--everything` desktop starts it -- with `xlatency` as its X client.
//! xtask moves QEMU's tablet over the probe's window with
//! `input-send-event`, one motion at a time, and times each on the host
//! from the command to the probe's line for the frame that shows it. The
//! probe counts the refreshes from the last completion before the motion
//! arrived to the completion that showed it (its `frames`), which needs
//! yserver's Present to advance its `msc` once a refresh. Then the guest
//! runs the same pair as on Linux, `vptr` through hyprix's virtual pointer
//! and the probe, and `xlatency summarize` gives their numbers.
//!
//! It passes when every motion was shown and the median frame count is one
//! to [`MOST_FRAMES`]. A Present whose `msc` never advances shows no motion
//! after the first frame, and the gate says so.

use std::time::{Duration, Instant};

use super::boot::{build_image, undithered, with_the_transcript};
use super::browser::CHROME_WINDOW_PATIENCE;
use super::{Carried, EITHER, Programs, gates_busybox};
use crate::args::Args;
use crate::display::{Qmp, free_port};
use crate::paths::Arch;
use crate::qemu::Watching;
use crate::{Error, Result};

/// Where [`SCRIPT`] is in the image.
const SCRIPT_PATH: &str = "etc/xlatency.sh";

/// The motions xtask sends through QEMU.
const MOTIONS: usize = 40;

/// The most refreshes from a motion's arrival to its frame the gate
/// accepts as a median: one in flight and one drawn take two (Linux's sway
/// gives two), and the rest is slack for a guest without a card.
const MOST_FRAMES: u64 = 4;

/// How long a motion may take to be shown before the gate gives up on it.
const MOTION_PATIENCE: Duration = Duration::from_secs(3);

/// The script the compositor starts beside yserver: the probe for xtask's
/// motions, then `vptr` and the probe in the guest, then the summary.
/// Every line of its own starts `xlatency:`; it ends with [`END`].
const SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp
echo "xlatency: start"
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "xlatency: the socket was there after ${waited}s"
/bin/xlatency --seconds 150 --idle 4 2>&1
echo "xlatency: injector"
/bin/xlatency --seconds 25 > /tmp/probe.txt 2>&1 &
probe=$!
waited=0
until grep -q 'xlatency: ready' /tmp/probe.txt || [ $waited -ge 10 ]; do
    sleep 1
    waited=$((waited + 1))
done
/bin/vptr 7 12 --log /tmp/sends.txt 2>&1 | sed 's/^/xlatency: /'
wait $probe
/bin/xlatency summarize /tmp/sends.txt /tmp/probe.txt 2>&1
grep -E 'xlatency: (summary|no completion|failed|X error)' /tmp/probe.txt | sed 's/^xlatency: /xlatency: in-guest /'
grep -v ' 0x[0-9a-f]* - ' /tmp/yserver.log | tail -n 25 | sed 's/^/xlatency: yserver: /'
echo "xlatency: end"
"#;

/// The line [`SCRIPT`] ends with.
const END: &str = "xlatency: end";

/// The line the probe says once its first frame completed.
const READY: &str = "xlatency: ready";

/// The tablet's coordinate for pixel `at` of `across`.
fn tablet(at: usize, across: usize) -> i32 {
    let at = i64::try_from(at).unwrap_or(0);
    let across = i64::try_from(across.max(1)).unwrap_or(1);
    i32::try_from(at * 0x7FFF / across).unwrap_or(0)
}

/// What the image carries besides the programs: yserver's start, the
/// script, the probe and the injector.
fn ports(arch: Arch) -> Result<Vec<crate::ports::File>> {
    let mut ports = crate::yserver::desktop_files(&[]);
    ports.push(crate::ports::File {
        path: SCRIPT_PATH.to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(SCRIPT.as_bytes().to_vec()),
    });
    for (package, binary) in [
        ("compositor-xlatency", "xlatency"),
        ("compositor-vptr", "vptr"),
    ] {
        let built = super::build(arch, package, binary)?;
        ports.push(crate::ports::File {
            path: format!("bin/{binary}"),
            mode: 0o755,
            content: crate::ports::Content::Bytes(
                std::fs::read(&built)
                    .map_err(|error| Error::new(format!("reading {}: {error}", built.display())))?,
            ),
        });
    }
    Ok(ports)
}

/// Move the tablet `MOTIONS` times over the probe's window, each when the
/// last was shown, and time each from the command to the probe's line.
/// The pause between them is not a multiple of the refresh, so the motions
/// fall at every phase of it.
fn drive(qmp: &mut Qmp, watching: &mut Watching<'_>) -> Result<Vec<Option<Duration>>> {
    let shown = |lines: &[String]| {
        lines
            .iter()
            .filter(|line| line.contains("xlatency: shown "))
            .count()
    };
    let mut timed = Vec::new();
    for number in 1..=MOTIONS {
        let x = if number % 2 == 0 { 400 } else { 600 };
        // What the guest has said by now, so that only a line that comes
        // after the motion counts. The first motion into the window can
        // make two events, so the probe's numbers are not the motions'.
        let _ = watching.read_more(Instant::now() + Duration::from_millis(20), |_| false)?;
        let before = shown(watching.after());
        let sent = Instant::now();
        qmp.input_send_event(&[
            crate::compositor::absolute("x", tablet(x, 1024)),
            crate::compositor::absolute("y", tablet(384, 768)),
        ])?;
        let seen = watching.read_more(Instant::now() + MOTION_PATIENCE, |lines| {
            shown(lines) > before
        })?;
        timed.push(seen.then(|| sent.elapsed()));
        // Three not shown: the frames have stopped, and the rest would only
        // wait out their patience.
        if timed.iter().filter(|shown| shown.is_none()).count() >= 3 {
            break;
        }
        let pause = 90 + (number * 37) % 110;
        std::thread::sleep(Duration::from_millis(u64::try_from(pause).unwrap_or(100)));
    }
    Ok(timed)
}

/// `cargo xtask test-xlatency`.
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, QEMU cannot be
/// run, the probe never drew, or [`judge`] finds the motions not shown.
pub(crate) fn test_xlatency(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-xlatency runs on x86-64 only, as yserver's volume does",
        ));
    }
    let args = super::apps::xwindow_args(args)?;
    let busybox = gates_busybox(arch)
        .ok_or_else(|| Error::new("test-xlatency needs a static busybox, as test-xwindow does"))?;
    let programs = Programs::build(arch)?;
    let carried = Carried {
        busybox: Some(std::path::PathBuf::from(busybox)),
        ports: ports(arch)?,
        ..Carried::none()
    };
    let config = format!(
        "# Carried into the initramfs by `cargo xtask test-xlatency`.\n{}\
         {}exec-once = /bin/busybox sh /{SCRIPT_PATH}\n",
        crate::chrome::WINDOW_ENV,
        crate::yserver::desktop_config(),
    );
    let (image, kernel) = build_image(arch, &programs, &undithered(&config), carried, &args)?;
    let port = free_port()?;
    let mut qemu_args = args;
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let mut said: Vec<String> = Vec::new();
    let mut timed = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let ready = watching.read_more(Instant::now() + CHROME_WINDOW_PATIENCE, |lines| {
            lines
                .iter()
                .any(|line| line.contains(READY) || line.contains(END))
        })?;
        if ready {
            timed = drive(&mut qmp, watching)?;
        }
        let ended = watching.read_more(Instant::now() + CHROME_WINDOW_PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(END))
        })?;
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        watching.stop_when_done();
        if ended {
            Ok(())
        } else {
            Err(with_the_transcript(
                &Error::new(format!("{arch}: the xlatency script never ended")),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    judge(arch, &said, &timed)
}

/// Milliseconds with three places.
fn millis(duration: Duration) -> String {
    format!("{:.3}", duration.as_secs_f64() * 1000.0)
}

/// The `frames` of each `xlatency: shown` line before the injector's phase.
fn frames_shown(lines: &[String]) -> Vec<Option<u64>> {
    lines
        .iter()
        .take_while(|line| !line.contains("xlatency: injector"))
        .filter_map(|line| {
            let rest = line.split("xlatency: shown ").nth(1)?;
            let mut words = rest.split_whitespace();
            let _ = words.by_ref().find(|word| *word == "frames")?;
            Some(words.next().and_then(|frames| frames.parse().ok()))
        })
        .collect()
}

/// Say what was measured, and pass when every motion was shown within
/// [`MOST_FRAMES`] refreshes at the median.
fn judge(arch: Arch, lines: &[String], timed: &[Option<Duration>]) -> Result<()> {
    for line in lines.iter().filter(|line| {
        line.contains("xlatency: summary")
            || line.contains("xlatency: input-to-scanout")
            || line.contains("xlatency: in-guest")
            || line.contains("xlatency: vptr")
            || line.contains("xlatency: ready")
            || line.contains("xlatency: failed")
            || line.contains("xlatency: yserver:")
    }) {
        println!("  {arch}: {}", line.trim());
    }
    let mut host: Vec<Duration> = timed.iter().flatten().copied().collect();
    host.sort_unstable();
    let mut frames: Vec<u64> = frames_shown(lines).into_iter().flatten().collect();
    frames.sort_unstable();
    let at = |sorted_len: usize, p: usize| (sorted_len * p).div_ceil(100).max(1) - 1;
    if let (Some(median), Some(p99)) = (host.get(at(host.len(), 50)), host.get(at(host.len(), 99)))
    {
        println!(
            "  {arch}: QMP motion to the probe's line for its frame, timed on the host: \
             {} of {MOTIONS}, median {} ms, 99th {} ms",
            host.len(),
            millis(*median),
            millis(*p99)
        );
    }
    if !lines.iter().any(|line| line.contains(READY)) {
        let closed = lines
            .iter()
            .any(|line| line.contains("xlatency: failed: the server closed the connection"));
        let why = if closed {
            "yserver closed the probe's connection; the end of its log is above"
        } else {
            "no Present CompleteNotify from yserver"
        };
        return Err(Error::new(format!(
            "{arch}: the probe never had a frame completed: {why}"
        )));
    }
    if host.len() < MOTIONS {
        return Err(Error::new(format!(
            "{arch}: {} of {MOTIONS} motions were shown; a Present whose msc does not advance \
             completes no frame after the first (yserver's rootless clock, R1)",
            host.len()
        )));
    }
    let median = frames.get(at(frames.len(), 50)).copied();
    println!(
        "  {arch}: refreshes from arrival to the frame that showed it: median {}, most {}",
        median.map_or("-".to_owned(), |n| n.to_string()),
        frames.last().map_or("-".to_owned(), ToString::to_string)
    );
    match median {
        Some(median) if (1..=MOST_FRAMES).contains(&median) => Ok(()),
        _ => Err(Error::new(format!(
            "{arch}: the median refresh count is not 1 to {MOST_FRAMES}: the msc does not count refreshes"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_read_from_the_probes_lines_before_the_injector() {
        let lines: Vec<String> = [
            "xlatency: ready msc 2 ust 9 mode 0",
            "xlatency: shown 1 arrival 5 swap 6 ust 7 msc 4 frames 2 in 1.000",
            "xlatency: shown 2 arrival 5 swap 6 ust 7 msc 4 frames - in 1.000",
            "xlatency: injector",
            "xlatency: shown 1 arrival 5 swap 6 ust 7 msc 4 frames 9 in 1.000",
        ]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
        assert_eq!(frames_shown(&lines), [Some(2), None]);
    }

    #[test]
    fn a_gate_with_motions_missing_fails_and_one_with_all_shown_passes() {
        let mut lines = vec!["xlatency: ready msc 2 ust 9 mode 0".to_owned()];
        let timed = vec![Some(Duration::from_millis(30)); MOTIONS];
        assert!(judge(Arch::X86_64, &lines, &timed[..3]).is_err());
        for number in 1..=MOTIONS {
            lines.push(format!(
                "xlatency: shown {number} arrival 5 swap 6 ust 7 msc 4 frames 2 in 1.000"
            ));
        }
        assert!(judge(Arch::X86_64, &lines, &timed).is_ok());
        let stuck: Vec<String> = lines
            .iter()
            .map(|line| line.replace("frames 2", "frames 0"))
            .collect();
        assert!(judge(Arch::X86_64, &stuck, &timed).is_err());
    }
}
