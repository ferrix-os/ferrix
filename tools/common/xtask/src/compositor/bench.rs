//! `bench-chrome` and `bench-chrome-video`: Chrome on the compositor, driven
//! for thirty seconds, and what that cost.
//!
//! Numbers to hold a change to, not gates: each fails only when the boot
//! never got as far as measuring.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::boot::{absolute, build_image, said_on_its_own, undithered, with_the_transcript};
use super::browser::CHROME_WINDOW_PATIENCE;
use super::{Carried, EITHER, Programs, gates_busybox};
use crate::args::Args;
use crate::display::{Qmp, free_port};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The page `bench-chrome` opens: a box turning for ever, which Chrome
/// draws sixty times a second if it can, and a thousand lines to scroll
/// through and point at. No spaces, for [`CHROME_WINDOW_PAGE`]'s reason.
///
/// [`CHROME_WINDOW_PAGE`]: super::browser::CHROME_WINDOW_PAGE
const BENCH_PAGE: &str = "data:text/html,<style>@keyframes%20t{to{transform:rotate(360deg)}}\
%23t{width:120px;height:120px;background:%23c30;animation:t%202s%20linear%20infinite}\
p:hover{background:%23fff}</style><body%20style=background:%23fc0;font-family:sans-serif>\
<div%20id=t></div><script>for(let%20i=0;i<1000;i++)document.body.insertAdjacentHTML(\
'beforeend','<p>Line%20'+i+'%20of%20the%20benchmark,%20long%20enough%20to%20wrap%20a%20little\
%20and%20be%20pointed%20at.</p>')</script>";

/// What the guest runs beside Chrome for `bench-chrome`: it waits for the
/// browser to be up and settled, then says what every process has used and
/// how much memory is taken at the start and after each phase the host
/// drives. Phases are ten seconds on both sides of the wire.
const BENCH_SCRIPT: &str = r#"PATH=/bin
i=0
while [ $i -lt 240 ]; do
  n=0
  for c in /proc/[0-9]*/comm; do
    read -r x < $c 2>/dev/null && [ "$x" = chrome ] && n=$((n+1))
  done
  [ $n -ge 4 ] && break
  sleep 1
  i=$((i+1))
done
sleep 15
snap() {
  for s in /proc/[0-9]*/stat; do
    read -r x < $s 2>/dev/null && echo "bench: $1 proc $x"
  done
  read -r x < /proc/stat
  echo "bench: $1 $x"
  while read -r k v u; do
    case $k in MemTotal:|MemAvailable:|Shmem:) echo "bench: $1 mem $k $v";; esac
  done < /proc/meminfo
}
echo "bench: start"
snap start
for p in idle scroll hover; do
  sleep 10
  snap $p
done
echo "bench: end"
"#;

/// The phases `bench-chrome` drives, in order, after the one it starts at.
const BENCH_PHASES: [&str; 3] = ["idle", "scroll", "hover"];

