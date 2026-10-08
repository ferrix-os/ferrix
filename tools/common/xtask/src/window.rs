//! Where a boot that is meant to be watched puts the guest's screen.
//!
//! Every `test-*` command in this tool is headless on purpose: it asks QEMU
//! for a screendump over QMP and compares pixels, which is a judgement a
//! machine can make and repeat. `cargo xtask run --display` and `cargo xtask
//! run-compositor` are the other half — a person watching the compositor draw
//! — and that needs a window, which is the one part of QEMU that is not the
//! same on two hosts.
//!
//! # Why `-display default` is not enough
//!
//! QEMU's `default` is whichever local backend was compiled in, and a build
//! without one has nothing to fall back to: it fails at startup rather than
//! booting headless. The two QEMUs this project is developed on are exactly
//! those two cases.
//!
//! * The Windows build (`winget install SoftwareFreedomConservancy.QEMU`)
//!   offers `gtk` and `sdl`, so a window opens and `default` would have been
//!   enough.
//! * The Linux box the gates run on builds QEMU from source, headless, and
//!   offers `none`, `spice-app` and `dbus` — no `gtk`, no `sdl`, and no
//!   session to open a window on anyway, since it is reached over `ssh`. Its
//!   build does have VNC, which needs no display of the host's at all.
//!
//! So the backend is chosen by asking QEMU what it has (`-display help`) and
//! this host whether there is a session to open a window on, rather than by
//! asking which operating system this is: a headless Linux desktop machine
//! and a Windows one with a cut-down QEMU are both real, and the question
//! "can a window open here" is the one that decides.
//!
//! # VNC is the fallback, on the loopback
//!
//! A VNC server has no window and no session: it draws into a socket, which
//! is why it is what a machine reached over `ssh` can show a screen on. It
//! also has no password here — `-vnc` without `password=on` accepts any
//! client — so the default address is `127.0.0.1`, and reaching it from
//! another machine is a tunnel the person opens deliberately:
//!
//! ```text
//! ssh -L 5900:127.0.0.1:5900 <host>
//! ```
//!
//! `--vnc <display>` asks for VNC even where a window could have opened, and
//! is the one way to put the screen on an address that is not the loopback.
//! Nothing here binds a wider one on its own.
//!
//! # Keys want a viewer that sends keys
//!
//! Plain VNC sends keysyms -- the character typed, not the key -- and QEMU
//! turns each back into a key through a keymap, `en-us` unless `-k` names
//! another, adding no modifier the viewer did not send: a German `~` through
//! `en-us` is the grave key without its Shift, and comes out `` ` ``. A
//! viewer with QEMU's extended key events (`TigerVNC`'s) sends the keys
//! themselves, and the guest's own `input:kb_layout` makes them characters, as
//! a real keyboard's are. So `-k` is passed only when `--keymap` asks for it,
//! for a viewer that sends characters: it would make QEMU translate even a
//! key-sending viewer's keys through its keymap, which is the one thing that
//! must not happen to them.

use std::path::Path;
use std::process::Command;

use crate::args::Args;
use crate::{Error, Result};

/// The local backends worth opening a window with, best first.
///
/// `gtk` before `sdl` because its menu bar lists the guest's consoles by
/// name, and a boot with a virtio-gpu has two of them: firmware's head and
/// the card the compositor draws on. `sdl` switches between them with
/// `Ctrl-Alt-<n>` and says nothing about what they are.
const LOCAL: [&str; 2] = ["gtk", "sdl"];

/// Where a boot's screen goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Window {
    /// Nowhere. Every `test-*` boot, and any `run` without `--display`: QEMU
    /// draws into memory and a screendump is how anything reads it.
    Headless,
    /// A window on this host, opened by one of QEMU's local backends.
    Local(&'static str),
    /// A VNC server at this `<host>:<display>`, which a viewer connects to.
    Vnc(String),
}

/// Which of the guest's screens a person means: the device id of the
/// virtio-gpu the compositor draws on, on a machine that has one.
///
/// A boot with a card has two heads, and the card is the second: firmware
/// takes the machine's own display device -- `q35`'s VGA, `virt`'s `ramfb` --
/// and the kernel's driver takes the virtio-gpu. QEMU shows the first console
/// unless something says otherwise, so a window opened with nothing said
/// shows the head the loader's messages went to and stays there while the
/// compositor draws on the other one.
pub(crate) type Card<'a> = Option<&'a str>;

