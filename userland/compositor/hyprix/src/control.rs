//! The two `hyprctl` sockets: the one that answers, and the one that tells.
//!
//! Hyprland puts both in an instance directory under
//! `$XDG_RUNTIME_DIR/hypr/`, and `hyprctl` and every bar find them there.
//!
//! * **`.socket.sock`**, [`Control`]: a connection sends one line, is
//!   answered, and is closed.
//! * **`.socket2.sock`**, [`Events`]: a connection is kept, and every state
//!   change is a line written to it. It is never read from.
//!
//! What a request means, what an answer says and what an event's line is are
//! `userland/compositor/ipc`'s; this is the sockets under them.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use compositor_ipc::{Monitor, Request, Snapshot, Window, Workspace};
use compositor_layout::{State, WindowId};
use compositor_server::{Client, ForeignToplevel};
use compositor_wire::ObjectId;

use crate::frame::Source;

/// The instance directory and the socket in it.
#[derive(Debug)]
pub struct Control {
    listener: UnixListener,
    path: PathBuf,
    directory: PathBuf,
}

impl Control {
    /// Bind the request socket for `instance`.
    ///
    /// Hyprland's own layout: `$XDG_RUNTIME_DIR/hypr/<instance>/.socket.sock`,
    /// with the instance in `HYPRLAND_INSTANCE_SIGNATURE`. A program looks
    /// there and nowhere else, so a compositor that puts it somewhere else is
    /// one `hyprctl` cannot find.
    ///
    /// An `instance` with a `/` in it is the directory itself, as a
    /// `--display` with one is the socket itself. Nothing real passes one:
    /// `HYPRLAND_INSTANCE_SIGNATURE` is a name. It is how a test gives two
    /// compositors in one process two instance directories without setting
    /// an environment variable that the whole process shares.
    ///
    /// # Errors
    ///
    /// Whatever the bind said.
    pub fn bind(runtime: &Path, instance: &str) -> std::io::Result<Self> {
        let directory = if instance.contains('/') {
            PathBuf::from(instance)
        } else {
            runtime.join("hypr").join(instance)
        };
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(compositor_ipc::REQUEST_SOCKET);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path,
            directory,
        })
    }

    /// Where the socket is, for `HYPRLAND_INSTANCE_SIGNATURE` and for a test.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The instance directory, which the event socket goes in beside this
    /// one.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The request listener descriptor, for the compositor's event wait.
    #[must_use]
    pub fn raw_fd(&self) -> i32 {
        self.listener.as_raw_fd()
    }

    /// Take one waiting connection, if there is one.
    ///
    /// A request is one line and an answer is one write, so a connection is
    /// served and closed in one go rather than kept: `hyprctl` opens one per
    /// request and so does every bar.
    pub fn accept(&self) -> Option<UnixStream> {
        match self.listener.accept() {
            Ok((stream, _)) => Some(stream),
            Err(_) => None,
        }
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        // A socket file left behind is a `hyprctl` that connects to nothing.
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

/// A request connection accepted and not answered yet.
///
/// A client connects and then writes, and the two are not one call, so the
/// request is read as it arrives, with the connection polled beside every
/// other descriptor, while the compositor goes on drawing. Reading it in one
/// blocking go lost it whenever the client was slower than the wait: first
/// as `Broken pipe` with no wait at all, then, with a 250 ms one, as an
/// empty request (`unknown request`) or a `Broken pipe` again whenever the
/// guest did not run `hyprctl` for a quarter of a second between its
/// `connect` and its `write` (docs/BACKLOG.md, P1 flakes, 2026-09-28).
#[derive(Debug)]
pub struct Pending {
    stream: UnixStream,
    line: String,
    whole: bool,
    since: Instant,
}

/// How long a connection may take to send its request. What it sent by then
/// is answered, and a connection that sent nothing is closed.
const PATIENCE: Duration = Duration::from_secs(5);

impl Pending {
    /// A connection just accepted.
    ///
    /// # Errors
    ///
    /// Making it non-blocking failing.
    pub fn new(stream: UnixStream) -> std::io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            line: String::new(),
            whole: false,
            since: Instant::now(),
        })
    }

    /// Its descriptor, for the compositor's event wait.
    #[must_use]
    pub fn raw_fd(&self) -> i32 {
        self.stream.as_raw_fd()
    }

    /// When it stops being waited for.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.since + PATIENCE
    }

    /// Read what has arrived. Gives whether the request is ready to answer:
    /// its first line has come, the client shut its end (which is what
    /// `hyprctl` does after its line), or the time is up with something
    /// said. `None` is a connection to close unanswered: one that failed,
    /// or said nothing in time.
    pub fn read(&mut self, now: Instant) -> Option<bool> {
        let mut buffer = [0u8; 4096];
        while !self.whole {
            match self.stream.read(&mut buffer) {
                Ok(0) => self.whole = true,
                Ok(read) => {
                    self.line
                        .push_str(&String::from_utf8_lossy(buffer.get(..read).unwrap_or(&[])));
                    self.whole = self.line.contains('\n');
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
        if self.whole {
            return Some(true);
        }
        if now >= self.deadline() {
            return (!self.line.is_empty()).then_some(true);
        }
        Some(false)
    }
}

/// Answer a request `Pending::read` says is ready, and close.
///
/// Gives back the requests that change something, which the caller runs: this
/// function holds no compositor.
///
/// # Errors
///
/// Writing the answer failing.
pub fn serve(
    pending: Pending,
    snapshot: &Snapshot,
    plugins: &mut crate::plugins::Plugins,
) -> std::io::Result<Vec<compositor_ipc::Reply>> {
    let Pending {
        mut stream, line, ..
    } = pending;
    // Back to blocking for the answer; a plugin's reads are made
    // non-blocking again when it is taken.
    stream.set_nonblocking(false)?;

    // A connection that opens with `[[PLUGIN]]` is a plugin, and is kept
    // rather than answered and closed.
    if line.trim_start().starts_with(crate::plugins::MARKER) {
        let kept = stream.try_clone()?;
        if plugins.take(kept, &line) {
            return Ok(Vec::new());
        }
    }

    let mut todo = Vec::new();
    let mut answer = String::new();
    for request in Request::parse_batch(&line) {
        match compositor_ipc::answer(&request, snapshot, compositor_ipc::Version::default()) {
            compositor_ipc::Reply::Text(text) => answer.push_str(&text),
            other => {
                // Hyprland answers `ok` for a request that did something and
                // leaves the doing to the compositor; so does this, unless
                // the reply brought an answer of its own -- which
                // `switchxkblayout all` does when it moved some keyboards
                // and had a reason for the others.
                answer.push_str(other.said().unwrap_or("ok\n"));
                todo.push(other);
            }
        }
    }
    stream.write_all(answer.as_bytes())?;
    stream.flush()?;
    Ok(todo)
}

/// Describe the compositor for an answer.
///
/// A snapshot rather than a borrow: an answer is written whole, and a request
/// that arrived halfway through a frame should describe the frame before it
/// rather than half of the next.
pub fn snapshot(
    state: &State,
    clients: &[crate::state::Slot],
    sources: &BTreeMap<WindowId, Source>,
    reported: &Reported<'_>,
) -> Snapshot {
    let mut snapshot = describe_all(state, clients, sources, reported.styles, reported.submap);
    snapshot.plugins = reported.plugins.listed();
    snapshot.binds = reported.binds.iter().map(described_bind).collect();
    snapshot.devices = described_devices(reported.devices, reported.config, reported.keyboard);
    snapshot.layers = described_layers(state, clients, reported.layers);
    snapshot.cursor = reported.cursor;
    snapshot.locked = reported.locked;
    snapshot.options = described_options(reported.config);
    snapshot.animations = reported.animations.to_vec();
    snapshot.beziers = reported.beziers.to_vec();
    snapshot.errors = reported.errors.to_vec();
    snapshot.workspace_rules = reported.workspace_rules.to_vec();
    snapshot.log = reported.log.to_vec();
    snapshot.shortcuts = clients
        .iter()
        .flat_map(|slot| slot.client().shortcut_names())
        .map(|name| compositor_ipc::Shortcut {
            name,
            // The protocol carries a description with each shortcut; the
            // compositor keeps the name, which is what `dispatch global`
            // takes and what a script reads this command for.
            description: String::new(),
        })
        .collect();
    snapshot.system = compositor_ipc::System {
        os: "Ferrix".to_owned(),
        kernel: env!("CARGO_PKG_VERSION").to_owned(),
        counts: (
            snapshot.monitors.len(),
            snapshot.windows.len(),
            clients.len(),
        ),
        uptime: reported.uptime,
    };
    snapshot
}

/// What `hyprctl` answers about that the layout does not know.
///
/// The seat, the configuration and the input devices, gathered into one
/// value because they are read together and only by [`snapshot`]: a bar
/// reading `zwlr_foreign_toplevel_management_v1` wants none of them, and
/// `describe_all` is what answers that.
#[derive(Clone, Copy, Debug)]
pub struct Reported<'a> {
    /// The submap in force, empty for the global map.
    pub submap: &'a str,
    /// Every bind the configuration holds, in the order it wrote them.
    pub binds: &'a [compositor_config::Bind],
    /// The input devices the seat reads.
    pub devices: &'a crate::devices::Devices,
    /// The seat's keyboard state: which layout group it is in, how many the
    /// keymap has, and which locks are on. `hyprctl devices` prints all
    /// three, and `switchxkblayout` works out the group it asks for from
    /// the first two.
    pub keyboard: &'a compositor_xkb::Keyboard,
    /// Where each layer surface was placed this pass.
    pub layers: &'a [crate::frame::Placed],
    /// The plugins that are loaded.
    pub plugins: &'a crate::plugins::Plugins,
    /// Where the pointer is.
    pub cursor: (i32, i32),
    /// Whether a session lock is up, which this compositor has no protocol
    /// for and so never is.
    pub locked: bool,
    /// The configuration, for `hyprctl getoption` and `descriptions`.
    pub config: &'a compositor_config::Config,
    /// Every animation node with what it ended up with: `hyprctl
    /// animations`.
    pub animations: &'a [compositor_ipc::Animation],
    /// Every bezier, for the same.
    pub beziers: &'a [compositor_ipc::Bezier],
    /// What could not be read in the configuration: `hyprctl
    /// configerrors`.
    pub errors: &'a [String],
    /// Every `workspace =` line, read: `hyprctl workspacerules`.
    pub workspace_rules: &'a [compositor_config::WorkspaceRule],
    /// What a rule gave each window to be drawn with, which `hyprctl
    /// getprop` reads.
    pub styles: &'a BTreeMap<WindowId, compositor_render::WindowStyle>,
    /// The last lines the compositor said: `hyprctl rollinglog`.
    pub log: &'a [String],
    /// How long it has been running, in seconds.
    pub uptime: u64,
}

/// Every option the compositor has, with what it holds now.
///
/// The type word is the key Hyprland puts the value under in JSON, and a
/// script reads exactly that key: `int` for an integer, a boolean or a
/// colour, `float` for a float, `str` for anything written out.
fn described_options(config: &compositor_config::Config) -> Vec<compositor_ipc::Opt> {
    config
        .options()
        .map(|(name, value)| compositor_ipc::Opt {
            name: name.to_owned(),
            value: value.to_string(),
            kind: match value {
                compositor_config::OptionValue::Int(_) => "int",
                compositor_config::OptionValue::Float(_) => "float",
                compositor_config::OptionValue::Str(_) => "str",
                compositor_config::OptionValue::Gradient(_)
                | compositor_config::OptionValue::Gaps(_) => "custom",
            },
            // Whether the configuration said so, rather than it being
            // the table's own default.
            set: config.option(name) != Some(&default_of(name)),
        })
        .collect()
}

/// What the table says an option is, for the `set` flag.
fn default_of(name: &str) -> compositor_config::OptionValue {
    compositor_config::default_of(name).unwrap_or(compositor_config::OptionValue::Int(0))
}

/// What one window is drawn with, as `hyprctl getprop` reads it.
fn described_style(style: &compositor_render::WindowStyle) -> compositor_ipc::Style {
    compositor_ipc::Style {
        alpha: style.opacity,
        rounding: style.rounding,
        border: style.border,
        no_blur: !style.blur,
        no_shadow: !style.shadow,
        no_dim: !style.dim,
    }
}

/// One bind, as `hyprctl binds` prints it.
///
/// The configuration's own value: Hyprland lists what it parsed rather than
/// what the seat resolved, so a bind naming a key this keymap does not have
/// is still listed -- which is what makes the list worth reading when a
/// bind is not firing.
fn described_bind(bind: &compositor_config::Bind) -> compositor_ipc::Bind {
    compositor_ipc::Bind {
        locked: bind.flags.locked,
        mouse: bind.flags.mouse,
        release: bind.flags.release,
        repeat: bind.flags.repeat,
        long_press: bind.flags.long_press,
        non_consuming: bind.flags.non_consuming,
        has_description: bind.flags.description,
        modmask: bind.mods.0,
        submap: bind.submap.clone().unwrap_or_default(),
        submap_universal: bind.flags.submap_universal,
        key: bind.key.to_string(),
        keycode: match bind.key {
            compositor_config::Key::Code(code) => i32::try_from(code).unwrap_or(0),
            _ => 0,
        },
        catch_all: bind.flags.ignore_mods,
        description: bind.description.clone(),
        dispatcher: bind.dispatcher.clone(),
        arg: bind.arg.clone(),
    }
}

/// The input devices, in the groups `hyprctl devices` prints.
///
/// # Why every keyboard reports the same group
///
/// Hyprland's layout group belongs to the keyboard: two keyboards may sit
/// on different layouts, and what the clients are told is the group of
/// whichever one was last typed on. This compositor's seat has one XKB
/// state for every device it reads, so every keyboard here honestly reports
/// the same group -- they share it -- and `switchxkblayout` naming one of
/// them moves the seat. That is reported rather than hidden: the group and
/// the keymap name are the same in every row, so a person reading the
/// answer can see that the keyboards are not separate layouts pretending to
/// be one.
fn described_devices(
    devices: &crate::devices::Devices,
    config: &compositor_config::Config,
    keyboard: &compositor_xkb::Keyboard,
) -> compositor_ipc::Devices {
    let layouts = crate::seat::chosen_layouts(config);
    let group = keyboard.group();
    // Whether the locks are on. Hyprland reads only the *locked* mask
    // (`getModState`, `src/debug/HyprCtl.cpp:800`), so a keyboard with
    // `Caps Lock` held down -- which capitalises every letter the client
    // receives -- is reported as having it off. The effective mask is read
    // here instead, which is the state the keys are actually typed in and
    // the question `capsLock` is asking. `Num Lock` is `Mod2`, as
    // `XKB_MOD_NAME_NUM` is in every keymap this compositor ships.
    let modifiers = keyboard.modifiers();
    let effective = modifiers.depressed | modifiers.latched | modifiers.locked;
    // The rules as the configuration wrote them, whole: Hyprland reports
    // `m_currentRules`, which is what it passed
    // `xkb_keymap_new_from_names`, so `kb_layout = de,us` is reported as
    // `de,us` and not as the group in force
    // (`src/debug/HyprCtl.cpp:799`). The model and rules this compositor
    // would pass if it had libxkbcommon to pass them to are the ones the
    // keymaps were probed with.
    let option = |name: &str| config.str(name).unwrap_or_default().to_owned();
    let rules = match option("input:kb_rules") {
        empty if empty.is_empty() => "evdev".to_owned(),
        written => written,
    };
    let model = match option("input:kb_model") {
        empty if empty.is_empty() => "pc105".to_owned(),
        written => written,
    };
    let mut out = compositor_ipc::Devices::default();
    // The names a device is given are Hyprland's: normalised, and told
    // apart when two devices say they are the same thing. Mice and
    // keyboards are numbered in the one series, as `getNameForNewDevice`
    // numbers them, because it looks at every device it has named.
    let mut named: Vec<String> = Vec::new();
    let groups = keyboard.groups();
    for (address, name, types_keys) in devices.listed() {
        let name = compositor_ipc::new_device_name(&name, &named);
        named.push(name.clone());
        let device = compositor_ipc::Device { address, name };
        if types_keys {
            out.keyboards.push(compositor_ipc::Keyboard {
                device,
                rules: rules.clone(),
                model: model.clone(),
                layout: option("input:kb_layout"),
                variant: option("input:kb_variant"),
                options: option("input:kb_options"),
                active_layout_index: Some(group),
                active_keymap: layouts
                    .get(group as usize)
                    .map_or_else(|| "none".to_owned(), |(layout, _)| layout.label.to_owned()),
                groups,
                caps_lock: effective & compositor_xkb::generated::LOCK != 0,
                num_lock: effective & compositor_xkb::generated::MOD2 != 0,
                // The first keyboard listed. Hyprland's `main` is the one
                // that last produced an event, and this seat does not
                // record which that was; with one state shared by every
                // keyboard, `main` and any other are the same keyboard to
                // switch, so naming the first is a choice with no
                // consequence rather than a claim about the hardware.
                main: out.keyboards.is_empty(),
            });
        } else {
            out.mice.push(device);
        }
    }
    out
}

/// Every layer surface, with the monitor and the level `hyprctl layers`
/// groups them by.
fn described_layers(
    state: &State,
    clients: &[crate::state::Slot],
    placed: &[crate::frame::Placed],
) -> Vec<compositor_ipc::Layer> {
    let mut out = Vec::new();
    for (index, slot) in clients.iter().enumerate() {
        for (id, surface) in slot.client().layer_surfaces() {
            let Some(at) = placed
                .iter()
                .find(|placed| placed.client == index && placed.surface == surface.surface)
            else {
                continue;
            };
            // Which monitor it landed on: the one whose rectangle holds its
            // top-left, since a layer surface is placed inside one monitor.
            let monitor = state
                .monitors()
                .find(|monitor| {
                    monitor.rect.x <= at.rect.x
                        && at.rect.x < monitor.rect.x.saturating_add(monitor.rect.width)
                })
                .map(|monitor| monitor.name.clone())
                .unwrap_or_default();
            out.push(compositor_ipc::Layer {
                monitor,
                level: match surface.layer {
                    compositor_server::Layer::Background => 0,
                    compositor_server::Layer::Bottom => 1,
                    compositor_server::Layer::Top => 2,
                    compositor_server::Layer::Overlay => 3,
                },
                // Hyprland prints the object's address; this is the
                // connection and the object, which is as unique and as
                // opaque.
                address: (u64::try_from(index).unwrap_or(0) << 32) | u64::from(id.0),
                at: (
                    i32::try_from(at.rect.x).unwrap_or(0),
                    i32::try_from(at.rect.y).unwrap_or(0),
                ),
                size: (
                    i32::try_from(at.rect.width).unwrap_or(0),
                    i32::try_from(at.rect.height).unwrap_or(0),
                ),
                namespace: surface.namespace.clone(),
                pid: slot.pid(),
            });
        }
    }
    out
}

/// The same, without the plugins.
///
/// A bar reading `zwlr_foreign_toplevel_management_v1` wants the windows and
/// nothing else, and it has to be answered from inside one client's pass --
/// before the roundtrip it sent after binding comes back -- where the
/// plugins are not to hand.
#[must_use]
pub fn describe_all(
    state: &State,
    clients: &[crate::state::Slot],
    sources: &BTreeMap<WindowId, Source>,
    styles: &BTreeMap<WindowId, compositor_render::WindowStyle>,
    submap: &str,
) -> Snapshot {
    let mut snapshot = Snapshot {
        submap: submap.to_owned(),
        ..Snapshot::default()
    };
    // One monitor a screen, in the order they were added, which is the order
    // `hyprctl monitors` prints and the order `focusmonitor +1` walks.
    for monitor in state.monitors() {
        let active = state.active_workspace(monitor.id);
        // Hyprland prints `m_pixelSize`, the connector's mode, which a
        // turned monitor's layout rectangle has the other way round: a
        // 1920x1080 monitor stood on its edge is laid out 1080 wide and
        // printed 1920x1080 with `transform: 1` under it.
        let (width, height) = monitor
            .transform
            .size((monitor.rect.width, monitor.rect.height));
        snapshot.monitors.push(Monitor {
            id: monitor_id(monitor.id),
            name: monitor.name.clone(),
            width: i32::try_from(width).unwrap_or(0),
            height: i32::try_from(height).unwrap_or(0),
            refresh: 60.0,
            at: (
                i32::try_from(monitor.rect.x).unwrap_or(0),
                i32::try_from(monitor.rect.y).unwrap_or(0),
            ),
            active_workspace: active.map_or(0, |id| i32::try_from(id.0).unwrap_or(0)),
            active_workspace_name: active
                .map(|id| state.workspace_name(id))
                .unwrap_or_default(),
            special_workspace: state
                .special_on(monitor.id)
                .map(|id| (i32::try_from(id.0).unwrap_or(0), state.workspace_name(id))),
            scale: monitor.scale,
            transform: monitor.transform,
            focused: Some(monitor.id) == state.focused_monitor(),
            description: monitor.description.clone(),
            make: monitor.made.0.clone(),
            model: monitor.made.1.clone(),
            serial: monitor.made.2.clone(),
            // Hyprland's order: left, top, right, bottom.
            reserved: (
                i32::try_from(monitor.reserved.left).unwrap_or(0),
                i32::try_from(monitor.reserved.top).unwrap_or(0),
                i32::try_from(monitor.reserved.right).unwrap_or(0),
                i32::try_from(monitor.reserved.bottom).unwrap_or(0),
            ),
            // `dpms` is the compositor's, not the layout's; the snapshot
            // that has it fills it in.
            dpms: true,
        });
    }

    let focused = state.focused_window();
    // Hyprland's focus history id: 0 is the focused window, and the rest
    // count back from it. Only the focused one is known here, so the others
    // are -1, which Hyprland uses for a window that is not in the history.
    for output in state.layout() {
        let mut windows = 0;
        let mut has_fullscreen = false;
        for placed in &output.windows {
            has_fullscreen |= placed.fullscreen;
            // A group's slot draws one member; Hyprland lists the rest as
            // hidden windows with the same box, so a bar can draw the tabs.
            let grouped: Vec<u64> = state
                .group(placed.window)
                .map(|group| group.members.iter().map(|member| member.0).collect())
                .unwrap_or_default();
            let shown = placed.window;
            let members = if grouped.is_empty() {
                vec![shown]
            } else {
                grouped.iter().copied().map(WindowId).collect()
            };
            for member in members {
                windows += 1;
                let described = Described {
                    window: member,
                    placed,
                    workspace: output.workspace,
                    hidden: member != shown,
                    grouped: &grouped,
                };
                if let Some(window) = describe(state, clients, sources, styles, focused, &described)
                {
                    snapshot.windows.push(window);
                }
            }
        }
        snapshot.workspaces.push(Workspace {
            id: i32::try_from(output.workspace.0).unwrap_or(0),
            name: state.workspace_name(output.workspace),
            monitor: state
                .monitors()
                .find(|monitor| monitor.id == output.monitor)
                .map(|monitor| monitor.name.clone())
                .unwrap_or_default(),
            windows,
            has_fullscreen,
        });
        // The active workspace is the focused monitor's, not the last
        // monitor's: `hyprctl activeworkspace` answers about where the
        // person is.
        if Some(output.monitor) == state.focused_monitor() {
            snapshot.active_workspace = i32::try_from(output.workspace.0).unwrap_or(0);
        }
    }
    snapshot.active_window = focused.map(|window| window.0);
    snapshot
}

/// The id `hyprctl` gives a monitor, which Hyprland counts from zero where
/// this tree's `MonitorId` counts from one.
fn monitor_id(monitor: compositor_layout::MonitorId) -> i32 {
    i32::try_from(monitor.0.saturating_sub(1)).unwrap_or(0)
}

/// What every window in `snapshot` looks like to a bar.
///
/// The same description `hyprctl clients` prints, narrowed to the six things
/// `zwlr_foreign_toplevel_handle_v1` carries. `maximized` is always false:
/// this layout has one fullscreen state and no separate maximised one, and
/// saying a window is maximised when the compositor cannot tell would put a
/// wrong tick in every taskbar's menu. `minimized` is false for the same
/// reason -- nothing here is minimised; a window a group does not draw is
/// still on its workspace, and a bar that hid it would hide half a group.
#[must_use]
pub fn toplevels(snapshot: &Snapshot) -> Vec<ForeignToplevel> {
    snapshot
        .windows
        .iter()
        .map(|window| ForeignToplevel {
            window: window.address,
            title: window.title.clone(),
            app_id: window.class.clone(),
            activated: snapshot.active_window == Some(window.address),
            fullscreen: window.fullscreen,
            maximized: false,
            minimized: false,
        })
        .collect()
}

/// The title and app id of the window on `surface`, if it has them.
/// One window for `hyprctl clients`, and where it is.
struct Described<'a> {
    /// The window being described, which for a group's hidden member is not
    /// the window whose placement it borrows.
    window: WindowId,
    /// The placement of the slot it is in.
    placed: &'a compositor_layout::Placed,
    /// The workspace that slot is on.
    workspace: compositor_layout::WorkspaceId,
    /// Whether a group draws another member in its place.
    hidden: bool,
    /// Its group's members, empty if it is in none.
    grouped: &'a [u64],
}

