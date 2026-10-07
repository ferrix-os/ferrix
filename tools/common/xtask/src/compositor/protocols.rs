//! The boots of a client speaking one protocol and the compositor answering
//! it: a bar through `zwlr_layer_shell_v1`, a menu through `xdg_popup`, the
//! screen lock, a screenshot, a taskbar, the clipboard and the primary
//! selection, and a program that types.
//!
//! The pictures say the compositor is still drawing. What proves the
//! protocol is mostly what the clients say they were handed, since several of
//! these change nothing on the screen at all.

use super::Programs;
use super::boot::{Wanted, boot_and_dump, said_on_its_own};
use super::picture::expected;
use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result};

/// The picture a bar and two windows make, which the second boot requires.
const BAR_EXPECTED: (&str, &str) = (
    "a bar across the top with the windows under it",
    "src/user/system/linux/compositor/render/tests/data/layer-bar-two-clients.xrle",
);

/// The picture a window with a menu on it makes, which the sixteenth boot
/// requires.
///
/// Nothing is pressed: the client asks for the popup as soon as its window
/// has drawn, which is when a toolkit would, so what is required is the
/// picture it makes.
const MENU_EXPECTED: [(&str, &str); 1] = [(
    "a menu over the window it hangs off, where the positioner puts it",
    "src/user/system/linux/compositor/render/tests/data/menu-on-a-window.xrle",
)];

/// The configuration the sixteenth boot is given: a window with a menu and
/// a window without one.
///
/// `--menu 200` is what `src/user/system/linux/compositor/render` blesses the picture with, and a
/// number changed here and not there is a picture that cannot match.
const MENU_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
exec-once = /bin/pattern checkerboard one --menu 200
exec-once = /bin/pattern gradient two --after one
";

