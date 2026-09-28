//! Bad Apple!! in a window on the desktop.
//!
//! The same player as on the card, with the compositor between it and the
//! screen: a toolkit toplevel (`userland/compositor/toolkit`), which the
//! compositor tiles and sizes. The picture is fitted to whatever size the
//! window is given and drawn whole into each buffer, since the toolkit
//! rotates two or three of them. The sound card is still the clock; frame
//! callbacks only say when the compositor is ready for the next picture, so
//! the picture never runs ahead of what the screen can show.
//!
//! Closing the window stops the song and ends the program. When the video
//! ends, the last frame stays until the window is closed.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use compositor_toolkit::{Client, Event, SurfaceId, ToplevelOptions};
use media_bav::Video;
use media_pcm::RATE;

use crate::args::Options;
use crate::clock::Clock;
use crate::linux::{Progress, fail, finish, last_frame, say, start_song};
use crate::scale::Fit;
use crate::step::{Step, Stepper};

/// The size asked for, twice the video's: a tiling compositor gives its own.
const ASKED: (u32, u32) = (1024, 768);

/// What the window loop is waiting on.
#[derive(Debug, Default)]
struct Window {
    /// Configured size in buffer pixels, once known.
    size: Option<(usize, usize)>,
    /// A frame callback is outstanding: the compositor has not yet shown
    /// the last picture.
    awaiting: bool,
    /// The picture changed, or the window did, since the last draw.
    stale: bool,
    closed: bool,
}

pub(crate) fn run(options: &Options, video: Video<'_>) {
    let header = video.header;
    let mut client =
        Client::connect().unwrap_or_else(|error| fail(&format!("the compositor: {error}")));
    let surface = client
        .toplevel(&ToplevelOptions {
            title: "Bad Apple!!".to_owned(),
            app_id: "badapple".to_owned(),
            size: ASKED,
            ..ToplevelOptions::default()
        })
        .unwrap_or_else(|error| fail(&format!("a window: {error}")));
    let mut window = Window::default();
    // The first configure, before anything plays: its size is the fit.
    while window.size.is_none() && !window.closed {
        let events = client
            .dispatch(Some(Duration::from_secs(5)))
            .unwrap_or_else(|error| fail(&format!("the compositor: {error}")));
        handle(&client, surface, &events, &mut window);
    }
    let (width, height) = window.size.unwrap_or((ASKED.0 as usize, ASKED.1 as usize));
    let fit = Fit::new(
        usize::from(header.width),
        usize::from(header.height),
        width,
        height,
    );
    say(&format!(
        "window {width}x{height} fit {} {} {} {} video {}x{} {} frames at {}/{}",
        fit.x,
        fit.y,
        fit.width,
        fit.height,
        header.width,
        header.height,
        header.frames,
        header.rate_num,
        header.rate_den
    ));

    let clock = Arc::new(Clock::new(RATE));
    let mut song = start_song(options, &clock);
    say("ready");
    let mut stepper = Stepper::new(video, last_frame(options, &header));
    let mut fit = fit;
    let mut fitted = (width, height);
    let mut progress = Progress::new(&header);
    let mut ended = false;
    let mut announced = false;
    while !window.closed {
        // Bring the picture to the song, whether or not it can be drawn yet.
        let wait = if ended {
            None
        } else {
            match stepper.advance(clock.micros()) {
                Step::Ended => {
                    ended = true;
                    None
                }
                Step::Wait(wait) => Some(Duration::from_micros(wait.clamp(1_000, 20_000))),
                Step::Show { frame, .. } => {
                    window.stale = true;
                    progress.shown(&stepper, frame, clock.micros());
                    Some(Duration::from_millis(1))
                }
            }
        };
        if window.stale && !window.awaiting {
            if let Some(size) = window.size
                && size != fitted
            {
                fitted = size;
                fit = Fit::new(
                    usize::from(header.width),
                    usize::from(header.height),
                    size.0,
                    size.1,
                );
                say_fit(size, &fit);
            }
            draw(&mut client, surface, &fit, stepper.shades());
            window.stale = false;
            window.awaiting = true;
        }
        // The song ends with the video. Once the last frame has been drawn
        // and the compositor has shown it, wait for the song, say how both
        // went, and hold the frame until the window is closed.
        if ended && !announced && !window.stale && !window.awaiting {
            finish(&mut song, stepper.shown);
            announced = true;
        }
        let events = client
            .dispatch(wait)
            .unwrap_or_else(|error| fail(&format!("the compositor: {error}")));
        handle(&client, surface, &events, &mut window);
    }
    // Closed: stop the song where it is, and go.
    song.stop.store(true, Ordering::Relaxed);
    if !announced {
        finish(&mut song, stepper.shown);
    }
    client.destroy(surface);
    let _ = client.flush();
}

/// Say where the picture goes in the window: `xtask test-badapple` reads
/// the last such line to find it on the screen.
fn say_fit((width, height): (usize, usize), fit: &Fit) {
    say(&format!(
        "window {width}x{height} fit {} {} {} {}",
        fit.x, fit.y, fit.width, fit.height
    ));
}

/// Fill the window black and draw the picture fitted into it.
fn draw(client: &mut Client, surface: SurfaceId, fit: &Fit, shades: &[u8]) {
    let invert = cfg!(feature = "negative-control");
    let drawn = client.draw(surface, |pixmap| {
        let pitch = pixmap.width() as usize * 4;
        let pixels = pixmap.data_mut();
        for pixel in pixels.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0, 0, 0, 255]);
        }
        fit.draw(shades, pixels, pitch, 0..fit.height, invert);
    });
    if let Err(error) = drawn {
        say(&format!("drawing: {error}"));
    }
    client.request_frame(surface);
}

fn handle(client: &Client, surface: SurfaceId, events: &[Event], window: &mut Window) {
    for event in events {
        match event {
            Event::Configure {
                surface: configured,
                width,
                height,
            } if *configured == surface => {
                let scale = client.scale(surface).max(1) as usize;
                let size = (*width as usize * scale, *height as usize * scale);
                if window.size != Some(size) {
                    window.size = Some(size);
                    window.stale = true;
                }
            }
            Event::Frame {
                surface: framed, ..
            } if *framed == surface => window.awaiting = false,
            Event::CloseRequested(closed) | Event::Closed(closed) if *closed == surface => {
                window.closed = true;
            }
            _ => {}
        }
    }
}
