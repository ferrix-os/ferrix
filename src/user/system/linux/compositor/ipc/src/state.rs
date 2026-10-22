//! What the compositor tells the IPC about itself.
//!
//! A snapshot rather than a borrow of the compositor's own state: an answer
//! is written whole, and a request that arrived halfway through a frame
//! should describe the frame before it rather than half of the next.

/// One window, with the fields `hyprctl clients` prints.
///
/// Not `Eq`: a window's own opacity is a fraction, as Hyprland's is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Window {
    /// Hyprland prints an address, which programs use as an opaque handle.
    /// This is the compositor's own window id, printed the same way.
    pub address: u64,
    /// Whether it is showing a buffer.
    pub mapped: bool,
    /// Whether something hides it: a group draws only its active member,
    /// and Hyprland lists the rest as hidden rather than not at all. A
    /// window behind a fullscreen one is not hidden: Hyprland blocks its
    /// input instead, which is `accepts_input`.
    pub hidden: bool,
    /// Hyprland's `CWindow::visible`: mapped and not hidden, whether or not
    /// a monitor shows its workspace or a fullscreen window covers it.
    pub visible: bool,
    /// Whether input may reach it: not hidden, and not covered by a
    /// fullscreen window or the monocle layout's shown one
    /// (`CWindow::acceptsInput`).
    pub accepts_input: bool,
    /// Its client area's top-left, in the space all monitors share.
    pub at: (i32, i32),
    /// Its client area's size.
    pub size: (i32, i32),
    /// The workspace it is on.
    pub workspace: i32,
    /// That workspace's name.
    pub workspace_name: String,
    /// Whether it floats.
    pub floating: bool,
    /// Whether it is fullscreen.
    pub fullscreen: bool,
    /// Hyprland's fullscreen mode, which `hyprctl clients` prints as a
    /// number: 0 none, 1 maximized, 2 fullscreen (`eFullscreenMode`).
    pub fullscreen_mode: u8,
    /// Which monitor it is on.
    pub monitor: i32,
    /// `xdg_toplevel.set_app_id`, which Hyprland calls the class and which
    /// `windowrule` matches on.
    pub class: String,
    /// `xdg_toplevel.set_title`.
    pub title: String,
    /// The process, when the compositor started it; zero otherwise.
    pub pid: i32,
    /// How recently it was focused: 0 is the focused window.
    pub focus_history: i32,
    /// The addresses of every window in its group, itself among them, in
    /// the group's own order. Empty when it is in no group.
    pub grouped: Vec<u64>,
    /// What it is drawn with where a `windowrule` or `setprop` asked for
    /// something other than the style every window has: `hyprctl getprop`.
    pub style: Style,
}

/// One window's own decorations, as `hyprctl getprop` reads them.
///
/// Each is `None` for "whatever the style says", which is what a window
/// with no rule has, and `hyprctl getprop` prints the compositor's own
/// value for one of those.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    /// `alpha`: how much of the window shows.
    pub alpha: Option<f32>,
    /// `rounding`: how far its corners are cut.
    pub rounding: Option<i64>,
    /// `bordersize`: how wide its border is.
    pub border: Option<i64>,
    /// `noblur`.
    pub no_blur: bool,
    /// `noshadow`.
    pub no_shadow: bool,
    /// `nodim`.
    pub no_dim: bool,
}

/// One workspace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Workspace {
    /// Its number.
    pub id: i32,
    /// Its name, which for a numbered workspace is its number.
    pub name: String,
    /// The monitor showing it.
    pub monitor: String,
    /// How many windows are on it.
    pub windows: u32,
    /// Whether any of them is fullscreen.
    pub has_fullscreen: bool,
}

