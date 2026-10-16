//! `test-clipboard`: the host's clipboard and the guest's, joined over
//! vdagent, both ways in one boot (`docs/CLIPBOARD.md` §9).
//!
//! The host half is this program, not QEMU's `qemu-vdagent`: the clipboard
//! port's far end is a Unix socket xtask listens on, and it speaks vdagent to
//! the guest's agent as a viewer's QEMU would. So the whole guest path --
//! the device, `src/user/system/native/drivers/console/vport`, the agent, the compositor, `clip` --
//! is what is under test, and no display or host clipboard is.
//!
//! 1. The host grabs its clipboard with [`host_text`], which is longer than
//!    one vdagent chunk, and answers the agent's request for it; the guest's
//!    `clip paste` must print it.
//! 2. Then the guest runs `clip copy` with [`GUEST_TEXT`]; the agent must
//!    grab on the host's side, and the host's request must bring the text.
//!
//! The guest runs the two one after the other under zinc, so its own copy
//! cannot race the host's grab for the selection.
//!
//! The boot switches its root, as the desktop `run-compositor` boots does:
//! a fresh root volume is attached, `devmgr` -- and with it the port's
//! driver -- starts from the initramfs, pid 1 moves onto the volume, and the
//! compositor and the agent start from there (`docs/INIT.md` §7.3). A boot
//! without the switch passed while every desktop with one had no clipboard,
//! because the driver's socket was a path in the initramfs's `/tmp`; so the
//! gate requires the kernel's line that the switch happened.

use std::io::{Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ferrix_vdagent::chunk::{self, Reassembler};
use ferrix_vdagent::message::{ClipboardType, Message, Selection, Shape, Types, cap};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::task::Task;
use crate::{Error, Result, compositor, qemu};

// A watched desktop's clipboard, bridged to the host's Wayland one.
pub(crate) mod host;

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-clipboard",
    help_at: 270,
    help: &[
        "test-clipboard  Boot the desktop with its clipboard port on a socket xtask speaks vdagent over, and carry text both ways",
    ],
    run: |_, args| test_clipboard(args),
}];

/// What the guest copies for the host to take. No quote in it: it goes on a
/// `zinc -c '...'` line.
const GUEST_TEXT: &str = "the guest clipboard reached the host";

/// The capabilities the host half says it has: QEMU's with `clipboard=on`.
const HOST_CAPS: u32 = cap::bit(cap::CLIPBOARD_BY_DEMAND)
    | cap::bit(cap::CLIPBOARD_SELECTION)
    | cap::bit(cap::CLIPBOARD_GRAB_SERIAL);

/// The serial of the host's grab. Not 1: QEMU's serials come from its
/// clipboard's own count, and a guest that answers every grab with the same
/// small number loses to any of them.
const HOST_SERIAL: u32 = 7;

/// What the kernel says when it has moved pid 1 onto the root volume
/// (`docs/INIT.md` §7.3).
const SWITCHED: &str = "root     pid 1 moved onto the volume with the switch";

/// How long the guest may take from the compositor being up to the host
/// holding the guest's text.
const PATIENCE: Duration = Duration::from_secs(90);

/// What the host copies: a sentence repeated past one chunk's 1024 bytes, so
/// a message that arrives in pieces is reassembled or the gate fails.
fn host_text() -> String {
    let mut text = String::new();
    let mut n = 0;
    while text.len() < 3 * chunk::MAX_PAYLOAD {
        text.push_str(&format!("host line {n} crossed to the guest; "));
        n += 1;
    }
    text
}

/// What the compositor starts: the agent, and the paste then the copy.
fn config() -> String {
    format!(
        "# Carried into the initramfs by `cargo xtask test-clipboard`.\n\
         exec-once = /bin/vdagent\n\
         exec-once = /bin/zinc -c '/bin/clip paste; /bin/clip copy {GUEST_TEXT}'\n"
    )
}

/// Run the gate on x86-64 and AArch64; ARMv7-A has no clipboard port.
///
/// # Errors
///
/// A build that failed, a boot that did not come up, or either direction not
/// carrying its text.
pub(crate) fn test_clipboard(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if arch == Arch::Armv7a {
            println!("  {arch}: no clipboard port on ARMv7-A (docs/CLIPBOARD.md §3.1); skipped");
            continue;
        }
        one(arch, args)?;
    }
    Ok(())
}

