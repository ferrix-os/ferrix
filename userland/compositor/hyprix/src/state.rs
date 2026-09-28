//! The loop: accept, read, lay out, draw, show.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use compositor_config::{Config, MonitorRule, NoSources, Position, Transform};
use compositor_layout::{Monitor, MonitorId, Rect, Settings, State, WindowId};
use compositor_protocol::{core, xdg_shell};
use compositor_render::{Canvas, Style};
use compositor_server::{
    Client, Event, ForeignRequest, Globals, PoolKey, Rect as ServerRect, Role,
};

use crate::clipboard::{Through, Which};
use compositor_socket::{Connection, Listener, RecvError};
use compositor_wire::{Fd, ObjectId};

use crate::backend::{Backend, Headless};
use crate::deliver::Focus;
use crate::devices::Devices;
use crate::frame::Source;
use crate::keymap::Keymap;
use crate::options::Options;
use crate::pool::Mapping;
use crate::seat::Seat;

/// One connection: the protocol side, the socket, and the memory it shared.
#[derive(Debug)]
pub struct Slot {
    client: Client,
    connection: Connection,
    /// Each pool's mapping, by its key: an object id the client may give
    /// to its next pool once it has destroyed this one, while buffers cut
    /// from this one still draw.
    pools: BTreeMap<PoolKey, Mapping>,
    /// Pools the client has destroyed that still have buffers made from
    /// them. `wl_shm_pool.destroy` releases the object, not the memory:
    /// "the mmapped memory will be released when all buffers that have
    /// been created from this pool are gone". A compositor that unmapped
    /// at `destroy` hands nothing back to a client that made its buffers
    /// and then threw the pool away, which is what almost every toolkit
    /// does.
    retired: std::collections::BTreeSet<PoolKey>,
    /// Windows this connection owns, so they can be closed when it goes.
    windows: Vec<(ObjectId, WindowId)>,
    /// Layer surfaces it owns, in the order it made them: wlroots places
    /// them in that order, so a bar that started first gets the edge.
    layers: Vec<ObjectId>,
    /// Where each of its layer surfaces was last placed, in the space all
    /// screens share: what a popup a layer surface took with `get_popup`
    /// (a bar's tooltip) is placed against.
    layer_rects: BTreeMap<ObjectId, Rect>,
    /// What each of its windows was called when it mapped: the class and
    /// then the title. A client may rename a window afterwards, and
    /// `initialclass:` and `initialtitle:` are what it was called first.
    firsts: BTreeMap<WindowId, (String, String)>,
    /// Dialogs floated when they mapped, still waiting for their first
    /// buffer: its size is the size they float at (`fit_dialog`).
    unsized_dialogs: std::collections::BTreeSet<WindowId>,
    /// The process that opened the connection, or 0 where the kernel would
    /// not say.
    pid: i32,
    /// Whether it is finished and waiting to be dropped.
    gone: bool,
    /// Which connection this is of all the compositor has had, counting
    /// from one. A slot's *place* changes when another connection ends;
    /// this does not, which is what something kept across frames for one of
    /// its surfaces -- a texture -- has to be kept by.
    serial: u64,
}

impl Slot {
    /// This connection's layer surfaces where they were last placed: the
    /// `wl_surface`, its rectangle in the space all screens share, and its
    /// layer. What the pointer is delivered to beside the windows.
    pub(crate) fn placed_layers(
        &self,
    ) -> impl Iterator<Item = (ObjectId, Rect, compositor_server::Layer)> + '_ {
        self.layers.iter().filter_map(|id| {
            let rect = self.layer_rects.get(id).copied()?;
            let layer = self.client.layer_surface(*id)?;
            Some((layer.surface, rect, layer.layer))
        })
    }

    /// A fresh client on one end of a socket pair, offered the globals the
    /// compositor offers with one screen, for a test that drives a slot the
    /// way the loop does.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let (ours, _theirs) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
        Self {
            client: Client::new(globals(1)),
            connection: Connection::new(ours).expect("a connection"),
            pools: BTreeMap::new(),
            retired: std::collections::BTreeSet::new(),
            windows: Vec::new(),
            layers: Vec::new(),
            layer_rects: BTreeMap::new(),
            firsts: BTreeMap::new(),
            unsized_dialogs: std::collections::BTreeSet::new(),
            pid: 0,
            gone: false,
            serial: 1,
        }
    }

    /// Which connection this is of all the compositor has had.
    pub(crate) const fn serial(&self) -> u64 {
        self.serial
    }

    /// The protocol side of this connection.
    pub const fn client(&self) -> &Client {
        &self.client
    }

    /// The client descriptor that wakes the compositor for a request.
    #[must_use]
    pub(crate) fn raw_fd(&self) -> i32 {
        self.connection.as_raw_fd()
    }

    /// Send whatever this client has queued, now, and whatever an earlier
    /// send left waiting because the client's socket was full.
    ///
    /// The loop sends at the end of every pass, and that is soon enough for
    /// everything but a descriptor: the clipboard hands a client a pipe and
    /// then has to let go of it, and a descriptor let go of before the
    /// message carrying it has been sent is a descriptor the client never
    /// gets. The connection keeps its own copy of one the socket would not
    /// take yet, so letting go after this is safe even then. Gives whether
    /// the connection is still there.
    pub fn flush(&mut self) -> bool {
        let outgoing = self.client.take_outgoing();
        let sent = if !outgoing.bytes.is_empty() {
            self.connection.send(&outgoing.bytes, &outgoing.descriptors)
        } else if self.connection.has_pending_writes() {
            self.connection.flush()
        } else {
            return !self.gone;
        };
        if sent.is_err() {
            self.gone = true;
        }
        !self.gone
    }

    /// Whether bytes are waiting for this client's socket to take them,
    /// which the loop wakes for as well as for a request.
    #[must_use]
    pub(crate) fn has_pending_writes(&self) -> bool {
        self.connection.has_pending_writes()
    }

    /// The same, to be sent to: the seat's events go out through it.
    pub const fn client_mut(&mut self) -> &mut Client {
        &mut self.client
    }

    /// The pools this connection has shared, by key.
    pub const fn pools(&self) -> &BTreeMap<PoolKey, Mapping> {
        &self.pools
    }

    /// The process that opened it.
    pub const fn pid(&self) -> i32 {
        self.pid
    }

    /// What one of its windows was called when it mapped: the class and
    /// then the title.
    pub fn first_called(&self, window: WindowId) -> Option<(&str, &str)> {
        self.firsts
            .get(&window)
            .map(|(class, title)| (class.as_str(), title.as_str()))
    }
}

/// Run the compositor.
///
/// # Errors
///
/// A sentence saying what could not be done.
pub fn run(options: &Options) -> Result<String, String> {
    run_with(options, &mut |_| {})
}