/// `cargo xtask bench-chrome`: Chrome in a window on the compositor, driven
/// for thirty seconds -- left alone with its animation, scrolled, pointed
/// at -- and what that cost: each phase's processor time by who spent it,
/// the compositor's frames and their time, and the memory taken at the end.
///
/// A number to hold a change to, not a gate: it fails only when the boot
/// never got as far as measuring.
///
/// # Errors
///
/// When the image cannot be built, QEMU cannot be run, or the guest never
/// says what it measured.
pub(crate) fn bench_chrome(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "bench-chrome runs on x86-64 only, as Chrome does",
        ));
    }
    let mut args = args.clone();
    args.data_image = Some(crate::chrome::volume()?);
    if !args.memory_given {
        args.memory = crate::chrome::MEMORY;
    }
    let busybox = gates_busybox(arch).ok_or_else(|| {
        Error::new("bench-chrome needs a static busybox: Alpine's busybox-static at ~/.local/share/ferrix/busybox/x86_64/bin/busybox.static, or ferrousli's from `cargo xtask busybox`")
    })?;
    let programs = Programs::build(arch)?;
    let mut ports = crate::rustc::files(crate::chrome::LINKS);
    ports.extend(crate::chrome::window_files());
    ports.push(crate::ports::File {
        path: "etc/bench.sh".to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(BENCH_SCRIPT.as_bytes().to_vec()),
    });
    let carried = Carried {
        busybox: Some(PathBuf::from(busybox)),
        ports,
        ..Carried::none()
    };
    let config = format!(
        "# Carried into the initramfs by `cargo xtask bench-chrome`.\n{}exec-once = {}\nexec-once = /bin/busybox sh /etc/bench.sh\n",
        crate::chrome::WINDOW_ENV,
        crate::chrome::window_command(BENCH_PAGE)
    );
    let (image, kernel) = build_image(arch, &programs, &undithered(&config), carried, &args)?;
    let port = free_port()?;
    let mut qemu_args = args;
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let mut said: Vec<String> = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let started = watching.read_more(Instant::now() + CHROME_WINDOW_PATIENCE, |lines| {
            lines.iter().any(|line| line.contains("bench: start"))
        })?;
        if !started {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: the guest never started measuring")),
                watching,
            ));
        }
        drive_bench(&mut qmp, watching)?;
        let _ = watching.read_more(Instant::now() + Duration::from_secs(30), |lines| {
            lines.iter().any(|line| line.contains("bench: end"))
        })?;
        said = watching.after().to_vec();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    let report = bench_report(&said)?;
    println!("{report}");
    Ok(())
}

/// The input of [`BENCH_PHASES`], ten seconds each: nothing, the wheel
/// turned down and back up, and the pointer swept across the page.
fn drive_bench(qmp: &mut Qmp, watching: &mut Watching<'_>) -> Result<()> {
    let wheel = |button: &str, down: bool| {
        format!("{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":\"{button}\"}}}}")
    };
    let phase = Duration::from_secs(10);
    // Left alone: only the animation draws.
    let _ = watching.read_more(Instant::now() + phase, |_| false)?;
    // Scrolled: a notch every 50 ms, down for five seconds and back up.
    qmp.input_send_event(&[absolute("x", 16384), absolute("y", 16384)])?;
    let began = Instant::now();
    let mut turned = 0u32;
    while began.elapsed() < phase {
        let button = if turned % 200 < 100 {
            "wheel-down"
        } else {
            "wheel-up"
        };
        qmp.input_send_event(&[wheel(button, true)])?;
        qmp.input_send_event(&[wheel(button, false)])?;
        turned += 1;
        std::thread::sleep(Duration::from_millis(50));
    }
    // Pointed at: across the page and back, a move every 16 ms.
    let began = Instant::now();
    let mut step = 0i32;
    while began.elapsed() < phase {
        let across = (step % 100 - 50).abs();
        let x = 4000 + across * 500;
        let y = 6000 + (step % 37) * 600;
        qmp.input_send_event(&[absolute("x", x), absolute("y", y)])?;
        step += 1;
        std::thread::sleep(Duration::from_millis(16));
    }
    Ok(())
}

/// One process as `/proc/<pid>/stat` said it in a `bench:` line: its
/// name, processor time in clock ticks and resident pages.
fn bench_process(stat: &str) -> Option<(u32, String, u64, u64)> {
    let pid = stat.split_whitespace().next()?.parse().ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_owned();
    let rest: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    // After the name: state is field 3, utime 14, stime 15 and rss 24.
    let field = |number: usize| rest.get(number - 3)?.parse::<u64>().ok();
    Some((pid, name, field(14)? + field(15)?, field(24)?))
}

/// What one snapshot of the guest held.
#[derive(Default)]
struct BenchSnap {
    /// Each process's name and ticks, by pid.
    ticks: std::collections::BTreeMap<u32, (String, u64)>,
    /// Resident pages by process name, and how many processes had it.
    resident: std::collections::BTreeMap<String, (u64, u32)>,
    /// `/proc/stat`'s busy and idle ticks, summed over the processors.
    busy: u64,
    idle: u64,
    /// `/proc/meminfo`, in KiB, by key.
    memory: std::collections::BTreeMap<String, u64>,
    /// The reports the compositor had made before it, the frames they
    /// counted, and the microseconds those frames took.
    frames: (u64, u64, u64),
}