fn one(arch: Arch, args: &Args) -> Result<()> {
    let host = host_text();
    let (image, kernel) = compositor::desktop_image(arch, &config(), args)?;
    // In the temporary directory: a Unix socket's path must fit in 108
    // bytes, which a worktree's build directory does not. Named by process
    // and architecture, so two gates at once do not meet.
    let socket = std::env::temp_dir().join(format!(
        "ferrix-clipboard-{}-{arch}.sock",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)
        .map_err(|error| Error::new(format!("binding {}: {error}", socket.display())))?;

    let mut args = args.clone();
    args.clipboard = true;
    args.display = true;
    args.clipboard_socket = Some(socket.clone());

    // The host half runs beside the boot: QEMU connects as it starts.
    let (tell, told) = mpsc::channel();
    let deadline = Instant::now() + Duration::from_secs(args.timeout) + PATIENCE;
    let text = host.clone().into_bytes();
    let _host_half = std::thread::spawn(move || {
        let _ = tell.send(host_half(&listener, deadline, &text));
    });

    let pasted = format!("clip: pasted {host}");
    let mut guest_text: Option<std::result::Result<Vec<u8>, String>> = None;
    let lines = qemu::watch_then(
        arch,
        &image,
        &kernel,
        &args,
        compositor::MARKER,
        |watching| {
            let until = Instant::now() + PATIENCE;
            let _ = watching.read_more(until, |lines| {
                lines.iter().any(|line| line.contains(&pasted))
            })?;
            let wait = until.saturating_duration_since(Instant::now());
            guest_text = Some(
                told.recv_timeout(wait)
                    .unwrap_or_else(|_| Err("the host half heard nothing in time".to_owned())),
            );
            Ok(())
        },
    );
    let _ = std::fs::remove_file(&socket);
    let lines = lines?;

    let log = paths::build_dir(arch).join("serial.log");
    // The desktop `run-compositor` boots: `devmgr`, and so the port's driver,
    // started before `/` moves onto the root volume, and the agent after.
    if !lines.iter().any(|line| line.contains(SWITCHED)) {
        return Err(Error::new(format!(
            "{arch}: the boot never moved pid 1 onto the root volume, so the port's driver and \
             the agent were not on two sides of the switch.\n  Serial output is in {}",
            log.display()
        )));
    }
    if !lines.iter().any(|line| line.contains(&pasted)) {
        return Err(Error::new(format!(
            "{arch}: the guest's `clip paste` never printed the host's {} bytes.\n  Serial output is in {}",
            host.len(),
            log.display()
        )));
    }
    println!(
        "  {arch}: the host's clipboard, {} bytes in {} chunks, reached the guest's `clip paste`",
        host.len(),
        host.len().div_ceil(chunk::MAX_PAYLOAD)
    );
    match guest_text {
        Some(Ok(text)) if text == GUEST_TEXT.as_bytes() => {
            println!("  {arch}: the guest's `clip copy` reached the host, grab, request and data");
            Ok(())
        }
        Some(Ok(text)) => Err(Error::new(format!(
            "{arch}: the host was given {:?}, not {GUEST_TEXT:?}",
            String::from_utf8_lossy(&text)
        ))),
        Some(Err(why)) => Err(Error::new(format!("{arch}: the host half: {why}"))),
        None => Err(Error::new(format!("{arch}: the compositor never came up"))),
    }
}

/// The host's side of vdagent: say the capabilities, grab with `text` and
/// serve it, then take the guest's grab and bring its text back.
fn host_half(
    listener: &UnixListener,
    deadline: Instant,
    text: &[u8],
) -> std::result::Result<Vec<u8>, String> {
    let stream = accept(listener, deadline)?;
    let mut host = Host {
        stream,
        shape: Shape::QEMU_CLIPBOARD,
        text,
        grabbed: false,
    };
    host.send(&Message::AnnounceCapabilities {
        request: true,
        caps: HOST_CAPS,
    })?;
    let mut buffer = vec![0_u8; 2 * 1024 * 1024];
    let mut reassembler = Reassembler::new(&mut buffer);
    let mut pending: Vec<u8> = Vec::new();
    let mut read = [0_u8; 4096];
    while Instant::now() < deadline {
        match host.stream.read(&mut read) {
            Ok(0) => return Err("the guest closed the port".to_owned()),
            Ok(got) => pending.extend_from_slice(read.get(..got).unwrap_or_default()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(format!("reading: {error}")),
        }
        while !pending.is_empty() {
            let taken = reassembler
                .feed(&pending)
                .map_err(|error| format!("the guest's framing: {error:?}"))?;
            if taken == 0 {
                break;
            }
            let _ = pending.drain(..taken);
            let Some(message) = reassembler.message().map(<[u8]>::to_vec) else {
                continue;
            };
            reassembler.take();
            if let Some(guest) = host.answer(&message)? {
                return Ok(guest);
            }
        }
    }
    Err("the guest's text never came".to_owned())
}

/// QEMU's connection, waited for until `deadline`.
fn accept(listener: &UnixListener, deadline: Instant) -> std::result::Result<UnixStream, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err("QEMU never connected to the clipboard socket".to_owned());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(format!("accepting: {error}")),
        }
    };
    stream
        .set_nonblocking(false)
        .and_then(|()| stream.set_read_timeout(Some(Duration::from_millis(200))))
        .map_err(|error| error.to_string())?;
    Ok(stream)
}

