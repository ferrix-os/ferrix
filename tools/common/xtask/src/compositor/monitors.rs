//! The boots about the screen the compositor is given: two monitors, a
//! monitor at scale 2, a mode the configuration asks for, a monitor stood on
//! its edge, and a monitor that describes itself by its own EDID.
//!
//! What they share is the `monitor =` line. Each one's picture is a screen of
//! the size and shape that line says, and where `hyprctl monitors` is asked
//! from inside the guest, it has to say the same.

use std::path::Path;

use super::boot::{Wanted, boot_and_dump, boot_and_dump_carrying, said_on_its_own};
use super::drawing::one_picture;
use super::pointer::{POINTER_AT, POINTER_EXPECTED};
use super::{Carried, Programs};
use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result};

/// The picture the mode boot requires: the two windows on a screen of the
/// size its `monitor =` line asked for.
const MODE_EXPECTED: [(&str, &str); 1] = [(
    "tiled on a 1920x1080 screen, which is the mode the configuration asked for",
    "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients-1920x1080.xrle",
)];

/// The configuration the mode boot is given. The card prefers 1024x768, as
/// every judged boot's does, and this asks for a size it only lists.
const MODE_CONFIG: &str = "# Carried into the initramfs by `cargo xtask test-compositor`.
monitor = , 1920x1080@60, auto, 1
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
";

/// The pictures the transform boot requires, one a boot: the two windows on
/// a monitor stood on its edge, as the connector's buffer holds them --
/// which is what QEMU's screendump reads, since QEMU's window is the
/// connector and knows nothing of how the monitor stands.
const TRANSFORM_EXPECTED: [(u32, (&str, &str)); 2] = [
    (
        1,
        (
            "tiled on a monitor turned clockwise onto its edge, the picture turned \
             counter-clockwise into the buffer",
            "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients-transform-1.xrle",
        ),
    ),
    (
        3,
        (
            "tiled on a monitor turned counter-clockwise onto its edge, the picture turned \
             clockwise into the buffer",
            "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients-transform-3.xrle",
        ),
    ),
];

/// The configuration the transform boot is given for `transform`: the
/// monitor turned, `hyprctl monitors` asked once so the transcript says what
/// it reports, and the two windows.
fn transform_config(transform: u32) -> String {
    format!(
        "# Carried into the initramfs by `cargo xtask test-compositor --boot transform`.
monitor = , preferred, auto, 1, transform, {transform}
exec-once = /bin/hyprctl monitors
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
"
    )
}

/// The two pictures the two-monitor boot requires, one a screen: the
/// windows tiled on the first, then the gradient alone on the second with
/// the checkerboard alone on the first.
const MONITOR_EXPECTED: [(&str, &str); 2] = [
    (
        "tiled",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
    (
        "the checkerboard alone on the first monitor",
        "src/user/system/linux/compositor/render/tests/data/two-monitors-left.xrle",
    ),
];

/// What the second screen must show once the keybind has been pressed.
const MONITOR_OTHERS: [(&str, &str); 1] = [(
    "the gradient alone on the second monitor",
    "src/user/system/linux/compositor/render/tests/data/two-monitors-right.xrle",
)];

/// The keybind the two-monitor boot presses between its two pictures.
const MONITOR_BINDS: [(&str, &[&str]); 1] = [("SUPER M", &["meta_l", "m"])];

/// The configuration the fifth boot is given: two monitors, and a keybind
/// that sends the focused window to the second one.
pub(super) const MONITOR_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
exec-once = /bin/hyprctl subscribe
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
bind = SUPER, M, movewindow, mon:1
# Both answers from one press, because the presses are the ones every boot
# makes: the monitors, and the clients whose last line is what the wait for
# the answers looks for.
bind = SUPER, C, exec, /bin/hyprctl --batch monitors ; clients
bind = SUPER, W, exec, /bin/hyprctl activewindow
";

/// The picture a scaled monitor makes, which the sixth boot requires.
const SCALED_EXPECTED: (&str, &str) = (
    "every logical pixel drawn as two on a monitor at scale 2",
    "src/user/system/linux/compositor/render/tests/data/scaled-two-clients.xrle",
);

/// The configuration the sixth boot is given: one monitor at scale 2, where
/// the windows tile in 512x384 logical pixels and are drawn as 1024x768.
///
/// The clients read `wl_output.scale` and send buffers twice the size, which
/// is what a client on a scaled monitor does and what the expected image is
/// blessed with.
const SCALED_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
monitor = , preferred, auto, 2
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
";

/// A fifth boot: two monitors, which on QEMU are two virtio-gpu devices and
/// so two cards in the guest.
///
/// The keybind sends the focused window to the second monitor, and each
/// screen is then required to be the picture `src/user/system/linux/compositor/render`'s own tests
/// bless for it: one window each, neither monitor drawing the other's.
pub(super) fn test_monitors(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        MONITOR_CONFIG,
        &Wanted {
            states: &MONITOR_EXPECTED,
            others: &MONITOR_OTHERS,
            moving: None,
            pointer: None,
            awaiting: &[],
        },
        &MONITOR_BINDS,
        args,
    )?;
    if screens.len() != MONITOR_EXPECTED.len() + MONITOR_OTHERS.len() {
        return Err(Error::new(format!(
            "{arch}: {} of {} pictures were taken",
            screens.len(),
            MONITOR_EXPECTED.len() + MONITOR_OTHERS.len()
        )));
    }
    // The two monitors are two pictures: a compositor drawing the same frame
    // on both screens would match one of them and not the other, and this is
    // what says so out loud.
    if let (Some(left), Some(right)) = (screens.get(1), screens.get(2))
        && left.pixels == right.pixels
    {
        return Err(Error::new(format!(
            "{arch}: both monitors show the same picture"
        )));
    }
    monitors_were_said(arch, &said)
}

