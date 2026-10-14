//! `test-frames`: stage 24's frame recorder on Ferrix, with its negative
//! control (docs/GAME-PATH.md §2.4 and §8, R1). No card.
//!
//! The desktop is `test-xwindow`'s: hyprix, with yserver on `:0` as a
//! Wayland client of it, from yserver's volume. [`SCRIPT`] runs the
//! recorder's test client, `framegl` (`tools/common/game/framegl.c`), on
//! Mesa's llvmpipe with the recorder preloaded, twice: [`PLAIN`] seconds as
//! it is, then [`STALLED`] seconds with `FRAMEREC_STALL_MS=50`, a 50 ms
//! sleep inside the swap every ten seconds. Each record comes out on the
//! serial line, and `crate::frames` reads it.
//!
//! It passes when yserver answered the recorder's `PresentNotifyMSC` for
//! the swaps, its msc moves as a clock at the screen's refresh, the paced
//! client's median frame is one refresh, and in the second run each of the
//! [`STALLS`] stalls is counted as exactly three missed refreshes, by msc
//! and by time, at the swap it came before. A 50 ms stall at 60 Hz is three
//! refreshes and the frame after it shows at the fourth.
//!
//! The rootless backend gained a Present clock for this (yserver's
//! `wayland/clock.rs`); before it, the msc never moved, and the gate says
//! so.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::boot::{build_image, undithered, with_the_transcript};
use super::{Carried, EITHER, Programs, gates_busybox};
use crate::args::Args;
use crate::frames::{Record, Summary, summarise};
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::qemu::Watching;
use crate::{Error, Result};

/// Where [`SCRIPT`] is in the image.
const SCRIPT_PATH: &str = "etc/frames.sh";

/// Where the recorder and the client are in the image.
const RECORDER_PATH: &str = "rig/libframerec.so";
const CLIENT_PATH: &str = "rig/framegl";

/// Seconds of the run without a stall.
const PLAIN: u64 = 30;

/// Seconds of the run with the stalls: three of them, at 10, 20 and 30 s
/// after its first swap.
const STALLED: u64 = 35;

/// The stalls the second run must hold.
const STALLS: usize = 3;

/// The refreshes each stall must be counted as.
const MISSED: u64 = 3;

/// What the script prints last, whatever happened.
const END: &str = "frames: end";

/// How long the script may take from the boot: yserver's start, and both
/// runs with their records printed.
const PATIENCE: Duration = Duration::from_secs(600);

/// The script hyprix's `exec-once` runs. Every line of its own starts
/// `frames:`; each run's output and record come out as
/// `frames: <run>: out:` and `frames: <run>: rec:` lines. The client and
/// the recorder run on the volume's glibc through its loader, as yserver
/// does, with the recorder given to the loader as `LD_PRELOAD` would give
/// it.
const SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp XDG_RUNTIME_DIR=/tmp
export LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe
echo "frames: start"
waited=0
while [ ! -S /tmp/.X11-unix/X0 ] && [ $waited -lt 120 ]; do
    sleep 1
    waited=$((waited + 1))
done
echo "frames: the X socket was there after ${waited}s"
run() {
    FRAMEREC_OUT=/tmp/$1.rec FRAMEREC_STALL_MS=$2 SWAPFPS_LOG=/tmp/$1.fps \
        /data/usr/lib64/ld-linux-x86-64.so.2 --library-path /data/usr/lib/x86_64-linux-gnu \
        --preload /rig/libframerec.so /rig/framegl $3 > /tmp/$1.out 2>&1
    echo "frames: $1: exited $?"
    sed "s/^/frames: $1: out: /" /tmp/$1.out
    sed "s/^/frames: $1: fps: /" /tmp/$1.fps
    sed "s/^/frames: $1: rec: /" /tmp/$1.rec
}
run plain 0 30
run stall 50 35
tail -n 20 /tmp/yserver.log | sed 's/^/frames: yserver: /'
echo "frames: end"
"#;

/// The desktop's configuration: yserver on `:0`, the client's window
/// floating at its own size, and [`SCRIPT`].
fn config() -> String {
    format!(
        "# Carried into the initramfs by `cargo xtask test-frames`.\n\
         env = XDG_RUNTIME_DIR,/tmp\n{}\
         windowrule = float, match:class ^(framegl)$\n\
         exec-once = /bin/busybox sh /{SCRIPT_PATH}\n",
        crate::yserver::desktop_config(),
    )
}