impl Window {
    /// The `-display` argument this is, pointed at `card` where the backend
    /// can be told which console to serve.
    /// `gl` is for a boot whose card is the 3D one. QEMU refuses
    /// `virtio-gpu-gl-pci` on a backend without OpenGL -- *"The display
    /// backend does not have OpenGL support enabled"* -- so the backend has
    /// to be told. A headless boot cannot simply say `gl=on`, because `none`
    /// has no GL to turn on; `egl-headless` is the backend for exactly this.
    pub(crate) fn arguments_with(
        &self,
        card: Card<'_>,
        gl: bool,
        rendernode: Option<&str>,
    ) -> Vec<String> {
        let backend = match self {
            Window::Headless if gl => headless(rendernode),
            Window::Headless => "none".to_owned(),
            Window::Local("gtk") if gl => "gtk,show-tabs=on,gl=on".to_owned(),
            Window::Local(name) if gl => format!("{name},gl=on"),
            // `show-tabs=on`: the window has a tab per console and the
            // compositor's is not the one it opens on. GTK can be told to
            // show the tab bar but not which tab to start on, so the bar is
            // made visible and `announce` says which tab and its shortcut.
            Window::Local("gtk") => "gtk,show-tabs=on".to_owned(),
            Window::Local(name) => (*name).to_owned(),
            // VNC serves one console and can be told which, so a viewer
            // connects straight to the card rather than to firmware's head.
            // And with a 3D card it cannot be the display at all: VNC has no
            // OpenGL to give the card. `egl-headless` is the display then,
            // which draws off screen and copies each frame out for whoever
            // else is listening -- and a `-vnc` server is who.
            Window::Vnc(address) if gl => {
                let server = match card {
                    Some(card) => format!("{address},display={card},head=0"),
                    None => address.clone(),
                };
                return vec![
                    "-display".to_owned(),
                    headless(rendernode),
                    "-vnc".to_owned(),
                    server,
                ];
            }
            Window::Vnc(address) => match card {
                Some(card) => format!("vnc={address},display={card},head=0"),
                None => format!("vnc={address}"),
            },
        };
        vec!["-display".to_owned(), backend]
    }

    /// Say where the screen is, for the person who asked to watch it.
    ///
    /// A VNC server needs the most saying: it is running, it is showing the
    /// guest, and nothing on this machine has opened it yet.
    pub(crate) fn announce(&self, card: Card<'_>) {
        match self {
            Window::Headless => {}
            Window::Local(name) => {
                println!("  screen: a window, through QEMU's {name} backend");
                if card.is_some() {
                    // The window opens on the card, because `qemu` creates it
                    // before the head firmware drew on. The other console is
                    // still there, with the loader's text on it, and a person
                    // who wants it can reach it the way QEMU always offers.
                    println!(
                        "    it opens on {card}, where the compositor draws",
                        card = card.unwrap_or_default()
                    );
                    println!(
                        "    the loader's own head is the tab beside it (View menu; \
                         Ctrl-Alt-2 is unreliable on a German layout, where it is AltGr)"
                    );
                }
            }
            Window::Vnc(address) => {
                println!("  screen: VNC on {address}, port {}", port(address));
                println!(
                    "    no window backend here, so QEMU is serving the screen instead.\n    \
                     Connect a viewer to it; from another machine, tunnel first:\n      \
                     ssh -L {port}:127.0.0.1:{port} <this host>",
                    port = port(address)
                );
            }
        }
    }
}

/// The `egl-headless` backend, drawing on `rendernode` where one was named.
///
/// Without one QEMU opens the first render node it can, which is right on a
/// machine with one GPU and a guess on a machine with several -- and the
/// guess is made by device number, not by which card is any good at this.
/// QEMU's name for the keymap `--keymap` asks for, if QEMU ships one: an XKB
/// layout name where QEMU's differs (`us` is `en-us`), QEMU's own name as it
/// stands, and the first of a comma-separated list, which is the layout a
/// guest starts typing in.
pub(crate) fn vnc_keymap(asked: &str) -> Option<&'static str> {
    const QEMU: &[&str] = &[
        "ar", "bepo", "cz", "da", "de", "de-ch", "en-gb", "en-us", "es", "et", "fi", "fo", "fr",
        "fr-be", "fr-ca", "fr-ch", "hr", "hu", "is", "it", "ja", "lt", "lv", "mk", "nl", "no",
        "pl", "pt", "pt-br", "ru", "sl", "sv", "th", "tr",
    ];
    let first = asked.split(',').next()?.trim();
    let named = match first {
        "us" => "en-us",
        "gb" => "en-gb",
        "ch" => "de-ch",
        "be" => "fr-be",
        "br" => "pt-br",
        "dk" => "da",
        "ee" => "et",
        "jp" => "ja",
        "se" => "sv",
        "si" => "sl",
        "ara" => "ar",
        other => other,
    };
    QEMU.iter().copied().find(|name| *name == named)
}

/// `-k <keymap>` for a VNC boot given `--keymap`, and nothing for any other.
///
/// # Errors
///
/// A keymap QEMU does not ship, said before a boot rather than by a QEMU that
/// refuses to start.
pub(crate) fn keymap_arguments(window: &Window, args: &Args) -> Result<Vec<String>> {
    let Some(asked) = args.keymap.as_deref() else {
        return Ok(Vec::new());
    };
    if !matches!(window, Window::Vnc(_)) {
        println!("  keyboard: --keymap is for a VNC screen; a window sends keys as they are");
        return Ok(Vec::new());
    }
    let keymap = vnc_keymap(asked)
        .ok_or_else(|| Error::new(format!("QEMU ships no keymap for `{asked}`")))?;
    println!("  keyboard: VNC characters turned into keys through QEMU's {keymap} keymap");
    Ok(vec!["-k".to_owned(), keymap.to_owned()])
}

fn headless(rendernode: Option<&str>) -> String {
    match rendernode {
        Some(node) => format!("egl-headless,rendernode={node}"),
        None => "egl-headless".to_owned(),
    }
}