impl BenchSnap {
    /// Take in one `bench:` line's worth, after its tag.
    fn take(&mut self, what: &str) {
        if let Some(stat) = what.strip_prefix("proc ") {
            if let Some((pid, name, ticks, pages)) = bench_process(stat) {
                let entry = self.resident.entry(name.clone()).or_default();
                entry.0 += pages;
                entry.1 += 1;
                let _ = self.ticks.insert(pid, (name, ticks));
            }
        } else if let Some(memory) = what.strip_prefix("mem ") {
            let mut words = memory.split_whitespace();
            if let (Some(key), Some(value)) = (words.next(), words.next()) {
                let _ = self.memory.insert(
                    key.trim_end_matches(':').to_owned(),
                    value.parse().unwrap_or(0),
                );
            }
        } else if let Some(cpu) = what.strip_prefix("cpu ") {
            let ticks: Vec<u64> = cpu
                .split_whitespace()
                .filter_map(|word| word.parse().ok())
                .collect();
            self.busy = ticks.iter().take(3).sum();
            self.idle = ticks.get(3).copied().unwrap_or(0);
        }
    }
}

/// The guest's snapshots, by tag in the order taken, from its `bench:` lines
/// and the compositor's frame reports between them.
fn bench_snapshots(said: &[String]) -> Vec<(String, BenchSnap)> {
    let mut snaps: Vec<(String, BenchSnap)> = Vec::new();
    let (mut reports, mut counted, mut spent) = (0u64, 0u64, 0u64);
    for line in said {
        let line = said_on_its_own(line);
        if let Some(rest) = line.strip_prefix("hyprix: frames ") {
            let words: Vec<&str> = rest.split_whitespace().collect();
            // "N slowest of the last M X us, all of them Y us (...".
            if let (Some(m), Some(y)) = (words.get(5), words.get(11)) {
                reports += 1;
                counted += m.parse::<u64>().unwrap_or(0);
                spent += y.parse::<u64>().unwrap_or(0);
            }
            continue;
        }
        let Some((tag, what)) = line
            .strip_prefix("bench: ")
            .and_then(|rest| rest.split_once(' '))
        else {
            continue;
        };
        if snaps.last().is_none_or(|(last, _)| last != tag) {
            let snap = BenchSnap {
                frames: (reports, counted, spent),
                ..BenchSnap::default()
            };
            snaps.push((tag.to_owned(), snap));
        }
        if let Some((_, snap)) = snaps.last_mut() {
            snap.take(what);
        }
    }
    snaps
}

/// One phase's row: how busy the machine was, the compositor's reports in
/// it, the frames they counted and their time, and the processor time each
/// process name spent.
///
/// The frames per second are the frames per report, not the frames over
/// ten seconds. The compositor reports at the first frame a second or more
/// after its last report, so a phase holds whole reports only: ten of them,
/// or nine when the phase's ten seconds and a little began just after one.
/// Counted over ten seconds, the nine read as sixty frames lost, a second
/// of nothing drawn that nobody saw. A report that covers a real stall
/// counts fewer frames for its second, so the rate still shows it.
fn bench_row(tag: &str, before: &BenchSnap, after: &BenchSnap) -> String {
    let mut by: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
    for (pid, (name, ticks)) in &after.ticks {
        let was = before.ticks.get(pid).map_or(0, |(_, ticks)| *ticks);
        *by.entry(name.as_str()).or_default() += ticks.saturating_sub(was);
    }
    let mut by: Vec<(&str, u64)> = by.into_iter().filter(|&(_, ticks)| ticks >= 10).collect();
    by.sort_by_key(|&(_, ticks)| std::cmp::Reverse(ticks));
    let busy = after.busy.saturating_sub(before.busy);
    let idle = after.idle.saturating_sub(before.idle);
    let busy_share = if busy + idle == 0 {
        0.0
    } else {
        100.0 * busy as f64 / (busy + idle) as f64
    };
    let reports = after.frames.0.saturating_sub(before.frames.0);
    let drawn = after.frames.1.saturating_sub(before.frames.1);
    let took = after.frames.2.saturating_sub(before.frames.2);
    let rate = if reports == 0 {
        0.0
    } else {
        drawn as f64 / reports as f64
    };
    let per_frame = if drawn == 0 {
        0.0
    } else {
        took as f64 / drawn as f64 / 1000.0
    };
    // A tick is a hundredth of a second and a phase ten seconds, so a
    // name's ticks in a phase over ten are its percent of one processor.
    let spent: Vec<String> = by
        .iter()
        .map(|(name, ticks)| format!("{name} {}%", ticks / 10))
        .collect();
    format!(
        "bench-chrome: {tag:<7} {busy_share:>5.1} {reports:>7} {drawn:>6} {rate:>5.1} {per_frame:>9.2}  {}\n",
        spent.join(", ")
    )
}