/// A file of the image, from bytes read off the host.
fn carried(path: &str, mode: u32, from: &Path) -> Result<File> {
    let bytes = std::fs::read(from)
        .map_err(|error| Error::new(format!("reading {}: {error}", from.display())))?;
    Ok(File {
        path: path.to_owned(),
        mode,
        content: Content::Bytes(bytes),
    })
}

/// The image and kernel: hyprix with [`config`], yserver's start and links,
/// the recorder, the client and [`SCRIPT`]; and the recorder's SHA-256.
fn desktop(arch: Arch, args: &Args) -> Result<(PathBuf, PathBuf, String)> {
    let busybox = gates_busybox(arch).ok_or_else(|| {
        Error::new("test-frames needs a static busybox: Alpine's busybox-static at ~/.local/share/ferrix/busybox/x86_64/bin/busybox.static, or ferrousli's from `cargo xtask busybox`")
    })?;
    let built = crate::frames::build()?;
    let sha = crate::frames::sha256_of(&built.recorder)?;
    let programs = Programs::build(arch)?;
    let mut ports = crate::yserver::desktop_files(&[]);
    ports.push(carried(RECORDER_PATH, 0o755, &built.recorder)?);
    ports.push(carried(CLIENT_PATH, 0o755, &built.client)?);
    ports.push(File {
        path: SCRIPT_PATH.to_owned(),
        mode: 0o644,
        content: Content::Bytes(SCRIPT.as_bytes().to_vec()),
    });
    let carried = Carried {
        busybox: Some(PathBuf::from(busybox)),
        ports,
        ..Carried::none()
    };
    let (image, kernel) = build_image(arch, &programs, &undithered(&config()), carried, args)?;
    Ok((image, kernel, sha))
}

