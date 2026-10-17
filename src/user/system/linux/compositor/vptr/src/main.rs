//! `vptr <hz> <seconds> [<pixels>] [--log FILE]`: move the pointer back and
//! forth through `zwlr_virtual_pointer_v1`, so the compositor sends relative
//! motion to the focused client as a mouse would. hyprix and sway both serve
//! the protocol, so the same program injects on Ferrix and on Linux.
//!
//! It is the injector of stage 24's latency probe (docs/GAME-PATH.md §2.4,
//! R2): with `--log` it writes, when it is done, one line a motion,
//! `vptr: send <n> <ns>`, the motion's number from 1 and the
//! `CLOCK_MONOTONIC` time just before it was written to the socket, the
//! clock X's Present gives its completions in. `xlatency summarize` pairs
//! those with the probe's frames. A rate that divides no refresh, such as 7,
//! spreads the motions over the refresh's phase.
//!
//! `vptr click <x> <y> <width> <height>`: put the pointer at `(x, y)` of a
//! `width` by `height` screen and click its left button, as a hand does --
//! through the compositor's seat, so the window under the point gets the
//! click where it is shown. xdotool's clicks go to X's positions, which are
//! not where hyprix shows an X window (docs/WINE.md §10).

use std::io::Write as _;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use compositor_protocol::core::{wl_display, wl_registry};
use compositor_protocol::virtual_pointer::{
    zwlr_virtual_pointer_manager_v1 as manager, zwlr_virtual_pointer_v1 as pointer,
};
use compositor_socket::{Connection, RecvError};
use compositor_wire::{Arg, ArgType, Fixed, ObjectId, Reader, Writer};

const DISPLAY: ObjectId = ObjectId(1);
const REGISTRY: ObjectId = ObjectId(2);
// New ids in the order they are made, with no gap: libwayland's server
// (sway's) refuses a bind whose id skips one, as hyprix does not.
const SEAT: ObjectId = ObjectId(3);
const MANAGER: ObjectId = ObjectId(4);
const POINTER: ObjectId = ObjectId(5);

/// `wl_registry.global`'s arguments: name, interface, version.
const GLOBAL: &[ArgType] = &[
    ArgType::Uint,
    ArgType::Str { nullable: false },
    ArgType::Uint,
];

const USAGE: &str =
    "usage: vptr <hz> <seconds> [<pixels>] [--log FILE] | vptr click <x> <y> <width> <height>";

/// Linux's `BTN_LEFT`, the button a click presses.
const BTN_LEFT: u32 = 0x110;

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq)]
struct Options {
    hz: u64,
    seconds: u64,
    step: f64,
    log: Option<String>,
}

/// The command line: three numbers in order, each with a default, and
/// `--log FILE` anywhere.
fn options(args: &[String]) -> Result<Options, String> {
    let mut numbers = Vec::new();
    let mut log = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        if arg == "--log" {
            log = Some(rest.next().ok_or(USAGE)?.clone());
        } else {
            numbers.push(arg.as_str());
        }
    }
    let number = |at: usize, default: f64| -> Result<f64, String> {
        numbers.get(at).map_or(Ok(default), |text| {
            text.parse().map_err(|_| USAGE.to_owned())
        })
    };
    let whole = |at: usize, default: u64| -> Result<u64, String> {
        numbers.get(at).map_or(Ok(default), |text| {
            text.parse().map_err(|_| USAGE.to_owned())
        })
    };
    if numbers.len() > 3 {
        return Err(USAGE.to_owned());
    }
    Ok(Options {
        hz: whole(0, 500)?.max(1),
        seconds: whole(1, 10)?,
        step: number(2, 2.0)?,
        log,
    })
}

/// `click`'s point and the screen's size it is on.
fn click_options(args: &[String]) -> Result<[u32; 4], String> {
    let numbers: Vec<u32> = args
        .iter()
        .map(|arg| arg.parse().map_err(|_| USAGE.to_owned()))
        .collect::<Result<_, _>>()?;
    match numbers.as_slice() {
        [x, y, width, height] if x < width && y < height => Ok([*x, *y, *width, *height]),
        _ => Err(USAGE.to_owned()),
    }
}