/// `bench-chrome`'s table, from the guest's `bench:` lines and the
/// compositor's frame reports between them.
fn bench_report(said: &[String]) -> Result<String> {
    use std::fmt::Write as _;
    let snaps = bench_snapshots(said);
    if snaps.len() < BENCH_PHASES.len() + 1 {
        return Err(Error::new(format!(
            "the guest said {} of the {} snapshots bench-chrome takes",
            snaps.len(),
            BENCH_PHASES.len() + 1
        )));
    }
    let mut out = String::from(
        "bench-chrome: phase   busy% reports frames   fps  ms/frame  processor time by process name\n",
    );
    for pair in snaps.windows(2) {
        if let [(_, before), (tag, after)] = pair {
            out.push_str(&bench_row(tag, before, after));
        }
    }
    if let Some((_, last)) = snaps.last() {
        let total = last.memory.get("MemTotal").copied().unwrap_or(0);
        let available = last.memory.get("MemAvailable").copied().unwrap_or(0);
        let _ = writeln!(
            out,
            "bench-chrome: memory used {} MiB of {} MiB, shmem {} MiB",
            total.saturating_sub(available) / 1024,
            total / 1024,
            last.memory.get("Shmem").copied().unwrap_or(0) / 1024
        );
        for (name, (pages, count)) in &last.resident {
            if *pages * 4 >= 8 * 1024 {
                let _ = writeln!(
                    out,
                    "bench-chrome: resident {name} {} MiB in {count} processes",
                    pages * 4 / 1024
                );
            }
        }
    }
    Ok(out)
}

/// Where `bench-chrome-video`'s page and video are in the guest.
const VIDEO_DIRECTORY: &str = "usr/share/ferrix/bench";

/// The page `bench-chrome-video` opens: the video, playing with its sound
/// and looping, and once a second a console line with where it is and the
/// frames it has shown and dropped, which Chrome's `--enable-logging=stderr`
/// puts on the serial line.
const VIDEO_PAGE: &str = r"<!doctype html>
<body style='margin:0;background:#000'>
<video id=v src=video.webm autoplay loop style='width:100%'></video>
<script>
const v = document.getElementById('v');
setInterval(() => {
  const q = v.getVideoPlaybackQuality();
  console.log('bench-video: ' + v.currentTime.toFixed(3) + ' ' + q.totalVideoFrames + ' ' +
    q.droppedVideoFrames + ' ' + v.readyState + ' ' + (v.paused ? 'paused' : 'playing'));
}, 1000);
</script>
";

/// The pitch of the video's sound: one tone, so that anything else in what
/// the card played is a fault.
const VIDEO_TONE_HZ: f64 = 440.0;

/// What the guest runs beside Chrome for `bench-chrome-video`: the snapshots
/// of [`BENCH_SCRIPT`], after each of three ten-second phases of playing.
const VIDEO_BENCH_SCRIPT: &str = r#"PATH=/bin
i=0
while [ $i -lt 240 ]; do
  n=0
  for c in /proc/[0-9]*/comm; do
    read -r x < $c 2>/dev/null && [ "$x" = chrome ] && n=$((n+1))
  done
  [ $n -ge 4 ] && break
  sleep 1
  i=$((i+1))
done
sleep 15
snap() {
  for s in /proc/[0-9]*/stat; do
    read -r x < $s 2>/dev/null && echo "bench: $1 proc $x"
  done
  read -r x < /proc/stat
  echo "bench: $1 $x"
  while read -r k v u; do
    case $k in MemTotal:|MemAvailable:|Shmem:) echo "bench: $1 mem $k $v";; esac
  done < /proc/meminfo
}
echo "bench: start"
snap start
for p in play1 play2 play3; do
  sleep 10
  snap $p
