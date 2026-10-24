//! The loop goes on while a card shows a frame.
//!
//! On Ferrix the kernel answers `PAGE_FLIP` once the card's driver has shown
//! the frame: the Intel driver's card0 in its next vertical blank, 7 to 17 ms
//! later, and a stuck one only when the kernel gives up after five seconds.
//! Made on the compositor's loop, every one of those waits was the pointer's
//! and the keyboard's too: on the N150 the loop sat in `card0` 23 % of a
//! game's map load, in runs of up to 1.3 s, and 5 s a frame while its driver
//! was stuck. So flips go to a thread of their own and the loop hears of each
//! through its wait (`src/flush.rs`, `FlipThread`).
//!
//! Here a headless screen plays such a card: its flips take 50 ms on that
//! thread, or its first hangs and then times out. A virtual pointer moves
//! the pointer as a mouse does -- through the seat, as a person's input --
//! and `hyprctl cursorpos` says when the move was carried out. Every move
//! makes a frame, and the next move comes while that frame's flip is out.
//!
//! `HYPRIX_SYNC_FLIP=1` makes the flips on the loop as before, and these
//! tests then fail: that is their negative control.

#![expect(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "a test's fixtures should fail loudly, and the workspace's ban is about the compositor"
)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use compositor_protocol::core::{wl_display, wl_registry};
use compositor_protocol::virtual_pointer::{
    zwlr_virtual_pointer_manager_v1 as manager, zwlr_virtual_pointer_v1 as pointer,
};
use compositor_socket::{Connection, RecvError};
use compositor_wire::{Arg, ArgType, ObjectId, Reader, Writer};
use hyprix::Options;

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;

/// How long a flip of the slow card takes.
const FLIP: Duration = Duration::from_millis(50);

/// How long the stuck card's first flip hangs before it times out.
const HANG: Duration = Duration::from_secs(3);

/// The longest a move may take to be carried out while a flip is out, in
/// the median of a run: a few milliseconds, against the 50 ms of a flip
/// the loop waited for.
const PROMPT: Duration = Duration::from_millis(15);

/// The longest any wait here lasts before it fails the test.
const PATIENCE: Duration = Duration::from_secs(60);

/// The compositor's own deadline, past every wait.
const BACKSTOP: Duration = Duration::from_secs(120);

/// One test's compositor: its directory, removed however the test ends, and
/// its sockets.
struct Desk {
    work: PathBuf,
    socket: PathBuf,
    control: PathBuf,
    /// Everything the compositor said, as it said it.
    said: Arc<Mutex<Vec<String>>>,
}

impl Drop for Desk {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.work);
    }
}

