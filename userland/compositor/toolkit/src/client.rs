//! The connection and everything bound on it.

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::{FromRawFd as _, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

use compositor_protocol::core::{
    self as wl, wl_buffer, wl_compositor, wl_display, wl_keyboard, wl_output, wl_pointer,
    wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use compositor_protocol::cursor_shape::{
    self, wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1,
};
use compositor_protocol::idle_notify::{self, ext_idle_notification_v1, ext_idle_notifier_v1};
use compositor_protocol::layer_shell::{self, zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use compositor_protocol::session_lock::{
    self, ext_session_lock_manager_v1, ext_session_lock_surface_v1, ext_session_lock_v1,
};
use compositor_protocol::xdg_output::{self, zxdg_output_manager_v1, zxdg_output_v1};
use compositor_protocol::xdg_shell::{
    self, xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use compositor_shm::Shared;
use compositor_socket::{Connection, RecvError};
use compositor_wire::{Arg, Fd, Fixed, Interface, ObjectId, Reader, Writer};

use crate::buffer::{self, Slot};
use crate::keyboard::Modifiers;
use crate::loop_sources::{Command, Waker};
use crate::sources::{Children, Sources};
use crate::{
    ChildId, CursorShape, Event, IdleId, Key, KeyboardEvent, LayerOptions, Output, OutputId,
    PointerEvent, PopupOptions, Rect, SurfaceId, TimerId, ToplevelOptions, Transform, Value,
    WatchId,
};

/// Why something could not be done.
#[derive(Debug)]
pub enum Error {
    /// No `WAYLAND_DISPLAY`, or the socket is not there.
    NoDisplay(String),
    /// The compositor does not offer a global this needs, by interface name.
    Missing(&'static str),
    /// `wl_display.error`: the compositor refused something and closed the
    /// connection. The text is the compositor's.
    Protocol {
        /// The object the error is about.
        object: u32,
        /// The interface's error code.
        code: u32,
        /// What the compositor said.
        message: String,
    },
    /// The compositor went away.
    Closed,
    /// A system call failed.
    Io(std::io::Error),
    /// Anything else, in a sentence.
    Other(String),
}

impl core::fmt::Display for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoDisplay(why) => write!(formatter, "no Wayland display: {why}"),
            Self::Missing(interface) => write!(formatter, "the compositor offers no {interface}"),
            Self::Protocol {
                object,
                code,
                message,
            } => write!(
                formatter,
                "the compositor refused this client (object {object}, error {code}): {message}"
            ),
            Self::Closed => formatter.write_str("the compositor closed the connection"),
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Other(text) => formatter.write_str(text),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// One global the registry announced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Global {
    /// The registry's name for it.
    pub name: u32,
    /// Its interface.
    pub interface: String,
    /// The highest version the compositor offers.
    pub version: u32,
}

/// The versions this crate speaks, which it binds at or below.
const SPOKEN: [(&str, u32); 10] = [
    ("wl_compositor", 6),
    ("wl_shm", 1),
    ("wl_seat", 8),
    ("xdg_wm_base", 3),
    ("zwlr_layer_shell_v1", 4),
    ("ext_session_lock_manager_v1", 1),
    ("wp_cursor_shape_manager_v1", 1),
    ("ext_idle_notifier_v1", 2),
    ("zxdg_output_manager_v1", 3),
    ("wl_output", 4),
];

/// What an object is to this client, which says what its events mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Display,
    Registry,
    /// A `wl_display.sync`, by its token.
    Sync(u64),
    /// A frame callback.
    Frame(SurfaceId),
    /// A global of this crate's, which says nothing it needs.
    Quiet,
    Seat,
    Keyboard,
    Pointer,
    Output(OutputId),
    XdgOutput(OutputId),
    WmBase,
    XdgSurface(SurfaceId),
    Popup(SurfaceId),
    Toplevel(SurfaceId),
    LayerSurface(SurfaceId),
    Surface(SurfaceId),
    Lock,
    LockSurface(SurfaceId),
    Idle(IdleId),
    /// A buffer of a surface's.
    SurfaceBuffer(SurfaceId),
    /// Something the program made; its events go back to it.
    User,
}

/// One live object.
#[derive(Clone, Copy, Debug)]
struct Object {
    interface: &'static Interface,
    version: u32,
    role: Role,
    /// A destructor was sent; the entry stays until `delete_id`, so events
    /// already in flight for it can still be read past.
    dead: bool,
}

/// What a surface is for.
#[derive(Clone, Debug)]
enum Kind {
    Layer {
        role: ObjectId,
        options: LayerOptions,
    },
    Lock {
        role: ObjectId,
    },
    Popup {
        xdg: ObjectId,
        popup: ObjectId,
        parent: SurfaceId,
        options: PopupOptions,
        /// The size the last `xdg_popup.configure` gave.
        pending: (u32, u32),
    },
    Toplevel {
        xdg: ObjectId,
        toplevel: ObjectId,
        options: ToplevelOptions,
        /// The size the last `xdg_toplevel.configure` gave; a zero is the
        /// compositor leaving it to the program.
        pending: (u32, u32),
    },
}

/// One surface and what it needs to be drawn.
#[derive(Debug)]
struct SurfaceState {
    surface: ObjectId,
    kind: Kind,
    configured: Option<(u32, u32)>,
    /// `wl_surface.preferred_buffer_scale`, once sent.
    preferred: Option<u32>,
    /// The screens it is on.
    entered: Vec<OutputId>,
    /// The scale its buffers are drawn at, and last told the compositor.
    scale: u32,
    sent_scale: u32,
    slots: Vec<Slot>,
    scratch: Vec<u8>,
}

/// One screen's objects beside its description.
#[derive(Debug)]
struct OutputState {
    global: u32,
    object: ObjectId,
    xdg: Option<ObjectId>,
    info: Output,
    announced: bool,
}

/// A key being held, which the client retypes itself.
#[derive(Clone, Copy, Debug)]
struct Held {
    code: u32,
    next: Instant,
}

/// The seat's state.
#[derive(Debug, Default)]
struct Seat {
    seat: Option<ObjectId>,
    keyboard: Option<ObjectId>,
    pointer: Option<ObjectId>,
    cursor_device: Option<ObjectId>,
    layouts: Vec<&'static compositor_xkb::generated::Layout>,
    mask: u32,
    group: u32,
    focus: Option<SurfaceId>,
    repeat_delay: Duration,
    repeat_interval: Option<Duration>,
    held: Option<Held>,
    pointer_focus: Option<SurfaceId>,
    pointer_at: (f64, f64),
    enter_serial: u32,
    /// The last button or key press's serial, which a grab needs.
    input_serial: u32,
    cursor: CursorShape,
    hidden: bool,
    /// A picture of the program's own that stands for the pointer, which
    /// [`Client::set_cursor_image`] gave and [`Client::set_cursor`] ends.
    image: Option<CursorImage>,
    /// An axis event being put together until `wl_pointer.frame`.
    axis: Option<Axis>,
    /// What `axis_value120` has sent that is not yet a whole click.
    residue: (i32, i32),
}

/// A cursor drawn by the program: a surface of the seat's own and the one
/// buffer it shows.
#[derive(Debug)]
struct CursorImage {
    surface: ObjectId,
    buffer: ObjectId,
    pool: ObjectId,
    /// The buffer's bytes, kept mapped until the buffer is replaced.
    shared: Shared,
    hot: (i32, i32),
}

/// An axis event being put together.
#[derive(Clone, Copy, Debug, Default)]
struct Axis {
    vertical: f64,
    horizontal: f64,
    discrete: (i32, i32),
}

/// A buffer the program made for a request of its own.
#[derive(Debug)]
struct ProgramBuffer {
    shared: Shared,
    pool: ObjectId,
}

/// A connection to the compositor, and everything bound on it.
#[derive(Debug)]
pub struct Client {
    connection: Connection,
    out: Writer,
    objects: BTreeMap<u32, Object>,
    next_id: u32,
    free_ids: Vec<u32>,
    globals: Vec<Global>,
    /// This crate's own bindings, by interface name.
    bound: BTreeMap<&'static str, (ObjectId, u32)>,
    outputs: BTreeMap<OutputId, OutputState>,
    next_output: u32,
    surfaces: BTreeMap<SurfaceId, SurfaceState>,
    next_surface: u32,
    seat: Seat,
    lock: Option<ObjectId>,
    idles: BTreeMap<IdleId, ObjectId>,
    next_idle: u32,
    program_buffers: BTreeMap<u32, ProgramBuffer>,
    sources: Sources,
    children: Children,
    pending: Vec<Event>,
    synced: BTreeSet<u64>,
    next_sync: u64,
    /// The first failure of a call that cannot answer one, handed back by
    /// the next [`Client::dispatch`].
    deferred: Option<Error>,
    /// Things said once.
    said: BTreeSet<&'static str>,
}

impl Client {
    /// Connect to the compositor `WAYLAND_DISPLAY` names (joined to
    /// `XDG_RUNTIME_DIR` unless it is a path), bind what this crate speaks,
    /// and wait until every screen has been described.
    ///
    /// # Errors
    ///
    /// No display, a socket that refuses, or a compositor that closes the
    /// connection during the first round trip.
    pub fn connect() -> Result<Self, Error> {
        let display = std::env::var("WAYLAND_DISPLAY")
            .map_err(|_| Error::NoDisplay("WAYLAND_DISPLAY is not set".to_owned()))?;
        let path = compositor_socket::socket_path(&display)
            .map_err(|error| Error::NoDisplay(error.to_string()))?;
        Self::connect_to(&path)
    }

    /// The same, to a socket at `path`.
    ///
    /// # Errors
    ///
    /// As [`Client::connect`].
    pub fn connect_to(path: &Path) -> Result<Self, Error> {
        let stream = std::os::unix::net::UnixStream::connect(path)
            .map_err(|error| Error::NoDisplay(format!("{}: {error}", path.display())))?;
        let connection = Connection::new(stream)?;
        let mut client = Self {
            connection,
            out: Writer::new(),
            objects: BTreeMap::new(),
            next_id: 2,
            free_ids: Vec::new(),
            globals: Vec::new(),
            bound: BTreeMap::new(),
            outputs: BTreeMap::new(),
            next_output: 1,
            surfaces: BTreeMap::new(),
            next_surface: 1,
            seat: Seat::default(),
            lock: None,
            idles: BTreeMap::new(),
            next_idle: 1,
            program_buffers: BTreeMap::new(),
            sources: Sources::default(),
            children: Children::default(),
            pending: Vec::new(),
            synced: BTreeSet::new(),
            next_sync: 1,
            deferred: None,
            said: BTreeSet::new(),
        };
        let _ = client.objects.insert(
            ObjectId::DISPLAY.0,
            Object {
                interface: &wl::WL_DISPLAY,
                version: 1,
                role: Role::Display,
                dead: false,
            },
        );
        let registry = client.make(&wl::WL_REGISTRY, 1, Role::Registry);
        client.send(
            ObjectId::DISPLAY,
            wl_display::request::GET_REGISTRY,
            &[Arg::NewId(registry)],
        )?;
        // The globals, then what binding them made the compositor say (the
        // screens' descriptions, the seat's devices), then what those made
        // it say (the keymap, the xdg outputs).
        for _ in 0..3 {
            client.sync_and_wait()?;
        }
        Ok(client)
    }

    /// Every global the registry has announced and not removed.
    #[must_use]
    pub fn globals(&self) -> &[Global] {
        &self.globals
    }

    /// The version `interface` was bound at by this crate, or `None` where
    /// it is not bound (the compositor lacks it, or this crate does not
    /// bind it: then [`Client::bind`] it).
    #[must_use]
    pub fn bound_version(&self, interface: &str) -> Option<u32> {
        if interface == "wl_output" {
            return self.outputs.values().next().map(|output| {
                self.objects
                    .get(&output.object.0)
                    .map_or(0, |object| object.version)
            });
        }
        self.bound.get(interface).map(|(_, version)| *version)
    }

    /// Every screen that has been described, in the order they came.
    #[must_use]
    pub fn outputs(&self) -> Vec<&Output> {
        self.outputs
            .values()
            .filter(|output| output.info.done)
            .map(|output| &output.info)
            .collect()
    }

    /// One screen.
    #[must_use]
    pub fn output(&self, id: OutputId) -> Option<&Output> {
        self.outputs.get(&id).map(|output| &output.info)
    }

    /// The `wl_output` object of a screen, for a request of the program's
    /// own that names one (`zwlr_screencopy_manager_v1.capture_output`).
    #[must_use]
    pub fn output_object(&self, id: OutputId) -> Option<ObjectId> {
        self.outputs.get(&id).map(|output| output.object)
    }

    // -- surfaces -----------------------------------------------------------

    /// Make a `zwlr_layer_surface_v1` placed as `options` says, and commit
    /// it bare so the compositor configures it: a [`Event::Configure`]
    /// follows, after which it can be drawn.
    ///
    /// # Errors
    ///
    /// [`Error::Missing`] without `zwlr_layer_shell_v1`.
    pub fn layer_surface(&mut self, options: &LayerOptions) -> Result<SurfaceId, Error> {
        let (shell, version) = self.global("zwlr_layer_shell_v1")?;
        let output = match options.output {
            Some(id) => self
                .outputs
                .get(&id)
                .map(|output| output.object)
                .ok_or_else(|| Error::Other(format!("no output {}", id.0)))?,
            None => ObjectId::NULL,
        };
        let id = self.new_surface_id();
        let surface = self.create_surface(id)?;
        let role = self.make(
            &layer_shell::ZWLR_LAYER_SURFACE_V1,
            version,
            Role::LayerSurface(id),
        );
        let layer = options.layer.wire();
        self.send(
            shell,
            zwlr_layer_shell_v1::request::GET_LAYER_SURFACE,
            &[
                Arg::NewId(role),
                Arg::Object(surface),
                Arg::Object(output),
                Arg::Uint(layer),
                Arg::Str(Some(&options.namespace)),
            ],
        )?;
        self.send_layer(role, options, None)?;
        self.send(surface, wl_surface::request::COMMIT, &[])?;
        let _ = self.surfaces.insert(
            id,
            SurfaceState::new(
                surface,
                Kind::Layer {
                    role,
                    options: options.clone(),
                },
            ),
        );
        Ok(id)
    }

    /// Change a layer surface's placement: every field that differs from
    /// what was last sent is sent again, and the surface committed.
    /// `output` and `namespace` cannot change after creation; a change to
    /// either is ignored (destroy and make a new one).
    pub fn set_layer_options(&mut self, surface: SurfaceId, options: &LayerOptions) {
        let Some(state) = self.surfaces.get(&surface) else {
            return;
        };
        let Kind::Layer {
            role,
            options: before,
        } = &state.kind
        else {
            return;
        };
        let (role, before, object) = (*role, before.clone(), state.surface);
        let result = self
            .send_layer(role, options, Some(&before))
            .and_then(|()| self.send(object, wl_surface::request::COMMIT, &[]));
        self.defer(result);
        if let Some(state) = self.surfaces.get_mut(&surface)
            && let Kind::Layer { options: kept, .. } = &mut state.kind
        {
            let (output, namespace) = (kept.output, kept.namespace.clone());
            *kept = options.clone();
            kept.output = output;
            kept.namespace = namespace;
        }
    }

    /// Take the session lock (`ext_session_lock_manager_v1.lock`).
    /// [`Event::Locked`] says the screens are covered, which the protocol
    /// only does once every screen has a lock surface with a buffer: call
    /// [`Client::lock_surface`] for each output next.
    ///
    /// # Errors
    ///
    /// [`Error::Missing`] without `ext_session_lock_manager_v1`, or a lock
    /// already taken.
    pub fn lock(&mut self) -> Result<(), Error> {
        if self.lock.is_some() {
            return Err(Error::Other("the lock is already taken".to_owned()));
        }
        let (manager, _) = self.global("ext_session_lock_manager_v1")?;
        let lock = self.make(&session_lock::EXT_SESSION_LOCK_V1, 1, Role::Lock);
        self.send(
            manager,
            ext_session_lock_manager_v1::request::LOCK,
            &[Arg::NewId(lock)],
        )?;
        self.lock = Some(lock);
        Ok(())
    }

    /// An `ext_session_lock_surface_v1` on `output`, for the lock taken with
    /// [`Client::lock`]. Its [`Event::Configure`] gives the screen's size,
    /// and the buffer drawn must be exactly that.
    ///
    /// # Errors
    ///
    /// No lock taken, or an unknown output.
    pub fn lock_surface(&mut self, output: OutputId) -> Result<SurfaceId, Error> {
        let lock = self
            .lock
            .ok_or_else(|| Error::Other("no lock is taken".to_owned()))?;
        let screen = self
            .outputs
            .get(&output)
            .map(|state| state.object)
            .ok_or_else(|| Error::Other(format!("no output {}", output.0)))?;
        let id = self.new_surface_id();
        let surface = self.create_surface(id)?;
        let role = self.make(
            &session_lock::EXT_SESSION_LOCK_SURFACE_V1,
            1,
            Role::LockSurface(id),
        );
        self.send(
            lock,
            ext_session_lock_v1::request::GET_LOCK_SURFACE,
            &[Arg::NewId(role), Arg::Object(surface), Arg::Object(screen)],
        )?;
        let mut state = SurfaceState::new(surface, Kind::Lock { role });
        state.entered.push(output);
        state.scale = self.scale_of_outputs(&state.entered);
        let _ = self.surfaces.insert(id, state);
        Ok(id)
    }

    /// `ext_session_lock_v1.unlock_and_destroy`, flushed, and the lock
    /// surfaces destroyed: the screen comes back.
    ///
    /// # Errors
    ///
    /// Writing to the compositor failing.
    pub fn unlock(&mut self) -> Result<(), Error> {
        let Some(lock) = self.lock.take() else {
            return Ok(());
        };
        self.send(lock, ext_session_lock_v1::request::UNLOCK_AND_DESTROY, &[])?;
        self.mark_dead(lock);
        let locks: Vec<SurfaceId> = self
            .surfaces
            .iter()
            .filter(|(_, state)| matches!(state.kind, Kind::Lock { .. }))
            .map(|(id, _)| *id)
            .collect();
        for id in locks {
            self.destroy(id);
        }
        // The compositor has to have read it before this program may go: a
        // lock whose client died with the unlock unread stays locked, which
        // is what the protocol says must happen.
        self.sync_and_wait()
    }

    /// An `xdg_popup` on `parent`, placed by an `xdg_positioner` built from
    /// `options`. The parent is a layer surface (through
    /// `zwlr_layer_surface_v1.get_popup`), a window or another popup (its
    /// `xdg_surface`, a menu's submenu). A grab is taken if `options.grab`
    /// and there is a button serial to take it with.
    ///
    /// # Errors
    ///
    /// [`Error::Missing`] without `xdg_wm_base`, or a parent that is a lock
    /// surface or not there.
    pub fn popup(&mut self, parent: SurfaceId, options: &PopupOptions) -> Result<SurfaceId, Error> {
        let (base, version) = self.global("xdg_wm_base")?;
        // The layer surface that takes the popup, or the xdg_surface it is
        // made on.
        let (layer, parent_xdg) = match self.surfaces.get(&parent).map(|state| &state.kind) {
            Some(Kind::Layer { role, .. }) => (Some(*role), ObjectId::NULL),
            Some(Kind::Toplevel { xdg, .. } | Kind::Popup { xdg, .. }) => (None, *xdg),
            _ => {
                return Err(Error::Other(
                    "a popup's parent must be a layer surface, a window or a popup".to_owned(),
                ));
            }
        };
        let id = self.new_surface_id();
        let surface = self.create_surface(id)?;
        let positioner = self.positioner(base, version, options)?;
        let xdg = self.make(&xdg_shell::XDG_SURFACE, version, Role::XdgSurface(id));
        self.send(
            base,
            xdg_wm_base::request::GET_XDG_SURFACE,
            &[Arg::NewId(xdg), Arg::Object(surface)],
        )?;
        let popup = self.make(&xdg_shell::XDG_POPUP, version, Role::Popup(id));
        self.send(
            xdg,
            xdg_surface::request::GET_POPUP,
            &[
                Arg::NewId(popup),
                Arg::Object(parent_xdg),
                Arg::Object(positioner),
            ],
        )?;
        if let Some(layer) = layer {
            self.send(
                layer,
                zwlr_layer_surface_v1::request::GET_POPUP,
                &[Arg::Object(popup)],
            )?;
        }
        self.destroy_object(positioner, xdg_positioner::request::DESTROY);
        if options.grab
            && let Some(seat) = self.seat.seat
        {
            let serial = self.seat.input_serial;
            self.send(
                popup,
                xdg_popup::request::GRAB,
                &[Arg::Object(seat), Arg::Uint(serial)],
            )?;
        }
        self.send(surface, wl_surface::request::COMMIT, &[])?;
        let mut state = SurfaceState::new(
            surface,
            Kind::Popup {
                xdg,
                popup,
                parent,
                options: *options,
                pending: (0, 0),
            },
        );
        state.scale = self.surfaces.get(&parent).map_or(1, |parent| parent.scale);
        let _ = self.surfaces.insert(id, state);
        Ok(id)
    }

    /// A window: an `xdg_toplevel` with `options`' title and app id,
    /// committed bare so the compositor configures it. A
    /// [`Event::Configure`] follows with the size to draw at -- the
    /// compositor's, or `options.size` where it leaves the choice to the
    /// program -- and a person closing the window arrives as
    /// [`Event::CloseRequested`], which tears nothing down.
    ///
    /// # Errors
    ///
    /// [`Error::Missing`] without `xdg_wm_base`.
    pub fn toplevel(&mut self, options: &ToplevelOptions) -> Result<SurfaceId, Error> {
        let (base, version) = self.global("xdg_wm_base")?;
        let id = self.new_surface_id();
        let surface = self.create_surface(id)?;
        let xdg = self.make(&xdg_shell::XDG_SURFACE, version, Role::XdgSurface(id));
        self.send(
            base,
            xdg_wm_base::request::GET_XDG_SURFACE,
            &[Arg::NewId(xdg), Arg::Object(surface)],
        )?;
        let toplevel = self.make(&xdg_shell::XDG_TOPLEVEL, version, Role::Toplevel(id));
        self.send(
            xdg,
            xdg_surface::request::GET_TOPLEVEL,
            &[Arg::NewId(toplevel)],
        )?;
        // Before the first commit, where a compositor decides whether the
        // window floats.
        if let Some(Kind::Toplevel {
            toplevel: parent, ..
        }) = options
            .parent
            .and_then(|parent| self.surfaces.get(&parent))
            .map(|state| &state.kind)
        {
            let parent = *parent;
            self.send(
                toplevel,
                xdg_toplevel::request::SET_PARENT,
                &[Arg::Object(parent)],
            )?;
        }
        self.send(
            toplevel,
            xdg_toplevel::request::SET_TITLE,
            &[Arg::Str(Some(&options.title))],
        )?;
        self.send(
            toplevel,
            xdg_toplevel::request::SET_APP_ID,
            &[Arg::Str(Some(&options.app_id))],
        )?;
        self.send(surface, wl_surface::request::COMMIT, &[])?;
        let _ = self.surfaces.insert(
            id,
            SurfaceState::new(
                surface,
                Kind::Toplevel {
                    xdg,
                    toplevel,
                    options: options.clone(),
                    pending: (0, 0),
                },
            ),
        );
        Ok(id)
    }

    /// Change a window's title (`xdg_toplevel.set_title`). Anything but a
    /// window is left alone.
    pub fn set_title(&mut self, window: SurfaceId, title: &str) {
        let result = self.toplevel_text(window, xdg_toplevel::request::SET_TITLE, title);
        self.defer(result);
    }

    /// Change a window's app id (`xdg_toplevel.set_app_id`). Anything but a
    /// window is left alone.
    pub fn set_app_id(&mut self, window: SurfaceId, app_id: &str) {
        let result = self.toplevel_text(window, xdg_toplevel::request::SET_APP_ID, app_id);
        self.defer(result);
    }

    /// Make a window a dialog of another (`xdg_toplevel.set_parent`), or,
    /// with `None`, a window of its own again. Anything but two windows is
    /// left alone.
    pub fn set_parent(&mut self, window: SurfaceId, parent: Option<SurfaceId>) {
        let toplevel_of = |id: SurfaceId| match self.surfaces.get(&id).map(|state| &state.kind) {
            Some(Kind::Toplevel { toplevel, .. }) => Some(*toplevel),
            _ => None,
        };
        let Some(toplevel) = toplevel_of(window) else {
            return;
        };
        let parent = match parent {
            Some(parent) => match toplevel_of(parent) {
                Some(object) => object,
                None => return,
            },
            None => ObjectId::NULL,
        };
        let result = self.send(
            toplevel,
            xdg_toplevel::request::SET_PARENT,
            &[Arg::Object(parent)],
        );
        self.defer(result);
    }

    fn toplevel_text(&mut self, window: SurfaceId, opcode: u16, text: &str) -> Result<(), Error> {
        let Some(SurfaceState {
            kind: Kind::Toplevel {
                toplevel, options, ..
            },
            ..
        }) = self.surfaces.get_mut(&window)
        else {
            return Ok(());
        };
        if opcode == xdg_toplevel::request::SET_TITLE {
            text.clone_into(&mut options.title);
        } else {
            text.clone_into(&mut options.app_id);
        }
        let toplevel = *toplevel;
        self.send(toplevel, opcode, &[Arg::Str(Some(text))])
    }

    /// Move a popup (`xdg_popup.reposition`, version 3); where the
    /// compositor's `xdg_wm_base` is older, the popup is made again at the
    /// new place under the same [`SurfaceId`].
    pub fn reposition_popup(&mut self, popup: SurfaceId, options: &PopupOptions) {
        let result = self.reposition(popup, options);
        self.defer(result);
    }

    fn reposition(&mut self, id: SurfaceId, options: &PopupOptions) -> Result<(), Error> {
        let (base, version) = self.global("xdg_wm_base")?;
        let Some(SurfaceState {
            kind: Kind::Popup { popup, parent, .. },
            ..
        }) = self.surfaces.get(&id)
        else {
            return Ok(());
        };
        let (popup, parent) = (*popup, *parent);
        let popup_version = self
            .objects
            .get(&popup.0)
            .map_or(0, |object| object.version);
        if popup_version >= 3 {
            let positioner = self.positioner(base, version, options)?;
            self.send(
                popup,
                xdg_popup::request::REPOSITION,
                &[Arg::Object(positioner), Arg::Uint(0)],
            )?;
            self.destroy_object(positioner, xdg_positioner::request::DESTROY);
            if let Some(SurfaceState {
                kind: Kind::Popup { options: kept, .. },
                ..
            }) = self.surfaces.get_mut(&id)
            {
                *kept = *options;
            }
            return Ok(());
        }
        // Remade: the same number for the program, new objects underneath.
        self.destroy(id);
        let made = self.popup(parent, options)?;
        if let Some(state) = self.surfaces.remove(&made) {
            let _ = self.surfaces.insert(id, state);
            self.retarget(made, id);
        }
        Ok(())
    }

    /// Destroy any surface this made, with its role object and buffers.
    pub fn destroy(&mut self, surface: SurfaceId) {
        let Some(state) = self.surfaces.remove(&surface) else {
            return;
        };
        match state.kind {
            Kind::Layer { role, .. } => {
                self.destroy_object(role, zwlr_layer_surface_v1::request::DESTROY);
            }
            Kind::Lock { role } => {
                self.destroy_object(role, ext_session_lock_surface_v1::request::DESTROY);
            }
            Kind::Popup { xdg, popup, .. } => {
                self.destroy_object(popup, xdg_popup::request::DESTROY);
                self.destroy_object(xdg, xdg_surface::request::DESTROY);
            }
            Kind::Toplevel { xdg, toplevel, .. } => {
                self.destroy_object(toplevel, xdg_toplevel::request::DESTROY);
                self.destroy_object(xdg, xdg_surface::request::DESTROY);
            }
        }
        for slot in state.slots {
            self.destroy_slot(slot);
        }
        self.destroy_object(state.surface, wl_surface::request::DESTROY);
        if self.seat.focus == Some(surface) {
            self.seat.focus = None;
            self.seat.held = None;
        }
        if self.seat.pointer_focus == Some(surface) {
            self.seat.pointer_focus = None;
        }
    }

    /// The last configured size of a surface in logical pixels.
    #[must_use]
    pub fn size(&self, surface: SurfaceId) -> Option<(u32, u32)> {
        self.surfaces
            .get(&surface)
            .and_then(|state| state.configured)
    }

    /// The integer scale its buffers are drawn at.
    #[must_use]
    pub fn scale(&self, surface: SurfaceId) -> u32 {
        self.surfaces.get(&surface).map_or(1, |state| state.scale)
    }

    /// The screen a surface is on, once the compositor has said
    /// (`wl_surface.enter`).
    #[must_use]
    pub fn surface_output(&self, surface: SurfaceId) -> Option<OutputId> {
        self.surfaces
            .get(&surface)
            .and_then(|state| state.entered.first().copied())
    }

    /// The `wl_surface` object behind a surface, for a request of the
    /// program's own that names one.
    #[must_use]
    pub fn surface_object(&self, surface: SurfaceId) -> Option<ObjectId> {
        self.surfaces.get(&surface).map(|state| state.surface)
    }

    /// Draw a frame: `paint` is given the surface's pixels at its configured
    /// size times its scale, cleared to transparent, in tiny-skia's
    /// premultiplied RGBA; then the frame is copied into a free `wl_shm`
    /// buffer, attached, damaged whole and committed.
    ///
    /// Nothing is drawn before the first [`Event::Configure`], and a size of
    /// zero draws nothing; both answer `Ok(false)`.
    ///
    /// # Errors
    ///
    /// Shared memory that cannot be made.
    pub fn draw(
        &mut self,
        surface: SurfaceId,
        paint: impl FnOnce(&mut tiny_skia::PixmapMut<'_>),
    ) -> Result<bool, Error> {
        let Some(state) = self.surfaces.get(&surface) else {
            return Ok(false);
        };
        let Some((width, height)) = state.configured else {
            return Ok(false);
        };
        let scale = state.scale.max(1);
        let (Some(pixel_width), Some(pixel_height)) =
            (width.checked_mul(scale), height.checked_mul(scale))
        else {
            return Ok(false);
        };
        self.present(surface, (pixel_width, pixel_height), scale, paint)
    }

    /// Draw a frame of a size of the program's own rather than the
    /// configured one: `width` × `height` buffer pixels at a buffer scale
    /// of 1, which is also the surface's size in logical pixels. For a
    /// program whose windows have sizes of their own that the compositor's
    /// configures only ask it to change, as an X server's do: until the
    /// window has taken the size asked for, it shows at the size it has.
    ///
    /// As [`Client::draw`], nothing is drawn before the first
    /// [`Event::Configure`] or at a size of zero.
    ///
    /// # Errors
    ///
    /// Shared memory that cannot be made.
    pub fn draw_sized(
        &mut self,
        surface: SurfaceId,
        (width, height): (u32, u32),
        paint: impl FnOnce(&mut tiny_skia::PixmapMut<'_>),
    ) -> Result<bool, Error> {
        if self
            .surfaces
            .get(&surface)
            .is_none_or(|state| state.configured.is_none())
        {
            return Ok(false);
        }
        self.present(surface, (width, height), 1, paint)
    }

    /// Paint a `pixel_width` × `pixel_height` frame, copy it into a free
    /// buffer, and attach, damage and commit it at buffer scale `scale`.
    fn present(
        &mut self,
        surface: SurfaceId,
        (pixel_width, pixel_height): (u32, u32),
        scale: u32,
        paint: impl FnOnce(&mut tiny_skia::PixmapMut<'_>),
    ) -> Result<bool, Error> {
        let Some(len) = buffer::length(pixel_width, pixel_height) else {
            return Ok(false);
        };
        let Some(state) = self.surfaces.get_mut(&surface) else {
            return Ok(false);
        };
        let mut scratch = core::mem::take(&mut state.scratch);
        scratch.clear();
        scratch.resize(len, 0);
        {
            let Some(mut pixmap) =
                tiny_skia::PixmapMut::from_bytes(&mut scratch, pixel_width, pixel_height)
            else {
                return Ok(false);
            };
            paint(&mut pixmap);
        }
        let at = self.free_slot(surface, pixel_width, pixel_height, len)?;
        let Some(state) = self.surfaces.get_mut(&surface) else {
            return Ok(false);
        };
        let Some(slot) = state.slots.get_mut(at) else {
            return Ok(false);
        };
        buffer::swizzle_into(&scratch, slot.shared.bytes_mut());
        slot.busy = true;
        let buffer = slot.buffer;
        state.scratch = scratch;
        let object = state.surface;
        let resend_scale = state.sent_scale != scale;
        state.sent_scale = scale;
        let version = self.objects.get(&object.0).map_or(1, |entry| entry.version);
        if resend_scale && version >= 3 {
            self.send(object, wl_surface::request::SET_BUFFER_SCALE, &[int(scale)])?;
        }
        self.send(
            object,
            wl_surface::request::ATTACH,
            &[Arg::Object(buffer), Arg::Int(0), Arg::Int(0)],
        )?;
        if version >= 4 {
            self.send(
                object,
                wl_surface::request::DAMAGE_BUFFER,
                &[
                    Arg::Int(0),
                    Arg::Int(0),
                    int(pixel_width),
                    int(pixel_height),
                ],
            )?;
        } else {
            self.send(
                object,
                wl_surface::request::DAMAGE,
                &[
                    Arg::Int(0),
                    Arg::Int(0),
                    int(pixel_width / scale),
                    int(pixel_height / scale),
                ],
            )?;
        }
        self.send(object, wl_surface::request::COMMIT, &[])?;
        Ok(true)
    }

    /// Ask for a [`Event::Frame`] when the compositor next wants a frame
    /// of this surface. Before a surface's first [`Client::draw`] the
    /// request rides on that draw's commit; after it, it is committed at
    /// once, so asking right after drawing works as asking before does.
    pub fn request_frame(&mut self, surface: SurfaceId) {
        let Some(state) = self.surfaces.get(&surface) else {
            return;
        };
        let (object, drawn) = (
            state.surface,
            state.sent_scale != 0 && !state.slots.is_empty(),
        );
        let callback = self.make(&wl::WL_CALLBACK, 1, Role::Frame(surface));
        let mut result = self.send(object, wl_surface::request::FRAME, &[Arg::NewId(callback)]);
        if drawn && result.is_ok() {
            result = self.send(object, wl_surface::request::COMMIT, &[]);
        }
        self.defer(result);
    }

    /// Where the surface takes pointer input: `None` everywhere (the
    /// default), `Some(&[])` nowhere, so clicks pass through.
    pub fn set_input_region(&mut self, surface: SurfaceId, region: Option<&[Rect]>) {
        let Some(object) = self.surfaces.get(&surface).map(|state| state.surface) else {
            return;
        };
        let result = self.input_region(object, region);
        self.defer(result);
    }

    fn input_region(&mut self, surface: ObjectId, region: Option<&[Rect]>) -> Result<(), Error> {
        let Some(rects) = region else {
            return self.send(
                surface,
                wl_surface::request::SET_INPUT_REGION,
                &[Arg::Object(ObjectId::NULL)],
            );
        };
        let (compositor, _) = self.global("wl_compositor")?;
        let made = self.make(&wl::WL_REGION, 1, Role::Quiet);
        self.send(
            compositor,
            wl_compositor::request::CREATE_REGION,
            &[Arg::NewId(made)],
        )?;
        for rect in rects {
            self.send(
                made,
                wl_region::request::ADD,
                &[
                    Arg::Int(rect.x),
                    Arg::Int(rect.y),
                    Arg::Int(rect.width),
                    Arg::Int(rect.height),
                ],
            )?;
        }
        self.send(
            surface,
            wl_surface::request::SET_INPUT_REGION,
            &[Arg::Object(made)],
        )?;
        self.destroy_object(made, wl_region::request::DESTROY);
        Ok(())
    }

    // -- the seat -----------------------------------------------------------

    /// Show `shape` for the pointer over this client's surfaces, through
    /// `wp_cursor_shape_v1` with the last enter's serial. Without the
    /// protocol this does nothing: the compositor's own arrow stays.
    pub fn set_cursor(&mut self, shape: CursorShape) {
        self.seat.cursor = shape;
        self.seat.hidden = false;
        if let Some(image) = self.seat.image.take() {
            self.drop_cursor_image(image, true);
        }
        let result = self.apply_cursor();
        self.defer(result);
    }

    /// Show a picture of the program's own for the pointer over this
    /// client's surfaces (`wl_pointer.set_cursor` with a surface), as an X
    /// server does with its clients' cursors. `pixels` is `width` × `height`
    /// in `wl_shm`'s ARGB8888, premultiplied: four bytes a pixel, blue first
    /// in memory. `hot` is the point of it that is the pointer's position.
    /// It stays, across the pointer leaving and coming back, until the next
    /// call or [`Client::set_cursor`].
    ///
    /// # Errors
    ///
    /// A size of zero, `pixels` of another length, or shared memory that
    /// cannot be made.
    pub fn set_cursor_image(
        &mut self,
        width: u32,
        height: u32,
        hot: (i32, i32),
        pixels: &[u8],
    ) -> Result<(), Error> {
        let len = buffer::length(width, height)
            .filter(|&len| len == pixels.len())
            .ok_or_else(|| {
                Error::Other(format!(
                    "a {width}x{height} cursor of {} bytes",
                    pixels.len()
                ))
            })?;
        let surface = match self.seat.image.as_ref() {
            Some(image) => image.surface,
            None => {
                let (compositor, version) = self.global("wl_compositor")?;
                let surface = self.make(&wl::WL_SURFACE, version, Role::Quiet);
                self.send(
                    compositor,
                    wl_compositor::request::CREATE_SURFACE,
                    &[Arg::NewId(surface)],
                )?;
                surface
            }
        };
        // ARGB8888 is wl_shm format 0.
        let (pool, buffer, mut shared) = self.shm_buffer(width, height, len, 0, Role::Quiet)?;
        shared.bytes_mut().copy_from_slice(pixels);
        self.send(
            surface,
            wl_surface::request::ATTACH,
            &[Arg::Object(buffer), Arg::Int(0), Arg::Int(0)],
        )?;
        self.send(
            surface,
            wl_surface::request::DAMAGE,
            &[Arg::Int(0), Arg::Int(0), int(width), int(height)],
        )?;
        self.send(surface, wl_surface::request::COMMIT, &[])?;
        let image = CursorImage {
            surface,
            buffer,
            pool,
            shared,
            hot,
        };
        if let Some(old) = self.seat.image.replace(image) {
            self.drop_cursor_image(old, false);
        }
        self.seat.hidden = false;
        self.apply_cursor()
    }

    /// Let go of a cursor picture's buffer, and of its surface too when the
    /// picture is not being replaced.
    fn drop_cursor_image(&mut self, image: CursorImage, surface: bool) {
        self.destroy_object(image.buffer, wl_buffer::request::DESTROY);
        self.destroy_object(image.pool, wl_shm_pool::request::DESTROY);
        drop(image.shared);
        if surface {
            self.destroy_object(image.surface, wl_surface::request::DESTROY);
        }
    }

    /// Hide the pointer over this client's surfaces (`wl_pointer.set_cursor`
    /// with no surface), as hyprlock's `hide_cursor` does.
    pub fn hide_cursor(&mut self) {
        self.seat.hidden = true;
        let result = self.apply_cursor();
        self.defer(result);
    }

    fn apply_cursor(&mut self) -> Result<(), Error> {
        if self.seat.pointer_focus.is_none() {
            return Ok(());
        }
        let serial = self.seat.enter_serial;
        if self.seat.hidden {
            if let Some(pointer) = self.seat.pointer {
                self.send(
                    pointer,
                    wl_pointer::request::SET_CURSOR,
                    &[
                        Arg::Uint(serial),
                        Arg::Object(ObjectId::NULL),
                        Arg::Int(0),
                        Arg::Int(0),
                    ],
                )?;
            }
            return Ok(());
        }
        if let (Some(pointer), Some(image)) = (self.seat.pointer, self.seat.image.as_ref()) {
            let (surface, (x, y)) = (image.surface, image.hot);
            return self.send(
                pointer,
                wl_pointer::request::SET_CURSOR,
                &[
                    Arg::Uint(serial),
                    Arg::Object(surface),
                    Arg::Int(x),
                    Arg::Int(y),
                ],
            );
        }
        if let Some(device) = self.seat.cursor_device {
            self.send(
                device,
                wp_cursor_shape_device_v1::request::SET_SHAPE,
                &[Arg::Uint(serial), Arg::Uint(self.seat.cursor.wire())],
            )?;
        }
        Ok(())
    }

    /// The layout of the keyboard's group in force, and the group's index,
    /// once a keymap has arrived: hyprlock's `$LAYOUT` is its `label`.
    #[must_use]
    pub fn keyboard_layout(&self) -> Option<(&'static compositor_xkb::generated::Layout, u32)> {
        let layout = self.layout_in_force()?;
        Some((layout, self.seat.group))
    }

    fn layout_in_force(&self) -> Option<&'static compositor_xkb::generated::Layout> {
        let group = usize::try_from(self.seat.group).unwrap_or(0);
        self.seat
            .layouts
            .get(group)
            .or_else(|| self.seat.layouts.first())
            .copied()
    }

    // -- idle ---------------------------------------------------------------

    /// An `ext_idle_notification_v1`: [`Event::Idled`] after `timeout`
    /// without input, [`Event::Resumed`] at the next. `respect_inhibitors`
    /// false asks for `get_input_idle_notification` (version 2), which
    /// ignores `zwp_idle_inhibitor_v1`; where the compositor offers only
    /// version 1 the plain one is made and the difference said once.
    ///
    /// # Errors
    ///
    /// [`Error::Missing`] without `ext_idle_notifier_v1`.
    pub fn idle_notification(
        &mut self,
        timeout: Duration,
        respect_inhibitors: bool,
    ) -> Result<IdleId, Error> {
        let (notifier, version) = self.global("ext_idle_notifier_v1")?;
        let seat = self.seat.seat.ok_or(Error::Missing("wl_seat"))?;
        let id = IdleId(self.next_idle);
        self.next_idle = self.next_idle.wrapping_add(1);
        let notification = self.make(&idle_notify::EXT_IDLE_NOTIFICATION_V1, 1, Role::Idle(id));
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let opcode = if respect_inhibitors {
            ext_idle_notifier_v1::request::GET_IDLE_NOTIFICATION
        } else if version >= 2 {
            ext_idle_notifier_v1::request::GET_INPUT_IDLE_NOTIFICATION
        } else {
            self.say_once(
                "idle-v1",
                "ext_idle_notifier_v1 is version 1 here: idle inhibitors are respected",
            );
            ext_idle_notifier_v1::request::GET_IDLE_NOTIFICATION
        };
        self.send(
            notifier,
            opcode,
            &[
                Arg::NewId(notification),
                Arg::Uint(millis),
                Arg::Object(seat),
            ],
        )?;
        let _ = self.idles.insert(id, notification);
        Ok(id)
    }

    /// Destroy an idle notification.
    pub fn destroy_idle(&mut self, idle: IdleId) {
        if let Some(object) = self.idles.remove(&idle) {
            self.destroy_object(object, ext_idle_notification_v1::request::DESTROY);
        }
    }

    // -- the loop -----------------------------------------------------------

    /// A timer that comes due after `after`, and then every `every` if one
    /// is given.
    pub fn add_timer(&mut self, after: Duration, every: Option<Duration>) -> TimerId {
        self.sources.add_timer(after, every)
    }

    /// Stop a timer. A timer already due in this pass is still reported.
    pub fn cancel_timer(&mut self, timer: TimerId) {
        self.sources.cancel_timer(timer);
    }

    /// Start `command` with `/bin/sh -c`, in its own process group, with
    /// `PR_SET_PDEATHSIG` so it goes when this program does, its standard
    /// output piped back as [`Event::ChildLine`]s or one
    /// [`Event::ChildExited`], and its standard error this program's.
    ///
    /// # Errors
    ///
    /// The pipe or the `fork` failing.
    pub fn run(&mut self, command: &Command) -> Result<ChildId, Error> {
        Ok(self.children.run(command)?)
    }

    /// Send `SIGTERM` to a child's process group. Its [`Event::ChildExited`]
    /// still arrives.
    pub fn kill(&mut self, child: ChildId) {
        self.children.kill(child);
    }

    /// Deliver these signals as [`Event::Signal`] rather than their default
    /// action, through `signalfd4`: they are blocked in this thread, which
    /// must be the only one. `SIGRTMIN+N` is `libc::SIGRTMIN() + N`.
    ///
    /// # Errors
    ///
    /// `signalfd4` failing.
    pub fn watch_signals(&mut self, signals: &[i32]) -> Result<(), Error> {
        Ok(self.sources.watch_signals(signals)?)
    }

    /// Report [`Event::Readable`] whenever `fd` is readable. The program
    /// keeps owning `fd` and must [`Client::unwatch_fd`] before closing it.
    pub fn watch_fd(&mut self, fd: i32) -> WatchId {
        self.sources.watch_fd(fd)
    }

    /// Stop watching a descriptor.
    pub fn unwatch_fd(&mut self, watch: WatchId) {
        self.sources.unwatch_fd(watch);
    }

    /// Something another thread can wake this loop with.
    ///
    /// # Errors
    ///
    /// The `eventfd` failing.
    pub fn waker(&mut self) -> Result<Waker, Error> {
        Ok(self.sources.waker()?)
    }

    /// The compositor's socket, for a program whose own loop does the
    /// waiting: it polls this for reading and then calls
    /// [`Client::dispatch`] with a zero timeout, which reads what arrived
    /// without blocking. yserver's Wayland backend is one (docs/YSERVER.md).
    #[must_use]
    pub fn as_raw_fd(&self) -> i32 {
        self.connection.as_raw_fd()
    }

    /// Send what is queued, wait until something happens or `timeout`
    /// passes (`None` waits for ever), and hand back everything that
    /// happened -- held keys repeating included. An empty list is a timeout.
    ///
    /// # Errors
    ///
    /// The compositor closing the connection or refusing a request.
    pub fn dispatch(&mut self, timeout: Option<Duration>) -> Result<Vec<Event>, Error> {
        if let Some(error) = self.deferred.take() {
            return Err(error);
        }
        if self.pending.is_empty() {
            self.pump(timeout)?;
        }
        Ok(core::mem::take(&mut self.pending))
    }

    /// `wl_display.sync` and dispatch until it is answered: everything the
    /// compositor had to say about what was sent before it has been said.
    ///
    /// # Errors
    ///
    /// As [`Client::dispatch`].
    pub fn roundtrip(&mut self) -> Result<Vec<Event>, Error> {
        self.sync_and_wait()?;
        Ok(core::mem::take(&mut self.pending))
    }

    /// Send what is queued.
    ///
    /// # Errors
    ///
    /// Writing to the compositor failing.
    pub fn flush(&mut self) -> Result<(), Error> {
        if !self.out.is_empty() {
            let (bytes, fds) = self.out.take();
            self.connection
                .send(&bytes, &fds)
                .map_err(|error| Error::Other(format!("writing to the compositor: {error:?}")))?;
        }
        self.connection
            .flush()
            .map_err(|error| Error::Other(format!("writing to the compositor: {error:?}")))
    }

    // -- the program's own objects ------------------------------------------

    /// Bind a global of `interface` at the lower of `version` and what the
    /// compositor offers; the first one announced, or the one named `name`.
    /// Its events arrive as [`Event::Object`].
    ///
    /// # Errors
    ///
    /// [`Error::Missing`] when the compositor offers none.
    pub fn bind(
        &mut self,
        interface: &'static Interface,
        version: u32,
        name: Option<u32>,
    ) -> Result<(ObjectId, u32), Error> {
        let global = self
            .globals
            .iter()
            .find(|global| {
                global.interface == interface.name && name.is_none_or(|name| name == global.name)
            })
            .cloned()
            .ok_or(Error::Missing(interface.name))?;
        let version = version.min(global.version).min(interface.version).max(1);
        let id = self.make(interface, version, Role::User);
        self.bind_global(global.name, interface, version, id)?;
        Ok((id, version))
    }

    /// An id for a new object of `interface` at `version`, to pass as a
    /// `new_id` ([`Value::NewId`]) in the [`Client::request`] that makes it.
    pub fn new_object(&mut self, interface: &'static Interface, version: u32) -> ObjectId {
        self.make(interface, version, Role::User)
    }

    /// Send a request on an object the program made or was handed, read
    /// against that object's interface table. A destructor request forgets
    /// the object once the compositor confirms (`wl_display.delete_id`).
    ///
    /// # Errors
    ///
    /// An unknown object, an opcode it lacks, or arguments that do not
    /// match its signature.
    pub fn request(&mut self, object: ObjectId, opcode: u16, args: &[Value]) -> Result<(), Error> {
        let borrowed: Vec<Arg<'_>> = args.iter().map(Value::to_arg).collect();
        self.send(object, opcode, &borrowed)?;
        let destructor = self
            .objects
            .get(&object.0)
            .and_then(|entry| entry.interface.request(opcode))
            .is_some_and(|method| method.destructor);
        if destructor {
            self.mark_dead(object);
        }
        Ok(())
    }

    /// Say what an object the compositor made (a `new_id` in an event of the
    /// program's own objects) is, so its events can be read.
    pub fn adopt(&mut self, object: ObjectId, interface: &'static Interface, version: u32) {
        let _ = self.objects.insert(
            object.0,
            Object {
                interface,
                version,
                role: Role::User,
                dead: false,
            },
        );
    }

    /// A `wl_shm` buffer of `width` × `height` in `format` (a
    /// `wl_shm.format` value) the program fills itself, for a request that
    /// wants one (`zwlr_screencopy_frame_v1.copy`). The bytes stay mapped
    /// until [`Client::destroy_buffer`].
    ///
    /// # Errors
    ///
    /// Shared memory that cannot be made.
    pub fn new_buffer(
        &mut self,
        width: u32,
        height: u32,
        format: u32,
    ) -> Result<(ObjectId, u32), Error> {
        let len = buffer::length(width, height)
            .ok_or_else(|| Error::Other(format!("a {width}x{height} buffer")))?;
        let (pool, buffer, shared) = self.shm_buffer(width, height, len, format, Role::User)?;
        let _ = self
            .program_buffers
            .insert(buffer.0, ProgramBuffer { shared, pool });
        Ok((buffer, width.saturating_mul(4)))
    }

    /// The bytes of a buffer [`Client::new_buffer`] made: `stride` × height,
    /// the stride being its second return value.
    #[must_use]
    pub fn buffer_bytes(&mut self, buffer: ObjectId) -> Option<&mut [u8]> {
        self.program_buffers
            .get_mut(&buffer.0)
            .map(|made| made.shared.bytes_mut())
    }

    /// Destroy a buffer [`Client::new_buffer`] made.
    pub fn destroy_buffer(&mut self, buffer: ObjectId) {
        if let Some(made) = self.program_buffers.remove(&buffer.0) {
            self.destroy_object(buffer, wl_buffer::request::DESTROY);
            self.destroy_object(made.pool, wl_shm_pool::request::DESTROY);
        }
    }
}

// -- the machinery ----------------------------------------------------------

impl Client {
    /// A fresh object id of this client's, recorded as `interface`.
    fn make(&mut self, interface: &'static Interface, version: u32, role: Role) -> ObjectId {
        let id = match self.free_ids.pop() {
            Some(id) => id,
            None => {
                let id = self.next_id;
                self.next_id = self.next_id.saturating_add(1);
                id
            }
        };
        let _ = self.objects.insert(
            id,
            Object {
                interface,
                version,
                role,
                dead: false,
            },
        );
        ObjectId(id)
    }

    /// Queue one request, its signature from the object's own table.
    fn send(&mut self, object: ObjectId, opcode: u16, args: &[Arg<'_>]) -> Result<(), Error> {
        let entry = self
            .objects
            .get(&object.0)
            .ok_or_else(|| Error::Other(format!("no object {}", object.0)))?;
        let method = entry.interface.request(opcode).ok_or_else(|| {
            Error::Other(format!("{} has no request {opcode}", entry.interface.name))
        })?;
        if method.since > entry.version {
            return Err(Error::Other(format!(
                "{}.{} needs version {}, bound at {}",
                entry.interface.name, method.name, method.since, entry.version
            )));
        }
        self.out
            .write(object, opcode, method.signature, args)
            .map_err(|error| {
                Error::Other(format!(
                    "{}.{}: {error:?}",
                    entry.interface.name, method.name
                ))
            })
    }

    /// Send a destructor and keep the object until `delete_id`.
    fn destroy_object(&mut self, object: ObjectId, opcode: u16) {
        let result = self.send(object, opcode, &[]);
        self.defer(result);
        self.mark_dead(object);
    }

    fn mark_dead(&mut self, object: ObjectId) {
        if let Some(entry) = self.objects.get_mut(&object.0) {
            entry.dead = true;
        }
    }

    /// Keep the first failure of a call that has nowhere to say it.
    fn defer(&mut self, result: Result<(), Error>) {
        if let Err(error) = result
            && self.deferred.is_none()
        {
            self.deferred = Some(error);
        }
    }

    fn say_once(&mut self, key: &'static str, line: &str) {
        if self.said.insert(key) {
            use std::io::Write as _;
            let _ = writeln!(std::io::stderr(), "toolkit: {line}");
        }
    }

    /// One of this crate's own bindings.
    fn global(&self, interface: &'static str) -> Result<(ObjectId, u32), Error> {
        self.bound
            .get(interface)
            .copied()
            .ok_or(Error::Missing(interface))
    }

    fn bind_global(
        &mut self,
        name: u32,
        interface: &'static Interface,
        version: u32,
        id: ObjectId,
    ) -> Result<(), Error> {
        let registry = ObjectId(2);
        self.send(
            registry,
            wl_registry::request::BIND,
            &[
                Arg::Uint(name),
                Arg::AnyNewId {
                    interface: interface.name,
                    version,
                    id,
                },
            ],
        )
    }

    fn new_surface_id(&mut self) -> SurfaceId {
        let id = SurfaceId(self.next_surface);
        self.next_surface = self.next_surface.wrapping_add(1);
        id
    }

    fn create_surface(&mut self, id: SurfaceId) -> Result<ObjectId, Error> {
        let (compositor, version) = self.global("wl_compositor")?;
        let surface = self.make(&wl::WL_SURFACE, version, Role::Surface(id));
        self.send(
            compositor,
            wl_compositor::request::CREATE_SURFACE,
            &[Arg::NewId(surface)],
        )?;
        Ok(surface)
    }

    /// Send whatever of a layer surface's placement differs from `before`
    /// (everything, when there is no before).
    fn send_layer(
        &mut self,
        role: ObjectId,
        options: &LayerOptions,
        before: Option<&LayerOptions>,
    ) -> Result<(), Error> {
        let changed = |pick: &dyn Fn(&LayerOptions) -> bool| before.is_none_or(pick);
        if changed(&|old| old.size != options.size) {
            self.send(
                role,
                zwlr_layer_surface_v1::request::SET_SIZE,
                &[Arg::Uint(options.size.0), Arg::Uint(options.size.1)],
            )?;
        }
        if changed(&|old| old.anchor != options.anchor) {
            self.send(
                role,
                zwlr_layer_surface_v1::request::SET_ANCHOR,
                &[Arg::Uint(options.anchor.0)],
            )?;
        }
        if changed(&|old| old.exclusive_zone != options.exclusive_zone) {
            self.send(
                role,
                zwlr_layer_surface_v1::request::SET_EXCLUSIVE_ZONE,
                &[Arg::Int(options.exclusive_zone)],
            )?;
        }
        if changed(&|old| old.margin != options.margin) {
            let margin = options.margin;
            self.send(
                role,
                zwlr_layer_surface_v1::request::SET_MARGIN,
                &[
                    Arg::Int(margin.top),
                    Arg::Int(margin.right),
                    Arg::Int(margin.bottom),
                    Arg::Int(margin.left),
                ],
            )?;
        }
        if changed(&|old| old.keyboard != options.keyboard) {
            let version = self.objects.get(&role.0).map_or(1, |entry| entry.version);
            let mut wanted = options.keyboard;
            if wanted == crate::KeyboardInteractivity::OnDemand && version < 4 {
                self.say_once(
                    "on-demand",
                    "zwlr_layer_shell_v1 is older than version 4 here: on-demand keyboard focus is exclusive",
                );
                wanted = crate::KeyboardInteractivity::Exclusive;
            }
            self.send(
                role,
                zwlr_layer_surface_v1::request::SET_KEYBOARD_INTERACTIVITY,
                &[Arg::Uint(wanted.wire())],
            )?;
        }
        if let Some(old) = before
            && old.layer != options.layer
        {
            self.send(
                role,
                zwlr_layer_surface_v1::request::SET_LAYER,
                &[Arg::Uint(options.layer.wire())],
            )?;
        }
        Ok(())
    }

    fn positioner(
        &mut self,
        base: ObjectId,
        version: u32,
        options: &PopupOptions,
    ) -> Result<ObjectId, Error> {
        let positioner = self.make(&xdg_shell::XDG_POSITIONER, version, Role::Quiet);
        self.send(
            base,
            xdg_wm_base::request::CREATE_POSITIONER,
            &[Arg::NewId(positioner)],
        )?;
        self.send(
            positioner,
            xdg_positioner::request::SET_SIZE,
            &[
                Arg::Int(options.size.0.max(1)),
                Arg::Int(options.size.1.max(1)),
            ],
        )?;
        let rect = options.anchor_rect;
        self.send(
            positioner,
            xdg_positioner::request::SET_ANCHOR_RECT,
            &[
                Arg::Int(rect.x),
                Arg::Int(rect.y),
                Arg::Int(rect.width.max(1)),
                Arg::Int(rect.height.max(1)),
            ],
        )?;
        self.send(
            positioner,
            xdg_positioner::request::SET_ANCHOR,
            &[Arg::Uint(options.anchor)],
        )?;
        self.send(
            positioner,
            xdg_positioner::request::SET_GRAVITY,
            &[Arg::Uint(options.gravity)],
        )?;
        self.send(
            positioner,
            xdg_positioner::request::SET_CONSTRAINT_ADJUSTMENT,
            &[Arg::Uint(options.constraint_adjustment)],
        )?;
        self.send(
            positioner,
            xdg_positioner::request::SET_OFFSET,
            &[Arg::Int(options.offset.0), Arg::Int(options.offset.1)],
        )?;
        Ok(positioner)
    }

    /// Point every object of one surface's at another number, after a
    /// popup was remade under its old one.
    fn retarget(&mut self, from: SurfaceId, to: SurfaceId) {
        for entry in self.objects.values_mut() {
            entry.role = match entry.role {
                Role::Surface(id) if id == from => Role::Surface(to),
                Role::XdgSurface(id) if id == from => Role::XdgSurface(to),
                Role::Popup(id) if id == from => Role::Popup(to),
                Role::Toplevel(id) if id == from => Role::Toplevel(to),
                Role::Frame(id) if id == from => Role::Frame(to),
                Role::SurfaceBuffer(id) if id == from => Role::SurfaceBuffer(to),
                other => other,
            };
        }
    }

    /// A pool and a buffer over new shared memory.
    fn shm_buffer(
        &mut self,
        width: u32,
        height: u32,
        len: usize,
        format: u32,
        role: Role,
    ) -> Result<(ObjectId, ObjectId, Shared), Error> {
        let (shm, _) = self.global("wl_shm")?;
        let shared = Shared::new(len)?;
        let pool = self.make(&wl::WL_SHM_POOL, 1, Role::Quiet);
        self.send(
            shm,
            wl_shm::request::CREATE_POOL,
            &[
                Arg::NewId(pool),
                Arg::Fd(Fd(shared.as_raw_fd())),
                Arg::Int(i32::try_from(len).unwrap_or(i32::MAX)),
            ],
        )?;
        // The descriptor has to be sent before `shared` could be dropped:
        // the connection copies it now.
        self.flush()?;
        let buffer = self.make(&wl::WL_BUFFER, 1, role);
        self.send(
            pool,
            wl_shm_pool::request::CREATE_BUFFER,
            &[
                Arg::NewId(buffer),
                Arg::Int(0),
                int(width),
                int(height),
                int(width.saturating_mul(4)),
                Arg::Uint(format),
            ],
        )?;
        Ok((pool, buffer, shared))
    }

    /// The index of a free buffer of this size on `surface`, made if need
    /// be; buffers of another size that are free are destroyed.
    fn free_slot(
        &mut self,
        surface: SurfaceId,
        width: u32,
        height: u32,
        len: usize,
    ) -> Result<usize, Error> {
        let Some(state) = self.surfaces.get_mut(&surface) else {
            return Err(Error::Other("no such surface".to_owned()));
        };
        let mut stale = Vec::new();
        let mut kept = Vec::new();
        for slot in state.slots.drain(..) {
            if !slot.busy && (slot.width != width || slot.height != height) {
                stale.push(slot);
            } else {
                kept.push(slot);
            }
        }
        state.slots = kept;
        let found = state
            .slots
            .iter()
            .position(|slot| !slot.busy && slot.width == width && slot.height == height);
        for slot in stale {
            self.destroy_slot(slot);
        }
        if let Some(at) = found {
            return Ok(at);
        }
        // ARGB8888 is wl_shm format 0.
        let (pool, buffer, shared) =
            self.shm_buffer(width, height, len, 0, Role::SurfaceBuffer(surface))?;
        let Some(state) = self.surfaces.get_mut(&surface) else {
            return Err(Error::Other("no such surface".to_owned()));
        };
        state.slots.push(Slot {
            shared,
            pool,
            buffer,
            width,
            height,
            busy: false,
        });
        Ok(state.slots.len().saturating_sub(1))
    }

    fn destroy_slot(&mut self, slot: Slot) {
        self.destroy_object(slot.buffer, wl_buffer::request::DESTROY);
        self.destroy_object(slot.pool, wl_shm_pool::request::DESTROY);
    }

    fn scale_of_outputs(&self, outputs: &[OutputId]) -> u32 {
        outputs
            .iter()
            .filter_map(|id| self.outputs.get(id))
            .map(|output| u32::try_from(output.info.scale.max(1)).unwrap_or(1))
            .max()
            .unwrap_or(1)
    }

    /// Work out a surface's scale again, and say so if it changed.
    fn rescale(&mut self, surface: SurfaceId) {
        let Some(state) = self.surfaces.get(&surface) else {
            return;
        };
        let scale = match state.preferred {
            Some(scale) => scale.max(1),
            None => self.scale_of_outputs(&state.entered),
        };
        if let Some(state) = self.surfaces.get_mut(&surface)
            && state.scale != scale
        {
            state.scale = scale;
            self.pending.push(Event::Scale { surface, scale });
        }
    }

    /// Sync and read until answered.
    fn sync_and_wait(&mut self) -> Result<(), Error> {
        let token = self.next_sync;
        self.next_sync = self.next_sync.wrapping_add(1);
        let callback = self.make(&wl::WL_CALLBACK, 1, Role::Sync(token));
        self.send(
            ObjectId::DISPLAY,
            wl_display::request::SYNC,
            &[Arg::NewId(callback)],
        )?;
        while !self.synced.remove(&token) {
            self.pump(None)?;
            if let Some(error) = self.deferred.take() {
                return Err(error);
            }
        }
        Ok(())
    }

    /// One wait: flush, poll everything, and turn what happened into
    /// events in `pending`.
    fn pump(&mut self, timeout: Option<Duration>) -> Result<(), Error> {
        self.flush()?;
        let now = Instant::now();
        let mut wait = timeout;
        let mut sooner = |due: Instant| {
            let left = due.saturating_duration_since(now);
            wait = Some(wait.map_or(left, |wait| wait.min(left)));
        };
        if let Some(due) = self.sources.next_due() {
            sooner(due);
        }
        if let Some(held) = self.seat.held {
            sooner(held.next);
        }
        if self.children.reaping() {
            sooner(now + Duration::from_millis(20));
        }
        let wayland = self.connection.as_raw_fd();
        let wants_write = self.connection.has_pending_writes();
        let ready = self
            .sources
            .poll(wayland, wants_write, &self.children, wait)?;
        if ready.wayland {
            self.receive()?;
        }
        self.sources.collect(&ready, &mut self.pending);
        self.children.collect(&ready.children, &mut self.pending);
        self.sources.fire_timers(&mut self.pending);
        self.autorepeat();
        Ok(())
    }

    /// Read what the socket has and handle every whole message.
    fn receive(&mut self) -> Result<(), Error> {
        loop {
            match self.connection.receive() {
                Ok(0) => break,
                Ok(_) => {}
                Err(RecvError::WouldBlock) => break,
                Err(RecvError::Closed) => return Err(Error::Closed),
                Err(error) => {
                    return Err(Error::Other(format!(
                        "reading from the compositor: {error:?}"
                    )));
                }
            }
        }
        let bytes = self.connection.bytes().to_vec();
        let fds = self.connection.fds();
        let mut reader = Reader::new(&bytes, &fds);
        let mut outcome = Ok(());
        while !reader.is_done() {
            let Ok(header) = reader.peek() else {
                break;
            };
            let Some(entry) = self.objects.get(&header.sender.0).copied() else {
                // An object this client never heard of: step over it. What
                // it would have carried in descriptors cannot be known, and
                // a compositor that sent one broke the protocol first.
                if reader.skip(0).is_err() {
                    break;
                }
                continue;
            };
            let Some(method) = entry.interface.event(header.opcode) else {
                outcome = Err(Error::Other(format!(
                    "{} has no event {}",
                    entry.interface.name, header.opcode
                )));
                break;
            };
            let args = match reader.read(method.signature) {
                Ok((_, args)) => args,
                Err(compositor_wire::Error::Incomplete { .. }) => break,
                Err(error) => {
                    outcome = Err(Error::Other(format!(
                        "{}.{}: {error:?}",
                        entry.interface.name, method.name
                    )));
                    break;
                }
            };
            if let Err(error) = self.event(header.sender, entry, header.opcode, &args) {
                outcome = Err(error);
                break;
            }
        }
        self.connection
            .consume(reader.consumed(), reader.descriptors_taken());
        outcome
    }

    /// Handle one event.
    fn event(
        &mut self,
        sender: ObjectId,
        entry: Object,
        opcode: u16,
        args: &[Arg<'_>],
    ) -> Result<(), Error> {
        let uint = |at: usize| args.get(at).and_then(Arg::as_uint).unwrap_or(0);
        let signed = |at: usize| args.get(at).and_then(Arg::as_int).unwrap_or(0);
        let text = |at: usize| args.get(at).and_then(Arg::as_str).unwrap_or("").to_owned();
        let object = |at: usize| {
            args.get(at)
                .and_then(Arg::as_object)
                .unwrap_or(ObjectId::NULL)
        };
        if entry.dead && entry.role != Role::Display {
            close_descriptors(args);
            return Ok(());
        }
        match entry.role {
            Role::Display => match opcode {
                wl_display::event::ERROR => {
                    return Err(Error::Protocol {
                        object: object(0).0,
                        code: uint(1),
                        message: text(2),
                    });
                }
                wl_display::event::DELETE_ID => {
                    let id = uint(0);
                    if self.objects.remove(&id).is_some() && id != 1 {
                        self.free_ids.push(id);
                    }
                }
                _ => {}
            },
            Role::Registry => match opcode {
                wl_registry::event::GLOBAL => self.global_added(uint(0), &text(1), uint(2))?,
                wl_registry::event::GLOBAL_REMOVE => self.global_removed(uint(0)),
                _ => {}
            },
            Role::Sync(token) => {
                let _ = self.synced.insert(token);
            }
            Role::Frame(surface) => {
                self.pending.push(Event::Frame {
                    surface,
                    time: uint(0),
                });
            }
            Role::Quiet => close_descriptors(args),
            Role::Seat if opcode == wl_seat::event::CAPABILITIES => self.capabilities(uint(0))?,
            Role::Seat => {}
            Role::Keyboard => self.keyboard_event(opcode, args)?,
            Role::Pointer => self.pointer_event(opcode, args, entry.version),
            Role::Output(id) => self.output_event(id, opcode, args),
            Role::XdgOutput(id) => {
                let Some(output) = self.outputs.get_mut(&id) else {
                    return Ok(());
                };
                match opcode {
                    zxdg_output_v1::event::LOGICAL_POSITION => {
                        output.info.logical_position = Some((signed(0), signed(1)));
                    }
                    zxdg_output_v1::event::LOGICAL_SIZE => {
                        output.info.logical_extent = Some((signed(0), signed(1)));
                    }
                    zxdg_output_v1::event::NAME => output.info.xdg_name = text(0),
                    zxdg_output_v1::event::DESCRIPTION => output.info.xdg_description = text(0),
                    _ => {}
                }
            }
            Role::WmBase if opcode == xdg_wm_base::event::PING => {
                self.send(sender, xdg_wm_base::request::PONG, &[Arg::Uint(uint(0))])?;
            }
            Role::WmBase => {}
            Role::XdgSurface(surface) if opcode == xdg_surface::event::CONFIGURE => {
                self.send(
                    sender,
                    xdg_surface::request::ACK_CONFIGURE,
                    &[Arg::Uint(uint(0))],
                )?;
                let size = match self.surfaces.get(&surface) {
                    Some(SurfaceState {
                        kind: Kind::Popup { pending, .. },
                        ..
                    }) => *pending,
                    // Zero is "you choose": the size the window has, or,
                    // before it has one, the size it asked for. A window the
                    // compositor sized and then let go (floated) keeps its
                    // size rather than snapping back.
                    Some(SurfaceState {
                        kind:
                            Kind::Toplevel {
                                pending, options, ..
                            },
                        configured,
                        ..
                    }) => {
                        let (width, height) = configured.unwrap_or(options.size);
                        (
                            if pending.0 == 0 { width } else { pending.0 },
                            if pending.1 == 0 { height } else { pending.1 },
                        )
                    }
                    _ => (0, 0),
                };
                self.configured(surface, size.0, size.1);
            }
            Role::XdgSurface(_) => {}
            Role::Popup(surface) => match opcode {
                xdg_popup::event::CONFIGURE => {
                    if let Some(SurfaceState {
                        kind: Kind::Popup { pending, .. },
                        ..
                    }) = self.surfaces.get_mut(&surface)
                    {
                        *pending = (nonnegative(signed(2)), nonnegative(signed(3)));
                    }
                }
                xdg_popup::event::POPUP_DONE => self.pending.push(Event::Closed(surface)),
                _ => {}
            },
            Role::Toplevel(surface) => match opcode {
                // The size (and the states, which this does not keep) the
                // `xdg_surface.configure` that follows commits to.
                xdg_toplevel::event::CONFIGURE => {
                    if let Some(SurfaceState {
                        kind: Kind::Toplevel { pending, .. },
                        ..
                    }) = self.surfaces.get_mut(&surface)
                    {
                        *pending = (nonnegative(signed(0)), nonnegative(signed(1)));
                    }
                }
                xdg_toplevel::event::CLOSE => self.pending.push(Event::CloseRequested(surface)),
                _ => {}
            },
            Role::LayerSurface(surface) => match opcode {
                zwlr_layer_surface_v1::event::CONFIGURE => {
                    self.send(
                        sender,
                        zwlr_layer_surface_v1::request::ACK_CONFIGURE,
                        &[Arg::Uint(uint(0))],
                    )?;
                    let (mut width, mut height) = (uint(1), uint(2));
                    if let Some(SurfaceState {
                        kind: Kind::Layer { options, .. },
                        ..
                    }) = self.surfaces.get(&surface)
                    {
                        // Zero is "you choose", which is what was asked for.
                        if width == 0 {
                            width = options.size.0;
                        }
                        if height == 0 {
                            height = options.size.1;
                        }
                    }
                    self.configured(surface, width, height);
                }
                zwlr_layer_surface_v1::event::CLOSED => self.pending.push(Event::Closed(surface)),
                _ => {}
            },
            Role::Surface(surface) => match opcode {
                wl_surface::event::ENTER | wl_surface::event::LEAVE => {
                    let output = self.output_of_object(object(0));
                    if let (Some(output), Some(state)) = (output, self.surfaces.get_mut(&surface)) {
                        state.entered.retain(|id| *id != output);
                        if opcode == wl_surface::event::ENTER {
                            state.entered.push(output);
                        }
                    }
                    self.rescale(surface);
                }
                wl_surface::event::PREFERRED_BUFFER_SCALE => {
                    if let Some(state) = self.surfaces.get_mut(&surface) {
                        state.preferred = Some(u32::try_from(signed(0).max(1)).unwrap_or(1));
                    }
                    self.rescale(surface);
                }
                _ => {}
            },
            Role::Lock => match opcode {
                ext_session_lock_v1::event::LOCKED => self.pending.push(Event::Locked),
                ext_session_lock_v1::event::FINISHED => self.pending.push(Event::LockFinished),
                _ => {}
            },
            Role::LockSurface(surface)
                if opcode == ext_session_lock_surface_v1::event::CONFIGURE =>
            {
                self.send(
                    sender,
                    ext_session_lock_surface_v1::request::ACK_CONFIGURE,
                    &[Arg::Uint(uint(0))],
                )?;
                self.configured(surface, uint(1), uint(2));
            }
            Role::LockSurface(_) => {}
            Role::Idle(idle) => match opcode {
                ext_idle_notification_v1::event::IDLED => self.pending.push(Event::Idled(idle)),
                ext_idle_notification_v1::event::RESUMED => {
                    self.pending.push(Event::Resumed(idle));
                }
                _ => {}
            },
            Role::SurfaceBuffer(surface) if opcode == wl_buffer::event::RELEASE => {
                if let Some(state) = self.surfaces.get_mut(&surface)
                    && let Some(slot) = state.slots.iter_mut().find(|slot| slot.buffer == sender)
                {
                    slot.busy = false;
                }
            }
            Role::SurfaceBuffer(_) => {}
            Role::User => self.pending.push(Event::Object {
                object: sender,
                interface: entry.interface.name,
                opcode,
                args: args.iter().map(Value::from_arg).collect(),
            }),
        }
        Ok(())
    }

    fn configured(&mut self, surface: SurfaceId, width: u32, height: u32) {
        if let Some(state) = self.surfaces.get_mut(&surface) {
            state.configured = Some((width, height));
            self.pending.push(Event::Configure {
                surface,
                width,
                height,
            });
        }
    }

    fn output_of_object(&self, object: ObjectId) -> Option<OutputId> {
        match self.objects.get(&object.0).map(|entry| entry.role) {
            Some(Role::Output(id)) => Some(id),
            _ => None,
        }
    }

    fn global_added(&mut self, name: u32, interface: &str, version: u32) -> Result<(), Error> {
        self.globals.push(Global {
            name,
            interface: interface.to_owned(),
            version,
        });
        let Some((_, spoken)) = SPOKEN.iter().find(|(known, _)| *known == interface) else {
            return Ok(());
        };
        let version = version.min(*spoken).max(1);
        if interface == "wl_output" {
            return self.output_added(name, version);
        }
        if self.bound.contains_key(interface) {
            return Ok(());
        }
        let (table, role): (&'static Interface, Role) = match interface {
            "wl_compositor" => (&wl::WL_COMPOSITOR, Role::Quiet),
            "wl_shm" => (&wl::WL_SHM, Role::Quiet),
            "wl_seat" => (&wl::WL_SEAT, Role::Seat),
            "xdg_wm_base" => (&xdg_shell::XDG_WM_BASE, Role::WmBase),
            "zwlr_layer_shell_v1" => (&layer_shell::ZWLR_LAYER_SHELL_V1, Role::Quiet),
            "ext_session_lock_manager_v1" => {
                (&session_lock::EXT_SESSION_LOCK_MANAGER_V1, Role::Quiet)
            }
            "wp_cursor_shape_manager_v1" => {
                (&cursor_shape::WP_CURSOR_SHAPE_MANAGER_V1, Role::Quiet)
            }
            "ext_idle_notifier_v1" => (&idle_notify::EXT_IDLE_NOTIFIER_V1, Role::Quiet),
            "zxdg_output_manager_v1" => (&xdg_output::ZXDG_OUTPUT_MANAGER_V1, Role::Quiet),
            _ => return Ok(()),
        };
        let version = version.min(table.version);
        let id = self.make(table, version, role);
        self.bind_global(name, table, version, id)?;
        let _ = self.bound.insert(table.name, (id, version));
        match interface {
            "wl_seat" => self.seat.seat = Some(id),
            "zxdg_output_manager_v1" => {
                let outputs: Vec<OutputId> = self.outputs.keys().copied().collect();
                for output in outputs {
                    self.xdg_output_for(output)?;
                }
            }
            "wp_cursor_shape_manager_v1" => self.cursor_device()?,
            _ => {}
        }
        Ok(())
    }

    fn output_added(&mut self, name: u32, version: u32) -> Result<(), Error> {
        let id = OutputId(self.next_output);
        self.next_output = self.next_output.wrapping_add(1);
        let object = self.make(&wl::WL_OUTPUT, version, Role::Output(id));
        self.bind_global(name, &wl::WL_OUTPUT, version, object)?;
        let _ = self.outputs.insert(
            id,
            OutputState {
                global: name,
                object,
                xdg: None,
                info: Output {
                    id: Some(id),
                    scale: 1,
                    ..Output::default()
                },
                announced: false,
            },
        );
        self.xdg_output_for(id)
    }

    fn xdg_output_for(&mut self, output: OutputId) -> Result<(), Error> {
        let Ok((manager, version)) = self.global("zxdg_output_manager_v1") else {
            return Ok(());
        };
        let Some(state) = self.outputs.get(&output) else {
            return Ok(());
        };
        if state.xdg.is_some() {
            return Ok(());
        }
        let wl = state.object;
        let xdg = self.make(
            &xdg_output::ZXDG_OUTPUT_V1,
            version,
            Role::XdgOutput(output),
        );
        self.send(
            manager,
            zxdg_output_manager_v1::request::GET_XDG_OUTPUT,
            &[Arg::NewId(xdg), Arg::Object(wl)],
        )?;
        if let Some(state) = self.outputs.get_mut(&output) {
            state.xdg = Some(xdg);
        }
        Ok(())
    }

    fn global_removed(&mut self, name: u32) {
        self.globals.retain(|global| global.name != name);
        let gone: Option<OutputId> = self
            .outputs
            .iter()
            .find(|(_, output)| output.global == name)
            .map(|(id, _)| *id);
        if let Some(id) = gone
            && let Some(output) = self.outputs.remove(&id)
        {
            if let Some(xdg) = output.xdg {
                self.destroy_object(xdg, zxdg_output_v1::request::DESTROY);
            }
            let version = self
                .objects
                .get(&output.object.0)
                .map_or(1, |entry| entry.version);
            if version >= 3 {
                self.destroy_object(output.object, wl_output::request::RELEASE);
            } else {
                self.mark_dead(output.object);
            }
            if output.announced {
                self.pending.push(Event::OutputRemoved(id));
            }
        }
    }

    fn output_event(&mut self, id: OutputId, opcode: u16, args: &[Arg<'_>]) {
        let signed = |at: usize| args.get(at).and_then(Arg::as_int).unwrap_or(0);
        let uint = |at: usize| args.get(at).and_then(Arg::as_uint).unwrap_or(0);
        let text = |at: usize| args.get(at).and_then(Arg::as_str).unwrap_or("").to_owned();
        let Some(output) = self.outputs.get_mut(&id) else {
            return;
        };
        let info = &mut output.info;
        match opcode {
            wl_output::event::GEOMETRY => {
                info.position = (signed(0), signed(1));
                info.physical_mm = (signed(2), signed(3));
                info.make = text(5);
                info.model = text(6);
                info.transform = Transform::from_wire(signed(7));
            }
            wl_output::event::MODE => {
                // Only the current mode: the others are what it could be.
                if uint(0) & 1 != 0 {
                    info.mode = (signed(1), signed(2));
                    info.refresh_mhz = signed(3);
                }
            }
            wl_output::event::SCALE => info.scale = signed(0).max(1),
            wl_output::event::NAME => info.name = text(0),
            wl_output::event::DESCRIPTION => info.description = text(0),
            wl_output::event::DONE => {
                info.done = true;
                if output.announced {
                    self.pending.push(Event::OutputChanged(id));
                } else {
                    output.announced = true;
                    self.pending.push(Event::OutputAdded(id));
                }
                let on: Vec<SurfaceId> = self
                    .surfaces
                    .iter()
                    .filter(|(_, state)| state.entered.contains(&id))
                    .map(|(surface, _)| *surface)
                    .collect();
                for surface in on {
                    self.rescale(surface);
                }
            }
            _ => {}
        }
    }

    fn capabilities(&mut self, capabilities: u32) -> Result<(), Error> {
        let Some(seat) = self.seat.seat else {
            return Ok(());
        };
        let version = self.objects.get(&seat.0).map_or(1, |entry| entry.version);
        if capabilities & 1 != 0 && self.seat.pointer.is_none() {
            let pointer = self.make(&wl::WL_POINTER, version, Role::Pointer);
            self.send(seat, wl_seat::request::GET_POINTER, &[Arg::NewId(pointer)])?;
            self.seat.pointer = Some(pointer);
            self.cursor_device()?;
        }
        if capabilities & 2 != 0 && self.seat.keyboard.is_none() {
            let keyboard = self.make(&wl::WL_KEYBOARD, version, Role::Keyboard);
            self.send(
                seat,
                wl_seat::request::GET_KEYBOARD,
                &[Arg::NewId(keyboard)],
            )?;
            self.seat.keyboard = Some(keyboard);
        }
        Ok(())
    }

    fn cursor_device(&mut self) -> Result<(), Error> {
        let (Ok((manager, _)), Some(pointer)) =
            (self.global("wp_cursor_shape_manager_v1"), self.seat.pointer)
        else {
            return Ok(());
        };
        if self.seat.cursor_device.is_some() {
            return Ok(());
        }
        let device = self.make(&cursor_shape::WP_CURSOR_SHAPE_DEVICE_V1, 1, Role::Quiet);
        self.send(
            manager,
            wp_cursor_shape_manager_v1::request::GET_POINTER,
            &[Arg::NewId(device), Arg::Object(pointer)],
        )?;
        self.seat.cursor_device = Some(device);
        Ok(())
    }

    fn surface_of_object(&self, object: ObjectId) -> Option<SurfaceId> {
        match self.objects.get(&object.0).map(|entry| entry.role) {
            Some(Role::Surface(id)) => Some(id),
            _ => None,
        }
    }

    fn keyboard_event(&mut self, opcode: u16, args: &[Arg<'_>]) -> Result<(), Error> {
        let uint = |at: usize| args.get(at).and_then(Arg::as_uint).unwrap_or(0);
        let object = |at: usize| {
            args.get(at)
                .and_then(Arg::as_object)
                .unwrap_or(ObjectId::NULL)
        };
        match opcode {
            wl_keyboard::event::KEYMAP => {
                if let Some(fd) = args.get(1).and_then(Arg::as_fd) {
                    #[expect(
                        unsafe_code,
                        reason = "AUDIT: the descriptor arrived with this message and is this client's to own"
                    )]
                    // SAFETY: the wire reader hands over a descriptor nothing
                    // else in this process holds.
                    let owned = unsafe { OwnedFd::from_raw_fd(fd.0) };
                    let size = usize::try_from(uint(2)).unwrap_or(0);
                    if let Some(text) = crate::keymap::read(&owned, size) {
                        self.seat.layouts = compositor_xkb::groups_of(&text);
                    }
                }
            }
            wl_keyboard::event::ENTER => {
                let surface = self.surface_of_object(object(1));
                self.seat.focus = surface;
                if let Some(surface) = surface {
                    self.pending
                        .push(Event::Keyboard(KeyboardEvent::Enter(surface)));
                }
            }
            wl_keyboard::event::LEAVE => {
                self.seat.held = None;
                let surface = self.surface_of_object(object(1)).or(self.seat.focus);
                self.seat.focus = None;
                if let Some(surface) = surface {
                    self.pending
                        .push(Event::Keyboard(KeyboardEvent::Leave(surface)));
                }
            }
            wl_keyboard::event::KEY => {
                let (serial, time, code, state) = (uint(0), uint(1), uint(2), uint(3));
                let pressed = state == wl_keyboard::key_state::PRESSED;
                if pressed {
                    self.seat.input_serial = serial;
                }
                let key = self.key(code, pressed, false, serial, time);
                if pressed {
                    let repeats = key.keysym.is_some_and(|keysym| !is_modifier(keysym));
                    self.seat.held = match (repeats, self.seat.repeat_interval) {
                        (true, Some(_)) => Some(Held {
                            code,
                            next: Instant::now() + self.seat.repeat_delay,
                        }),
                        _ => None,
                    };
                } else if self.seat.held.is_some_and(|held| held.code == code) {
                    self.seat.held = None;
                }
                self.pending.push(Event::Keyboard(KeyboardEvent::Key(key)));
            }
            wl_keyboard::event::MODIFIERS => {
                self.seat.mask = uint(1) | uint(2) | uint(3);
                self.seat.group = uint(4);
                self.pending.push(Event::Keyboard(KeyboardEvent::Modifiers(
                    Modifiers::from_mask(self.seat.mask),
                )));
            }
            wl_keyboard::event::REPEAT_INFO => {
                let rate = args.first().and_then(Arg::as_int).unwrap_or(0);
                let delay = args.get(1).and_then(Arg::as_int).unwrap_or(0);
                self.seat.repeat_delay =
                    Duration::from_millis(u64::from(delay.max(0).unsigned_abs()));
                self.seat.repeat_interval = u32::try_from(rate)
                    .ok()
                    .filter(|rate| *rate > 0)
                    .map(|rate| Duration::from_micros(1_000_000 / u64::from(rate)));
                if self.seat.repeat_interval.is_none() {
                    self.seat.held = None;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A key as the program sees it, read in the group in force.
    fn key(&self, code: u32, pressed: bool, repeat: bool, serial: u32, time: u32) -> Key {
        let layout = self.layout_in_force();
        let mask = self.seat.mask;
        let read = crate::keyboard::read_key(layout, mask, code);
        Key {
            surface: self.seat.focus,
            code,
            keysym: read.keysym,
            text: read.text,
            plain: read.plain,
            consumed: read.consumed,
            layout,
            pressed,
            repeat,
            modifiers: Modifiers::from_mask(mask),
            serial,
            time,
        }
    }

    /// Retype the held key if its time has come.
    fn autorepeat(&mut self) {
        let (Some(held), Some(interval)) = (self.seat.held, self.seat.repeat_interval) else {
            return;
        };
        let now = Instant::now();
        if now < held.next {
            return;
        }
        let key = self.key(held.code, true, true, self.seat.input_serial, 0);
        self.pending.push(Event::Keyboard(KeyboardEvent::Key(key)));
        // Behind by more than one gap -- the program was busy -- is one
        // repeat, not a burst of them.
        let next = held.next + interval;
        self.seat.held = Some(Held {
            code: held.code,
            next: if next <= now { now + interval } else { next },
        });
    }

    fn pointer_event(&mut self, opcode: u16, args: &[Arg<'_>], version: u32) {
        let uint = |at: usize| args.get(at).and_then(Arg::as_uint).unwrap_or(0);
        let signed = |at: usize| args.get(at).and_then(Arg::as_int).unwrap_or(0);
        let fixed = |at: usize| {
            args.get(at)
                .and_then(Arg::as_fixed)
                .map_or(0.0, Fixed::to_f64)
        };
        let object = |at: usize| {
            args.get(at)
                .and_then(Arg::as_object)
                .unwrap_or(ObjectId::NULL)
        };
        match opcode {
            wl_pointer::event::ENTER => {
                let Some(surface) = self.surface_of_object(object(1)) else {
                    return;
                };
                self.seat.enter_serial = uint(0);
                self.seat.pointer_focus = Some(surface);
                let (x, y) = (fixed(2), fixed(3));
                self.seat.pointer_at = (x, y);
                let result = self.apply_cursor();
                self.defer(result);
                self.pending
                    .push(Event::Pointer(PointerEvent::Enter { surface, x, y }));
            }
            wl_pointer::event::LEAVE => {
                let surface = self
                    .surface_of_object(object(1))
                    .or(self.seat.pointer_focus);
                self.seat.pointer_focus = None;
                if let Some(surface) = surface {
                    self.pending
                        .push(Event::Pointer(PointerEvent::Leave { surface }));
                }
            }
            wl_pointer::event::MOTION => {
                let (x, y) = (fixed(1), fixed(2));
                self.seat.pointer_at = (x, y);
                if let Some(surface) = self.seat.pointer_focus {
                    self.pending
                        .push(Event::Pointer(PointerEvent::Motion { surface, x, y }));
                }
            }
            wl_pointer::event::BUTTON => {
                let pressed = uint(3) == 1;
                if pressed {
                    self.seat.input_serial = uint(0);
                }
                if let Some(surface) = self.seat.pointer_focus {
                    let (x, y) = self.seat.pointer_at;
                    self.pending.push(Event::Pointer(PointerEvent::Button {
                        surface,
                        x,
                        y,
                        button: uint(2),
                        pressed,
                        serial: uint(0),
                    }));
                }
            }
            wl_pointer::event::AXIS => {
                let axis = self.seat.axis.get_or_insert_with(Axis::default);
                if uint(1) == 0 {
                    axis.vertical += fixed(2);
                } else {
                    axis.horizontal += fixed(2);
                }
                if version < 5 {
                    self.flush_axis();
                }
            }
            wl_pointer::event::AXIS_DISCRETE => {
                let axis = self.seat.axis.get_or_insert_with(Axis::default);
                if uint(0) == 0 {
                    axis.discrete.0 += signed(1);
                } else {
                    axis.discrete.1 += signed(1);
                }
            }
            wl_pointer::event::AXIS_VALUE120 => {
                let vertical = uint(0) == 0;
                let residue = if vertical {
                    &mut self.seat.residue.0
                } else {
                    &mut self.seat.residue.1
                };
                *residue = residue.saturating_add(signed(1));
                let clicks = *residue / 120;
                *residue -= clicks * 120;
                let axis = self.seat.axis.get_or_insert_with(Axis::default);
                if vertical {
                    axis.discrete.0 += clicks;
                } else {
                    axis.discrete.1 += clicks;
                }
            }
            wl_pointer::event::FRAME => self.flush_axis(),
            _ => {}
        }
    }

    fn flush_axis(&mut self) {
        let Some(axis) = self.seat.axis.take() else {
            return;
        };
        if let Some(surface) = self.seat.pointer_focus {
            self.pending.push(Event::Pointer(PointerEvent::Axis {
                surface,
                vertical: axis.vertical,
                horizontal: axis.horizontal,
                discrete: axis.discrete,
            }));
        }
    }
}

impl Drop for Client {
    /// The children go with the program: waybar's `killpg` on exit.
    fn drop(&mut self) {
        self.children.kill_all();
    }
}

impl SurfaceState {
    fn new(surface: ObjectId, kind: Kind) -> Self {
        Self {
            surface,
            kind,
            configured: None,
            preferred: None,
            entered: Vec::new(),
            scale: 1,
            sent_scale: 1,
            slots: Vec::new(),
            scratch: Vec::new(),
        }
    }
}

/// A `u32` argument as the `int` the wire wants.
fn int(value: u32) -> Arg<'static> {
    Arg::Int(i32::try_from(value).unwrap_or(i32::MAX))
}

fn nonnegative(value: i32) -> u32 {
    u32::try_from(value.max(0)).unwrap_or(0)
}

/// Close whatever descriptors an event handed over that nothing wants.
fn close_descriptors(args: &[Arg<'_>]) {
    for arg in args {
        if let Some(fd) = arg.as_fd() {
            #[expect(
                unsafe_code,
                reason = "AUDIT: a descriptor that arrived with an event this client does not use; it is this client's and nothing else holds it"
            )]
            // SAFETY: as the reason says; dropping it closes it once.
            drop(unsafe { OwnedFd::from_raw_fd(fd.0) });
        }
    }
}

/// Whether a keysym is a modifier, which does not repeat.
fn is_modifier(keysym: &str) -> bool {
    matches!(
        keysym,
        "Shift_L"
            | "Shift_R"
            | "Control_L"
            | "Control_R"
            | "Alt_L"
            | "Alt_R"
            | "Meta_L"
            | "Meta_R"
            | "Super_L"
            | "Super_R"
            | "Hyper_L"
            | "Hyper_R"
            | "Caps_Lock"
            | "Num_Lock"
            | "Scroll_Lock"
            | "ISO_Level3_Shift"
            | "ISO_Level5_Shift"
            | "ISO_Next_Group"
            | "ISO_Prev_Group"
            | "Mode_switch"
    )
}