/// One monitor.
///
/// Not `Eq`: the refresh rate and the scale are what `hyprctl monitors`
/// prints, and Hyprland prints them as floats.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Monitor {
    /// Its number.
    pub id: i32,
    /// Its name, as `wl_output.name` carries it.
    pub name: String,
    /// In pixels.
    pub width: i32,
    /// In pixels.
    pub height: i32,
    /// In hertz, as `hyprctl monitors` prints it.
    pub refresh: f64,
    /// Where it is in the space all monitors share.
    pub at: (i32, i32),
    /// The workspace it is showing.
    pub active_workspace: i32,
    /// That workspace's name.
    pub active_workspace_name: String,
    /// The special workspace shown over it, if one is: its id and its name.
    /// Hyprland prints `special workspace: 0 ()` for a monitor showing none,
    /// which is what `None` becomes.
    pub special_workspace: Option<(i32, String)>,
    /// Buffer pixels per logical pixel.
    pub scale: f64,
    /// How it is turned: `monitor = ..., transform, N`, printed as the
    /// `wl_output.transform` number, as Hyprland prints `m_transform`.
    pub transform: compositor_config::Transform,
    /// Whether it holds the focus.
    pub focused: bool,
    /// What the monitor says it is: the make, the model and the serial with
    /// spaces between them. Hyprland calls this the *short* description and
    /// prints it under `description`; it is what a `monitor = desc:` line
    /// and a bar's own `"output"` setting match on.
    pub description: String,
    /// The three parts of it, which Hyprland prints apart as well.
    pub make: String,
    /// The same.
    pub model: String,
    /// The same.
    pub serial: String,
    /// What the layer surfaces reserved on each side, in Hyprland's order:
    /// left, top, right, bottom.
    pub reserved: (i32, i32, i32, i32),
    /// Whether `dpms` has it on.
    pub dpms: bool,
    /// Every mode its connector lists, `(width, height, refresh in
    /// hertz)`, in the connector's order: `availableModes`. Empty for a
    /// monitor that cannot say, which then lists its own mode.
    pub modes: Vec<(u32, u32, f64)>,
}

/// Everything an answer is written from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// The plugins that are loaded.
    pub plugins: Vec<Plugin>,
    /// The monitors.
    pub monitors: Vec<Monitor>,
    /// The workspaces that exist.
    pub workspaces: Vec<Workspace>,
    /// The windows, in the order the compositor holds them.
    pub windows: Vec<Window>,
    /// The focused window's address, if one is focused.
    pub active_window: Option<u64>,
    /// The workspace the focused monitor shows.
    pub active_workspace: i32,
    /// The submap in force, empty for the global map, which is what
    /// `hyprctl submap` prints and what the `submap` event carries.
    pub submap: String,
    /// Every key binding, in the order the configuration wrote them.
    pub binds: Vec<Bind>,
    /// The input devices, which `hyprctl devices` prints.
    pub devices: Devices,
    /// Every layer surface, which `hyprctl layers` prints by monitor and
    /// level.
    pub layers: Vec<Layer>,
    /// Where the pointer is, in the space all monitors share.
    pub cursor: (i32, i32),
    /// Whether a session lock is up.
    pub locked: bool,
    /// Every `workspace =` line, read: `hyprctl workspacerules`.
    pub workspace_rules: Vec<compositor_config::WorkspaceRule>,
    /// Every option the compositor has and what it holds now, which is
    /// what `hyprctl getoption` and `hyprctl descriptions` read.
    pub options: Vec<Opt>,
    /// Every animation the configuration names, for `hyprctl animations`.
    pub animations: Vec<Animation>,
    /// Every bezier it names, for the same.
    pub beziers: Vec<Bezier>,
    /// What could not be read in the configuration, a line each:
    /// `hyprctl configerrors`.
    pub errors: Vec<String>,
    /// The last lines the compositor said: `hyprctl rollinglog`.
    pub log: Vec<String>,
    /// Every global shortcut a program registered, by name and
    /// description: `hyprctl globalshortcuts`.
    pub shortcuts: Vec<Shortcut>,
    /// What the compositor is running on: `hyprctl systeminfo`.
    pub system: System,
}

/// One option, as `hyprctl getoption` prints it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Opt {
    /// Its name, `category:key`.
    pub name: String,
    /// What it holds now, as the compositor would write it.
    pub value: String,
    /// Which of Hyprland's shapes it is: `int`, `float`, `str`, `vec2`,
    /// `custom`. That word is the JSON key its value goes under, which is
    /// what a script reads.
    pub kind: &'static str,
    /// Whether the configuration set it, rather than it being the default.
    pub set: bool,
}

/// One animation, as `hyprctl animations` prints it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Animation {
    /// What it animates: `windows`, `workspaces`, `fade`.
    pub name: String,
    /// Whether the configuration named it rather than its parent.
    pub overridden: bool,
    /// The bezier it uses.
    pub bezier: String,
    /// Whether it runs at all.
    pub enabled: bool,
    /// How fast, in tenths of a second.
    pub speed: f64,
    /// Its style, where the animation has one.
    pub style: String,
}