/// Move to the point, then press and release the left button, each with
/// its frame.
fn click(at: [u32; 4]) -> Result<(), String> {
    let mut writer = Writer::new();
    let mut connection = connect(&mut writer)?;
    let pause = |millis| std::thread::sleep(Duration::from_millis(millis));
    let began = Instant::now();
    let time = || u32::try_from(began.elapsed().as_millis()).unwrap_or(0);
    let [x, y, width, height] = at;
    // Arrive a little above and to the left, then rest on the point: a
    // toolkit reads its first motion as the pointer entering.
    for (x, y) in [(x.saturating_sub(8), y.saturating_sub(8)), (x, y)] {
        write(
            &mut writer,
            POINTER,
            pointer::request::MOTION_ABSOLUTE,
            &[ArgType::Uint; 5],
            &[
                Arg::Uint(time()),
                Arg::Uint(x),
                Arg::Uint(y),
                Arg::Uint(width),
                Arg::Uint(height),
            ],
        )?;
        write(&mut writer, POINTER, pointer::request::FRAME, &[], &[])?;
        send(&mut connection, &mut writer)?;
        pause(200);
    }
    for state in [1, 0] {
        write(
            &mut writer,
            POINTER,
            pointer::request::BUTTON,
            &[ArgType::Uint; 3],
            &[Arg::Uint(time()), Arg::Uint(BTN_LEFT), Arg::Uint(state)],
        )?;
        write(&mut writer, POINTER, pointer::request::FRAME, &[], &[])?;
        send(&mut connection, &mut writer)?;
        pause(80);
    }
    // A round trip, so everything sent is read before the pointer goes.
    pause(200);
    let _ = connection.receive();
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("click") {
        let rest = args.get(1..).unwrap_or_default();
        return match click_options(rest).and_then(click) {
            Ok(()) => {
                say(&format!("vptr: clicked at {}", rest.join(" ")));
                ExitCode::SUCCESS
            }
            Err(error) => {
                say(&format!("vptr: failed: {error}"));
                ExitCode::FAILURE
            }
        };
    }
    let result = options(&args).and_then(|options| {
        let sent = run(&options)?;
        if let Some(path) = &options.log {
            write_log(path, &sent)?;
        }
        Ok((options, sent.len()))
    });
    match result {
        Ok((options, n)) => {
            say(&format!(
                "vptr: sent {n} motions at {} Hz for {} s",
                options.hz, options.seconds
            ));
            ExitCode::SUCCESS
        }
        Err(error) => {
            say(&format!("vptr: failed: {error}"));
            ExitCode::FAILURE
        }
    }
}

/// `CLOCK_MONOTONIC` in nanoseconds.
fn monotonic() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a valid, writable timespec for the call's duration.
    let _ = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
    let seconds = u64::try_from(now.tv_sec).unwrap_or(0);
    let nanos = u64::try_from(now.tv_nsec).unwrap_or(0);
    seconds.saturating_mul(1_000_000_000).saturating_add(nanos)
}

/// The log's lines, one a motion.
fn log_text(sent: &[u64]) -> String {
    sent.iter()
        .zip(1_u64..)
        .map(|(at, n)| format!("vptr: send {n} {at}\n"))
        .collect()
}

fn write_log(path: &str, sent: &[u64]) -> Result<(), String> {
    std::fs::write(path, log_text(sent)).map_err(|error| format!("writing {path}: {error}"))
}

/// Connect, find the manager and the seat, and make the virtual pointer.
fn connect(writer: &mut Writer) -> Result<Connection, String> {
    let display =
        std::env::var("WAYLAND_DISPLAY").map_err(|_| "WAYLAND_DISPLAY is not set".to_owned())?;
    let path =
        compositor_socket::socket_path(&display).map_err(|error| format!("the socket: {error}"))?;
    let stream = std::os::unix::net::UnixStream::connect(&path)
        .map_err(|error| format!("connecting: {error}"))?;
    let mut connection =
        Connection::new(stream).map_err(|error| format!("the connection: {error}"))?;
    write(
        writer,
        DISPLAY,
        wl_display::request::GET_REGISTRY,
        &[ArgType::NewId],
        &[Arg::NewId(REGISTRY)],
    )?;
    send(&mut connection, writer)?;
    let (manager_name, seat_name) = globals(&mut connection)?;
    bind(writer, seat_name, "wl_seat", 7, SEAT)?;
    bind(
        writer,
        manager_name,
        "zwlr_virtual_pointer_manager_v1",
        1,
        MANAGER,
    )?;
    write(
        writer,
        MANAGER,
        manager::request::CREATE_VIRTUAL_POINTER,
        &[ArgType::Object { nullable: true }, ArgType::NewId],
        &[Arg::Object(SEAT), Arg::NewId(POINTER)],
    )?;
    send(&mut connection, writer)?;
    Ok(connection)
}

/// The registry's names for the virtual pointer manager and the seat.
fn globals(connection: &mut Connection) -> Result<(u32, u32), String> {
    let (mut manager_name, mut seat_name) = (None, None);
    let started = Instant::now();
    loop {
        if let (Some(manager), Some(seat)) = (manager_name, seat_name) {
            return Ok((manager, seat));
        }
        if started.elapsed() > Duration::from_secs(10) {
            return Err("no virtual pointer manager or seat".to_owned());
        }
        read(connection)?;
        let bytes = connection.bytes().to_vec();
        let descriptors = connection.fds();
        let mut reader = Reader::new(&bytes, &descriptors);
        while let Ok(header) = reader.peek() {
            if header.sender != REGISTRY || header.opcode != wl_registry::event::GLOBAL {
                if reader.skip(0).is_err() {
                    break;
                }
                continue;
            }
            let Ok((_, args)) = reader.read(GLOBAL) else {
                break;
            };
            let name = args.first().and_then(Arg::as_uint);
            match args.get(1).and_then(Arg::as_str) {
                Some("zwlr_virtual_pointer_manager_v1") => manager_name = name,
                Some("wl_seat") => seat_name = name,
                _ => {}
            }
        }
        let taken = reader.consumed();
        connection.consume(taken, 0);
    }
}

