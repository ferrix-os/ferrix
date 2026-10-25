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
    let mut connection = match Connection::open(&path, uid) {
        Ok(connection) => connection,
        Err(error) => {
            say(&format!("the session bus at {path}: {error}"));
            std::process::exit(1);
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
    loop {
        let message = match connection.receive() {
            Ok(message) => message,
            Err(error) => {
                say(&format!("the session bus went: {error}"));
                return;
            }
        };
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