/// The same, saying each thing as it happens.
///
/// A compositor's log line comes at the end, and a compositor does not end:
/// so whatever is watching -- a person, or `cargo xtask test-compositor` --
/// has to be told when the screen is up rather than when the run is over.
///
/// # Errors
///
/// A sentence saying what could not be done.
pub fn run_with(options: &Options, report: &mut dyn FnMut(&str)) -> Result<String, String> {
    // Everything the compositor says goes to whoever is watching *and* into
    // a rolling buffer, which is what `hyprctl rollinglog` reads. Hyprland
    // keeps the same buffer for the same reason: when something is wrong,
    // the last few lines are what a person asks for first.
    //
    // A `RefCell` because the closure holds the buffer while the snapshot
    // reads it, and both are shared borrows of the cell rather than of the
    // lines.
    let rolling: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
    let said_to = |line: &str| {
        let mut lines = rolling.borrow_mut();
        lines.push(line.to_owned());
        if lines.len() > ROLLING {
            let _ = lines.remove(0);
        }
    };
    let mut report = |line: &str| {
        said_to(line);
        report(line);
    };
    let report = &mut report;
    let mut config = read_config(options)?;
    let mut settings = Settings::from_config(&config);
    let mut style = Style::from_config(&config);
    // `debug:overlay`'s samples, kept whether or not it is up so that it
    // shows numbers the moment it is turned on; and whether the last frame
    // drew it, so that turning it on or off is owed a frame.
    let mut overlay = crate::overlay::Overlay::default();
    let mut overlay_was = false;

    // The screens: memory when `--headless` asked for one, and every
    // connected connector of every card otherwise. A card's size is the
    // mode's, not the compositor's to choose.
    // The `monitor =` lines, which say where a monitor goes, how it is
    // scaled, which of its modes it is set to and whether it is used at all.
    // A line that cannot be read is said and the rest apply, as everywhere
    // else in the configuration. Read before the screens are opened, because
    // the mode is set when one is.
    let (rules, refused) =
        MonitorRule::read_all(config.monitors.iter().map(|raw| raw.value.as_str()));
    for (value, why) in refused {
        report(&format!("hyprix: monitor = {value}: {why}"));
    }
    let backends: Vec<Box<dyn Backend>> = match options.headless {
        Some((width, height)) => vec![Box::new(Headless::new(width, height))],
        None => open_screens(&rules)?,
    };
    let mut screens = Screen::all(backends, &rules, options.renderer, report)?;
    // The `windowrule` lines, which are applied when a window maps.
    let mut window_rules = crate::rules::Rules::new(&config, report);
    // `layerrule = <rule>, <namespace>`: what a bar, a wallpaper or a
    // notification is drawn with. The `zwlr_layer_shell_v1` half of
    // `windowrule`, matched on the namespace a surface asked for.
    let layer_rules = read_layer_rules(&config, report);
    // `workspace = <workspace>, <rule>…`: what one workspace is unlike the
    // others -- its gaps, its border, its layout, which screen it lives on
    // and whether it exists with nothing on it.
    let workspace_rules = read_workspace_rules(&config, report);
    // The clipboard: what one client copied, for the others to paste.
    let mut clipboard = crate::clipboard::Clipboard::new();
    // The screenshots asked for in one pass, kept until the screens are in
    // reach: a client's own borrow is open while its requests are read.
    let mut shots: Vec<Shot> = Vec::new();
    // The session lock, while one is held.
    let mut lock: Option<Lock> = None;
    // The input method, while a program is one.
    let mut method: Option<Method> = None;
    if !window_rules.is_empty() {
        report(&format!("hyprix: {} window rules", window_rules.len()));
    }
    // What the whole desktop covers, which is what the pointer moves over.
    let desktop = Screen::desktop(&screens);

    // Where the socket goes. Wayland's rule is `$XDG_RUNTIME_DIR/<name>`,
    // which is what a session manager sets; a compositor started as init on a
    // machine that has just booted has no session manager and no variable, so
    // it falls back to a directory that always exists and says so in its log
    // line. A name with a slash in it is an absolute path either way.
    let display = resolve_display(&options.display);
    let listener = Listener::bind(&display).map_err(|error| format!("the socket: {error}"))?;

    // The seat, but only for a compositor that owns the screen.
    //
    // Taking a device means grabbing it, and a grab takes the keyboard away
    // from whatever else is reading it. A `--headless` compositor is one
    // running beside something else -- a test on a build machine, a nested
    // session -- and it has no business taking that machine's keyboard. So
    // the devices go with the screen: the card has them, memory does not.
    //
    // A compositor that owns the screen and finds no devices is not a
    // failure either; it is a machine with nothing plugged in, so what is
    // missing is said and the loop goes on.
    let mut devices = if options.headless.is_some() {
        Devices::default()
    } else {
        match Devices::open() {
            Ok((devices, refused)) => {
                for reason in refused {
                    report(&format!("hyprix: {reason}"));
                }
                devices
            }
            Err(error) => {
                report(&format!("hyprix: no input devices: {error}"));
                Devices::default()
            }
        }
    };
    let mut capabilities = seat_capabilities(&devices);
    // When `/dev/input` is looked at again: devices found after the first
    // look are opened then, and the seat grows the capabilities they bring.
    // Never for a headless compositor, whose devices are none on purpose.
    let mut rescan = options
        .headless
        .is_none()
        .then(|| Instant::now() + RESCAN_EARLY);
    // The keymap is made whether or not there is a keyboard: a client that
    // binds one on a seat that announced none is already refused, and a
    // machine whose keyboard arrives later should not need a new file.
    // `input:kb_layout` and `input:kb_variant`, and a sentence when the
    // configuration asked for a layout this compositor does not ship: a
    // person whose keyboard suddenly types English is owed a reason.
    let asked = crate::seat::chosen_layouts(&config);
    for (chosen, exact) in &asked {
        if !*exact {
            report(&format!(
                "hyprix: no keymap for kb_layout = {}, kb_variant = {}; using {}",
                config.str("input:kb_layout").unwrap_or_default(),
                config.str("input:kb_variant").unwrap_or_default(),
                chosen.described()
            ));
        }
    }
    // One keymap with a group for each layout, which is what libxkbcommon
    // hands Hyprland for `kb_layout = de,us` and what lets a switch send
    // only a new group rather than a new keymap.
    let layouts = asked.iter().map(|(layout, _)| *layout).collect::<Vec<_>>();
    let text = compositor_xkb::merged(&layouts);
    let keymap = match Keymap::new(&text) {
        Ok(keymap) => Some(keymap),
        Err(error) => {
            report(&format!("hyprix: no keymap: {error}"));
            None
        }
    };
    let mut seat = Seat::new(&config, desktop.2, desktop.3);
    seat.place_at(desktop.0, desktop.1);
    let mut animations = crate::animate::Animations::new(&config);
    // What `hyprctl animations`, `configerrors` and `rollinglog` read,
    // gathered when the configuration is read rather than on every frame.
    let said = Said {
        animations: animations.described(),
        beziers: animations.beziers(),
        errors: animations.diagnostics().to_vec(),
        workspace_rules,
    };
    for reason in animations.diagnostics() {
        report(&format!("hyprix: an animation line was dropped: {reason}"));
    }
    // Workspaces whose `on-created-empty:` command has been run.
    let mut opened: std::collections::BTreeSet<compositor_layout::WorkspaceId> =
        std::collections::BTreeSet::new();
    let mut focus = Focus::new();
    let repeat = (
        i32::try_from(config.int("input:repeat_rate").unwrap_or(25)).unwrap_or(25),
        i32::try_from(config.int("input:repeat_delay").unwrap_or(600)).unwrap_or(600),
    );
    let follow_mouse = config.int("input:follow_mouse").unwrap_or(1) != 0;
    // The pointer on a cursor plane where a screen has one: Hyprland's
    // `cursor:no_hardware_cursors`, read once as `follow_mouse` is.
    let planes = crate::plane::wanted(config.int("cursor:no_hardware_cursors").unwrap_or(2));
    // Off in Hyprland: a program asking for another's window makes it
    // urgent rather than taking the focus away from what is being used.
    let focus_on_activate = config.int("misc:focus_on_activate").unwrap_or(0) != 0;
    {
        let (live, unresolved) = seat.binds();
        for reason in unresolved {
            report(&format!("hyprix: a bind was dropped: {reason}"));
        }
        let named = devices.describe();
        report(&format!(
            "hyprix: seat {} devices [{}], {live} binds",
            devices.len(),
            named.join(", ")
        ));
    }

    let mut state = State::new(settings);
    for screen in &screens {
        let _ = state
            .add_monitor(Monitor {
                id: screen.monitor,
                name: screen.name.clone(),
                rect: screen.rect,
                reserved: compositor_layout::Gaps::default(),
                scale: screen.scale,
                transform: screen.transform,
                description: screen.description.clone(),
                made: screen.made.clone(),
            })
            .map_err(|error| format!("the monitor: {error:?}"))?;
    }
    // The workspace rules go in once the monitors are there: a
    // `persistent:` workspace has to be put on one, and `monitor:` names
    // it.
    let made = state.set_workspace_rules(said.workspace_rules.clone());
    if !said.workspace_rules.is_empty() {
        report(&format!(
            "hyprix: {} workspace rule{}, {} workspace{} made",
            said.workspace_rules.len(),
            if said.workspace_rules.len() == 1 {
                ""
            } else {
                "s"
            },
            made.len(),
            if made.len() == 1 { "" } else { "s" }
        ));
    }
    report(&format!(
        "hyprix: {} monitor{} [{}]",
        screens.len(),
        if screens.len() == 1 { "" } else { "s" },
        screens
            .iter()
            .map(|screen| screen.describe())
            .collect::<Vec<_>>()
            .join(", ")
    ));

    // `hyprctl`'s socket, when one was asked for. Hyprland puts it under
    // $XDG_RUNTIME_DIR/hypr/<instance>/, and a program looks there.
    let mut events = None;
    let control = match options.instance.as_deref() {
        Some(instance) => {
            let runtime = std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let control = crate::control::Control::bind(&runtime, instance)
                .map_err(|error| format!("hyprctl's socket: {error}"))?;
            // The event socket beside it, refused the way the request socket
            // is. `.socket2.sock` is a byte longer than `.socket.sock`, so an
            // instance directory can take the one and not the other; said
            // only in the compositor's report, that was a bar that got no
            // events and a test that failed as if the machine were slow.
            events = Some(
                crate::control::Events::bind(control.directory())
                    .map_err(|error| format!("hyprctl's event socket: {error}"))?,
            );
            Some(control)
        }
        None => None,
    };

    // The plugins: programs the compositor starts and talks to over the
    // control socket. Started before `exec-once`, because a plugin that adds
    // a dispatcher should be there before anything presses it, and after the
    // sockets, because it connects to one as soon as it runs.
    let mut plugins = crate::plugins::Plugins::new();
    // `hyprctl` connections accepted and not answered yet.
    let mut requests: Vec<crate::control::Pending> = Vec::new();
    for command in &config.plugins {
        match start(command, listener.path(), options.instance.as_deref()) {
            Ok(pid) => report(&format!("hyprix: plugin {command} started as {pid}")),
            Err(error) => report(&format!("hyprix: plugin {command} did not start: {error}")),
        }
    }

    // `exec-once` from the configuration, then anything --exec added --
    // after the sockets, because a bar started by `exec-once` looks for them
    // as soon as it runs and a compositor that binds them later has started
    // a program that cannot find it.
    for command in config
        .exec_once
        .iter()
        .map(String::as_str)
        .chain(options.exec.iter().map(String::as_str))
    {
        match start(command, listener.path(), options.instance.as_deref()) {
            Ok(pid) => report(&format!("hyprix: started {command} as {pid}")),
            // A program that will not start is the person's to fix, not a
            // reason to have no compositor; Hyprland logs it and carries on.
            Err(error) => {
                if let Some(line) = not_started(command, &error) {
                    report(&line);
                }
            }
        }
    }

    let mut slots: Vec<Slot> = Vec::new();
    // How many connections there have been: a slot's serial.
    let mut connections: u64 = 0;
    let mut sources: BTreeMap<WindowId, Source> = BTreeMap::new();
    let mut placed_layers: Vec<crate::frame::Placed> = Vec::new();
    let mut settling = false;
    // Whether something has changed that no frame has shown yet, and when
    // the next frame may be drawn. `crate::pace` says why a change waits.
    let mut owed = false;
    let mut pace = crate::pace::Pace::default();
    let mut slowest = Duration::ZERO;
    // What the clients have said they drew since the last frame, and the
    // fewest pixels any frame has redrawn. `crate::damage` says what the
    // first is for; the second is what says the compositor is redrawing
    // what changed rather than the screen.
    let mut commits = crate::damage::Told::default();
    let mut least = i64::MAX;
    // The slowest of the frames since the last report, how many there were,
    // and when that report was: the line is a second apart at most, and
    // silent while nothing is drawn.
    let mut since = Duration::ZERO;
    let mut spent = Duration::ZERO;
    let mut counted = 0u32;
    let mut since_phases = [0; compositor_render::timing::Phase::ALL.len()];
    let mut since_drew = (0i64, 0usize, 0i64);
    let mut since_from = [0i64; 4];
    let mut reported = Instant::now();
    let mut next_window = 1u32;
    let mut drawn = 0u32;
    // The most windows at once, not the count at the end: a client that ran
    // and closed leaves none behind, and "it never got a window" and "it got
    // one and gave it back" are not the same thing for a test to read.
    let mut most = 0usize;
    let started = Instant::now();
    let deadline = options.deadline.map(Duration::from_millis);
    // What the compositor's own dispatchers change: which screens `dpms`
    // has turned off, which windows have asked to be raised and not been
    // looked at, the drag a `bindm` started, whether `exit` was asked for,
    // and whether `toggleswallow` is on.
    let mut dpms: BTreeMap<String, bool> = BTreeMap::new();
    // The ramps a night-light set on each screen, by its place in the
    // list: `zwlr_gamma_control_v1`.
    let mut gammas: BTreeMap<usize, crate::frame::Gamma> = BTreeMap::new();
    // Whether the session was locked last pass, so that
    // `hyprland-lock-notify-v1` is told at the moment it changes. A
    // recorder or a notifier has no other way to know: `ext-session-lock-v1`
    // is the *locker's* protocol and says nothing to anybody else.
    let mut was_locked = false;
    let mut urgent: Vec<WindowId> = Vec::new();
    let mut drag: Option<crate::act::Drag> = None;
    // The drag a client started with `wl_data_device.start_drag`, while one
    // is going on. Two clients that cannot see each other, joined by the
    // compositor.
    let mut carried: Option<crate::dragging::Carried> = None;
    let mut quit = false;
    let mut swallow = false;
    // When the seat was last used, and how far back `forceidle` pretended
    // it was: `ext-idle-notify` measures from the last of the two.
    let mut last_input = Instant::now();
    let mut forced: Option<Duration> = None;
    // What a `zwp_virtual_keyboard_v1` or a `zwlr_virtual_pointer_v1` asked
    // the seat to do. A client's requests are read after the input loop has
    // run, so what one injects is carried out on the next pass -- one frame
    // later, which is the same delay a real device's event has when it
    // arrives a moment after the read.
    let mut injected: Vec<crate::seat::Input> = Vec::new();
    // `None` is the first non-blocking sweep. Every following pass receives
    // exactly the descriptors `poll` woke for; a timer wake is `Some([])`.
    let mut ready: Option<Vec<i32>> = None;
    // What ends among the programs started here -- and, as pid 1, among
    // everybody's orphans -- is reaped on every pass, and its end wakes one.
    let children = match crate::children::Children::watch() {
        Ok(children) => Some(children),
        Err(error) => {
            report(&format!(
                "hyprix: SIGCHLD not caught ({error}); children are reaped only when the loop wakes"
            ));
            None
        }
    };

    loop {
        if let Some(children) = children.as_ref() {
            let _ = children.reap();
        }
        if quit {
            break;
        }
        if let Some(limit) = deadline
            && started.elapsed() > limit
        {
            break;
        }
        if let Some(limit) = options.frames
            && drawn >= limit
        {
            break;
        }

        // A card whose driver died: the screen is lost until the card is
        // back, and the frames that look for it are owed from now on.
        for screen in &mut screens {
            let Some(fd) = screen.backend.raw_fd() else {
                continue;
            };
            if ready.as_ref().is_some_and(|fds| fds.contains(&fd)) && screen.backend.check() {
                screen.say_gone(report);
                owed = true;
            }
        }

        // A card whose modes changed -- a virtio-gpu whose window on the host
        // was resized -- has its screens follow, where their `monitor =` line
        // leaves the mode to the monitor.
        if screens.iter_mut().fold(false, |changed, screen| {
            screen.backend.modes_changed() | changed
        }) && follow_modes(&mut screens, &rules, options.renderer, &mut state, report)
        {
            let (x, y, width, height) = Screen::desktop(&screens);
            seat.resize(width, height);
            seat.place_at(x, y);
            let outputs: Vec<compositor_server::Output> =
                screens.iter().map(Screen::output).collect();
            for slot in &mut slots {
                slot.client_mut().set_outputs(outputs.clone());
                slot.client_mut().publish_outputs(&outputs);
            }
            owed = true;
        }

        // New connections.
        let first_new = slots.len();
        if ready
            .as_ref()
            .is_none_or(|fds| fds.contains(&listener.as_raw_fd()))
        {
            while let Ok(Some(stream)) = listener.accept() {
                match Connection::new(stream) {
                    Ok(connection) => slots.push(Slot {
                        serial: {
                            connections = connections.saturating_add(1);
                            connections
                        },
                        pid: connection.peer_pid(),
                        client: {
                            let mut client = Client::new(globals(screens.len()));
                            // What each screen is, and what the seat has: only
                            // the capabilities there are devices for, since a
                            // client may not ask for one the seat did not
                            // announce and should not wait for keys that will
                            // never come.
                            client.set_outputs(screens.iter().map(Screen::output).collect());
                            client.set_seat_capabilities(capabilities);
                            client.set_keymap(keymap.as_ref().map(Keymap::handed));
                            client.set_repeat_info(repeat.0, repeat.1);
                            client
                        },
                        connection,
                        pools: BTreeMap::new(),
                        retired: std::collections::BTreeSet::new(),
                        windows: Vec::new(),
                        layers: Vec::new(),
                        layer_rects: BTreeMap::new(),
                        firsts: BTreeMap::new(),
                        unsized_dialogs: std::collections::BTreeSet::new(),
                        gone: false,
                    }),
                    Err(_) => continue,
                }
            }
        }

        // `hyprctl`: one request a connection, answered and closed -- unless
        // the connection opens with `[[PLUGIN]]`, which is a plugin and is
        // kept.
        let mut asked: Vec<compositor_ipc::Reply> = Vec::new();
        // A description of the compositor is not free -- it walks every
        // window of every monitor -- so it is made only when there is a
        // plugin to answer.
        if !plugins.is_empty()
            && (ready
                .as_ref()
                .is_none_or(|fds| plugins.raw_fds().any(|fd| fds.contains(&fd)))
                || plugins.needs_poll())
        {
            let snapshot = crate::control::snapshot(
                &state,
                &slots,
                &sources,
                &as_reported(
                    &config,
                    &seat,
                    &devices,
                    &placed_layers,
                    &plugins,
                    &said,
                    &rolling.borrow(),
                    window_rules.styles(),
                    lock.is_some(),
                    started.elapsed().as_secs(),
                ),
            );
            asked.extend(plugins.poll(&snapshot));
        }
        if let Some(control) = control.as_ref()
            && ready
                .as_ref()
                .is_none_or(|fds| fds.contains(&control.raw_fd()))
        {
            while let Some(stream) = control.accept() {
                if let Ok(pending) = crate::control::Pending::new(stream) {
                    requests.push(pending);
                }
            }
        }
        // Each request as it arrives, answered once it is whole.
        let now = Instant::now();
        let mut whole = Vec::new();
        for mut pending in std::mem::take(&mut requests) {
            let readable = ready
                .as_ref()
                .is_none_or(|fds| fds.contains(&pending.raw_fd()))
                || now >= pending.deadline();
            match readable.then(|| pending.read(now)) {
                Some(Some(true)) => whole.push(pending),
                Some(None) => {}
                Some(Some(false)) | None => requests.push(pending),
            }
        }
        if !whole.is_empty() {
            let snapshot = crate::control::snapshot(
                &state,
                &slots,
                &sources,
                &as_reported(
                    &config,
                    &seat,
                    &devices,
                    &placed_layers,
                    &plugins,
                    &said,
                    &rolling.borrow(),
                    window_rules.styles(),
                    lock.is_some(),
                    started.elapsed().as_secs(),
                ),
            );
            for pending in whole {
                match crate::control::serve(pending, &snapshot, &mut plugins) {
                    Ok(todo) => asked.extend(todo),
                    Err(_) => {
                        // A client that went away mid-request is not the
                        // compositor's problem.
                    }
                }
            }
        }

        let mut changed = false;
        // What each screen is called and where it is, which is all a
        // dispatcher needs of one: `dpms` names a screen and
        // `movecursortocorner` falls back to the first. Taken here so that
        // the screens themselves stay free for the frame below.
        let placements: Vec<(String, Rect)> = screens
            .iter()
            .map(|screen| (screen.name.clone(), screen.rect))
            .collect();

        // Input, before the clients are read: a key that fires a dispatcher
        // changes the layout, and a window told its new size in the same pass
        // draws once rather than twice.
        //
        // One input at a time, each carried out before the next is read.
        // That matters for anything a dispatcher changes about what the
        // *next* key means: `submap` is the whole of that, and a batch of
        // events turned into actions all at once would judge every key in it
        // against the map that was in force before the first. On a machine
        // fast enough to see each key on its own the two are the same; under
        // emulation a whole sequence arrives in one read, which is where
        // this was found.
        let now = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
        // While the session is locked the keyboard is the lock's: a bind
        // fires only if it was written `bindl`, and every other key goes to
        // the lock's own surface and to no window. That is
        // `ext-session-lock-v1`'s other half -- a lock that showed a picture
        // and still let a key reach the browser under it would not be one.
        seat.set_locked(lock.is_some());
        if let Some(due) = rescan
            && Instant::now() >= due
        {
            rescan = Some(Instant::now() + rescan_period(started.elapsed()));
            let (added, refused) = devices.rescan();
            for reason in refused {
                report(&format!("hyprix: {reason}"));
            }
            if !added.is_empty() {
                report(&format!("hyprix: seat found [{}]", added.join(", ")));
            }
            let now_has = seat_capabilities(&devices) | virtual_capabilities(&slots);
            if now_has != capabilities {
                capabilities = now_has;
                for slot in &mut slots {
                    slot.client_mut().change_seat_capabilities(capabilities);
                }
            }
        }
        // A virtual pointer is a pointer device while it lives, as Hyprland
        // adds one for it: without the capability no client has a
        // `wl_pointer`, and what the virtual pointer moves reaches nobody --
        // which is every headless compositor's case, whose real devices are
        // none.
        let wanted = seat_capabilities(&devices) | virtual_capabilities(&slots);
        if wanted != capabilities {
            capabilities = wanted;
            for slot in &mut slots {
                slot.client_mut().change_seat_capabilities(capabilities);
            }
        }
        let inputs = match ready.as_deref() {
            Some(fds) => devices.read_ready(fds),
            None => devices.read(),
        };
        for input in inputs.into_iter().chain(std::mem::take(&mut injected)) {
            // Any input at all ends the idle: that is what the protocol
            // measures, and what `forceidle` was pretending about.
            last_input = Instant::now();
            forced = None;
            let actions = seat.input(input);
            devices.show_locks(seat.modifiers().locked);
            // Where the pointer is, for the layout: Hyprland's dwindle tree
            // asks the input manager for it at the moment a window opens,
            // and `dwindle:use_active_for_splits`, `force_split = 0` and
            // `smart_split` are all about which window it was over then.
            // Kept up to date here rather than passed in at the open,
            // because a window can open long after the pointer last moved.
            state.set_pointer(seat.pointer());
            if actions.is_empty() {
                continue;
            }
            // `general:resize_on_border`: a press on the ring around a
            // window grabs that edge, and neither the press nor the release
            // that ends it reaches the client -- a client that was sent a
            // press it never saw the end of would think the button is still
            // held. Hyprland's `processMouseDownNormal` returns before
            // `sendPointerButton` for the same reason.
            let actions = crate::act::grab_border(actions, &state, &seat, &mut drag);
            if actions.is_empty() {
                continue;
            }
            // A pointer drawn into the frame makes moving it a change to the
            // screen even when nothing else moved: without this the arrow
            // would stay where the last redraw left it and catch up only
            // when a window did something. A pointer on every screen's
            // cursor plane is moved there and owes no frame -- unless a drag
            // carries a surface along with it, which is drawn.
            if actions
                .iter()
                .any(|action| matches!(action, crate::seat::Action::Pointer { .. }))
                && (carried
                    .as_ref()
                    .is_some_and(crate::dragging::Carried::holding)
                    || screens.iter().any(|screen| !screen.plane.on))
            {
                changed = true;
            }
            let done = crate::deliver::deliver(
                &actions,
                &mut focus,
                &state,
                &mut slots,
                &sources,
                now,
                follow_mouse,
                carried
                    .as_ref()
                    .is_some_and(crate::dragging::Carried::holding),
            );
            let mut pending = Vec::new();
            for asked in done.dispatch {
                let mut around = crate::act::Around {
                    slots: &mut slots,
                    sources: &sources,
                    socket: listener.path(),
                    instance: options.instance.as_deref(),
                    opened: &mut opened,
                    seat: &mut seat,
                    plugins: &mut plugins,
                    rules: &mut window_rules,
                    events: &mut events,
                    dpms: &mut dpms,
                    screens: &placements,
                    urgent: &mut urgent,
                    drag: &mut drag,
                    pending: &mut pending,
                    quit: &mut quit,
                    swallow: &mut swallow,
                    forced: &mut forced,
                    focus: &mut focus,
                    trigger: asked.trigger,
                    report,
                };
                if dispatch(&asked.name, &asked.argument, &mut state, &mut around) {
                    changed = true;
                }
            }
            // A dispatcher that moved the pointer moved it for the clients
            // too: the same actions a hand would have caused.
            if !pending.is_empty() {
                let _done = crate::deliver::deliver(
                    &pending,
                    &mut focus,
                    &state,
                    &mut slots,
                    &sources,
                    now,
                    follow_mouse,
                    carried
                        .as_ref()
                        .is_some_and(crate::dragging::Carried::holding),
                );
                changed = true;
            }
            // A drag carries on for as long as the button is held: one bind
            // starts it and every movement after that moves the window.
            if let Some(held) = drag.as_mut() {
                let (x, y) = seat.pointer();
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "the pointer is held inside the screen, which is far inside i64"
                )]
                if crate::act::dragged(held, &mut state, (x as i64, y as i64)) {
                    changed = true;
                }
            }
            // `follow_mouse`: the pointer moved onto a window that is not
            // focused, so focus it.
            if let Some(window) = done.focus
                && state.focus_window(window).is_ok()
            {
                changed = true;
            }
        }

        // What the screen protocols asked for this pass, drained below: a
        // client's own borrow holds the screens and the layout, and each of
        // these reaches one of them.
        let mut asks = ScreenAsks::default();
        for index in 0..slots.len() {
            let woke = ready.as_ref().is_none_or(|fds| {
                slots
                    .get(index)
                    .is_none_or(|slot| fds.contains(&slot.raw_fd()))
            });
            if !woke && index < first_new {
                continue;
            }
            if serve(
                &mut slots,
                index,
                &mut state,
                &mut sources,
                &mut next_window,
                &mut window_rules,
                &mut clipboard,
                &mut method,
                &mut shots,
                &mut lock,
                &screens,
                &mut urgent,
                &mut injected,
                &mut asks,
                &mut commits,
                focus_on_activate,
                report,
            )? {
                changed = true;
            }
        }
        // A focus on a surface a client has just destroyed, dropped before
        // anything moves the focus and sends a `leave` naming it.
        focus.prune(&slots);
        // The drag, now that every client's own borrow is over: it reaches
        // two connections at once and a client's borrow holds one of them.
        changed |= carry_drag(
            &mut asks,
            &mut carried,
            &mut slots,
            &state,
            &sources,
            &seat,
            now,
            report,
        );

        // What the screen protocols asked for, now that every client's own
        // borrow is over: each of these reaches the screens or the layout.
        changed |= carry_out(
            &mut asks,
            &mut slots,
            &mut screens,
            &mut state,
            &mut gammas,
            &mut dpms,
            report,
        );

        // `wp_pointer_warp_v1`: a client put the pointer inside its own
        // window. The last one wins, which is what a client sending two in
        // one pass means, and the move goes out as a person's would --
        // through the seat, so a window is entered and left the same way.
        if let Some((x, y)) = asks.warps.pop() {
            let moved = seat.warp(x, y);
            if !moved.is_empty() {
                let _done = crate::deliver::deliver(
                    &moved,
                    &mut focus,
                    &state,
                    &mut slots,
                    &sources,
                    now,
                    follow_mouse,
                    carried
                        .as_ref()
                        .is_some_and(crate::dragging::Carried::holding),
                );
                changed = true;
            }
        }

        // The screenshots asked for in this pass, now that the screens are
        // in reach again.
        for shot in shots.drain(..) {
            take_shot(&shot, &mut screens, &mut slots);
        }
        // Where a `zwp_pointer_constraints_v1` is holding the pointer, and
        // whether a client has asked for the keybinds. Both are worked out
        // from what has the pointer and what has the keyboard, which is the
        // compositor's judgement and not the client's: a constraint applies
        // only while its own surface has the pointer.
        hold_pointer(&mut seat, &mut slots, &focus, &state, &sources);
        seat.set_shortcuts_inhibited(
            focus
                .keyboard()
                .and_then(|(client, surface)| Some((slots.get(client)?, surface)))
                .is_some_and(|(slot, surface)| slot.client().inhibits_shortcuts(surface)),
        );

        if lock.is_some() != was_locked {
            was_locked = lock.is_some();
            for slot in &mut slots {
                if slot.client().watches_lock() {
                    slot.client_mut().lock_changed(was_locked);
                }
            }
        }

        // What every `ext_idle_notification_v1` is waiting for: how long
        // the seat has gone without input, and whether any client holds
        // idling off with a `zwp_idle_inhibitor_v1` on a mapped surface.
        let idle = u64::try_from(forced.unwrap_or_else(|| last_input.elapsed()).as_millis())
            .unwrap_or(u64::MAX);
        let inhibited = slots.iter().any(|slot| slot.client().inhibits_idle());
        for slot in &mut slots {
            let _said = slot.client_mut().idle_tick(idle, inhibited);
        }

        let mut pending = Vec::new();
        let forced_before = forced;
        for reply in asked {
            let mut around = crate::act::Around {
                slots: &mut slots,
                sources: &sources,
                socket: listener.path(),
                instance: options.instance.as_deref(),
                opened: &mut opened,
                seat: &mut seat,
                plugins: &mut plugins,
                rules: &mut window_rules,
                events: &mut events,
                dpms: &mut dpms,
                screens: &placements,
                urgent: &mut urgent,
                drag: &mut drag,
                pending: &mut pending,
                quit: &mut quit,
                swallow: &mut swallow,
                forced: &mut forced,
                focus: &mut focus,
                // `hyprctl dispatch pass` has no key behind it: `pass`
                // sends on the key that fired a bind, and a socket request
                // fired none.
                trigger: None,
                report,
            };
            if run_ipc(
                &reply,
                &mut state,
                &mut config,
                &mut settings,
                &mut style,
                &mut around,
            ) {
                changed = true;
            }
        }
        // `forceidle` changed how long the seat has been idle after the
        // notifications above were told. Tell them again now: the wait
        // below has no idle timer while the idle is forced, so without this
        // they would hear of it only when something else woke the loop --
        // hypridle's listeners fired seconds late, at the next keypress.
        if forced != forced_before {
            let idle = u64::try_from(forced.unwrap_or_else(|| last_input.elapsed()).as_millis())
                .unwrap_or(u64::MAX);
            let inhibited = slots.iter().any(|slot| slot.client().inhibits_idle());
            for slot in &mut slots {
                let _said = slot.client_mut().idle_tick(idle, inhibited);
            }
        }
        if !pending.is_empty() {
            let _done = crate::deliver::deliver(
                &pending,
                &mut focus,
                &state,
                &mut slots,
                &sources,
                now,
                follow_mouse,
                carried
                    .as_ref()
                    .is_some_and(crate::dragging::Carried::holding),
            );
            changed = true;
        }

        // A connection that ended takes its windows with it.
        for index in 0..slots.len() {
            if slots.get(index).is_some_and(|slot| slot.gone) {
                let windows = slots
                    .get(index)
                    .map(|slot| slot.windows.clone())
                    .unwrap_or_default();
                for (_, window) in windows {
                    remember_size(window, &mut slots, index, &mut state, &mut window_rules);
                    let _ = state.window_gone(window);
                    let _ = sources.remove(&window);
                    // What a rule gave it goes with it, so that a window id
                    // handed out again is drawn as a new window.
                    window_rules.window_gone(window);
                    changed = true;
                }
                clipboard.client_gone(&mut slots, index);
                // A lock whose program died leaves the screen locked with
                // nothing drawn on it, which is the one thing
                // `ext-session-lock-v1` is most explicit about: an unlocked
                // session is not what a crash is allowed to produce.
                if let Some(held) = lock.as_mut()
                    && held.client == index
                {
                    held.orphaned = true;
                    held.surfaces.clear();
                    report("hyprix: the program holding the lock went; the screen stays locked");
                    changed = true;
                }
                if slots.get(index).is_some_and(|slot| !slot.layers.is_empty()) {
                    // Its bars go with it, and the space they reserved comes
                    // back to the windows.
                    changed = true;
                }
            }
        }
        // Taking a slot out moves every slot after it, and a window's
        // `Source` and the focus both name a client by its *place* in the
        // list. Renumber them as the list is compacted: a window that
        // outlived an earlier client would otherwise be drawn from somebody
        // else's buffer and typed into by somebody else's keyboard.
        let places = renumbered(&slots);
        slots.retain(|slot| !slot.gone);
        sources.retain(
            |_, source| match places.get(source.client).copied().flatten() {
                Some(at) => {
                    source.client = at;
                    true
                }
                None => false,
            },
        );
        focus.renumber(&places);
        // The clipboard, the lock and the input method hold a client the
        // same way and move the same way.
        clipboard.renumber(&places);
        // A drag whose *source* went is a drag with nothing on it: the
        // target is told to leave and the drag ends.
        if let Some(held) = carried.as_mut()
            && !held.renumber(&places)
        {
            if let Some(mut held) = carried.take() {
                held.ended(&mut slots, false);
            }
            changed = true;
        }
        if let Some(held) = lock.as_mut() {
            held.client = places
                .get(held.client)
                .copied()
                .flatten()
                .unwrap_or(held.client);
        }
        if let Some(held) = method.as_mut() {
            match places.get(held.client).copied().flatten() {
                Some(at) => held.client = at,
                None => method = None,
            }
        }

        // A window arriving or leaving resizes every other window on the
        // workspace, and a window that is not told is one drawing at the
        // size it had before -- which the compositor then draws scaled into
        // a rectangle that is not its buffer's. Hyprland reconfigures the
        // whole workspace for the same reason.
        //
        // Everything that changes the layout is above this, *including* a
        // connection ending: a window left alone when its neighbour's client
        // went is the one case where nothing the compositor was asked to do
        // changed the layout and it changed anyway, and it was the one case
        // this missed.
        if changed {
            // The layer surfaces first: their exclusive zones decide how
            // much of the monitor is left for the windows to tile in, so a
            // bar has to be placed before a window is told its size.
            // `monitor = NAME, addreserved, ...` lines, the file's and
            // `hyprctl keyword`'s since, which reserve strips beside the
            // layer surfaces' zones.
            let added = compositor_config::AddedReserved::read_all(
                config.monitors.iter().map(|raw| raw.value.as_str()),
            );
            placed_layers = place_layers(&mut slots, &mut state, &screens, &layer_rules, &added);
            reconfigure(&mut slots, &state);
        }
        // The popups, which are drawn over the windows like a layer surface
        // on the top level: a menu is not a window, has no border and no
        // gaps, and belongs where its parent put it.
        let popups = placed_popups(&slots, &state, &sources);
        // The keyboard follows the layout's focus, and a window that has just
        // arrived is what the layout focused.
        // Who the keyboard is on. A locked session takes it away from every
        // window and gives it to the lock's own surface, so that a key
        // typed at a lock screen cannot reach what is behind it.
        if let Some(held) = lock.as_ref() {
            // The lock surface on the focused monitor, or the first one it
            // covered; a lock whose program has gone gets nothing, which
            // leaves the keyboard on no client at all.
            let wanted = held
                .surfaces
                .values()
                .next()
                .map(|(_, surface)| (held.client, *surface));
            focus.follow(
                wanted,
                &mut slots,
                seat.keyboard().pressed(),
                seat.keyboard().modifiers(),
            );
        } else if let Some(layer) = Focus::interactive_layer(&slots) {
            // A launcher or a menu on a layer above the windows that asked
            // for the keyboard has it while it is up.
            focus.follow(
                Some(layer),
                &mut slots,
                seat.keyboard().pressed(),
                seat.keyboard().modifiers(),
            );
        } else {
            focus.follow_layout(
                &state,
                &mut slots,
                &sources,
                seat.keyboard().pressed(),
                seat.keyboard().modifiers(),
            );
        }

        // The event socket and the bars, from the same description
        // `hyprctl` answers from: a bar and a script must not be told two
        // different things.
        let watched = slots
            .iter()
            .any(|slot| slot.client().watches_toplevels() || slot.client().lists_toplevels());
        let workspaces_watched = slots.iter().any(|slot| slot.client().watches_workspaces());
        let event_ready = events.as_ref().is_some_and(|socket| {
            ready
                .as_ref()
                .is_none_or(|fds| fds.contains(&socket.raw_fd()))
        });
        let events_watched = events
            .as_ref()
            .is_some_and(crate::control::Events::has_subscribers);
        // A plugin watcher needs every state change, even before it
        // subscribes: its `Watcher` then starts at the state the plugin saw
        // rather than replaying events from before `subscribe`.
        let plugins_tracking = !plugins.is_empty() && (plugins.watches() || changed);
        // Where the focus went this pass, taken every pass so that it
        // never piles up: two windows mapped at once each took it, and a
        // bar is told of both, as Hyprland tells it.
        let trail: Vec<Option<u64>> = state
            .take_focus_trail()
            .into_iter()
            .map(|window| window.map(|window| window.0))
            .collect();
        if event_ready || events_watched || plugins_tracking || watched || workspaces_watched {
            let snapshot = crate::control::snapshot(
                &state,
                &slots,
                &sources,
                &as_reported(
                    &config,
                    &seat,
                    &devices,
                    &placed_layers,
                    &plugins,
                    &said,
                    &rolling.borrow(),
                    window_rules.styles(),
                    lock.is_some(),
                    started.elapsed().as_secs(),
                ),
            );
            if let Some(socket) = events.as_mut() {
                socket.publish(&snapshot, &trail);
            }
            // A plugin that subscribed hears the same lines a bar does.
            plugins.tell(&snapshot, &trail);
            if workspaces_watched {
                // `ext-workspace-v1`: the workspace numbers a bar draws,
                // one group a monitor.
                let listed = crate::control::workspaces(&snapshot);
                let groups = snapshot.monitors.len();
                for slot in &mut slots {
                    slot.client_mut().publish_workspaces(groups, &listed);
                }
            }
            if watched {
                let windows = crate::control::toplevels(&snapshot);
                for slot in &mut slots {
                    // The newer list as well as the wlroots one: a taskbar
                    // written this year binds `ext-foreign-toplevel-list-v1`
                    // and one written three years ago binds the other.
                    slot.client_mut().list_toplevels(&windows);
                    slot.client_mut().show_toplevels(&windows);
                }
            }
        }

        most = most.max(sources.len());
        // A frame is drawn when something changed, when nothing has been
        // drawn yet, and while any window is still on its way somewhere: an
        // animation is a frame a change does not ask for.
        // The loop's `now` is milliseconds as a `u32`, which is what
        // `wl_keyboard.key` and `wl_pointer.motion` carry; an animation's is
        // the same clock, widened.
        let millis = u64::from(now);
        let animating = animations.busy(millis);
        // `settling` is the one pass after the last moving one: the frame
        // that is drawn while a window is still moving is a frame short of
        // its goal, and without this the screen would keep that last frame
        // for ever.
        //
        // And when the screen can take one. A change is owed a frame, not
        // drawn the moment it happens: what changed is kept -- the commits
        // in `commits`, everything else in the plan the next frame is
        // compared with -- and the frame that shows it is the next one the
        // screen's refresh allows.
        // The pointer on the screens with a cursor plane, as it is now: a
        // move there waits for no frame. A screen whose pointer went from
        // its frames to its plane, or back, owes a frame that draws it or
        // takes it away.
        let pointer = (lock.is_none() && devices.has_pointer() && seat.pointer_used()).then(|| {
            crate::frame::Cursor {
                at: (0, 0),
                surface: cursor_surface(&slots, &focus),
                shown: cursor_shown(&slots, &focus),
            }
        });
        changed |= sync_planes(
            &mut screens,
            &slots,
            pointer.map(|cursor| (cursor, seat.pointer())),
            &|name| dpms.get(name).copied().unwrap_or(false),
            planes,
            report,
        );
        owed |= changed;
        // `debug:overlay`, read every pass so that `hyprctl keyword` turns
        // it on and off at once. While it is up its numbers move every
        // 200 ms, and a frame is owed that often, as Hyprland's
        // `COverlay::draw` schedules one.
        let overlay_on = config.int("debug:overlay").unwrap_or(0) == 1;
        if overlay_on != overlay_was || (overlay_on && overlay.ask(Instant::now())) {
            owed = true;
        }
        overlay_was = overlay_on;
        if (owed || drawn == 0 || animating || settling)
            && pace.due(Instant::now(), refresh_ns(&screens))
        {
            owed = false;
            settling = animating;
            // A pass that moves the animations is one of their ticks:
            // Hyprland keeps how long it was since the last.
            if animating {
                overlay.tick(Instant::now());
            }
            overlay.screens(screens.iter().map(|screen| screen.name.as_str()));
            let outputs = state.layout();
            #[expect(
                clippy::cast_possible_truncation,
                reason = "the pointer is held inside the screen, which is far inside i64"
            )]
            let cursor_at = {
                let (x, y) = seat.pointer();
                (x as i64, y as i64)
            };
            // What each window is drawn with: what a `windowrule` gave it,
            // and over that whatever `wp_alpha_modifier_v1` asked for. The
            // protocol's multiplier is the client's own word about how much
            // of its surface shows, so it wins over a rule's opacity.
            let drawn_with = drawn_with(window_rules.styles(), &slots, &sources);
            let began = Instant::now();
            // How many pixels this frame redrew, over every screen: what
            // damage tracking is worth is how little a frame that changes
            // little costs, and the line the loop prints says so.
            let mut redrew = 0i64;
            let mut drew_what = (0i64, 0usize, 0i64);
            let mut drew_from = [0i64; 4];
            // Whether a screen is lost and not yet back: the next frame is
            // owed so that it is looked for again.
            let mut waiting = false;
            // A frame a monitor: each screen draws the workspace it shows,
            // with the windows' rectangles moved into its own pixels.
            for (which, screen) in screens.iter_mut().enumerate() {
                let Some(output) = outputs
                    .iter()
                    .find(|output| output.monitor == screen.monitor)
                else {
                    continue;
                };
                // A screen whose card went away is looked for again every
                // frame and drawn for only once it is back: then as a whole
                // frame, since the card it is on now has seen none of the
                // last one's, and in software, since the GPU went with it.
                // The loss is said here too: a cursor that could not be
                // shown or moved finds it with nothing said, and a driver
                // killed as the pointer first reaches its plane is lost
                // that way, before any frame or wait could find it.
                let lost = screen.backend.lost();
                if lost {
                    screen.say_gone(report);
                }
                if lost && !screen.backend.recover() {
                    waiting = true;
                    continue;
                }
                if lost {
                    report(&format!(
                        "hyprix: {}: the card is back; drawing on it again",
                        screen.name
                    ));
                    screen.said_gone = false;
                    screen.gpu = None;
                    screen.watch = crate::damage::Watch::default();
                    screen.plane.forget();
                }
                // When this screen's frame began, which is what the
                // counter's frame times and render times are taken from.
                let screen_began = Instant::now();
                if overlay_on {
                    overlay.frame(
                        &screen.name,
                        screen.output().refresh,
                        which == 0,
                        screen_began,
                    );
                }
                // Where each window *is*, rather than where the tiling put
                // it.
                let output = &animations.follow(output, millis);
                let (width, height) = screen.size();
                let origin = (screen.rect.x, screen.rect.y);
                let scale = screen.scale;
                // A screen `dpms off` turned off shows nothing at all,
                // before the lock and before the windows: that is what
                // turning a screen off means.
                let dark = dpms.get(&screen.name).copied().unwrap_or(false);
                // What is drawn over the windows, or over the lock: the
                // bars and the menus, or the lock's own surface and
                // whatever a `layerrule = abovelock` asked for over it --
                // an on-screen keyboard, which is the whole reason that
                // rule exists, because a compositor that drew nothing over
                // the lock would leave a person with no way to type the
                // password.
                let over: Vec<crate::frame::Placed> = if dark {
                    Vec::new()
                } else if let Some(held) = lock.as_ref() {
                    held.surfaces
                        .get(&which)
                        .map(|(_, surface)| crate::frame::Placed {
                            client: held.client,
                            surface: *surface,
                            rect: screen.rect,
                            above: true,
                            rules: crate::frame::LayerRules::default(),
                        })
                        .into_iter()
                        .chain(
                            placed_layers
                                .iter()
                                .copied()
                                .filter(|placed| placed.rules.above_lock),
                        )
                        .collect()
                } else {
                    placed_layers
                        .iter()
                        .copied()
                        .chain(popups.iter().copied())
                        .collect()
                };
                // The surface a drag is carrying, drawn at the pointer:
                // that is what makes a drag look like one.
                let drag_icon = carried
                    .as_ref()
                    .filter(|held| !dark && held.holding())
                    .and_then(|held| {
                        Some(crate::frame::Placed {
                            client: held.client,
                            surface: held.icon?,
                            rect: Rect::new(cursor_at.0, cursor_at.1, 0, 0),
                            above: true,
                            rules: crate::frame::LayerRules::default(),
                        })
                    });
                // The pointer, unless the session is locked: a lock screen
                // draws its own and the compositor's arrow over it would be
                // two pointers.
                // And not where the screen's cursor plane shows it.
                let cursor = (!dark
                    && !screen.plane.on
                    && lock.is_none()
                    && devices.has_pointer()
                    && seat.pointer_used())
                .then(|| crate::frame::Cursor {
                    at: cursor_at,
                    surface: cursor_surface(&slots, &focus),
                    shown: cursor_shown(&slots, &focus),
                });
                // The ramps a night-light set on this screen, applied to
                // the pixels on their way out.
                let gamma = gammas.get(&which).copied();
                // A locked screen draws no window, and a screen that is off
                // draws nothing at all: a plan says what is drawn, not what
                // the layout holds.
                let scaled = style.at_scale(scale);
                let layout = if dark || lock.is_some() {
                    compositor_layout::MonitorLayout {
                        windows: Vec::new(),
                        ..output.clone()
                    }
                } else {
                    compositor_render::scaled(output, origin, scale)
                };
                let mut blurred = blurs_behind(
                    &scaled,
                    &layout,
                    &over,
                    &slots,
                    &sources,
                    &drawn_with,
                    (origin, scale),
                );
                // The counter, on the first screen as Hyprland draws it,
                // over everything but a screen that is off. Its boxes blur
                // what is behind them as it stands, which the damage has to
                // know to redraw them whole.
                let picture = (overlay_on && which == 0 && !dark)
                    .then(|| overlay.picture(screen_began, scale));
                if scaled.blur.is_some()
                    && let Some(picture) = picture.as_ref()
                {
                    blurred.extend(
                        picture
                            .blurred()
                            .map(|rect| crate::damage::Blurred { rect, live: true }),
                    );
                }
                // Everything this frame is drawn from but the clients' own
                // pixels, which is what the damage is worked out from by
                // comparing it with the frame before's.
                let plan = crate::damage::Plan {
                    size: (width, height),
                    origin,
                    scale,
                    style: scaled,
                    dark,
                    locked: lock.is_some(),
                    gamma,
                    layout,
                    blurred,
                    styles: drawn_with.clone(),
                    layers: over.clone(),
                    cursor: cursor.and_then(|cursor| {
                        Some((
                            cursor,
                            crate::frame::cursor_rect(&slots, &cursor, origin, scale)?,
                        ))
                    }),
                    drag_icon: drag_icon.and_then(|icon| {
                        Some((icon, crate::frame::drag_rect(&slots, &icon, origin, scale)?))
                    }),
                    overlay: picture.as_ref().map(|picture| picture.stamp),
                    plane: screen
                        .plane
                        .on
                        .then(|| cursor_surface(&slots, &focus))
                        .flatten()
                        .map(|(client, surface, _)| (client, surface)),
                };
                // And the clients' own pixels: where on this screen each
                // commit since the last frame landed.
                let heard = commits.on(&plan, &sources);
                let frame = screen.watch.frame(plan, &heard, screen.backend.age());
                redrew = redrew.saturating_add(frame.canvas.area());
                // What the report says of the slowest frame's damage: how much
                // was drawn and in how many pieces, and how much was copied to
                // the screen -- a small change drawn as a large one is a
                // damage question, not a drawing one.
                drew_what = (
                    drew_what.0.saturating_add(frame.canvas.area()),
                    drew_what.1.saturating_add(frame.canvas.rects().len()),
                    drew_what.2.saturating_add(frame.screen.area()),
                );
                drew_from = frame.sources;
                let mut target = crate::frame::Output {
                    canvas: &mut screen.canvas,
                    backdrop: &mut screen.backdrop,
                    gpu: screen.gpu.as_mut(),
                    backend: screen.backend.as_mut(),
                    origin,
                    style: &style,
                    styles: &drawn_with,
                    scale,
                    transform: screen.transform,
                    drag_icon,
                    gamma,
                    cursor,
                    present: frame.screen,
                    overlay: picture.as_ref(),
                    overlay_took: Duration::ZERO,
                };
                let drew = if dark {
                    crate::frame::draw_dark(&mut target, &frame.canvas)
                } else if lock.is_some() {
                    // The lock's own surface is the first of `over`, which
                    // is where the damage above expects it too: the drawing
                    // and the damage read one list.
                    crate::frame::draw_locked(
                        &mut target,
                        output,
                        &slots,
                        None,
                        &over,
                        &frame.canvas,
                    )
                } else {
                    crate::frame::draw(&mut target, output, &slots, &sources, &over, &frame.canvas)
                };
                let overlay_took = target.overlay_took;
                if overlay_on {
                    overlay.rendered(&screen.name, screen_began.elapsed(), overlay_took);
                }
                match drew {
                    Ok(()) if screen.backend.lost() => {
                        screen.say_gone(report);
                        waiting = true;
                    }
                    Ok(()) => {}
                    // A GPU that has gone is not a screen that has. The
                    // software canvas has drawn nothing while the GPU was
                    // drawing, so what it is owed is everything: the watch
                    // forgets what it saw, which makes the next frame a
                    // whole one, and that frame is owed now.
                    Err(why)
                        if screen.gpu.is_some() && why.starts_with(crate::frame::GPU_FAILED) =>
                    {
                        report(&format!(
                            "hyprix: {}: {why}; drawing in software from here on",
                            screen.name
                        ));
                        screen.gpu = None;
                        screen.watch = crate::damage::Watch::default();
                        owed = true;
                    }
                    Err(why) => return Err(why),
                }
            }
            owed |= waiting;
            // The frame has been drawn from them, so the next one starts
            // from what happens next.
            commits.taken();
            least = least.min(redrew);
            drawn = drawn.saturating_add(1);
            // Every surface that went into the frame is owed two things: a
            // `wl_callback.done` for the `wl_surface.frame` it asked for,
            // and a `wp_presentation_feedback.presented` if it asked for
            // one. A client that waits on the first before drawing again --
            // which every toolkit does -- draws once and stops without it.
            frames_done(
                &mut slots,
                &sources,
                &placed_layers,
                &popups,
                lock.as_ref(),
                now,
                drawn,
                refresh_ns(&screens),
            );
            // The slowest frame, which is the bound `docs/ROADMAP.md` asks
            // each software effect to have: blur is the expensive one, and a
            // number measured on the machine that ran it is worth more than
            // one somebody hoped for.
            let took = began.elapsed();
            slowest = slowest.max(took);
            // Where the slowest frame of the report's interval spent its
            // time, which the report prints after it: a slow frame on a slow
            // machine is a question whose answer is one of these.
            let phases = compositor_render::timing::take();
            if took >= since {
                since_phases = phases;
                since_drew = drew_what;
                since_from = drew_from;
            }
            // Every so many frames, say how long the slowest of them took.
            // The compositor does not end on a machine it is the session of,
            // so a number only in the line it prints when it stops is a
            // number nobody sees: `docs/ROADMAP.md` asks each software
            // effect for a frame-time bound, and this is where it is
            // measured on the machine that ran it.
            //
            // And all of them together, after the slowest, which is what
            // the machine spent drawing: one frame in sixty may be slow for
            // a reason of its own, and a slowest frame cannot tell that
            // from sixty slow ones.
            since = since.max(took);
            spent = spent.saturating_add(took);
            counted = counted.saturating_add(1);
            if reported.elapsed() >= FRAME_REPORT {
                report(&format!(
                    "hyprix: frames {drawn} slowest of the last {counted} {} us, all of them {} us ({}; drew {} px in {} rects, showed {} px; from layout {} commits {} backdrop {} blurs {})",
                    since.as_micros(),
                    spent.as_micros(),
                    compositor_render::timing::describe(&since_phases),
                    since_drew.0,
                    since_drew.1,
                    since_drew.2,
                    since_from[0],
                    since_from[1],
                    since_from[2],
                    since_from[3]
                ));
                (since, spent, counted, reported) =
                    (Duration::ZERO, Duration::ZERO, 0, Instant::now());
            }
            if drawn == 1 {
                // The screens are up and the first frame is on them. This is
                // what a watcher waits for, in the shape
                // `userland/compositor/blank`'s marker has.
                report(&format!("hyprix: {} {display}", described(&screens)));
            }
            if let Some(directory) = options.dump.as_ref() {
                dump(&mut screens, directory, drawn)?;
            }
        }

        // A request may have changed the layout after the event snapshot was
        // made above, and a plugin's hello may have carried its first command
        // too. Run that follow-up pass now, before waiting for another
        // descriptor edge.
        if changed || plugins.needs_poll() {
            continue;
        }

        // `frames_done`, screenshots, and desktop protocol timers can queue
        // output after their slot was served. Send it before the next wait:
        // the old fixed polling pass happened to do this two milliseconds
        // later, whereas an idle compositor may otherwise wait forever for a
        // client which is waiting for this very reply.
        if slots.iter_mut().any(|slot| !slot.flush()) {
            // Let the normal connection cleanup above remove a client whose
            // queued reply could not be written before it enters the wait
            // set.
            continue;
        }

        // Nothing changes until an input descriptor, a client, a control
        // socket or a plugin becomes ready. Do not wake merely to discover
        // that: wait for one of them, or for the next frame, idle, or test
        // timer.
        let frame_wait = (owed || animating || settling)
            .then(|| pace.until(Instant::now()))
            .flatten();
        let idle = u64::try_from(forced.unwrap_or_else(|| last_input.elapsed()).as_millis())
            .unwrap_or(u64::MAX);
        let inhibited = slots.iter().any(|slot| slot.client().inhibits_idle());
        let idle_wait = forced
            .is_none()
            .then(|| {
                slots
                    .iter()
                    .filter_map(|slot| slot.client().idle_wait(idle, inhibited))
                    .min()
            })
            .flatten();
        let deadline_wait = deadline.map(|limit| limit.saturating_sub(started.elapsed()));
        // The counter's next 200 ms, so that its numbers move over a
        // desktop where nothing else does.
        let overlay_wait = overlay_was.then(|| overlay.wait(Instant::now()));
        let rescan_wait = rescan.map(|due| due.saturating_duration_since(Instant::now()));
        // What a virtual device injected this pass is carried out on the
        // next, which must then come at once: waiting for some other
        // descriptor to wake the loop left `ydotool`'s motion unseen until
        // the next frame or client message, which on a quiet desktop is
        // never.
        let injected_wait = (!injected.is_empty()).then_some(Duration::ZERO);
        // A request that has said nothing is given up on at its deadline.
        let request_wait = requests
            .iter()
            .map(crate::control::Pending::deadline)
            .min()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let timeout = [
            frame_wait,
            idle_wait,
            deadline_wait,
            overlay_wait,
            rescan_wait,
            injected_wait,
            request_wait,
        ]
        .into_iter()
        .flatten()
        .min();

        let mut fds = Vec::with_capacity(
            1 + slots.len()
                + devices.len()
                + usize::from(control.is_some())
                + usize::from(events.is_some())
                + plugins.len()
                + requests.len(),
        );
        fds.push(listener.as_raw_fd());
        fds.extend(slots.iter().map(Slot::raw_fd));
        fds.extend(devices.raw_fds());
        if let Some(socket) = control.as_ref() {
            fds.push(socket.raw_fd());
        }
        if let Some(socket) = events.as_ref() {
            fds.push(socket.raw_fd());
        }
        fds.extend(plugins.raw_fds());
        fds.extend(requests.iter().map(crate::control::Pending::raw_fd));
        // A card's descriptor: readable when its driver dies, so a screen
        // nothing is redrawn on still finds out.
        fds.extend(screens.iter().filter_map(|screen| screen.backend.raw_fd()));
        fds.extend(
            children
                .as_ref()
                .and_then(crate::children::Children::raw_fd),
        );
        // A client whose socket was full has bytes waiting for it; the loop
        // wakes when it can take them, not only when it next asks something.
        let writable: Vec<i32> = slots
            .iter()
            .filter(|slot| slot.has_pending_writes())
            .map(Slot::raw_fd)
            .collect();
        ready = Some(
            crate::wait::wait(&fds, &writable, timeout)
                .map_err(|error| format!("hyprix: event wait: {error}"))?,
        );
    }

    let (subscribers, told) = events
        .as_ref()
        .map_or((0, 0), crate::control::Events::counts);
    let (copied, pasted) = clipboard.counts();
    // How many pixels every screen together holds, which is what a frame
    // used to redraw whatever had changed.
    let pixels: i64 = screens
        .iter()
        .map(|screen| {
            let (width, height) = screen.size();
            i64::from(width).saturating_mul(i64::from(height))
        })
        .sum();
    Ok(format!(
        "hyprix: {} {display} frames {drawn} windows {} most {most} subscribers {subscribers} \
         events {told} copied {copied} pasted {pasted} slowest frame {} us smallest frame {} of \
         {pixels} pixels",
        described(&screens),
        sources.len(),
        slowest.as_micros(),
        if least == i64::MAX { 0 } else { least }
    ))
}

