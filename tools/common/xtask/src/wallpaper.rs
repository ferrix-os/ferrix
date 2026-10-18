//! The picture `cargo xtask run-compositor` puts behind the desktop, and
//! `cargo xtask wallpapers`, which is how pictures get to where it looks.
//!
//! A desktop somebody is looking at has a wallpaper. Ferrix has no JPEG
//! decoder and no video decoder, and carrying either to show one picture is
//! the wrong trade; what it has is `src/user/system/linux/compositor/pattern --wallpaper`, a
//! background layer surface that shows raw `XRGB8888` rows behind a
//! twelve-byte header. So a picture is converted on a machine that can, once,
//! and kept.
//!
//! A video is kept the same way and played rather than shown:
//! `--video` on the same surface, which is the `mpvpaper` line of a Linux
//! desktop's `hyprland.conf` with the decoding moved to the other side of the
//! conversion. Its frames are AV1 in IVF, because a second of 1920x1080 raw
//! pixels is half a gigabyte and the initramfs is built into the kernel.
//!
//! Two halves, and the line between them is the network.
//!
//! * `run-compositor` reads **this machine's** wallpapers directory --
//!   `$FERRIX_WALLPAPERS`, or `~/.local/share/ferrix/wallpapers` -- and
//!   nothing else. It names no host and opens no connection, so it does the
//!   same thing on a train as at a desk; with nothing there it draws
//!   [`ember`], a picture of its own, and a line says how to change that.
//!   None of it can fail a boot.
//! * `cargo xtask wallpapers --from <where>` fills that directory, and is
//!   the only thing here that may reach another machine -- the one its
//!   argument names, which nothing in this file does. `<where>` is a
//!   directory of this machine's, converted by its `ffmpeg`, or
//!   `host:directory`, converted by that host's over `ssh`: the pictures are
//!   made where the pictures are, and what crosses the wire is the rows.
//!   `ffmpeg` scales a picture until it covers the screen and cuts the
//!   overflow evenly; a video it also takes at a frame rate and a length,
//!   and what is kept is those frames.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::args::Args;
use crate::task::Task;
use crate::{Error, Result};

mod config;

pub(crate) use config::Settings;

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "wallpapers",
    help_at: 150,
    help: &[
        "wallpapers    Convert pictures for run-compositor's desktop and keep them on this machine",
    ],
    run: |_, args| import(args),
}];

/// The variable that names the wallpapers directory, where it is not the
/// usual one.
const VAR: &str = "FERRIX_WALLPAPERS";

/// The usual one, under the home directory.
const KEPT: &[&str] = &[".local", "share", "ferrix", "wallpapers"];

/// The screen a picture is cut for when `wallpapers` is not told another.
pub(crate) const SCREEN: (u32, u32) = (1920, 1080);

/// The name `--wallpaper` gives [`ember`], Ferrix's own, whatever is kept:
/// what `release-image` shows, since a kept picture is somebody else's art.
pub(crate) const EMBER: &str = "ember";

/// What a picture's file begins with: `compositor_pattern::Picture::MAGIC`.
const MAGIC: &[u8; 8] = b"FXWALL1\n";

/// What a kept picture's name ends in.
const KIND: &str = ".fxwall";

/// What an IVF video begins with.
const MOVIE_MAGIC: &[u8; 4] = b"DKIF";

/// What a kept video's name ends in.
const MOVIE_KIND: &str = ".ivf";

/// What is kept as a wallpaper that moves rather than as its first frame.
/// The rest of [`KINDS`] is a still picture however many frames it has.
const MOVING: [&str; 4] = ["mp4", "webm", "mkv", "gif"];

// How finely and how much of a video is kept -- the frame rate, the length
// and the frame size -- is `config`'s, because it is a person's to change:
// `~/.config/ferrix/wallpaper.toml`, over the defaults that module holds.

/// What `ffmpeg` can take a frame from, among what a pictures directory
/// holds.
const KINDS: [&str; 9] = [
    "jpg", "jpeg", "png", "webp", "gif", "bmp", "mp4", "webm", "mkv",
];

