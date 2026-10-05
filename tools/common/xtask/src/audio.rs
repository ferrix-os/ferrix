//! `cargo xtask test-audio`: a second of a counter written to
//! `/dev/snd/pcmC0D0p`, and found again, frame for frame, in the file QEMU
//! wrote of what the device played.
//!
//! `docs/AUDIO.md` L7. Every part below this has its own test -- the ALSA
//! numbers against `asound.h`, virtio-snd against QEMU's source, the stream
//! against Linux's rules, the driver against a device doing what QEMU's does
//! -- and each is a part alone. This is the whole path at once, and nothing
//! in it is simulated: `src/user/system/linux/compositor/tone` writes frames through the ALSA
//! ioctls, the kernel's audio core copies them into its buffer and submits
//! them, the ring-3 driver posts them to a real `virtio-sound-pci`, and
//! QEMU's `wav` backend, with its mixing engine off so it neither resamples
//! nor scales, writes what the device consumed to a file this reads.
//!
//! **The negative control.** The same boot again with tone built with
//! `negative-control`, which writes period 10 twice and skips period 11. The
//! frames still reach the file, so the control does not fail by the guest
//! falling over; the check must fail, and at exactly the first frame of
//! period 11.
//!
//! **The restart.** A third boot, with tone built with `restart`: the
//! `snd` driver killed twice under a running stream of silence, the stream
//! answering `EBADFD` each time, devmgr starting the driver again
//! (`docs/DEVMGR.md` §4), and then the same second played on the third
//! driver's card and held to the same check.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// What every line tone prints begins with: the boot is watched from its
/// first, so a failure in the restart's rounds, before the stream it plays
/// is prepared, is reported in tone's own words.
const SPOKE: &str = "tone: ";
/// What it prints once the drain is over.
const DONE: &str = "tone: done";
/// What it prints when anything was refused.
const FAILED: &str = "tone: failed";

/// Frames tone writes.
const FRAMES: u32 = 48_000;
/// Frames in a period, which the negative control moves one of.
const PERIOD: u32 = 960;

/// How long QEMU is left running after a program's last word, for its audio
/// backend to write what it still holds.
const SETTLE: Duration = Duration::from_secs(1);

/// How long to wait for the second to play and drain, in an emulated guest,
/// after two restarts of the driver for the restart boot.
const PATIENCE: Duration = Duration::from_secs(120);

/// Which `src/user/system/linux/compositor/tone` is built.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavour {
    Plain,
    /// `negative-control`: period 10 twice, period 11 never.
    Negative,
    /// `restart`: the driver killed twice before the second is played.
    Restart,
}

/// Build `src/user/system/linux/compositor/tone` for `arch`, in `flavour`.
fn build_tone(arch: Arch, flavour: Flavour) -> Result<PathBuf> {
    let target = crate::display::target(arch)
        .ok_or_else(|| Error::new(format!("{arch} has no user-space target for tone")))?;
    let feature = match flavour {
        Flavour::Plain => None,
        Flavour::Negative => Some("negative-control"),
        Flavour::Restart => Some("restart"),
    };
    let flavour = match flavour {
        Flavour::Plain => "plain",
        Flavour::Negative => "negative",
        Flavour::Restart => "restart",
    };
    let target_dir = paths::target_dir()
        .join("compositor")
        .join(format!("tone-{flavour}"));
    println!("  building src/user/system/linux/compositor/tone ({flavour}) for {target}");
    let program = target_dir.join(target).join("release").join("tone");
    let mut build = crate::builds::Build::cargo(
        format!("cargo build (src/user/system/linux/compositor/tone, {flavour}) --target {target}"),
        paths::workspace_root().join("src/user/system/linux/compositor"),
    )
    .args([
        "build",
        "--release",
        "-p",
        "compositor-tone",
        "--target",
        target,
    ])
    .env("CARGO_TARGET_DIR", &target_dir)
    .output(&program);
    if let Some(feature) = feature {
        build = build.args(["--features", feature]);
    }
    build.run()?;
    Ok(program)
}

