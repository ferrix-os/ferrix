//! `wallpaper FILE`: FILE (PNG, JPEG or SVG) behind everything on every
//! screen, scaled to cover it, on a background layer surface per screen;
//! a screen plugged in later gets one, one unplugged loses its own.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::Path;

use compositor_image::Fit;
use compositor_toolkit::{Anchor, Client, Event, Layer, LayerOptions, OutputId, SurfaceId};

fn main() {
    if let Err(error) = run() {
        say(&format!("wallpaper: {error}"));
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let file = std::env::args()
        .nth(1)
        .ok_or("wallpaper FILE: the picture to show")?;
    // An SVG is drawn at 4K, the largest a screen here is likely to be.
    let picture = compositor_image::load(Path::new(&file), Fit::Within(3840, 2160))
        .map_err(|error| format!("{file}: {error}"))?;
    let mut client = Client::connect().map_err(|error| error.to_string())?;
    let mut surfaces: HashMap<OutputId, SurfaceId> = HashMap::new();
    let ids: Vec<OutputId> = client
        .outputs()
        .iter()
        .filter_map(|output| output.id)
        .collect();
    for id in ids {
        add(&mut client, &mut surfaces, id);
    }
    loop {
        for event in client.dispatch(None).map_err(|error| error.to_string())? {
            match event {
                Event::OutputAdded(id) => add(&mut client, &mut surfaces, id),
                Event::OutputRemoved(id) => {
                    if let Some(surface) = surfaces.remove(&id) {
                        client.destroy(surface);
                    }
                }
                Event::Configure { surface, .. } | Event::Scale { surface, .. } => {
                    let drawn = client
                        .draw(surface, |target| {
                            compositor_wallpaper::draw(picture.as_ref(), target);
                        })
                        .map_err(|error| error.to_string())?;
                    if drawn && let Some((width, height)) = client.size(surface) {
                        say(&format!("wallpaper: {file} drawn at {width}x{height}"));
                    }
                }
                Event::Closed(surface) => {
                    client.destroy(surface);
                    surfaces.retain(|_, ours| *ours != surface);
                }
                _ => {}
            }
        }
    }
}

/// A background surface on screen `id`, unless it has one.
fn add(client: &mut Client, surfaces: &mut HashMap<OutputId, SurfaceId>, id: OutputId) {
    if surfaces.contains_key(&id) {
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
            let _ = surfaces.insert(id, surface);
        }
        Err(error) => say(&format!("wallpaper: no surface on a screen: {error}")),
    }
}

/// A line on the standard output, flushed.
fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}
