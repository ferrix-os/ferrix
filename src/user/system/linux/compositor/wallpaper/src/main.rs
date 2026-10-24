//! `wallpaper FILE`, or `wallpaper --line LINE…`: what is behind everything
//! on every screen, on a background layer surface per screen; a screen
//! plugged in later gets one, one unplugged loses its own.
//!
//! Each `--line` is the value of a `wallpaper =` line
//! (`compositor/config/src/wallpaper.rs`): which output, and a picture, a
//! colour, a slideshow or a video, fitted as it says. hyprix starts this
//! program with the lines its configuration and the Settings app's file
//! hold (`docs/RUNTIME-SETTINGS.md` §3.3), and starts it again when they
//! change. `wallpaper FILE` is `--line ", picture FILE"`.
//!
//! A video is played by `pattern --video`, beside this program, which plays
//! AV1 in IVF (what `cargo xtask wallpapers` keeps) on one screen; any other
//! file shows the line's colour and says why.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use compositor_config::{Wallpaper, WallpaperFit, WallpaperSource};
use compositor_image::Fit;
use compositor_image::tiny_skia::Pixmap;
use compositor_toolkit::{Anchor, Client, Event, Layer, LayerOptions, OutputId, SurfaceId};
use compositor_wallpaper::Span;

/// How long a slideshow waits before it looks again while it is paused.
const PAUSED_LOOK: Duration = Duration::from_secs(10);

fn main() {
    if let Err(error) = run() {
        say(&format!("wallpaper: {error}"));
        std::process::exit(1);
    }
}

/// The lines the arguments give.
fn lines() -> Result<Vec<Wallpaper>, String> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.is_empty() || arguments.iter().any(|word| word == "--help") {
        return Err("wallpaper FILE, or wallpaper --line LINE [--line LINE]…".to_owned());
    }
    let values: Vec<String> = if arguments.first().is_some_and(|word| word == "--line") {
        let mut values = Vec::new();
        let mut words = arguments.into_iter();
        while let Some(word) = words.next() {
            if word != "--line" {
                return Err(format!("{word}: every argument is --line LINE"));
            }
            values.push(words.next().ok_or("--line needs a line")?);
        }
        values
    } else {
        vec![format!(", picture {}", arguments.join(" "))]
    };
    let mut kept: Vec<Wallpaper> = Vec::new();
    for value in values {
        let (output, line) = Wallpaper::parse(&value)?;
        kept.retain(|known| known.output != output);
        kept.extend(line);
    }
    if kept.is_empty() {
        return Err("no wallpaper line to show".to_owned());
    }
    Ok(kept)
}

/// One line, being shown.
struct Shown {
    line: Wallpaper,
    /// The picture now, if the line has one and it could be read.
    picture: Option<Pixmap>,
    /// A slideshow's pictures, in the order they are shown.
    slides: Vec<PathBuf>,
    /// Which of them is shown.
    at: usize,
    /// When a slideshow shows its next picture.
    next: Option<Instant>,
    /// `pattern --video`, playing this line's video.
    video: Option<std::process::Child>,
}

