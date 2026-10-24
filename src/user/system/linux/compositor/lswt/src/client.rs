//! The Wayland side of listing and acting on windows.
//!
//! The objects a taskbar needs and no others:
//! `zwlr_foreign_toplevel_manager_v1`, bound from the registry, and the
//! `zwlr_foreign_toplevel_handle_v1` the compositor makes for each window.
//! No surface, no buffer, no seat -- a program that lists the windows does
//! not need one of its own.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use compositor_protocol::core::{self, wl_display, wl_registry};
use compositor_protocol::foreign_toplevel::{
    self, zwlr_foreign_toplevel_handle_v1 as handle, zwlr_foreign_toplevel_manager_v1 as manager,
};
use compositor_socket::{Connection, RecvError};
use compositor_wire::{Arg, ArgType, Interface, ObjectId, Reader, Writer};

/// The objects this client makes, at fixed ids.
mod id {
    use compositor_wire::ObjectId;

    pub(super) const DISPLAY: ObjectId = ObjectId(1);
    pub(super) const REGISTRY: ObjectId = ObjectId(2);
    pub(super) const SYNC: ObjectId = ObjectId(3);
    pub(super) const MANAGER: ObjectId = ObjectId(4);
    /// The roundtrip after the manager is bound, which is what says the
    /// compositor has finished sending the windows it already had.
    pub(super) const SETTLED: ObjectId = ObjectId(5);
    /// The seat, which only `activate` needs.
    pub(super) const SEAT: ObjectId = ObjectId(6);
}

/// One window, as the compositor described it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Toplevel {
    /// Its application id.
    pub app_id: String,
    /// Its title.
    pub title: String,
    /// Whether it is the focused window.
    pub activated: bool,
    /// Whether it is fullscreen.
    pub fullscreen: bool,
    /// Whether it says it is maximized.
    pub maximized: bool,
    /// Whether it says it is minimized.
    pub minimized: bool,
}

impl Toplevel {
    /// The line `lswt` prints for it.
    #[must_use]
    pub fn line(&self) -> String {
        let mut states = Vec::new();
        for (on, name) in [
            (self.activated, "activated"),
            (self.fullscreen, "fullscreen"),
            (self.maximized, "maximized"),
            (self.minimized, "minimized"),
        ] {
            if on {
                states.push(name);
            }
        }
        format!(
            "lswt: {} \"{}\" [{}]",
            self.app_id,
            self.title,
            states.join(" ")
        )
    }
}

/// What to do once the windows are known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    /// Print them.
    List,
    /// Focus the one whose title is this.
    Activate(String),
    /// Ask the one whose title is this to close.
    Close(String),
    /// Minimize the one whose title is this, as a taskbar's click on the
    /// active entry does.
    Minimize(String),
    /// Bring the minimized one whose title is this back, as a taskbar's
    /// click on a minimized entry does.
    Unminimize(String),
}

/// How long to wait for the compositor to say what it has.
const PATIENCE: Duration = Duration::from_secs(20);

/// How long to keep reading after a request with no answer to wait for.
const ASKED: Duration = Duration::from_secs(2);