/// Boot tone with a virtio-snd card whose far end writes `wav`, and give the
/// lines tone printed.
fn boot_and_play(arch: Arch, program: &Path, wav: &Path, args: &Args) -> Result<Vec<String>> {
    boot_and_record(arch, program, "", &[], 1, wav, args)
}

/// Boot `init`, running `script` if it is a shell, with `files` in the
/// initramfs and a virtio-snd card whose far end writes `wav`, and give what
/// the guest said once a line of tone's -- or of a script that says the same
/// -- has ended it.
fn boot_and_record(
    arch: Arch,
    init: &Path,
    script: &str,
    files: &[crate::ports::File],
    ends: usize,
    wav: &Path,
    args: &Args,
) -> Result<Vec<String>> {
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel = crate::cargo::build_kernel_with_init(arch, args.release, init, script)?;
    let natives = crate::native::build(arch, args.release)?;
    let initramfs = crate::initramfs::build(None, &natives, None, files)?;
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
    if wav.exists() {
        std::fs::remove_file(wav)
            .map_err(|error| Error::new(format!("{}: {error}", wav.display())))?;
    }
    let mut qemu_args = args.clone();
    qemu_args.audio = Some(format!("wav:{}", wav.display()));
    let mut lines = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let _ = watching.read_more(Instant::now() + PATIENCE, |seen| {
            seen.iter().any(|line| line.contains(FAILED))
                || seen.iter().filter(|line| line.contains(DONE)).count() >= ends
        })?;
        // QEMU's audio backend writes the file at the audio's own pace, a
        // buffer behind the device: a moment for the last of it to land.
        let _ = watching.read_more(Instant::now() + SETTLE, |_| false)?;
        lines = watching.lines().to_vec();
        lines.extend_from_slice(watching.after());
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, SPOKE, hook)?;
    Ok(lines)
}

/// The frames of a WAV file's data, as `(left, right)`, whatever its header
/// says of its length: a QEMU stopped before it closed the file leaves the
/// lengths zero.
fn frames(wav: &[u8]) -> Result<Vec<(u16, u16)>> {
    let data = wav
        .windows(4)
        .position(|window| window == b"data")
        .ok_or_else(|| Error::new("the WAV file has no data chunk".to_owned()))?;
    let samples = wav.get(data + 8..).unwrap_or(&[]);
    let (whole, _) = samples.as_chunks::<4>();
    Ok(whole
        .iter()
        .map(|&[a, b, c, d]| (u16::from_le_bytes([a, b]), u16::from_le_bytes([c, d])))
        .collect())
}

/// Where the played frames stop matching the counter, as the counter's frame
/// number, or `None` when they are frames 1 to [`FRAMES`] exactly, leading
/// and trailing silence aside.
fn first_wrong(played: &[(u16, u16)]) -> Option<u32> {
    let silent = |frame: &(u16, u16)| *frame == (0, 0);
    let start = played
        .iter()
        .position(|frame| !silent(frame))
        .unwrap_or(played.len());
    let end = played
        .iter()
        .rposition(|frame| !silent(frame))
        .map_or(start, |at| at + 1);
    let heard = played.get(start..end).unwrap_or(&[]);
    for n in 1..=FRAMES {
        let expected = (n as u16, !(n as u16));
        match heard.get((n - 1) as usize) {
            Some(frame) if *frame == expected => {}
            _ => return Some(n),
        }
    }
    (heard.len() != FRAMES as usize).then_some(FRAMES + 1)
}

/// What the file holds at the frame where it went wrong, for the message.
fn describe(played: &[(u16, u16)], at: u32) -> String {
    let start = played
        .iter()
        .position(|frame| *frame != (0, 0))
        .unwrap_or(0);
    match played.get(start + (at as usize).saturating_sub(1)) {
        Some((left, right)) => format!("left {left} right {right}"),
        None => "nothing".to_owned(),
    }
}