done
echo "bench: end"
"#;

/// The video `bench-chrome-video` plays: `FERRIX_BENCH_VIDEO` when it names
/// one -- a clip fetched from a video site, say -- or else one made here with
/// the host's `ffmpeg`, once, and kept: forty seconds of a moving test
/// picture at 1280x720 and 30 frames a second in VP9, as a video site sends
/// to a window of that size, with a 440 Hz tone in Opus, stereo at 48 kHz.
fn bench_video() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("FERRIX_BENCH_VIDEO") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| Error::new("HOME is not set"))?;
    let directory = PathBuf::from(home).join(".local/share/ferrix/bench-video");
    let video = directory.join("tone-720p30-vp9-opus.webm");
    if video.is_file() {
        return Ok(video);
    }
    std::fs::create_dir_all(&directory)
        .map_err(|error| Error::new(format!("{}: {error}", directory.display())))?;
    let partial = directory.join("partial.webm");
    let status = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "aevalsrc=0.5*sin(2*PI*440*t):s=48000:c=stereo",
        ])
        .args([
            "-t",
            "40",
            "-c:v",
            "libvpx-vp9",
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
        ])
        .args([
            "-b:v", "1500k", "-g", "60", "-c:a", "libopus", "-b:a", "128k",
        ])
        .arg(&partial)
        .status()
        .map_err(|error| Error::new(format!("running ffmpeg: {error}")))?;
    if !status.success() {
        return Err(Error::new(format!(
            "ffmpeg could not make {}",
            video.display()
        )));
    }
    std::fs::rename(&partial, &video)
        .map_err(|error| Error::new(format!("{}: {error}", video.display())))?;
    Ok(video)
}

/// What `bench-chrome-video`'s image carries beside the compositor: the
/// volume's links, or ferrousli's in glibc's place, the fonts, the guest's
/// script, and the page with its video.
fn bench_video_files(
    arch: Arch,
    volume: &Path,
    ferrousli: bool,
    args: &Args,
    video_bytes: Vec<u8>,
) -> Result<Vec<crate::ports::File>> {
    let mut ports = if ferrousli {
        crate::chrome::ferrousli_files(arch, volume, crate::chrome::WINDOW_PROGRAM, args)?
    } else {
        crate::rustc::files(crate::chrome::LINKS)
    };
    ports.extend(crate::chrome::window_files());
    for (name, bytes) in [
        ("etc/bench.sh", VIDEO_BENCH_SCRIPT.as_bytes().to_vec()),
        (
            &format!("{VIDEO_DIRECTORY}/video.html"),
            VIDEO_PAGE.as_bytes().to_vec(),
        ),
        (&format!("{VIDEO_DIRECTORY}/video.webm"), video_bytes),
    ] {
        ports.push(crate::ports::File {
            path: name.to_owned(),
            mode: 0o644,
            content: crate::ports::Content::Bytes(bytes),
        });
    }
    Ok(ports)
}