/// Connect, take the list, and do `want` with it.
///
/// Gives the lines to print: one a window, and for a request one more
/// saying what was asked of which.
///
/// # Errors
///
/// A sentence saying what could not be done, including the compositor not
/// offering the protocol and no window having the title asked for.
pub fn run(socket: &std::path::Path, want: &Want) -> Result<Vec<String>, String> {
    let mut session = Session::connect(socket)?;
    session.run()?;
    let mut lines: Vec<String> = session.ordered().iter().map(Toplevel::line).collect();
    let title = match want {
        Want::List => return Ok(lines),
        Want::Activate(title)
        | Want::Close(title)
        | Want::Minimize(title)
        | Want::Unminimize(title) => title.clone(),
    };
    let handle = session
        .named(&title)
        .ok_or_else(|| format!("no window is called {title}"))?;
    let (opcode, did) = match want {
        Want::Activate(_) => (handle::request::ACTIVATE, "activated"),
        // `activate` takes the seat that is doing it; `close` takes nothing.
        Want::Close(_) => (handle::request::CLOSE, "closed"),
        Want::Minimize(_) => (handle::request::SET_MINIMIZED, "minimized"),
        Want::Unminimize(_) => (handle::request::UNSET_MINIMIZED, "unminimized"),
        Want::List => return Ok(lines),
    };
    session.ask(handle, opcode)?;
    match want {
        // A close is carried out by the window's own client, not by the
        // compositor: it asks the client, the client destroys the window, and
        // only then is `closed` sent here. That is three programs' turns, and
        // on ARMv7-A under TCG at two processors, with the compositor spending
        // a second and more on each frame, it took longer than the fixed two
        // seconds this used to read for, so the list printed afterwards still
        // named the window. The close is waited for instead, with the patience
        // a listing gets.
        Want::Close(_) => {
            session.until(|session| !session.windows.contains_key(&handle), PATIENCE)?;
        }
        // A minimize and its undoing are answered with the window's new
        // state, which is what is waited for.
        Want::Minimize(_) => {
            session.until(|session| session.is_minimized(handle), PATIENCE)?;
        }
        Want::Unminimize(_) => {
            session.until(|session| !session.is_minimized(handle), PATIENCE)?;
        }
        // An activation has no event of its own to wait for when the window
        // was already the active one.
        Want::Activate(_) | Want::List => session.until(|_| false, ASKED)?,
    }
    lines.push(format!("lswt: {did} \"{title}\""));
    // And what is left afterwards, which is what a taskbar redraws and what
    // says the request reached the window it named rather than another.
    let left: Vec<String> = session
        .ordered()
        .iter()
        .map(|window| format!("{:?}", window.title))
        .collect();
    lines.push(format!("lswt: left {}", left.join(" ")));
    Ok(lines)
}

/// One connection, listing what the compositor has.
struct Session {
    connection: Connection,
    out: Writer,
    globals: BTreeMap<String, (u32, u32)>,
    bound: bool,
    /// Whether the roundtrip after the bind has come back, which is when the
    /// compositor has sent every window it already had.
    settled: bool,
    /// The windows, by handle, and the order they arrived in.
    windows: BTreeMap<ObjectId, Toplevel>,
    order: Vec<ObjectId>,
    /// A window is not shown until its `done` has arrived, which is what
    /// makes the title, the id and the states one change rather than three.
    done: Vec<ObjectId>,
    /// The seat, for `activate`, which the protocol says names one.
    seat: Option<ObjectId>,
}

impl Session {
    /// Connect and ask for the registry.
    fn connect(socket: &std::path::Path) -> Result<Self, String> {
        let stream = std::os::unix::net::UnixStream::connect(socket)
            .map_err(|error| format!("connecting to {}: {error}", socket.display()))?;
        let connection =
            Connection::new(stream).map_err(|error| format!("the connection: {error}"))?;
        let mut out = Writer::new();
        request(
            &mut out,
            id::DISPLAY,
            wl_display::request::GET_REGISTRY,
            &[ArgType::NewId],
            &[Arg::NewId(id::REGISTRY)],
        );
        request(
            &mut out,
            id::DISPLAY,
            wl_display::request::SYNC,
            &[ArgType::NewId],
            &[Arg::NewId(id::SYNC)],
        );
        Ok(Self {
            connection,
            out,
            globals: BTreeMap::new(),
            bound: false,
            settled: false,
            windows: BTreeMap::new(),
            order: Vec::new(),
            done: Vec::new(),
            seat: None,
        })
    }

    /// The windows whose description is complete, in the order they arrived.
    fn ordered(&self) -> Vec<Toplevel> {
        self.order
            .iter()
            .filter(|handle| self.done.contains(handle))
            .filter_map(|handle| self.windows.get(handle).cloned())
            .collect()
    }

    /// The handle of the window called `title`, if there is one.
    fn named(&self, title: &str) -> Option<ObjectId> {
        self.order
            .iter()
            .find(|handle| {
                self.windows
                    .get(handle)
                    .is_some_and(|window| window.title == title)
            })
            .copied()
    }

    /// Send one request to a handle.
    /// Whether the window behind `handle` was last said to be minimized.
    fn is_minimized(&self, handle: ObjectId) -> bool {
        self.done.contains(&handle)
            && self
                .windows
                .get(&handle)
                .is_some_and(|window| window.minimized)
    }

    fn ask(&mut self, handle: ObjectId, opcode: u16) -> Result<(), String> {
        if opcode == handle::request::ACTIVATE {
            let seat = self
                .seat
                .ok_or_else(|| "the compositor offers no wl_seat".to_owned())?;
            request(
                &mut self.out,
                handle,
                opcode,
                &[ArgType::Object { nullable: false }],
                &[Arg::Object(seat)],
            );
        } else {
            request(&mut self.out, handle, opcode, &[], &[]);
        }
        self.flush()
    }