/// `test-audio` on each architecture asked for.
///
/// # Errors
///
/// A card or stream that refused a request, frames missing, repeated or
/// changed in the file, or a negative control that passed.
pub(crate) fn test_audio(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let wav = paths::build_dir(arch).join("audio.wav");
        // Every build before the first boot, so a plan run, which stops at
        // the first boot, records the later boots' programs too. The pulsed
        // and mixed boots build theirs again where they did, which is then
        // cargo saying they are current.
        let plain = build_tone(arch, Flavour::Plain)?;
        let negative = build_tone(arch, Flavour::Negative)?;
        let restart = build_tone(arch, Flavour::Restart)?;
        if crate::zinc::built(arch)?.is_some() {
            let _ = build_media(arch, "media-pulsed", "pulsed")?;
            let _ = build_media(arch, "media-pa-tone", "pa-tone")?;
        }
        let lines = boot_and_play(arch, &plain, &wav, args)?;
        played_whole(arch, &lines, &wav)?;
        println!("  {arch}: all {FRAMES} frames written reached the device whole and in order");

        let lines = boot_and_play(arch, &negative, &wav, args)?;
        if !lines.iter().any(|line| line.contains(DONE)) {
            return Err(Error::new(format!(
                "{arch}: the negative control never finished: it proves nothing about the check"
            )));
        }
        let bytes = std::fs::read(&wav)
            .map_err(|error| Error::new(format!("{}: {error}", wav.display())))?;
        match first_wrong(&frames(&bytes)?) {
            Some(at) if at == 11 * PERIOD + 1 => println!(
                "  {arch}: the negative control's moved period was found at frame {at}, and \
                 failed the check"
            ),
            Some(at) => {
                return Err(Error::new(format!(
                    "{arch}: the negative control failed at frame {at}, not at {}",
                    11 * PERIOD + 1
                )));
            }
            None => {
                return Err(Error::new(format!(
                    "{arch}: the negative control passed the check it is built to fail"
                )));
            }
        }

        let lines = boot_and_play(arch, &restart, &wav, args)?;
        played_whole(arch, &lines, &wav)?;
        let restarts = lines.iter().filter(|line| line.contains(RESTARTED)).count();
        if restarts < 2 {
            return Err(Error::new(format!(
                "{arch}: devmgr said `{RESTARTED}` {restarts} times, not 2"
            )));
        }
        quarantined(arch, &lines)?;
        println!(
            "  {arch}: the sound driver was killed twice under a running stream, the stream \
             answered EBADFD each time, devmgr started the driver again, and the third one's \
             card played all {FRAMES} frames whole"
        );
        test_aplay(arch, &wav, args)?;
        test_pulsed(arch, &wav, args)?;
        test_mixed(arch, &wav, args)?;
    }
    Ok(())
}

/// Where the aplay boot's counter is on the guest.
const COUNTER_WAV: &str = "usr/share/ferrix/counter.wav";

/// What the aplay boot's shell runs: ferrousli's aplay, through alsa-lib's
/// `default` device and so its configuration and `plug`, playing the same
/// second tone plays. It says so in tone's words, for [`played_whole`].
/// The shell stays up after, as tone does: were init to exit, the machine
/// would power off with QEMU's audio backend still holding the last 100 ms
/// or so, which never reach the file.
const APLAY_SCRIPT: &str = "aplay -D default /usr/share/ferrix/counter.wav \
    && echo 'tone: done' || echo \"tone: failed: aplay exited $?\"; sleep 30";

