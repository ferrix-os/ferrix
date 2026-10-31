//! Which X client a window of the X server is (`docs/LOGGING.md` §18 "Hang
//! snapshots"). An X program's windows reach hyprix through the X server,
//! yserver, which is their Wayland client, so the pid hyprix knows for them
//! is yserver's. An app error's snapshot wants the program's: the X server
//! is asked, as any X client may ask it, which top-level window has the
//! title and class the Wayland window was given, and that window's
//! `_NET_WM_PID`, the pid its program says it has (Wine, GTK, Qt and SDL all
//! set it).
//!
//! The few requests this needs are written out here: the connection setup,
//! `InternAtom`, `QueryTree` and `GetProperty`, one at a time, each reply
//! read before the next request, with a timeout on every read, so an X
//! server that does not answer costs at most that.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// How long one reply may take.
const TIMEOUT: Duration = Duration::from_secs(2);
/// Predefined atoms (X11 protocol, appendix B).
const ATOM_WM_NAME: u32 = 39;
const ATOM_WM_CLASS: u32 = 67;
/// How many top-level windows are looked at, at most.
const MAX_WINDOWS: usize = 4096;

/// The display an X server's process serves (`:0` gives 0), when `pid` is
/// an X server: a program called `yserver*`, `Xwayland` or `Xorg` among its
/// arguments (yserver runs through the dynamic loader, so its own name is
/// an argument, not the program), with a `:N` argument.
#[must_use]
pub fn display_of(pid: i32) -> Option<u32> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    display_in(&cmdline)
}

/// [`display_of`] for a command line as `/proc` gives it (NUL-separated).
#[must_use]
pub fn display_in(cmdline: &[u8]) -> Option<u32> {
    let words: Vec<&[u8]> = cmdline.split(|&byte| byte == 0).collect();
    let server = words.iter().any(|word| {
        let name = word.rsplit(|&byte| byte == b'/').next().unwrap_or(word);
        name.starts_with(b"yserver") || name == b"Xwayland" || name == b"Xorg"
    });
    if !server {
        return None;
    }
    words.iter().find_map(|word| {
        let number = word.strip_prefix(b":")?;
        if number.is_empty() || !number.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(number).ok()?.parse().ok()
    })
}

/// What a top-level window of the X server said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    /// The window.
    pub window: u32,
    /// Its `_NET_WM_PID`.
    pub pid: i32,
}

/// The windows of display `display` whose title is `title` and whose
/// `WM_CLASS` (either half) is `class`, with the pid each says it has.
/// Empty when the server cannot be reached or nothing matches.
#[must_use]
pub fn owners(display: u32, title: &str, class: &str) -> Vec<Owner> {
    let path = format!("/tmp/.X11-unix/X{display}");
    UnixStream::connect(path)
        .ok()
        .and_then(|stream| owners_on(stream, title, class).ok())
        .unwrap_or_default()
}

/// [`owners`] over a connected stream (a test's server, or the display's).
///
/// # Errors
/// The connection failed, timed out or answered out of the protocol.
pub fn owners_on(stream: UnixStream, title: &str, class: &str) -> std::io::Result<Vec<Owner>> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut x = Connection::setup(stream)?;
    let net_wm_pid = x.atom(b"_NET_WM_PID")?;
    if net_wm_pid == 0 {
        return Ok(Vec::new());
    }
    let net_wm_name = x.atom(b"_NET_WM_NAME")?;
    let mut found = Vec::new();
    for window in x.children(x.root)?.into_iter().take(MAX_WINDOWS) {
        let name = match net_wm_name {
            0 => None,
            atom => x.property(window, atom)?,
        };
        let name = match name {
            Some(name) => name,
            None => x.property(window, ATOM_WM_NAME)?.unwrap_or_default(),
        };
        if name != title.as_bytes() {
            continue;
        }
        let classes = x.property(window, ATOM_WM_CLASS)?.unwrap_or_default();
        if !classes
            .split(|&byte| byte == 0)
            .any(|half| !half.is_empty() && half == class.as_bytes())
        {
            continue;
        }
        if let Some(value) = x.property(window, net_wm_pid)?
            && let Some(bytes) = value.first_chunk::<4>()
        {
            let pid = i32::from_le_bytes(*bytes);
            if pid > 0 {
                found.push(Owner { window, pid });
            }
        }
    }
    Ok(found)
}

