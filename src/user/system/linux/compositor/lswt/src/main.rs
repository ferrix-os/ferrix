//! `lswt`, and `lswt activate|close|minimize|unminimize <title>`.

use std::io::Write as _;

use compositor_lswt::Want;

fn main() {
    let arguments =
        compositor_evecho::init::unshell(std::env::args().skip(1).collect::<Vec<String>>());
    let rest = || {
        arguments
            .get(1..)
            .map(|words| words.join(" "))
            .unwrap_or_default()
    };
    let want = match arguments.first().map(String::as_str) {
        None | Some("list") => Want::List,
        Some("activate") => Want::Activate(rest()),
        Some("close") => Want::Close(rest()),
        Some("minimize") => Want::Minimize(rest()),
        Some("unminimize") => Want::Unminimize(rest()),
        _ => {
            say("lswt: usage: lswt [list] | lswt activate|close|minimize|unminimize <title>");
            std::process::exit(2);
        }
    };
    match socket().and_then(|path| compositor_lswt::run(&path, &want)) {
        Ok(lines) => {
            if lines.is_empty() {
                say("lswt: no windows");
            }
            for line in lines {
                say(&line);
            }
        }
        Err(error) => {
            say(&format!("lswt: failed: {error}"));
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