/// The fourth boot, on x86-64, where the ports are built (`docs/AUDIO.md`,
/// U1): ferrousli's busybox as the shell, the alsa-lib and alsa-utils apps
/// (`cargo xtask build-apps`), and a WAV file of the counter, played
/// through `/dev/snd` and held to the same check as tone's. Skipped, saying
/// so, when those apps or ferrousli's busybox are not built here.
fn test_aplay(arch: Arch, wav: &Path, args: &Args) -> Result<()> {
    if arch != Arch::X86_64 {
        return Ok(());
    }
    let mut files = crate::apps::taken(arch, &["alsa-lib", "alsa-utils"])?;
    let Some(shell) = crate::busybox::installed_program(arch) else {
        println!("  {arch}: ferrousli's busybox is not built here, so aplay's boot is skipped");
        return Ok(());
    };
    if !files.iter().any(|file| file.path == "bin/aplay") {
        println!(
            "  {arch}: the alsa-lib and alsa-utils apps are not built here (`cargo xtask \
             build-apps --app alsa-lib --app alsa-utils`), so aplay's boot is skipped"
        );
        return Ok(());
    }
    files.push(crate::ports::File {
        path: COUNTER_WAV.to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(counter_wav()),
    });
    let lines = boot_and_record(arch, &shell, APLAY_SCRIPT, &files, 1, wav, args)?;
    played_whole(arch, &lines, wav)?;
    println!(
        "  {arch}: ferrousli's aplay played all {FRAMES} frames of the counter through \
         alsa-lib's default device, whole and in order"
    );
    Ok(())
}

/// Where the pulsed boot's server listens.
const PULSE_SOCKET: &str = "/tmp/pulse-native";

/// What the pulsed boot's shell runs: the server in the background, and the
/// client that plays tone's counter through it and says so in tone's words.
/// Then `wait`, for the server, which does not end: the shell stays up, for
/// the reason [`APLAY_SCRIPT`] gives (zinc has no `sleep`).
fn pulsed_script() -> String {
    format!("/bin/pulsed {PULSE_SOCKET} &\n/bin/pa-tone {PULSE_SOCKET}\nwait\n")
}