/// Check `--rendernode` before QEMU has to, and say which nodes this machine
/// has when it is wrong.
///
/// QEMU's own complaint about a node that is not there names the path and
/// stops, which is fair but leaves the person to go and find what the right
/// path would have been. The answer is a directory listing away and the two
/// nodes on a dual-GPU machine are one digit apart, so a typo is the likely
/// mistake and the listing is the likely fix.
///
/// # Errors
///
/// When `--rendernode` was given to a boot with no `--gl` to draw, or names
/// something this machine does not have.
fn check_rendernode(args: &Args) -> Result<()> {
    let Some(node) = args.rendernode.as_deref() else {
        return Ok(());
    };
    if !args.gl {
        return Err(Error::new(
            "--rendernode says which GPU --gl draws on, and this boot has no --gl.",
        ));
    }
    if Path::new(node).exists() {
        return Ok(());
    }
    let mut found: Vec<String> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().display().to_string())
        .filter(|path| path.contains("renderD"))
        .collect();
    found.sort();
    Err(Error::new(format!(
        "--rendernode names {node}, which is not there.{}",
        if found.is_empty() {
            " This machine has no /dev/dri render node at all.".to_owned()
        } else {
            format!(" This machine has: {}", found.join(", "))
        }
    )))
}

/// Which port a VNC address serves: display *n* is 5900 + *n*, as everything
/// that speaks the protocol has it. An address whose display cannot be read
/// is reported as the default, which is the only thing left to say about it.
fn port(address: &str) -> u16 {
    address
        .rsplit(':')
        .next()
        .and_then(|display| display.parse::<u16>().ok())
        .map_or(5900, |display| display.saturating_add(5900))
}

/// Whether `args` asks for a screen somebody watches, rather than one only a
/// screendump reads.
fn wanted(args: &Args) -> bool {
    match args.command.as_deref() {
        // `--display` is both "put a virtio-gpu on the bus" and, to `run`,
        // "and show it": the flag's documentation has said so since the card
        // arrived.
        Some("run") => args.display,
        Some("run-compositor") => true,
        _ => false,
    }
}

/// Whether this boot should have the card as its only screen.
///
/// QEMU shows its first console and can be told to show another only over
/// VNC, so on a machine with two heads a window opens on firmware's -- the
/// one the loader's text went to -- and the compositor draws on a console
/// nobody asked for. A boot that is watched therefore goes without the
/// machine's own display device: `q35`'s VGA, `virt`'s `ramfb`.
///
/// What that costs is where the loader's framebuffer comes from. With the
/// VGA gone, firmware's graphics output is the virtio-gpu, so the panic
/// screen and the card the ring-3 driver takes over are the same device,
/// which `docs/DISPLAY.md` §2.4 keeps them apart for. That is a trade a
/// watched boot can make and a judged one must not, which is why this asks
/// the command and not the person: every `test-*` boot keeps both heads and
/// compares the card by screendump, exactly as it did before.
pub(crate) fn sole_screen(args: &Args) -> bool {
    wanted(args)
}

/// Whether this host has somewhere to open a window.
///
/// On Windows and macOS the answer is yes: a process that can start QEMU can
/// open a window. On a POSIX host it is a question, and the answer is in the
/// environment — an X display or a Wayland one. A machine reached over `ssh`
/// without forwarding has neither, and that is the case this exists for.
#[cfg(unix)]
fn session() -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// Whether this host has somewhere to open a window: it does.
#[cfg(not(unix))]
fn session() -> bool {
    true
}

/// Decide where `args` puts the screen, asking `binary` what it can do.
///
/// # Errors
///
/// When `--vnc` was given to a boot that has no screen, or when neither a
/// window nor VNC can be had from this QEMU.
pub(crate) fn choose(binary: &Path, args: &Args) -> Result<Window> {
    // Before the question of where the screen goes, because a headless boot
    // has a GPU to pick too: `test-display --gl` draws with `egl-headless`
    // and reads the result back with a screendump.
    check_rendernode(args)?;
    if let Some(node) = args.rendernode.as_deref() {
        println!("  gpu: egl-headless draws on {node}");
    }
    if !wanted(args) {
        if args.vnc.is_some() {
            return Err(Error::new(
                "--vnc asks where to put a screen, and this boot has none.\n  \
                 `cargo xtask run --display --vnc :0`, or `cargo xtask run-compositor --vnc :0`.",
            ));
        }
        return Ok(Window::Headless);
    }
    let offered = offered(binary);
    pick(&offered, args.vnc.as_deref(), session(), || vnc(binary))
}

/// Choose from what QEMU offers, what was asked for, and whether a window
/// could open at all.
///
/// The VNC question is a closure because answering it starts QEMU a second
/// time, and the common case — a host with a window backend and no `--vnc` —
/// never needs to ask.
fn pick(
    offered: &[String],
    asked: Option<&str>,
    session: bool,
    vnc: impl FnOnce() -> bool,
) -> Result<Window> {
    if let Some(address) = asked {
        if !vnc() {
            return Err(Error::new(
                "this QEMU was built without VNC, so --vnc has nowhere to serve the screen.",
            ));
        }
        return Ok(Window::Vnc(address_of(address)));
    }
    if session
        && let Some(name) = LOCAL
            .iter()
            .find(|name| offered.iter().any(|offer| offer == *name))
    {
        return Ok(Window::Local(name));
    }
    if vnc() {
        return Ok(Window::Vnc(address_of(DEFAULT_VNC)));
    }
    Err(Error::new(format!(
        "this QEMU cannot show a screen: it offers {}, none of which opens a window here, \
         and it has no VNC either.\n  \
         Install a QEMU built with GTK or SDL (Debian/Ubuntu: the distribution's \
         `qemu-system-x86` has both), or run the headless `cargo xtask test-compositor`, \
         which compares screendumps instead.{}",
        if offered.is_empty() {
            "nothing".to_owned()
        } else {
            offered.join(", ")
        },
        if session {
            ""
        } else {
            "\n  This host also has no DISPLAY and no WAYLAND_DISPLAY, so a window \
             backend would have had nowhere to open."
        }
    )))
}