/// The file `src/user/system/linux/compositor/pattern --wallpaper` is given: `--wallpaper <name>`
/// if one was asked for, and otherwise one of the kept pictures, a different
/// one from run to run. One cut for a screen of `size` where there is one,
/// since the client copies that and scales any other.
///
/// `None` only for `--wallpaper none`. With nothing kept and none asked for
/// it is [`ember`], having said why: a desktop shown to somebody for the
/// first time should not be a grey one, and every picture people had kept
/// is somebody else's art that cannot be shown in public.
///
/// A name comes from `--wallpaper`, or from `name =` in the settings file
/// where the flag is not given.
///
/// # Errors
///
/// A name that matches nothing kept, wherever it was asked for. A name given
/// is a request, and a request that cannot be met stops the run rather than
/// quietly drawing a different desktop: a boot watched for a wallpaper that
/// is not there shows a bare one, and a frame time measured on it is a wrong
/// number rather than a missing one. Four measured runs were lost to that
/// when the AV1 landing left `.fxvid` behind and the name went on matching
/// nothing. A name from the settings file is refused the same way and for
/// the same reason -- a file written once is a request that outlives being
/// typed, and one that has gone stale should say so rather than show
/// something else every boot for a month.
pub(crate) fn file(args: &Args, size: (u32, u32)) -> Result<Option<Chosen>> {
    pick(args, size, false)
}

/// [`file()`] for a board's desktop, whose screen is `screen` as the desktop
/// is laid out -- a monitor standing on its edge is taller than wide.
///
/// Two things differ from a desktop in QEMU. A wallpaper that moves is
/// chosen only by name: on the DK1's 650 MHz Cortex-A7, decoding a
/// screen's worth of AV1 thirty times a second would be most of what the
/// machine has, so a desktop that picks for itself picks a still one. And
/// a still one kept for another screen is cut for this one here, on the
/// machine that builds the image, rather than scaled by the client on the
/// board when it starts: the kept pictures are 1920x1080, a portrait
/// monitor wants 720x1280, and the card carries 3.7 MB of the right size
/// instead of 8.3 MB of the wrong one.
///
/// # Errors
///
/// As [`file()`].
pub(crate) fn for_board(args: &Args, screen: (u32, u32)) -> Result<Option<Chosen>> {
    Ok(match pick(args, screen, true)? {
        Some(Chosen::Still(bytes)) => match picture_size(&bytes) {
            Some(size) if size != screen => {
                let cut = recut(&bytes, screen);
                if cut.is_some() {
                    println!(
                        "  wallpaper cut from {}x{} for a {}x{} screen",
                        size.0, size.1, screen.0, screen.1
                    );
                }
                Some(Chosen::Still(cut.unwrap_or(bytes)))
            }
            _ => Some(Chosen::Still(bytes)),
        },
        other => other,
    })
}

/// [`file()`]'s choice, of any kept wallpaper or of the still ones only when
/// nothing names one.
fn pick(args: &Args, size: (u32, u32), stills_only: bool) -> Result<Option<Chosen>> {
    let settings = Settings::load();
    let named = config::path().map_or_else(
        || "the settings file's `name`".to_owned(),
        |path| format!("`name` in {}", path.display()),
    );
    let asked = match (args.wallpaper.as_deref(), settings.name.as_deref()) {
        (Some(name), _) => Some(Asked {
            name,
            source: "--wallpaper",
        }),
        (None, Some(name)) => Some(Asked {
            name,
            source: &named,
        }),
        (None, None) => None,
    };
    if asked.is_some_and(|asked| asked.name == "none") {
        return Ok(None);
    }
    if asked.is_some_and(|asked| asked.name == EMBER) {
        println!("  wallpaper ember, Ferrix's own, as asked");
        return Ok(Some(Chosen::Still(ember(size))));
    }
    let Some(dir) = kept_dir() else {
        return refuse_or_none(asked, size, "there is nowhere wallpapers are kept");
    };
    let mut kept: Vec<String> = std::fs::read_dir(&dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(KIND) || name.ends_with(MOVIE_KIND))
        .filter(|name| !stills_only || asked.is_some() || name.ends_with(KIND))
        .collect();
    kept.sort();
    let cut = [
        format!(".{}x{}{KIND}", size.0, size.1),
        format!(".{}x{}{MOVIE_KIND}", size.0, size.1),
    ];
    let for_this_screen = |name: &String| cut.iter().any(|end| name.ends_with(end));
    if kept.iter().any(for_this_screen) {
        kept.retain(for_this_screen);
    }
    let Some(name) = choose(&kept, asked.map(|asked| asked.name)) else {
        return refuse_or_none(
            asked,
            size,
            &format!(
                "{} holds none; `cargo xtask wallpapers --from <directory>`, or \
                 `--from <host>:<directory>`, puts some there",
                dir.display()
            ),
        );
    };
    let chosen = read_kept(&dir.join(&name));
    match &chosen {
        Some(Chosen::Still(_)) => println!("  wallpaper {name}"),
        Some(Chosen::Moving(_)) => println!("  wallpaper {name}, which moves"),
        None => {
            return refuse_or_none(
                asked,
                size,
                &format!("{name} is not a picture `wallpapers` made"),
            );
        }
    }
    Ok(chosen)
}