/// The three pictures the lock boot requires, and the two keys between them.
///
/// The windows, then the lock over them, then the windows again: what
/// `ext-session-lock-v1` is for is that the middle one shows *nothing* of
/// the first, and what says the lock let go is that the third is the first
/// again.
const LOCK_EXPECTED: [(&str, &str); 3] = [
    (
        "tiled",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
    (
        "the lock's own surface over the whole screen, and no window on it",
        "src/user/system/linux/compositor/render/tests/data/locked-screen.xrle",
    ),
    (
        "the windows again, once the lock let go",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
];

/// The keys the lock boot presses. The second is the one that must do
/// nothing: `K` is bound, and a bind that is not `bindl` does not fire while
/// the session is locked.
///
/// No modifier, for the reason `TASKBAR_BINDS` gives.
const LOCK_BINDS: [(&str, &[&str]); 2] = [
    ("L, which locks the screen for four seconds", &["l"]),
    (
        "K, a bind that must not fire while the screen is locked",
        &["k"],
    ),
];

/// The configuration the fifteenth boot is given: two windows and a key
/// that locks the screen.
const LOCK_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
bind = , L, exec, /bin/lock 4
bind = , K, exec, /bin/lswt close one
";

/// The two pictures the screenshot boot requires, and the key between them.
///
/// A screenshot changes nothing on the screen, so both are the tiled pair:
/// what the boot is *for* is the digest the guest prints, which must be the
/// digest of the picture `src/user/system/linux/compositor/render` blesses. The second picture is
/// there so that a compositor which had stopped drawing would still be
/// caught.
const SHOT_EXPECTED: [(&str, &str); 2] = [
    (
        "tiled",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
    (
        "still tiled, with a screenshot taken of it",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
];

/// The key the screenshot boot presses.
///
/// No modifier, for the reason `TASKBAR_BINDS` gives: a modifier reaches the
/// focused client, and this client draws something else when it is sent a
/// key.
const SHOT_BINDS: [(&str, &[&str]); 1] = [("S, which takes a screenshot", &["s"])];

/// The configuration the fourteenth boot is given: two windows and a key
/// that screenshots them.
const SHOT_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
bind = , S, exec, /bin/shot
";

/// The two pictures the taskbar boot requires, and the two keys between
/// them.
///
/// Nothing draws the taskbar: `zwlr_foreign_toplevel_management_v1` is a
/// list and not a surface, and what it does is visible only when a window
/// acts on it. The first key lists the windows, which changes no picture;
/// the second asks one of them to close, which changes the picture to the
/// one window that is left.
const TASKBAR_EXPECTED: [(&str, &str); 3] = [
    (
        "tiled",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
    (
        "still tiled, with a taskbar having listed both windows",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
    (
        "one window left, closed from outside it by the taskbar",
        "src/user/system/linux/compositor/render/tests/data/one-client-alone.xrle",
    ),
];

/// The keys the taskbar boot presses between its pictures.
///
/// No modifier, which matters here and nowhere else. A bind consumes its
/// own key but never the modifier held with it, so `SUPER B` reaches the
/// focused client as a `meta` press -- and `src/user/system/linux/compositor/pattern` draws the
/// *other* pattern for every key it is sent, on purpose, so that a key
/// shows on the screen. It redraws only when it is configured, so in every
/// other boot the change never reaches a frame; this is the one boot that
/// presses a key and then resizes a window, and with `SUPER` in front of
/// them the window that was left drew a checkerboard.
const TASKBAR_BINDS: [(&str, &[&str]); 2] = [
    ("B, which lists the windows", &["b"]),
    ("K, which closes one of them", &["k"]),
];

/// The configuration the thirteenth boot is given: two windows, and a
/// program that reads them through the foreign-toplevel protocol.
const TASKBAR_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
bind = , B, exec, /bin/lswt
bind = , K, exec, /bin/lswt close one
";

/// What the clipboard boot copies, and the picture it makes while it does.
///
/// The two windows are the ordinary tiled pair: the clipboard has nothing to
/// draw, and the picture is there so that a boot which copied and pasted on
/// a compositor that had stopped drawing would still be caught.
const CLIPBOARD_TEXT: &str = "the clipboard went through the compositor";
const CLIPBOARD_EXPECTED: [(&str, &str); 1] = [(
    "the windows tiled while one program copied and another pasted",
    "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
)];

/// The configuration the eleventh boot is given: two windows, a program
/// that copies and a program that pastes.
///
/// Neither `clip` has a window: a Wayland clipboard needs a connection and
/// nothing else. The copy is started first, but the order does not matter --
/// a paste waits for the compositor to say what the selection holds, which
/// is what every `wl-paste` does.
fn clipboard_config() -> String {
    format!(
        "# Carried into the initramfs by `cargo xtask test-compositor`.\n\
         exec-once = /bin/pattern checkerboard one\n\
         exec-once = /bin/pattern gradient two --after one\n\
         exec-once = /bin/clip copy {CLIPBOARD_TEXT}\n\
         exec-once = /bin/clip paste\n\
         exec-once = /bin/clip --primary copy {PRIMARY_TEXT}\n\
         exec-once = /bin/clip --primary paste\n"
    )
}

/// What the same boot puts on the *primary* selection, which is what a
/// middle click pastes.
///
/// Different text from the clipboard's, because the point of having both is
/// that they are two: a compositor that answered a primary paste from the
/// clipboard would pass with the same string and fail with this one.
const PRIMARY_TEXT: &str = "the primary selection is the other one";

/// The configuration the second boot is given: a bar through
/// `zwlr_layer_shell_v1`, and the same two windows.
///
/// A second boot rather than a fourth picture, because a bar changes every
/// picture: the three states above are the stage's exit criterion and are
/// compared against images blessed without one.
pub(super) const BAR_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
exec-once = /bin/pattern checkerboard bar --bar 30
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
bind = , A, exec, /bin/hyprctl --batch binds ; devices ; layers ; cursorpos ; locked
";

/// The keys the bar boot presses: one, which asks for everything `hyprctl`
/// answers about that is not a window.
///
/// No modifier, for the reason `TASKBAR_BINDS` gives.
const BAR_BINDS: [(&str, &[&str]); 1] = [(
    "A, which asks hyprctl for the binds, the devices and the layers",
    &["a"],
)];

/// The bar boot's two pictures, which are the same one: asking `hyprctl`
/// about the compositor must change nothing on the screen.
const BAR_PICTURES: [(&str, &str); 2] = [BAR_EXPECTED, BAR_EXPECTED];

/// The second boot: a bar through `zwlr_layer_shell_v1`, and what
/// `hyprctl` says about everything that is not a window.
///
/// The bar is here because this is the boot that has one: `hyprctl layers`
/// is what a bar reads to find its own surface, and a compositor with no
/// layer surface would answer it with an empty list whatever it did wrong.
/// The binds, the devices, the pointer and the lock are asked for in the
/// same batch, because each is one line and a boot is minutes.
pub(super) fn test_bar(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        BAR_CONFIG,
        &Wanted {
            states: &BAR_PICTURES,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &["Layer level 2 (top)"],
        },
        &BAR_BINDS,
        args,
    )?;
    let Some(screen) = screens.first() else {
        return Err(Error::new(format!("{arch}: the bar boot took no picture")));
    };
    let has = |wanted: &str| said.iter().any(|line| line.contains(wanted));
    for wanted in [
        // `binds`: the one this configuration has, with the letters and the
        // fields Hyprland prints.
        "\tdispatcher: exec",
        "\tkey: A",
        // `devices`: the two QEMU publishes, in their groups.
        "Keyboards:",
        "QEMU Virtio Keyboard",
        "\t\t\tmain: yes",
        // `layers`: the bar, on the level and under the namespace it asked
        // for.
        "Layer level 2 (top)",
        "namespace: pattern-bar",
        // `cursorpos`, which starts in the middle of the screen, and
        // `locked`, which without a session lock protocol is never true.
        "512, 384",
        "false",
    ] {
        if !has(wanted) {
            // The last of what the guest said, which is where the reason is:
            // a batch that stopped at the first unknown request prints
            // nothing after it.
            let tail: Vec<&str> = said
                .iter()
                .rev()
                .take(20)
                .rev()
                .map(|line| said_on_its_own(line))
                .collect();
            return Err(Error::new(format!(
                "{arch}: `hyprctl` never said `{wanted}`; the guest's last lines:\n    {}",
                tail.join("\n    ")
            )));
        }
    }
    println!(
        "  {arch}: a bar reserved its strip and the windows tiled under it, every one of {} \
         pixels, and `hyprctl` named its binds, its devices and the bar's own layer from inside \
         the guest",
        screen.width * screen.height
    );
    Ok(())
}

/// A sixteenth boot: a menu, through `xdg_popup`.
///
/// Every right-click menu, dropdown and tooltip in every toolkit is an
/// `xdg_popup`, and a client that makes one and is never configured waits
/// for ever -- the menu simply does not appear. This boot is a window that
/// asks for one the moment it has drawn, and what is required is the
/// picture: the popup over the window, at the rectangle
/// `xdg_positioner`'s rules put it, which `src/user/system/linux/compositor/render` blesses by
/// calling those same rules.
pub(super) fn test_menu(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        MENU_CONFIG,
        &Wanted {
            states: &MENU_EXPECTED,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &["menu at"],
        },
        &[],
        args,
    )?;
    let Some(screen) = screens.first() else {
        return Err(Error::new(format!("{arch}: the menu boot took no picture")));
    };
    // And the client was told where it was put, which says the configure
    // reached it rather than the picture having come from somewhere else.
    if !said
        .iter()
        .any(|line| line.contains("pattern: one menu at 1,1 200x200"))
    {
        return Err(Error::new(format!(
            "{arch}: the client was never told where its menu is"
        )));
    }
    println!(
        "  {arch}: a window asked for a menu and the compositor placed it, drew it over the \
         window and told the client where it is, in every one of {} pixels",
        screen.width * screen.height
    );
    Ok(())
}

/// A fifteenth boot: the screen lock, through `ext-session-lock-v1`.
///
/// Three pictures: the windows, the lock over them, and the windows again.
/// The middle one is the point -- while the lock is held the compositor
/// draws its surface and *nothing else*, so the screen must be the picture
/// `src/user/system/linux/compositor/render` blesses for a locked screen and not one pixel of
/// either window.
///
/// The second key is the other half. `K` closes a window, and it is pressed
/// while the screen is locked: a bind that is not written `bindl` does not
/// fire then, so both windows have to still be there when the lock lets go.
/// A lock that showed a picture and still let a keybind through would not
/// be one.
pub(super) fn test_lock(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        LOCK_CONFIG,
        &Wanted {
            states: &LOCK_EXPECTED,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &["lock: locked"],
        },
        &LOCK_BINDS,
        args,
    )?;
    if screens.len() != LOCK_EXPECTED.len() {
        return Err(Error::new(format!(
            "{arch}: {} of {} pictures were taken",
            screens.len(),
            LOCK_EXPECTED.len()
        )));
    }
    // The locked screen and the tiled one must be two pictures, which a
    // compositor that ignored the lock would fail here rather than at the
    // comparison above.
    if let (Some(before), Some(locked)) = (screens.first(), screens.get(1))
        && before.pixels == locked.pixels
    {
        return Err(Error::new(format!(
            "{arch}: the locked screen is the picture the windows made"
        )));
    }
    let has = |wanted: &str| said.iter().any(|line| line.contains(wanted));
    for wanted in [
        "hyprix: the session is locked",
        "hyprix: the lock covers 1 screen(s)",
        "hyprix: the session is unlocked",
        "lock: locked 1 screen(s) and unlocked again",
    ] {
        if !has(wanted) {
            return Err(Error::new(format!(
                "{arch}: the lock boot did not say `{wanted}`"
            )));
        }
    }
    // And the bind pressed while the screen was locked did not fire: the
    // window it would have closed is still drawn in the third picture,
    // which is the tiled pair.
    if has("lswt: closed") {
        return Err(Error::new(format!(
            "{arch}: a bind fired while the session was locked"
        )));
    }
    println!(
        "  {arch}: a program locked the screen and every one of its {} pixels was the lock's own \
         picture, a keybind pressed while it was locked did nothing, and the windows came back \
         when it let go",
        screens
            .first()
            .map_or(0, |screen| screen.width * screen.height)
    );
    Ok(())
}

/// A fourteenth boot: a screenshot, through `zwlr_screencopy_v1`.
///
/// The guest takes a picture of its own screen with `/bin/shot` -- which is
/// `grim` without the file format -- and prints its size and a digest of
/// every pixel. What is required is that the digest is the one the expected
/// image has: the screenshot the compositor wrote into a client's shared
/// memory inside the guest is, pixel for pixel, the frame
/// `src/user/system/linux/compositor/render` builds on the host by calling the renderer with
/// rectangles.
///
/// That is a stronger statement than the screendump the other boots make.
/// QEMU's screendump reads the virtio-gpu's scanout; this reads what the
/// compositor handed to a program *through the Wayland protocol*, so a
/// compositor that drew the right thing and answered screencopy with
/// rubbish is caught here and nowhere else.
pub(super) fn test_screenshot(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        SHOT_CONFIG,
        &Wanted {
            states: &SHOT_EXPECTED,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &["shot: "],
        },
        &SHOT_BINDS,
        args,
    )?;
    let Some(screen) = screens.first() else {
        return Err(Error::new(format!(
            "{arch}: the screenshot boot took no picture"
        )));
    };
    if screens.len() != SHOT_EXPECTED.len() {
        return Err(Error::new(format!(
            "{arch}: {} of {} pictures were taken",
            screens.len(),
            SHOT_EXPECTED.len()
        )));
    }
    // The picture the guest must have handed the program: the same expected
    // image every screendump above is compared against, and its size is the
    // screen's, since a screendump of another size would already have
    // failed.
    let want = expected(SHOT_EXPECTED[0].1)?;
    let line = format!(
        "shot: {}x{} {:016x}",
        screen.width,
        screen.height,
        fnv1a(&want)
    );
    if !said.iter().any(|said| said_on_its_own(said) == line) {
        let printed = said
            .iter()
            .map(|said| said_on_its_own(said))
            .filter(|said| said.starts_with("shot: "))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(Error::new(format!(
            "{arch}: the guest's screenshot is not the expected image: it said `{printed}` and \
             the image is `{line}`"
        )));
    }
    println!(
        "  {arch}: a program on the guest took a screenshot through `zwlr_screencopy_v1` and \
         every one of its {} pixels is the one the renderer blesses",
        screen.width * screen.height
    );
    Ok(())
}

/// The two pictures the typing boot requires, and the key between them.
///
/// Nothing a person does closes a window here. `V` starts a program, and
/// that program types `SUPER Q` through `zwp_virtual_keyboard_v1` -- so the
/// key that fires the bind comes from a *client*, on the Wayland socket, and
/// not from a device at all.
const TYPING_EXPECTED: [(&str, &str); 2] = [
    (
        "tiled",
        "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
    ),
    (
        "one window left, closed by a key another program typed",
        "src/user/system/linux/compositor/render/tests/data/one-client-alone.xrle",
    ),
];

/// No modifier, for the reason `TASKBAR_BINDS` gives.
const TYPING_BINDS: [(&str, &[&str]); 1] =
    [("V, which starts a program that types SUPER Q", &["v"])];

/// The configuration the eighteenth boot is given.
///
/// `closewindow` takes one of Hyprland's window expressions rather than a
/// direction, so the key the other program types closes the window *named*
/// `one` -- whichever is focused. Two new things in one picture: a client
/// acting as a keyboard, and a dispatcher that picks a window out by title.
const TYPING_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
bind = , V, exec, /bin/vkbd SUPER Q
bind = SUPER, Q, closewindow, title:^(one)$
";

/// An eighteenth boot: a program that types, through
/// `zwp_virtual_keyboard_v1`.
///
/// `wtype`, `ydotool` and every on-screen keyboard are clients that act as a
/// device: what they report has to reach the seat as a person's input does,
/// keybinds and all. That is the whole point of the protocol and the one
/// thing a test can check from outside -- so this boot presses one key that
/// starts `/bin/vkbd`, and `vkbd` types the chord that fires a bind.
///
/// The bind is `closewindow, title:^(one)$`, which names its window with one
/// of Hyprland's window expressions. So the picture proves two things at
/// once: the key crossed from a client into the seat, and the compositor
/// picked a window out by its title.
pub(super) fn test_typing(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        TYPING_CONFIG,
        &Wanted {
            states: &TYPING_EXPECTED,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &["vkbd: typed"],
        },
        &TYPING_BINDS,
        args,
    )?;
    let Some(last) = screens.last() else {
        return Err(Error::new(format!(
            "{arch}: the typing boot took no picture"
        )));
    };
    for wanted in [
        "vkbd: typed SUPER Q",
        // And the window that went is the one the expression named.
        "the compositor asked the Checkerboard window called one to close",
    ] {
        if !said.iter().any(|line| line.contains(wanted)) {
            return Err(Error::new(format!(
                "{arch}: the typing boot never said `{wanted}`"
            )));
        }
    }
    println!(
        "  {arch}: a program typed SUPER Q through `zwp_virtual_keyboard_v1`, the bind fired, \
         and `closewindow title:^(one)$` closed the window it named -- every one of {} pixels \
         the renderer's own picture of the window that is left",
        last.width * last.height
    );
    Ok(())
}

/// FNV-1a, which is how a whole screen is compared through a serial port.
///
/// The same function `src/user/system/linux/compositor/shot`'s `digest` is, and it has to stay the
/// same: the guest prints the digest of what it was handed and this is what
/// that is compared against. Short enough to print on one line, and simple
/// enough that two copies cannot drift without a test saying so.
pub(super) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A thirteenth boot: a taskbar, through
/// `zwlr_foreign_toplevel_management_v1`.
///
/// `/bin/lswt` is what a bar's window list is once the drawing is taken
/// out: it binds the manager, takes the handle the compositor makes for
/// each window, and reads the title, the application id and the states.
/// Then it sends a request back through one of those handles -- `close`, on
/// a window it does not own -- which is what a middle click on a taskbar
/// entry does, and the screen says whether it arrived.
pub(super) fn test_taskbar(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        TASKBAR_CONFIG,
        &Wanted {
            states: &TASKBAR_EXPECTED,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &["lswt: left"],
        },
        &TASKBAR_BINDS,
        args,
    )?;
    let Some(last) = screens.last() else {
        return Err(Error::new(format!(
            "{arch}: the taskbar boot took no picture"
        )));
    };
    // Both windows, by application id, title and state: the focused one is
    // marked and the other is not, which is the tick a taskbar draws.
    let has = |wanted: &str| said.iter().any(|line| line.contains(wanted));
    for wanted in [
        "lswt: rocks.magical.pattern \"one\" []",
        "lswt: rocks.magical.pattern \"two\" [activated]",
        "lswt: closed \"one\"",
        // And the window that is left is the other one, which says the
        // close reached the window the bar named and not its neighbour.
        "lswt: left \"two\"",
    ] {
        if !has(wanted) {
            return Err(Error::new(format!(
                "{arch}: the taskbar never said `{wanted}`"
            )));
        }
    }
    println!(
        "  {arch}: a taskbar listed both windows with the focused one marked, and closed the \
         other from outside it, leaving every one of {} pixels the renderer's own picture of \
         one window",
        last.width * last.height
    );
    Ok(())
}

