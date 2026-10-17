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
//!
//! **Per hop** (stage 24's I1, `docs/GAME-PATH.md` §4.7): the input core
//! stamps each tablet report, hyprix sends the stamp on as the relative
//! motion's time and says how old the input was when it sent it
//! (`HYPRIX_INPUT_AGES`), yserver gives the X event the stamp's time and
//! says how old it was when it handed it to X (`YSERVER_INPUT_AGES`), and
//! the probe's line for each event carries its X time. [`hops`] joins the
//! three on the stamp and the X time and gives each hop's age. A yserver
//! that does not take the stamp gives no lines to join, which is said and
//! is not a failure.
//!
//! **A game's pointer lock** (`docs/GAME-PATH.md` §4.7): last, `xlatency
//! grab` is an X game's mouse -- a grab confined to its window, a cursor
//! with nothing in it, a warp to the centre after each motion -- and `vptr`
//! moves the mouse [`GRAB_STEP`] pixels a motion. A yserver that has hyprix
//! lock the pointer gives the game each motion as that step from the
//! centre; one that does not gives it wherever hyprix's pointer has got to,
//! and [`judge_grab`] fails it. The control is yserver with
//! `YSERVER_POINTER_LOCK=0`. The game counts `XInput2` raw motions too, and
//! must have exactly one for each motion it read: raw events come only from
//! the device, so the game's own warps make none (Xorg's `miPointerMove`
//! passes `POINTER_NORAW`). A yserver that sends a raw motion for each warp
//! (as before fork commit ac2485ea) is failed on [`WARP_RAW`].

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
grep -v ' 0x[0-9a-f]* - ' /tmp/yserver.log | grep -v 'input-age' | tail -n 25 | sed 's/^/xlatency: yserver: /'
grep 'input-age stamp' /tmp/yserver.log | sed 's/^.*input-age/xlatency: yserver-age: input-age/'
echo "xlatency: grab phase"
/bin/xlatency grab --seconds 45 --idle 3 --step 9 > /tmp/grab.txt 2>&1 &
grab=$!
waited=0
until grep -q 'xlatency: grab ready' /tmp/grab.txt || [ $waited -ge 20 ]; do
    sleep 1
    waited=$((waited + 1))
done
/bin/vptr 20 6 9 2>&1 | sed 's/^/xlatency: grab /'
wait $grab
cat /tmp/grab.txt
grep -E 'holds the pointer|the pointer on window|pointer lock on window|pointer_constraints|POINTER_LOCK' /tmp/yserver.log | head -n 12 | sed 's/^/xlatency: yserver-lock: /'
echo "xlatency: end"
"#;

/// The line [`SCRIPT`] ends with.
const END: &str = "xlatency: end";

/// The line the probe says once its first frame completed.
const READY: &str = "xlatency: ready";

/// The pixels `vptr` moves the mouse a motion in the grab phase, which the
/// grabbing game must read as such.
const GRAB_STEP: u64 = 9;

/// The motions `vptr` sends in the grab phase: 20 a second for 6 seconds.
const GRAB_SENT: u64 = 120;

/// The tablet's coordinate for pixel `at` of `across`.
pub(super) fn tablet(at: usize, across: usize) -> i32 {
    let at = i64::try_from(at).unwrap_or(0);
    let across = i64::try_from(across.max(1)).unwrap_or(1);
    i32::try_from(at * 0x7FFF / across).unwrap_or(0)
}

/// The compositor's unit, saying each input's age as it passes.
const AGES_DROP_IN: &str = "# Carried by `cargo xtask test-xlatency`.\n\
    [Service]\n\
    Environment=HYPRIX_INPUT_AGES=1\n";

/// What the image carries besides the programs: yserver's start, the
/// script, the probe and the injector, and the compositor's ages.
fn ports(arch: Arch) -> Result<Vec<crate::ports::File>> {
    let mut ports = crate::yserver::desktop_files(&[]);
    ports.push(crate::ports::File {
        path: "etc/ferrix/units/hyprix.service.d/ages.conf".to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(AGES_DROP_IN.as_bytes().to_vec()),
    });
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
         env = YSERVER_INPUT_AGES,1\n\
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
            || line.contains("xlatency: grab")
            || line.contains("xlatency: yserver-lock:")
    }) {
        println!("  {arch}: {}", line.trim());
    }
    println!("  {arch}: {}", hops(lines));
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
        Some(median) if (1..=MOST_FRAMES).contains(&median) => judge_grab(arch, lines),
        _ => Err(Error::new(format!(
            "{arch}: the median refresh count is not 1 to {MOST_FRAMES}: the msc does not count refreshes"
        ))),
    }
}