/// Build one of `src/user/system/linux/media`'s programs for `arch`: `package`'s `bin`.
pub(crate) fn build_media(arch: Arch, package: &str, bin: &str) -> Result<PathBuf> {
    let target = crate::display::target(arch)
        .ok_or_else(|| Error::new(format!("{arch} has no user-space target for {bin}")))?;
    let target_dir = paths::target_dir().join("media").join(bin);
    println!("  building src/user/system/linux/media/{bin} for {target}");
    let program = target_dir.join(target).join("release").join(bin);
    crate::builds::Build::cargo(
        format!("cargo build (src/user/system/linux/media/{bin}) --target {target}"),
        paths::workspace_root().join("src/user/system/linux/media"),
    )
    .args(["build", "--release", "-p", package, "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    .output(&program)
    .run()?;
    Ok(program)
}

/// A program as the file the guest runs from `/bin`.
fn program_file(program: &Path, name: &str) -> Result<crate::ports::File> {
    Ok(crate::ports::File {
        path: format!("bin/{name}"),
        mode: 0o755,
        content: crate::ports::Content::Bytes(
            std::fs::read(program)
                .map_err(|error| Error::new(format!("{}: {error}", program.display())))?,
        ),
    })
}

/// The fifth boot (`docs/AUDIO.md`, U2b): zinc starts `pulsed`, the
/// PulseAudio-protocol server, and `pa-tone`, which sends tone's counter to
/// it over the protocol; the server plays it through `/dev/snd`, clocked by
/// the card, and QEMU's file must hold the counter frame for frame. Skipped,
/// saying so, where zinc is not built.
fn test_pulsed(arch: Arch, wav: &Path, args: &Args) -> Result<()> {
    let Some(shell) = crate::zinc::built(arch)? else {
        println!("  {arch}: zinc is not built here, so pulsed's boot is skipped");
        return Ok(());
    };
    let files = [
        program_file(&build_media(arch, "media-pulsed", "pulsed")?, "pulsed")?,
        program_file(&build_media(arch, "media-pa-tone", "pa-tone")?, "pa-tone")?,
    ];
    let lines = boot_and_record(arch, &shell, &pulsed_script(), &files, 1, wav, args)?;
    played_whole(arch, &lines, wav)?;
    println!(
        "  {arch}: pa-tone sent all {FRAMES} frames of the counter to pulsed over the \
         PulseAudio protocol, and the card played them whole and in order"
    );
    Ok(())
}

/// The two sines the mixed boot plays, each a second at a quarter of full
/// scale: `(hz, rate)`. The first at the card's rate, the second at CD's,
/// so that the server converts one of them.
const SINES: [(u32, u32); 2] = [(440, 48_000), (1000, 44_100)];

/// A frequency neither sine has, at which the mix must be near silence: the
/// sum is the two tones and not their product, a distortion, or noise.
const CONTROL_HZ: u32 = 700;

/// A quarter of full scale, the amplitude each sine has.
const SINE_AMPLITUDE: f64 = 8192.0;

/// The mixed boot's shell: the server, one sine in the background, the other
/// in front, and then `wait`, for the reason [`pulsed_script`] gives.
fn mixed_script() -> String {
    let [(a, a_rate), (b, b_rate)] = SINES;
    format!(
        "/bin/pulsed {PULSE_SOCKET} &\n/bin/pa-tone {PULSE_SOCKET} sine {a} {a_rate} &\n\
         /bin/pa-tone {PULSE_SOCKET} sine {b} {b_rate}\nwait\n"
    )
}

/// The sixth boot (`docs/AUDIO.md`, U2c): two clients at once, a 440 Hz sine
/// at 48 kHz and a 1000 Hz one at 44.1 kHz, which `pulsed` mixes after
/// converting the second to the card's rate. In the middle half second of
/// what QEMU's file holds, the left channel must have each sine at a quarter
/// of full scale, within 5%, and nothing at 700 Hz. Skipped where zinc is
/// not built.
fn test_mixed(arch: Arch, wav: &Path, args: &Args) -> Result<()> {
    let Some(shell) = crate::zinc::built(arch)? else {
        println!("  {arch}: zinc is not built here, so the mixed boot is skipped");
        return Ok(());
    };
    let files = [
        program_file(&build_media(arch, "media-pulsed", "pulsed")?, "pulsed")?,
        program_file(&build_media(arch, "media-pa-tone", "pa-tone")?, "pa-tone")?,
    ];
    let lines = boot_and_record(arch, &shell, &mixed_script(), &files, 2, wav, args)?;
    if let Some(line) = lines.iter().find(|line| line.contains(FAILED)) {
        return Err(Error::new(format!("{arch}: {}", line.trim())));
    }
    let bytes =
        std::fs::read(wav).map_err(|error| Error::new(format!("{}: {error}", wav.display())))?;
    let left: Vec<f64> = frames(&bytes)?
        .iter()
        .map(|&(left, _)| f64::from(left as i16))
        .collect();
    let window = middle(&left, 24_000).ok_or_else(|| {
        Error::new(format!(
            "{arch}: QEMU's file holds {} frames, less than the half second the check reads",
            left.len()
        ))
    })?;
    let heard: Vec<(u32, f64)> = SINES
        .iter()
        .map(|&(hz, _)| (hz, amplitude(window, hz)))
        .collect();
    let control = amplitude(window, CONTROL_HZ);
    for &(hz, got) in &heard {
        if (got - SINE_AMPLITUDE).abs() > SINE_AMPLITUDE * 0.05 {
            return Err(Error::new(format!(
                "{arch}: the mix has {hz} Hz at {got:.0}, not {SINE_AMPLITUDE:.0} within 5%, \
                 and {} Hz at {:.0}",
                heard
                    .iter()
                    .find(|&&(other, _)| other != hz)
                    .map_or(0, |&(other, _)| other),
                heard
                    .iter()
                    .find(|&&(other, _)| other != hz)
                    .map_or(0.0, |&(_, a)| a),
            )));
        }
    }
    if control > SINE_AMPLITUDE * 0.02 {
        return Err(Error::new(format!(
            "{arch}: the mix has {control:.0} at {CONTROL_HZ} Hz, where neither sine is"
        )));
    }
    println!(
        "  {arch}: pulsed mixed a 440 Hz sine at 48 kHz with a 1000 Hz one it converted from \
         44.1 kHz: {:.0} and {:.0} of {SINE_AMPLITUDE:.0}, and {control:.0} at {CONTROL_HZ} Hz",
        heard.first().map_or(0.0, |&(_, a)| a),
        heard.get(1).map_or(0.0, |&(_, a)| a),
    );
    Ok(())
}

/// `length` samples from the middle of what in `samples` is not silence.
fn middle(samples: &[f64], length: usize) -> Option<&[f64]> {
    let start = samples.iter().position(|sample| sample.abs() > 64.0)?;
    let end = samples.iter().rposition(|sample| sample.abs() > 64.0)? + 1;
    if end - start < length {
        return None;
    }
    let from = start + (end - start - length) / 2;
    samples.get(from..from + length)
}

/// The amplitude of `hz` in `samples` at 48 kHz, by the Goertzel algorithm:
/// a window a whole number of cycles long leaks nothing from the other
/// tones.
fn amplitude(samples: &[f64], hz: u32) -> f64 {
    let omega = std::f64::consts::TAU * f64::from(hz) / 48_000.0;
    let coefficient = 2.0 * omega.cos();
    let (mut previous, mut before) = (0.0_f64, 0.0_f64);
    for &sample in samples {
        let now = sample + coefficient * previous - before;
        before = previous;
        previous = now;
    }
    let power = previous * previous + before * before - coefficient * previous * before;
    2.0 * power.max(0.0).sqrt() / samples.len().max(1) as f64
}

/// The second tone plays, as a WAV file: `S16_LE`, two channels, 48 kHz,
/// frame `n` holding `n` on the left and its complement on the right.
fn counter_wav() -> Vec<u8> {
    const CHANNELS: u16 = 2;
    const RATE: u32 = 48_000;
    const BYTES_PER_FRAME: u16 = CHANNELS * 2;
    let data = FRAMES * u32::from(BYTES_PER_FRAME);
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&CHANNELS.to_le_bytes());
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * u32::from(BYTES_PER_FRAME)).to_le_bytes());
    out.extend_from_slice(&BYTES_PER_FRAME.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for n in 1..=FRAMES {
        let value = n as u16;
        out.extend_from_slice(&value.to_le_bytes());
        out.extend_from_slice(&(!value).to_le_bytes());
    }
    out
}