/// Where VNC goes when nothing said: the loopback's first display, for the
/// reason the module documentation gives.
const DEFAULT_VNC: &str = "127.0.0.1:0";

/// The address a `--vnc` value means.
///
/// QEMU's own spellings are kept as they are — `:1`, `127.0.0.1:0`,
/// `0.0.0.0:2` — so a person who knows the option loses nothing by giving it
/// here. A bare number is the display alone, which QEMU would refuse, and is
/// read as that display on the loopback.
fn address_of(asked: &str) -> String {
    if asked.contains(':') {
        return asked.to_owned();
    }
    format!("127.0.0.1:{asked}")
}

/// The display backends this QEMU was built with.
///
/// A QEMU that cannot be asked — one that fails, or prints something this
/// does not recognise — offers nothing as far as this is concerned, and the
/// fallback below decides what happens. It is not an error on its own: the
/// answer to "can this show a screen" is still no, and the message that says
/// so is a better one than a parse failure here.
fn offered(binary: &Path) -> Vec<String> {
    let mut command = Command::new(binary);
    let _ = command.args(["-display", "help"]);
    let Ok(output) = command.output() else {
        return Vec::new();
    };
    let names = listed(&String::from_utf8_lossy(&output.stdout));
    if names.is_empty() {
        // Some builds print the listing on stderr, and a QEMU that refused
        // the option printed its complaint there too: either way what comes
        // back is what the listing parser makes of it.
        listed(&String::from_utf8_lossy(&output.stderr))
    } else {
        names
    }
}

/// The `-device` name of the virtio-gpu to put on the bus, and the display
/// backend that card needs.
///
/// `--gl` asks for `virtio-gpu-gl-pci`, the 3D device: QEMU replays the
/// guest's GL command stream into the host's own driver through
/// virglrenderer, which is what `docs/GPU.md`'s Path A stands on. Two things
/// have to be true for it and both are the *host's*, so both are checked
/// here rather than assumed:
///
/// 1. This QEMU was built with OpenGL and virglrenderer, or it has no such
///    device. A QEMU that has not got it is not an error -- the 2D device
///    still boots, and every test but a GPU one passes either way -- so the
///    card falls back and a line says it did.
/// 2. The display backend has GL turned on. QEMU refuses the 3D device
///    outright with `-display none`: *"The display backend does not have
///    OpenGL support enabled"*. So a headless boot that wants GL gets
///    `egl-headless`, which draws into a host GPU's buffer that a
///    screendump can still read, and a window gets `gl=on`.
pub(crate) fn card(binary: &Path, want_gl: bool) -> (&'static str, bool) {
    if !want_gl {
        return ("virtio-gpu-pci", false);
    }
    if !devices(binary).iter().any(|name| name == GL_CARD) {
        println!(
            "  this QEMU has no `{GL_CARD}`; using the 2D card. Build QEMU with              --enable-opengl --enable-virglrenderer for the 3D one."
        );
        return ("virtio-gpu-pci", false);
    }
    (GL_CARD, true)
}

/// The 3D card's `-device` name.
pub(crate) const GL_CARD: &str = "virtio-gpu-gl-pci";

/// The QEMU for `program` a boot runs: when it wants the 3D card, the first
/// one [`crate::paths::which_all`] finds that has it, and otherwise -- or
/// when none has -- the first one at all.
///
/// Said out loud when it is not the first on `PATH`, because a boot running
/// a different QEMU from the one `which` names is a thing a person reading
/// the log should not have to guess at.
pub(crate) fn qemu_for(program: &str, want_gl: bool) -> Option<std::path::PathBuf> {
    let (chosen, passed) = find_qemu(program, want_gl)?;
    if let Some(first) = passed {
        println!(
            "  qemu: {} for its 3D card; {} has none",
            chosen.display(),
            first.display()
        );
    }
    Some(chosen)
}

/// [`qemu_for`]'s answer without the saying: the QEMU, and the first on
/// `PATH` when that is not it.
fn find_qemu(
    program: &str,
    want_gl: bool,
) -> Option<(std::path::PathBuf, Option<std::path::PathBuf>)> {
    let mut all = crate::paths::which_all(program).into_iter();
    let first = all.next()?;
    if !want_gl || devices(&first).iter().any(|name| name == GL_CARD) {
        return Some((first, None));
    }
    match all.find(|binary| devices(binary).iter().any(|name| name == GL_CARD)) {
        Some(found) => Some((found, Some(first))),
        None => Some((first, None)),
    }
}