    /// Read what the compositor sends until `done` says what a request was
    /// waiting for has arrived, or `patience` has passed.
    ///
    /// The compositor has to read a request before this program leaves, and
    /// a socket closed with bytes still in flight is a request nobody ran.
    /// What is left is printed from what has arrived by the end of this.
    fn until(&mut self, done: impl Fn(&Self) -> bool, patience: Duration) -> Result<(), String> {
        let sent = Instant::now();
        while sent.elapsed() < patience {
            match self.connection.receive() {
                Ok(_) | Err(RecvError::WouldBlock) => {}
                Err(_) => break,
            }
            let (consumed, claimed) = self.read()?;
            if consumed > 0 {
                self.connection.consume(consumed, claimed);
            }
            if done(self) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    /// Read until the compositor has said what it has.
    fn run(&mut self) -> Result<(), String> {
        let started = Instant::now();
        self.flush()?;
        while started.elapsed() < PATIENCE {
            match self.connection.receive() {
                Ok(_) => {}
                Err(RecvError::WouldBlock) => {}
                Err(RecvError::Closed) => break,
                Err(error) => return Err(format!("reading: {error:?}")),
            }
            let (consumed, claimed) = self.read()?;
            if consumed > 0 {
                self.connection.consume(consumed, claimed);
            }
            self.flush()?;
            if self.settled {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Err("the compositor never finished listing its windows".to_owned())
    }

    /// Send whatever is queued.
    fn flush(&mut self) -> Result<(), String> {
        if self.out.is_empty() {
            // What a full socket left queued still has to go.
            return self
                .connection
                .flush()
                .map_err(|error| format!("writing: {error:?}"));
        }
        let (bytes, fds) = self.out.take();
        self.connection
            .send(&bytes, &fds)
            .map_err(|error| format!("writing: {error:?}"))
    }

    /// Read whatever arrived, and answer it.
    fn read(&mut self) -> Result<(usize, usize), String> {
        let fds = self.connection.fds();
        let bytes = self.connection.bytes().to_vec();
        let mut reader = Reader::new(&bytes, &fds);
        while !reader.is_done() {
            let Ok(header) = reader.peek() else {
                break;
            };
            let Some(interface) = self.interface_of(header.sender) else {
                return Err(format!("an event for object {}", header.sender.0));
            };
            let Some(method) = interface.event(header.opcode) else {
                return Err(format!("{} has no event {}", interface.name, header.opcode));
            };
            let (_, args) = match reader.read(method.signature) {
                Ok(read) => read,
                Err(compositor_wire::Error::Incomplete { .. }) => break,
                Err(error) => return Err(format!("{}.{}: {error:?}", interface.name, method.name)),
            };
            self.event(header.sender, header.opcode, &args)?;
        }
        Ok((reader.consumed(), reader.descriptors_taken()))
    }

    /// Which interface an object speaks.
    ///
    /// Every server-made object here is a toplevel handle: this client binds
    /// one manager and asks for nothing else that the compositor answers
    /// with an object of its own.
    fn interface_of(&self, id: ObjectId) -> Option<&'static Interface> {
        if id.is_server() || self.windows.contains_key(&id) {
            return Some(&foreign_toplevel::ZWLR_FOREIGN_TOPLEVEL_HANDLE_V1);
        }
        Some(match id {
            id::DISPLAY => &core::WL_DISPLAY,
            id::REGISTRY => &core::WL_REGISTRY,
            id::SYNC | id::SETTLED => &core::WL_CALLBACK,
            id::MANAGER => &foreign_toplevel::ZWLR_FOREIGN_TOPLEVEL_MANAGER_V1,
            id::SEAT => &core::WL_SEAT,
            _ => return None,
        })
    }

    /// Answer one event.
    fn event(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) -> Result<(), String> {
        match sender {
            id::DISPLAY if opcode == wl_display::event::ERROR => {
                let text = args.get(2).and_then(Arg::as_str).unwrap_or("");
                return Err(format!("the compositor refused this client: {text}"));
            }
            id::REGISTRY if opcode == wl_registry::event::GLOBAL => {
                let (Some(name), Some(interface), Some(version)) = (
                    args.first().and_then(Arg::as_uint),
                    args.get(1).and_then(Arg::as_str),
                    args.get(2).and_then(Arg::as_uint),
                ) else {
                    return Ok(());
                };
                let _ = self.globals.insert(interface.to_owned(), (name, version));
            }
            id::SYNC if opcode == core::wl_callback::event::DONE => self.bind()?,
            id::SETTLED if opcode == core::wl_callback::event::DONE => self.settled = true,
            id::MANAGER if opcode == manager::event::TOPLEVEL => {
                if let Some(id) = args.first().and_then(Arg::as_object) {
                    let _ = self.windows.insert(id, Toplevel::default());
                    self.order.push(id);
                }
            }
            id::MANAGER if opcode == manager::event::FINISHED => self.settled = true,
            other => self.toplevel(other, opcode, args),
        }
        Ok(())
    }

    /// One handle's events.
    fn toplevel(&mut self, sender: ObjectId, opcode: u16, args: &[Arg<'_>]) {
        let Some(window) = self.windows.get_mut(&sender) else {
            return;
        };
        match opcode {
            handle::event::TITLE => {
                if let Some(title) = args.first().and_then(Arg::as_str) {
                    window.title = title.to_owned();
                }
            }
            handle::event::APP_ID => {
                if let Some(app_id) = args.first().and_then(Arg::as_str) {
                    window.app_id = app_id.to_owned();
                }
            }
            handle::event::STATE => {
                let bytes = args.first().and_then(Arg::as_array).unwrap_or(&[]);
                let held = states(bytes);
                window.maximized = held.contains(&handle::state::MAXIMIZED);
                window.minimized = held.contains(&handle::state::MINIMIZED);
                window.activated = held.contains(&handle::state::ACTIVATED);
                window.fullscreen = held.contains(&handle::state::FULLSCREEN);
            }
            handle::event::DONE => {
                if !self.done.contains(&sender) {
                    self.done.push(sender);
                }
            }
            handle::event::CLOSED => {
                let _ = self.windows.remove(&sender);
                self.order.retain(|held| *held != sender);
                self.done.retain(|held| *held != sender);
            }
            _ => {}
        }
    }

    /// Bind the manager and the seat, and ask for a roundtrip after it.
    fn bind(&mut self) -> Result<(), String> {
        if self.bound {
            return Ok(());
        }
        self.bound = true;
        let interface = "zwlr_foreign_toplevel_manager_v1";
        let (name, offered) = self
            .globals
            .get(interface)
            .copied()
            .ok_or_else(|| format!("the compositor offers no {interface}"))?;
        self.out
            .write(
                id::REGISTRY,
                wl_registry::request::BIND,
                &[ArgType::Uint, ArgType::AnyNewId],
                &[
                    Arg::Uint(name),
                    Arg::AnyNewId {
                        interface,
                        version: offered.min(3),
                        id: id::MANAGER,
                    },
                ],
            )
            .map_err(|error| format!("binding {interface}: {error:?}"))?;
        // The seat is only needed to activate a window, and a compositor
        // that has none is one this can still list from.
        if let Some((name, offered)) = self.globals.get("wl_seat").copied() {
            let seat = id::SEAT;
            if self
                .out
                .write(
                    id::REGISTRY,
                    wl_registry::request::BIND,
                    &[ArgType::Uint, ArgType::AnyNewId],
                    &[
                        Arg::Uint(name),
                        Arg::AnyNewId {
                            interface: "wl_seat",
                            version: offered.min(7),
                            id: seat,
                        },
                    ],
                )
                .is_ok()
            {
                self.seat = Some(seat);
            }
        }
        // The compositor sends a handle for every window it already has
        // before it answers anything else, so a roundtrip after the bind is
        // how a client knows it has the whole list. `lswt` does the same.
        request(
            &mut self.out,
            id::DISPLAY,
            wl_display::request::SYNC,
            &[ArgType::NewId],
            &[Arg::NewId(id::SETTLED)],
        );
        Ok(())
    }
}

/// The 32-bit values an `array` argument holds.
fn states(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .filter_map(|chunk| <[u8; 4]>::try_from(chunk).ok())
        .map(u32::from_ne_bytes)
        .collect()
}

/// Queue one request.
fn request(
    out: &mut Writer,
    sender: ObjectId,
    opcode: u16,
    signature: &'static [ArgType],
    args: &[Arg<'_>],
) {
    let _ = out.write(sender, opcode, signature, args);
}