/// The EDID boot's configuration for a monitor that describes itself as
/// `description`: the lines a configuration written for several monitors
/// has -- this one by its description at `2560x0`, as the customer's middle
/// monitor is, then a catch-all -- and the pointer boot's windows and
/// software pointer, with the two `hyprctl` binds every boot presses.
fn edid_config(description: &str) -> String {
    format!(
        "\
# Carried into the initramfs by `cargo xtask test-compositor --boot edid`.
monitor = desc:{description}, preferred, 2560x0, 1
monitor = , preferred, auto, 1
cursor:no_hardware_cursors = 1
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
bind = SUPER, C, exec, /bin/hyprctl --batch monitors ; clients
bind = SUPER, W, exec, /bin/hyprctl activewindow
"
    )
}

/// The key the EDID boot presses between its two pictures: the monitors,
/// asked for while the pointer is still unused.
const EDID_BINDS: [(&str, &[&str]); 1] = [("SUPER C", &["meta_l", "c"])];

/// A boot with a monitor's EDID on the screen: `drm.edid_firmware=`
/// (`docs/DISPLAY.md` §7).
///
/// The monitor is `run-compositor`'s default, read from this machine as
/// `run-compositor` reads it, or where this machine has no such monitor a
/// stand-in EDID `crate::edid` makes, so the boot runs anywhere. What is
/// required: the kernel read the file for `Virtual-1`; `hyprctl monitors`
/// gives the screen the monitor's description, and the position the
/// `monitor = desc:` line gives it rather than the catch-all's after it; and
/// on that screen, 2560 pixels from the layout's corner with nothing left of
/// it, the pointer boot's two pictures to the pixel -- the windows tiled,
/// then the arrow where the tablet put it, which a pointer mapped over the
/// empty space left of the screen would have put somewhere else.
pub(super) fn test_edid(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let registry = std::fs::read_to_string(crate::edid::REGISTRY).ok();
    let found = crate::edid::find(
        Path::new(crate::edid::SYSFS_DRM),
        crate::edid::DEFAULT_MONITOR,
        registry.as_deref(),
    )?;
    let monitor = match found {
        Some(monitor) => {
            println!(
                "  {arch}: the EDID of this machine's {} ({})",
                monitor.description, monitor.connector
            );
            monitor
        }
        None => {
            let mut monitor = crate::edid::stand_in();
            // Described with the registry the guest is given, as the
            // compositor will describe it.
            if let Some(described) = crate::edid::describe(&monitor.bytes, registry.as_deref()) {
                monitor.description = described;
            }
            println!(
                "  {arch}: no \"{}\" on this machine; a stand-in EDID, {}",
                crate::edid::DEFAULT_MONITOR,
                monitor.description
            );
            monitor
        }
    };
    let edid = crate::edid::carry(&monitor, registry);
    let carried = Carried {
        ports: edid.files,
        ..Carried::none()
    };
    let (_, said) = boot_and_dump_carrying(
        arch,
        programs,
        &edid_config(&edid.description),
        (carried, Some(&edid.argument)),
        &Wanted {
            states: &POINTER_EXPECTED,
            others: &[],
            moving: None,
            pointer: Some(POINTER_AT),
            awaiting: &[],
        },
        &EDID_BINDS,
        args,
    )?;
    judge_edid(arch, &said, &edid.description)
}