/// One bezier curve, as the same command prints it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bezier {
    /// What it is called.
    pub name: String,
    /// The first control point.
    pub first: (f64, f64),
    /// The second.
    pub second: (f64, f64),
}

/// One global shortcut a program registered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shortcut {
    /// `<app_id>:<id>`, which is what `dispatch global` takes.
    pub name: String,
    /// What the program says it is for.
    pub description: String,
}

/// What `hyprctl systeminfo` and `hyprctl status` say the compositor is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct System {
    /// The operating system it is running on.
    pub os: String,
    /// The kernel.
    pub kernel: String,
    /// How many monitors, windows and connections there are.
    pub counts: (usize, usize, usize),
    /// How long it has been running, in seconds.
    pub uptime: u64,
}

/// One key binding, with the fields `hyprctl binds` prints.
///
/// The names are Hyprland's JSON keys, which is what a script reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bind {
    /// `l`: works while the session is locked.
    pub locked: bool,
    /// `m`: a mouse binding.
    pub mouse: bool,
    /// `r`: fires on release.
    pub release: bool,
    /// `e`: repeats while held.
    pub repeat: bool,
    /// `o`: fires on a long press.
    pub long_press: bool,
    /// `n`: the key still reaches the focused client.
    pub non_consuming: bool,
    /// `d`: a description was given.
    pub has_description: bool,
    /// The modifiers, as the bits a keymap gives.
    pub modmask: u32,
    /// The submap it is in, empty for the global map.
    pub submap: String,
    /// `u`: it fires in every submap.
    pub submap_universal: bool,
    /// The key as it was written.
    pub key: String,
    /// The keycode, for a `code:NN` binding; zero otherwise.
    pub keycode: i32,
    /// Whether it fires whatever modifiers are held, which is the `i` flag
    /// and what Hyprland calls a catch-all.
    pub catch_all: bool,
    /// The description, empty unless one was given.
    pub description: String,
    /// The dispatcher's name.
    pub dispatcher: String,
    /// Its argument, as written.
    pub arg: String,
}

/// The input devices, as `hyprctl devices` groups them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Devices {
    /// Pointers.
    pub mice: Vec<Device>,
    /// Keyboards.
    pub keyboards: Vec<Keyboard>,
    /// Tablets, which this compositor reports as pointers of their own.
    pub tablets: Vec<Device>,
    /// Touchscreens.
    pub touch: Vec<Device>,
    /// Lid and tablet-mode switches.
    pub switches: Vec<Device>,
}

/// One input device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Device {
    /// Hyprland prints the object's address; this is the node's number,
    /// printed the same way, so that two devices are told apart.
    pub address: u64,
    /// What the device is called, as [`device_name`] writes it.
    ///
    /// Hyprland keeps one name for a device and it is the normalised one:
    /// `m_hlName`, which `hyprctl devices` prints, which a `device =`
    /// section matches, and which `switchxkblayout <device>` selects on.
    /// The raw name the kernel gave is not kept anywhere, so this is the
    /// name, not a display form of one.
    pub name: String,
}

/// One keyboard, which carries its keymap as well.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Keyboard {
    /// The device.
    pub device: Device,
    /// The XKB rules, model, layout, variant and options.
    ///
    /// The strings the configuration wrote, whole: `input:kb_layout =
    /// de,us` is reported as `de,us` and not as the group this keyboard is
    /// in, because Hyprland reports `m_currentRules`, which is what it
    /// passed `xkb_keymap_new_from_names` (`src/debug/HyprCtl.cpp:799`).
    pub rules: String,
    /// The model.
    pub model: String,
    /// The layout.
    pub layout: String,
    /// The variant.
    pub variant: String,
    /// The options.
    pub options: String,
    /// Which layout group is active, counting from zero, or `None` for a
    /// keyboard whose state has none.
    ///
    /// `IKeyboard::getActiveLayoutIndex` (`src/devices/IKeyboard.cpp:293`)
    /// returns an empty optional when no group is active, and `hyprctl
    /// devices` prints the word `none` for it.
    pub active_layout_index: Option<u32>,
    /// What that group is called: `xkb_keymap_layout_get_name`, which for
    /// `us` is `English (US)`. `none` where there is no active group, which
    /// is what `IKeyboard::getActiveLayout` answers
    /// (`src/devices/IKeyboard.cpp:306`).
    pub active_keymap: String,
    /// How many layout groups its keymap has:
    /// `xkb_keymap_num_layouts`, which `switchxkblayout` range-checks a
    /// numeric argument against and wraps `next` and `prev` around.
    ///
    /// A keymap has at least one group, so a zero here is read as one:
    /// Hyprland range-checks against `LAYOUTS - 1` in unsigned arithmetic
    /// (`src/debug/HyprCtl.cpp:1387`), where a zero count lets every index
    /// through, and answering as though there were one group is the same
    /// answer for the only keymap that can exist.
    pub groups: u32,
    /// Whether Caps Lock is on.
    pub caps_lock: bool,
    /// Whether Num Lock is on.
    pub num_lock: bool,
    /// Whether it is the seat's keyboard: the one that last produced an
    /// event, which is Hyprland's `m_active` and what `main`, `active` and
    /// `current` name (`src/managers/SeatManager.cpp:166`).
    pub main: bool,
}