/// An eleventh boot: the clipboard, between two programs that have no
/// window.
///
/// `/bin/clip copy` offers text as `text/plain;charset=utf-8` and stays
/// alive to answer, as Wayland's clipboard requires: the data lives in the
/// program that copied it and the compositor holds only the promise.
/// `/bin/clip paste` is told what the selection holds, asks for the text on
/// a pipe it makes, and prints what comes back. Nothing but the pipe joins
/// the two processes, and the compositor is what passed it across.
pub(super) fn test_clipboard(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    // The copying program prints when it leaves, which is a moment after it
    // has answered: the boot is waited for that line rather than drained for
    // a fixed time, which under emulation is a coin toss.
    let asked = format!(
        "clip: copied {} bytes to the clipboard, asked for 1 times",
        CLIPBOARD_TEXT.len()
    );
    let pasted = format!("clip: pasted {CLIPBOARD_TEXT}");
    let asked_primary = format!(
        "clip: copied {} bytes to the primary, asked for 1 times",
        PRIMARY_TEXT.len()
    );
    let pasted_primary = format!("clip: pasted {PRIMARY_TEXT}");
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        &clipboard_config(),
        &Wanted {
            states: &CLIPBOARD_EXPECTED,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &[
                asked.as_str(),
                pasted.as_str(),
                asked_primary.as_str(),
                pasted_primary.as_str(),
            ],
        },
        &[],
        args,
    )?;
    let Some(screen) = screens.first() else {
        return Err(Error::new(format!(
            "{arch}: the clipboard boot took no picture"
        )));
    };
    let has = |wanted: &str| said.iter().any(|line| line.contains(wanted));
    for wanted in [
        // The compositor took both selections and handed each pipe on.
        "hyprix: the selection is 1 type(s) from client",
        "hyprix: the primary selection is 1 type(s) from client",
        "pasted text/plain;charset=utf-8, on a pipe to whoever copied",
    ] {
        if !has(wanted) {
            return Err(Error::new(format!(
                "{arch}: the clipboard boot did not say `{wanted}`"
            )));
        }
    }
    // The copying program was asked for its data exactly once, which is the
    // paste and nothing else.
    if !has(&asked) {
        return Err(Error::new(format!(
            "{arch}: the copying program did not say `{asked}`"
        )));
    }
    // And what came out of one program is what went into the other, for
    // each of the two selections.
    for (wanted, text) in [(&asked_primary, PRIMARY_TEXT), (&pasted, CLIPBOARD_TEXT)] {
        if !has(wanted) {
            return Err(Error::new(format!(
                "{arch}: nothing said `{wanted}` for {text:?}"
            )));
        }
    }
    if !has(&pasted_primary) {
        return Err(Error::new(format!(
            "{arch}: nothing pasted `{PRIMARY_TEXT}` from the primary selection"
        )));
    }
    println!(
        "  {arch}: one program copied {} bytes to the clipboard and {} to the primary selection \
         and two others pasted each back, on pipes the compositor passed between them, with the \
         windows still drawn in every one of {} pixels",
        CLIPBOARD_TEXT.len(),
        PRIMARY_TEXT.len(),
        screen.width * screen.height
    );
    Ok(())
}