/// What `hyprctl clients` says about one window, or nothing if no client of
/// this compositor owns it.
fn describe(
    state: &State,
    clients: &[crate::state::Slot],
    sources: &BTreeMap<WindowId, Source>,
    styles: &BTreeMap<WindowId, compositor_render::WindowStyle>,
    focused: Option<WindowId>,
    it: &Described,
) -> Option<Window> {
    let source = sources.get(&it.window)?;
    let slot = clients.get(source.client)?;
    let named = toplevel_of(slot.client(), source.surface);
    let placed = it.placed;
    Some(Window {
        address: it.window.0,
        mapped: slot
            .client()
            .surface(source.surface)
            .is_some_and(compositor_server::Surface::is_mapped),
        hidden: it.hidden,
        visible: !it.hidden,
        at: (
            i32::try_from(placed.rect.x).unwrap_or(0),
            i32::try_from(placed.rect.y).unwrap_or(0),
        ),
        size: (
            i32::try_from(placed.rect.width).unwrap_or(0),
            i32::try_from(placed.rect.height).unwrap_or(0),
        ),
        workspace: i32::try_from(it.workspace.0).unwrap_or(0),
        workspace_name: state.workspace_name(it.workspace),
        floating: placed.floating,
        fullscreen: placed.fullscreen,
        monitor: 0,
        class: named.as_ref().map(|top| top.1.clone()).unwrap_or_default(),
        title: named.map(|top| top.0).unwrap_or_default(),
        pid: slot.pid(),
        focus_history: if Some(it.window) == focused { 0 } else { -1 },
        grouped: it.grouped.to_vec(),
        style: styles
            .get(&it.window)
            .map(described_style)
            .unwrap_or_default(),
    })
}