/// One motion by `dx` and its frame, queued.
fn motion(writer: &mut Writer, time: u32, dx: f64) -> Result<(), String> {
    write(
        writer,
        POINTER,
        pointer::request::MOTION,
        &[ArgType::Uint, ArgType::Fixed, ArgType::Fixed],
        &[
            Arg::Uint(time),
            Arg::Fixed(Fixed::from_f64(dx)),
            Arg::Fixed(Fixed::from_f64(0.0)),
        ],
    )?;
    write(writer, POINTER, pointer::request::FRAME, &[], &[])
}

/// Send the motions, and say when each went.
///
/// With a log, one motion goes first, unlogged, a period before the rest:
/// the first motion of a virtual pointer brings the pointer into the
/// window, and sway's Xwayland makes no raw motion of it, so the log and
/// the probe's events then pair one to one.
fn run(options: &Options) -> Result<Vec<u64>, String> {
    let mut writer = Writer::new();
    let mut connection = connect(&mut writer)?;
    let period = Duration::from_micros(1_000_000 / options.hz);
    if options.log.is_some() {
        motion(&mut writer, 0, -options.step)?;
        send(&mut connection, &mut writer)?;
        std::thread::sleep(period);
    }
    let began = Instant::now();
    let mut sent = Vec::new();
    let mut next = began;
    while began.elapsed() < Duration::from_secs(options.seconds) {
        // 64 steps one way, 64 back: the pointer stays where it was.
        let count = u64::try_from(sent.len()).unwrap_or(0);
        let dx = if (count / 64) % 2 == 0 {
            options.step
        } else {
            -options.step
        };
        let time = u32::try_from(began.elapsed().as_millis()).unwrap_or(0);
        motion(&mut writer, time, dx)?;
        sent.push(monotonic());
        send(&mut connection, &mut writer)?;
        next += period;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let _ = connection.receive();
        let n = connection.bytes().len();
        connection.consume(n, 0);
    }
    Ok(sent)
}

/// `wl_registry.bind`.
fn bind(
    writer: &mut Writer,
    name: u32,
    interface: &str,
    version: u32,
    id: ObjectId,
) -> Result<(), String> {
    write(
        writer,
        REGISTRY,
        wl_registry::request::BIND,
        &[ArgType::Uint, ArgType::AnyNewId],
        &[
            Arg::Uint(name),
            Arg::AnyNewId {
                interface,
                version,
                id,
            },
        ],
    )
}

/// One request into the outgoing buffer.
fn write(
    writer: &mut Writer,
    sender: ObjectId,
    opcode: u16,
    signature: &'static [ArgType],
    args: &[Arg<'_>],
) -> Result<(), String> {
    writer
        .write(sender, opcode, signature, args)
        .map_err(|error| format!("writing a request: {error:?}"))
}

/// Send what is queued.
fn send(connection: &mut Connection, writer: &mut Writer) -> Result<(), String> {
    let bytes = writer.bytes().to_vec();
    writer.clear();
    connection
        .send(&bytes, &[])
        .map_err(|error| format!("sending: {error:?}"))
}

/// Wait for something to arrive.
fn read(connection: &mut Connection) -> Result<(), String> {
    match connection.receive() {
        Ok(_) | Err(RecvError::WouldBlock) => Ok(()),
        Err(RecvError::Closed) => Err("the compositor closed the connection".to_owned()),
        Err(error) => Err(format!("reading: {error:?}")),
    }
}

/// Say a line where the compositor's own log goes.
fn say(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn the_numbers_keep_their_order_and_the_log_goes_anywhere() {
        let parsed = options(&strings(&["7", "--log", "/tmp/s", "12"]));
        assert_eq!(
            parsed,
            Ok(Options {
                hz: 7,
                seconds: 12,
                step: 2.0,
                log: Some("/tmp/s".to_owned()),
            })
        );
        assert_eq!(options(&[]).map(|o| (o.hz, o.seconds)), Ok((500, 10)));
        assert!(options(&strings(&["--log"])).is_err());
        assert!(options(&strings(&["x"])).is_err());
    }

    #[test]
    fn a_click_is_a_point_on_its_screen() {
        assert_eq!(
            click_options(&strings(&["10", "20", "1920", "1080"])),
            Ok([10, 20, 1920, 1080])
        );
        assert!(click_options(&strings(&["1920", "20", "1920", "1080"])).is_err());
        assert!(click_options(&strings(&["10", "20", "1920"])).is_err());
        assert!(click_options(&strings(&["x", "20", "1920", "1080"])).is_err());
    }

    #[test]
    fn the_log_numbers_each_motion_from_one() {
        assert_eq!(log_text(&[5, 9]), "vptr: send 1 5\nvptr: send 2 9\n");
    }
}