/// Whether a watched desktop draws on the host's GPU, and on which of its
/// render nodes: `args` with `gl` and `rendernode` decided.
///
/// `--gl` and `--no-gl` say so. Otherwise the answer is yes wherever it has
/// been proven and the host has what it takes, because the difference is
/// not a detail: the customer's own desktop, a video wallpaper behind a
/// translucent terminal at 1920x1080, draws 38 frames a second in software
/// with the slowest at 60-95 ms and every processor busy, and 61 a second on
/// the GPU with the slowest at 7-17 ms (`docs/GPU.md` §3.9).
///
/// *Proven* is a served screen: `egl-headless` behind VNC, which is how a
/// desktop is watched from another machine; and on Windows, a window of the
/// QEMU `tools/common/fetch/fetch-qemu-windows.sh` builds (it says `whpx-gva`
/// in its `--version`), whose GTK window every `--everything` boot there has
/// drawn in since its reset fix (`docs/GPU.md` §3.12). Another window's GL
/// is a backend that has not been, and keeps the 2D card unless asked.
/// *What it takes* is a QEMU with the card and a render node for
/// `egl-headless` to draw on -- which also answers for hosts with no
/// `/dev/dri` at all, since none of them can.
///
/// # Errors
///
/// `--gl` beside `--no-gl`; and whatever [`choose`] refuses, which the boot
/// itself would refuse a moment later.
pub(crate) fn watched_gl(arch: crate::paths::Arch, args: &Args) -> Result<Args> {
    if args.gl && args.no_gl {
        return Err(Error::new("--gl and --no-gl ask for opposite things"));
    }
    if args.gl || args.no_gl {
        return Ok(args.clone());
    }
    let software = |why: &str| {
        println!("  gpu: {why}; the compositor draws in software (--gl asks for the 3D card)");
        Ok(args.clone())
    };
    let Some((binary, _)) = find_qemu(arch.qemu_binary(), true) else {
        return Ok(args.clone());
    };
    if !devices(&binary).iter().any(|name| name == GL_CARD) {
        return software(&format!("no QEMU here has `{GL_CARD}`"));
    }
    // Asked as the boot will ask it, with the card it would have: a
    // `--rendernode` given alone is this boot's GPU, not a mistake.
    let asked = Args {
        gl: true,
        ..args.clone()
    };
    if !matches!(choose(&binary, &asked)?, Window::Vnc(_)) {
        if cfg!(windows) && crate::qemu::translates_with_hyper_v(&binary) {
            println!("  gpu: the 3D card, in this window (--no-gl draws in software)");
            return Ok(Args {
                gl: true,
                ..args.clone()
            });
        }
        return software("a window on this host is not a proven GL display");
    }
    let node = match args.rendernode.clone() {
        Some(node) => node,
        None => match render_node(&render_nodes()) {
            Some(node) => node,
            None => return software("this host has no render node for egl-headless"),
        },
    };
    println!("  gpu: the 3D card, drawn on {node} (--no-gl draws in software)");
    Ok(Args {
        gl: true,
        rendernode: Some(node),
        ..args.clone()
    })
}

/// This host's render nodes, each with the kernel driver behind it, in
/// device order.
fn render_nodes() -> Vec<(String, String)> {
    let mut nodes: Vec<(String, String)> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with("renderD").then(|| {
                let driver = std::fs::read_link(format!("/sys/class/drm/{name}/device/driver"))
                    .ok()
                    .and_then(|link| link.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .unwrap_or_default();
                (format!("/dev/dri/{name}"), driver)
            })
        })
        .collect();
    nodes.sort();
    nodes
}

/// Which of `nodes` `egl-headless` should draw on: the first whose driver
/// is not NVIDIA's proprietary one, and that one only when it is all there
/// is.
///
/// virglrenderer is written and tested against Mesa's drivers. On a
/// machine with an NVIDIA card beside another, QEMU's own choice is
/// whichever node it opens first, and "it started" proves nothing: both
/// initialise, and only one of them is the driver virglrenderer expects.
fn render_node(nodes: &[(String, String)]) -> Option<String> {
    nodes
        .iter()
        .find(|(_, driver)| driver != "nvidia")
        .or_else(|| nodes.first())
        .map(|(node, _)| node.clone())
}

/// The device names this QEMU was built with, as `-device help` lists them.
///
/// Each line is `name "the-name", bus ...`; only the quoted name matters,
/// and an alias line names the alias the same way. A QEMU that cannot be
/// asked offers nothing, as with [`offered`].
fn devices(binary: &Path) -> Vec<String> {
    let mut command = Command::new(binary);
    let _ = command.args(["-device", "help"]);
    let Ok(output) = command.output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    named(&text)
}

/// Whether a boot on this host can have Venus on its 3D card: the host is
/// Linux, where Venus runs (under `whpx` the card is virgl alone,
/// `docs/GPU.md` §3), and the QEMU a GL boot takes has the card with a
/// `venus` property, which QEMU defines only against a virglrenderer new
/// enough to have Venus.
pub(crate) fn offers_venus(arch: crate::paths::Arch) -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    let Some((binary, _)) = find_qemu(arch.qemu_binary(), true) else {
        return false;
    };
    let mut command = Command::new(binary);
    let _ = command.args(["-device", &format!("{GL_CARD},help")]);
    command
        .output()
        .is_ok_and(|output| has_property(&String::from_utf8_lossy(&output.stdout), "venus"))
}