/// `cargo xtask bench-chrome-video`: Chrome in a window on the compositor
/// playing a video with its sound through the virtio-snd card, as a person
/// watching a video site does, for thirty seconds, and what that cost and
/// how well it went: each phase's processor time by who spent it, the
/// compositor's frames, the frames the video showed and dropped, the
/// underruns the kernel counted, and the card's sound as QEMU's `wav`
/// backend took it -- which the device consumes at the host's pace, so a
/// second in which it was given less than a second of sound shows as a
/// short second, and a gap Chrome filled with silence shows as silence in
/// the tone.
///
/// A number to hold a change to, not a gate: it fails only when the boot
/// never got as far as measuring.
///
/// # Errors
///
/// When the video cannot be made, the image cannot be built, QEMU cannot be
/// run, or the guest never says what it measured.
pub(crate) fn bench_chrome_video(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "bench-chrome-video runs on x86-64 only, as Chrome does",
        ));
    }
    let video = bench_video()?;
    let video_bytes = std::fs::read(&video)
        .map_err(|error| Error::new(format!("{}: {error}", video.display())))?;
    let mut args = args.clone();
    let volume = crate::chrome::volume()?;
    args.data_image = Some(volume.clone());
    if !args.memory_given {
        args.memory = crate::chrome::MEMORY;
    }
    let busybox = gates_busybox(arch).ok_or_else(|| {
        Error::new(
            "bench-chrome-video needs a static busybox: Alpine's busybox-static at ~/.local/share/ferrix/busybox/x86_64/bin/busybox.static, or ferrousli's from `cargo xtask busybox`",
        )
    })?;
    let programs = Programs::build(arch)?;
    // The C library is named, never a default another command's may change:
    // glibc, the volume's own, unless `--interpreter` or `--library` asks for
    // ferrousli, as `test-chrome-window` takes them. Two runs compared are
    // then the same browser on the same library.
    let ferrousli = crate::chrome::on_ferrousli(&args);
    println!(
        "  {arch}: Chrome on {}",
        if ferrousli {
            "ferrousli's loader and libc.so.6"
        } else {
            "the volume's glibc"
        }
    );
    let ports = bench_video_files(arch, &volume, ferrousli, &args, video_bytes)?;
    let carried = Carried {
        busybox: Some(PathBuf::from(busybox)),
        ports,
        ..Carried::none()
    };
    let page = format!("file:///{VIDEO_DIRECTORY}/video.html");
    let config = format!(
        "# Carried into the initramfs by `cargo xtask bench-chrome-video`.\n{}{}exec-once = {}\nexec-once = /bin/busybox sh /etc/bench.sh\n",
        crate::chrome::WINDOW_ENV,
        crate::chrome::window_library_path(ferrousli),
        crate::chrome::window_command(&page)
    );
    let (image, kernel) = build_image(arch, &programs, &undithered(&config), carried, &args)?;
    let wav = paths::build_dir(arch).join("bench-chrome-video.wav");
    if wav.exists() {
        std::fs::remove_file(&wav)
            .map_err(|error| Error::new(format!("{}: {error}", wav.display())))?;
    }
    let mut qemu_args = args;
    qemu_args.display = true;
    // Through QEMU's mixing engine at the card's own rate, as a desktop's
    // sound server is, not `wav:PATH`'s engine-less file: with the engine
    // off, the `wav` backend runs at its own default of 44100 Hz whatever
    // the stream's rate, so the device would take 48 kHz frames 8% slow.
    if qemu_args.audio.is_none() {
        qemu_args.audio = Some(format!("wav,path={}", wav.display()));
    }
    let mut said: Vec<String> = Vec::new();
    // The card's file's length each half second the phases ran, host time.
    let mut sizes: Vec<(Instant, u64)> = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let started = watching.read_more(Instant::now() + CHROME_WINDOW_PATIENCE, |lines| {
            lines.iter().any(|line| line.contains("bench: start"))
        })?;
        if !started {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: the guest never started measuring")),
                watching,
            ));
        }
        let ended = |lines: &[String]| lines.iter().any(|line| line.contains("bench: end"));
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let length = std::fs::metadata(&wav).map_or(0, |metadata| metadata.len());
            sizes.push((Instant::now(), length));
            if watching.read_more(Instant::now() + Duration::from_millis(500), ended)? {
                break;
            }
        }
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    let mut report = bench_video_report(&said)?;
    report.push_str(&bench_video_sound(&wav, &sizes));
    println!("{report}");
    Ok(())
}

