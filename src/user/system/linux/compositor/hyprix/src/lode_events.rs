//! What the desktop tells the log (`docs/LOGGING.md` §18 "Session
//! programs and app errors"): a window that opened, a window that looks
//! like an error dialog -- the Windows game's `SuspendThread loop failed`,
//! which nothing else records since nothing crashed -- and each program
//! hyprix starts writing under its own name.
//!
//! Each kind is switched in `/etc/ferrix/lode.d/events.conf`, a person's
//! `~/.config/ferrix/lode/events.conf` over it (`ferrix_lode_core::events`,
//! whose defaults this repeats: app errors on, streams and windows off).
//! The files are read each time an event would be made, so a change takes
//! effect at once, with no reload. Nothing here waits for lode: a lode that
//! is not running costs a failed `connect`, and the event is not made.

use std::os::unix::net::UnixDatagram;

/// Where lode takes systemd's native protocol.
const JOURNAL: &str = "/run/systemd/journal/socket";
/// lode's reader, whose `cat` carries a program's output.
const LODE: &str = "/bin/lode";

/// One kind's switch and level, as `events.conf` set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switch {
    /// On.
    pub on: bool,
    /// The level its records get, 0 to 7.
    pub level: u8,
}

/// `kind`'s switch: its default, then the system's file, then the
/// person's.
#[must_use]
pub fn switch(kind: &str) -> Switch {
    let mut switch = match kind {
        "window-opened" => Switch {
            on: false,
            level: 6,
        },
        "app-error" => Switch { on: true, level: 4 },
        // A program wrapped in `lode cat` loses its output when its session
        // ends and takes the `lode cat` with it, while the program lingers
        // (test-compositor's linger row): off until the wrapper can outlive
        // the session; the box turns it on.
        "session-streams" => Switch {
            on: false,
            level: 6,
        },
        _ => Switch { on: true, level: 6 },
    };
    let mut files = vec![std::path::PathBuf::from("/etc/ferrix/lode.d/events.conf")];
    if let Some(home) = std::env::var_os("HOME") {
        files.push(std::path::PathBuf::from(home).join(".config/ferrix/lode/events.conf"));
    }
    for file in files {
        if let Ok(text) = std::fs::read_to_string(&file) {
            apply(&text, kind, &mut switch);
        }
    }
    switch
}

/// `text`'s line for `kind`, if it has one, applied to `switch`.
pub fn apply(text: &str, kind: &str, switch: &mut Switch) {
    for raw in text.lines() {
        let line = raw.split_once('#').map_or(raw, |(before, _)| before);
        let words: Vec<&str> = line.split_whitespace().collect();
        let (Some(&name), Some(&state)) = (words.first(), words.get(1)) else {
            continue;
        };
        if name != kind {
            continue;
        }
        switch.on = matches!(state, "on" | "yes" | "true");
        if let Some(level) = words.get(2).and_then(|word| level_of(word)) {
            switch.level = level;
        }
    }
}

/// A level's number from its name or number.
fn level_of(word: &str) -> Option<u8> {
    const NAMES: [&str; 8] = [
        "emerg", "alert", "crit", "err", "warning", "notice", "info", "debug",
    ];
    word.parse::<u8>()
        .ok()
        .filter(|&level| level < 8)
        .or_else(|| {
            NAMES
                .iter()
                .position(|name| *name == word)
                .and_then(|at| u8::try_from(at).ok())
        })
}

/// Whether a window called `title`, of class `class`, looks like an error
/// dialog: Windows' dialog class (`#32770`, which Wine's message boxes keep),
/// or a title or class that says error, failed, failure, fatal, exception or
/// crash.
#[must_use]
pub fn is_error_dialog(class: &str, title: &str) -> bool {
    if class == "#32770" {
        return true;
    }
    let words = format!("{class} {title}").to_lowercase();
    ["error", "failed", "failure", "fatal", "exception", "crash"]
        .iter()
        .any(|word| words.contains(word))
}