/// A device's name, as Hyprland writes every device's name.
///
/// `deviceNameToInternalString` (`src/helpers/MiscFunctions.cpp:777`): lower
/// case, with a space, a newline or a comma replaced by a dash. The three
/// that are replaced are the three that would make the name unusable where
/// it is used -- a space splits `switchxkblayout <device> <cmd>`'s
/// arguments, a comma splits the `activelayout` event's payload, and a
/// newline ends an event line -- so the normalisation is not cosmetic and a
/// compositor that skipped it would have devices nothing can name.
///
/// Lower-casing is ASCII only. Hyprland calls `std::tolower` on each byte,
/// which in the `C` locale every compositor runs under changes `A`-`Z` and
/// nothing else; doing it per character rather than per byte differs only
/// for names holding non-ASCII letters, where Hyprland would mangle the
/// bytes of one character apart and this leaves them alone.
#[must_use]
pub fn device_name(raw: &str) -> String {
    raw.chars()
        .map(|character| match character {
            ' ' | '\n' | ',' => '-',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

/// The name to give a device that has just appeared, given the names
/// already taken.
///
/// `CInputManager::getNameForNewDevice`
/// (`src/managers/input/InputManager.cpp:2119`): the normalised name, or
/// `unknown-device` when the device gave none, and then `-1`, `-2` and so
/// on until it is nobody else's. Two identical keyboards -- which is what a
/// machine with a built-in keyboard and a USB one of the same model has --
/// would otherwise share a name, and `switchxkblayout <device>` could name
/// only the first of them.
#[must_use]
pub fn new_device_name(raw: &str, taken: &[String]) -> String {
    let normalised = device_name(raw);
    let stem = if normalised.is_empty() {
        "unknown-device"
    } else {
        &normalised
    };
    let mut dupe = 0u32;
    loop {
        let candidate = if dupe == 0 {
            stem.to_owned()
        } else {
            format!("{stem}-{dupe}")
        };
        if !taken.contains(&candidate) {
            return candidate;
        }
        dupe = dupe.saturating_add(1);
    }
}

/// One layer surface: a bar, a wallpaper, a launcher.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layer {
    /// The monitor it is on, by name.
    pub monitor: String,
    /// Which of `zwlr_layer_shell_v1`'s four levels it is on.
    pub level: u32,
    /// Hyprland prints an address; this is the compositor's own handle.
    pub address: u64,
    /// Its top-left, in the space all monitors share.
    pub at: (i32, i32),
    /// Its size.
    pub size: (i32, i32),
    /// The namespace it was made with, which is what a rule matches on.
    pub namespace: String,
    /// The process, when the compositor started it; zero otherwise.
    pub pid: i32,
}

/// One plugin, as `hyprctl plugin list` prints it.
///
/// Hyprland's plugins are shared objects it loads into itself and this
/// compositor's are programs it talks to, but what a plugin *says about
/// itself* is the same: `PLUGIN_INIT` returns a name, an author, a version
/// and a description, and this is that.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plugin {
    /// What it calls itself.
    pub name: String,
    /// Who wrote it.
    pub author: String,
    /// Its version.
    pub version: String,
    /// What it says it does.
    pub description: String,
    /// The compositor's handle for it, which Hyprland prints as the address
    /// of the loaded object and this prints as the connection's number.
    pub handle: u64,
    /// The dispatchers it has registered, in the order it registered them.
    pub dispatchers: Vec<String>,
}

impl Snapshot {
    /// The focused window, if there is one.
    #[must_use]
    pub fn active(&self) -> Option<&Window> {
        let address = self.active_window?;
        self.windows.iter().find(|window| window.address == address)
    }
}