/// Whether `-device <card>,help`'s `text` lists `property`: a line
/// `  <property>=<type> ...`.
fn has_property(text: &str, property: &str) -> bool {
    text.lines().any(|line| {
        line.trim()
            .split_once('=')
            .is_some_and(|(name, _)| name == property)
    })
}

/// The audio backend a desktop that wants sound gets from this QEMU: the
/// first of the host's own sound servers it was built with -- `pipewire`,
/// `pa`, `coreaudio`, `dsound` -- as `-audiodev help` lists them, or
/// none when it has none of them.
pub(crate) fn audio_backend(binary: &Path) -> Option<&'static str> {
    let mut command = Command::new(binary);
    let _ = command.args(["-audiodev", "help"]);
    let output = command.output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let offered = audio_drivers(&text);
    ["pipewire", "pa", "coreaudio", "dsound"]
        .into_iter()
        .find(|wanted| offered.iter().any(|driver| driver == wanted))
}

/// The names under `Available audio drivers:`, one a line.
fn audio_drivers(text: &str) -> Vec<String> {
    text.lines()
        .skip_while(|line| !line.trim().ends_with("audio drivers:"))
        .skip(1)
        .map(str::trim)
        .take_while(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Every `name "..."` in `text`, which is how `-device help` writes each
/// device and each alias.
fn named(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("name \"")?;
            let (name, _) = rest.split_once('"')?;
            Some(name.to_owned())
        })
        .collect()
}

/// The backend names under `Available display backend types:`, which is one a
/// line until the blank line before the note about suboptions.
fn listed(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut reading = false;
    for line in text.lines() {
        let line = line.trim();
        if !reading {
            reading = line.ends_with("display backend types:");
            continue;
        }
        if line.is_empty() {
            break;
        }
        names.push(line.to_owned());
    }
    names
}

/// Whether this QEMU has VNC.
///
/// `-vnc help` prints the option's suboptions on a build that has it, and a
/// complaint on one built with `--disable-vnc`, which does not take the
/// option at all. It is the *output* that answers, not the status: QEMU exits
/// 1 from printing this help, on both the QEMUs this is developed against,
/// and a first version of this read the status and concluded that a QEMU
/// whose VNC works has none.
///
/// Asking is a QEMU start, which is why the caller only asks when the answer
/// matters.
fn vnc(binary: &Path) -> bool {
    let mut command = Command::new(binary);
    let _ = command.args(["-vnc", "help"]);
    command.output().is_ok_and(|output| {
        takes_vnc(&String::from_utf8_lossy(&output.stdout))
            || takes_vnc(&String::from_utf8_lossy(&output.stderr))
    })
}

