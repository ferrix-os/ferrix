//! The Wayland side of locking the screen.
//!
//! The objects a lock needs and no others: `ext_session_lock_manager_v1`,
//! the `ext_session_lock_v1` it hands back, a `wl_output` for each screen
//! and an `ext_session_lock_surface_v1` on each of them. No `xdg_toplevel`
//! -- a lock screen is not a window, and the protocol gives it a role of
//! its own so that a compositor can tell the difference.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use compositor_protocol::core::{self, wl_display, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use compositor_protocol::session_lock::{
    self, ext_session_lock_manager_v1 as manager, ext_session_lock_surface_v1 as lock_surface,
    ext_session_lock_v1,
};
use compositor_render::{Format, Pattern};
use compositor_shm::Shared;
use compositor_socket::{Connection, RecvError};
use compositor_wire::{Arg, ArgType, Fd, Interface, ObjectId, Reader, Writer};

/// The objects this client makes at fixed ids, and the two ranges that grow
/// with the number of screens.
mod id {
    use compositor_wire::ObjectId;

    pub(super) const DISPLAY: ObjectId = ObjectId(1);
    pub(super) const REGISTRY: ObjectId = ObjectId(2);
    pub(super) const SYNC: ObjectId = ObjectId(3);
    pub(super) const SHM: ObjectId = ObjectId(4);
    pub(super) const MANAGER: ObjectId = ObjectId(5);
    pub(super) const LOCK: ObjectId = ObjectId(6);
    /// The first `wl_output`; one id a screen from here.
    pub(super) const OUTPUT: u32 = 16;
    /// The first `wl_surface`, then its lock surface, its pool and its
    /// buffer: four ids a screen.
    pub(super) const PER_SCREEN: u32 = 32;
}

/// How long to wait for the compositor to say the screen is covered.
const PATIENCE: Duration = Duration::from_secs(20);

/// What happened while the screen was locked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Locked {
    /// How many screens the lock covered.
    pub screens: usize,
    /// Whether the compositor said every one of them was covered.
    pub told: bool,
}

/// Lock the screen, hold it for `held`, and unlock.
///
/// # Errors
///
/// A sentence saying what could not be done, including the compositor not
/// offering the protocol and never saying the screen was covered.
pub fn lock(socket: &std::path::Path, held: Duration) -> Result<Locked, String> {
    // From the moment the screen is covered, which is when `release` is
    // first asked.
    let until = std::cell::Cell::new(None::<Instant>);
    lock_until(socket, &|| {
        let at = until.get().unwrap_or_else(|| Instant::now() + held);
        until.set(Some(at));
        Instant::now() >= at
    })
}

/// Lock the screen, hold it until `release` says so, and unlock.
///
/// What a test that takes a picture of the locked screen asks for: the
/// lock is held for as long as the picture takes, rather than for a time
/// that a busy host may not take the picture inside. `release` is asked
/// between turns, about every ten milliseconds.
///
/// # Errors
///
/// As [`lock`].
pub fn lock_until(socket: &std::path::Path, release: &dyn Fn() -> bool) -> Result<Locked, String> {
    let mut session = Session::connect(socket)?;
    session.run(PATIENCE)?;
    if !session.told {
        return Err("the compositor never said the screen was covered".to_owned());
    }
    // Held so that a test can take a picture of a locked screen, which is
    // the only way to see that the windows have stopped being drawn.
    while !release() {
        session.turn()?;
        std::thread::sleep(Duration::from_millis(10));
    }
    session.unlock()?;
    Ok(Locked {
        screens: session.screens.len(),
        told: session.told,
    })
}

