//! `portald [--light]`: `org.freedesktop.portal.Settings` on the session
//! bus `DBUS_SESSION_BUS_ADDRESS` names, until the bus goes.

use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;

use compositor_dbus::{Connection, session_path};
use compositor_portald::{NAME, answer};

/// Say one line.
fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "portald: {line}");
    let _ = out.flush();
}

fn main() {
    let mut dark = true;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--light" => dark = false,
            "--dark" => dark = true,
            other => {
                say(&format!("unknown argument {other}; portald [--light]"));
                std::process::exit(2);
            }
        }
    }
    let Some(path) = std::env::var("DBUS_SESSION_BUS_ADDRESS")
        .ok()
        .as_deref()
        .and_then(session_path)
    else {
        say("no session bus (DBUS_SESSION_BUS_ADDRESS names no unix:path); nothing to do");
        return;
    };
    let uid = std::fs::metadata("/proc/self").map_or(0, |meta| meta.uid());
    // The bus may come up after the desktop starts this: a few seconds.
    let mut tries = 0;
    let mut connection = loop {
        match Connection::open(&path, uid) {
            Ok(connection) => break connection,
            Err(error) if tries < 50 => {
                tries += 1;
                if tries == 1 {
                    say(&format!(
                        "the session bus at {path}: {error}; waiting for it"
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(error) => {
                say(&format!("the session bus at {path}: {error}"));
                std::process::exit(1);
            }
        }
    };
    match connection.request_name(NAME) {
        Ok(true) => say(&format!(
            "{NAME} on the session bus as {}, {}",
            connection.unique,
            if dark { "dark" } else { "light" }
        )),
        Ok(false) => {
            say(&format!("{NAME} is someone else's; nothing to do"));
            return;
        }
        Err(error) => {
            say(&format!("RequestName: {error}"));
            std::process::exit(1);
        }
    }
    // A dialog runs as a program of its own; what it chose comes back on
    // this channel, and the loop, waking a few times a second, sends it.
    let _ = connection
        .stream()
        .set_read_timeout(Some(std::time::Duration::from_millis(200)));
    let (done, finished) = std::sync::mpsc::channel::<(String, String, Option<String>)>();
    let mut serial = 0_u64;
    loop {
        while let Ok((path, sender, chosen)) = finished.try_recv() {
            let signal = compositor_portald::chooser::response(&path, &sender, chosen.as_deref());
            if let Err(error) = connection.send(&signal) {
                say(&format!("a response was not sent: {error}"));
                return;
            }
            say(&format!(
                "{path}: {}",
                chosen
                    .as_deref()
                    .map_or("cancelled".to_owned(), |file| format!("chose {file}"))
            ));
        }
        let message = match connection.receive() {
            Ok(message) => message,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => {
                say(&format!("the session bus went: {error}"));
                return;
            }
        };
        serial += 1;
        if let Some((reply, path, sender, asked)) = compositor_portald::dialog(&message, serial) {
            if let Err(error) = connection.send(&reply) {
                say(&format!("a reply was not sent: {error}"));
                return;
            }
            if path.is_empty() {
                continue;
            }
            say(&format!(
                "{sender} {} {path}",
                if asked.save { "SaveFile" } else { "OpenFile" }
            ));
            let done = done.clone();
            let _ = std::thread::Builder::new()
                .name("portald-dialog".to_owned())
                .spawn(move || {
                    let chosen = run_dialog(&asked);
                    let _ = done.send((path, sender, chosen));
                });
            continue;
        }
        if let Some(reply) = answer(&message, dark) {
            if let Err(error) = connection.send(&reply) {
                say(&format!("a reply was not sent: {error}"));
                return;
            }
            say(&format!(
                "{} {}.{} answered",
                message.sender.as_deref().unwrap_or("?"),
                message.interface.as_deref().unwrap_or("-"),
                message.member.as_deref().unwrap_or("-")
            ));
        }
    }
}

/// Run the dialog for `asked`; the path chosen, or `None` for a cancel or
/// a dialog that could not run.
fn run_dialog(asked: &compositor_portald::chooser::Asked) -> Option<String> {
    let program = std::env::var("PORTALD_CHOOSER")
        .unwrap_or_else(|_| compositor_portald::chooser::PROGRAM.to_owned());
    let output = std::process::Command::new(&program)
        .args(compositor_portald::chooser::arguments(asked))
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            text.lines()
                .next()
                .map(str::to_owned)
                .filter(|l| !l.is_empty())
        }
        Ok(_) => None,
        Err(error) => {
            say(&format!("{program}: {error}"));
            None
        }
    }
}