/// `bench-chrome-video`'s table: [`bench_row`] for each phase, then the
/// video's frames shown and dropped in each second the page reported, and
/// the kernel's underrun lines.
fn bench_video_report(said: &[String]) -> Result<String> {
    use std::fmt::Write as _;
    let snaps = bench_snapshots(said);
    if snaps.len() < 4 {
        return Err(Error::new(format!(
            "the guest said {} of the 4 snapshots bench-chrome-video takes",
            snaps.len()
        )));
    }
    let mut out = String::from(
        "bench-chrome: phase   busy% reports frames   fps  ms/frame  processor time by process name\n",
    );
    for pair in snaps.windows(2) {
        if let [(_, before), (tag, after)] = pair {
            out.push_str(&bench_row(tag, before, after));
        }
    }
    // "bench-video: <time> <shown> <dropped> <readyState> <playing>".
    let mut last: Option<(f64, u64, u64)> = None;
    let (mut seconds, mut slow, mut dropped_seconds) = (0u32, 0u32, 0u32);
    let mut worst: Option<f64> = None;
    let mut totals = (0u64, 0u64);
    for line in said {
        let Some(rest) = line.split_once("bench-video: ").map(|(_, rest)| rest) else {
            continue;
        };
        let words: Vec<&str> = rest.trim_end_matches('"').split_whitespace().collect();
        let (Some(time), Some(shown), Some(dropped)) = (
            words.first().and_then(|word| word.parse::<f64>().ok()),
            words.get(1).and_then(|word| word.parse::<u64>().ok()),
            words.get(2).and_then(|word| word.parse::<u64>().ok()),
        ) else {
            continue;
        };
        if let Some((was_time, was_shown, was_dropped)) = last {
            // A loop back to the start counts from zero again.
            let advanced = if time >= was_time {
                time - was_time
            } else {
                time
            };
            seconds += 1;
            worst = Some(worst.map_or(advanced, |least| least.min(advanced)));
            if advanced < 0.9 {
                slow += 1;
            }
            if dropped > was_dropped {
                dropped_seconds += 1;
            }
            totals.0 += shown.saturating_sub(was_shown);
            totals.1 += dropped.saturating_sub(was_dropped);
        }
        last = Some((time, shown, dropped));
    }
    let _ = writeln!(
        out,
        "bench-video: {seconds} seconds reported, {slow} advanced under 0.9 s (least {:.2} s), \
         {} frames shown, {} dropped, in {dropped_seconds} seconds",
        worst.unwrap_or(0.0),
        totals.0,
        totals.1
    );
    let underruns: Vec<&str> = said
        .iter()
        .map(|line| said_on_its_own(line))
        .filter(|line| line.contains("underrun"))
        .collect();
    let _ = writeln!(out, "bench-video: {} underrun lines", underruns.len());
    for line in underruns.iter().rev().take(3).rev() {
        let _ = writeln!(out, "bench-video:   {line}");
    }
    Ok(out)
}

