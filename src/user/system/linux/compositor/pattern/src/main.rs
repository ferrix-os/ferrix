//! The pattern client's command line.
//!
//! Everything it does is in the library beside this, so the compositor's own
//! test can run a client in a thread rather than as a process.

use std::io::Write;

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    // `--wallpaper <file>`: the picture in `file` behind everything, which
    // is what `cargo xtask run-compositor` starts this as.
    if let Some(at) = arguments.iter().position(|word| word == "--wallpaper") {
        wallpaper(arguments.get(at + 1).map(String::as_str));
    }
    // `--video [-o <options>] [<output>] <file>`: the same, moving.
    // `mpvpaper [-o "..."] ALL <file>` is the line this stands in for on a
    // Linux desktop, and it is read the same way round; the file is frames
    // rather than a video because the decoding happened on a machine that
    // had a decoder.
    if let Some(at) = arguments.iter().position(|word| word == "--video") {
        video(arguments.get(at + 1..).unwrap_or(&[]));
    }
    let pattern = match arguments.first().map(String::as_str) {
        Some("checkerboard") | None => compositor_render::Pattern::Checkerboard,
        Some("gradient") => compositor_render::Pattern::Gradient,
        Some(other) => {
            say(&format!(
                "pattern: {other} is not a pattern; try checkerboard or gradient"
            ));
            std::process::exit(2);
        }
    };
    let title = arguments
        .get(1)
        .cloned()
        .unwrap_or_else(|| format!("{pattern:?}"));
    // A window's placed once the compositor has drawn it; `--after <title>`
    // waits for the window of that title to be before this one connects.
    // Two windows started together otherwise race to be placed, and a
    // tiling layout puts whichever wins first: a test that expects `one` on
    // the left and `two` on the right says `two --after one`.
    compositor_pattern::announce_when_drawn(drawn_path(&title));
    if let Some(at) = arguments.iter().position(|word| word == "--after") {
        let Some(other) = arguments.get(at + 1) else {
            say("pattern: --after takes the title of the window to wait for");
            std::process::exit(2);
        };
        wait_until_drawn(other);
    }
    let sized = |flag: &str| {
        arguments
            .iter()
            .position(|word| word == flag)
            .map(|at| arguments.get(at + 1).and_then(|word| word.parse().ok()))
    };
    // `--bar <height>`: a `zwlr_layer_surface_v1` across the top rather than
    // a window. `--menu <side>`: a window with an `xdg_popup` on it, which
    // is what every right-click menu and dropdown is.
    // `--twin`: two windows on this one connection, the second of which is
    // destroyed once both have been drawn. What it proves is in
    // `compositor_pattern::Shape::Twin`.
    if arguments.iter().any(|word| word == "--twin") {
        match compositor_pattern::run_shaped(pattern, &title, compositor_pattern::Shape::Twin) {
            Ok(line) => {
                say(&line);
                std::process::exit(0)
            }
            Err(error) => {
                say(&format!("pattern: failed: {error}"));
                std::process::exit(1)
            }
        }
    }
    let shape = match (sized("--bar"), sized("--menu")) {
        // Both: a bar with a menu hanging off it, as a bar's tooltip does.
        (Some(Some(height)), Some(Some(side))) => compositor_pattern::Shape::BarMenu(height, side),
        (Some(Some(height)), _) => compositor_pattern::Shape::Bar(height),
        (Some(None), _) => {
            say("pattern: --bar takes a height in pixels");
            std::process::exit(2);
        }
        (_, Some(Some(side))) => compositor_pattern::Shape::Menu(side),
        (_, Some(None)) => {
            say("pattern: --menu takes a size in pixels");
            std::process::exit(2);
        }
        _ => compositor_pattern::Shape::Window,
    };

    match compositor_pattern::run_shaped(pattern, &title, shape) {
        Ok(line) => say(&line),
        Err(error) => {
            say(&format!("pattern: failed: {error}"));
            std::process::exit(1);
        }
    }
}

/// Where the window titled `title` says it has been drawn.
fn drawn_path(title: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("pattern-{title}.drawn"))
}

/// Wait until the window titled `title` has been drawn, for as long as a
/// slow emulated machine could take to start one, and then go on either
/// way: a window that never comes is the other window's failure to report,
/// not this one's.
fn wait_until_drawn(title: &str) {
    const PATIENCE: std::time::Duration = std::time::Duration::from_secs(60);
    let path = drawn_path(title);
    let began = std::time::Instant::now();
    while !path.exists() {
        if began.elapsed() >= PATIENCE {
            say(&format!(
                "pattern: {title} was never drawn; going on without it"
            ));
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Be the wallpaper `file` holds, and end when the compositor does.
fn wallpaper(file: Option<&str>) -> ! {
    let Some(file) = file else {
        say("pattern: --wallpaper takes the picture's file");
        std::process::exit(2);
    };
    let shown = std::fs::read(file)
        .map_err(|error| format!("reading {file}: {error}"))
        .and_then(|bytes| compositor_pattern::Picture::parse(&bytes))
        .and_then(compositor_pattern::run_wallpaper);
    match shown {
        Ok(line) => {
            say(&line);
            std::process::exit(0)
        }
        Err(error) => {
            say(&format!("pattern: no wallpaper: {error}"));
            std::process::exit(1)
        }
    }
}

/// Be the moving wallpaper the rest of the command line names, and end when
/// the compositor does.
///
/// `mpvpaper`'s own shape, so that a `hyprland.conf` written for that says
/// the same thing here: `[-o <options>] [<output>|ALL] <file>`. The options
/// are mpv's and there is no mpv, so what is understood of them is what has
/// a meaning without one, and the rest is said and ignored rather than
/// refused -- a configuration carried from a desktop should start a
/// wallpaper, not an error.
fn video(rest: &[String]) -> ! {
    let mut options: Vec<String> = Vec::new();
    let mut words: Vec<&str> = Vec::new();
    let mut at = 0;
    while let Some(word) = rest.get(at) {
        match word.as_str() {
            "-o" | "--mpv-options" => {
                if let Some(given) = rest.get(at + 1) {
                    options.extend(given.split_whitespace().map(str::to_owned));
                }
                at += 2;
            }
            other => {
                words.push(other);
                at += 1;
            }
        }
    }
    // The file is the last word; an output before it is which screen, and
    // this guest has one, so it is taken and said.
    let Some((file, before)) = words.split_last() else {
        say("pattern: --video takes [-o <options>] [<output>] <file>");
        std::process::exit(2);
    };
    if let Some(output) = before.last()
        && !output.eq_ignore_ascii_case("all")
    {
        say(&format!(
            "pattern: one screen here, so the wallpaper goes on it rather than on {output}"
        ));
    }
    for option in &options {
        match option.trim_start_matches('-') {
            // There is no audio on this machine at all, so the option every
            // `mpvpaper` line carries is already true.
            "no-audio" | "loop" | "loop-playlist" | "loop-file" => {}
            other => say(&format!("pattern: no mpv here, so `{other}` does nothing")),
        }
    }
    let shown = std::fs::read(file)
        .map_err(|error| format!("reading {file}: {error}"))
        .and_then(|bytes| compositor_pattern::Movie::parse(&bytes))
        .and_then(compositor_pattern::run_video);
    match shown {
        Ok(line) => {
            say(&line);
            std::process::exit(0)
        }
        Err(error) => {
            say(&format!("pattern: no wallpaper: {error}"));
            std::process::exit(1)
        }
    }
}

fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}
