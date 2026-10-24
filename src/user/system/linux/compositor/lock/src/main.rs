//! `lock [--clear] [seconds]`: lock the screen, hold it, and unlock;
//! `--clear` draws the lock see-through, so the compositor's wallpaper
//! shows under it where it is allowed to.

use std::io::Write as _;
use std::time::Duration;

fn main() {
    let arguments =
        compositor_evecho::init::unshell(std::env::args().skip(1).collect::<Vec<String>>());
    let clear = arguments.iter().any(|word| word == "--clear");
    let held = arguments
        .iter()
        .find(|word| *word != "--clear")
        .and_then(|word| word.parse::<u64>().ok())
        .map_or(Duration::from_secs(5), Duration::from_secs);
    match socket().and_then(|path| compositor_lock::lock_drawing(&path, held, clear)) {
        Ok(locked) => say(&format!(
            "lock: locked {} screen(s) and unlocked again",
            locked.screens
        )),
        Err(error) => {
            say(&format!("lock: failed: {error}"));
            std::process::exit(1);
        }
    }
}

/// Where the compositor's socket is.
fn socket() -> Result<std::path::PathBuf, String> {
    let display =
        std::env::var("WAYLAND_DISPLAY").map_err(|_| "WAYLAND_DISPLAY is not set".to_owned())?;
    compositor_socket::socket_path(&display).map_err(|error| format!("the socket: {error}"))
}

/// Say a line on the standard output, flushed.
fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}