/// What a window that opened is, for the log: `window-opened` or
/// `app-error`, and the fields its record carries.
#[must_use]
pub fn window_fields(
    kind: &str,
    level: u8,
    class: &str,
    title: &str,
    pid: i32,
    dialog_of: bool,
) -> Vec<(String, Vec<u8>)> {
    let message = match kind {
        "app-error" => format!("app error: {title} ({class})"),
        _ => format!("window opened: {title} ({class})"),
    };
    let mut fields = vec![
        ("MESSAGE".to_owned(), message.into_bytes()),
        ("PRIORITY".to_owned(), level.to_string().into_bytes()),
        ("SYSLOG_IDENTIFIER".to_owned(), b"hyprix".to_vec()),
        ("FERRIX_EVENT".to_owned(), kind.as_bytes().to_vec()),
        ("WINDOW_TITLE".to_owned(), title.as_bytes().to_vec()),
        ("WINDOW_CLASS".to_owned(), class.as_bytes().to_vec()),
        ("WINDOW_PID".to_owned(), pid.to_string().into_bytes()),
    ];
    if dialog_of {
        fields.push(("WINDOW_DIALOG".to_owned(), b"1".to_vec()));
    }
    // Who owns it, read now, while the client is connected and alive.
    if pid > 0 {
        if let Ok(comm) = std::fs::read(format!("/proc/{pid}/comm")) {
            let comm = comm.strip_suffix(b"\n").unwrap_or(&comm).to_vec();
            fields.push(("WINDOW_COMM".to_owned(), comm));
        }
        if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
            fields.push((
                "WINDOW_EXE".to_owned(),
                exe.to_string_lossy().into_owned().into_bytes(),
            ));
        }
    }
    fields
}

/// `fields` as one datagram of systemd's native protocol: `NAME=value`, or
/// `NAME`, a newline, a little-endian length and the value for one with a
/// newline in it.
#[must_use]
pub fn datagram(fields: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(name.as_bytes());
        if value.contains(&b'\n') {
            out.push(b'\n');
            out.extend_from_slice(&(value.len() as u64).to_le_bytes());
        } else {
            out.push(b'=');
        }
        out.extend_from_slice(value);
        out.push(b'\n');
    }
    out
}

/// A window opened: say so, if its kind is on.
pub fn window_opened(class: &str, title: &str, pid: i32, dialog_of: bool) {
    let kind = if is_error_dialog(class, title) {
        "app-error"
    } else {
        "window-opened"
    };
    let setting = switch(kind);
    if !setting.on {
        return;
    }
    // An X program's window comes from the X server, whose pid is the one
    // hyprix knows: the server is asked which program it is, on a thread of
    // its own, since the compositor must not wait on another program.
    if kind == "app-error"
        && pid > 0
        && let Some(display) = crate::x_owner::display_of(pid)
    {
        let (class, title) = (class.to_owned(), title.to_owned());
        let asked = std::thread::Builder::new()
            .name("app-error-x".to_owned())
            .spawn(move || {
                let owners = crate::x_owner::owners(display, &title, &class);
                x_app_error(setting.level, &class, &title, pid, &owners, dialog_of);
            });
        if asked.is_ok() {
            return;
        }
    }
    let fields = window_fields(kind, setting.level, class, title, pid, dialog_of);
    send(&fields);
    // What the program is doing while it shows its error: a hang snapshot
    // of the window's process, by `lode snapshot` as this session's user.
    if kind == "app-error" {
        snapshot(pid);
    }
}

/// An X program's app error: the record names the program the X server says
/// the window is (`owners`, from its `_NET_WM_PID`) with the server beside
/// it, and the snapshot is of that program. With no owner found, the record
/// and the snapshot are the server's, as for any other window.
fn x_app_error(
    level: u8,
    class: &str,
    title: &str,
    server: i32,
    owners: &[crate::x_owner::Owner],
    dialog_of: bool,
) {
    // The first window's program is the record's; every program any matching
    // window names gets a snapshot, once.
    let owner = owners.first().map_or(server, |first| first.pid);
    let mut pids: Vec<i32> = owners.iter().map(|owner| owner.pid).collect();
    pids.sort_unstable();
    pids.dedup();
    let mut fields = window_fields("app-error", level, class, title, owner, dialog_of);
    fields.push(("X_SERVER_PID".to_owned(), server.to_string().into_bytes()));
    if let Some(first) = owners.first() {
        fields.push((
            "X_WINDOW".to_owned(),
            format!("{:#x}", first.window).into_bytes(),
        ));
    }
    send(&fields);
    if pids.is_empty() {
        snapshot(server);
    }
    for pid in pids {
        snapshot(pid);
    }
}