/// One connection, holding one lock.
struct Session {
    connection: Connection,
    out: Writer,
    globals: BTreeMap<String, (u32, u32)>,
    /// The `wl_output` globals, in the order the registry announced them.
    outputs: Vec<(u32, u32)>,
    bound: bool,
    /// The memory each screen's surface is drawn into, once its size is
    /// known.
    screens: Vec<Option<Shared>>,
    /// Whether the compositor said every screen is covered.
    told: bool,
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
            outputs: Vec::new(),
            bound: false,
            screens: Vec::new(),
            told: false,
        })
    }

    /// Read until the compositor says the screen is covered.
    fn run(&mut self, patience: Duration) -> Result<(), String> {
        let started = Instant::now();
        self.flush()?;
        while started.elapsed() < patience {
            self.turn()?;
            if self.told {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Err("the compositor never answered the lock".to_owned())
    }

    /// One pass: read what arrived, answer it, send what that queued.
    fn turn(&mut self) -> Result<(), String> {
        match self.connection.receive() {
            Ok(_) | Err(RecvError::WouldBlock) => {}
            Err(RecvError::Closed) => return Err("the compositor went".to_owned()),
            Err(error) => return Err(format!("reading: {error:?}")),
        }
        let (consumed, claimed) = self.read()?;
        if consumed > 0 {
            self.connection.consume(consumed, claimed);
        }
        self.flush()
    }

    /// Give the screen back.
    fn unlock(&mut self) -> Result<(), String> {
        request(
            &mut self.out,
            id::LOCK,
            ext_session_lock_v1::request::UNLOCK_AND_DESTROY,
            &[],
            &[],
        );
        self.flush()?;
        // The compositor has to read it before this program leaves: a
        // socket closed with the request still in flight is a screen that
        // stays locked, which is exactly what the protocol says must happen
        // to a lock whose client died.
        let sent = Instant::now();
        while sent.elapsed() < Duration::from_millis(500) {
            match self.connection.receive() {
                Ok(_) | Err(RecvError::WouldBlock) => {}
                Err(_) => break,
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
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
            let Some(interface) = interface_of(header.sender) else {
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
                if interface == "wl_output" {
                    self.outputs.push((name, version));
                }
                let _ = self.globals.insert(interface.to_owned(), (name, version));
            }
            id::SYNC if opcode == core::wl_callback::event::DONE => self.bind()?,
            id::LOCK if opcode == ext_session_lock_v1::event::LOCKED => self.told = true,
            id::LOCK if opcode == ext_session_lock_v1::event::FINISHED => {
                return Err("the compositor refused the lock".to_owned());
            }
            other if opcode == lock_surface::event::CONFIGURE => {
                let value = |at: usize| args.get(at).and_then(Arg::as_uint).unwrap_or(0);
                self.configured(other, value(0), value(1), value(2))?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Bind what a lock needs, take the lock, and ask for a surface on every
    /// screen.
    fn bind(&mut self) -> Result<(), String> {
        if self.bound {
            return Ok(());
        }
        self.bound = true;
        for (interface, id, want) in [
            ("wl_shm", id::SHM, 1u32),
            ("wl_compositor", ObjectId(7), 6),
            ("ext_session_lock_manager_v1", id::MANAGER, 1),
        ] {
            let (name, offered) = self
                .globals
                .get(interface)
                .copied()
                .ok_or_else(|| format!("the compositor offers no {interface}"))?;
            bind(&mut self.out, name, interface, want.min(offered), id)?;
        }
        request(
            &mut self.out,
            id::MANAGER,
            manager::request::LOCK,
            &[ArgType::NewId],
            &[Arg::NewId(id::LOCK)],
        );
        // One surface a screen, which is what the protocol requires before
        // the compositor will say the screen is covered.
        let outputs = self.outputs.clone();
        for (which, (name, version)) in outputs.iter().enumerate() {
            let at = u32::try_from(which).unwrap_or(0);
            let output = ObjectId(id::OUTPUT.saturating_add(at));
            bind(&mut self.out, *name, "wl_output", (*version).min(4), output)?;
            let (surface, lock) = (self.surface_of(which), self.lock_surface_of(which));
            request(
                &mut self.out,
                ObjectId(7),
                core::wl_compositor::request::CREATE_SURFACE,
                &[ArgType::NewId],
                &[Arg::NewId(surface)],
            );
            request(
                &mut self.out,
                id::LOCK,
                ext_session_lock_v1::request::GET_LOCK_SURFACE,
                &[
                    ArgType::NewId,
                    ArgType::Object { nullable: false },
                    ArgType::Object { nullable: false },
                ],
                &[Arg::NewId(lock), Arg::Object(surface), Arg::Object(output)],
            );
            self.screens.push(None);
        }
        if self.screens.is_empty() {
            return Err("the compositor has no screens to lock".to_owned());
        }
        Ok(())
    }

    /// Draw one screen at the size the compositor gave it.
    ///
    /// The buffer has to be exactly that size: the protocol makes a
    /// mismatch an error, because a lock surface that did not cover its
    /// screen would leave a strip of what was underneath.
    fn configured(
        &mut self,
        lock: ObjectId,
        serial: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let Some(which) = (0..self.screens.len()).find(|at| self.lock_surface_of(*at) == lock)
        else {
            return Ok(());
        };
        request(
            &mut self.out,
            lock,
            lock_surface::request::ACK_CONFIGURE,
            &[ArgType::Uint],
            &[Arg::Uint(serial)],
        );
        let stride = width.saturating_mul(4);
        let len = usize::try_from(stride.saturating_mul(height))
            .ok()
            .filter(|len| *len > 0)
            .ok_or_else(|| format!("a {width}x{height} lock surface"))?;
        let mut shared = Shared::new(len).map_err(|error| format!("the memory: {error}"))?;
        // The checkerboard, because `src/user/system/linux/compositor/render` draws it and the
        // expected image is made from the same code: a locked screen is a
        // picture a test can compare, and this is the picture.
        let pixels = Pattern::Checkerboard.draw(width, height);
        let room = shared.bytes_mut();
        let take = pixels.len().min(room.len());
        if let (Some(to), Some(from)) = (room.get_mut(..take), pixels.get(..take)) {
            to.copy_from_slice(from);
        }
        let (pool, buffer) = (self.pool_of(which), self.buffer_of(which));
        let surface = self.surface_of(which);
        request_with_fd(
            &mut self.out,
            id::SHM,
            wl_shm::request::CREATE_POOL,
            &[ArgType::NewId, ArgType::Fd, ArgType::Int],
            &[
                Arg::NewId(pool),
                Arg::Fd(Fd(shared.as_raw_fd())),
                Arg::Int(i32::try_from(len).unwrap_or(i32::MAX)),
            ],
        );
        let int = |value: u32| Arg::Int(i32::try_from(value).unwrap_or(0));
        request(
            &mut self.out,
            pool,
            wl_shm_pool::request::CREATE_BUFFER,
            &[
                ArgType::NewId,
                ArgType::Int,
                ArgType::Int,
                ArgType::Int,
                ArgType::Int,
                ArgType::Uint,
            ],
            &[
                Arg::NewId(buffer),
                Arg::Int(0),
                int(width),
                int(height),
                int(stride),
                Arg::Uint(Format::Xrgb8888.wl_shm()),
            ],
        );
        request(
            &mut self.out,
            surface,
            wl_surface::request::ATTACH,
            &[
                ArgType::Object { nullable: true },
                ArgType::Int,
                ArgType::Int,
            ],
            &[Arg::Object(buffer), Arg::Int(0), Arg::Int(0)],
        );
        request(
            &mut self.out,
            surface,
            wl_surface::request::DAMAGE_BUFFER,
            &[ArgType::Int, ArgType::Int, ArgType::Int, ArgType::Int],
            &[Arg::Int(0), Arg::Int(0), int(width), int(height)],
        );
        request(
            &mut self.out,
            surface,
            wl_surface::request::COMMIT,
            &[],
            &[],
        );
        if let Some(slot) = self.screens.get_mut(which) {
            *slot = Some(shared);
        }
        Ok(())
    }

    /// The four ids one screen uses.
    fn surface_of(&self, which: usize) -> ObjectId {
        ObjectId(id::PER_SCREEN.saturating_add(u32::try_from(which).unwrap_or(0) * 4))
    }
    fn lock_surface_of(&self, which: usize) -> ObjectId {
        ObjectId(self.surface_of(which).0.saturating_add(1))
    }
    fn pool_of(&self, which: usize) -> ObjectId {
        ObjectId(self.surface_of(which).0.saturating_add(2))
    }
    fn buffer_of(&self, which: usize) -> ObjectId {
        ObjectId(self.surface_of(which).0.saturating_add(3))
    }
}

/// Which interface an object speaks.
///
/// The per-screen ids are four apart, so which of the four an id is follows
/// from its place in the block.
fn interface_of(id: ObjectId) -> Option<&'static Interface> {
    Some(match id {
        id::DISPLAY => &core::WL_DISPLAY,
        id::REGISTRY => &core::WL_REGISTRY,
        id::SYNC => &core::WL_CALLBACK,
        id::SHM => &core::WL_SHM,
        ObjectId(7) => &core::WL_COMPOSITOR,
        id::MANAGER => &session_lock::EXT_SESSION_LOCK_MANAGER_V1,
        id::LOCK => &session_lock::EXT_SESSION_LOCK_V1,
        ObjectId(value) if (id::OUTPUT..id::PER_SCREEN).contains(&value) => &core::WL_OUTPUT,
        ObjectId(value) if value >= id::PER_SCREEN => match (value - id::PER_SCREEN) % 4 {
            0 => &core::WL_SURFACE,
            1 => &session_lock::EXT_SESSION_LOCK_SURFACE_V1,
            2 => &core::WL_SHM_POOL,
            _ => &core::WL_BUFFER,
        },
        _ => return None,
    })
}

/// Bind one global at a fixed id.
fn bind(
    out: &mut Writer,
    name: u32,
    interface: &'static str,
    version: u32,
    id: ObjectId,
) -> Result<(), String> {
    out.write(
        id::REGISTRY,
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
    .map_err(|error| format!("binding {interface}: {error:?}"))
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

/// The same, for a request that carries a descriptor.
fn request_with_fd(
    out: &mut Writer,
    sender: ObjectId,
    opcode: u16,
    signature: &'static [ArgType],
    args: &[Arg<'_>],
) {
    request(out, sender, opcode, signature, args);
}