/// A kept picture's width and height, from its header.
fn picture_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let header = bytes.get(..MAGIC.len().checked_add(8)?)?;
    let (magic, numbers) = header.split_at(MAGIC.len());
    if magic != MAGIC {
        return None;
    }
    let number = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(
            numbers.get(at..at + 4)?.try_into().ok()?,
        ))
    };
    Some((number(0)?, number(4)?))
}

/// A kept picture cut for a `size` screen: scaled until it covers it, the
/// same amount each way, cut evenly on the two sides that overflow -- what
/// `wallpapers` asks `ffmpeg` for, and what the client does itself with a
/// picture of another size -- sampled bilinearly, since this runs once, on
/// the machine building the image, where the client's nearest pixel runs
/// on the board at every start. `None` for bytes that are not a whole
/// picture, or an empty size.
fn recut(bytes: &[u8], size: (u32, u32)) -> Option<Vec<u8>> {
    let (width, height) = picture_size(bytes)?;
    let pixels = bytes.get(MAGIC.len() + 8..)?;
    let (from_w, from_h) = (usize::try_from(width).ok()?, usize::try_from(height).ok()?);
    let (to_w, to_h) = (usize::try_from(size.0).ok()?, usize::try_from(size.1).ok()?);
    if from_w == 0 || from_h == 0 || to_w == 0 || to_h == 0 {
        return None;
    }
    if pixels.len() != from_w.checked_mul(from_h)?.checked_mul(4)? {
        return None;
    }
    // Source pixels per screen pixel, the smaller of the two ratios, so the
    // picture covers the screen both ways; the overflow is cut evenly.
    let (sw, sh, tw, th) = (from_w as f64, from_h as f64, to_w as f64, to_h as f64);
    let step = (sw / tw).min(sh / th);
    let (left, top) = ((sw - tw * step) / 2.0, (sh - th * step) / 2.0);
    let sample = |x: f64, y: f64| -> [u8; 4] {
        let at = |value: f64, most: usize| (value.max(0.0) as usize).min(most - 1);
        let (x0, y0) = (at(x.floor(), from_w), at(y.floor(), from_h));
        let (x1, y1) = ((x0 + 1).min(from_w - 1), (y0 + 1).min(from_h - 1));
        let (fx, fy) = (
            (x - x.floor()).clamp(0.0, 1.0),
            (y - y.floor()).clamp(0.0, 1.0),
        );
        let texel = |px: usize, py: usize| -> [u8; 4] {
            let start = (py * from_w + px) * 4;
            pixels
                .get(start..start + 4)
                .and_then(|texel| <[u8; 4]>::try_from(texel).ok())
                .unwrap_or([0, 0, 0, 0xFF])
        };
        let (a, b, c, d) = (texel(x0, y0), texel(x1, y0), texel(x0, y1), texel(x1, y1));
        let mut out = [0u8; 4];
        for (channel, slot) in out.iter_mut().enumerate() {
            let level = |p: [u8; 4]| f64::from(p.get(channel).copied().unwrap_or(0));
            let mix = |p: [u8; 4], q: [u8; 4], t: f64| level(p) * (1.0 - t) + level(q) * t;
            let value = mix(a, b, fx) * (1.0 - fy) + mix(c, d, fx) * fy;
            let byte = value.round().clamp(0.0, 255.0) as u8;
            *slot = byte;
        }
        out
    };
    let mut out = Vec::with_capacity(MAGIC.len() + 8 + to_w * to_h * 4);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&size.0.to_le_bytes());
    out.extend_from_slice(&size.1.to_le_bytes());
    for row in 0..to_h {
        let y = top + (row as f64 + 0.5) * step - 0.5;
        for column in 0..to_w {
            let x = left + (column as f64 + 0.5) * step - 0.5;
            out.extend_from_slice(&sample(x, y));
        }
    }
    Some(out)
}

/// What a run found to put behind the desktop.
#[derive(Debug)]
pub(crate) enum Chosen {
    /// A picture, which `src/user/system/linux/compositor/pattern --wallpaper` shows.
    Still(Vec<u8>),
    /// A video's frames, which `src/user/system/linux/compositor/pattern --video` plays, and which
    /// is what `mpvpaper` is started for on a Linux desktop.
    Moving(Vec<u8>),
}

impl Chosen {
    /// The file's bytes, whichever it is.
    pub(crate) fn bytes(self) -> Vec<u8> {
        match self {
            Self::Still(bytes) | Self::Moving(bytes) => bytes,
        }
    }
}