/// One connection, little-endian, with no authorisation: what the local
/// socket of an X server with open local access takes (yserver's).
struct Connection {
    stream: UnixStream,
    root: u32,
}

impl Connection {
    /// The setup request and its reply; the first screen's root.
    fn setup(mut stream: UnixStream) -> std::io::Result<Self> {
        // 'l', unused, protocol 11.0, no authorisation name or data, unused.
        stream.write_all(&[b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0])?;
        let mut head = [0u8; 8];
        stream.read_exact(&mut head)?;
        let words = usize::from(u16::from_le_bytes([head[6], head[7]]));
        let mut body = vec![0u8; words * 4];
        stream.read_exact(&mut body)?;
        if head[0] != 1 {
            return Err(refused("the X server refused the connection"));
        }
        let root = setup_root(&body).ok_or_else(|| refused("a setup reply with no screen"))?;
        Ok(Self { stream, root })
    }

    /// `InternAtom` with only-if-exists: 0 when the server has no such atom.
    fn atom(&mut self, name: &[u8]) -> std::io::Result<u32> {
        let mut request = Vec::new();
        let padded = name.len().div_ceil(4) * 4;
        request.extend_from_slice(&[16, 1]);
        request.extend_from_slice(&length(2 + padded / 4)?);
        request.extend_from_slice(&u16_of(name.len())?);
        request.extend_from_slice(&[0, 0]);
        request.extend_from_slice(name);
        request.resize(8 + padded, 0);
        let reply = self.call(&request)?;
        Ok(u32_at(&reply, 8))
    }

    /// `QueryTree`: `window`'s children.
    fn children(&mut self, window: u32) -> std::io::Result<Vec<u32>> {
        let mut request = vec![15, 0, 2, 0];
        request.extend_from_slice(&window.to_le_bytes());
        let reply = self.call(&request)?;
        let count = usize::from(u16_at(&reply, 16));
        Ok((0..count)
            .map_while(|at| reply.get(32 + at * 4..)?.first_chunk::<4>().copied())
            .map(u32::from_le_bytes)
            .collect())
    }

    /// `GetProperty` of any type: its bytes, `None` when the window has no
    /// such property (or is gone).
    fn property(&mut self, window: u32, property: u32) -> std::io::Result<Option<Vec<u8>>> {
        let mut request = vec![20, 0, 6, 0];
        request.extend_from_slice(&window.to_le_bytes());
        request.extend_from_slice(&property.to_le_bytes());
        request.extend_from_slice(&0u32.to_le_bytes()); // AnyPropertyType
        request.extend_from_slice(&0u32.to_le_bytes()); // offset
        request.extend_from_slice(&1024u32.to_le_bytes()); // length, in 4-byte units
        let reply = match self.call(&request) {
            Ok(reply) => reply,
            // A window that went away between QueryTree and here.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let format = usize::from(reply.get(1).copied().unwrap_or(0));
        if u32_at(&reply, 8) == 0 || format == 0 {
            return Ok(None);
        }
        let count = usize::try_from(u32_at(&reply, 16)).unwrap_or(usize::MAX);
        let bytes = count.saturating_mul(format / 8);
        Ok(reply.get(32..32 + bytes).map(<[u8]>::to_vec))
    }

    /// Send `request`, read its reply (events are skipped; an error is
    /// `NotFound`).
    fn call(&mut self, request: &[u8]) -> std::io::Result<Vec<u8>> {
        self.stream.write_all(request)?;
        loop {
            let mut head = [0u8; 32];
            self.stream.read_exact(&mut head)?;
            match head[0] {
                0 => return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "X error")),
                1 => {
                    let extra = usize::try_from(u32_at(&head, 4)).unwrap_or(usize::MAX);
                    if extra > 1 << 20 {
                        return Err(refused("an X reply too long to be one"));
                    }
                    let mut reply = head.to_vec();
                    reply.resize(32 + extra * 4, 0);
                    self.stream
                        .read_exact(reply.get_mut(32..).unwrap_or_default())?;
                    return Ok(reply);
                }
                _ => {} // an event: none was asked for, but skip it
            }
        }
    }
}