/// Read what one connection sent and act on it.
///
/// Gives whether the layout changed.
#[expect(
    clippy::too_many_arguments,
    reason = "what a client says reaches the layout, the rules and the clipboard, and passing one \
              struct would only move the list"
)]
fn serve(
    slots: &mut [Slot],
    index: usize,
    state: &mut State,
    sources: &mut BTreeMap<WindowId, Source>,
    next_window: &mut u32,
    rules: &mut crate::rules::Rules,
    clipboard: &mut crate::clipboard::Clipboard,
    method: &mut Option<Method>,
    shots: &mut Vec<Shot>,
    lock: &mut Option<Lock>,
    screens: &[Screen],
    urgent: &mut Vec<WindowId>,
    injected: &mut Vec<crate::seat::Input>,
    asks: &mut ScreenAsks,
    commits: &mut crate::damage::Told,
    taking_focus: bool,
    report: &mut dyn FnMut(&str),
) -> Result<bool, String> {
    let Some(slot) = slots.get_mut(index) else {
        return Ok(false);
    };
    match slot.connection.receive() {
        Ok(_) => {}
        Err(RecvError::WouldBlock) => {}
        Err(_) => {
            slot.gone = true;
            return Ok(false);
        }
    }

    let arrived = slot.connection.fds();
    let consumed = slot.client.read(slot.connection.bytes(), &arrived);
    let mut changed = false;
    // The clipboard's, kept until the client's own borrow is over: telling
    // every client what the selection holds needs them all.
    let mut selection: Vec<(Which, Option<ObjectId>, Vec<String>, Through)> = Vec::new();
    let mut wanted: Vec<(Which, ObjectId, String, Fd, Through)> = Vec::new();
    // Clipboard managers that have just made a device, which are owed both
    // selections at once whether or not they have a window.
    let mut watching: Vec<ObjectId> = Vec::new();
    // Where a client asked the pointer to be put, inside one of its own
    // surfaces: `wp_pointer_warp_v1`.
    let mut warped: Vec<(
        usize,
        ObjectId,
        (compositor_wire::Fixed, compositor_wire::Fixed),
    )> = Vec::new();

    let mut made_device: Vec<Which> = Vec::new();
    // A cursor shape a client named, and a window it asked to be raised.
    let mut shaped: Option<u32> = None;
    let mut activating: Option<(String, ObjectId)> = None;
    // The input method's two halves, which are two connections.
    let mut typing: Vec<(ObjectId, bool)> = Vec::new();
    let mut became_method: Option<ObjectId> = None;
    let mut was_typed: Vec<compositor_server::Typed> = Vec::new();
    let mut method_gone = false;
    // The same, for what a bar asked: acting on a window reaches every
    // client's slot, and this one's borrow is still open.
    let mut asked: Vec<(WindowId, ForeignRequest)> = Vec::new();
    // The windows `xdg_dialog_v1` called modal in this pass, which the
    // layout floats once the client's own borrow is over.
    let mut modals: Vec<(WindowId, bool)> = Vec::new();
    // The windows whose `xdg_toplevel` the client destroyed in this pass,
    // taken out of the layout once its borrow is over.
    let mut closed: Vec<WindowId> = Vec::new();
    // What a window asked `xdg_toplevel` for in this pass: the last
    // `set_fullscreen`/`set_maximized` state each one is in, acted on once
    // the client's own borrow is over.
    let mut asked_states: Vec<(WindowId, bool, bool)> = Vec::new();
    // Whether this client has just become a bar, which is owed the windows
    // there already are before the roundtrip it sent after binding comes
    // back.
    let mut bound_manager = false;
    // The popups made in this pass, which are placed once the client's own
    // borrow is over: placing one reads the layout and the parent window.
    let mut fresh_popups: Vec<ObjectId> = Vec::new();
    // The session lock's, for the same reason: the screens are the loop's.
    let mut locking: Option<ObjectId> = None;
    let mut covered: Vec<(ObjectId, ObjectId, usize)> = Vec::new();
    let mut unlocking: Option<bool> = None;
    if consumed > 0 {
        let events = slot.client.take_events();
        let mut claimed = 0;
        for event in events {
            match event {
                Event::PoolCreated { pool, memory } => {
                    claimed += 1;
                    match Mapping::new(memory.fd, memory.size) {
                        Ok(mapping) => {
                            let _ = slot.pools.insert(pool, mapping);
                        }
                        Err(_) => {
                            // A descriptor that is not memory: the client
                            // gets nothing drawn, and the protocol has no
                            // error the compositor may send after the fact.
                        }
                    }
                }
                Event::PoolResized { pool, size } => {
                    let resized = match slot.pools.get_mut(&pool) {
                        Some(mapping) => {
                            let _ = mapping.resize(size);
                            true
                        }
                        None => false,
                    };
                    // A pool mapped again is memory at another address, so
                    // what every surface drawn from it shows may have
                    // changed with no commit to say so.
                    let showing = if resized {
                        showing_from(&slot.client, pool)
                    } else {
                        Vec::new()
                    };
                    for surface in showing {
                        commits.painted(whole_of(index, surface));
                    }
                }
                // A window the client closed. Destroying the
                // `xdg_toplevel` is how a client with more than one window
                // shuts one of them: the connection stays open, so the loop
                // that takes a gone client's windows away never runs, and
                // without this the layout would keep tiling a window that
                // no longer exists.
                Event::Destroyed {
                    object,
                    role: Role::XdgToplevel,
                } => {
                    if let Some(at) = slot.windows.iter().position(|(top, _)| *top == object) {
                        let (_, window) = slot.windows.remove(at);
                        closed.push(window);
                    }
                }
                Event::PoolRetired { pool } => {
                    let _ = slot.retired.insert(pool);
                    if release_retired_pools(slot) {
                        commits.everything();
                    }
                }
                // A buffer going may be the last one of a pool the client
                // has already destroyed, which is when the memory is
                // finally let go. A surface still showing a buffer that has
                // gone is drawn as its border and background instead, which
                // is a change to the screen no commit announced; every
                // other destroy changes nothing, since a client destroys a
                // buffer it has finished with.
                Event::Destroyed {
                    object,
                    role: Role::Buffer,
                } => {
                    let showing: Vec<ObjectId> = slot
                        .client
                        .surfaces()
                        .filter(|(_, state)| state.current.buffer == Some(object))
                        .map(|(id, _)| id)
                        .collect();
                    let _ = release_retired_pools(slot);
                    for surface in showing {
                        commits.painted(whole_of(index, surface));
                    }
                }
                Event::LayerSurfaceCreated { layer_surface, .. } => {
                    slot.layers.push(layer_surface);
                    changed = true;
                }
                Event::LayerSurfaceChanged { .. } => changed = true,
                // The clipboard. What one client copied is the compositor's
                // to remember and to offer to the others; the data never
                // passes through it.
                // A clipboard manager: it has no window and no keyboard,
                // and is told what both selections hold anyway. That is the
                // whole of what `wlr-data-control` and `ext-data-control`
                // are, and why `cliphist` works.
                Event::DataControlBound { device } => watching.push(device),
                Event::DataControlSelection {
                    source,
                    primary,
                    mimes,
                } => selection.push((
                    if primary {
                        Which::Primary
                    } else {
                        Which::Clipboard
                    },
                    source,
                    mimes.clone(),
                    Through::Manager,
                )),
                Event::DataControlPaste {
                    offer,
                    mime,
                    fd,
                    primary,
                } => {
                    claimed += 1;
                    wanted.push((
                        if primary {
                            Which::Primary
                        } else {
                            Which::Clipboard
                        },
                        offer,
                        mime.clone(),
                        fd,
                        Through::Manager,
                    ));
                }
                // A night-light's three ramps, on a descriptor. The
                // compositor reads them here and applies them to the
                // pixels on their way out; `None` is the control going
                // away, which puts the screen back.
                Event::Gamma { output, table } => {
                    if table.is_some() {
                        claimed += 1;
                    }
                    asks.ramps.push((index, output, table));
                }
                // `wlopm` turning a screen off, which is what the `dpms`
                // dispatcher does from a keybind.
                Event::OutputPower { output, on } => asks.powered.push((index, output, on)),
                // `wlr-randr` and `kanshi` arranging the screens.
                Event::OutputConfigured {
                    configuration,
                    testing,
                    heads,
                } => asks.arranged.push((
                    index,
                    Arrangement {
                        configuration,
                        testing,
                        heads,
                    },
                )),
                // The newer screenshot: a session is owed the size of the
                // buffer to make, and a frame is filled in.
                Event::CaptureSession { session, source } => {
                    shots.extend(captured(session, source, None, state, sources, screens));
                }
                Event::CaptureAsked {
                    frame,
                    source,
                    buffer,
                } => {
                    shots.extend(captured(
                        frame,
                        source,
                        Some(buffer),
                        state,
                        sources,
                        screens,
                    ));
                }
                // A recorder sharing one window rather than a screen.
                Event::ToplevelExportAsked { frame, window } => {
                    shots.extend(exported(frame, window, None, state, sources, screens));
                }
                Event::ToplevelExportCopy {
                    frame,
                    window,
                    buffer,
                } => {
                    shots.extend(exported(
                        frame,
                        window,
                        Some(buffer),
                        state,
                        sources,
                        screens,
                    ));
                }
                // A launcher asking for the focus to stay on its own
                // surfaces until a click lands outside them.
                Event::FocusGrabbed { grab, surfaces } => {
                    report(&format!(
                        "hyprix: a client grabbed the focus onto {} surface(s)",
                        surfaces.len()
                    ));
                    let _ = grab;
                }
                // A client putting the pointer inside its own window.
                Event::PointerWarped {
                    surface,
                    at,
                    serial,
                } => {
                    let _ = serial;
                    warped.push((index, surface, at));
                }
                // A sandbox handed over a socket of its own. The compositor
                // has one listener and takes connections on it alone, so
                // the descriptors are closed and the sandbox is named in
                // the log -- which is more than silence and less than a
                // pretence that its clients are being told apart.
                Event::SecurityContext {
                    listener,
                    close,
                    engine,
                    app_id,
                    instance,
                } => {
                    claimed += 2;
                    crate::clipboard::close(listener);
                    crate::clipboard::close(close);
                    report(&format!(
                        "hyprix: a {engine} sandbox for {app_id} ({instance}) asked for a \
                         socket of its own, which this compositor does not make"
                    ));
                }
                // A bar clicking a workspace number.
                Event::WorkspaceAsked { workspace, what } => {
                    asks.workspaces.push((workspace, what));
                }
                // The requests are carried out as they arrive, so the end
                // of a batch is nothing to do.
                Event::WorkspacesCommitted => {}
                // Drag and drop: the source began it, and the target
                // answers with what it will take.
                Event::DragStarted {
                    source,
                    origin,
                    icon,
                    serial,
                    mimes,
                } => {
                    let _ = (origin, serial);
                    asks.drags.push(Drag::Started {
                        client: index,
                        source,
                        icon,
                        mimes: mimes.clone(),
                    });
                }
                Event::DragAccepted { offer, mime } => {
                    let _ = offer;
                    asks.drags.push(Drag::Accepted { mime: mime.clone() });
                }
                Event::DragActions {
                    offer,
                    actions,
                    preferred,
                } => {
                    let _ = offer;
                    asks.drags.push(Drag::Actions { actions, preferred });
                }
                Event::DragFinished { offer } => {
                    let _ = offer;
                    asks.drags.push(Drag::Finished);
                }
                Event::DataDeviceMade { .. } => made_device.push(Which::Clipboard),
                Event::PrimaryDeviceMade { .. } => made_device.push(Which::Primary),
                Event::SelectionSet { source, mimes } => {
                    selection.push((Which::Clipboard, source, mimes.clone(), Through::Window));
                }
                Event::PrimarySet { source, mimes } => {
                    selection.push((Which::Primary, source, mimes.clone(), Through::Window));
                }
                Event::SelectionWanted { offer, mime, fd } => {
                    // An offer made for a *drag* is not the selection's:
                    // the data comes from the client that started the
                    // drag, and goes on the same kind of pipe.
                    if slot.client.drag_offer() == Some(offer) {
                        asks.drags.push(Drag::Receive {
                            mime: mime.clone(),
                            fd,
                        });
                    } else {
                        wanted.push((Which::Clipboard, offer, mime.clone(), fd, Through::Window));
                    }
                    claimed += 1;
                }
                Event::PrimaryWanted { offer, mime, fd } => {
                    wanted.push((Which::Primary, offer, mime.clone(), fd, Through::Window));
                    claimed += 1;
                }
                // A client naming its cursor rather than drawing one, and a
                // client asking for another's window: both reach the loop.
                Event::CursorShaped { shape } => shaped = Some(shape),
                // Typing through an input method: the application's half
                // and the method's half are two connections, and joining
                // them is the whole of what the compositor does here.
                Event::TextInputEnabled {
                    text_input,
                    enabled,
                } => typing.push((text_input, enabled)),
                Event::InputMethodMade { method } => became_method = Some(method),
                Event::InputMethodTyped { typed, .. } => was_typed.push(typed.clone()),
                Event::InputMethodGone { .. } => method_gone = true,
                Event::ActivationAsked { token, surface } => {
                    activating = Some((token.clone(), surface));
                }
                Event::Bound {
                    role: Role::ForeignToplevelManager,
                    ..
                } => bound_manager = true,
                // A popup: a menu, a tooltip, a dropdown. Where it goes
                // needs the window its parent is, which the layout has.
                Event::PopupCreated { popup, .. } => fresh_popups.push(popup),
                Event::PopupGone { .. } | Event::PopupGrabbed { .. } => changed = true,
                // The session lock. Which screens it covers and when it is
                // told so are the loop's, which has them.
                Event::SessionLocked { lock } => locking = Some(lock),
                Event::SessionLockSurfaceMade {
                    lock_surface,
                    surface,
                    output,
                } => covered.push((lock_surface, surface, output)),
                Event::SessionUnlocked { asked } => unlocking = Some(asked),
                // A screenshot: the screens are the loop's, not this
                // client's, so both halves are carried out there.
                Event::ScreencopyWanted {
                    frame,
                    output,
                    region,
                } => shots.push(Shot::Wanted {
                    client: index,
                    frame,
                    output,
                    region,
                }),
                Event::ScreencopyInto {
                    frame,
                    buffer,
                    output,
                    region,
                    with_damage,
                } => shots.push(Shot::Into {
                    client: index,
                    frame,
                    buffer,
                    output,
                    region,
                    with_damage,
                }),
                // A bar asked for something to be done to a window it
                // does not own, which is the whole point of the protocol:
                // clicking a taskbar entry focuses that window, and the
                // middle click closes it.
                Event::ForeignToplevelAsked { window, what } => {
                    asked.push((WindowId(window), what));
                }
                // A client asking to be fullscreen or maximized. Hyprland
                // does both, and `windowrule = suppress_event` is how a
                // person turns one off -- which is only worth writing
                // because the compositor obeys it by default.
                Event::ToplevelAsked {
                    toplevel,
                    maximized,
                    fullscreen,
                } => {
                    if let Some((_, window)) = slot.windows.iter().find(|(top, _)| *top == toplevel)
                    {
                        asked_states.push((*window, maximized, fullscreen));
                    }
                }
                // A modal dialog is one the application will not let you
                // look past, so it floats: Hyprland's own `windowrule =
                // float, xdg_dialog` says the same thing by hand, and this
                // is the protocol saying it for itself.
                Event::ToplevelModal { toplevel, modal } => {
                    if let Some((_, window)) = slot.windows.iter().find(|(top, _)| *top == toplevel)
                    {
                        modals.push((*window, modal));
                    }
                }
                // There is nothing to ring: this compositor has no sound.
                // Saying so is more than the warning a toolkit logs when
                // the global is missing.
                Event::Bell { .. } => {
                    report("hyprix: a client rang the bell");
                }
                // A virtual device's input is a person's: it goes to the
                // seat, keybinds and all, which is what makes `wtype` type
                // into whatever is focused and what wlroots gates behind a
                // compositor's own policy. This one offers it to every
                // client, as Hyprland does.
                Event::Injected(what) => {
                    if let Some(input) = as_input(what) {
                        injected.push(input);
                    }
                }
                Event::ToplevelCreated { toplevel, .. } => {
                    let window = WindowId(u64::from(*next_window));
                    *next_window = next_window.saturating_add(1);
                    slot.windows.push((toplevel, window));
                    let _ = state
                        .open_window(window)
                        .map_err(|error| format!("placing a window: {error:?}"))?;
                    changed = true;
                }
                Event::SurfaceCommitted { surface, change } => {
                    if change.buffer.is_none()
                        && let Some((toplevel, window)) = slot
                            .windows
                            .iter()
                            .find(|(top, _)| {
                                slot.client
                                    .toplevel(*top)
                                    .is_some_and(|state| state.surface == surface)
                            })
                            .copied()
                    {
                        // The first commit of a window asks to be
                        // configured, and is where Hyprland applies the
                        // window's rules: by now the client has said what
                        // it is called.
                        // The frame is redrawn below whatever the rules
                        // did, since a window that has just mapped is a
                        // change in itself.
                        // What it is called now is what it opened as, and
                        // `initialclass:` and `initialtitle:` keep it after
                        // the client has renamed the window.
                        let called = slot.client.toplevel(toplevel).map_or_else(
                            || (String::new(), String::new()),
                            |top| (top.app_id.clone(), top.title.clone()),
                        );
                        let _ = slot.firsts.insert(window, called);
                        let _ = apply_rules(rules, &slot.client, toplevel, window, state, report);
                        configure_first(
                            &mut slot.client,
                            &mut slot.unsized_dialogs,
                            toplevel,
                            window,
                            state,
                        );
                        let _ = sources.insert(
                            window,
                            Source {
                                client: index,
                                surface,
                            },
                        );
                    }
                    if change.mapped
                        && let Some((toplevel, window)) = slot
                            .windows
                            .iter()
                            .find(|(top, window)| {
                                slot.unsized_dialogs.contains(window)
                                    && slot
                                        .client
                                        .toplevel(*top)
                                        .is_some_and(|state| state.surface == surface)
                            })
                            .copied()
                    {
                        let _ = slot.unsized_dialogs.remove(&window);
                        fit_dialog(&slot.client, &slot.windows, toplevel, window, state);
                        configure(&mut slot.client, state, toplevel, window);
                    }
                    changed = true;
                    // What the client says it drew, which is the only thing
                    // that can change the pixels inside a surface: where
                    // that lands on a screen is worked out when the frame
                    // is drawn, because only then is it known where the
                    // surface is.
                    commits.painted(painted(&slot.client, index, surface));
                    if let Some(old) = change.released {
                        slot.client.release_buffer(old);
                    }
                }
                _ => {}
            }
        }
        slot.connection.consume(consumed, claimed);
    }

    // Now that the client's own borrow is over: what it copied goes to
    // every other client, and what it pasted goes to whoever copied.
    for which in made_device {
        clipboard.offer_to(which, slots, index);
    }
    for _device in watching {
        clipboard.offer_both_to(slots, index);
    }
    for (which, source, mimes, through) in selection {
        let held = mimes.len();
        let name = match which {
            Which::Clipboard => "selection",
            Which::Primary => "primary selection",
        };
        clipboard.copied(which, slots, index, source, mimes, through);
        if source.is_some() {
            report(&format!(
                "hyprix: the {name} is {held} type(s) from client {index}"
            ));
        } else {
            report(&format!("hyprix: the {name} was given up"));
        }
    }
    // A cursor shape is the seat's: the compositor draws its own arrow for
    // every one of them, and says which was asked for so that what a real
    // toolkit wanted is on the record.
    if let Some(shape) = shaped {
        report(&format!("hyprix: a client asked for cursor shape {shape}"));
        changed = true;
    }
    // An activation: one program asking for another's window to be raised,
    // with a token this compositor gave out. A token it did not give out is
    // refused, which is the whole of the protocol's security.
    if let Some((token, surface)) = activating {
        changed |= activate(
            &token,
            surface,
            slots,
            index,
            state,
            sources,
            urgent,
            taking_focus,
            report,
        );
    }
    input_method_turn(
        method,
        slots,
        index,
        &typing,
        became_method,
        &was_typed,
        method_gone,
        report,
    );
    // Before anything else this client is owed: a `wl_display.sync` sent
    // after the bind is how every such client knows it has the whole list,
    // and an answer that arrives after the callback is a list it never sees.
    if bound_manager {
        let windows = crate::control::toplevels(&crate::control::describe_all(
            state,
            slots,
            sources,
            &BTreeMap::new(),
            "",
        ));
        if let Some(slot) = slots.get_mut(index) {
            slot.client_mut().show_toplevels(&windows);
        }
    }
    for popup in fresh_popups {
        changed |= place_popup(popup, slots, index, state, sources, screens);
    }
    changed |= lock_changed(
        lock, slots, index, screens, locking, &covered, unlocking, report,
    );
    for (window, what) in asked {
        report(&format!(
            "hyprix: a bar asked for {what:?} of window {}, {}",
            window.0,
            named(window, slots, sources)
        ));
        changed |= for_the_bar(window, what, state, slots, sources);
    }
    for (client, surface, (x, y)) in warped {
        // The place is inside the surface, and the pointer is put in the
        // space all screens share: a client may only move the pointer
        // inside its own window, which is what the protocol is for.
        let Some(rect) = rect_of(state, sources, client, surface) else {
            continue;
        };
        #[expect(
            clippy::cast_precision_loss,
            reason = "a screen's pixels are far inside f64's exact range"
        )]
        let at = (rect.x as f64 + x.to_f64(), rect.y as f64 + y.to_f64());
        asks.warps.push(at);
        report(&format!(
            "hyprix: client {client} put the pointer at {}, {}",
            at.0, at.1
        ));
    }
    // The windows the client closed, now that its own borrow is over:
    // the same things a gone connection's windows are given.
    for window in closed {
        remember_size(window, slots, index, state, rules);
        let _ = state.window_gone(window);
        let _ = sources.remove(&window);
        rules.window_gone(window);
        changed = true;
    }
    for (window, maximized, fullscreen) in asked_states {
        // Hyprland's fullscreen modes: 0 is the whole screen, 1 is
        // maximized inside the gaps. A window asking for both gets the
        // larger, as Hyprland's `set_fullscreen` does.
        let wanted = if fullscreen {
            Some("0")
        } else if maximized {
            Some("1")
        } else {
            None
        };
        let event = if fullscreen { "fullscreen" } else { "maximize" };
        if wanted.is_some() && rules.suppresses(window, event) {
            report(&format!(
                "hyprix: window {} asked for {event}, which a windowrule suppressed",
                window.0
            ));
            continue;
        }
        let holds = state
            .workspace_of(window)
            .and_then(|workspace| state.fullscreen(workspace))
            .is_some_and(|(id, _)| id == window);
        // `fullscreen <mode>` turns it on and off again, so a window
        // already where it asked to be is left alone.
        let mode = match wanted {
            Some(mode) if !holds => mode,
            None if holds => "0",
            _ => continue,
        };
        let was = state.focused_window();
        if state.focus_window(window).is_ok() {
            let made = state.dispatch_str("fullscreen", mode);
            changed |= made.is_ok_and(|changes| !changes.is_empty());
            if let Some(was) = was {
                let _ = state.focus_window(was);
            }
        }
    }
    for (window, modal) in modals {
        // `setfloating` and `settiled` act on the focused window, and this
        // one need not be focused; the layout's own call is what a rule
        // uses, and it takes the window.
        let was = state.focused_window();
        if state.focus_window(window).is_ok() {
            let made = state.dispatch_str(if modal { "setfloating" } else { "settiled" }, "");
            changed |= made.is_ok_and(|changes| !changes.is_empty());
            if let Some(was) = was {
                let _ = state.focus_window(was);
            }
        }
        report(&format!(
            "hyprix: window {} is {}modal",
            window.0,
            if modal { "" } else { "no longer " }
        ));
    }
    for (which, offer, mime, fd, through) in wanted {
        if clipboard.pasted(which, slots, index, offer, &mime, fd, through) {
            report(&format!(
                "hyprix: client {index} pasted {mime}, on a pipe to whoever copied"
            ));
        } else {
            report(&format!(
                "hyprix: client {index} pasted {mime} through an offer that is not the \
                 selection any more"
            ));
        }
    }
    let Some(slot) = slots.get_mut(index) else {
        return Ok(changed);
    };
    let outgoing = slot.client.take_outgoing();
    let sent = if !outgoing.bytes.is_empty() {
        slot.connection.send(&outgoing.bytes, &outgoing.descriptors)
    } else if slot.connection.has_pending_writes() {
        slot.connection.flush()
    } else {
        Ok(())
    };
    match sent {
        Ok(()) => {}
        // A full socket is a client busy for a moment, and what it would not
        // take waits in the queue. A queue past Hyprland's limit is a client
        // that has stopped reading.
        Err(compositor_socket::SendError::Overflow(waiting)) => {
            report(&format!(
                "hyprix: client {index} has stopped reading, with {waiting} bytes waiting, \
                 and is dropped"
            ));
            slot.gone = true;
        }
        Err(error) => {
            report(&format!(
                "hyprix: client {index} could not be sent to, and is dropped: {error:?}"
            ));
            slot.gone = true;
        }
    }
    // A client the server has sent a protocol error is ended by it, and the
    // client's own log names only the request it was sending when its
    // socket closed -- which is seldom the one that was wrong.
    if let Some(fatal) = slot.client.fatal() {
        report(&format!(
            "hyprix: client {index} broke the protocol, and is dropped: {}",
            fatal.message()
        ));
        slot.gone = true;
    }
    Ok(changed)
}