/// `cargo xtask wallpapers --from <where>`: convert every picture `<where>`
/// holds for a screen of `--size`, or 1920x1080, and keep them where
/// `run-compositor` looks.
///
/// # Errors
///
/// No `--from`, a source that lists no pictures, or nowhere to keep them. A
/// picture that will not convert is said and left out.
pub(crate) fn import(args: &Args) -> Result<()> {
    let source = args.from.as_deref().ok_or_else(|| {
        Error::new(
            "wallpapers needs --from <directory> or --from <host>:<directory>: where the \
             pictures are"
                .to_owned(),
        )
    })?;
    let size = args.size.unwrap_or(SCREEN);
    let settings = Settings::load();
    config::write_example();
    let dir = kept_dir().ok_or_else(|| {
        Error::new(format!(
            "no home directory to keep wallpapers under; set {VAR}"
        ))
    })?;
    let names = names(source).ok_or_else(|| {
        Error::new(format!(
            "{source} lists no pictures: it is a directory of this machine's, or \
             <host>:<directory> for a host `ssh <host>` reaches without asking anything"
        ))
    })?;
    println!(
        "  {} pictures in {source}, for a {}x{} screen, into {}",
        names.len(),
        size.0,
        size.1,
        dir.display()
    );
    let mut made = 0usize;
    for name in &names {
        let path = dir.join(kept_name(name, size));
        if read_kept(&path).is_some() {
            println!("  {name}: kept already");
            made = made.saturating_add(1);
            continue;
        }
        // A video becomes a wallpaper that moves, which is what somebody who
        // put a video in the directory asked for; everything else becomes the
        // one picture it is.
        let converted = if is_moving(name) {
            convert_moving(source, name, Moving::asked(args, &settings, size))
        } else {
            convert(source, name, size)
        };
        match converted {
            Some(bytes) => {
                keep(&path, &bytes)?;
                println!("  {name}: converted");
                made = made.saturating_add(1);
            }
            None => println!("  {name}: would not convert, and is left out"),
        }
    }
    if made == 0 {
        return Err(Error::new(format!(
            "none of them converted: is there an `ffmpeg` where {source} is?"
        )));
    }
    println!("  {made} wallpapers kept; `cargo xtask run-compositor` shows one");
    Ok(())
}

/// The source's host, if it has one, and its directory.
fn parts(source: &str) -> (Option<&str>, &str) {
    match source.split_once(':') {
        // `C:\Users\...` is a directory of this machine's, not a host
        // called `C`.
        Some((host, directory)) if host.len() > 1 => (Some(host), directory),
        _ => (None, source),
    }
}

/// `ssh` to `host`, never asking a question: a run that stops to ask for a
/// password is a run that hung.
fn ssh(host: &str) -> Command {
    let mut command = Command::new("ssh");
    let _ = command.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", host]);
    command
}

/// `word` as one word of a POSIX shell's command line, whatever is in it.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// What a command printed, if it ran and succeeded.
fn output(mut command: Command) -> Option<Vec<u8>> {
    let output = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// The names of the pictures the source holds, sorted.
fn names(source: &str) -> Option<Vec<String>> {
    let listed: Vec<String> = match parts(source) {
        (Some(host), directory) => {
            let mut command = ssh(host);
            let _ = command.arg(format!("ls -1 -- {}", quoted(directory)));
            String::from_utf8_lossy(&output(command)?)
                .lines()
                .map(str::to_owned)
                .collect()
        }
        (None, directory) => std::fs::read_dir(directory)
            .ok()?
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect(),
    };
    let mut pictures: Vec<String> = listed.into_iter().filter(|name| is_picture(name)).collect();
    pictures.sort();
    (!pictures.is_empty()).then_some(pictures)
}

/// A wallpaper asked for by name, and where it was asked for.
///
/// The source is carried rather than assumed because a refusal has to be
/// actionable: `--wallpaper shiroko` is fixed at the command line, and a
/// `name` left in the settings file months ago is fixed in that file, which
/// the person has to be told the path of.
#[derive(Clone, Copy, Debug)]
struct Asked<'a> {
    /// The part of a kept wallpaper's name to look for.
    name: &'a str,
    /// How to say where it was asked for.
    source: &'a str,
}

/// What a run with no kept wallpaper to show does: stop when one was asked
/// for by name, and show [`ember`] cut for `size`, having said `why`, when
/// none was.
fn refuse_or_none(asked: Option<Asked<'_>>, size: (u32, u32), why: &str) -> Result<Option<Chosen>> {
    match asked {
        Some(Asked { name, source }) => Err(Error::new(format!(
            "{source} {name}: no wallpaper is called anything like it, and {why}"
        ))),
        None => {
            println!("  wallpaper ember, Ferrix's own: {why}");
            Ok(Some(Chosen::Still(ember(size))))
        }
    }
}