/// `fields` to lode, if it is there.
fn send(fields: &[(String, Vec<u8>)]) {
    if let Ok(socket) = UnixDatagram::unbound()
        && socket.connect(JOURNAL).is_ok()
    {
        let _ = socket.send(&datagram(fields));
    }
}

/// A hang snapshot of `pid` (`events.conf`'s `snapshot`), by `lode snapshot`
/// as this session's user.
fn snapshot(pid: i32) {
    if pid > 0
        && switch("snapshot").on
        && std::path::Path::new(LODE).exists()
        && let Ok(child) = std::process::Command::new(LODE)
            .args(["snapshot", &pid.to_string(), "--reason", "app-error"])
            .stdout(std::process::Stdio::null())
            .spawn()
    {
        crate::children::started(child.id());
    }
}

/// The name a program hyprix starts writes under: its program's file name,
/// or for a shell running a script (`sh /etc/yserver.sh`), the script's
/// without `.sh`.
#[must_use]
pub fn identifier(program: &str, arguments: &[String]) -> String {
    let base = |path: &str| path.rsplit('/').next().unwrap_or(path).to_owned();
    let name = base(program);
    let shell = matches!(
        name.as_str(),
        "sh" | "bash" | "zsh" | "dash" | "ash" | "busybox"
    );
    if shell
        && let Some(script) = arguments
            .iter()
            .find(|word| !word.starts_with('-') && !matches!(word.as_str(), "sh" | "ash"))
    {
        let script = base(script);
        return script.strip_suffix(".sh").unwrap_or(&script).to_owned();
    }
    name
}

/// The write end a program called `identifier` writes its standard output
/// and error to: a pipe into `lode cat -t IDENTIFIER --echo`, which hands
/// each line to lode under that name and writes it on to hyprix's own
/// standard output as before, so the console and every gate that reads it
/// see what they saw; lode keeps the two copies as one (`ferrix_lode_core::
/// units::Echoes`). The `lode cat` process is reaped as hyprix's other
/// children are. `None` when session streams are off or there is no lode,
/// and the program inherits hyprix's output as it always did.
#[must_use]
pub fn stream(identifier: &str) -> Option<std::os::fd::OwnedFd> {
    if !switch("session-streams").on || !std::path::Path::new(LODE).exists() {
        return None;
    }
    let mut cat = std::process::Command::new(LODE)
        .args(["cat", "-t", identifier, "--echo"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    crate::children::started(cat.id());
    cat.stdin.take().map(std::os::fd::OwnedFd::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_dialogs_are_told_from_windows() {
        assert!(is_error_dialog("#32770", "Endfield"));
        assert!(is_error_dialog("endfield.exe", "SuspendThread loop failed"));
        assert!(is_error_dialog("steam", "Fatal Error"));
        assert!(!is_error_dialog("foot", "~/src"));
        assert!(!is_error_dialog("firefox", "Mozilla Firefox"));
    }

    #[test]
    fn a_switch_reads_the_file_a_later_line_winning() {
        let mut switch = Switch {
            on: false,
            level: 6,
        };
        apply(
            "# x\nwindow-opened on info\napp-error off\nwindow-opened on notice\n",
            "window-opened",
            &mut switch,
        );
        assert_eq!(switch, Switch { on: true, level: 5 });
        apply("window-opened off\n", "window-opened", &mut switch);
        assert!(!switch.on);
    }

    #[test]
    fn a_program_writes_under_its_name_or_its_scripts() {
        let words = |list: &[&str]| {
            list.iter()
                .map(|word| (*word).to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(identifier("/usr/bin/waybar", &[]), "waybar");
        assert_eq!(
            identifier("/bin/busybox", &words(&["sh", "/etc/yserver.sh"])),
            "yserver"
        );
        assert_eq!(identifier("sh", &words(&["-c", "x"])), "x");
        assert_eq!(identifier("sh", &[]), "sh");
    }

    #[test]
    fn a_window_record_is_one_native_datagram() {
        let fields = window_fields(
            "app-error",
            4,
            "#32770",
            "SuspendThread loop failed",
            0,
            true,
        );
        let bytes = datagram(&fields);
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("MESSAGE=app error: SuspendThread loop failed (#32770)\n"));
        assert!(text.contains("FERRIX_EVENT=app-error\n"));
        assert!(text.contains("PRIORITY=4\n"));
        assert!(text.contains("WINDOW_DIALOG=1\n"));
    }
}