/// What the judge says when the game's warps came with raw motion.
const WARP_RAW: &str = "a warp sent raw motion";

/// Pass when the grabbing game read at least half of `vptr`'s motions, nine
/// in ten of them as the mouse's [`GRAB_STEP`] across and none down, and
/// exactly as many raw motions: what a yserver gives that has hyprix lock
/// the pointer and makes raw events only from the device, as Xorg does.
fn judge_grab(arch: Arch, lines: &[String]) -> Result<()> {
    let Some(line) = lines
        .iter()
        .find(|line| line.contains("xlatency: grab motions "))
    else {
        return Err(Error::new(format!(
            "{arch}: the grabbing game said nothing of what it read (xlatency grab)"
        )));
    };
    let (Some(motions), Some(raw), Some(as_mouse)) = (
        after(line, "motions"),
        after(line, "raw"),
        after(line, "as-the-mouse"),
    ) else {
        return Err(Error::new(format!("{arch}: cannot read {line:?}")));
    };
    if motions * 2 < GRAB_SENT {
        return Err(Error::new(format!(
            "{arch}: the grabbing game read {motions} of {GRAB_SENT} motions"
        )));
    }
    if as_mouse * 10 < motions * 9 {
        return Err(Error::new(format!(
            "{arch}: the grabbing game read {as_mouse} of {motions} motions as the mouse's \
             {GRAB_STEP} pixels: yserver did not lock the pointer"
        )));
    }
    if raw > motions {
        return Err(Error::new(format!(
            "{arch}: {WARP_RAW}: the grabbing game had {raw} raw motions for {motions} motions, \
             which a warp makes none of"
        )));
    }
    if raw < motions {
        return Err(Error::new(format!(
            "{arch}: the grabbing game had {raw} raw motions for {motions} motions"
        )));
    }
    println!(
        "  {arch}: a game's grab and warps: {as_mouse} of {motions} motions read as the \
         mouse's {GRAB_STEP} pixels, {raw} raw, of {GRAB_SENT} sent"
    );
    Ok(())
}

/// The number after `word` in `line`.
fn after(line: &str, word: &str) -> Option<u64> {
    let mut words = line.split_whitespace();
    let _ = words.find(|found| *found == word)?;
    words.next()?.parse().ok()
}

/// The median and 99th percentile of `values`, microseconds, in
/// milliseconds.
fn spread(mut values: Vec<u64>) -> String {
    values.sort_unstable();
    let at = |p: usize| {
        let index = (values.len() * p).div_ceil(100).max(1) - 1;
        values.get(index).map_or("-".to_owned(), |micros| {
            format!("{:.3}", *micros as f64 / 1000.0)
        })
    };
    format!("{} / {}", at(50), at(99))
}