/// Ferrix's own wallpaper, drawn here rather than kept, so that it belongs
/// to the project and costs the tree no picture: the brand's dark iron
/// (`docs/brand/BRAND.md`) under a rust-coloured glow rising from the
/// lower left, a fainter one at the upper right, and a cool navy one at
/// the lower right. A kept picture's
/// bytes -- [`MAGIC`], the width and height, then `bgr0` rows -- so the
/// client shows it as it shows any other.
///
/// A smooth gradient over a whole screen bands in eight bits a channel, and
/// the bands are what the eye sees first; a hash of the position dithers
/// each channel by under one step, which hides them and draws the same
/// picture every time.
pub(crate) fn ember(size: (u32, u32)) -> Vec<u8> {
    let (width, height) = (size.0.max(1), size.1.max(1));
    let pixels = u64::from(width) * u64::from(height);
    let mut bytes = Vec::with_capacity(MAGIC.len() + 8 + usize::try_from(pixels * 4).unwrap_or(0));
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&width.to_le_bytes());
    bytes.extend_from_slice(&height.to_le_bytes());
    // Distances are measured in the screen's shorter side, so a portrait
    // board's desktop has the same glow as a landscape one.
    let unit = f64::from(width.min(height));
    let (w, h) = (f64::from(width), f64::from(height));
    // Each colour as its red, green and blue, from 0 to 1.
    let base_top = (0.051, 0.059, 0.071); // #0d0f12
    let base_bottom = (0.071, 0.082, 0.114); // #12151d
    let rust = (1.0, 0.478, 0.169); // #ff7a2b
    let ember = (1.0, 0.710, 0.278); // #ffb547
    let navy = (0.165, 0.243, 0.478); // #2a3e7a
    for y in 0..height {
        let fy = f64::from(y);
        let down = fy / h;
        for x in 0..width {
            let fx = f64::from(x);
            let glow = |cx: f64, cy: f64, radius: f64| {
                let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() / (radius * unit);
                (1.0 - d).clamp(0.0, 1.0).powi(2)
            };
            let low = glow(0.10 * w, 1.12 * h, 1.15);
            let core = glow(0.08 * w, 1.10 * h, 0.55);
            let high = glow(0.95 * w, -0.15 * h, 0.80);
            let cool = glow(0.85 * w, 1.05 * h, 1.10);
            // One channel: the base gradient, then each glow in its colour.
            let mix = |top: f64, bottom: f64, rust: f64, ember: f64, navy: f64| {
                top + (bottom - top) * down
                    + rust * (0.55 * low + 0.10 * high)
                    + ember * 0.30 * core
                    + navy * 0.35 * cool
            };
            let red = mix(base_top.0, base_bottom.0, rust.0, ember.0, navy.0);
            let green = mix(base_top.1, base_bottom.1, rust.1, ember.1, navy.1);
            let blue = mix(base_top.2, base_bottom.2, rust.2, ember.2, navy.2);
            let noise = dither(x, y);
            let channel = |value: f64, salt: u32| -> u8 {
                let jitter = f64::from((noise >> salt) & 0xff) / 255.0 - 0.5;
                (value * 255.0 + jitter).round().clamp(0.0, 255.0) as u8
            };
            bytes.extend_from_slice(&[channel(blue, 0), channel(green, 8), channel(red, 16), 0]);
        }
    }
    bytes
}

/// A well-mixed 32-bit hash of a pixel's position, for [`ember`]'s dither.
fn dither(x: u32, y: u32) -> u32 {
    let mut v = x.wrapping_mul(0x9e37_79b1) ^ y.wrapping_mul(0x85eb_ca77);
    v ^= v >> 15;
    v = v.wrapping_mul(0x2c1b_3c6d);
    v ^= v >> 12;
    v = v.wrapping_mul(0x297a_2d39);
    v ^ (v >> 15)
}

/// Whether `name` ends in one of [`KINDS`].
fn is_picture(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|kind| kind.to_str())
        .is_some_and(|kind| KINDS.iter().any(|known| kind.eq_ignore_ascii_case(known)))
}