/// The first configure of a window that has just mapped: its place in the
/// layout, or, for a dialog `float_dialog` floats, 0x0, so that it chooses
/// its own size, which its first buffer then says.
fn configure_first(
    client: &mut Client,
    unsized_dialogs: &mut std::collections::BTreeSet<WindowId>,
    toplevel: ObjectId,
    window: WindowId,
    state: &mut State,
) {
    if float_dialog(client, toplevel, window, state) {
        let _ = unsized_dialogs.insert(window);
        let focused = state.focused_window() == Some(window);
        client.configure_toplevel(toplevel, 0, 0, &states(focused));
    } else {
        configure(client, state, toplevel, window);
    }
}

/// Float a window that has just mapped if it is a dialog, as Hyprland
/// does: one with a parent (`xdg_toplevel.set_parent`), which is also what
/// an X server's transient windows become. A window a rule floated already
/// is left where the rule put it. Floats it, for now, centred at half the
/// monitor, until its first buffer gives its size (`fit_dialog`); gives
/// whether it did.
fn float_dialog(client: &Client, toplevel: ObjectId, window: WindowId, state: &mut State) -> bool {
    let parented = client
        .toplevel(toplevel)
        .is_some_and(|top| top.parent.is_some());
    if !parented || state.is_floating(window) {
        return false;
    }
    let Some(monitor) = monitor_rect(state, window) else {
        return false;
    };
    let (width, height) = (monitor.width / 2, monitor.height / 2);
    let rect = Rect::new(
        monitor
            .x
            .saturating_add(monitor.width.saturating_sub(width) / 2),
        monitor
            .y
            .saturating_add(monitor.height.saturating_sub(height) / 2),
        width.max(1),
        height.max(1),
    );
    state.float_window(window, rect).is_ok()
}

/// Give a dialog `float_dialog` floated the size of the buffer it has just
/// drawn, centred over its parent where the parent is one of this client's
/// windows, else over its monitor.
fn fit_dialog(
    client: &Client,
    windows: &[(ObjectId, WindowId)],
    toplevel: ObjectId,
    window: WindowId,
    state: &mut State,
) {
    let Some(top) = client.toplevel(toplevel) else {
        return;
    };
    let size = client
        .window_geometry(top.surface)
        .map(|(_, _, width, height)| (i64::from(width), i64::from(height)))
        .or_else(|| {
            let surface = client.surface(top.surface)?;
            let buffer = client.buffer(surface.current.buffer?)?;
            let scale = i64::from(surface.current.scale.max(1));
            Some((
                i64::from(buffer.width) / scale,
                i64::from(buffer.height) / scale,
            ))
        });
    let Some((width, height)) = size.filter(|&(width, height)| width > 0 && height > 0) else {
        return;
    };
    let placed = |wanted: WindowId| {
        state
            .layout()
            .iter()
            .flat_map(|output| output.windows.iter())
            .find(|placed| placed.window == wanted)
            .map(|placed| placed.rect)
    };
    let over = top
        .parent
        .and_then(|parent| windows.iter().find(|(object, _)| *object == parent))
        .and_then(|(_, parent)| placed(*parent))
        .or_else(|| monitor_rect(state, window));
    let Some(over) = over else {
        return;
    };
    let rect = Rect::new(
        over.x.saturating_add(over.width.saturating_sub(width) / 2),
        over.y
            .saturating_add(over.height.saturating_sub(height) / 2),
        width,
        height,
    );
    let _ = state.float_window(window, rect);
}

/// The rectangle of the monitor a window's workspace is on.
fn monitor_rect(state: &State, window: WindowId) -> Option<Rect> {
    let workspace = state.workspace_of(window)?;
    let monitor = state.workspace_monitor(workspace)?;
    state
        .monitors()
        .find(|found| found.id == monitor)
        .map(|found| found.rect)
}

/// Apply the `windowrule` lines to a window that has just mapped.
///
/// What the window is called is what the client has said by now: a client
/// sets its title and its application id before its first commit, and the
/// first commit is where this is called from.
fn apply_rules(
    rules: &mut crate::rules::Rules,
    client: &Client,
    toplevel: ObjectId,
    window: WindowId,
    state: &mut State,
    report: &mut dyn FnMut(&str),
) -> bool {
    if rules.is_empty() {
        return false;
    }
    let named = client.toplevel(toplevel);
    let (title, class) = named.map_or((String::new(), String::new()), |top| {
        (top.title.clone(), top.app_id.clone())
    });
    let modal = named.is_some_and(|top| top.modal);
    let xdg_tag = named.map(|top| top.tag.clone()).unwrap_or_default();
    // `wp_content_type_v1` on the window's own surface, by the names
    // Hyprland's `match:content` uses.
    let content = named
        .and_then(|top| client.surface(top.surface))
        .map_or("none", |surface| {
            use compositor_protocol::content_type::wp_content_type_v1::r#type;
            match surface.current.content {
                r#type::PHOTO => "photo",
                r#type::VIDEO => "video",
                r#type::GAME => "game",
                _ => "none",
            }
        });
    let fullscreen = state
        .workspace_of(window)
        .and_then(|workspace| state.fullscreen(workspace))
        .is_some_and(|(id, _)| id == window);
    let workspace = state
        .workspace_of(window)
        .map(|workspace| state.workspace_name(workspace))
        .unwrap_or_default();
    let tags: Vec<String> = state.tags_of(window).to_vec();
    let what = compositor_config::Window {
        class: &class,
        title: &title,
        // A window that has just mapped has had no other name, so the
        // initial ones are these.
        initial_class: &class,
        initial_title: &title,
        floating: state.is_floating(window),
        fullscreen,
        focused: state.focused_window() == Some(window),
        pinned: state.is_pinned(window),
        modal,
        grouped: state.group(window).is_some(),
        tags: &tags,
        workspace: &workspace,
        // A window has no namespace; only a layer surface does, and this
        // matcher is here because Hyprland's one engine reads both.
        namespace: "",
        xdg_tag: &xdg_tag,
        content,
        // Hyprland numbers the fullscreen states, and this compositor has
        // the two a tiling layout can be in: none, or fullscreen.
        fullscreen_state_internal: i64::from(fullscreen),
        fullscreen_state_client: i64::from(named.is_some_and(|top| top.fullscreen)),
    };
    rules.apply(window, &what, state, report)
}

/// Hold the pointer where a client asked, and tell it whether the hold is
/// in force.
///
/// A constraint applies only while its own surface has the pointer: the
/// protocol says so, and it is the compositor that knows. A client whose
/// surface loses the pointer is told `unlocked` or `unconfined`, and a
/// one-shot constraint is destroyed by that.
fn hold_pointer(
    seat: &mut Seat,
    slots: &mut [Slot],
    focus: &Focus,
    state: &State,
    sources: &BTreeMap<WindowId, Source>,
) {
    let under = focus.pointer_on();
    // Every client is told, because the one that lost the pointer is the
    // one that has to hear the constraint has stopped.
    let mut hold = crate::seat::Hold::Free;
    for (index, slot) in slots.iter_mut().enumerate() {
        let surfaces: Vec<ObjectId> = slot
            .client()
            .surfaces()
            .map(|(id, _)| id)
            .filter(|surface| slot.client().constraint_on(*surface).is_some())
            .collect();
        for surface in surfaces {
            let on = under == Some((index, surface));
            if on && let Some(held) = slot.client().constraint_on(surface) {
                hold = if held.locked {
                    crate::seat::Hold::Locked
                } else {
                    rect_of(state, sources, index, surface)
                        .map_or(crate::seat::Hold::Free, crate::seat::Hold::Inside)
                };
            }
            slot.client_mut().constrain(surface, on);
        }
    }
    seat.set_hold(hold);
}

/// Where the window a surface belongs to is, in the space every window's
/// rectangle is in.
fn rect_of(
    state: &State,
    sources: &BTreeMap<WindowId, Source>,
    client: usize,
    surface: ObjectId,
) -> Option<Rect> {
    let window = sources
        .iter()
        .find(|(_, source)| source.client == client && source.surface == surface)
        .map(|(window, _)| *window)?;
    state
        .layout()
        .iter()
        .flat_map(|output| output.windows.iter())
        .find(|placed| placed.window == window)
        .map(|placed| placed.rect)
}

/// One virtual device's request as the seat's own input.
///
/// `modifiers` has no place here: the seat keeps its own xkb state from the
/// keys it is given, and a client's idea of which modifiers are held would
/// fight it. `wtype` sends both and works either way.
fn as_input(what: compositor_server::Injected) -> Option<crate::seat::Input> {
    Some(match what {
        compositor_server::Injected::Key { key, pressed } => crate::seat::Input::Key {
            code: u16::try_from(key).ok()?,
            pressed,
            repeat: false,
        },
        compositor_server::Injected::Motion { dx, dy } => crate::seat::Input::Motion {
            dx: dx.to_f64(),
            dy: dy.to_f64(),
        },
        compositor_server::Injected::MotionAbsolute {
            x,
            y,
            width,
            height,
        } => {
            // The client chooses the unit and sends the whole each number
            // is out of; a whole of nothing is a place nobody can read.
            if width == 0 || height == 0 {
                return None;
            }
            crate::seat::Input::Absolute {
                x: f64::from(x) / f64::from(width),
                y: f64::from(y) / f64::from(height),
            }
        }
        compositor_server::Injected::Button { button, pressed } => {
            crate::seat::Input::Button { button, pressed }
        }
        compositor_server::Injected::Axis { axis, value } => crate::seat::Input::Axis {
            axis,
            value: value.to_f64(),
        },
        compositor_server::Injected::Modifiers { .. } => return None,
    })
}

/// What each window is drawn with: its rule's style, with the alpha a
/// client set through `wp_alpha_modifier_v1` over the top.
fn drawn_with(
    styled: &BTreeMap<WindowId, compositor_render::WindowStyle>,
    slots: &[Slot],
    sources: &BTreeMap<WindowId, Source>,
) -> BTreeMap<WindowId, compositor_render::WindowStyle> {
    let mut out = styled.clone();
    for (&window, source) in sources {
        let Some(slot) = slots.get(source.client) else {
            continue;
        };
        if let Some(alpha) = slot.client().surface_alpha(source.surface) {
            out.entry(window).or_default().opacity = Some(alpha);
        }
        // `ext_background_effect_surface_v1.set_blur_region`: a client
        // asking for what is behind it to be blurred, which is the
        // protocol's own `layerrule = blur` for a window.
        if slot
            .client()
            .surface(source.surface)
            .is_some_and(|state| state.current.blur)
        {
            out.entry(window).or_default().blur = true;
        }
    }
    out
}

/// Tell every surface that went into the frame that it was drawn.
///
/// Two events, both owed once a frame has been presented: the frame
/// callbacks a client asked for with `wl_surface.frame`, and the
/// `wp_presentation_feedback` it asked for with `wp_presentation.feedback`.
/// The first is what makes a toolkit draw its next frame at all; the second
/// is how it knows how far ahead to draw it.
///
/// Every surface on the screen is told, not only the windows: a bar, a
/// wallpaper, a menu and the lock's own surface each drew, and each waits
/// the same way.
#[expect(
    clippy::too_many_arguments,
    reason = "a frame's surfaces come from four places and the event carries the clock, the count and the refresh"
)]
fn frames_done(
    slots: &mut [Slot],
    sources: &BTreeMap<WindowId, Source>,
    layers: &[crate::frame::Placed],
    popups: &[crate::frame::Placed],
    lock: Option<&Lock>,
    now: u32,
    drawn: u32,
    refresh: u32,
) {
    let mut surfaces: Vec<(usize, ObjectId)> = sources
        .values()
        .map(|source| (source.client, source.surface))
        .collect();
    surfaces.extend(
        layers
            .iter()
            .chain(popups)
            .map(|placed| (placed.client, placed.surface)),
    );
    if let Some(held) = lock {
        surfaces.extend(
            held.surfaces
                .values()
                .map(|(_, surface)| (held.client, *surface)),
        );
    }
    let at = now_monotonic();
    for (index, surface) in surfaces {
        let Some(slot) = slots.get_mut(index) else {
            continue;
        };
        slot.client_mut().fire_frame_callbacks(surface, now);
        slot.client_mut()
            .presented(surface, at, refresh, u64::from(drawn));
    }
}

/// How long one frame lasts on the first screen, in nanoseconds, which is
/// what `wp_presentation_feedback.presented` carries.
fn refresh_ns(screens: &[Screen]) -> u32 {
    let millihertz = screens
        .first()
        .map_or(60_000, |screen| screen.output().refresh.max(1));
    // Refresh is in millihertz: a nanosecond period is 10^12 over it.
    u32::try_from(1_000_000_000_000i64 / i64::from(millihertz)).unwrap_or(16_666_666)
}

/// Read the `workspace =` lines, saying what could not be read.
fn read_workspace_rules(
    config: &Config,
    report: &mut dyn FnMut(&str),
) -> Vec<compositor_config::WorkspaceRule> {
    let mut rules = Vec::new();
    for raw in &config.workspaces {
        match compositor_config::WorkspaceRule::parse(&raw.value) {
            Ok(rule) => rules.push(rule),
            Err(why) => report(&format!("hyprix: workspace = {}: {why}", raw.value)),
        }
    }
    rules
}

/// Read the `layerrule` lines, saying what could not be read.
fn read_layer_rules(
    config: &Config,
    report: &mut dyn FnMut(&str),
) -> Vec<compositor_config::LayerRule> {
    let mut rules = Vec::new();
    for raw in &config.layer_rules {
        match compositor_config::LayerRule::parse(&raw.value) {
            Ok(rule) => rules.push(rule),
            Err(why) => report(&format!("hyprix: layerrule = {}: {why}", raw.value)),
        }
    }
    rules
}

/// Where the Wayland socket goes.
///
/// A name with a `/` in it is an absolute path, as `wl_display_connect` reads
/// one. A bare name is joined to `XDG_RUNTIME_DIR` when there is one, and to
/// `/tmp` when there is not: a compositor started as init has no session
/// manager to set the variable, and refusing to start over it would be a
/// compositor that only runs where something else ran first.
fn resolve_display(display: &str) -> String {
    if display.contains('/') || std::env::var_os("XDG_RUNTIME_DIR").is_some() {
        return display.to_owned();
    }
    format!("/tmp/{display}")
}

/// How often the compositor says how slow its frames have been, at most.
///
/// A line a second while it is drawing, and nothing while it is not: a
/// compositor that drew ten frames in a second should say so, and one
/// showing a still screen has nothing to report.
const FRAME_REPORT: Duration = Duration::from_secs(1);

/// How often `/dev/input` is looked at again while the machine may still be
/// finding what is plugged in: a USB keyboard behind a hub on the DK1
/// arrives about a second after the host controller starts.
const RESCAN_EARLY: Duration = Duration::from_secs(1);

/// For how long after the compositor starts [`RESCAN_EARLY`] applies.
const RESCAN_SETTLING: Duration = Duration::from_secs(15);

/// How often it is looked at after that, for a device plugged in later: a
/// few directory reads every so often, and a keyboard that works within
/// seconds of being plugged in.
const RESCAN_LATER: Duration = Duration::from_secs(5);

/// The wait before the next look at `/dev/input`, `running` after start.
fn rescan_period(running: Duration) -> Duration {
    if running < RESCAN_SETTLING {
        RESCAN_EARLY
    } else {
        RESCAN_LATER
    }
}

/// The capability a live `zwlr_virtual_pointer_v1` gives the seat.
fn virtual_capabilities(slots: &[Slot]) -> u32 {
    if slots.iter().any(|slot| slot.client().has_virtual_pointer()) {
        core::wl_seat::capability::POINTER
    } else {
        0
    }
}

/// The `wl_seat.capability` bits the open devices give the seat.
fn seat_capabilities(devices: &Devices) -> u32 {
    let (keyboard, pointer) = devices.capabilities();
    let mut capabilities = 0;
    if keyboard {
        capabilities |= core::wl_seat::capability::KEYBOARD;
    }
    if pointer {
        capabilities |= core::wl_seat::capability::POINTER;
    }
    capabilities
}