/// What devmgr's word of a driver started again says, printed by the kernel.
const RESTARTED: &str = "was started again and published";

/// What the kernel says as a dead driver's quarantined pins go back
/// (`src/kernel/src/object/pin.rs`), before the count of pages written late.
const RELEASED: &str = "pages a dead driver's device could still write went back";

/// Require, on an architecture whose card is behind a translating IOMMU,
/// that each death's pins went to the quarantine and back, and that QEMU
/// wrote at least one of their pages after the driver had died: the writes
/// that, before the quarantine, landed in frames the allocator had given to
/// the next driver (`docs/AUDIO.md` §3.3). ARMv7-A's card is behind none, and
/// an untranslated domain keeps a dead driver's pins for good instead.
fn quarantined(arch: Arch, lines: &[String]) -> Result<()> {
    if arch == Arch::Armv7a {
        return Ok(());
    }
    let late: Vec<usize> = lines
        .iter()
        .filter_map(|line| {
            let (_, rest) = line.split_once(RELEASED)?;
            let (_, count) = rest.split_once(", ")?;
            count.split_whitespace().next()?.parse().ok()
        })
        .collect();
    if late.len() < 2 {
        return Err(Error::new(format!(
            "{arch}: the kernel said `{RELEASED}` {} times, not once for each of the 2 deaths",
            late.len()
        )));
    }
    let written: usize = late.iter().sum();
    if written == 0 {
        return Err(Error::new(format!(
            "{arch}: no quarantined page was written after its driver died, so this boot \
             no longer shows what the quarantine is for"
        )));
    }
    println!(
        "  {arch}: each dead driver's pins were quarantined until the next driver had reset \
         the card, and the device wrote {written} of their pages after its driver had died"
    );
    Ok(())
}