/// Boot the desktop, let [`SCRIPT`] run both records, and judge them.
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, the boot fails,
/// or a record does not show a paced client on a clock with each stall
/// counted as three missed refreshes.
pub(crate) fn test_frames(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => return Err(Error::new("test-frames runs on x86-64")),
    };
    let mut args = args.clone();
    args.gl = false;
    args.data_image = Some(crate::yserver::volume()?);
    if !args.memory_given {
        args.memory = crate::yserver::MEMORY;
    }
    if !args.timeout_given {
        args.timeout = PATIENCE.as_secs() + 300;
    }
    let (image, kernel, sha) = desktop(arch, &args)?;
    println!(
        "  {arch}: framegl on llvmpipe through yserver on hyprix, with the frame recorder \
         (libframerec.so {sha}), {PLAIN} s plain and {STALLED} s with a 50 ms stall every 10 s \
         (timeout {}s)",
        args.timeout
    );
    let mut said: Vec<String> = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let ended = watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(END))
        })?;
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        if ended {
            watching.stop_when_done();
            Ok(())
        } else {
            Err(with_the_transcript(
                &Error::new(format!("{arch}: the frames script never ended")),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &args, EITHER, hook)?;
    let (plain, stalled) = judge(&said).map_err(|error| Error::new(format!("{arch}: {error}")))?;
    println!("  {arch}: the plain record, libframerec.so {sha}:\n{plain}");
    println!("  {arch}: the stalled record:\n{stalled}");
    println!(
        "  {arch}: yserver's msc moved as a clock, and each of the {STALLS} stalls of 50 ms was \
         counted as {MISSED} missed refreshes, by msc and by time"
    );
    Ok(())
}

/// One run's record, summarised.
fn run_of(said: &[String], run: &str) -> Result<Summary> {
    let prefix = format!("frames: {run}: rec: ");
    let record = Record::parse(said.iter().map(String::as_str), &prefix);
    if let Some(error) = record.errors.first() {
        return Err(Error::new(format!(
            "the {run} record says `{error}` (framerec.c's `e` lines)"
        )));
    }
    summarise(&record, None).map_err(|error| {
        Error::new(format!(
            "the {run} record: {error}; the `frames: {run}: out:` lines say what framegl did"
        ))
    })
}

/// Whether a summary is of a paced client on a server whose msc is a
/// clock: answered, moving one msc a refresh, and a refresh a frame.
fn judge_clock(run: &str, summary: &Summary) -> Result<()> {
    if summary.answered * 10 < summary.swaps * 9 {
        return Err(Error::new(format!(
            "yserver answered the recorder for {} of the {run} run's {} swaps\n{summary}",
            summary.answered, summary.swaps
        )));
    }
    let Some(refresh) = summary.refresh else {
        return Err(Error::new(format!(
            "yserver's msc never moved by one between two swaps of the {run} run: the rootless \
             backend has no Present clock\n{summary}"
        )));
    };
    if !(15_000.0..18_500.0).contains(&refresh) {
        return Err(Error::new(format!(
            "the {run} run's refresh is {:.3} ms, not about 16.7 ms\n{summary}",
            refresh / 1000.0
        )));
    }
    let expected = summary.seconds * 1e6 / refresh;
    if (summary.msc_span as f64) < expected * 0.9 {
        return Err(Error::new(format!(
            "yserver's msc moved {} in the {run} run's {:.1} s, not about {expected:.0}: it is \
             not a clock\n{summary}",
            summary.msc_span, summary.seconds
        )));
    }
    if (summary.median * 1000.0 - refresh).abs() > refresh * 0.1 {
        return Err(Error::new(format!(
            "the {run} run's median frame is {:.2} ms, not a refresh: framegl was not paced\n\
             {summary}",
            summary.median
        )));
    }
    Ok(())
}

/// Judge both records; their summaries.
///
/// # Errors
///
/// What is wrong, in words.
fn judge(said: &[String]) -> Result<(Summary, Summary)> {
    if !said.iter().any(|line| line.contains(END)) {
        return Err(Error::new("the frames script never ended"));
    }
    let plain = run_of(said, "plain")?;
    judge_clock("plain", &plain)?;
    let stalled = run_of(said, "stall")?;
    judge_clock("stall", &stalled)?;
    if stalled.stalls.len() != STALLS {
        return Err(Error::new(format!(
            "the stalled run holds {} stalls, not {STALLS}\n{stalled}",
            stalled.stalls.len()
        )));
    }
    let wrong: Vec<String> = stalled
        .stalls
        .iter()
        .filter(|(_, msc, time)| *msc != Some(MISSED) || *time != Some(MISSED))
        .map(|(swap, msc, time)| format!("swap {swap}: {msc:?} by msc, {time:?} by time"))
        .collect();
    if !wrong.is_empty() {
        return Err(Error::new(format!(
            "a 50 ms stall must be {MISSED} missed refreshes, by msc and by time: {}\n{stalled}",
            wrong.join("; ")
        )));
    }
    Ok((plain, stalled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    /// The script runs as long as the gate expects, and ends with [`END`].
    #[test]
    fn the_script_matches_the_judge() {
        assert!(SCRIPT.contains(&format!("run plain 0 {PLAIN}\n")));
        assert!(SCRIPT.contains(&format!("run stall 50 {STALLED}\n")));
        assert!(SCRIPT.contains(&format!("echo \"{END}\"")));
        assert!(SCRIPT.contains(&format!("--preload /{RECORDER_PATH} /{CLIENT_PATH} ")));
        assert_eq!(STALLED / 10, STALLS as u64);
    }

    /// A transcript of both runs at 60 Hz, each stall in the second
    /// `extra` refreshes long, with the msc moving (`clock`) or not.
    fn transcript(extra: u64, clock: bool) -> Vec<String> {
        let mut lines = Vec::new();
        for (run, seconds, stalled) in [("plain", PLAIN, false), ("stall", STALLED, true)] {
            let (mut at, mut msc) = (5_000_000_000_u64, 10_u64);
            let mut text = String::new();
            for index in 0..u32::try_from(seconds * 60).unwrap() {
                if stalled && index > 0 && index % 600 == 0 {
                    at += extra * 16_666_667;
                    msc += extra;
                    let _ = writeln!(text, "x {index} 50");
                }
                let _ = writeln!(text, "s {index} {} {at}", at - 2_000_000);
                let shown = if clock { msc } else { 10 };
                let _ = writeln!(text, "c {index} n {} {shown}", at / 1000);
                at += 16_666_667;
                msc += 1;
            }
            lines.extend(
                text.lines()
                    .map(|line| format!("  9.00 | frames: {run}: rec: {line}")),
            );
        }
        lines.push(format!("  99.00 | {END}"));
        lines
    }

    /// Three stalls of three refreshes each pass; a fourth refresh, or an
    /// msc that never moves, fails.
    #[test]
    fn the_judge_wants_three_misses_a_stall_on_a_clock() {
        assert!(judge(&transcript(3, true)).is_ok());
        assert!(judge(&transcript(4, true)).is_err(), "four a stall");
        assert!(judge(&transcript(2, true)).is_err(), "two a stall");
        let frozen = judge(&transcript(3, false)).unwrap_err().to_string();
        assert!(frozen.contains("no Present clock"), "{frozen}");
        assert!(judge(&[]).is_err(), "nothing");
    }
}