/// Every screen in one line, for the compositor's marker and its log line.
///
/// One screen reads as it always did -- `card0 Virtual-1 1024x768 ...` --
/// and two are separated by a comma, so a watcher looking for the first
/// still finds it.
fn described(screens: &[Screen]) -> String {
    screens
        .iter()
        .map(Screen::describe)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Write each screen's last frame as a PPM.
///
/// One screen writes `frame-0001.ppm`, as it always did; two write
/// `frame-0001-<name>.ppm` each, so a test can tell the monitors apart.
fn dump(screens: &mut [Screen], directory: &std::path::Path, drawn: u32) -> Result<(), String> {
    let _ = std::fs::create_dir_all(directory);
    let one = screens.len() == 1;
    for screen in screens.iter_mut() {
        // As `take_shot` does, and for the same reason.
        screen.fetch();
        let path = if one {
            directory.join(format!("frame-{drawn:04}.ppm"))
        } else {
            directory.join(format!("frame-{drawn:04}-{}.ppm", screen.name))
        };
        crate::backend::write_ppm(screen.backend.as_ref(), &path)
            .map_err(|error| format!("writing {}: {error}", path.display()))?;
    }
    Ok(())
}

/// One screen, and the monitor it is.
///
/// The canvas is the screen's own: a frame is composed on it and handed to
/// the backend, and two screens never share one, since a monitor's pixels
/// are its own and its damage is too.
struct Screen {
    /// Where the frame goes.
    backend: Box<dyn Backend>,
    /// What the frame is composed on.
    canvas: Canvas,
    /// What is behind its windows, and the blur of that.
    backdrop: compositor_render::Backdrop,
    /// The same two on a GPU, when there is one to draw the frame: the
    /// software pair is then what is fallen back to if it goes.
    gpu: Option<crate::frame::Gpu>,
    /// The monitor it is, in the layout.
    monitor: MonitorId,
    /// Where it is and how big, in the logical pixels every window's
    /// rectangle is in: the screen's own size divided by the scale.
    rect: Rect,
    /// What it is called: the connector's name.
    name: String,
    /// What the monitor says it is: its make, model and serial, which a
    /// `monitor = desc:` line, `hyprctl monitors`, `wl_output.description`
    /// and a bar's own `"output"` setting all match on.
    description: String,
    /// The three parts of it, which `hyprctl monitors` prints apart.
    made: (String, String, String),
    /// How many buffer pixels one logical pixel is: `monitor = ..., 2`.
    scale: f64,
    /// How it is turned: `monitor = ..., transform, 1`. The canvas, the
    /// backdrop, the GPU's canvas and everything laid out are the monitor
    /// as it is read -- a quarter turn exchanges their width and height --
    /// and the frame is turned only when it is put in the backend's buffer
    /// (`compositor_render::transform`).
    transform: Transform,
    /// What it last drew, which is what the next frame's damage is worked
    /// out against.
    watch: crate::damage::Watch,
    /// Its cursor plane, as it was last told: `crate::plane`.
    plane: crate::plane::Plane,
    /// Whether its card's going has been said since it last came back.
    said_gone: bool,
}

impl Screen {
    /// Say that the card went away, once a loss, whichever path found it.
    fn say_gone(&mut self, report: &mut dyn FnMut(&str)) {
        if !self.said_gone {
            report(&format!(
                "hyprix: {}: the card went away; waiting for it to come back",
                self.name
            ));
            self.said_gone = true;
        }
    }

    /// Put the frame where a reader of this screen's pixels will find it.
    ///
    /// A frame drawn in software is in the backend's buffer already. One
    /// drawn on the GPU and *shown* from there never reaches it, so the
    /// whole of it is fetched here -- for a screenshot or a dumped PPM,
    /// which is a reader that is usually not there at all.
    fn fetch(&mut self) {
        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        if !self.backend.adopted() {
            return;
        }
        let (width, height) = self.backend.size();
        let stride = self.backend.stride();
        let whole = Rect::new(0, 0, i64::from(width), i64::from(height));
        if let Ok(pixels) = gpu.canvas.read(whole) {
            crate::frame::fetched(self.backend.buffer(), stride, whole, &pixels);
        }
    }
}

impl Screen {
    /// Every screen, laid out side by side from the left in the order the
    /// backends came.
    ///
    /// Hyprland's `monitor = ..., auto` does the same: a monitor with no
    /// position goes to the right of the ones already placed, so two
    /// 1024-wide screens cover 0..1024 and 1024..2048 and the pointer walks
    /// from one to the other.
    fn all(
        backends: Vec<Box<dyn Backend>>,
        rules: &[MonitorRule],
        renderer: crate::options::Renderer,
        report: &mut dyn FnMut(&str),
    ) -> Result<Vec<Self>, String> {
        let mut screens = Vec::with_capacity(backends.len());
        let mut x = 0i64;
        let mut id = 0u32;
        for mut backend in backends {
            let name = backend.name();
            let description = backend.description();
            // The last rule that names this monitor, or the last rule with
            // no name at all: Hyprland reads the file top to bottom and a
            // later line wins, but a catch-all is only for a monitor no line
            // names. A rule names a monitor by its connector or by `desc:`
            // and its description, and a person writes the second because a
            // connector's name moves when a cable does.
            let rule = MonitorRule::for_monitor(rules, &name, &description);
            if rule.is_some_and(|rule| rule.disabled) {
                continue;
            }
            let scale = rule.map_or(1.0, MonitorRule::scale_factor);
            let transform = rule.map_or(Transform::Normal, |rule| rule.transform);
            // The monitor as it is read: a 1024x768 connector turned a
            // quarter is 768 wide and 1024 tall, which is Hyprland's
            // `m_transformedSize`. Everything is drawn at this size and
            // turned into the connector's own on the way out.
            let (width, height) = transform.size(backend.size());
            let canvas =
                Canvas::new(width, height).map_err(|error| format!("a canvas: {error:?}"))?;
            let backdrop = compositor_render::Backdrop::new(width, height)
                .map_err(|error| format!("a backdrop: {error:?}"))?;
            let mut gpu = gpu_for(renderer, &name, (width, height), report)?;
            // And if this screen can be pointed at what that renderer draws
            // into, point it: the frame is then shown where it was made,
            // and nothing of it crosses between the device and the guest.
            //
            // Not a turned one: the GPU draws the monitor upright, and a
            // card pointed at that would show it upright on a monitor that
            // is not. Its frame is fetched and turned on the way to the
            // card instead, as the software canvas's is.
            if let Some(held) = gpu.as_mut() {
                if transform == Transform::Normal {
                    adopt(held, backend.as_mut(), (width, height), &name, report);
                } else {
                    report(&format!(
                        "hyprix: {name}: the monitor is turned (transform {}), so the GPU's frame \
                         is fetched and turned rather than shown where it was drawn",
                        transform.value()
                    ));
                }
            }
            // The monitor is laid out in logical pixels: a 1024x768 screen
            // at `scale = 2` tiles its windows in 512x384, as Hyprland's
            // does.
            let (logical_width, logical_height) = (logical(width, scale), logical(height, scale));
            let (at_x, at_y) = match rule.map(|rule| rule.position) {
                Some(Position::At(x, y)) => (x, y),
                // `auto`, which puts it to the right of the ones placed.
                _ => (x, 0),
            };
            x = at_x.saturating_add(logical_width);
            id = id.saturating_add(1);
            screens.push(Self {
                name,
                description,
                made: backend.made(),
                backend,
                canvas,
                backdrop,
                gpu,
                monitor: MonitorId(id),
                rect: Rect::new(at_x, at_y, logical_width, logical_height),
                scale,
                transform,
                // Nothing drawn yet, which is what makes a screen's first
                // frame a whole one.
                watch: crate::damage::Watch::default(),
                plane: crate::plane::Plane::default(),
                said_gone: false,
            });
        }
        if screens.is_empty() {
            return Err("every monitor is disabled".to_owned());
        }
        Ok(screens)
    }

    /// Run the screen at `size`, the connector's own pixels: its buffers,
    /// canvas, backdrop and GPU target made again at that size, and the
    /// GPU's frame adopted again where it was. What is laid out on it is
    /// the caller's to move, since the logical size changes with it.
    ///
    /// A screen whose GPU target cannot be made again at the new size draws
    /// in software from then on rather than not at all.
    ///
    /// # Errors
    ///
    /// The backend refusing the size; the screen is then as it was.
    fn resize(
        &mut self,
        size: (u32, u32),
        renderer: crate::options::Renderer,
        report: &mut dyn FnMut(&str),
    ) -> Result<(), String> {
        let (width, height) = self.transform.size(size);
        let canvas = Canvas::new(width, height).map_err(|error| format!("a canvas: {error:?}"))?;
        let backdrop = compositor_render::Backdrop::new(width, height)
            .map_err(|error| format!("a backdrop: {error:?}"))?;
        self.backend
            .resize(size)
            .map_err(|error| error.to_string())?;
        self.canvas = canvas;
        self.backdrop = backdrop;
        // The old target first: it holds a render device of its own.
        let renderer = if self.gpu.take().is_some() {
            renderer
        } else {
            crate::options::Renderer::Software
        };
        self.gpu = gpu_for(renderer, &self.name, (width, height), report).unwrap_or_else(|why| {
            report(&format!(
                "hyprix: {}: {why}; drawing in software",
                self.name
            ));
            None
        });
        if let Some(held) = self.gpu.as_mut()
            && self.transform == Transform::Normal
        {
            adopt(
                held,
                self.backend.as_mut(),
                (width, height),
                &self.name,
                report,
            );
        }
        self.rect = Rect::new(
            self.rect.x,
            self.rect.y,
            logical(width, self.scale),
            logical(height, self.scale),
        );
        // Nothing of the new buffers is drawn, and the cursor plane was set
        // with the old mode.
        self.watch = crate::damage::Watch::default();
        self.plane = crate::plane::Plane::default();
        Ok(())
    }

    /// What every screen covers together, which is the space the pointer
    /// moves in: its left and top edges, and its width and height.
    ///
    /// From the leftmost and topmost screen, not from 0, 0: a monitor rule
    /// may put the only screen at `2560x0`, as a configuration written for
    /// three monitors puts its middle one, and the pointer must then move
    /// over that screen and not over the 2560 pixels left of it where
    /// nothing is. Hyprland's layout box is the same.
    fn desktop(screens: &[Self]) -> (i64, i64, u32, u32) {
        let left = screens
            .iter()
            .map(|screen| screen.rect.x)
            .min()
            .unwrap_or(0);
        let top = screens
            .iter()
            .map(|screen| screen.rect.y)
            .min()
            .unwrap_or(0);
        let right = screens
            .iter()
            .map(|screen| screen.rect.right())
            .max()
            .unwrap_or(0);
        let bottom = screens
            .iter()
            .map(|screen| screen.rect.bottom())
            .max()
            .unwrap_or(0);
        (
            left,
            top,
            u32::try_from(right.saturating_sub(left)).unwrap_or(0),
            u32::try_from(bottom.saturating_sub(top)).unwrap_or(0),
        )
    }

    /// The screen's size in pixels, as it is read: the canvas's, which for a
    /// monitor turned a quarter is the connector's mode with its width and
    /// height exchanged.
    fn size(&self) -> (u32, u32) {
        self.transform.size(self.backend.size())
    }

    /// What to say about it in the compositor's log line.
    fn describe(&self) -> String {
        match self.transform {
            Transform::Normal => self.backend.describe(),
            turned => format!("{} transform {}", self.backend.describe(), turned.value()),
        }
    }

    /// What `wl_output` tells a client this screen is.
    ///
    /// The mode is in buffer pixels, which is what `wl_output.mode` carries;
    /// the position is logical, as `wl_output.geometry`'s is. The scale is
    /// the protocol's integer one, rounded up from a fractional scale: a
    /// client drawing at 2 on a screen at 1.5 is scaled down to fit, which
    /// is what a compositor without `wp_fractional_scale_v1` can offer.
    ///
    /// The mode is the connector's own, never turned: `wl_output.mode` is
    /// the hardware's, and the transform beside it is what says how it
    /// stands.
    fn output(&self) -> compositor_server::Output {
        let (width, height) = self.backend.size();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a monitor's scale is a small positive number"
        )]
        let scale = self.scale.ceil().max(1.0) as i32;
        compositor_server::Output {
            x: i32::try_from(self.rect.x).unwrap_or(0),
            y: i32::try_from(self.rect.y).unwrap_or(0),
            width: i32::try_from(width).unwrap_or(0),
            height: i32::try_from(height).unwrap_or(0),
            scale,
            transform: self.transform.value().cast_signed(),
            name: self.name.clone(),
            // Aquamarine's, which is what a client reads: the description,
            // and the connector in brackets after it.
            description: if self.description.is_empty() {
                self.name.clone()
            } else {
                format!("{} ({})", self.description, self.name)
            },
            ..compositor_server::Output::default()
        }
    }
}

/// A screen's size in the logical pixels a monitor at `scale` is laid out
/// in, never below one pixel.
fn logical(pixels: u32, scale: f64) -> i64 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a screen's pixels are far inside f64's exact range"
    )]
    let logical = (f64::from(pixels) / scale).round() as i64;
    logical.max(1)
}

/// The GPU a screen's frames are drawn on, if `renderer` asks for one and
/// there is one.
///
/// A render node is taken only when its driver is the one whose streams this
/// compositor writes: the name is what a back end is picked by, here as on
/// Linux, and a development host's own render node names another.
///
/// # Errors
///
/// `--renderer gpu` or `vtest` with none to be had, which is a test asking
/// not to be passed by the software renderer.
fn gpu_for(
    renderer: crate::options::Renderer,
    screen: &str,
    (width, height): (u32, u32),
    report: &mut dyn FnMut(&str),
) -> Result<Option<crate::frame::Gpu>, String> {
    use crate::options::Renderer;

    let device: Result<Box<dyn compositor_virgl::Device>, String> = match renderer {
        Renderer::Software => return Ok(None),
        Renderer::Vtest => compositor_virgl::vtest::Vtest::start(&format!("hyprix-{screen}"))
            .map_err(|error| error.to_string())
            .and_then(|server| server.ok_or_else(|| "no virgl_test_server".to_owned()))
            .map(|server| Box::new(server) as Box<dyn compositor_virgl::Device>),
        Renderer::Auto | Renderer::Gpu => render_node(),
    };
    let made = device.and_then(|device| {
        compositor_render::gpu::Canvas::new(device, width, height)
            .map_err(|error| error.to_string())
    });
    match made {
        Ok(canvas) => {
            report(&format!("hyprix: {screen}: frames are drawn on the GPU"));
            Ok(Some(crate::frame::Gpu {
                canvas,
                backdrop: compositor_render::gpu::Backdrop::new(width, height),
            }))
        }
        Err(why) if renderer == Renderer::Auto => {
            report(&format!(
                "hyprix: {screen}: no GPU to draw on ({why}); drawing in software"
            ));
            Ok(None)
        }
        Err(why) => Err(format!("--renderer asked for a GPU: {why}")),
    }
}

/// Show what the renderer draws into on `backend`, if both can.
///
/// `docs/GPU.md` §3.5 piece 6. A backend that cannot -- anything but a real
/// card -- and a renderer with nothing to export -- the test server, which
/// has no card beside it -- leave the frame to be fetched, which is right
/// and only slower.
fn adopt(
    gpu: &mut crate::frame::Gpu,
    backend: &mut dyn Backend,
    (width, height): (u32, u32),
    screen: &str,
    report: &mut dyn FnMut(&str),
) {
    use std::os::fd::AsFd;

    let exported = match gpu.canvas.export() {
        Ok(Some(fd)) => fd,
        Ok(None) => return,
        Err(error) => {
            report(&format!(
                "hyprix: {screen}: the frame cannot be handed to the card ({error}); it is fetched"
            ));
            return;
        }
    };
    match backend.adopt(exported.as_fd(), width, height, width.saturating_mul(4)) {
        Ok(()) => report(&format!(
            "hyprix: {screen}: the screen shows what the GPU drew, where it drew it"
        )),
        Err(error) => report(&format!(
            "hyprix: {screen}: the screen will not show the GPU's own frame ({error}); it is fetched"
        )),
    }
}

/// The card's render node, when its driver speaks virgl.
#[cfg(target_os = "linux")]
fn render_node() -> Result<Box<dyn compositor_virgl::Device>, String> {
    let device = compositor_drm::RenderDevice::open().map_err(|error| error.to_string())?;
    match device.driver() {
        Ok(driver) if driver == "virtio_gpu" => Ok(Box::new(device)),
        Ok(driver) => Err(format!(
            "the render node's driver is `{driver}`, not virtio_gpu"
        )),
        Err(error) => Err(error.to_string()),
    }
}

/// The same, where there is no `/dev/dri`.
#[cfg(not(target_os = "linux"))]
fn render_node() -> Result<Box<dyn compositor_virgl::Device>, String> {
    Err("this host has no /dev/dri".to_owned())
}

/// Open every screen the machine has, or say why there is none.
///
/// One screen a connected connector, across every card: two monitors are two
/// screens whether they are two connectors of one card or a card each.
///
/// Only Linux, and Ferrix through its Linux ABI, have `/dev/dri`; elsewhere
/// the compositor is headless or it is nothing.
#[cfg(target_os = "linux")]
fn open_screens(rules: &[MonitorRule]) -> Result<Vec<Box<dyn Backend>>, String> {
    let screens: Vec<Box<dyn Backend>> = crate::backend::Drm::open_all(rules)
        .into_iter()
        .map(|screen| Box::new(screen) as Box<dyn Backend>)
        .collect();
    if screens.is_empty() {
        // Say what the first card said rather than "none": a card that is
        // there and will not open is a different problem from no card.
        let why = crate::backend::Drm::open().err().map_or_else(
            || "no connected connector".to_owned(),
            |error| error.to_string(),
        );
        return Err(format!("/dev/dri: {why}"));
    }
    Ok(screens)
}

/// The same, where there is no `/dev/dri`.
#[cfg(not(target_os = "linux"))]
fn open_screens(_rules: &[MonitorRule]) -> Result<Vec<Box<dyn Backend>>, String> {
    Err("this host has no /dev/dri; run with --headless".to_owned())
}

/// Do what a `hyprctl` request asked, and say whether the layout changed.
///
/// `dispatch` is `userland/compositor/layout`'s own dispatcher table, so `hyprctl
/// dispatch movefocus l` and a keybind of the same name do the same thing.
/// `keyword` changes one option while the compositor runs, which is what
/// `hyprctl keyword general:gaps_in 10` is for.
fn run_ipc(
    reply: &compositor_ipc::Reply,
    state: &mut State,
    config: &mut Config,
    settings: &mut Settings,
    style: &mut Style,
    around: &mut crate::act::Around<'_>,
) -> bool {
    match reply {
        compositor_ipc::Reply::Dispatch { name, argument } => {
            dispatch(name, argument, state, around)
        }
        compositor_ipc::Reply::Keyword { name, value } => {
            if config.keyword(name, value).is_err() {
                return false;
            }
            *settings = Settings::from_config(config);
            *style = Style::from_config(config);
            let _ = state.set_settings(*settings);
            true
        }
        // `notify`, `dismissnotify` and `seterror`: a message for the
        // person at the screen. This compositor draws no overlay of its
        // own -- Hyprland's is a rectangle over everything -- so the
        // message is said and put on the event socket, where a
        // notification daemon or a bar picks it up. That is more use than
        // an overlay only this compositor can draw.
        compositor_ipc::Reply::Notify { message, error } => {
            let what = if *error { "error" } else { "notify" };
            if message.is_empty() {
                around.say(&format!("hyprix: the {what} was taken away"));
            } else {
                around.say(&format!("hyprix: {what}: {message}"));
            }
            if let Some(events) = around.events.as_mut() {
                events.say(&format!("custom>>{what},{message}\n"));
            }
            false
        }
        // `hyprctl switchxkblayout`: put the keyboard in another layout
        // group. The group was worked out by `userland/compositor/ipc`, which had
        // the snapshot to work it out from; what is left is the part only
        // the compositor can do.
        //
        // Every keyboard this seat reads shares one XKB state, so the
        // addresses name which keyboards were asked and the group is the
        // seat's either way -- `switchxkblayout all` on a machine with two
        // keyboards asks for the same group twice and gets it once.
        //
        // What the clients are told is one `wl_keyboard.modifiers` with the
        // new group in it and no new keymap, which is the whole of a
        // layout switch on the wire: the keymap they already hold carries
        // every layout the configuration named. The event socket's
        // `activelayout` follows from the next snapshot, like every other
        // event on it.
        compositor_ipc::Reply::SwitchLayout { groups, .. } => {
            let mut moved = false;
            for (_, group) in groups {
                moved |= around.seat.set_layout_group(*group);
            }
            if moved {
                around
                    .pending
                    .push(crate::seat::Action::Modifiers(around.seat.modifiers()));
            }
            // No window moved, so nothing is re-tiled: a layout switch
            // changes what the keys mean and not where anything is.
            false
        }
        compositor_ipc::Reply::Reload | compositor_ipc::Reply::Text(_) => false,
    }
}

/// Run one dispatcher, and say whether the layout changed.
///
/// The one path for both: `hyprctl dispatch movefocus l` and a keybind of the
/// same name do the same thing, because Hyprland's do and because two paths
/// would drift.
///
/// `exec` is answered here rather than by the layout, because starting a
/// program is the compositor's to do and not the tiling's. It is how a person
/// opens a terminal -- `bind = SUPER, Return, exec, foot` -- and it changes
/// no layout by itself: the window arrives later, through the socket, like
/// any other client's.
///
/// `submap` is here for the same reason: which binds are in force is the
/// seat's and not the tiling's, and it moves no window either. Both are in
/// Hyprland's one dispatcher table, and so in this one.
fn dispatch(
    name: &str,
    argument: &str,
    state: &mut State,
    around: &mut crate::act::Around<'_>,
) -> bool {
    let name = name.to_ascii_lowercase();
    let name = name.as_str();
    // The compositor's own half of Hyprland's table.
    if let Some(changed) = crate::act::compositor(name, argument, state, around) {
        return changed;
    }
    // Four dispatchers carry a window expression rather than a direction:
    // which window it picks out is the compositor's to say, since a title,
    // a class and a process are all things the layout does not hold. The
    // layout is reached back into once the window is known.
    if let Some(what) = window_named(name) {
        return one_window(what, name, argument, state, around);
    }
    let changes = match state.dispatch_str(name, argument) {
        Ok(changes) => changes,
        // A name the layout does not know may be a plugin's: Hyprland's
        // `addDispatcher` puts a plugin's name in the same table a keybind
        // and `hyprctl dispatch` look in, and this is that table's last
        // entry. The plugin acts by sending requests back, so nothing has
        // changed yet.
        Err(compositor_layout::Error::UnknownDispatcher(_)) => {
            let _ = around.plugins.dispatch(name, argument);
            return false;
        }
        Err(error) => {
            around.say(&format!("hyprix: {name}: {error}"));
            return false;
        }
    };
    // `killactive` asks a window to close, which is the client's to obey;
    // the layout says which window.
    for change in &changes {
        match change {
            compositor_layout::Change::Close(window) => {
                // `windowrule = no_close_for <ms>`: a window too young to
                // be closed is left alone, and the person is told why --
                // a key that does nothing for no reason is worse than one
                // that says so.
                if around.rules.held_open(*window) {
                    around.say(&format!(
                        "hyprix: window {} is held open by a no_close_for rule",
                        window.0
                    ));
                } else {
                    close(*window, around.slots, around.sources);
                }
            }
            // `workspace = 5, on-created-empty:foot`: going to a workspace
            // that has nothing on it starts the program the line names.
            // Once, which is what the set is for -- Hyprland runs it when
            // the workspace is *created*, and coming back to an empty one
            // must not start a second terminal.
            compositor_layout::Change::Workspace { workspace, .. } => {
                created_empty(*workspace, state, around);
            }
            _ => {}
        }
    }
    !changes.is_empty()
}

/// Run a workspace's `on-created-empty:` command, if it has one, it has
/// nothing on it, and it has not been run before.
fn created_empty(
    workspace: compositor_layout::WorkspaceId,
    state: &State,
    around: &mut crate::act::Around<'_>,
) {
    if around.opened.contains(&workspace) || !state.windows(workspace).is_empty() {
        return;
    }
    let Some(command) = state.on_created_empty(workspace) else {
        return;
    };
    let _ = around.opened.insert(workspace);
    match start(&command, around.socket, around.instance) {
        Ok(pid) => around.say(&format!(
            "hyprix: workspace {} was made empty, started {command} as {pid}",
            workspace.0
        )),
        Err(why) => around.say(&format!(
            "hyprix: workspace {}: on-created-empty {command}: {why}",
            workspace.0
        )),
    }
}

/// What a dispatcher that names a window does to the one it picks out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Named {
    /// `focuswindow`, `focuswindowbyclass`.
    Focus,
    /// `closewindow`, `killwindow`.
    Close,
    /// `movewindowpixel`.
    Move,
    /// `resizewindowpixel`.
    Resize,
}

/// Whether a dispatcher names a window with one of Hyprland's window
/// expressions, and what it does to it.
fn window_named(name: &str) -> Option<Named> {
    match name {
        "focuswindow" | "focuswindowbyclass" => Some(Named::Focus),
        "closewindow" | "killwindow" => Some(Named::Close),
        "movewindowpixel" => Some(Named::Move),
        "resizewindowpixel" => Some(Named::Resize),
        _ => None,
    }
}

/// Carry out one of those four.
fn one_window(
    what: Named,
    name: &str,
    argument: &str,
    state: &mut State,
    around: &mut crate::act::Around<'_>,
) -> bool {
    // The two pixel dispatchers put the change first and the window after a
    // comma, which is the one place Hyprland puts the window last.
    let (which, by) = match what {
        Named::Focus | Named::Close => (argument.trim(), None),
        Named::Move | Named::Resize => {
            let Some((how, which)) = argument.split_once(',') else {
                around.say(&format!(
                    "hyprix: {name} takes a change, a comma and a window"
                ));
                return false;
            };
            let Some(by) = compositor_layout::Move::parse(how.trim()) else {
                around.say(&format!("hyprix: {name}: {how:?} is not a change"));
                return false;
            };
            (which.trim(), Some(by))
        }
    };
    let Some(window) = crate::act::pick(Some(which), state, around) else {
        return false;
    };
    match (what, by) {
        (Named::Focus, _) => !state.focus_window(window).unwrap_or_default().is_empty(),
        (Named::Close, _) => {
            close(window, around.slots, around.sources);
            // The window is still there until its client obeys, so nothing
            // has moved yet.
            false
        }
        (Named::Move, Some(by)) => state
            .move_window_pixel(window, &by)
            .is_ok_and(|changes| !changes.is_empty()),
        (Named::Resize, Some(by)) => state
            .resize_window_pixel(window, &by)
            .is_ok_and(|changes| !changes.is_empty()),
        (Named::Move | Named::Resize, None) => false,
    }
}

/// Every window there is, as one of Hyprland's window expressions sees it.
///
/// Built when a dispatcher names a window and not held, because it borrows
/// every client's titles: a name a client changes between two dispatchers
/// has to be the new one, and a cache of them is a cache that goes stale in
/// the one direction that matters.
pub(crate) fn as_seen<'a>(
    state: &'a State,
    slots: &'a [Slot],
    sources: &BTreeMap<WindowId, Source>,
) -> Vec<crate::select::Seen<'a>> {
    let mut seen = Vec::new();
    for (&window, source) in sources {
        let Some(slot) = slots.get(source.client) else {
            continue;
        };
        let named = slot
            .client()
            .toplevels()
            .find(|(_, top)| top.surface == source.surface)
            .map(|(_, top)| top);
        let (class, title) =
            named.map_or(("", ""), |top| (top.app_id.as_str(), top.title.as_str()));
        // A window whose client never said what it was called mapped with
        // the same nothing it is called now.
        let (initial_class, initial_title) = slot.first_called(window).unwrap_or((class, title));
        seen.push(crate::select::Seen {
            window,
            class,
            title,
            initial_class,
            initial_title,
            tags: state.tags_of(window),
            pid: slot.pid(),
            floating: state.is_floating(window),
            workspace: state.workspace_of(window),
        });
    }
    seen
}

/// The session lock, while a program holds it.
///
/// `ext-session-lock-v1`'s whole point is that the compositor stops drawing
/// everything else the moment the lock is taken -- before the client has
/// drawn anything -- so this exists from the `lock` request and not from
/// the first frame.
#[derive(Clone, Debug, Default)]
struct Lock {
    /// Which connection holds it.
    client: usize,
    /// The lock surface for each screen: its `ext_session_lock_surface_v1`
    /// and the `wl_surface` under it.
    surfaces: BTreeMap<usize, (ObjectId, ObjectId)>,
    /// Whether the client has been told every screen is covered.
    told: bool,
    /// Whether the client that took it has gone. The screen stays locked:
    /// a lock whose program died must not become an unlocked session, which
    /// is the one thing the protocol is most explicit about.
    orphaned: bool,
}

/// A screenshot, waiting for the part of the loop that has the screens.
///
/// `zwlr_screencopy_v1` is answered in two steps -- the compositor says what
/// buffer to make, the client makes one and hands it over -- and neither can
/// be done while a client's own borrow is open, because both read a screen
/// and one writes into another client's memory.
#[derive(Clone, Copy, Debug)]
enum Shot {
    /// A program asked for one: it is owed the size and the format.
    Wanted {
        /// Which connection asked.
        client: usize,
        /// Its `zwlr_screencopy_frame_v1`.
        frame: ObjectId,
        /// Which screen, by its place in the outputs.
        output: usize,
        /// The part of it, or `None` for all of it.
        region: Option<ServerRect>,
    },
    /// It handed over the buffer to write the screenshot into.
    Into {
        /// Which connection asked.
        client: usize,
        /// Its `zwlr_screencopy_frame_v1`.
        frame: ObjectId,
        /// The `wl_buffer` to fill.
        buffer: ObjectId,
        /// Which screen.
        output: usize,
        /// The part of it, or `None` for all of it.
        region: Option<ServerRect>,
        /// Whether `copy_with_damage` asked for a `damage` event.
        with_damage: bool,
    },
    /// `hyprland_toplevel_export_manager_v1`: the same two halves, for one
    /// *window* rather than a screen. A recorder sharing one window asks
    /// for this and the compositor answers it out of the same pixels.
    Exported {
        /// Which connection asked.
        client: usize,
        /// Its `hyprland_toplevel_export_frame_v1`.
        frame: ObjectId,
        /// The `wl_buffer` to fill, or `None` where it is still owed the
        /// size.
        buffer: Option<ObjectId>,
        /// Which screen the window is on.
        output: usize,
        /// The window's rectangle on it.
        region: ServerRect,
    },
    /// `ext-image-copy-capture-v1`: the same two halves again, for a source
    /// that may be a screen or a window. The client is the one that asked,
    /// which is where the buffer is.
    Captured {
        /// The session, or the frame once there is a buffer.
        frame: ObjectId,
        /// The `wl_buffer` to fill, or `None` where the session is still
        /// owed the size.
        buffer: Option<ObjectId>,
        /// Which screen the source is on.
        output: usize,
        /// Its rectangle on that screen.
        region: ServerRect,
    },
}

