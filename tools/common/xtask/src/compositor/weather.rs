//! `test-compositor --boot weather`: the sidebar's weather, strictly
//! opt-in (`docs/DESKTOP.md` §6.6). Off, the shell asks nothing of anyone;
//! a place typed and Return turns it on, and the shell asks Open-Meteo's
//! geocoding for the place and then its forecast, and shows it; "Turn off
//! weather" stops it.
//!
//! The guest has no network here: `YORU_WEATHER_FAKE` names a folder whose
//! two answers stand in for Open-Meteo's two hosts, and the shell says each
//! host it asks. The left sidebar opens on the weather (the arrangement's
//! file says so). Required: no host asked before the place was typed; the
//! geocoding host, then the forecast's, after; the forecast read; and no
//! host asked after "Turn off". Kept as `weather.ppm`.

use std::time::{Duration, Instant};

use super::boot::{absolute, build_image, press, with_the_transcript};
use super::{MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The screen, as the Yoru boot sets it.
const SCREEN: (i64, i64) = (1024, 768);

/// Open-Meteo's geocoding answer for the place typed, made up.
const GEOCODING: &str = r#"{"results":[{"name":"Hamburg","latitude":53.55073,"longitude":9.99302,"country":"Germany"}]}"#;

/// Its forecast answer, made up.
const FORECAST: &str = r#"{"current":{"temperature_2m":11.4,"apparent_temperature":9.2,"weather_code":2,"is_day":0},
"hourly":{"time":["2026-10-25T22:00","2026-10-25T23:00"],"temperature_2m":[11,10.6],"weather_code":[2,2]},
"daily":{"time":["2026-10-25","2026-10-26"],"weather_code":[2,61],"temperature_2m_max":[14,12],"temperature_2m_min":[8,7]}}"#;

/// The pointer to `(x, y)`.
fn at(qmp: &mut Qmp, (x, y): (i64, i64)) -> Result<()> {
    let ax = i32::try_from(x.clamp(0, SCREEN.0 - 1) * 0x7fff / (SCREEN.0 - 1)).unwrap_or(0);
    let ay = i32::try_from(y.clamp(0, SCREEN.1 - 1) * 0x7fff / (SCREEN.1 - 1)).unwrap_or(0);
    qmp.input_send_event(&[absolute("x", ax), absolute("y", ay)])
}

/// A left press at `(x, y)`, the pointer there a moment first.
fn click(qmp: &mut Qmp, point: (i64, i64)) -> Result<()> {
    at(qmp, point)?;
    std::thread::sleep(Duration::from_millis(800));
    let button = |down: bool| {
        format!("{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":\"left\"}}}}")
    };
    qmp.input_send_event(&[button(true)])?;
    std::thread::sleep(Duration::from_millis(120));
    qmp.input_send_event(&[button(false)])
}

/// `test-compositor --boot weather`.
///
/// # Errors
///
/// A host asked while the weather was off, or no forecast read once on.
#[expect(
    clippy::too_many_lines,
    reason = "one boot read top to bottom: each step and what it must have done"
)]
pub(super) fn test_weather(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (mut config, mut carried) = super::yoru::desktop(arch, args, false)?;
    config.push_str("env = YORU_WEATHER_FAKE,/etc/weather-fake\n");
    for (path, text) in [
        ("etc/weather-fake/geocoding.json", GEOCODING),
        ("etc/weather-fake/forecast.json", FORECAST),
        // The left sidebar opens on the weather.
        (
            ".config/yoru-shell/sidebars",
            "left clock weather\nshown left weather\nright settings todo\nshown right todo\n",
        ),
    ] {
        carried.ports.push(crate::ports::File {
            path: path.to_owned(),
            mode: 0o644,
            content: crate::ports::Content::Bytes(text.as_bytes().to_vec()),
        });
    }
    let (image, kernel) = build_image(arch, programs, &config, carried, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let up = watching.read_more(Instant::now() + SETTLE, |lines| {
            lines.iter().any(|l| l.contains("the left sidebar is up"))
        })?;
        if !up {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: weather: the left sidebar never came")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(5));
        let asked = |watching: &Watching<'_>, from: usize| {
            watching
                .lines()
                .iter()
                .chain(watching.after())
                .skip(from)
                .filter(|l| l.contains("weather: asking"))
                .count()
        };
        let mut failures = Vec::new();
        if asked(watching, 0) > 0 {
            failures.push("a host was asked before the weather was turned on".to_owned());
        }
        // Open the left sidebar, on its weather; type a place.
        at(&mut qmp, (300, 300))?;
        std::thread::sleep(Duration::from_millis(500));
        let from = watching.after().len();
        at(&mut qmp, (0, 300))?;
        if !watching.read_more(Instant::now() + Duration::from_secs(30), |lines| {
            lines
                .iter()
                .skip(from)
                .any(|l| l.contains("the left sidebar opened"))
        })? {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: weather: the left edge opened nothing")),
                watching,
            ));
        }
        std::thread::sleep(Duration::from_secs(3));
        click(&mut qmp, (8 + 16 + 100, 8 + 90 + 92 + 17))?;
        std::thread::sleep(Duration::from_secs(1));
        for key in ["h", "a", "m", "b", "u", "r", "g"] {
            press(&mut qmp, &[key])?;
            std::thread::sleep(Duration::from_millis(150));
        }
        let before = watching.lines().len() + watching.after().len();
        press(&mut qmp, &["ret"])?;
        let read = watching.read_more(Instant::now() + Duration::from_secs(60), |lines| {
            lines
                .iter()
                .any(|l| l.contains("weather: Hamburg, Germany read"))
        })?;
        let lines: Vec<String> = watching
            .lines()
            .iter()
            .chain(watching.after())
            .skip(before)
            .filter(|l| l.contains("weather: asking"))
            .cloned()
            .collect();
        let geocoding = lines
            .iter()
            .position(|l| l.contains("asking geocoding-api.open-meteo.com"));
        let forecast = lines
            .iter()
            .position(|l| l.contains("asking api.open-meteo.com"));
        if !read {
            failures.push("no forecast was read once the weather was on".to_owned());
        }
        if !matches!((geocoding, forecast), (Some(g), Some(f)) if g < f) {
            failures.push(format!("the hosts were not asked in turn: {lines:?}"));
        }
        std::thread::sleep(Duration::from_secs(3));
        let dump = paths::build_dir(arch).join("weather.ppm");
        qmp.screendump(Some(DEVICE_ID), &dump)?;
        println!(
            "  {arch}: weather: the forecast is shown (the screen is {})",
            dump.display()
        );
        // Off: nothing asked from now on.
        let from = watching.lines().len() + watching.after().len();
        click(&mut qmp, (8 + 16 + 60, 8 + 90 + 358 + 15))?;
        std::thread::sleep(Duration::from_secs(10));
        let _ = watching.read_more(Instant::now() + Duration::from_secs(5), |_| false)?;
        if !watching.after().iter().any(|l| l.contains("WeatherOff")) {
            failures.push("\"Turn off weather\" took no press".to_owned());
        }
        if asked(watching, from) > 0 {
            failures.push("a host was asked after the weather was turned off".to_owned());
        }
        if failures.is_empty() {
            println!(
                "  {arch}: weather: nothing asked while off, the place and its forecast asked once on, nothing after off"
            );
            Ok(())
        } else {
            Err(with_the_transcript(
                &Error::new(format!("{arch}: weather: {}", failures.join("; "))),
                watching,
            ))
        }
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, MARKER, hook)?;
    Ok(())
}