/// The one asked for -- the first whose name holds `asked` -- or, asked for
/// nothing, one the clock picks, so that a desktop started twice is not the
/// same desktop twice.
fn choose(names: &[String], asked: Option<&str>) -> Option<String> {
    if let Some(asked) = asked {
        // Saying nothing here: a name that matches nothing stops the run,
        // and `refuse_or_none` is where that is said.
        return names.iter().find(|name| name.contains(asked)).cloned();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let at = usize::try_from(now.checked_rem(u128::try_from(names.len()).ok()?)?).ok()?;
    names.get(at).cloned()
}

/// The rows `ffmpeg` makes of `name`, behind the picture file's header.
fn convert(source: &str, name: &str, (width, height): (u32, u32)) -> Option<Vec<u8>> {
    // Scaled until it covers the screen and cut evenly to it, the first
    // frame and no more, as `XRGB8888` rows on the standard output.
    let filter = format!(
        "scale={width}:{height}:force_original_aspect_ratio=increase,crop={width}:{height}"
    );
    let tail = ["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "bgr0", "-"];
    let command = match parts(source) {
        (Some(host), directory) => {
            let mut command = ssh(host);
            let _ = command.arg(format!(
                "ffmpeg -v error -nostdin -i {} -vf {} {}",
                quoted(&format!("{directory}/{name}")),
                quoted(&filter),
                tail.join(" ")
            ));
            command
        }
        (None, directory) => {
            let mut command = Command::new("ffmpeg");
            let _ = command
                .args(["-v", "error", "-nostdin", "-i"])
                .arg(Path::new(directory).join(name))
                .args(["-vf", &filter])
                .args(tail);
            command
        }
    };
    let rows = output(command)?;
    let expected = usize::try_from(u64::from(width) * u64::from(height) * 4).ok()?;
    if rows.len() != expected {
        println!(
            "  {name} came back as {} bytes where a {width}x{height} picture is {expected}",
            rows.len()
        );
        return None;
    }
    let mut file = Vec::with_capacity(expected.saturating_add(16));
    file.extend_from_slice(MAGIC);
    file.extend_from_slice(&width.to_le_bytes());
    file.extend_from_slice(&height.to_le_bytes());
    file.extend_from_slice(&rows);
    Some(file)
}

/// A four-frame AV1 test pattern for `cargo xtask test-video`.
///
/// The small IVF is checked in rather than encoded by the gate, so CI needs
/// neither `ffmpeg` nor an AV1 encoder. The client scales it to the screen.
pub(crate) fn fixture() -> Vec<u8> {
    include_bytes!("../../../../src/user/system/linux/compositor/pattern/tests/fixtures/tiny.ivf")
        .to_vec()
}

/// Whether `name` is kept as a wallpaper that moves.
fn is_moving(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|kind| kind.to_str())
        .is_some_and(|kind| MOVING.iter().any(|known| kind.eq_ignore_ascii_case(known)))
}

/// The AV1 IVF stream `ffmpeg` makes of `name`.
///
/// The same conversion as [`convert`] with a frame rate and length, then AV1
/// compression. IVF is deliberately simple to demux in the guest and is what
/// `rav1d` takes one temporal unit at a time.
fn convert_moving(source: &str, name: &str, how: Moving) -> Option<Vec<u8>> {
    let (width, height) = how.frame;
    let rate = how.rate;
    let filter = format!(
        "fps={rate},scale={width}:{height}:force_original_aspect_ratio=increase,\
         crop={width}:{height}"
    );
    let seconds = how.seconds.to_string();
    let tail = [
        "-t",
        seconds.as_str(),
        "-c:v",
        "libaom-av1",
        "-cpu-used",
        "8",
        "-crf",
        "35",
        "-b:v",
        "0",
        "-pix_fmt",
        "yuv420p",
        "-f",
        "ivf",
        "-",
    ];
    let command = match parts(source) {
        (Some(host), directory) => {
            let mut command = ssh(host);
            let _ = command.arg(format!(
                "ffmpeg -v error -nostdin -i {} -vf {} {}",
                quoted(&format!("{directory}/{name}")),
                quoted(&filter),
                tail.join(" ")
            ));
            command
        }
        (None, directory) => {
            let mut command = Command::new("ffmpeg");
            let _ = command
                .args(["-v", "error", "-nostdin", "-i"])
                .arg(Path::new(directory).join(name))
                .args(["-vf", &filter])
                .args(tail);
            command
        }
    };
    let file = output(command)?;
    if !file.starts_with(MOVIE_MAGIC) {
        println!(
            "  {name} came back as {} bytes, which is not an AV1 IVF video",
            file.len()
        );
        return None;
    }
    println!(
        "  {name}: AV1 {width}x{height} at {rate}/s, {} KiB",
        file.len() / 1024
    );
    Some(file)
}

/// How much of a video to keep, and how finely.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Moving {
    /// Frames a second.
    rate: u32,
    /// Seconds of it, after which the wallpaper begins again.
    seconds: u32,
    /// How large a frame is kept.
    frame: (u32, u32),
}

impl Moving {
    /// What the command line asked for, what the settings file asked for
    /// where it did not, and the defaults under both.
    fn asked(args: &Args, settings: &Settings, screen: (u32, u32)) -> Self {
        Self {
            rate: args.fps.unwrap_or(settings.fps),
            seconds: args.seconds.unwrap_or(settings.seconds),
            frame: args.video_size.unwrap_or_else(|| settings.frame(screen)),
        }
    }
}