/// What a commit changed inside one surface.
///
/// `userland/compositor/server`'s `surface.rs` keeps a commit's two damage lists
/// apart, because it cannot join them: `wl_surface.damage` is in surface
/// coordinates and `damage_buffer` in the buffer's, and what turns one into
/// the other is the surface's scale. This is the moment the scale is known,
/// so both become the buffer's own pixels here -- which is the space the
/// renderer draws from, since it draws the whole of a buffer into the
/// rectangle the layout gave it however the surface is scaled.
///
/// The lists read are the *current* ones, which is where the commit just
/// put them: `Surface::commit` makes the pending state current and starts
/// the next commit's lists empty.
fn painted(client: &Client, index: usize, surface: ObjectId) -> crate::damage::Painted {
    let state = client.surface(surface);
    let buffer = state
        .and_then(|state| state.current.buffer)
        .and_then(|held| client.buffer(held))
        .map(|buffer| (i64::from(buffer.width), i64::from(buffer.height)));
    let mut rects = Vec::new();
    if let Some(state) = state {
        let scale = i64::from(state.current.scale.max(1));
        let held = |rect: &ServerRect, scale: i64| {
            Rect::new(
                i64::from(rect.x).saturating_mul(scale),
                i64::from(rect.y).saturating_mul(scale),
                i64::from(rect.width).saturating_mul(scale),
                i64::from(rect.height).saturating_mul(scale),
            )
        };
        rects.extend(
            state
                .current
                .buffer_damage
                .iter()
                .map(|rect| held(rect, 1))
                .chain(state.current.damage.iter().map(|rect| held(rect, scale))),
        );
    }
    // A window drawn without its shadows: what is stretched to its tile is
    // the crop, so the damage is the crop's part of it, from the crop's
    // corner, and the buffer is the crop's size.
    if let Some(crop) = crate::frame::window_crop(client, surface) {
        let window = Rect::new(
            i64::from(crop.x),
            i64::from(crop.y),
            i64::from(crop.width),
            i64::from(crop.height),
        );
        let rects = rects
            .iter()
            .filter_map(|rect| {
                let left = rect.x.max(window.x);
                let top = rect.y.max(window.y);
                let right = rect.right().min(window.right());
                let bottom = rect.bottom().min(window.bottom());
                (right > left && bottom > top)
                    .then(|| Rect::new(left - window.x, top - window.y, right - left, bottom - top))
            })
            .collect();
        return crate::damage::Painted {
            client: index,
            surface,
            buffer: Some((window.width, window.height)),
            rects,
        };
    }
    crate::damage::Painted {
        client: index,
        surface,
        buffer,
        rects,
    }
}

/// The surfaces this frame draws a blur behind, in the screen's own
/// pixels, and which kind of blur each is.
///
/// The renderer's own condition, because the damage has to know exactly
/// which surfaces read further than they write (`crate::damage` says what
/// each kind is owed): the style has a blur, no rule turned it off for this
/// surface, and the surface can be seen through -- a buffer in a format
/// with alpha, or a window drawn at less than full opacity, which is
/// translucent everywhere. And the renderer's own rule for the kind:
/// `compositor_render::reads_backdrop`.
///
/// `layout` is already in the screen's own pixels; the layer surfaces are
/// in the logical space every rectangle above the renderer is in.
fn blurs_behind(
    style: &Style,
    layout: &compositor_layout::MonitorLayout,
    layers: &[crate::frame::Placed],
    slots: &[Slot],
    sources: &BTreeMap<WindowId, Source>,
    styles: &BTreeMap<WindowId, compositor_render::WindowStyle>,
    screen: ((i64, i64), f64),
) -> Vec<crate::damage::Blurred> {
    if style.blur.is_none() {
        return Vec::new();
    }
    let (origin, scale) = screen;
    let drawn_with = compositor_render::Styles {
        base: style,
        windows: styles,
    };
    let windows = layout
        .windows
        .iter()
        .enumerate()
        .filter(|(_, placed)| {
            let own = styles.get(&placed.window).copied().unwrap_or_default();
            let opacity = own
                .opacity
                .unwrap_or_else(|| style.opacity(placed.focused, placed.fullscreen));
            own.blur
                && (opacity < 1.0
                    || sources.get(&placed.window).is_some_and(|source| {
                        crate::frame::translucent(slots, source.client, source.surface)
                    }))
        })
        .map(|(at, placed)| crate::damage::Blurred {
            rect: placed
                .rect
                .translate(origin.0.saturating_neg(), origin.1.saturating_neg()),
            live: !compositor_render::reads_backdrop(&layout.windows, at, &drawn_with),
        });
    let layers = layers
        .iter()
        .filter(|placed| {
            placed.rules.blur && crate::frame::translucent(slots, placed.client, placed.surface)
        })
        .map(|placed| crate::damage::Blurred {
            rect: crate::frame::local(placed.rect, origin, scale),
            // `layerrule = xray` takes the blur from the kept backdrop, as
            // a tiled window does, so it is owed what such a window is owed
            // and not what a blur of the frame as it stands is: it is not
            // redrawn whole when something moves under it, and it *is*
            // grown by the blur's reach when what is behind the windows
            // changes. Only above the windows, where the renderer reads the
            // rule at all.
            live: !(placed.above && placed.rules.xray),
        });
    windows.chain(layers).collect()
}

/// `persistent_size`: keep the size a floating window closed at.
///
/// Hyprland does this in `CWindow::unmap`, for a floating window whose rule
/// asked, and reads it back when the next window of the same class and
/// title opens. A tiled window has no size of its own to keep -- its box is
/// the tiling's -- and is skipped, as Hyprland skips it.
///
/// Called while the window is still in the layout, because its rectangle is
/// what is being kept.
fn remember_size(
    window: WindowId,
    slots: &mut [Slot],
    index: usize,
    state: &mut State,
    rules: &mut crate::rules::Rules,
) {
    if !state.is_floating(window) {
        return;
    }
    let Some((class, title)) = slots.get(index).and_then(|slot| slot.first_called(window)) else {
        return;
    };
    let Some(rect) = state
        .layout()
        .iter()
        .flat_map(|output| output.windows.iter())
        .find(|placed| placed.window == window)
        .map(|placed| placed.rect)
    else {
        return;
    };
    rules.remember_size(window, class, title, (rect.width, rect.height));
}

/// Every surface of `client` whose buffer is in `pool`.
///
/// Which is to say: every surface whose pixels are in that memory, and
/// whose pixels have therefore moved when the memory has.
fn showing_from(client: &Client, pool: PoolKey) -> Vec<ObjectId> {
    client
        .surfaces()
        .filter(|(_, state)| {
            state
                .current
                .buffer
                .and_then(|held| client.buffer(held))
                .is_some_and(|buffer| buffer.pool == pool)
        })
        .map(|(id, _)| id)
        .collect()
}

/// A change to the whole of one surface, for something that happened to it
/// rather than in it: its memory moved, or its buffer went.
fn whole_of(client: usize, surface: ObjectId) -> crate::damage::Painted {
    crate::damage::Painted {
        client,
        surface,
        buffer: None,
        rects: Vec::new(),
    }
}

/// Let go of every pool the client destroyed that has no buffer left.
///
/// The two halves of `wl_shm_pool`'s lifetime: the object goes when the
/// client says so, and the memory goes when the last buffer made from it
/// does. Called on both, because either may be the last event.
///
/// Gives whether any memory went, which is a change to the screen nothing
/// else announces: a surface whose pool is no longer mapped is drawn as its
/// border and background alone.
fn release_retired_pools(slot: &mut Slot) -> bool {
    let done: Vec<PoolKey> = slot
        .retired
        .iter()
        .copied()
        .filter(|pool| !slot.client.pool_in_use(*pool))
        .collect();
    let any = !done.is_empty();
    for pool in done {
        let _ = slot.retired.remove(&pool);
        let _ = slot.pools.remove(&pool);
    }
    any
}

/// Answer one half of a screenshot.
fn take_shot(shot: &Shot, screens: &mut [Screen], slots: &mut [Slot]) {
    // A screen showing what the GPU drew has nothing in the buffer a
    // screenshot is copied out of: the frame is fetched now, for this one
    // reader, rather than every frame for a reader that is usually not
    // there.
    for screen in screens.iter_mut() {
        screen.fetch();
    }
    let screens: &[Screen] = screens;
    match *shot {
        Shot::Wanted {
            client,
            frame,
            output,
            region,
        } => {
            let Some(size) = shot_size(screens, output, region) else {
                if let Some(slot) = slots.get_mut(client) {
                    slot.client_mut().screencopy_failed(frame);
                }
                return;
            };
            if let Some(slot) = slots.get_mut(client) {
                // `XRGB8888` is what the frame is: the canvas is opaque, and
                // a screenshot with an alpha channel that is always 0xFF is
                // a larger file saying the same thing.
                slot.client_mut().screencopy_offer(
                    frame,
                    compositor_server::Format::Xrgb8888,
                    size,
                );
            }
        }
        // The newer screenshot: the same two halves, and the client that
        // asked is found from the frame rather than carried.
        Shot::Captured {
            frame,
            buffer,
            output,
            region,
        } => {
            let Some(client) = slots
                .iter()
                .position(|slot| slot.client().objects().get(frame).is_some())
            else {
                return;
            };
            match buffer {
                None => {
                    let Some((width, height)) = shot_size(screens, output, Some(region)) else {
                        if let Some(slot) = slots.get_mut(client) {
                            slot.client_mut().capture_stopped(frame);
                        }
                        return;
                    };
                    if let Some(slot) = slots.get_mut(client) {
                        slot.client_mut().capture_offer(frame, width, height);
                    }
                }
                Some(buffer) => {
                    let taken = copy_screen(screens, output, Some(region), slots, client, buffer);
                    if let Some(slot) = slots.get_mut(client) {
                        if taken {
                            slot.client_mut().capture_ready(frame, now_monotonic());
                        } else {
                            slot.client_mut().capture_failed(frame);
                        }
                    }
                }
            }
        }
        Shot::Exported {
            client,
            frame,
            buffer: None,
            output,
            region,
        } => {
            let Some((width, height)) = shot_size(screens, output, Some(region)) else {
                if let Some(slot) = slots.get_mut(client) {
                    slot.client_mut().export_done(frame, None);
                }
                return;
            };
            if let Some(slot) = slots.get_mut(client) {
                slot.client_mut().export_buffer(frame, width, height);
            }
        }
        Shot::Exported {
            client,
            frame,
            buffer: Some(buffer),
            output,
            region,
        } => {
            let taken = copy_screen(screens, output, Some(region), slots, client, buffer);
            if let Some(slot) = slots.get_mut(client) {
                slot.client_mut()
                    .export_done(frame, taken.then(now_monotonic));
            }
        }
        Shot::Into {
            client,
            frame,
            buffer,
            output,
            region,
            with_damage,
        } => {
            let taken = copy_screen(screens, output, region, slots, client, buffer);
            let Some(slot) = slots.get_mut(client) else {
                return;
            };
            if taken {
                let damaged = with_damage.then(|| {
                    let (width, height) = shot_size(screens, output, region).unwrap_or((0, 0));
                    ServerRect {
                        x: 0,
                        y: 0,
                        width: i32::try_from(width).unwrap_or(0),
                        height: i32::try_from(height).unwrap_or(0),
                    }
                });
                slot.client_mut()
                    .screencopy_ready(frame, now_monotonic(), damaged);
            } else {
                slot.client_mut().screencopy_failed(frame);
            }
        }
    }
}

/// How large the screenshot is: the screen, or the part of it asked for,
/// clipped to the screen.
///
/// Turned, for a turned monitor: a screenshot is the screen's buffer as the
/// connector scans it out, which is what Hyprland copies (its screencopy
/// frame is `m_pixelSize`, and a region's size is exchanged for a quarter
/// turn) and what `grim` expects, since it turns each output's picture by
/// the `wl_output.transform` it was told.
fn shot_size(screens: &[Screen], output: usize, region: Option<ServerRect>) -> Option<(u32, u32)> {
    let screen = screens.get(output)?;
    let area = shot_area(screen, region)?;
    let (width, height) = (
        u32::try_from(area.width).ok()?,
        u32::try_from(area.height).ok()?,
    );
    Some(screen.transform.size((width, height)))
}

/// The part of the screen a screenshot is of, in the canvas's pixels --
/// the monitor as it is read -- or `None` for a part that is not on it.
fn shot_area(screen: &Screen, region: Option<ServerRect>) -> Option<Rect> {
    let (width, height) = (screen.canvas.width(), screen.canvas.height());
    let Some(region) = region else {
        return Some(Rect::new(0, 0, i64::from(width), i64::from(height)));
    };
    let (x, y) = (region.x.max(0), region.y.max(0));
    let wanted = |start: i32, size: i32, limit: u32| -> u32 {
        let start = u32::try_from(start).unwrap_or(0);
        let size = u32::try_from(size.max(0)).unwrap_or(0);
        size.min(limit.saturating_sub(start))
    };
    let (w, h) = (
        wanted(x, region.width, width),
        wanted(y, region.height, height),
    );
    (w > 0 && h > 0).then(|| Rect::new(i64::from(x), i64::from(y), i64::from(w), i64::from(h)))
}

/// Write the screen into the client's buffer, row by row.
///
/// Gives whether it was written. The buffer has to be the size the
/// compositor said and one of the two formats `wl_shm` offers; anything else
/// is a client that did not do as it was told, and the frame fails rather
/// than the compositor writing outside what was agreed.
fn copy_screen(
    screens: &[Screen],
    output: usize,
    region: Option<ServerRect>,
    slots: &mut [Slot],
    client: usize,
    buffer: ObjectId,
) -> bool {
    let Some((width, height)) = shot_size(screens, output, region) else {
        return false;
    };
    let Some(screen) = screens.get(output) else {
        return false;
    };
    let Some(area) = shot_area(screen, region) else {
        return false;
    };
    let logical = (
        i64::from(screen.canvas.width()),
        i64::from(screen.canvas.height()),
    );
    // Where the part asked for is in the screen's buffer, which for a
    // turned monitor is not where it is on the canvas.
    let placed = compositor_render::transform::rect(screen.transform, logical, area);
    // The software canvas is the frame; a GPU's frame is not there, and
    // what was fetched of it for the screen is the screen's own buffer,
    // turned already.
    let (from, stride, (left, top)) = match screen.gpu {
        Some(_) => (
            screen.backend.drawn(),
            screen.backend.stride() as usize,
            (placed.x.max(0) as usize * 4, placed.y.max(0) as usize),
        ),
        None => (
            screen.canvas.data(),
            screen.canvas.width() as usize * 4,
            (area.x.max(0) as usize * 4, area.y.max(0) as usize),
        ),
    };
    // A turned canvas is turned into the client's buffer on the way, as it
    // is into the screen's.
    let turned = screen.gpu.is_none() && screen.transform != Transform::Normal;
    let Some(slot) = slots.get_mut(client) else {
        return false;
    };
    let Some(shape) = slot.client().buffer(buffer).copied() else {
        return false;
    };
    if shape.width != i32::try_from(width).unwrap_or(-1)
        || shape.height != i32::try_from(height).unwrap_or(-1)
    {
        return false;
    }
    let Some(mapping) = slot.pools().get(&shape.pool) else {
        return false;
    };
    let Ok(mut writable) = mapping.writable() else {
        return false;
    };
    let into = writable.bytes_mut();
    let row = width as usize * 4;
    let (offset, to_stride) = (shape.offset.max(0) as usize, shape.stride.max(0) as usize);
    if to_stride < row {
        return false;
    }
    if turned {
        let Some(bytes) = into.get_mut(offset..) else {
            return false;
        };
        let Ok(mut target) = compositor_render::Target::new(
            bytes,
            width,
            height,
            u32::try_from(to_stride).unwrap_or(0),
        ) else {
            return false;
        };
        compositor_render::transform::copy(
            screen.transform,
            logical,
            from,
            stride,
            (0, 0),
            area,
            &mut target,
            (placed.x, placed.y),
        );
        return true;
    }
    for y in 0..height as usize {
        let at = (top + y) * stride + left;
        let Some(source) = from.get(at..at + row) else {
            return false;
        };
        let start = offset + y * to_stride;
        let Some(target) = into.get_mut(start..start + row) else {
            return false;
        };
        target.copy_from_slice(source);
    }
    true
}

/// The monotonic clock, as `zwlr_screencopy_frame_v1.ready` carries it.
fn now_monotonic() -> (u64, u32) {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (since.as_secs(), since.subsec_nanos())
}

/// Everything `hyprctl` answers about that the layout does not know.
///
/// Gathered here rather than at each call so that the three places that ask
/// for a snapshot cannot disagree about what the seat and the configuration
/// say.
#[expect(
    clippy::too_many_arguments,
    reason = "`hyprctl` reads the seat, the configuration, the devices, the layers, the plugins, the animations, the log and the clock"
)]
fn as_reported<'a>(
    config: &'a Config,
    seat: &'a Seat,
    devices: &'a Devices,
    layers: &'a [crate::frame::Placed],
    plugins: &'a crate::plugins::Plugins,
    said: &'a Said,
    log: &'a [String],
    styles: &'a BTreeMap<WindowId, compositor_render::WindowStyle>,
    locked: bool,
    uptime: u64,
) -> crate::control::Reported<'a> {
    let (x, y) = seat.pointer();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the pointer is held inside the screen, which is far inside i32"
    )]
    let cursor = (x as i32, y as i32);
    crate::control::Reported {
        submap: seat.submap(),
        binds: &config.binds,
        devices,
        keyboard: seat.keyboard(),
        layers,
        plugins,
        cursor,
        locked,
        config,
        animations: &said.animations,
        beziers: &said.beziers,
        errors: &said.errors,
        workspace_rules: &said.workspace_rules,
        styles,
        log,
        uptime,
    }
}

/// What the compositor has to say about itself that is not the layout's:
/// the animation tree as it ended up, what could not be read, and the last
/// lines it printed.
///
/// Gathered once when the configuration is read and once more on a reload,
/// because `hyprctl animations` and `hyprctl configerrors` are asked for far
/// less often than a frame is drawn.
#[derive(Clone, Debug, Default)]
struct Said {
    /// Every animation node with what it ended up with.
    animations: Vec<compositor_ipc::Animation>,
    /// Every bezier.
    beziers: Vec<compositor_ipc::Bezier>,
    /// What could not be read in the configuration.
    errors: Vec<String>,
    /// Every `workspace =` line, read.
    workspace_rules: Vec<compositor_config::WorkspaceRule>,
}

/// How many lines `hyprctl rollinglog` keeps.
///
/// Hyprland keeps a rolling buffer of its own log; this keeps the same
/// shape and a bound, because a compositor that runs for a week must not
/// grow a log in memory for ever.
const ROLLING: usize = 500;

/// The cursor surface the client under the pointer asked for, if it asked
/// for one of its own.
///
/// A text field asks for an I-beam and a link for a hand; a client that has
/// said nothing is drawn the compositor's own arrow.
fn cursor_surface(slots: &[Slot], focus: &Focus) -> Option<(usize, ObjectId, (i32, i32))> {
    let (client, _) = focus.pointer_on()?;
    let slot = slots.get(client)?;
    let (surface, hotspot) = slot.client().cursor()?;
    Some((client, surface, hotspot))
}

/// Put the pointer on each screen's cursor plane, where it has one and the
/// image fits: `pointer` is the pointer -- which surface, and whether it is
/// shown at all -- and where it is, in the logical pixels screens are laid
/// out in. `true` when a screen's pointer went from its frames to its plane
/// or back, which owes that screen a frame.
///
/// The image is made again on every pass -- a pointer's worth of pixels --
/// and set only when it or its hotspot changed; otherwise the plane is only
/// moved, which waits for nothing. On a turned monitor it is turned with
/// the frame (`crate::plane::turned`), a 64 x 64 image's worth of work on
/// the passes the pointer moves.
fn sync_planes(
    screens: &mut [Screen],
    slots: &[Slot],
    pointer: Option<(crate::frame::Cursor, (f64, f64))>,
    dark: &dyn Fn(&str) -> bool,
    wanted: bool,
    report: &mut dyn FnMut(&str),
) -> bool {
    let mut switched = false;
    for screen in screens.iter_mut() {
        // The plane takes its image and its place in the buffer's
        // orientation, so on a turned monitor both are turned below -- which
        // a quarter turn can do only to a square image.
        let size = if wanted {
            screen
                .backend
                .cursor_plane()
                .filter(|&size| screen.transform.size(size) == size)
        } else {
            None
        };
        let Some(size) = size else {
            switched |= std::mem::replace(&mut screen.plane.on, false);
            continue;
        };
        // What this screen shows: the pointer where it is on this screen,
        // or nothing.
        let rect = screen.rect;
        let here = pointer.filter(|(cursor, (x, y))| {
            #[expect(
                clippy::cast_precision_loss,
                reason = "a screen's corner and size are far inside f64"
            )]
            let inside = *x >= rect.x as f64
                && *y >= rect.y as f64
                && *x < (rect.x + rect.width) as f64
                && *y < (rect.y + rect.height) as f64;
            cursor.shown && inside && !dark(&screen.name)
        });
        let arrow = compositor_render::cursor::arrow();
        let (surface, hot) = match here {
            None => (None, (0, 0)),
            Some((cursor, _)) => {
                // A client's own cursor, or the arrow when it has none or
                // its surface has no pixels yet -- which is what the frame
                // would have drawn.
                let own = cursor.surface.and_then(|(client, surface, hot)| {
                    Some((crate::frame::pixels(slots.get(client)?, surface)?, hot))
                });
                match own {
                    Some((surface, hot)) => (Some(surface), hot),
                    None => (
                        compositor_render::cursor::surface(&arrow).ok(),
                        compositor_render::cursor::HOTSPOT,
                    ),
                }
            }
        };
        let (image, on) = match crate::plane::image(surface.as_ref(), size) {
            Some(image) => (image, true),
            // Larger than the plane: the frame draws it, and the plane shows
            // nothing.
            None => (crate::plane::image(None, size).unwrap_or_default(), false),
        };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the pointer is held inside the screens, far inside i32"
        )]
        let at = here.map_or((0, 0), |(_, (x, y))| {
            let local = |value: f64, corner: i64| {
                #[expect(
                    clippy::cast_precision_loss,
                    reason = "a screen's corner is far inside f64"
                )]
                let corner = corner as f64;
                ((value - corner) * screen.scale).round() as i32
            };
            (
                local(x, rect.x).saturating_sub(hot.0),
                local(y, rect.y).saturating_sub(hot.1),
            )
        });
        // In the buffer's pixels, as the frame is.
        let frame = (screen.canvas.width(), screen.canvas.height());
        let Some((image, hot, at)) =
            crate::plane::turned(screen.transform, frame, image, size, hot, at)
        else {
            switched |= std::mem::replace(&mut screen.plane.on, false);
            continue;
        };
        let told = match screen.plane.tell(image, hot, at) {
            crate::plane::Tell::Nothing => Ok(()),
            crate::plane::Tell::Move(at) => screen.backend.move_cursor(at),
            crate::plane::Tell::Set { image, hot, at } => {
                screen.backend.set_cursor(&image, hot, at)
            }
        };
        let on = on && told.is_ok();
        if let Err(error) = told {
            // A plane that will not be told is one this screen does without:
            // the frame draws the pointer from now until it is told again.
            report(&format!(
                "hyprix: {}: the cursor plane refused: {error}",
                screen.name
            ));
            screen.plane.forget();
        }
        if std::mem::replace(&mut screen.plane.on, on) != on {
            switched = true;
            report(&format!(
                "hyprix: {}: {}",
                screen.name,
                if on {
                    "the pointer is on the card's cursor plane"
                } else {
                    "the pointer is drawn into the frame"
                }
            ));
        }
    }
    switched
}

/// Whether the pointer is drawn at all.
///
/// A client that asked for no cursor gets none, which is what a video player
/// playing full screen does. One that has never asked gets the arrow.
fn cursor_shown(slots: &[Slot], focus: &Focus) -> bool {
    let Some((client, _)) = focus.pointer_on() else {
        return true;
    };
    slots
        .get(client)
        .is_none_or(|slot| !slot.client().said_cursor() || slot.client().cursor().is_some())
}

/// Carry out what the screen protocols asked for.
///
/// Each of these reaches the screens or the layout, and a client's own
/// borrow holds both -- so they are queued while the clients are read and
/// done here. Gives whether the screen has to be drawn again.
fn carry_out(
    asks: &mut ScreenAsks,
    slots: &mut [Slot],
    screens: &mut [Screen],
    state: &mut State,
    gammas: &mut BTreeMap<usize, crate::frame::Gamma>,
    dpms: &mut BTreeMap<String, bool>,
    report: &mut dyn FnMut(&str),
) -> bool {
    let mut changed = false;
    for (client, output, table) in asks.ramps.drain(..) {
        match table.and_then(crate::frame::Gamma::read) {
            Some(gamma) => {
                let _ = gammas.insert(output, gamma);
                report(&format!(
                    "hyprix: a night-light set screen {output}'s ramps"
                ));
            }
            None => {
                let _ = gammas.remove(&output);
                report(&format!(
                    "hyprix: screen {output}'s ramps are the plain ones"
                ));
            }
        }
        let _ = client;
        changed = true;
    }
    for (client, output, on) in asks.powered.drain(..) {
        let Some(name) = screens.get(output).map(|screen| screen.name.clone()) else {
            continue;
        };
        let _ = dpms.insert(name, !on);
        if let Some(slot) = slots.get_mut(client) {
            slot.client_mut().output_powered(output, on);
        }
        report(&format!(
            "hyprix: screen {output} is {}",
            if on { "on" } else { "off" }
        ));
        changed = true;
    }
    let mut arranged = false;
    for (client, arrangement) in asks.arranged.drain(..) {
        let done = arrange(&arrangement, screens, state, report);
        if let Some(slot) = slots.get_mut(client) {
            slot.client_mut()
                .output_configured(arrangement.configuration, done);
        }
        arranged |= done && !arrangement.testing;
    }
    if arranged {
        // The screens moved, so every client's `wl_output` and every
        // `zwlr_output_head_v1` is stale. Both are the compositor's to
        // send again, and a manager that was not told would arrange
        // against yesterday's screens.
        let outputs: Vec<compositor_server::Output> = screens.iter().map(Screen::output).collect();
        for slot in slots.iter_mut() {
            slot.client_mut().set_outputs(outputs.clone());
            slot.client_mut().publish_outputs(&outputs);
        }
        changed = true;
    }
    for (workspace, what) in asks.workspaces.drain(..) {
        // `deactivate` is a workspace asking to stop being shown, which on
        // a compositor where a monitor always shows one is nothing to do.
        if what != compositor_server::WorkspaceRequest::Activate {
            continue;
        }
        let made = state.dispatch_str("workspace", &workspace.to_string());
        changed |= made.is_ok_and(|changes| !changes.is_empty());
        report(&format!("hyprix: a bar asked for workspace {workspace}"));
    }
    changed
}