impl Shown {
    fn new(line: Wallpaper) -> Self {
        let mut shown = Self {
            line,
            picture: None,
            slides: Vec::new(),
            at: 0,
            next: None,
            video: None,
        };
        match shown.line.source.clone() {
            WallpaperSource::Colour => {}
            WallpaperSource::Picture(path) => {
                shown.picture = load(&home(&path), shown.line.fit);
            }
            WallpaperSource::Slideshow {
                folder,
                every,
                shuffle,
            } => {
                let folder = home(&folder);
                let names = std::fs::read_dir(&folder)
                    .map(|entries| {
                        entries
                            .filter_map(Result::ok)
                            .map(|entry| entry.file_name().to_string_lossy().into_owned())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                shown.slides = compositor_wallpaper::pictures_in(names)
                    .into_iter()
                    .map(|name| folder.join(name))
                    .collect();
                if shuffle {
                    compositor_wallpaper::shuffle(&mut shown.slides, seed());
                }
                say(&format!(
                    "wallpaper: slideshow of {} pictures in {}, every {every} s{}",
                    shown.slides.len(),
                    folder.display(),
                    if shuffle { ", shuffled" } else { "" }
                ));
                shown.show_slide();
                shown.next = Some(Instant::now() + Duration::from_secs(u64::from(every)));
            }
            WallpaperSource::Video(path) => shown.video = play(&home(&path), &shown.line.output),
        }
        shown
    }

    /// Load the slide `at` points to, skipping any that cannot be read.
    fn show_slide(&mut self) {
        for _ in 0..self.slides.len() {
            let Some(path) = self.slides.get(self.at) else {
                return;
            };
            if let Some(picture) = load(path, self.line.fit) {
                say(&format!("wallpaper: showing {}", path.display()));
                self.picture = Some(picture);
                return;
            }
            self.at = (self.at + 1) % self.slides.len();
        }
    }

    /// The next slide, reshuffled once the order has been round.
    fn advance(&mut self) {
        if self.slides.is_empty() {
            return;
        }
        self.at += 1;
        if self.at >= self.slides.len() {
            self.at = 0;
            if let WallpaperSource::Slideshow { shuffle: true, .. } = self.line.source {
                compositor_wallpaper::shuffle(&mut self.slides, seed());
            }
        }
        self.show_slide();
    }

    /// Seconds between slides.
    fn every(&self) -> Option<Duration> {
        match self.line.source {
            WallpaperSource::Slideshow { every, .. } => Some(Duration::from_secs(u64::from(every))),
            _ => None,
        }
    }
}

impl Drop for Shown {
    fn drop(&mut self) {
        if let Some(mut video) = self.video.take() {
            let _ = video.kill();
            let _ = video.wait();
        }
    }
}

fn run() -> Result<(), String> {
    let lines = lines()?;
    let mut shown: Vec<Shown> = lines.into_iter().map(Shown::new).collect();
    let mut client = Client::connect().map_err(|error| error.to_string())?;
    let mut surfaces: HashMap<OutputId, (SurfaceId, usize)> = HashMap::new();
    let ids: Vec<OutputId> = client
        .outputs()
        .iter()
        .filter_map(|output| output.id)
        .collect();
    for id in ids {
        add(&mut client, &mut surfaces, &shown, id);
    }
    loop {
        let now = Instant::now();
        let wake = shown.iter().filter_map(|line| line.next).min();
        let timeout = wake.map(|when| when.saturating_duration_since(now));
        for event in client
            .dispatch(timeout)
            .map_err(|error| error.to_string())?
        {
            match event {
                Event::OutputAdded(id) => add(&mut client, &mut surfaces, &shown, id),
                Event::OutputRemoved(id) => {
                    if let Some((surface, _)) = surfaces.remove(&id) {
                        client.destroy(surface);
                    }
                }
                Event::Configure { surface, .. } | Event::Scale { surface, .. } => {
                    paint(&mut client, &surfaces, &shown, surface)?;
                }
                Event::Closed(surface) => {
                    client.destroy(surface);
                    surfaces.retain(|_, (ours, _)| *ours != surface);
                }
                _ => {}
            }
        }
        // The slideshows whose time has come: the next picture, unless
        // nobody would see it change or the machine is on its battery.
        let now = Instant::now();
        for index in 0..shown.len() {
            let Some(line) = shown.get_mut(index) else {
                continue;
            };
            if !line.next.is_some_and(|when| when <= now) {
                continue;
            }
            let showing: Vec<String> = surfaces
                .iter()
                .filter(|(_, (_, at))| *at == index)
                .filter_map(|(id, _)| name_of(&client, *id))
                .collect();
            let paused = (line.line.pause_battery && on_battery())
                || (line.line.pause_covered && !showing.is_empty() && all_covered(&showing));
            if paused {
                line.next = Some(now + PAUSED_LOOK);
                continue;
            }
            line.advance();
            line.next = line.every().map(|every| now + every);
            let ours: Vec<SurfaceId> = surfaces
                .values()
                .filter(|(_, at)| *at == index)
                .map(|(surface, _)| *surface)
                .collect();
            for surface in ours {
                paint(&mut client, &surfaces, &shown, surface)?;
            }
        }
    }
}

/// Draw `surface` with the line it shows.
fn paint(
    client: &mut Client,
    surfaces: &HashMap<OutputId, (SurfaceId, usize)>,
    shown: &[Shown],
    surface: SurfaceId,
) -> Result<(), String> {
    let Some((output, index)) = surfaces
        .iter()
        .find(|(_, (ours, _))| *ours == surface)
        .map(|(output, (_, index))| (*output, *index))
    else {
        return Ok(());
    };
    let Some(line) = shown.get(index) else {
        return Ok(());
    };
    let span = line.line.span.then(|| span_of(client, output)).flatten();
    let drawn = client
        .draw(surface, |target| {
            compositor_wallpaper::draw_fitted(
                line.picture.as_ref().map(Pixmap::as_ref),
                target,
                line.line.fit,
                line.line.colour,
                span,
            );
        })
        .map_err(|error| error.to_string())?;
    if drawn && let Some((width, height)) = client.size(surface) {
        say(&format!(
            "wallpaper: {} drawn at {width}x{height} on {}",
            what(&line.line),
            name_of(client, output).unwrap_or_default()
        ));
    }
    Ok(())
}

/// What a line shows, for the log.
fn what(line: &Wallpaper) -> String {
    match &line.source {
        WallpaperSource::Colour => format!("colour {:08x}", line.colour),
        WallpaperSource::Picture(path) => path.clone(),
        WallpaperSource::Slideshow { folder, .. } => format!("slideshow {folder}"),
        WallpaperSource::Video(path) => format!("video {path}"),
    }
}

/// The line output `id` shows: its own, or the line for every screen.
fn line_for(shown: &[Shown], name: &str) -> Option<usize> {
    shown
        .iter()
        .position(|line| line.line.output == name)
        .or_else(|| shown.iter().position(|line| line.line.output.is_empty()))
}

/// The output's connector name.
fn name_of(client: &Client, id: OutputId) -> Option<String> {
    client
        .outputs()
        .iter()
        .find(|output| output.id == Some(id))
        .map(|output| output.name.clone())
}

/// A screen's left, top, width and height, in the compositor's units.
type Placed = (i32, i32, u32, u32);

/// Where output `id` falls in the box every screen makes.
fn span_of(client: &Client, id: OutputId) -> Option<Span> {
    let boxes: Vec<(Option<OutputId>, Placed)> = client
        .outputs()
        .iter()
        .map(|output| {
            let scale = output.scale.max(1);
            let width = u32::try_from(output.mode.0 / scale).unwrap_or(0);
            let height = u32::try_from(output.mode.1 / scale).unwrap_or(0);
            (
                output.id,
                (output.position.0, output.position.1, width, height),
            )
        })
        .collect();
    let screen = boxes.iter().find(|(ours, _)| *ours == Some(id))?.1;
    let left = boxes.iter().map(|(_, b)| b.0).min()?;
    let top = boxes.iter().map(|(_, b)| b.1).min()?;
    let right = boxes
        .iter()
        .map(|(_, b)| i64::from(b.0) + i64::from(b.2))
        .max()?;
    let bottom = boxes
        .iter()
        .map(|(_, b)| i64::from(b.1) + i64::from(b.3))
        .max()?;
    Some(Span {
        whole: (
            left,
            top,
            u32::try_from(right - i64::from(left)).ok()?,
            u32::try_from(bottom - i64::from(top)).ok()?,
        ),
        screen,
    })
}

/// A background surface on screen `id` for the line it shows, unless it
/// has one, or none at all for a screen with no line or a video's.
fn add(
    client: &mut Client,
    surfaces: &mut HashMap<OutputId, (SurfaceId, usize)>,
    shown: &[Shown],
    id: OutputId,
) {
    if surfaces.contains_key(&id) {
        return;
    }
    let name = name_of(client, id).unwrap_or_default();
    let Some(index) = line_for(shown, &name) else {
        return;
    };
    if shown.get(index).is_some_and(|line| line.video.is_some()) {
        return;
    }
    let options = LayerOptions {
        output: Some(id),
        layer: Layer::Background,
        namespace: "wallpaper".to_owned(),
        size: (0, 0),
        anchor: Anchor::TOP
            .with(Anchor::BOTTOM)
            .with(Anchor::LEFT)
            .with(Anchor::RIGHT),
        exclusive_zone: -1,
        ..LayerOptions::default()
    };
    match client.layer_surface(&options) {
        Ok(surface) => {
            let _ = surfaces.insert(id, (surface, index));
        }
        Err(error) => say(&format!("wallpaper: no surface on {name}: {error}")),
    }
}

/// A picture, read; `None`, said, when it cannot be. Tiled and centred
/// pictures keep their own size; the rest are read at most 4K, the
/// largest a screen here is likely to be, which is what an SVG is drawn at.
fn load(path: &Path, fit: WallpaperFit) -> Option<Pixmap> {
    let size = match fit {
        WallpaperFit::Tile | WallpaperFit::Centre => Fit::Natural,
        _ => Fit::Within(3840, 2160),
    };
    match compositor_image::load(path, size) {
        Ok(picture) => Some(picture),
        Err(error) => {
            say(&format!("wallpaper: {}: {error}", path.display()));
            None
        }
    }
}

/// `pattern --video` playing `path`, beside this program, when it is AV1
/// in IVF; `None`, said, otherwise.
fn play(path: &Path, output: &str) -> Option<std::process::Child> {
    let ivf = std::fs::File::open(path).ok().and_then(|mut file| {
        let mut magic = [0_u8; 4];
        std::io::Read::read_exact(&mut file, &mut magic).ok()?;
        Some(&magic == b"DKIF")
    });
    if ivf != Some(true) {
        say(&format!(
            "wallpaper: {}: video wallpapers play AV1 in IVF only (`cargo xtask wallpapers` \
             makes one); showing the colour instead",
            path.display()
        ));
        return None;
    }
    let pattern = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("pattern")))
        .unwrap_or_else(|| PathBuf::from("pattern"));
    let mut command = std::process::Command::new(&pattern);
    let _ = command.arg("--video");
    if !output.is_empty() {
        let _ = command.arg(output);
    }
    match command.arg(path).spawn() {
        Ok(child) => {
            say(&format!(
                "wallpaper: video {} played by {} (pause_covered and pause_battery are not \
                 honoured for a video yet)",
                path.display(),
                pattern.display()
            ));
            Some(child)
        }
        Err(error) => {
            say(&format!("wallpaper: {}: {error}", pattern.display()));
            None
        }
    }
}

/// `~/` at the start of `path` as the home directory.
fn home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// A seed for a shuffle that differs from one run to the next.
fn seed() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    (now as u64) ^ (u64::from(std::process::id()) << 32) | 1
}

/// Whether every one of `outputs` shows a fullscreen window, asked of the
/// compositor; `false` when it cannot be asked.
fn all_covered(outputs: &[String]) -> bool {
    let Ok(socket) = compositor_ctl::socket() else {
        return false;
    };
    let (Ok(monitors), Ok(workspaces)) = (
        compositor_ctl::ask(&socket, "monitors"),
        compositor_ctl::ask(&socket, "workspaces"),
    ) else {
        return false;
    };
    outputs
        .iter()
        .all(|output| compositor_wallpaper::covered(&monitors, &workspaces, output))
}

/// Whether the machine runs on a battery now: a power supply of type
/// `Battery` that says `Discharging`.
fn on_battery() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/class/power_supply") else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let read = |name: &str| {
            std::fs::read_to_string(entry.path().join(name))
                .map(|text| text.trim().to_owned())
                .unwrap_or_default()
        };
        read("type") == "Battery" && read("status") == "Discharging"
    })
}

/// A line on the standard output, flushed.
fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}