/// What the card played: the frames per host second over the phases, the
/// seconds that were short of 48000 by more than a period, and the silences
/// inside the tone -- runs of 2 ms or more where the tone's samples were
/// near nothing, which is what Chrome writes when its renderer is late.
fn bench_video_sound(wav: &Path, sizes: &[(Instant, u64)]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let Ok(bytes) = std::fs::read(wav) else {
        let _ = writeln!(out, "bench-video: no sound file at {}", wav.display());
        return out;
    };
    // Frames the device took in each whole host second, from the lengths
    // taken each half second: the header is fixed, so differences are data.
    let mut rates: Vec<u64> = Vec::new();
    let mut from = sizes.first().copied();
    for &(when, length) in sizes {
        let Some((was, was_length)) = from else {
            break;
        };
        if when.duration_since(was) >= Duration::from_secs(1) {
            let frames = length.saturating_sub(was_length) / 4;
            let per_second = frames as f64 / when.duration_since(was).as_secs_f64();
            rates.push(per_second as u64);
            from = Some((when, length));
        }
    }
    // The first and last seconds may hold the start and the stop.
    let inner = rates.get(1..rates.len().saturating_sub(1)).unwrap_or(&[]);
    let short = inner.iter().filter(|&&rate| rate < 48_000 - 960).count();
    let least = inner.iter().min().copied().unwrap_or(0);
    let _ = writeln!(
        out,
        "bench-video: the card took {:?} frames a second; {short} of {} inner seconds short by \
         more than a period (least {least})",
        rates,
        inner.len()
    );
    let Some(data) = bytes.windows(4).position(|window| window == b"data") else {
        return out;
    };
    let (whole, _) = bytes.get(data + 8..).unwrap_or(&[]).as_chunks::<4>();
    let left: Vec<i16> = whole
        .iter()
        .map(|&[a, b, _, _]| i16::from_le_bytes([a, b]))
        .collect();
    let loud = |sample: &i16| sample.unsigned_abs() > 256;
    let (Some(start), Some(end)) = (left.iter().position(loud), left.iter().rposition(loud)) else {
        let _ = writeln!(out, "bench-video: the card played nothing but silence");
        return out;
    };
    let tone = left.get(start..=end).unwrap_or(&[]);
    let mut gaps: Vec<(usize, usize)> = Vec::new();
    let mut quiet_from = None;
    for (index, sample) in tone.iter().enumerate() {
        match (loud(sample), quiet_from) {
            (false, None) => quiet_from = Some(index),
            (true, Some(began)) => {
                if index - began >= 96 {
                    gaps.push((began, index - began));
                }
                quiet_from = None;
            }
            _ => {}
        }
    }
    // A jump inside the tone: a sample far from where a sine of this pitch
    // continuing the last two would be, which a lost or repeated stretch
    // makes and a whole tone never does.
    let turn = 2.0 * (2.0 * std::f64::consts::PI * VIDEO_TONE_HZ / 48_000.0).cos();
    let jumps = tone
        .array_windows::<3>()
        .filter(|&&[a, b, c]| {
            let [a, b, c] = [f64::from(a), f64::from(b), f64::from(c)];
            (c - (turn * b - a)).abs() > 2000.0
        })
        .count();
    let silent: usize = gaps.iter().map(|&(_, frames)| frames).sum();
    let _ = writeln!(
        out,
        "bench-video: {:.2} s of tone, {} silences of 2 ms or more ({:.0} ms in all), {jumps} jumps",
        tone.len() as f64 / 48_000.0,
        gaps.len(),
        silent as f64 / 48.0
    );
    for &(at, frames) in gaps.iter().take(10) {
        let _ = writeln!(
            out,
            "bench-video:   silence at {:.3} s for {:.1} ms",
            at as f64 / 48_000.0,
            frames as f64 / 48.0
        );
    }
    let _ = writeln!(out, "bench-video: the sound is {}", wav.display());
    out
}

#[cfg(test)]
mod tests {
    /// `bench-chrome`'s table from the guest's snapshots: each phase's
    /// processor time by name, from the ticks each process added, and the
    /// frames the compositor reported between two snapshots.
    #[test]
    fn the_chrome_bench_reads_its_snapshots() {
        let stat = |pid: u32, name: &str, ticks: u64| {
            format!("{pid} ({name}) S 1 1 1 0 -1 0 0 0 0 0 {ticks} 0 0 0 20 0 1 0 0 4096 256")
        };
        let mut said = Vec::new();
        for (tag, chrome, gpu, busy) in [("start", 100, 10, 1000), ("idle", 150, 30, 1100)] {
            said.push(format!(
                "  1.00 | bench: {tag} proc {}",
                stat(7, "chrome", chrome)
            ));
            said.push(format!(
                "  1.00 | bench: {tag} proc {}",
                stat(8, "gpu", gpu)
            ));
            said.push(format!(
                "  1.00 | bench: {tag} cpu  {busy} 0 0 {busy} 0 0 0"
            ));
            said.push(format!("  1.00 | bench: {tag} mem MemTotal: 4096000 kB"));
            said.push(format!(
                "  1.00 | bench: {tag} mem MemAvailable: 3072000 kB"
            ));
            if tag == "start" {
                said.push(
                    "  1.00 | hyprix: frames 9 slowest of the last 60 9000 us, all of them 300000 us (x)"
                        .to_owned(),
                );
            }
        }
        for tag in ["scroll", "hover"] {
            said.push(format!("  1.00 | bench: {tag} cpu  1100 0 0 1100 0 0 0"));
            said.push(format!("  1.00 | bench: {tag} mem MemTotal: 4096000 kB"));
            said.push(format!(
                "  1.00 | bench: {tag} mem MemAvailable: 3072000 kB"
            ));
        }
        let report = super::bench_report(&said).unwrap();
        assert!(
            report.contains(
                "bench-chrome: idle     50.0       1     60  60.0      5.00  chrome 5%, gpu 2%"
            ),
            "{report}"
        );
        assert!(
            report.contains("memory used 1000 MiB of 4000 MiB"),
            "{report}"
        );
    }
}