/// Give each screen the size its monitor prefers now, where its `monitor =`
/// line says `preferred` or nothing: a line that names a mode keeps it, as
/// Hyprland's does. Screens placed `auto` are placed again from the left,
/// since the ones before them may have grown or shrunk. Gives whether any
/// screen changed.
///
/// Every screen is looked at, not only the one whose card spoke: two
/// connectors of one card are two screens, and the first to read the card
/// takes its news for both.
fn follow_modes(
    screens: &mut [Screen],
    rules: &[MonitorRule],
    renderer: crate::options::Renderer,
    state: &mut State,
    report: &mut dyn FnMut(&str),
) -> bool {
    let rule = |screen: &Screen| MonitorRule::for_monitor(rules, &screen.name, &screen.description);
    let mut changed = false;
    for screen in screens.iter_mut() {
        let Some(preferred) = screen.backend.preferred() else {
            continue;
        };
        let was = screen.backend.size();
        if preferred == was || screen.backend.lost() {
            continue;
        }
        if let Some(MonitorRule {
            mode: compositor_config::Mode::Fixed { .. },
            ..
        }) = rule(screen)
        {
            report(&format!(
                "hyprix: {} now prefers {}x{}; its `monitor =` line names a mode, which it keeps",
                screen.name, preferred.0, preferred.1
            ));
            continue;
        }
        match screen.resize(preferred, renderer, report) {
            Ok(()) => {
                report(&format!(
                    "hyprix: {} is {}x{} now, as its monitor prefers (was {}x{})",
                    screen.name, preferred.0, preferred.1, was.0, was.1
                ));
                changed = true;
            }
            Err(why) => report(&format!(
                "hyprix: {} prefers {}x{} and stays {}x{}: {why}",
                screen.name, preferred.0, preferred.1, was.0, was.1
            )),
        }
    }
    if !changed {
        return false;
    }
    // `auto` is to the right of the monitors already placed, as when the
    // screens were first laid out ([`Screen::all`]).
    let mut x = 0i64;
    for screen in screens.iter_mut() {
        let at = match rule(screen).map(|rule| rule.position) {
            Some(Position::At(at_x, at_y)) => (at_x, at_y),
            _ => (x, 0),
        };
        screen.rect = Rect::new(at.0, at.1, screen.rect.width, screen.rect.height);
        x = at.0.saturating_add(screen.rect.width);
        let _ = state.move_monitor(screen.monitor, screen.rect, screen.scale);
    }
    true
}

/// Move and scale the screens one arrangement names.
///
/// Gives whether it could be done. A head asking for a mode this compositor
/// does not have, or to be turned off, is refused: the screens here are
/// whatever the card has and the compositor does not choose them.
fn arrange(
    arrangement: &Arrangement,
    screens: &mut [Screen],
    state: &mut State,
    report: &mut dyn FnMut(&str),
) -> bool {
    for (which, wanted) in &arrangement.heads {
        let Some(screen) = screens.get(*which) else {
            return false;
        };
        if !wanted.on {
            report("hyprix: a program asked for a screen to be turned off, which `dpms` does");
            return false;
        }
        // The mode published is the connector's, never turned or scaled,
        // so that is what a program naming it names.
        let (width, height) = screen.backend.size();
        if let Some(size) = wanted.size
            && (i64::from(size.0), i64::from(size.1)) != (i64::from(width), i64::from(height))
        {
            report("hyprix: a program asked for a mode this screen does not have");
            return false;
        }
        // A monitor is turned by its `monitor =` line, once, when its
        // canvas is made at the turned size; turning it while it runs would
        // mean making them again, which this does not do.
        if let Some(transform) = wanted.transform
            && transform != screen.transform.value().cast_signed()
        {
            report(
                "hyprix: a program asked for a screen to be turned, which its `monitor =` line's \
                 `transform` does",
            );
            return false;
        }
    }
    if arrangement.testing {
        return true;
    }
    for (which, wanted) in &arrangement.heads {
        let Some(screen) = screens.get_mut(*which) else {
            continue;
        };
        if let Some((x, y)) = wanted.at {
            screen.rect = Rect::new(
                i64::from(x),
                i64::from(y),
                screen.rect.width,
                screen.rect.height,
            );
        }
        if let Some(scale) = wanted.scale
            && scale.to_f64() > 0.0
        {
            screen.scale = scale.to_f64();
        }
        let (rect, scale, monitor) = (screen.rect, screen.scale, screen.monitor);
        let _ = state.move_monitor(monitor, rect, scale);
        report(&format!(
            "hyprix: screen {which} is at {},{} at scale {scale}",
            rect.x, rect.y
        ));
    }
    true
}

/// What the screen protocols asked for in one pass, carried out once every
/// client's own borrow is over: each reaches the screens or the layout, and
/// a client's borrow holds both.
#[derive(Debug, Default)]
struct ScreenAsks {
    /// A night-light's ramps: which client asked, which screen, and the
    /// descriptor they are on -- `None` for a control that went away.
    ramps: Vec<(usize, usize, Option<Fd>)>,
    /// `wlopm`: which client, which screen, and whether it is to be on.
    powered: Vec<(usize, usize, bool)>,
    /// `wlr-randr`: which client, and what it asked for.
    arranged: Vec<(usize, Arrangement)>,
    /// A bar clicking a workspace number.
    workspaces: Vec<(i64, compositor_server::WorkspaceRequest)>,
    /// What the drag protocol asked for this pass.
    drags: Vec<Drag>,
    /// Where a client asked the pointer to be put, in the space all
    /// screens share: `wp_pointer_warp_v1`.
    warps: Vec<(f64, f64)>,
}

/// Carry the drag: start it, follow the pointer, and drop it.
///
/// Every step is the compositor's, because the two clients cannot see each
/// other. Gives whether the screen has to be drawn again -- the drag icon
/// follows the pointer, so it always does while one is on.
#[expect(
    clippy::too_many_arguments,
    reason = "a drag reaches two connections, the layout, the pointer, the clock and the log"
)]
fn carry_drag(
    asks: &mut ScreenAsks,
    carried: &mut Option<crate::dragging::Carried>,
    slots: &mut [Slot],
    state: &State,
    sources: &BTreeMap<WindowId, Source>,
    seat: &Seat,
    now: u32,
    report: &mut dyn FnMut(&str),
) -> bool {
    let mut changed = false;
    for step in asks.drags.drain(..) {
        match step {
            Drag::Started {
                client,
                source,
                icon,
                mimes,
            } => {
                // A drag that starts while one is on ends the first, which
                // is what a client that lost track of its own button does.
                if let Some(held) = carried.as_mut() {
                    held.ended(slots, false);
                }
                let actions = source
                    .and_then(|source| {
                        slots
                            .get(client)
                            .map(|slot| slot.client().source_actions(source))
                    })
                    .unwrap_or(0);
                report(&format!(
                    "hyprix: client {client} started a drag of {} type(s)",
                    mimes.len()
                ));
                *carried = Some(crate::dragging::Carried {
                    client,
                    source,
                    icon,
                    mimes,
                    actions,
                    over: None,
                    at: None,
                    accepted: None,
                    action: 0,
                    dropped: false,
                });
                changed = true;
            }
            Drag::Accepted { mime } => {
                if let Some(held) = carried.as_mut() {
                    held.accepted(slots, mime);
                }
            }
            Drag::Actions { actions, preferred } => {
                if let Some(held) = carried.as_mut() {
                    held.actions(slots, actions, preferred);
                }
            }
            Drag::Finished => {
                if let Some(mut held) = carried.take() {
                    held.ended(slots, true);
                    report("hyprix: the drag was taken");
                }
                changed = true;
            }
            Drag::Receive { mime, fd } => {
                let sent = carried
                    .as_ref()
                    .and_then(|held| Some((held.client, held.source?)))
                    .and_then(|(client, source)| Some((slots.get_mut(client)?, source)));
                match sent {
                    Some((slot, source)) => {
                        slot.client_mut().send_selection(source, &mime, fd);
                        // Sent now: the descriptor is closed on the next
                        // line, and one let go of before the message
                        // carrying it has been written is one the client
                        // never gets.
                        let _ = slot.flush();
                        report(&format!("hyprix: the drag's {mime} went to whoever asked"));
                    }
                    None => report("hyprix: a drag was asked for data nobody is dragging"),
                }
                crate::clipboard::close(fd);
            }
        }
    }
    let Some(held) = carried.as_mut() else {
        return changed;
    };
    // A drag that has dropped is waiting for the target's `finish` and
    // nothing else: the pointer is the windows' again.
    if !held.holding() {
        return changed;
    }
    // Where the pointer is now, without moving its own focus: a window told
    // `wl_pointer.enter` mid-drag would think the person had clicked it.
    let at = seat.pointer();
    let under = crate::deliver::under_pointer(state, sources, at);
    changed |= held.moved(slots, under, now);
    // The button coming up is the drop. The seat holds no button state, so
    // this is the one place the compositor asks: a drag ends when nothing
    // is held down any more.
    if !held.dropped && !seat.buttons_held() {
        let taken = held.dropped(slots);
        report(if taken {
            "hyprix: the drag was dropped, and taken"
        } else {
            "hyprix: the drag was dropped on nothing that takes it"
        });
        changed = true;
        // A drop nobody took, and a drag with no source -- an icon and
        // nothing else -- have nobody to say `finish`, so they end here.
        // Waiting for one kept the drag on for good: its icon followed the
        // pointer, and no window was sent the pointer again, since none is
        // while a drag is carried.
        if (!taken || held.source.is_none())
            && let Some(mut held) = carried.take()
        {
            held.ended(slots, false);
        }
    }
    changed
}

/// One step of a drag, queued while the clients are read.
#[derive(Clone, Debug)]
enum Drag {
    /// `wl_data_device.start_drag`.
    Started {
        /// Which connection began it.
        client: usize,
        /// Its `wl_data_source`, or `None` for a drag with nothing on it.
        source: Option<ObjectId>,
        /// The surface drawn at the pointer, if it gave one.
        icon: Option<ObjectId>,
        /// The types the source can give the data in.
        mimes: Vec<String>,
    },
    /// `wl_data_offer.accept`: the type the target will take, or `None`.
    Accepted {
        /// The type.
        mime: Option<String>,
    },
    /// `wl_data_offer.set_actions`.
    Actions {
        /// What the target can do.
        actions: u32,
        /// Which of those it would rather.
        preferred: u32,
    },
    /// `wl_data_offer.finish`: the target has taken it.
    Finished,
    /// `wl_data_offer.receive` on a drag's offer: the target is asking for
    /// the data, which comes from the client that started the drag.
    Receive {
        /// The type it asked for.
        mime: String,
        /// The pipe to write it to.
        fd: Fd,
    },
}

/// One half of an `ext-image-copy-capture-v1` capture, as a [`Shot`].
///
/// `None` for the buffer is the session being told what size to make one;
/// `Some` is the frame being filled in. A source that is not on any screen
/// is no capture at all.
fn captured(
    object: ObjectId,
    source: compositor_server::Source,
    buffer: Option<ObjectId>,
    state: &State,
    sources: &BTreeMap<WindowId, Source>,
    screens: &[Screen],
) -> Option<Shot> {
    let (client, output, region) = match source {
        compositor_server::Source::Screen(which) => {
            let screen = screens.get(which)?;
            (
                None,
                which,
                ServerRect {
                    x: 0,
                    y: 0,
                    width: i32::try_from(screen.rect.width).unwrap_or(0),
                    height: i32::try_from(screen.rect.height).unwrap_or(0),
                },
            )
        }
        compositor_server::Source::Window(window) => {
            let Shot::Exported {
                client,
                output,
                region,
                ..
            } = exported(object, window, buffer, state, sources, screens)?
            else {
                return None;
            };
            (Some(client), output, region)
        }
    };
    let _ = client;
    Some(Shot::Captured {
        frame: object,
        buffer,
        output,
        region,
    })
}

/// One half of a window capture, as a [`Shot`] the loop can answer.
///
/// `None` for the buffer is the half that is owed the size; `Some` is the
/// one that fills it in. A window that is not on any screen, or a client
/// this compositor does not own, is no capture at all -- the frame is told
/// `failed` by the caller when this gives nothing.
fn exported(
    frame: ObjectId,
    window: u64,
    buffer: Option<ObjectId>,
    state: &State,
    sources: &BTreeMap<WindowId, Source>,
    screens: &[Screen],
) -> Option<Shot> {
    let window = WindowId(window);
    let source = sources.get(&window)?;
    let placed = state
        .layout()
        .into_iter()
        .flat_map(|output| output.windows)
        .find(|placed| placed.window == window)?;
    // Which screen it is on, and where on it: the region a capture reads is
    // in the screen's own pixels and a window's rectangle is in the space
    // all screens share.
    let (which, screen) = screens.iter().enumerate().find(|(_, screen)| {
        screen.rect.x <= placed.rect.x && placed.rect.x < screen.rect.right()
    })?;
    Some(Shot::Exported {
        client: source.client,
        frame,
        buffer,
        output: which,
        region: ServerRect {
            x: i32::try_from(placed.rect.x - screen.rect.x).unwrap_or(0),
            y: i32::try_from(placed.rect.y - screen.rect.y).unwrap_or(0),
            width: i32::try_from(placed.rect.width).unwrap_or(0),
            height: i32::try_from(placed.rect.height).unwrap_or(0),
        },
    })
}

/// One arrangement of the screens a program asked for.
#[derive(Clone, Debug)]
struct Arrangement {
    /// The `zwlr_output_configuration_v1`, which is owed an answer.
    configuration: ObjectId,
    /// Whether it only asked whether the arrangement would work.
    testing: bool,
    /// What each screen is to become, by its place in the outputs.
    heads: Vec<(usize, compositor_server::Wanted)>,
}

/// The input method for the seat, while a program is one.
#[derive(Clone, Copy, Debug)]
struct Method {
    /// Which connection it is.
    client: usize,
    /// Its `zwp_input_method_v2`.
    object: ObjectId,
    /// The text field it is typing into, if one is enabled.
    into: Option<(usize, ObjectId)>,
    /// The serial the next `done` carries, which the protocol makes the
    /// count of commits the text field has made.
    serial: u32,
}

/// Join the input method's two halves for one client's pass.
///
/// An application says it wants to be typed into and an input method says
/// what was typed; the two are different connections and neither can see the
/// other, so the compositor is what passes each to the other. With no input
/// method running, an application that enables a text input is told nothing
/// -- which is a session with no IME, and is the truth rather than a
/// pretence.
#[expect(
    clippy::too_many_arguments,
    reason = "the two halves of an input method are two connections and four kinds of event"
)]
fn input_method_turn(
    method: &mut Option<Method>,
    slots: &mut [Slot],
    index: usize,
    typing: &[(ObjectId, bool)],
    became: Option<ObjectId>,
    was_typed: &[compositor_server::Typed],
    gone: bool,
    report: &mut dyn FnMut(&str),
) {
    if let Some(object) = became {
        if method.is_some() {
            // One input method a seat. A second is told it will never be
            // given the keyboard, which is what `unavailable` means.
            if let Some(slot) = slots.get_mut(index) {
                slot.client_mut().input_method_unavailable(object);
            }
            report("hyprix: a second input method asked for the seat and was refused");
        } else {
            *method = Some(Method {
                client: index,
                object,
                into: None,
                serial: 0,
            });
            report("hyprix: an input method took the seat");
        }
    }
    if gone && method.is_some_and(|held| held.client == index) {
        *method = None;
        report("hyprix: the input method went");
    }
    for (text_input, enabled) in typing {
        let Some(held) = method.as_mut() else {
            // No input method: the application is told nothing, which is
            // what a session with no IME looks like from inside it.
            continue;
        };
        held.into = enabled.then_some((index, *text_input));
        let (client, object) = (held.client, held.object);
        if let Some(slot) = slots.get_mut(client) {
            slot.client_mut().input_method_active(object, *enabled);
        }
        report(&format!(
            "hyprix: a text field {} the input method",
            if *enabled { "took" } else { "let go of" }
        ));
    }
    for typed in was_typed {
        let Some(held) = method.as_mut() else {
            continue;
        };
        let Some((client, text_input)) = held.into else {
            continue;
        };
        held.serial = held.serial.saturating_add(1);
        let serial = held.serial;
        if let Some(slot) = slots.get_mut(client) {
            slot.client_mut()
                .text_input_typed(text_input, typed, serial);
        }
        report("hyprix: the input method typed into the focused text field");
    }
}

/// Raise the window `surface` is, if `token` is one the compositor gave
/// out.
///
/// `xdg_activation_v1` is how one program asks for another's window: the
/// first asks the compositor for a token, hands it over by whatever means it
/// has, and the second passes it back with the surface it wants raised. A
/// token the compositor did not make is refused, which is the whole of what
/// stops any program stealing the focus whenever it likes.
#[expect(
    clippy::too_many_arguments,
    reason = "an activation reaches every connection, the layout, the urgency list and the option that decides between the last two"
)]
fn activate(
    token: &str,
    surface: ObjectId,
    slots: &mut [Slot],
    index: usize,
    state: &mut State,
    sources: &BTreeMap<WindowId, Source>,
    urgent: &mut Vec<WindowId>,
    taking_focus: bool,
    report: &mut dyn FnMut(&str),
) -> bool {
    // Any client may have been given the token, since the program that asked
    // for it is not the one that uses it.
    let known = slots
        .iter_mut()
        .any(|slot| slot.client_mut().takes_token(token));
    if !known {
        report("hyprix: an activation with a token this compositor never gave out");
        return false;
    }
    let window = sources
        .iter()
        .find(|(_, source)| source.client == index && source.surface == surface)
        .map(|(window, _)| *window);
    let Some(window) = window else {
        report("hyprix: an activation for a surface that is not a window");
        return false;
    };
    // `misc:focus_on_activate` is off in Hyprland and off here: a program
    // that asks for another's window makes it urgent, and the person
    // decides, with `focusurgentorlast`, whether to go to it. With the
    // option on the focus goes there at once.
    if !taking_focus {
        urgent.retain(|held| *held != window);
        urgent.push(window);
        report(&format!("hyprix: window {} is urgent", window.0));
        return false;
    }
    if state.focus_window(window).is_err() {
        return false;
    }
    report(&format!("hyprix: window {} was activated", window.0));
    true
}

/// Where every popup is on the screen, in the order they were made.
///
/// A popup is drawn like a layer surface on the top level: above the
/// windows, with no border and no gaps, at the rectangle the compositor
/// configured it to. A submenu is a popup on a popup, and its coordinates
/// are its parent's, so the walk up is the same one that placed it.
pub(crate) fn placed_popups(
    slots: &[Slot],
    state: &State,
    sources: &BTreeMap<WindowId, Source>,
) -> Vec<crate::frame::Placed> {
    let mut out = Vec::new();
    for (index, slot) in slots.iter().enumerate() {
        for (_, popup) in slot.client().popups() {
            let Some(at) = popup.placed else {
                continue;
            };
            let Some(parent) = popup_parent_rect(slot, popup, state, sources, index) else {
                continue;
            };
            out.push(crate::frame::Placed {
                client: index,
                surface: popup.surface,
                rect: Rect::new(
                    parent.x.saturating_add(i64::from(at.0)),
                    parent.y.saturating_add(i64::from(at.1)),
                    i64::from(at.2),
                    i64::from(at.3),
                ),
                above: true,
                rules: crate::frame::LayerRules::default(),
            });
        }
    }
    out
}

/// Put one popup where its positioner says, and tell the client.
///
/// Gives whether anything changed, which is whenever the popup was placed:
/// a popup that has just been configured will draw, and a frame has to
/// follow.
///
/// The anchor rectangle and the answer are both in the parent's
/// surface-local coordinates, which is what `xdg_popup.configure` carries;
/// the *screen* is what the constraint adjustments are measured against, so
/// the parent's own place on it is what turns one into the other.
fn place_popup(
    popup: ObjectId,
    slots: &mut [Slot],
    index: usize,
    state: &State,
    sources: &BTreeMap<WindowId, Source>,
    screens: &[Screen],
) -> bool {
    let Some(slot) = slots.get(index) else {
        return false;
    };
    let Some(held) = slot.client().popup(popup).cloned() else {
        return false;
    };
    // Where the parent is on the screen: a window, a layer surface, or
    // another popup hanging off one.
    let Some(parent) = popup_parent_rect(slot, &held, state, sources, index) else {
        return false;
    };
    // The screen it is on, which is what it must be kept inside.
    let room = screens
        .iter()
        .find(|screen| {
            screen.rect.x <= parent.x && parent.x < screen.rect.x.saturating_add(screen.rect.width)
        })
        .map_or(parent, |screen| screen.rect);
    let numbers = held.positioner;
    let wanted = compositor_layout::popup::Positioner {
        size: (i64::from(numbers.size.0), i64::from(numbers.size.1)),
        anchor_rect: Rect::new(
            i64::from(numbers.anchor_rect.0),
            i64::from(numbers.anchor_rect.1),
            i64::from(numbers.anchor_rect.2),
            i64::from(numbers.anchor_rect.3),
        ),
        anchor: compositor_layout::popup::Anchor::from_wire(numbers.anchor).unwrap_or_default(),
        gravity: compositor_layout::popup::Anchor::from_wire(numbers.gravity).unwrap_or_default(),
        adjust: compositor_layout::popup::Adjust(numbers.adjust),
        offset: (i64::from(numbers.offset.0), i64::from(numbers.offset.1)),
        reactive: numbers.reactive,
    };
    let at = compositor_layout::popup::place(&wanted, parent, room);
    let Some(slot) = slots.get_mut(index) else {
        return false;
    };
    let number = |value: i64| i32::try_from(value).unwrap_or(0);
    slot.client_mut().configure_popup(
        popup,
        (
            number(at.x),
            number(at.y),
            number(at.width),
            number(at.height),
        ),
    );
    true
}

/// Where a popup's parent is on the screen: the layer surface that took it
/// with `get_popup` -- a bar, for its tooltip -- or else its `xdg_surface`.
fn popup_parent_rect(
    slot: &Slot,
    popup: &compositor_server::Popup,
    state: &State,
    sources: &BTreeMap<WindowId, Source>,
    index: usize,
) -> Option<Rect> {
    if let Some(layer) = popup.layer_parent {
        return slot.layer_rects.get(&layer).copied();
    }
    parent_rect(slot, popup.parent, state, sources, index)
}

/// Where a popup's parent is on the screen.
///
/// A popup hangs off an `xdg_surface`, which is either a window's or another
/// popup's: a submenu is a popup on a popup, and its coordinates are its
/// parent's, which are in turn its parent's -- down to a window or to the
/// layer surface a bar's popup hangs off.
fn parent_rect(
    slot: &Slot,
    parent: ObjectId,
    state: &State,
    sources: &BTreeMap<WindowId, Source>,
    index: usize,
) -> Option<Rect> {
    let client = slot.client();
    // A window: the rectangle the tiling gave it.
    let surface = client.xdg_surface(parent).map(|xdg| xdg.surface)?;
    if let Some((window, _)) = sources
        .iter()
        .find(|(_, source)| source.client == index && source.surface == surface)
    {
        return state
            .layout()
            .iter()
            .flat_map(|output| output.windows.iter())
            .find(|placed| placed.window == *window)
            .map(|placed| placed.rect);
    }
    // Another popup: where that one was put, inside *its* parent.
    let (id, held) = client
        .popups()
        .find(|(_, popup)| popup.xdg_surface == parent)?;
    let _ = id;
    let at = held.placed?;
    let grandparent = popup_parent_rect(slot, held, state, sources, index)?;
    Some(Rect::new(
        grandparent.x.saturating_add(i64::from(at.0)),
        grandparent.y.saturating_add(i64::from(at.1)),
        i64::from(at.2),
        i64::from(at.3),
    ))
}

/// Carry out what one client's pass said about the session lock.
///
/// Gives whether the screen has to be drawn again, which is every one of
/// them: taking the lock blanks the screen, covering a screen draws what
/// the client put there, and unlocking gives the screen back to the windows.
#[expect(
    clippy::too_many_arguments,
    reason = "the lock reaches the screens, the client and the log, and is three events in one               pass"
)]
fn lock_changed(
    lock: &mut Option<Lock>,
    slots: &mut [Slot],
    index: usize,
    screens: &[Screen],
    locking: Option<ObjectId>,
    covered: &[(ObjectId, ObjectId, usize)],
    unlocking: Option<bool>,
    report: &mut dyn FnMut(&str),
) -> bool {
    let mut changed = false;
    if let Some(object) = locking {
        if lock.is_some() {
            // One lock at a time. A second program is told it will never be
            // given the screen, which is what `finished` means and what it
            // is to answer by giving up.
            if let Some(slot) = slots.get_mut(index) {
                slot.client_mut().session_lock_refused(object);
            }
            report("hyprix: a second program asked to lock the session and was refused");
        } else {
            *lock = Some(Lock {
                client: index,
                ..Lock::default()
            });
            report("hyprix: the session is locked");
            // The windows stop being drawn now, not when the client has
            // drawn something: that is what the protocol is for.
            changed = true;
        }
    }
    for (lock_surface, surface, output) in covered {
        let Some(held) = lock.as_mut() else {
            continue;
        };
        if held.client != index {
            continue;
        }
        let _ = held.surfaces.insert(*output, (*lock_surface, *surface));
        // The size it must draw at, which is the screen's own as it is
        // read: a turned monitor's lock is drawn upright, like everything
        // else on it.
        if let (Some(screen), Some(slot)) = (screens.get(*output), slots.get_mut(index)) {
            let (width, height) = screen.size();
            slot.client_mut()
                .configure_lock_surface(*lock_surface, (width, height));
        }
        changed = true;
    }
    // Told once, and only when every screen is covered: `locked` means the
    // screen shows what the client drew and nothing of what was there.
    if let Some(held) = lock.as_mut()
        && !held.told
        && held.client == index
        && !screens.is_empty()
        && (0..screens.len()).all(|screen| held.surfaces.contains_key(&screen))
    {
        held.told = true;
        if let Some(slot) = slots.get_mut(index) {
            slot.client_mut().session_is_locked();
        }
        report(&format!(
            "hyprix: the lock covers {} screen(s)",
            screens.len()
        ));
    }
    if let Some(asked) = unlocking
        && lock.as_ref().is_some_and(|held| held.client == index)
    {
        *lock = None;
        changed = true;
        report(if asked {
            "hyprix: the session is unlocked"
        } else {
            "hyprix: the lock went"
        });
    }
    changed
}

/// Where each slot will be once the ones that have gone are taken out.
///
/// `None` for a slot that is going. The compositor holds a client by its
/// place in the list -- a `Source`, the focus -- so every such place has to
/// be moved in step with the list itself.
fn renumbered(slots: &[Slot]) -> Vec<Option<usize>> {
    let mut places = Vec::with_capacity(slots.len());
    let mut next = 0usize;
    for slot in slots {
        if slot.gone {
            places.push(None);
        } else {
            places.push(Some(next));
            next = next.saturating_add(1);
        }
    }
    places
}