/// The wallpapers directory.
fn kept_dir() -> Option<PathBuf> {
    match std::env::var_os(VAR) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => std::env::home_dir().map(|home| KEPT.iter().fold(home, |dir, name| dir.join(name))),
    }
}

/// What `name` at `size` is kept as: the name with anything a file system
/// might mind taken out, and the size, since a picture is cut for a screen.
fn kept_name(name: &str, (width, height): (u32, u32)) -> String {
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let kind = if is_moving(name) { MOVIE_KIND } else { KIND };
    format!("{safe}.{width}x{height}{kind}")
}

/// A kept wallpaper, if it is there and is one: which of the two it is comes
/// from the file itself rather than from its name, since the name is a
/// person's and the header is this program's.
fn read_kept(path: &Path) -> Option<Chosen> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.starts_with(MAGIC) {
        Some(Chosen::Still(bytes))
    } else if bytes.starts_with(MOVIE_MAGIC) {
        Some(Chosen::Moving(bytes))
    } else {
        None
    }
}

/// Keep `bytes` at `path`.
fn keep(path: &Path, bytes: &[u8]) -> Result<()> {
    path.parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(path, bytes))
        .map_err(|error| Error::new(format!("keeping {}: {error}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture file of `width` by `height` whose pixel at (x, y) is the
    /// bytes `[x, y, 7, 0xFF]`.
    fn picture(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        for y in 0..height {
            for x in 0..width {
                bytes.extend_from_slice(&[x as u8, y as u8, 7, 0xFF]);
            }
        }
        bytes
    }

    /// A landscape picture cut for a portrait screen: the header says the
    /// screen's size, the rows are all there, and what is kept is the
    /// middle, scaled to cover -- the columns either side are cut away.
    #[test]
    fn a_picture_is_cut_to_cover_a_screen_of_another_shape() {
        let wide = picture(64, 36);
        let cut = recut(&wide, (18, 32)).unwrap();
        assert_eq!(picture_size(&cut), Some((18, 32)));
        assert_eq!(cut.len(), MAGIC.len() + 8 + 18 * 32 * 4);
        // 36 rows cover 32: a step of 1.125 source pixels a screen pixel,
        // and 18 columns of that are 20.25 source columns out of 64, from
        // column 21.875 -- the middle.
        let first = &cut[MAGIC.len() + 8..MAGIC.len() + 12];
        assert!(
            (21..=23).contains(&first[0]),
            "the left edge is column {}",
            first[0]
        );
        assert_eq!(first[2], 7);
        assert_eq!(first[3], 0xFF);
        let last_row = MAGIC.len() + 8 + (31 * 18) * 4;
        assert!(
            cut[last_row + 1] >= 34,
            "the bottom row is row {}",
            cut[last_row + 1]
        );
    }

    /// A picture already the screen's size is the screen's picture, and
    /// bytes that are not one are refused rather than cut into noise.
    #[test]
    fn a_picture_cut_to_its_own_size_is_itself_and_a_broken_one_is_not_cut() {
        let same = picture(8, 4);
        assert_eq!(recut(&same, (8, 4)).as_deref(), Some(same.as_slice()));
        assert_eq!(recut(&same[..same.len() - 1], (4, 8)), None);
        assert_eq!(recut(b"not a picture at all", (4, 8)), None);
        assert_eq!(recut(&same, (0, 8)), None);
    }

    /// A host and a directory, a directory alone, and a Windows path, which
    /// has a colon and no host.
    #[test]
    fn a_source_is_a_host_and_a_directory_or_a_directory() {
        assert_eq!(parts("box:Pictures"), (Some("box"), "Pictures"));
        assert_eq!(parts("/home/u/Pictures"), (None, "/home/u/Pictures"));
        assert_eq!(
            parts("C:\\Users\\u\\Pictures"),
            (None, "C:\\Users\\u\\Pictures")
        );
    }

    /// A name reaches the far shell as one word whatever is in it.
    #[test]
    fn a_name_is_one_word_to_the_far_shell() {
        assert_eq!(quoted("a b.jpg"), "'a b.jpg'");
        assert_eq!(quoted("it's $(rm).png"), "'it'\\''s $(rm).png'");
    }

    /// Pictures and videos are, a directory and a script are not, and the
    /// one asked for is the first that holds the word.
    #[test]
    fn a_picture_is_chosen_by_its_kind_and_its_name() {
        assert!(is_picture("shiroko.1920x1080.MP4") && is_picture("a.jpg"));
        assert!(!is_picture("Screenshots") && !is_picture("pan.lua"));
        let names = ["kivotos.mp4".to_owned(), "shiroko-beach.mp4".to_owned()];
        assert_eq!(
            choose(&names, Some("beach")).as_deref(),
            Some("shiroko-beach.mp4")
        );
        assert_eq!(choose(&names, Some("hoshino")), None);
        assert!(choose(&names, None).is_some_and(|name| names.contains(&name)));
        assert_eq!(choose(&[], None), None);
    }

    /// A name that was asked for and cannot be met stops the run; asked for
    /// nothing, the same emptiness shows Ferrix's own picture.
    ///
    /// The difference is the whole of the rule: a boot told to show a
    /// particular wallpaper and showing none is a different scene, and
    /// anything measured on it is a wrong number rather than a missing one.
    #[test]
    fn a_wallpaper_asked_for_by_name_must_be_there() {
        let flag = Asked {
            name: "amiya",
            source: "--wallpaper",
        };
        let refused = refuse_or_none(Some(flag), (64, 36), "nothing is kept");
        let message = refused
            .expect_err("a name that matches nothing stops")
            .to_string();
        assert!(
            message.contains("amiya"),
            "it says what was asked for: {message}"
        );
        assert!(message.contains("nothing is kept"), "and why: {message}");
        assert!(
            message.contains("--wallpaper"),
            "and where it was asked for: {message}"
        );

        // The same name out of the settings file is refused too, and says
        // which file to go and change rather than naming a flag nobody typed.
        let written = Asked {
            name: "amiya",
            source: "`name` in /home/u/.config/ferrix/wallpaper.toml",
        };
        let message = refuse_or_none(Some(written), (64, 36), "nothing is kept")
            .expect_err("a stale name in the file stops the same way")
            .to_string();
        assert!(
            message.contains("wallpaper.toml") && !message.contains("--wallpaper"),
            "it sends the person to the file: {message}"
        );

        let quiet = refuse_or_none(None, (64, 36), "nothing is kept");
        assert!(
            matches!(&quiet, Ok(Some(Chosen::Still(bytes))) if picture_size(bytes) == Some((64, 36))),
            "asked for nothing, an empty directory shows ember at the screen's size"
        );
    }

    /// Ember is a whole picture the client accepts, the same every time,
    /// dark where the desktop's text sits and warm where the glow is -- and
    /// with no run of equal pixels long enough to show as a band.
    #[test]
    fn ember_is_a_whole_picture_and_does_not_band() {
        let (width, height) = (320u32, 180u32);
        let bytes = ember((width, height));
        assert_eq!(bytes.len(), MAGIC.len() + 8 + 320 * 180 * 4);
        assert_eq!(picture_size(&bytes), Some((width, height)));
        assert_eq!(bytes, ember((width, height)), "drawn the same every time");

        let pixel = |x: u32, y: u32| -> [u8; 3] {
            let at = MAGIC.len() + 8 + ((y * width + x) * 4) as usize;
            [bytes[at + 2], bytes[at + 1], bytes[at]]
        };
        let [r, g, b] = pixel(width / 2, height / 3);
        assert!(
            r < 40 && g < 40 && b < 50,
            "the middle is dark: {r} {g} {b}"
        );
        let [r, g, b] = pixel(8, height - 4);
        assert!(
            r > g && g > b && r > 60,
            "the lower left glows rust: {r} {g} {b}"
        );

        // Along a row where the gradient is slow, the dither must break it
        // up: no more than a few neighbours in a row may be identical.
        let row = height / 2;
        let mut longest = 0;
        let mut run = 0;
        for x in 1..width {
            run = if pixel(x, row) == pixel(x - 1, row) {
                run + 1
            } else {
                0
            };
            longest = longest.max(run);
        }
        assert!(longest < 12, "a band of {longest} equal pixels");
    }

    /// A kept picture is named for what it is of and the screen it was cut
    /// for, in characters any file system takes. A video is named the same
    /// way and ends differently, because it is played rather than shown.
    #[test]
    fn a_kept_picture_is_named_for_its_screen() {
        assert_eq!(
            kept_name("__shiroko (blue archive).jpg", (1920, 1080)),
            "__shiroko__blue_archive_.jpg.1920x1080.fxwall"
        );
        assert_eq!(
            kept_name("kivotos.mp4", (1920, 1080)),
            "kivotos.mp4.1920x1080.ivf"
        );
        assert!(is_moving("a.MP4") && is_moving("b.webm") && is_moving("c.gif"));
        assert!(!is_moving("a.jpg") && !is_moving("Screenshots"));
    }

    /// The QEMU gate's fixture is a compact, self-contained AV1 IVF stream.
    #[test]
    fn the_video_fixture_is_ivf() {
        let file = fixture();
        assert!(file.starts_with(MOVIE_MAGIC));
        assert!(file.len() > 32);
    }
}