/// What [`test_edid`] requires of what the guest said.
fn judge_edid(arch: Arch, said: &[String], description: &str) -> Result<()> {
    let has = |wanted: &str| said.iter().any(|line| line.contains(wanted));
    let described = format!("description: {description}");
    for (wanted, what) in [
        (
            "card0 Virtual-1: EDID from",
            "the kernel never said it read the EDID",
        ),
        (
            "Monitor Virtual-1 (ID 0)",
            "`hyprctl monitors` never named the screen",
        ),
        (
            described.as_str(),
            "`hyprctl monitors` did not give the monitor's description",
        ),
        (
            " at 2560x0",
            "the screen is not where its `monitor = desc:` line puts it",
        ),
    ] {
        if !has(wanted) {
            return Err(Error::new(format!("{arch}: {what}: no `{wanted}`")));
        }
    }
    println!(
        "  {arch}: the screen is \"{description} (Virtual-1)\" at 2560x0, as its \
         `monitor = desc:` line has it, and the pointer lands where the tablet puts it there"
    );
    Ok(())
}

/// The sixth boot: a monitor at `scale = 2`.
pub(super) fn test_scale(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    one_picture(
        arch,
        programs,
        args,
        "a monitor at scale 2, tiling in logical pixels and drawing in the screen's own",
        SCALED_CONFIG,
        SCALED_EXPECTED,
    )
}

/// A boot for `monitor = , WIDTHxHEIGHT`: the mode a configuration asks for
/// is the mode the screen is set to.
///
/// Under a window QEMU has just opened a virtio-gpu prefers 640x480,
/// whatever it was started with, so a desktop somebody watches is the size
/// of a postage stamp unless its configuration can say otherwise. The
/// kernel lists the standard sizes beside the preferred one, the compositor
/// takes the one its `monitor =` line names, and what is required here is
/// the whole of that: a picture 1920 by 1080, of windows tiled for a screen
/// that size, from a card that prefers 1024x768. A compositor that set the
/// preferred mode anyway shows a picture of another size, and the
/// comparison says so.
pub(super) fn test_mode(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        MODE_CONFIG,
        &Wanted {
            states: &MODE_EXPECTED,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &[],
        },
        &[],
        args,
    )?;
    let Some(screen) = screens.first() else {
        return Err(Error::new(format!("{arch}: the mode boot took no picture")));
    };
    if !said.iter().any(|line| line.contains("1920x1080")) {
        return Err(Error::new(format!(
            "{arch}: the compositor never said its screen is 1920x1080"
        )));
    }
    println!(
        "  {arch}: the screen took the mode its `monitor =` line asked for, {}x{}, from a card \
         that prefers 1024x768, every one of {} pixels",
        screen.width,
        screen.height,
        screen.width * screen.height
    );
    Ok(())
}

/// The pictures the modes boot requires, in turn: the card's preferred
/// 1024x768, then 1920x1080 once `hyprctl keyword monitor` asks for it.
///
/// No third picture after the reload: two windows resized at once a second
/// time show the first window's pixels in the second's place, with or
/// without a mode change (`docs/BACKLOG.md`, the row filed with this boot),
/// so the reload's switch is judged by its lines until that is fixed.
const MODES_EXPECTED: [(&str, &str); 2] = [
    (
        "tiled on the card's preferred 1024x768",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
    (
        "tiled again on 1920x1080, which `hyprctl keyword monitor` asked for while it ran",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients-1920x1080.xrle",
    ),
];

/// The modes boot's configuration: the card's preferred mode, a key for
/// `hyprctl keyword monitor`, and the transcript key every boot presses at
/// the end made to reload first, which reads the file (`preferred`) and
/// drops the keyword, as Hyprland's reload does.
const MODES_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --boot modes`.
monitor = , preferred, auto, 1
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
bind = SUPER, F, exec, /bin/hyprctl keyword monitor ,1920x1080@60,auto,1
bind = SUPER, C, exec, /bin/hyprctl --batch reload ; monitors ; clients
bind = SUPER, W, exec, /bin/hyprctl activewindow
";

/// The key the modes boot presses before its second picture.
const MODES_BINDS: [(&str, &[&str]); 1] = [("SUPER F", &["meta_l", "f"])];

/// A boot for switching modes while the desktop runs (`docs/DISPLAY.md`
/// §9): the card lists the modes of QEMU's EDID (displayctl v11's
/// `MODE_LIST`), `hyprctl keyword monitor` sets one of them, and `hyprctl
/// reload` sets the file's again. Each picture is the renderer's own for
/// that screen size, every pixel, so a mode set without the layout
/// following -- or a layout without the mode -- fails it. `hyprctl
/// monitors` must list `availableModes` as Hyprland does, the 1920x1080 the
/// keyword took among them, and hyprix must have said each switch.
pub(super) fn test_modes(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        MODES_CONFIG,
        &Wanted {
            states: &MODES_EXPECTED,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &["availableModes:", "is 1024x768@"],
        },
        &MODES_BINDS,
        args,
    )?;
    if screens.len() != MODES_EXPECTED.len() {
        return Err(Error::new(format!(
            "{arch}: {} of {} pictures were taken",
            screens.len(),
            MODES_EXPECTED.len()
        )));
    }
    let has = |wanted: &str| said.iter().any(|line| line.contains(wanted));
    for (wanted, what) in [
        (
            "modes listed, 1024x768",
            "the kernel's line for the driver's mode list",
        ),
        ("is 1920x1080@", "hyprix's line for the keyword's switch"),
        ("is 1024x768@", "hyprix's line for the reload's switch"),
        (
            "1920x1080@60.00Hz",
            "`availableModes` listing the 1920x1080 the keyword took",
        ),
    ] {
        if !has(wanted) {
            return Err(Error::new(format!("{arch}: {what}: no `{wanted}`")));
        }
    }
    let listed = said
        .iter()
        .find(|line| line.contains("availableModes:"))
        .map_or(0, |line| line.matches("Hz").count());
    if listed < 10 {
        return Err(Error::new(format!(
            "{arch}: `hyprctl monitors` listed {listed} available modes, not the EDID's"
        )));
    }
    println!(
        "  {arch}: the desktop switched 1024x768 -> 1920x1080 by keyword and back by reload \
         while it ran, and `hyprctl monitors` listed {listed} modes"
    );
    Ok(())
}