/// Do what a bar asked of a window, and say whether the layout changed.
///
/// Each is the dispatcher a keybind would run, aimed at the window the bar
/// named rather than the focused one: `activate` is `focuswindow`, `close`
/// is `killactive`, and the two state requests are `fullscreen` after the
/// window has been focused, since that is what this layout's dispatcher
/// acts on. `minimized` has nowhere to go -- nothing here is minimised --
/// so it is answered by leaving the window where it is, which is what a
/// compositor without a minimised state does.
fn for_the_bar(
    window: WindowId,
    what: ForeignRequest,
    state: &mut State,
    slots: &mut [Slot],
    sources: &BTreeMap<WindowId, Source>,
) -> bool {
    match what {
        ForeignRequest::Activate => state.focus_window(window).is_ok(),
        ForeignRequest::Close => {
            close(window, slots, sources);
            false
        }
        ForeignRequest::Fullscreen(on) => {
            let is = state
                .workspace_of(window)
                .and_then(|workspace| state.fullscreen(workspace))
                .is_some_and(|(full, _)| full == window);
            if is == on {
                return false;
            }
            if state.focus_window(window).is_err() {
                return false;
            }
            state
                .dispatch_str("fullscreen", "0")
                .is_ok_and(|made| !made.is_empty())
        }
        // Maximised and minimised are states this compositor does not have,
        // and a bar is told so: the window is never reported in either, so
        // a request to leave one is already true and a request to enter one
        // is refused by doing nothing rather than by doing the wrong thing.
        ForeignRequest::Maximized(_) | ForeignRequest::Minimized(_) => false,
    }
}

/// Ask the window's client to close it.
fn close(window: WindowId, slots: &mut [Slot], sources: &BTreeMap<WindowId, Source>) {
    let Some(source) = sources.get(&window) else {
        return;
    };
    let Some(slot) = slots.get_mut(source.client) else {
        return;
    };
    let toplevel = slot
        .client
        .toplevels()
        .find(|(_, top)| top.surface == source.surface)
        .map(|(id, _)| id);
    if let Some(toplevel) = toplevel {
        slot.client.close_toplevel(toplevel);
    }
}

/// What a window is called, for a diagnostic.
fn named(window: WindowId, slots: &[Slot], sources: &BTreeMap<WindowId, Source>) -> String {
    let Some(source) = sources.get(&window) else {
        return "a window that is not there".to_owned();
    };
    let Some(slot) = slots.get(source.client) else {
        return format!(
            "a window on connection {}, which is not there",
            source.client
        );
    };
    let title = slot
        .client()
        .toplevels()
        .find(|(_, top)| top.surface == source.surface)
        .map(|(_, top)| top.title.clone())
        .unwrap_or_default();
    format!("{title:?} on connection {}", source.client)
}

/// Place every layer surface, tell each one its size, and reserve what they
/// asked for.
///
/// The surfaces are taken in the order their clients made them, across every
/// connection, which is the order wlroots places them in: a bar that started
/// first gets the edge. The exclusive zones become the monitor's reserved
/// strips, so the windows tile in what is left.
fn place_layers(
    slots: &mut [Slot],
    state: &mut State,
    screens: &[Screen],
    rules: &[compositor_config::LayerRule],
    added: &[compositor_config::AddedReserved],
) -> Vec<crate::frame::Placed> {
    // Everything that has been given a role, with what it asked for and
    // which screen it asked for it on.
    let mut asked: Vec<(
        usize,
        ObjectId,
        ObjectId,
        bool,
        usize,
        crate::frame::LayerRules,
        compositor_layout::layers::Request,
    )> = Vec::new();
    // A layer surface that named no output goes on the focused monitor,
    // which is what "you choose" means and what wlroots' own helper does.
    let chosen = screens
        .iter()
        .position(|screen| Some(screen.monitor) == state.focused_monitor())
        .unwrap_or(0);
    for (index, slot) in slots.iter().enumerate() {
        for id in &slot.layers {
            let Some(layer) = slot.client.layer_surface(*id) else {
                continue;
            };
            let anchors = compositor_server::Anchors::from_raw(layer.anchor);
            let on = layer
                .output
                .and_then(|output| slot.client.output_of(output))
                .filter(|which| *which < screens.len())
                .unwrap_or(chosen);
            let named = compositor_config::Layered::of(rules, &layer.namespace);
            asked.push((
                index,
                *id,
                layer.surface,
                layer.layer.above_windows(),
                on,
                crate::frame::LayerRules {
                    blur: named.blur,
                    xray: named.xray,
                    dim_around: named.dim_around,
                    above_lock: named.above_lock,
                    no_screen_share: named.no_screen_share,
                    order: named.order,
                },
                compositor_layout::layers::Request {
                    top: anchors.top,
                    bottom: anchors.bottom,
                    left: anchors.left,
                    right: anchors.right,
                    size: layer.size,
                    margin: (
                        layer.margin.top,
                        layer.margin.right,
                        layer.margin.bottom,
                        layer.margin.left,
                    ),
                    exclusive_zone: layer.exclusive_zone,
                },
            ));
        }
    }

    // A monitor at a time: an exclusive zone reserves a strip of the screen
    // the surface is on and of no other, so a bar on one monitor does not
    // move the windows on the next.
    let mut placed: Vec<(
        usize,
        ObjectId,
        ObjectId,
        bool,
        crate::frame::LayerRules,
        Rect,
    )> = Vec::new();
    for (which, screen) in screens.iter().enumerate() {
        let mut here: Vec<&(
            usize,
            ObjectId,
            ObjectId,
            bool,
            usize,
            crate::frame::LayerRules,
            compositor_layout::layers::Request,
        )> = asked.iter().filter(|(.., on, _, _)| *on == which).collect();
        // `layerrule = order <n>`: a higher number goes nearer the top of
        // its own layer. The sort is stable, so surfaces with the same
        // order keep the sequence their clients made them in -- which is
        // wlroots' rule and what gives a bar that started first the edge.
        here.sort_by_key(|(.., rules, _)| rules.order);
        let requests: Vec<compositor_layout::layers::Request> =
            here.iter().map(|(.., request)| *request).collect();
        let (placements, zones) = compositor_layout::layers::place(screen.rect, &requests);
        // What `addreserved` keeps free is added to the zones, as Hyprland
        // adds its `m_reservedArea` to what the layers ask for.
        let extra =
            compositor_config::AddedReserved::for_monitor(added, &screen.name, &screen.description);
        let reserved = compositor_config::Gaps {
            top: zones.top + extra.top,
            right: zones.right + extra.right,
            bottom: zones.bottom + extra.bottom,
            left: zones.left + extra.left,
        };
        let _ = state.set_reserved(screen.monitor, reserved);
        for ((index, id, surface, above, _, rules, _), placement) in
            here.into_iter().zip(placements)
        {
            placed.push((*index, *id, *surface, *above, *rules, placement.rect));
        }
    }

    let mut drawn = Vec::with_capacity(placed.len());
    for slot in slots.iter_mut() {
        slot.layer_rects.clear();
    }
    for (index, id, surface, above, rules, rect) in placed {
        if let Some(slot) = slots.get_mut(index) {
            let _ = slot.layer_rects.insert(id, rect);
            let width = u32::try_from(rect.width).unwrap_or(0);
            let height = u32::try_from(rect.height).unwrap_or(0);
            let already = slot
                .client
                .layer_surface(id)
                .is_some_and(|layer| layer.sent_configure && layer.configured == (width, height));
            if !already {
                slot.client.configure_layer(id, width, height);
            }
        }
        drawn.push(crate::frame::Placed {
            client: index,
            surface,
            rect,
            above,
            rules,
        });
    }
    drawn
}

/// Tell every window the size the layout gives it now.
///
/// A configure a client has already been given and has acked is not sent
/// again: an unchanged size is a round trip a client does not need, and
/// Hyprland does not send one either.
fn reconfigure(slots: &mut [Slot], state: &State) {
    let mut sizes: BTreeMap<WindowId, (i32, i32, bool)> = BTreeMap::new();
    for output in state.layout() {
        for placed in &output.windows {
            let _ = sizes.insert(
                placed.window,
                (
                    i32::try_from(placed.rect.width).unwrap_or(0),
                    i32::try_from(placed.rect.height).unwrap_or(0),
                    placed.focused,
                ),
            );
        }
    }
    for slot in slots.iter_mut() {
        let windows = slot.windows.clone();
        for (toplevel, window) in windows {
            let Some((width, height, focused)) = sizes.get(&window).copied() else {
                continue;
            };
            // The states as well as the size: a window that has just been
            // focused is the same size and a different state, and a client
            // that is not told has a title bar that never lights up.
            let wanted = states(focused);
            let already = slot
                .client
                .toplevel(toplevel)
                .is_some_and(|top| top.configured == (width, height) && top.states == wanted);
            if already {
                continue;
            }
            slot.client
                .configure_toplevel(toplevel, width, height, &wanted);
        }
    }
}

/// The `xdg_toplevel` states a tiled window is in.
fn states(focused: bool) -> Vec<u32> {
    let mut states = vec![
        xdg_shell::xdg_toplevel::state::TILED_LEFT,
        xdg_shell::xdg_toplevel::state::TILED_RIGHT,
        xdg_shell::xdg_toplevel::state::TILED_TOP,
        xdg_shell::xdg_toplevel::state::TILED_BOTTOM,
    ];
    if focused {
        states.push(xdg_shell::xdg_toplevel::state::ACTIVATED);
    }
    states
}

/// Tell a window the size the layout gave it.
fn configure(client: &mut Client, state: &State, toplevel: ObjectId, window: WindowId) {
    let mut width = 0;
    let mut height = 0;
    for output in state.layout() {
        for placed in &output.windows {
            if placed.window == window {
                width = i32::try_from(placed.rect.width).unwrap_or(0);
                height = i32::try_from(placed.rect.height).unwrap_or(0);
            }
        }
    }
    let focused = state.focused_window() == Some(window);
    client.configure_toplevel(toplevel, width, height, &states(focused));
}

/// The globals the compositor offers.
fn globals(outputs: usize) -> Globals {
    let mut globals = Globals::new();
    for (interface, version, role) in [
        // Each at the version its own interface offers. A compositor that
        // advertises less is one a toolkit refuses to start against:
        // `hyprtoolkit` binds `wl_seat` at 9 and gives up with "Missing
        // protocols" when it is offered 7, which is how this was found.
        (&core::WL_COMPOSITOR, core::WL_COMPOSITOR.version, Role::Compositor),
        (
            &core::WL_SUBCOMPOSITOR,
            core::WL_SUBCOMPOSITOR.version,
            Role::Subcompositor,
        ),
        (&core::WL_SHM, core::WL_SHM.version, Role::Shm),
        (&core::WL_SEAT, core::WL_SEAT.version, Role::Seat),
        (
            &core::WL_DATA_DEVICE_MANAGER,
            core::WL_DATA_DEVICE_MANAGER.version,
            Role::DataDeviceManager,
        ),
        (&xdg_shell::XDG_WM_BASE, 6, Role::XdgWmBase),
        // Who draws the title bar, which for a tiling compositor is always
        // the compositor: a toolkit that finds no manager here assumes it is
        // its own job and draws one inside the rectangle the tiling gave it.
        (
            &compositor_protocol::xdg_decoration::ZXDG_DECORATION_MANAGER_V1,
            1,
            Role::DecorationManager,
        ),
        // The bars, wallpapers, launchers and notification daemons: every
        // one of them is a `zwlr_layer_shell_v1` client, and a compositor
        // that does not offer it is one a Hyprland setup does not start on.
        (
            &compositor_protocol::layer_shell::ZWLR_LAYER_SHELL_V1,
            5,
            Role::LayerShell,
        ),
        // The other half of a bar: layer-shell puts it on the screen and
        // this tells it which windows to draw. Waybar, eww and every
        // taskbar read it, and a compositor that does not offer it is one
        // whose bar shows a clock and nothing else.
        (
            &compositor_protocol::foreign_toplevel::ZWLR_FOREIGN_TOPLEVEL_MANAGER_V1,
            3,
            Role::ForeignToplevelManager,
        ),
        // A screenshot: `grim`, `hyprshot` and every screen recorder on
        // wlroots go through this and nothing else.
        (
            &compositor_protocol::screencopy::ZWLR_SCREENCOPY_MANAGER_V1,
            3,
            Role::ScreencopyManager,
        ),
        // The screen lock, which `hyprlock` and `swaylock` speak.
        (
            &compositor_protocol::session_lock::EXT_SESSION_LOCK_MANAGER_V1,
            1,
            Role::SessionLockManager,
        ),
        // The six a real toolkit asks for and warns about when it is not
        // offered. `foot` names every one of them on a compositor that has
        // none, which is how this list was written.
        (
            &compositor_protocol::cursor_shape::WP_CURSOR_SHAPE_MANAGER_V1,
            1,
            Role::CursorShapeManager,
        ),
        (
            &compositor_protocol::primary_selection::ZWP_PRIMARY_SELECTION_DEVICE_MANAGER_V1,
            1,
            Role::PrimaryManager,
        ),
        (
            &compositor_protocol::xdg_activation::XDG_ACTIVATION_V1,
            1,
            Role::Activation,
        ),
        (
            &compositor_protocol::viewporter::WP_VIEWPORTER,
            1,
            Role::Viewporter,
        ),
        (
            &compositor_protocol::fractional_scale::WP_FRACTIONAL_SCALE_MANAGER_V1,
            1,
            Role::FractionalScaleManager,
        ),
        (
            &compositor_protocol::toplevel_icon::XDG_TOPLEVEL_ICON_MANAGER_V1,
            1,
            Role::IconManager,
        ),
        // A bar reads a screen's logical size and name from here rather
        // than from `wl_output.mode`, which is in the screen's own pixels;
        // on a scaled monitor the two differ, and it is the logical one
        // that every window's rectangle is in.
        (
            &compositor_protocol::xdg_output::ZXDG_OUTPUT_MANAGER_V1,
            3,
            Role::XdgOutputManager,
        ),
        // When a frame actually reached the screen, which a frame callback
        // does not say: it fires when the compositor *began* one.
        (
            &compositor_protocol::presentation::WP_PRESENTATION,
            2,
            Role::Presentation,
        ),
        // The two halves of "is anyone there": a locker or a power daemon
        // waits on the first, a video player holds it off with the second.
        (
            &compositor_protocol::idle_notify::EXT_IDLE_NOTIFIER_V1,
            2,
            Role::IdleNotifier,
        ),
        (
            &compositor_protocol::idle_inhibit::ZWP_IDLE_INHIBIT_MANAGER_V1,
            1,
            Role::IdleInhibitManager,
        ),
        // A buffer that is one colour, which is how a client puts a solid
        // rectangle on the screen without sharing a megabyte of the same
        // four bytes.
        (
            &compositor_protocol::single_pixel::WP_SINGLE_PIXEL_BUFFER_MANAGER_V1,
            1,
            Role::SinglePixelManager,
        ),
        // What a surface is showing, and how much of it shows.
        (
            &compositor_protocol::content_type::WP_CONTENT_TYPE_MANAGER_V1,
            1,
            Role::ContentTypeManager,
        ),
        (
            &compositor_protocol::alpha_modifier::WP_ALPHA_MODIFIER_V1,
            1,
            Role::AlphaModifier,
        ),
        // A dialog saying it is modal, which floats it; the terminal bell;
        // and the name a window keeps across restarts.
        (
            &compositor_protocol::xdg_dialog::XDG_WM_DIALOG_V1,
            1,
            Role::DialogManager,
        ),
        (
            &compositor_protocol::system_bell::XDG_SYSTEM_BELL_V1,
            1,
            Role::SystemBell,
        ),
        (
            &compositor_protocol::toplevel_tag::XDG_TOPLEVEL_TAG_MANAGER_V1,
            1,
            Role::ToplevelTagManager,
        ),
        // KDE's own `xdg-decoration`, which a good deal of software still
        // asks first and warns about when it is not there.
        (
            &compositor_protocol::kde_decoration::ORG_KDE_KWIN_SERVER_DECORATION_MANAGER,
            1,
            Role::KdeDecorationManager,
        ),
        // How far the pointer moved rather than where it is, and keeping
        // it inside a window: the pair a game, a 3D modeller and a
        // remote-desktop viewer all need.
        (
            &compositor_protocol::relative_pointer::ZWP_RELATIVE_POINTER_MANAGER_V1,
            1,
            Role::RelativePointerManager,
        ),
        (
            &compositor_protocol::pointer_constraints::ZWP_POINTER_CONSTRAINTS_V1,
            1,
            Role::PointerConstraints,
        ),
        // A touchpad's gestures, which this compositor never reports: it
        // reads evdev and not libinput, and the recogniser is libinput's.
        // Offering the global is what stops a toolkit warning on start.
        (
            &compositor_protocol::pointer_gestures::ZWP_POINTER_GESTURES_V1,
            3,
            Role::PointerGestures,
        ),
        // A virtual machine or a nested compositor asking for `SUPER`
        // instead of the compositor eating it.
        (
            &compositor_protocol::shortcuts_inhibit::ZWP_KEYBOARD_SHORTCUTS_INHIBIT_MANAGER_V1,
            1,
            Role::ShortcutsInhibitManager,
        ),
        // A client acting as a device: `wtype`, `ydotool`, an on-screen
        // keyboard, a remote-desktop viewer.
        (
            &compositor_protocol::virtual_keyboard::ZWP_VIRTUAL_KEYBOARD_MANAGER_V1,
            1,
            Role::VirtualKeyboardManager,
        ),
        (
            &compositor_protocol::virtual_pointer::ZWLR_VIRTUAL_POINTER_MANAGER_V1,
            2,
            Role::VirtualPointerManager,
        ),
        // The window list as the newer specification has it, which is the
        // one a taskbar written this year binds. The wlroots list stays:
        // one written three years ago binds that.
        (
            &compositor_protocol::foreign_list::EXT_FOREIGN_TOPLEVEL_LIST_V1,
            1,
            Role::ForeignList,
        ),
        // A night-light, and a program that turns a screen off.
        (
            &compositor_protocol::gamma_control::ZWLR_GAMMA_CONTROL_MANAGER_V1,
            1,
            Role::GammaControlManager,
        ),
        (
            &compositor_protocol::output_power::ZWLR_OUTPUT_POWER_MANAGER_V1,
            1,
            Role::OutputPowerManager,
        ),
        // The clipboard as a *manager* sees it: `cliphist` and `wl-paste
        // --watch` have no window at all and are told anyway. Twice, because
        // the protocol is wlroots' and the standardised one and programs
        // bind whichever they were written against.
        (
            &compositor_protocol::data_control::ZWLR_DATA_CONTROL_MANAGER_V1,
            2,
            Role::DataControlManager(compositor_server::Flavour::Wlr),
        ),
        (
            &compositor_protocol::ext_data_control::EXT_DATA_CONTROL_MANAGER_V1,
            1,
            Role::DataControlManager(compositor_server::Flavour::Ext),
        ),
        // `kanshi` and `wlr-randr` arranging the screens, and the workspace
        // numbers a bar draws.
        (
            &compositor_protocol::output_management::ZWLR_OUTPUT_MANAGER_V1,
            4,
            Role::OutputManager,
        ),
        (
            &compositor_protocol::ext_workspace::EXT_WORKSPACE_MANAGER_V1,
            1,
            Role::WorkspaceManager,
        ),
        // Hyprland's own six. A shortcut a program registers rather than
        // a keybind, a launcher holding the focus, a program told when the
        // screen locks, the handle that joins a `wl_surface` to the window
        // every other protocol calls by address, a surface's own opacity,
        // and a screenshot of one window rather than a screen.
        (
            &compositor_protocol::global_shortcuts::HYPRLAND_GLOBAL_SHORTCUTS_MANAGER_V1,
            1,
            Role::GlobalShortcuts,
        ),
        (
            &compositor_protocol::focus_grab::HYPRLAND_FOCUS_GRAB_MANAGER_V1,
            1,
            Role::FocusGrabManager,
        ),
        (
            &compositor_protocol::lock_notify::HYPRLAND_LOCK_NOTIFIER_V1,
            1,
            Role::LockNotifier,
        ),
        (
            &compositor_protocol::toplevel_mapping::HYPRLAND_TOPLEVEL_MAPPING_MANAGER_V1,
            1,
            Role::ToplevelMapping,
        ),
        (
            &compositor_protocol::hyprland_surface::HYPRLAND_SURFACE_MANAGER_V1,
            2,
            Role::HyprlandSurfaceManager,
        ),
        (
            &compositor_protocol::toplevel_export::HYPRLAND_TOPLEVEL_EXPORT_MANAGER_V1,
            2,
            Role::ToplevelExportManager,
        ),
        // Where the pointer goes, what is behind a surface, and how a
        // client would like its frames scheduled.
        (
            &compositor_protocol::pointer_warp::WP_POINTER_WARP_V1,
            1,
            Role::PointerWarp,
        ),
        (
            &compositor_protocol::background_effect::EXT_BACKGROUND_EFFECT_MANAGER_V1,
            1,
            Role::BackgroundEffectManager,
        ),
        (
            &compositor_protocol::tearing_control::WP_TEARING_CONTROL_MANAGER_V1,
            1,
            Role::TearingManager,
        ),
        (
            &compositor_protocol::fifo::WP_FIFO_MANAGER_V1,
            1,
            Role::FifoManager,
        ),
        (
            &compositor_protocol::commit_timing::WP_COMMIT_TIMING_MANAGER_V1,
            1,
            Role::CommitTimingManager,
        ),
        // A sandbox asking for a socket of its own, and a launcher asking
        // for a key by keysym.
        (
            &compositor_protocol::security_context::WP_SECURITY_CONTEXT_MANAGER_V1,
            1,
            Role::SecurityContextManager,
        ),
        (
            &compositor_protocol::hotkey::VICINAE_HOTKEY_MANAGER_V1,
            1,
            Role::HotkeyManager,
        ),
        // Screenshots as the `ext` namespace has them: a source, and a
        // session that copies frames out of it. What a recorder or a
        // screen-sharing portal written this year binds.
        (
            &compositor_protocol::capture_source::EXT_OUTPUT_IMAGE_CAPTURE_SOURCE_MANAGER_V1,
            1,
            Role::OutputCaptureSourceManager,
        ),
        (
            &compositor_protocol::capture_source::EXT_FOREIGN_TOPLEVEL_IMAGE_CAPTURE_SOURCE_MANAGER_V1,
            1,
            Role::ToplevelCaptureSourceManager,
        ),
        (
            &compositor_protocol::image_copy::EXT_IMAGE_COPY_CAPTURE_MANAGER_V1,
            1,
            Role::CaptureManager,
        ),
        // Typing through an input method: the application's half and the
        // method's own. Offering both is what lets an on-screen keyboard or
        // an IME run at all; with neither running, a text field is told
        // nothing, which is a session with no IME.
        (
            &compositor_protocol::text_input::ZWP_TEXT_INPUT_MANAGER_V3,
            1,
            Role::TextInputManager,
        ),
        (
            &compositor_protocol::input_method::ZWP_INPUT_METHOD_MANAGER_V2,
            1,
            Role::InputMethodManager,
        ),
    ] {
        let _ = globals.add(interface, version, role);
    }
    // One `wl_output` a monitor, in the order the screens came: that is how
    // a client is told there are two screens, and which is which.
    for _ in 0..outputs.max(1) {
        let _ = globals.add(&core::WL_OUTPUT, 4, Role::Output);
    }
    globals
}

/// The configuration's `env =` pairs, which every program the compositor
/// starts is given.
///
/// Hyprland sets them in its own environment as it reads the file, so that
/// everything it starts inherits them. This keeps them beside the process
/// instead and hands them to each child: changing a running program's
/// environment is not safe once it has threads, and this one has a
/// renderer's.
static CHILD_ENV: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// The programs [`not_started`] has said are missing.
static MISSING: std::sync::LazyLock<std::sync::Mutex<crate::command::Missing>> =
    std::sync::LazyLock::new(std::sync::Mutex::default);

/// The line to log for `command` not starting, or `None` when it names a
/// program already said to be missing ([`crate::command::Missing`]).
pub(crate) fn not_started(command: &str, why: &crate::command::NotStarted) -> Option<String> {
    match MISSING.lock() {
        Ok(mut missing) => missing.line(command, why),
        Err(_) => Some(format!("hyprix: {command} did not start: {why}")),
    }
}

/// Read the configuration, or take Hyprland's defaults, and remember its
/// `env =` pairs for [`start`].
fn read_config(options: &Options) -> Result<Config, String> {
    let Some(path) = options.config.as_ref() else {
        return Ok(Config::default());
    };
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    let name = path.to_string_lossy().into_owned();
    let config = compositor_config::parse(&name, &text, &mut NoSources).config;
    if let Ok(mut env) = CHILD_ENV.lock() {
        env.clone_from(&config.env);
    }
    Ok(config)
}

/// Start a program with `WAYLAND_DISPLAY` pointing at this compositor.
///
/// # Errors
///
/// Why it did not start, which the caller logs ([`not_started`]): a program
/// that will not start is the person's to fix, not a reason to have no
/// compositor.
pub(crate) fn start(
    command: &str,
    socket: &std::path::Path,
    instance: Option<&str>,
) -> Result<u32, crate::command::NotStarted> {
    let words = crate::command::words(command)?;
    let (assigned, words) = crate::command::assignments(&words);
    let (program, arguments) = words
        .split_first()
        .ok_or_else(|| "only assignments, and no program".to_owned())?;
    let mut child = std::process::Command::new(program);
    let _ = child.args(arguments).env("WAYLAND_DISPLAY", socket);
    // The environment Hyprland gives everything it starts
    // (`CCompositor::initServer`). Without it a program started by
    // `exec-once` cannot find the compositor to ask: `hyprctl` looks for
    // `$HYPRLAND_INSTANCE_SIGNATURE` and prints "Couldn't connect to socket"
    // without one, which is how this was found -- a wallpaper daemon on this
    // machine could not read `hyprctl monitors`.
    if let Some(instance) = instance {
        let _ = child.env("HYPRLAND_INSTANCE_SIGNATURE", instance);
    }
    // Only when nothing set it, as Hyprland does: a session started by a
    // display manager has its own, and a toolkit reads this to decide which
    // portal to talk to.
    if std::env::var_os("XDG_CURRENT_DESKTOP").is_none() {
        let _ = child.env("XDG_CURRENT_DESKTOP", "Hyprland");
    }
    if std::env::var_os("XDG_SESSION_TYPE").is_none() {
        let _ = child.env("XDG_SESSION_TYPE", "wayland");
    }
    // A locale, which a distribution's login sets and nothing on Ferrix did.
    // Without one, `setlocale(LC_ALL, "")` names `C.UTF-8;C;C;C;C;C` -- UTF-8
    // for characters, C for the rest, as musl's does -- and btop refuses to
    // start on a name that is not plainly UTF-8. UTF-8 is what the terminal
    // speaks and what every program here writes, and C.UTF-8 is the locale
    // ferrousli and musl build in.
    if std::env::var_os("LANG").is_none() && std::env::var_os("LC_ALL").is_none() {
        let _ = child.env("LANG", "C.UTF-8");
    }
    // The configuration's own, last, so that an `env =` line can set any of
    // the above too, as it can in Hyprland.
    if let Ok(env) = CHILD_ENV.lock() {
        let _ = child.envs(env.iter().map(|(name, value)| (name, value)));
    }
    // The command's own `NAME=value` words, after those: in `sh` they are
    // this one program's, over whatever it would otherwise inherit.
    let _ = child.envs(assigned);
    let pid = child.spawn().map(|child| child.id()).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            crate::command::NotStarted::Missing {
                program: program.clone(),
                there: program.contains('/') && std::path::Path::new(program).exists(),
            }
        } else {
            crate::command::NotStarted::Other(error.to_string())
        }
    })?;
    // Not waited for here: the loop reaps it once it ends
    // (`crate::children`), or it would stay a zombie.
    crate::children::started(pid);
    crate::scope::group(program, pid);
    Ok(pid)
}