fn toplevel_of(client: &Client, surface: ObjectId) -> Option<(String, String)> {
    client
        .toplevels()
        .find(|(_, top)| top.surface == surface)
        .map(|(_, top)| (top.title.clone(), top.app_id.clone()))
}

/// Every workspace as `ext-workspace-v1` describes one.
///
/// The group is the monitor's place in the snapshot's list, which is what
/// `publish_workspaces` makes one group each of.
#[must_use]
pub fn workspaces(snapshot: &Snapshot) -> Vec<compositor_server::Workspace> {
    snapshot
        .workspaces
        .iter()
        .map(|workspace| compositor_server::Workspace {
            id: i64::from(workspace.id),
            name: workspace.name.clone(),
            group: snapshot
                .monitors
                .iter()
                .position(|monitor| monitor.name == workspace.monitor)
                .unwrap_or(0),
            active: snapshot
                .monitors
                .iter()
                .any(|monitor| monitor.active_workspace == workspace.id),
            // Nothing sets urgency on a workspace yet: `xdg_activation`
            // makes a *window* urgent, and which workspace that is is the
            // layout's to say once there is a rule that asks.
            urgent: false,
        })
        .collect()
}

/// The event socket: connections that are written to and never read.
///
/// A bar connects once and stays connected for the session, so the
/// connections are kept and each is written to as things happen. A client
/// that goes away is dropped on the write that fails, which is how Hyprland
/// notices too (`CEventManager::flushClient`).
#[derive(Debug)]
pub struct Events {
    listener: UnixListener,
    subscribers: Vec<UnixStream>,
    watcher: compositor_ipc::Watcher,
    /// How many lines have been written, for the compositor's own report.
    written: u64,
}