/// A boot for `monitor = , preferred, auto, 1, transform, N`: a monitor
/// stood on its edge, twice -- turned one way, then the other.
///
/// The card's mode stays 1024x768, and QEMU's screendump reads the card: so
/// what is required is the buffer Hyprland would scan out for the same line,
/// the windows tiled on a monitor 768 wide and 1024 tall and the picture
/// turned into the buffer, pixel for pixel as `src/user/system/linux/compositor/render` blesses
/// it. Both quarter turns, because each is the other upside down and a
/// compositor that turned the wrong way would pass one of them by drawing
/// the other; and `hyprctl monitors` from inside the guest has to say
/// `transform: N` beside the connector's own, unturned, mode, which is what
/// Hyprland prints.
pub(super) fn test_transform(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    for (transform, wanted) in TRANSFORM_EXPECTED {
        let said_transform = format!("transform: {transform}");
        let (screens, said) = boot_and_dump(
            arch,
            programs,
            &transform_config(transform),
            &Wanted {
                states: &[wanted],
                others: &[],
                moving: None,
                pointer: None,
                awaiting: &[said_transform.as_str()],
            },
            &[],
            args,
        )?;
        let Some(screen) = screens.first() else {
            return Err(Error::new(format!(
                "{arch}: the transform {transform} boot took no picture"
            )));
        };
        let told = |wanted: &str| said.iter().any(|line| said_on_its_own(line) == wanted);
        if !told(&said_transform) || !said.iter().any(|line| line.contains("1024x768@")) {
            return Err(Error::new(format!(
                "{arch}: `hyprctl monitors` did not say `{said_transform}` beside the 1024x768 \
                 mode"
            )));
        }
        println!(
            "  {arch}: transform {transform}: `hyprctl monitors` said `{said_transform}` and the \
             screen is the turned picture, every one of {} pixels",
            screen.width * screen.height
        );
    }
    Ok(())
}

/// What the two-monitor boot's `hyprctl` and event socket must have said:
/// two monitors by name, and the window's move between them.
fn monitors_were_said(arch: Arch, said: &[String]) -> Result<()> {
    let has = |wanted: &str| said.iter().any(|line| line.contains(wanted));
    for wanted in ["Monitor Virtual-1 (ID 0)", "Monitor Virtual-2 (ID 1)"] {
        if !has(wanted) {
            return Err(Error::new(format!(
                "{arch}: `hyprctl monitors` did not say `{wanted}`"
            )));
        }
    }
    let added = said
        .iter()
        .filter(|line| line.contains("monitoradded>>"))
        .count();
    if added < 2 {
        return Err(Error::new(format!(
            "{arch}: the event socket announced {added} monitors, not two"
        )));
    }
    if !has("movewindow>>") {
        return Err(Error::new(format!(
            "{arch}: nothing on the event socket said the window moved"
        )));
    }
    println!("  {arch}: `hyprctl monitors` named both screens and the socket announced both");
    Ok(())
}