impl Desk {
    fn new(name: &str) -> Self {
        let work = std::env::temp_dir().join(format!("hyprix-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        let instance = work.join("runtime").join("hypr").join("ferrix-test");
        std::fs::create_dir_all(&instance).expect("an instance directory");
        Self {
            socket: work.join("wayland"),
            control: instance.join(compositor_ipc::REQUEST_SOCKET),
            work,
            said: Arc::default(),
        }
    }

    /// Run the compositor on a headless screen whose flips take `takes`
    /// and whose first hangs `hangs`, and `test` beside it once its sockets
    /// are bound. Gives the compositor's last line and what `test` gave.
    fn run<T>(
        &self,
        takes: Duration,
        hangs: Option<Duration>,
        test: impl FnOnce(&Self) -> T,
    ) -> (String, T) {
        self.run_configured("", takes, hangs, test)
    }

    /// [`Desk::run`] with `lines` as the compositor's configuration.
    fn run_configured<T>(
        &self,
        lines: &str,
        takes: Duration,
        hangs: Option<Duration>,
        test: impl FnOnce(&Self) -> T,
    ) -> (String, T) {
        let config = self.work.join("hyprland.conf");
        std::fs::write(&config, lines).expect("a configuration");
        let instance = self
            .control
            .parent()
            .expect("the instance directory")
            .to_string_lossy()
            .into_owned();
        let options = Options {
            display: self.socket.to_string_lossy().into_owned(),
            headless: Some((WIDTH, HEIGHT)),
            instance: Some(instance),
            deadline: Some(u64::try_from(BACKSTOP.as_millis()).expect("milliseconds")),
            config: Some(config),
            flip_takes: Some(takes),
            flip_hangs: hangs,
            ..Options::default()
        };
        let said = Arc::clone(&self.said);
        let compositor = std::thread::spawn(move || {
            hyprix::run_with(&options, &mut |line| {
                said.lock().expect("the log").push(line.to_owned());
            })
        });
        until("the compositor's sockets", || {
            (self.socket.exists() && self.control.exists()).then_some(())
        });
        let given = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(self)));
        let _ = compositor_ctl::ask(&self.control, "dispatch exit");
        let line = compositor
            .join()
            .expect("the compositor's thread")
            .expect("the compositor ran");
        match given {
            Ok(given) => (line, given),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    /// Whether the compositor has said a line containing `text`.
    fn has_said(&self, text: &str) -> bool {
        self.said
            .lock()
            .expect("the log")
            .iter()
            .any(|line| line.contains(text))
    }

    /// Move the pointer to `at` with `pointer`, and give how long it was
    /// until `hyprctl cursorpos` said it was there.
    fn carried_out(&self, pointer: &mut VirtualPointer, at: (u32, u32)) -> Duration {
        let began = Instant::now();
        pointer.move_to(at);
        loop {
            let answer = compositor_ctl::ask(&self.control, "cursorpos").unwrap_or_default();
            // A place sent as a fraction of the screen comes back to its
            // pixel or the one beside it.
            let there = answer
                .trim()
                .split_once(", ")
                .and_then(|(x, y)| Some((x.parse::<i64>().ok()?, y.parse::<i64>().ok()?)))
                .is_some_and(|(x, y)| {
                    (x - i64::from(at.0)).abs() <= 1 && (y - i64::from(at.1)).abs() <= 1
                });
            if there {
                return began.elapsed();
            }
            assert!(
                began.elapsed() < PATIENCE,
                "the move to {at:?} was never carried out: {answer:?}"
            );
            std::thread::sleep(Duration::from_micros(300));
        }
    }
}

/// Ask `look` until it gives `Some`, failing the test after [`PATIENCE`].
fn until<T>(what: &str, mut look: impl FnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(found) = look() {
            return found;
        }
        assert!(started.elapsed() < PATIENCE, "never saw {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Say `line` where `cargo test --nocapture` shows it.
fn say(line: &str) {
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr(), "{line}");
}

/// The middle of `took`.
fn median(took: &[Duration]) -> Duration {
    let mut sorted = took.to_vec();
    sorted.sort();
    sorted[sorted.len() / 2]
}

/// The frames the compositor's last line says it drew.
fn frames(line: &str) -> u32 {
    line.split_once(" frames ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("no frame count in {line:?}"))
}

/// The `n`th place the pointer is sent to: each a new one.
fn place(n: u32) -> (u32, u32) {
    (40 + (n * 13) % (WIDTH - 80), 40 + (n * 7) % (HEIGHT - 80))
}

/// With every flip taking 50 ms, a move that comes while one is out is
/// carried out within a few milliseconds, not when the flip returns; and
/// the frames go on being drawn, one a flip.
#[test]
fn input_is_carried_out_while_a_slow_card_shows_a_frame() {
    const MOVES: u32 = 30;
    let desk = Desk::new("flip-slow");
    let (line, (took, spent)) = desk.run(FLIP, None, |desk| {
        let mut pointer = VirtualPointer::connect(&desk.socket);
        // The first frame is out, or about to be.
        let _ = desk.carried_out(&mut pointer, place(0));
        let started = Instant::now();
        let took: Vec<Duration> = (1..=MOVES)
            .map(|n| {
                // Long enough for the last move's frame to be drawn and
                // handed over, not for its flip to come back.
                std::thread::sleep(Duration::from_millis(20));
                desk.carried_out(&mut pointer, place(n))
            })
            .collect();
        (took, started.elapsed())
    });
    let middle = median(&took);
    say(&format!(
        "moves carried out in {took:?}, median {middle:?}, all in {spent:?}; {line}"
    ));
    assert!(
        middle < PROMPT,
        "a move waited for the flip out: median {middle:?} of {took:?}"
    );
    // Made on the loop, each move after the first waited out the flip the
    // one before it set going: 20 ms of sleep and the rest of a 50 ms flip.
    assert!(
        spent < FLIP * MOVES,
        "the moves took {spent:?}, as long as their flips"
    );
    assert!(
        frames(&line) >= 5,
        "the frames stopped while the moves went on: {line}"
    );
}

/// A card whose flip hangs for seconds and then times out holds no move
/// up; the frame is dropped as before, and the next ones are drawn.
#[test]
fn input_is_carried_out_while_a_stuck_card_hangs_a_flip() {
    let desk = Desk::new("flip-hung");
    let (line, (during, after)) = desk.run(FLIP, Some(HANG), |desk| {
        let mut pointer = VirtualPointer::connect(&desk.socket);
        // The first frame is handed over, and hangs, as soon as the
        // compositor is up; its line says it was drawn.
        let hung = Instant::now();
        let mut during = Vec::new();
        let mut n = 0;
        while hung.elapsed() < HANG / 2 {
            n += 1;
            during.push(desk.carried_out(&mut pointer, place(n)));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !desk.has_said("dropped a frame"),
            "the hung flip came back before the moves were made"
        );
        until("the hung frame dropped", || {
            desk.has_said("dropped a frame").then_some(())
        });
        n += 1;
        let after = desk.carried_out(&mut pointer, place(n));
        // The move owes a frame, drawn once the hold after the dropped one
        // is over: the line a second the frames are counted in says so.
        until("a frame after the hung one", || {
            desk.has_said("hyprix: frames ").then_some(())
        });
        (during, after)
    });
    let middle = median(&during);
    let longest = during.iter().max().copied().unwrap_or_default();
    say(&format!(
        "moves during the hang carried out in {during:?}, after it in {after:?}; {line}"
    ));
    assert!(during.len() >= 10, "too few moves made during the hang");
    assert!(
        middle < PROMPT && longest < HANG / 3,
        "a move waited for the hung flip: median {middle:?}, longest {longest:?}"
    );
    assert!(
        desk.has_said("Connection timed out") || desk.has_said("timed out"),
        "the hung flip's time-out was not said"
    );
    assert!(
        frames(&line) >= 2,
        "no frame was drawn after the hung one: {line}"
    );
}

/// A move that comes while the loop sleeps with nothing to do -- no frame
/// owed, no flip out -- is carried out at once, without a second event to
/// wake it: the input's descriptor is in the wait.
#[test]
fn an_idle_loop_carries_out_input_at_once() {
    const MOVES: u32 = 10;
    let desk = Desk::new("flip-idle-input");
    let (line, took) = desk.run(FLIP, None, |desk| {
        let mut pointer = VirtualPointer::connect(&desk.socket);
        (1..=MOVES)
            .map(|n| {
                // Long past the last move's frame and its flip.
                std::thread::sleep(FLIP * 4);
                desk.carried_out(&mut pointer, place(n))
            })
            .collect::<Vec<_>>()
    });
    let middle = median(&took);
    say(&format!(
        "moves into an idle loop carried out in {took:?}, median {middle:?}; {line}"
    ));
    assert!(
        middle < PROMPT,
        "a move into an idle loop waited: median {middle:?} of {took:?}"
    );
}

/// A frame the loop owes itself by the clock -- `debug:overlay`'s every
/// 200 ms, with nothing else happening -- is drawn when it is due, flip
/// out or not: the timer is in the wait's timeout, and a landed flip wakes
/// it through its pipe.
#[test]
fn an_idle_loop_wakes_for_its_own_timer() {
    const IDLE: Duration = Duration::from_secs(2);
    let desk = Desk::new("flip-idle-timer");
    let (_, counted) = desk.run_configured("debug:overlay = 1\n", FLIP, None, |desk| {
        // The first frame is up; nothing more is sent from here on.
        until("the first frame", || {
            desk.has_said("headless 640x480").then_some(())
        });
        std::thread::sleep(IDLE);
        desk.said
            .lock()
            .expect("the log")
            .iter()
            .filter_map(|line| line.strip_prefix("hyprix: frames "))
            .filter_map(|rest| rest.split_whitespace().next()?.parse::<u32>().ok())
            .max()
            .unwrap_or(0)
    });
    // Ten due in two seconds, and the line a second counts them.
    assert!(
        counted >= 5,
        "the overlay's timer drew {counted} frames in {IDLE:?} of an idle loop"
    );
}

// ---------------------------------------------------------------------------
// A virtual pointer, as `vptr` is one (`../vptr`)
// ---------------------------------------------------------------------------

const DISPLAY: ObjectId = ObjectId(1);
const REGISTRY: ObjectId = ObjectId(2);
const SEAT: ObjectId = ObjectId(3);
const MANAGER: ObjectId = ObjectId(4);
const POINTER: ObjectId = ObjectId(5);

/// `wl_registry.global`'s arguments: name, interface, version.
const GLOBAL: &[ArgType] = &[
    ArgType::Uint,
    ArgType::Str { nullable: false },
    ArgType::Uint,
];

/// A `zwlr_virtual_pointer_v1` on the compositor's seat.
struct VirtualPointer {
    connection: Connection,
    writer: Writer,
    began: Instant,
}

impl VirtualPointer {
    fn connect(socket: &Path) -> Self {
        let stream = std::os::unix::net::UnixStream::connect(socket).expect("the Wayland socket");
        let mut this = Self {
            connection: Connection::new(stream).expect("a connection"),
            writer: Writer::new(),
            began: Instant::now(),
        };
        this.write(
            DISPLAY,
            wl_display::request::GET_REGISTRY,
            &[ArgType::NewId],
            &[Arg::NewId(REGISTRY)],
        );
        this.send();
        let (manager_name, seat_name) = this.globals();
        this.bind(seat_name, "wl_seat", 7, SEAT);
        this.bind(manager_name, "zwlr_virtual_pointer_manager_v1", 1, MANAGER);
        this.write(
            MANAGER,
            manager::request::CREATE_VIRTUAL_POINTER,
            &[ArgType::Object { nullable: true }, ArgType::NewId],
            &[Arg::Object(SEAT), Arg::NewId(POINTER)],
        );
        this.send();
        this
    }

    /// Put the pointer at `at` on the screen, and say the move is whole.
    fn move_to(&mut self, (x, y): (u32, u32)) {
        let time = u32::try_from(self.began.elapsed().as_millis()).unwrap_or(0);
        self.write(
            POINTER,
            pointer::request::MOTION_ABSOLUTE,
            &[ArgType::Uint; 5],
            &[
                Arg::Uint(time),
                Arg::Uint(x),
                Arg::Uint(y),
                Arg::Uint(WIDTH),
                Arg::Uint(HEIGHT),
            ],
        );
        self.write(POINTER, pointer::request::FRAME, &[], &[]);
        self.send();
        // Whatever the compositor sent, read and forgotten, so its side of
        // the socket never fills.
        let _ = self.connection.receive();
        let n = self.connection.bytes().len();
        self.connection.consume(n, 0);
    }

    fn globals(&mut self) -> (u32, u32) {
        let (mut manager_name, mut seat_name) = (None, None);
        until("the virtual pointer's globals", || {
            if let (Some(manager), Some(seat)) = (manager_name, seat_name) {
                return Some((manager, seat));
            }
            match self.connection.receive() {
                Ok(_) | Err(RecvError::WouldBlock) => {}
                Err(error) => panic!("reading the globals: {error:?}"),
            }
            let bytes = self.connection.bytes().to_vec();
            let descriptors = self.connection.fds();
            let taken = globals_in(&bytes, &descriptors, &mut manager_name, &mut seat_name);
            self.connection.consume(taken, 0);
            None
        })
    }

    fn bind(&mut self, name: u32, interface: &str, version: u32, id: ObjectId) {
        self.write(
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
        );
    }

    fn write(
        &mut self,
        sender: ObjectId,
        opcode: u16,
        signature: &'static [ArgType],
        args: &[Arg<'_>],
    ) {
        self.writer
            .write(sender, opcode, signature, args)
            .expect("a request");
    }

    fn send(&mut self) {
        let bytes = self.writer.bytes().to_vec();
        self.writer.clear();
        self.connection.send(&bytes, &[]).expect("sending");
    }
}

/// The virtual pointer manager's and the seat's names among the globals in
/// `bytes`, kept where found; gives how many bytes were read.
fn globals_in(
    bytes: &[u8],
    descriptors: &[compositor_wire::Fd],
    manager_name: &mut Option<u32>,
    seat_name: &mut Option<u32>,
) -> usize {
    let mut reader = Reader::new(bytes, descriptors);
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
            Some("zwlr_virtual_pointer_manager_v1") => *manager_name = name,
            Some("wl_seat") => *seat_name = name,
            _ => {}
        }
    }
    reader.consumed()
}