/// The first screen's root window in a setup reply's body (after its
/// 8-byte head).
fn setup_root(body: &[u8]) -> Option<u32> {
    let vendor = usize::from(u16::from_le_bytes([*body.get(16)?, *body.get(17)?]));
    let formats = usize::from(*body.get(21)?);
    let screen = 32 + vendor.div_ceil(4) * 4 + formats * 8;
    body.get(screen..)?
        .first_chunk::<4>()
        .map(|root| u32::from_le_bytes(*root))
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    bytes
        .get(at..)
        .and_then(<[u8]>::first_chunk::<4>)
        .map_or(0, |b| u32::from_le_bytes(*b))
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    bytes
        .get(at..)
        .and_then(<[u8]>::first_chunk::<2>)
        .map_or(0, |b| u16::from_le_bytes(*b))
}

fn u16_of(value: usize) -> std::io::Result<[u8; 2]> {
    u16::try_from(value)
        .map(u16::to_le_bytes)
        .map_err(|_| refused("a request too long"))
}

fn length(words: usize) -> std::io::Result<[u8; 2]> {
    u16_of(words)
}

fn refused(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_x_server_is_told_by_its_arguments() {
        let yserver = b"/data/usr/lib64/ld-linux-x86-64.so.2\0--library-path\0/data/usr/lib/x86_64-linux-gnu\0/data/yserver/yserver.dri3-zc-5cd1bad4\0:0\0-nolisten\0tcp\0";
        assert_eq!(display_in(yserver), Some(0));
        assert_eq!(display_in(b"/usr/bin/Xwayland\0:12\0-rootless\0"), Some(12));
        assert_eq!(display_in(b"/usr/bin/foot\0"), None);
        // A program that only names a display is not an X server.
        assert_eq!(display_in(b"/usr/bin/xterm\0-display\0:0\0"), None);
        assert_eq!(display_in(b"/data/yserver/yserver\0"), None);
    }

    /// A little X server: one root with three top-levels, answering the
    /// requests `owners_on` makes, as a real server lays its replies out.
    /// A top-level of the test server: its id, title, `WM_CLASS` and pid.
    type Window = (u32, &'static [u8], &'static [u8], Option<i32>);

    fn serve(mut stream: UnixStream) {
        const PID_ATOM: u32 = 300;
        const NAME_ATOM: u32 = 301;
        let windows: [Window; 3] = [
            (
                0x40_0001,
                b"Endfield",
                b"steam_app_4732690\0steam_app_4732690\0",
                Some(4973),
            ),
            (
                0x40_0002,
                b"Fatal error in GC",
                b"steam_app_4732690\0steam_app_4732690\0",
                Some(4973),
            ),
            (0x50_0001, b"Fatal error in GC", b"other\0Other\0", Some(77)),
        ];
        let mut setup = [0u8; 12];
        stream.read_exact(&mut setup).unwrap();
        assert_eq!(setup[0], b'l');
        // A body: 32 bytes of fixed fields, a vendor of 6 bytes (padded to
        // 8), one format, then the screen with its root first.
        let mut body = vec![0u8; 32];
        body[16..18].copy_from_slice(&6u16.to_le_bytes());
        body[20] = 1; // screens
        body[21] = 1; // formats
        body.extend_from_slice(b"Ferrix\0\0");
        body.extend_from_slice(&[0u8; 8]);
        body.extend_from_slice(&0x1234u32.to_le_bytes());
        body.resize(body.len().div_ceil(4) * 4 + 36, 0);
        let mut head = vec![1u8, 0, 11, 0, 0, 0];
        head.extend_from_slice(&u16::try_from(body.len() / 4).unwrap().to_le_bytes());
        stream.write_all(&head).unwrap();
        stream.write_all(&body).unwrap();
        let reply = |stream: &mut UnixStream, mut fixed: [u8; 32], extra: &[u8]| {
            fixed[0] = 1;
            let words = u32::try_from(extra.len().div_ceil(4)).unwrap();
            fixed[4..8].copy_from_slice(&words.to_le_bytes());
            let mut out = fixed.to_vec();
            out.extend_from_slice(extra);
            out.resize(32 + extra.len().div_ceil(4) * 4, 0);
            stream.write_all(&out).unwrap();
        };
        loop {
            let mut request = [0u8; 4];
            if stream.read_exact(&mut request).is_err() {
                return;
            }
            let words = usize::from(u16::from_le_bytes([request[2], request[3]]));
            let mut rest = vec![0u8; words * 4 - 4];
            stream.read_exact(&mut rest).unwrap();
            let mut fixed = [0u8; 32];
            match request[0] {
                16 => {
                    let len = usize::from(u16::from_le_bytes([rest[0], rest[1]]));
                    let atom = match &rest[4..4 + len] {
                        b"_NET_WM_PID" => PID_ATOM,
                        b"_NET_WM_NAME" => NAME_ATOM,
                        _ => 0,
                    };
                    fixed[8..12].copy_from_slice(&atom.to_le_bytes());
                    // An event first, which the client must skip.
                    stream.write_all(&[12u8; 32]).unwrap();
                    reply(&mut stream, fixed, &[]);
                }
                15 => {
                    assert_eq!(u32_at(&rest, 0), 0x1234);
                    fixed[16..18].copy_from_slice(&3u16.to_le_bytes());
                    let mut children = Vec::new();
                    for (window, ..) in &windows {
                        children.extend_from_slice(&window.to_le_bytes());
                    }
                    reply(&mut stream, fixed, &children);
                }
                20 => {
                    let window = u32_at(&rest, 0);
                    let property = u32_at(&rest, 4);
                    let Some((_, name, class, pid)) = windows.iter().find(|(w, ..)| *w == window)
                    else {
                        stream.write_all(&[0u8; 32]).unwrap(); // BadWindow
                        continue;
                    };
                    let (format, value): (u8, Vec<u8>) = match property {
                        NAME_ATOM => (8, name.to_vec()),
                        ATOM_WM_CLASS => (8, class.to_vec()),
                        PID_ATOM => match pid {
                            Some(pid) => (32, pid.to_le_bytes().to_vec()),
                            None => (0, Vec::new()),
                        },
                        _ => (0, Vec::new()),
                    };
                    fixed[1] = format;
                    if format != 0 {
                        fixed[8..12].copy_from_slice(&property.to_le_bytes());
                        let units = value.len() / usize::from(format / 8);
                        fixed[16..20].copy_from_slice(&u32::try_from(units).unwrap().to_le_bytes());
                    }
                    reply(&mut stream, fixed, &value);
                }
                other => panic!("unexpected request {other}"),
            }
        }
    }

    #[test]
    fn the_x_server_says_which_program_a_window_is() {
        let (client, server) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || serve(server));
        let found = owners_on(client, "Fatal error in GC", "steam_app_4732690").unwrap();
        server.join().unwrap();
        assert_eq!(
            found,
            vec![Owner {
                window: 0x40_0002,
                pid: 4973
            }]
        );
    }

    #[test]
    fn no_window_of_that_title_and_class_is_no_owner() {
        let (client, server) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || serve(server));
        let found = owners_on(client, "Fatal error in GC", "foot").unwrap();
        server.join().unwrap();
        assert!(found.is_empty());
    }

    /// Against a real X server: `X_OWNER_DISPLAY=93 X_OWNER_TITLE=... \
    /// X_OWNER_CLASS=... X_OWNER_PID=... cargo test -- --ignored`, with a
    /// window of that title, class and pid open on it (Xvfb and Tk will do).
    #[test]
    #[ignore = "needs an X server with a known window"]
    fn a_real_x_server_says_which_program_a_window_is() {
        let var = |name: &str| std::env::var(name).unwrap();
        let found = owners(
            var("X_OWNER_DISPLAY").parse().unwrap(),
            &var("X_OWNER_TITLE"),
            &var("X_OWNER_CLASS"),
        );
        let pid: i32 = var("X_OWNER_PID").parse().unwrap();
        assert!(found.iter().any(|owner| owner.pid == pid), "{found:?}");
    }

    #[test]
    fn a_server_that_says_nothing_costs_the_timeout_not_a_hang() {
        let (client, _server) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        assert!(owners_on(client, "x", "y").is_err());
        assert!(started.elapsed() < TIMEOUT * 2);
    }
}