/// Each input's age hop by hop, for the motions xtask sent (the lines
/// before the injector's phase): from the input core's stamp to hyprix's
/// write, on to yserver's hand-over to X, on to the probe's arrival, and on
/// to the scan-out that showed it; and the whole from the stamp. Joined on
/// the stamp (hyprix's and yserver's lines) and on the X time (yserver's
/// and the probe's).
fn hops(lines: &[String]) -> String {
    use std::collections::BTreeMap;
    let hyprix: BTreeMap<u64, u64> = lines
        .iter()
        .filter(|line| line.contains("hyprix: input-age stamp"))
        .filter_map(|line| Some((after(line, "stamp")?, after(line, "age")?)))
        .collect();
    let yserver: BTreeMap<u64, (u64, u64)> = lines
        .iter()
        .filter(|line| line.contains("xlatency: yserver-age: input-age stamp"))
        .filter_map(|line| {
            Some((
                after(line, "xtime")?,
                (after(line, "stamp")?, after(line, "age")?),
            ))
        })
        .collect();
    let mut legs: [Vec<u64>; 5] = Default::default();
    for line in lines
        .iter()
        .take_while(|line| !line.contains("xlatency: injector"))
        .filter(|line| line.contains("xlatency: shown "))
    {
        let (Some(xtime), Some(arrival), Some(ust)) = (
            after(line, "xtime"),
            after(line, "arrival"),
            after(line, "ust"),
        ) else {
            continue;
        };
        let Some(&(stamp, at_yserver)) = yserver.get(&xtime) else {
            continue;
        };
        let Some(&at_hyprix) = hyprix.get(&stamp) else {
            continue;
        };
        let arrival = arrival / 1000;
        let [to_hyprix, to_yserver, to_probe, to_scanout, whole] = &mut legs;
        to_hyprix.push(at_hyprix);
        to_yserver.push(at_yserver.saturating_sub(at_hyprix));
        to_probe.push(arrival.saturating_sub(stamp).saturating_sub(at_yserver));
        to_scanout.push(ust.saturating_sub(arrival));
        whole.push(ust.saturating_sub(stamp));
    }
    if legs[0].is_empty() {
        return format!(
            "per hop: no motion paired ({} hyprix and {} yserver age lines; a yserver that \
             does not take the input's stamp gives none)",
            hyprix.len(),
            yserver.len()
        );
    }
    let [to_hyprix, to_yserver, to_probe, to_scanout, whole] = legs;
    format!(
        "per hop, median / 99th ms, {} motions: stamp to hyprix {}, to yserver {}, to the \
         probe {}, to the scan-out {}; stamp to scan-out {}",
        whole.len(),
        spread(to_hyprix),
        spread(to_yserver),
        spread(to_probe),
        spread(to_scanout),
        spread(whole)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hops_join_the_three_logs_on_the_stamp_and_the_x_time() {
        let lines: Vec<String> = [
            "  10.0 | hyprix: input-age stamp 1000000 age 300 loop",
            "xlatency: yserver-age: input-age stamp 1000000 xtime 77 age 1300",
            "xlatency: shown 1 arrival 1002000000 swap 6 ust 1020000 msc 4 frames 2 in 1.000 \
             xtime 77",
            "xlatency: shown 2 arrival 5 swap 6 ust 7 msc 4 frames 2 in 1.000 xtime 99",
            "xlatency: injector",
        ]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
        assert_eq!(
            hops(&lines),
            "per hop, median / 99th ms, 1 motions: stamp to hyprix 0.300 / 0.300, to yserver \
             1.000 / 1.000, to the probe 0.700 / 0.700, to the scan-out 18.000 / 18.000; stamp \
             to scan-out 20.000 / 20.000"
        );
        assert!(hops(&lines[3..]).starts_with("per hop: no motion paired"));
    }

    #[test]
    fn a_grab_passes_only_when_the_game_reads_the_mouses_step() {
        let grab = |line: &str| judge_grab(Arch::X86_64, &[line.to_owned()]);
        assert!(
            grab("xlatency: grab motions 118 raw 118 as-the-mouse 116 step 9 echoes 119").is_ok()
        );
        // A raw motion for each warp as well as each motion.
        let warps = grab("xlatency: grab motions 121 raw 243 as-the-mouse 114 step 9 echoes 122");
        assert!(warps.is_err_and(|error| error.to_string().contains(WARP_RAW)));
        assert!(grab("xlatency: grab motions 118 raw 119 as-the-mouse 116 step 9").is_err());
        assert!(grab("xlatency: grab motions 118 raw 117 as-the-mouse 116 step 9").is_err());
        // No lock: the game reads wherever hyprix's pointer is.
        let unlocked =
            grab("xlatency: grab motions 120 raw 120 as-the-mouse 1 step 9 echoes 0 first 52,-3");
        assert!(unlocked.is_err_and(|error| {
            error
                .to_string()
                .contains("yserver did not lock the pointer")
        }));
        assert!(grab("xlatency: grab motions 20 raw 20 as-the-mouse 20 step 9").is_err());
        assert!(grab("xlatency: grab motions 120 raw 3 as-the-mouse 120 step 9").is_err());
        assert!(judge_grab(Arch::X86_64, &[]).is_err());
    }

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
        assert!(judge(Arch::X86_64, &lines, &timed).is_err(), "no grab line");
        lines.push(
            "xlatency: grab motions 118 raw 118 as-the-mouse 115 step 9 echoes 118 first 9,0"
                .to_owned(),
        );
        assert!(judge(Arch::X86_64, &lines, &timed).is_ok());
        let stuck: Vec<String> = lines
            .iter()
            .map(|line| line.replace("frames 2", "frames 0"))
            .collect();
        assert!(judge(Arch::X86_64, &stuck, &timed).is_err());
    }
}