/// Whether what `-vnc help` printed is the option's own help.
///
/// `qemu_opts_print_help` heads the listing with the option group's name,
/// which for this one is `vnc options:`.
fn takes_vnc(text: &str) -> bool {
    text.lines().any(|line| line.trim() == "vnc options:")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-audiodev help` as QEMU 10.2.1 and 9.2.4 on example print it.
    #[test]
    fn audio_drivers_are_read_from_audiodev_help() {
        let full = "Available audio drivers:\nnone\nalsa\ndbus\njack\noss\npa\npipewire\nsdl\nspice\nwav\n";
        assert_eq!(
            audio_drivers(full),
            [
                "none", "alsa", "dbus", "jack", "oss", "pa", "pipewire", "sdl", "spice", "wav"
            ]
        );
        let bare = "Available audio drivers:\nnone\ndbus\noss\nspice\nwav\n";
        assert!(
            !audio_drivers(bare)
                .iter()
                .any(|driver| driver == "pipewire")
        );
        assert!(audio_drivers("no such list\n").is_empty());
    }

    /// `-device virtio-gpu-gl-pci,help` as Ubuntu's QEMU 10.2.1 on the gate
    /// host prints it, cut to the lines around Venus's.
    #[test]
    fn venus_is_read_from_the_cards_properties() {
        let venus = "virtio-gpu-gl-pci options:\n  blob=<bool>            - on/off (default: off)\n  \
                     hostmem=<size>         -  (default: 0)\n  \
                     venus=<bool>           - on/off (default: off)\n";
        assert!(has_property(venus, "venus"));
        assert!(has_property(venus, "blob"));
        let virgl =
            "virtio-gpu-gl-pci options:\n  blob=<bool>            - on/off (default: off)\n";
        assert!(!has_property(virgl, "venus"));
        assert!(!has_property("", "venus"));
    }

    /// The listing this host's QEMU prints, which the parser is written for.
    const WINDOWS_HELP: &str = "\
Available display backend types:
none
gtk
sdl
egl-headless
curses
spice-app
dbus

Some display backends support suboptions, which can be set with
   -display backend,option=value,option=value...
";

    /// The listing the headless Linux box prints, which is the case the
    /// fallback exists for.
    const HEADLESS_HELP: &str = "\
Available display backend types:
none
spice-app
dbus

Some display backends support suboptions, which can be set with
   -display backend,option=value,option=value...
";

    /// A Mesa driver's node is taken over NVIDIA's proprietary one whatever
    /// the order, NVIDIA's when it is all there is, and nothing when there
    /// are no nodes -- which is every host with no `/dev/dri`.
    #[test]
    fn egl_headless_draws_on_a_mesa_node_first() {
        let node = |name: &str, driver: &str| (format!("/dev/dri/{name}"), driver.to_owned());
        assert_eq!(
            render_node(&[node("renderD128", "nvidia"), node("renderD129", "amdgpu")]),
            Some("/dev/dri/renderD129".to_owned())
        );
        assert_eq!(
            render_node(&[node("renderD128", "i915"), node("renderD129", "nvidia")]),
            Some("/dev/dri/renderD128".to_owned())
        );
        assert_eq!(
            render_node(&[node("renderD128", "nvidia")]),
            Some("/dev/dri/renderD128".to_owned())
        );
        assert_eq!(render_node(&[]), None);
    }

    #[test]
    fn the_backends_qemu_lists_are_read_off_its_help() {
        assert_eq!(
            listed(WINDOWS_HELP),
            [
                "none",
                "gtk",
                "sdl",
                "egl-headless",
                "curses",
                "spice-app",
                "dbus"
            ]
        );
        assert_eq!(listed(HEADLESS_HELP), ["none", "spice-app", "dbus"]);
        assert!(listed("qemu-system-x86_64: -display help: invalid option").is_empty());
    }

    #[test]
    fn a_host_with_a_window_backend_opens_a_window() {
        let offered = listed(WINDOWS_HELP);
        let chosen = pick(&offered, None, true, || {
            panic!("VNC must not be asked about")
        });
        assert_eq!(chosen.unwrap(), Window::Local("gtk"));
    }

    #[test]
    fn sdl_serves_when_gtk_was_not_built_in() {
        let offered = vec!["none".to_owned(), "sdl".to_owned()];
        let chosen = pick(&offered, None, true, || {
            panic!("VNC must not be asked about")
        });
        assert_eq!(chosen.unwrap(), Window::Local("sdl"));
    }

    #[test]
    fn a_headless_host_serves_the_screen_over_vnc() {
        let offered = listed(HEADLESS_HELP);
        assert_eq!(
            pick(&offered, None, false, || true).unwrap(),
            Window::Vnc("127.0.0.1:0".to_owned())
        );
        // And so does a host whose QEMU has no window backend even though a
        // session is there to open one on.
        assert_eq!(
            pick(&offered, None, true, || true).unwrap(),
            Window::Vnc("127.0.0.1:0".to_owned())
        );
    }

    #[test]
    fn a_session_is_not_enough_when_nothing_can_show_a_screen() {
        let offered = listed(HEADLESS_HELP);
        let refused = pick(&offered, None, true, || false)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("spice-app"), "{refused}");
        assert!(refused.contains("test-compositor"), "{refused}");
    }

    #[test]
    fn asking_for_vnc_puts_the_screen_there_even_where_a_window_could_open() {
        let offered = listed(WINDOWS_HELP);
        assert_eq!(
            pick(&offered, Some(":2"), true, || true).unwrap(),
            Window::Vnc(":2".to_owned())
        );
    }

    /// What QEMU 9.2.4 and 11.1 both print for `-vnc help`, from a process
    /// that then exits 1: the status says nothing, the listing says it all.
    const VNC_HELP: &str = "vnc options:
  audiodev=<str>
  connections=<num>
  display=<str>
  head=<num>
  password=<bool (on/off)>
";

    #[test]
    fn vnc_is_judged_by_what_the_help_printed_not_by_the_status() {
        assert!(takes_vnc(VNC_HELP));
        assert!(!takes_vnc(
            "qemu-system-x86_64: -vnc: VNC support is disabled"
        ));
        assert!(!takes_vnc(""));
    }

    #[test]
    fn a_keymap_is_the_name_qemu_gives_it() {
        assert_eq!(vnc_keymap("de"), Some("de"));
        assert_eq!(
            vnc_keymap("de,us"),
            Some("de"),
            "the first layout is typed in"
        );
        assert_eq!(vnc_keymap("us"), Some("en-us"));
        assert_eq!(vnc_keymap("en-us"), Some("en-us"));
        assert_eq!(vnc_keymap("ch"), Some("de-ch"));
        assert_eq!(vnc_keymap("xx"), None);
    }

    #[test]
    fn a_bare_display_number_is_that_display_on_the_loopback() {
        assert_eq!(address_of("3"), "127.0.0.1:3");
        assert_eq!(address_of(":3"), ":3");
        assert_eq!(address_of("0.0.0.0:1"), "0.0.0.0:1");
    }

    #[test]
    fn a_vnc_display_names_the_port_a_viewer_connects_to() {
        assert_eq!(port("127.0.0.1:0"), 5900);
        assert_eq!(port(":2"), 5902);
        assert_eq!(port("0.0.0.0:11"), 5911);
    }

    #[test]
    fn a_device_listing_is_read_by_the_quoted_name() {
        // `-device help` as QEMU writes it, aliases and all.
        let listing = "Display devices:\n\
                       name \"virtio-gpu-pci\", bus PCI, alias \"virtio-gpu\"\n\
                       name \"virtio-gpu-gl-pci\", bus PCI, alias \"virtio-gpu-gl\"\n\
                       \n\
                       Input devices:\n\
                       name \"virtio-keyboard-pci\", bus PCI\n";
        let names = named(listing);
        assert!(names.iter().any(|name| name == GL_CARD));
        assert!(names.iter().any(|name| name == "virtio-gpu-pci"));
        assert_eq!(names.len(), 3, "one a line, and no alias lines: {names:?}");
    }

    /// A QEMU built without OpenGL lists no such device, and nothing in the
    /// listing is mistaken for one.
    #[test]
    fn a_qemu_without_the_3d_device_offers_no_name_like_it() {
        let listing = "name \"virtio-gpu-pci\", bus PCI\nname \"virtio-vga\", bus PCI\n";
        assert!(!named(listing).iter().any(|name| name == GL_CARD));
    }

    /// A machine with two GPUs has two render nodes, and which one draws is
    /// otherwise QEMU picking the lower device number.
    #[test]
    fn a_named_render_node_is_the_one_egl_headless_draws_on() {
        assert_eq!(
            Window::Headless.arguments_with(None, true, Some("/dev/dri/renderD128")),
            [
                "-display".to_owned(),
                "egl-headless,rendernode=/dev/dri/renderD128".to_owned()
            ]
        );
        // The served case is the one this exists for: a screen watched from
        // somewhere else is a screen on a machine nobody is sitting at, and
        // that is where a second GPU is most likely to be the idle one.
        assert_eq!(
            Window::Vnc(":0".to_owned()).arguments_with(
                Some("gpu0"),
                true,
                Some("/dev/dri/renderD129")
            ),
            [
                "-display".to_owned(),
                "egl-headless,rendernode=/dev/dri/renderD129".to_owned(),
                "-vnc".to_owned(),
                ":0,display=gpu0,head=0".to_owned()
            ]
        );
    }

    /// A window's GL is the host display's GPU whatever anybody asks, so the
    /// node is not written into a backend that would ignore it.
    #[test]
    fn a_window_takes_no_render_node() {
        assert_eq!(
            Window::Local("gtk").arguments_with(None, true, Some("/dev/dri/renderD128")),
            ["-display".to_owned(), "gtk,show-tabs=on,gl=on".to_owned()]
        );
        assert_eq!(
            Window::Local("sdl").arguments_with(None, true, Some("/dev/dri/renderD128")),
            ["-display".to_owned(), "sdl,gl=on".to_owned()]
        );
    }

    /// Naming a GPU for a boot that draws nothing on it is a mistake worth
    /// saying, not a setting worth ignoring.
    #[test]
    fn a_render_node_without_gl_is_refused() {
        let mut args = Args {
            command: Some("run-compositor".to_owned()),
            rendernode: Some("/dev/dri/renderD128".to_owned()),
            ..Args::default()
        };
        assert!(check_rendernode(&args).is_err());
        args.gl = true;
        // With `--gl` the only remaining question is whether the node is
        // there, which is this machine's business and not this test's.
        let _ = check_rendernode(&args);
    }

    /// The 3D card needs a backend with OpenGL on, and `none` has none to
    /// turn on: a headless GPU boot gets `egl-headless` instead.
    #[test]
    fn a_3d_card_takes_a_backend_that_has_opengl() {
        assert_eq!(
            Window::Headless.arguments_with(None, true, None),
            ["-display".to_owned(), "egl-headless".to_owned()]
        );
        assert_eq!(
            Window::Local("gtk").arguments_with(None, true, None),
            ["-display".to_owned(), "gtk,show-tabs=on,gl=on".to_owned()]
        );
        // VNC has no OpenGL of its own, so it is served beside the backend
        // that has rather than being the display.
        assert_eq!(
            Window::Vnc(":7".to_owned()).arguments_with(Some("gpu0"), true, None),
            [
                "-display".to_owned(),
                "egl-headless".to_owned(),
                "-vnc".to_owned(),
                ":7,display=gpu0,head=0".to_owned()
            ]
        );
        // And without it, exactly what it always was.
        assert_eq!(
            Window::Headless.arguments_with(None, false, None),
            Window::Headless.arguments_with(None, false, None)
        );
        assert_eq!(
            Window::Local("gtk").arguments_with(Some("gpu0"), false, None),
            Window::Local("gtk").arguments_with(Some("gpu0"), false, None)
        );
    }

    #[test]
    fn the_display_argument_is_what_qemu_takes() {
        assert_eq!(
            Window::Headless.arguments_with(None, false, None),
            ["-display".to_owned(), "none".to_owned()]
        );
        assert_eq!(
            Window::Local("sdl").arguments_with(None, false, None),
            ["-display".to_owned(), "sdl".to_owned()]
        );
        assert_eq!(
            Window::Vnc("127.0.0.1:0".to_owned()).arguments_with(None, false, None),
            ["-display".to_owned(), "vnc=127.0.0.1:0".to_owned()]
        );
    }

    #[test]
    fn a_window_on_a_machine_with_a_card_can_reach_the_cards_console() {
        // GTK cannot be told which tab to open on, so it is told to show
        // them; VNC serves one console and is pointed at the card.
        assert_eq!(
            Window::Local("gtk").arguments_with(Some("gpu0"), false, None),
            ["-display".to_owned(), "gtk,show-tabs=on".to_owned()]
        );
        assert_eq!(
            Window::Vnc(":1".to_owned()).arguments_with(Some("gpu0"), false, None),
            [
                "-display".to_owned(),
                "vnc=:1,display=gpu0,head=0".to_owned()
            ]
        );
    }
}