/// Require that tone finished and that the file holds the counter whole.
fn played_whole(arch: Arch, lines: &[String], wav: &Path) -> Result<()> {
    if let Some(line) = lines.iter().find(|line| line.contains(FAILED)) {
        return Err(Error::new(format!("{arch}: {}", line.trim())));
    }
    if !lines.iter().any(|line| line.contains(DONE)) {
        return Err(Error::new(format!("{arch}: tone never finished its drain")));
    }
    for line in lines.iter().filter(|line| line.contains("tone: ")) {
        println!("  {arch}: {}", line.trim());
    }
    let bytes =
        std::fs::read(wav).map_err(|error| Error::new(format!("{}: {error}", wav.display())))?;
    let played = frames(&bytes)?;
    if let Some(at) = first_wrong(&played) {
        return Err(Error::new(format!(
            "{arch}: the file QEMU wrote parts from the counter at frame {at} (it holds {}), \
             {} frames in all; {}",
            describe(&played, at),
            played.len(),
            wav.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FRAMES, PERIOD, first_wrong, frames, quarantined};
    use crate::paths::Arch;

    #[test]
    fn the_goertzel_reads_each_tone_of_a_mix_and_nothing_between() {
        let samples: Vec<f64> = (0..24_000)
            .map(|n| {
                let t = f64::from(n) / 48_000.0;
                8192.0 * (std::f64::consts::TAU * 440.0 * t).sin()
                    + 4096.0 * (std::f64::consts::TAU * 1000.0 * t).sin()
            })
            .collect();
        assert!((super::amplitude(&samples, 440) - 8192.0).abs() < 1.0);
        assert!((super::amplitude(&samples, 1000) - 4096.0).abs() < 1.0);
        assert!(super::amplitude(&samples, 700) < 1.0);
    }

    #[test]
    fn the_counters_wav_file_reads_back_as_the_counter() {
        let wav = super::counter_wav();
        assert_eq!(wav.len(), 44 + FRAMES as usize * 4);
        assert_eq!(first_wrong(&frames(&wav).unwrap()), None);
    }

    #[test]
    fn the_quarantine_must_have_caught_a_late_write_where_a_unit_translates() {
        let line = |written: usize| {
            format!(
                "    8.53 |   iommu    20 pages a dead driver's device could still write went \
                 back once its next driver had reset it, {written} of them written after it died"
            )
        };
        assert!(quarantined(Arch::X86_64, &[line(2), line(0)]).is_ok());
        assert!(quarantined(Arch::AArch64, &[line(0), line(0)]).is_err());
        assert!(
            quarantined(Arch::X86_64, &[line(3)]).is_err(),
            "one death of two"
        );
        assert!(
            quarantined(Arch::Armv7a, &[]).is_ok(),
            "no unit, no quarantine"
        );
    }

    fn counter(moved: bool) -> Vec<(u16, u16)> {
        (1..=FRAMES)
            .map(|n| {
                let n = if moved && (11 * PERIOD + 1..=12 * PERIOD).contains(&n) {
                    n - PERIOD
                } else {
                    n
                };
                (n as u16, !(n as u16))
            })
            .collect()
    }

    #[test]
    fn the_counter_passes_with_silence_around_it_and_fails_where_it_is_wrong() {
        let mut played = vec![(0, 0); 100];
        played.extend(counter(false));
        played.extend([(0, 0); 50]);
        assert_eq!(first_wrong(&played), None);
        assert_eq!(first_wrong(&counter(true)), Some(11 * PERIOD + 1));
        let mut short = counter(false);
        let _ = short.pop();
        assert_eq!(first_wrong(&short), Some(FRAMES));
        let mut repeated = counter(false);
        repeated.insert(500, repeated[499]);
        assert_eq!(first_wrong(&repeated), Some(501));
    }

    #[test]
    fn a_wav_files_frames_are_read_from_its_data_chunk() {
        let mut wav = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        wav.extend([0; 20]);
        wav.extend(b"data\0\0\0\0");
        wav.extend([1, 0, 0xfe, 0xff, 2, 0, 0xfd, 0xff]);
        assert_eq!(frames(&wav).unwrap(), [(1, 0xfffe), (2, 0xfffd)]);
    }
}