/// The host half's connection and what it has done on it.
struct Host<'a> {
    stream: UnixStream,
    shape: Shape,
    /// What the host copies.
    text: &'a [u8],
    /// Whether the host has grabbed its clipboard yet.
    grabbed: bool,
}

impl Host<'_> {
    /// Answer one whole message; the guest's text once it has come.
    fn answer(&mut self, message: &[u8]) -> std::result::Result<Option<Vec<u8>>, String> {
        let Ok(message) = Message::decode(message, self.shape) else {
            return Ok(None);
        };
        match message {
            Message::AnnounceCapabilities { request, .. } => {
                if request {
                    self.send(&Message::AnnounceCapabilities {
                        request: false,
                        caps: HOST_CAPS,
                    })?;
                }
                if !self.grabbed {
                    self.grabbed = true;
                    let types = Types::new(&[ClipboardType::Utf8Text])
                        .map_err(|error| format!("{error:?}"))?;
                    self.send(&Message::ClipboardGrab {
                        selection: Selection::Clipboard,
                        serial: Some(HOST_SERIAL),
                        types,
                    })?;
                }
            }
            Message::ClipboardRequest {
                selection: Selection::Clipboard,
                kind: ClipboardType::Utf8Text,
            } => {
                let text = self.text;
                self.send(&Message::Clipboard {
                    selection: Selection::Clipboard,
                    kind: ClipboardType::Utf8Text,
                    data: text,
                })?;
            }
            Message::ClipboardGrab {
                selection: Selection::Clipboard,
                serial,
                ..
            } if serial.unwrap_or(0) < HOST_SERIAL => {
                // What QEMU does with it (`vdagent_clipboard_recv_grab`): a
                // grab older than the last one is the loser of a race, and
                // is dropped without a word.
                return Err(format!(
                    "the guest grabbed with serial {}, below the host's {HOST_SERIAL}; QEMU would discard it",
                    serial.unwrap_or(0)
                ));
            }
            Message::ClipboardGrab {
                selection: Selection::Clipboard,
                types,
                ..
            } if types.holds(ClipboardType::Utf8Text) => {
                self.send(&Message::ClipboardRequest {
                    selection: Selection::Clipboard,
                    kind: ClipboardType::Utf8Text,
                })?;
            }
            Message::Clipboard {
                selection: Selection::Clipboard,
                kind: ClipboardType::Utf8Text,
                data,
            } => return Ok(Some(data.to_vec())),
            _ => {}
        }
        Ok(None)
    }

    fn send(&mut self, message: &Message<'_>) -> std::result::Result<(), String> {
        send(&mut self.stream, self.shape, message)
    }
}

/// Encode, frame and write one message.
fn send(
    stream: &mut UnixStream,
    shape: Shape,
    message: &Message<'_>,
) -> std::result::Result<(), String> {
    let mut body = vec![0_u8; message.encoded_len(shape)];
    let written = message
        .encode(shape, &mut body)
        .map_err(|error| format!("encoding: {error:?}"))?;
    let mut framed = vec![0_u8; chunk::framed_len(written)];
    let len = chunk::frame(body.get(..written).unwrap_or_default(), &mut framed)
        .map_err(|error| format!("framing: {error:?}"))?;
    stream
        .write_all(framed.get(..len).unwrap_or_default())
        .map_err(|error| format!("writing: {error}"))
}