impl Events {
    /// Bind the event socket beside the request one.
    ///
    /// # Errors
    ///
    /// Whatever the bind said.
    pub fn bind(directory: &Path) -> std::io::Result<Self> {
        let path = directory.join(compositor_ipc::EVENT_SOCKET);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            subscribers: Vec::new(),
            watcher: compositor_ipc::Watcher::new(),
            written: 0,
        })
    }

    /// How many subscribers there are and how many lines they have been
    /// sent, for the compositor's own report.
    #[must_use]
    pub fn counts(&self) -> (usize, u64) {
        (self.subscribers.len(), self.written)
    }

    /// Whether an event subscriber needs state changes to be published.
    #[must_use]
    pub fn has_subscribers(&self) -> bool {
        !self.subscribers.is_empty()
    }

    /// The subscriber listener descriptor, for the compositor's event wait.
    #[must_use]
    pub fn raw_fd(&self) -> i32 {
        self.listener.as_raw_fd()
    }

    /// Write one line to every subscriber, as `dispatch event` does.
    ///
    /// The line is the caller's whole line, newline and all, because the
    /// only caller is the `event` dispatcher and Hyprland's own
    /// `custom>>...` is not a shape the watcher knows.
    pub fn say(&mut self, line: &str) {
        self.subscribers
            .retain_mut(|stream| stream.write_all(line.as_bytes()).is_ok());
        self.written = self.written.saturating_add(1);
    }

    /// Take any new subscribers, work out what changed, and write it.
    ///
    /// A subscriber that has just connected is told what already exists,
    /// because its [`compositor_ipc::Watcher`] would otherwise start from
    /// the state it connected in and tell it nothing until something moved.
    /// Hyprland leaves a fresh client to ask `hyprctl` for that; telling it
    /// is strictly more useful and no reader can be surprised by an event
    /// for a window it does not know about yet.
    ///
    /// `trail` is where the focus went during the pass, which the watcher
    /// says a window at a time ([`compositor_ipc::Watcher::changed_through`]).
    pub fn publish(&mut self, snapshot: &Snapshot, trail: &[Option<u64>]) {
        let mut fresh = Vec::new();
        while let Ok((stream, _)) = self.listener.accept() {
            // Written to and never read: a blocking write to a bar that has
            // stopped reading would stop the compositor.
            if stream.set_nonblocking(true).is_ok() {
                fresh.push(stream);
            }
        }
        // A first subscriber is told the whole state; one joining an
        // existing one gets the changes from here on, as Hyprland's would.
        if !fresh.is_empty() && self.subscribers.is_empty() {
            self.watcher = compositor_ipc::Watcher::new();
        }
        let events = self.watcher.changed_through(snapshot, trail);
        self.subscribers.append(&mut fresh);
        if events.is_empty() || self.subscribers.is_empty() {
            return;
        }
        let lines: Vec<String> = events
            .iter()
            .flat_map(compositor_ipc::Event::lines)
            .collect();
        self.subscribers.retain_mut(|stream| {
            for line in &lines {
                if stream.write_all(line.as_bytes()).is_err() {
                    return false;
                }
            }
            true
        });
        self.written = self
            .written
            .saturating_add(lines.len().try_into().unwrap_or(u64::MAX));
    }
}
