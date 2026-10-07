//! Stage 19's second-pass effects and the drag back into the tiling, each a
//! boot: a tiled window dragged with the mouse and dropped, with and without
//! `dwindle:precise_mouse_move`; a bar's menu blurred by
//! `layerrule = blur_popups`; and a window `no_screen_share` hides from a
//! screenshot while the screen still shows it.
//!
//! Every picture is one `src/user/system/linux/compositor/render` blesses
//! from the layout and the renderer, so the boot and the host tests hold the
//! compositor to the same pixels.

use super::Programs;
use super::boot::{Wanted, boot_and_dump, said_on_its_own};
use super::picture::expected;
use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result};

/// The tiled pair every boot here starts from.
const TILED: (&str, &str) = (
    "tiled",
    "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
);

/// The drag, as `hyprctl` sends it: the pointer to the middle of the
/// focused gradient, the `bindm` press that starts a `movewindow` drag, the
/// pointer to (300, 40), and the release.
///
/// Through `hyprctl` rather than QMP's buttons because the drop is decided
/// by where the pointer is when the drag ends, and QEMU's tablet reports a
/// position the guest scales: a socket request names the pixel. `mouse` is
/// the dispatcher a `bindm` line fires, the same call a hand makes.
const DRAG_BIND: &str = "bind = , D, exec, /bin/hyprctl --batch \
    dispatch movecursor 768 384 ; dispatch mouse +movewindow ; \
    dispatch movecursor 300 40 ; dispatch mouse -movewindow\n";

/// The key that drags.
const DRAG_BINDS: [(&str, &[&str]); 1] = [("D, which drags the gradient and drops it", &["d"])];

/// The configuration of the drag boots, with `extra` added.
fn drag_config(extra: &str) -> String {
    format!(
        "# Carried into the initramfs by `cargo xtask test-compositor`.\n\
         exec-once = /bin/pattern checkerboard one\n\
         exec-once = /bin/pattern gradient two --after one\n\
         {extra}{DRAG_BIND}"
    )
}

/// A boot: the gradient, tiled on the right, dragged and dropped near the
/// top of the left half of the screen. Without `dwindle:precise_mouse_move`
/// it goes on the left half of the box it was dropped on.
pub(super) fn test_drag(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    dropped(
        arch,
        programs,
        args,
        "",
        (
            "dropped on the left half: the two windows have changed sides",
            "src/user/system/linux/compositor/render/tests/data/dropped-two-clients.xrle",
        ),
    )
}

/// The same drop with `dwindle:precise_mouse_move = 1`: the quarter of the
/// box, which near its top edge stacks the two with the gradient on top.
pub(super) fn test_drag_precise(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    dropped(
        arch,
        programs,
        args,
        "dwindle:precise_mouse_move = 1\n",
        (
            "dropped near the top with precise_mouse_move: stacked, the gradient on top",
            "src/user/system/linux/compositor/render/tests/data/dropped-precisely-two-clients.xrle",
        ),
    )
}

/// One drag boot: the tiled pair, then `after` once `D` has dragged.
fn dropped(
    arch: Arch,
    programs: &Programs,
    args: &Args,
    extra: &str,
    after: (&str, &str),
) -> Result<()> {
    let states = [TILED, after];
    let (screens, _) = boot_and_dump(
        arch,
        programs,
        &drag_config(extra),
        &Wanted {
            states: &states,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &[],
        },
        &DRAG_BINDS,
        args,
    )?;
    if screens.len() != states.len() {
        return Err(Error::new(format!(
            "{arch}: {} of {} pictures were taken",
            screens.len(),
            states.len()
        )));
    }
    println!(
        "  {arch}: a tiled window was dragged out of the tiling and dropped back in where the \
         pointer let go of it"
    );
    Ok(())
}

/// A boot: `layerrule = blur_popups` on the pattern bar, whose menu hangs
/// over the windows with what is behind it blurred.
pub(super) fn test_bar_menu(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
layerrule = blur_popups on, match:namespace ^(pattern-bar)$
exec-once = /bin/pattern checkerboard bar --bar 30 --menu 200
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
";
    let states = [(
        "a bar's menu over the windows, blurred behind by its layerrule",
        "src/user/system/linux/compositor/render/tests/data/blurred-menu-on-a-bar.xrle",
    )];
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        CONFIG,
        &Wanted {
            states: &states,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &["menu at"],
        },
        &[],
        args,
    )?;
    if screens.is_empty() {
        return Err(Error::new(format!(
            "{arch}: the bar-menu boot took no picture"
        )));
    }
    if !said
        .iter()
        .any(|line| line.contains("pattern: bar menu at 1,1 200x200"))
    {
        return Err(Error::new(format!(
            "{arch}: the bar was never told where its menu is"
        )));
    }
    println!("  {arch}: a bar's menu was drawn with the blur its layerrule asked for");
    Ok(())
}

/// A boot: `windowrule = no_screen_share` on the gradient. The screen shows
/// both windows; a screenshot taken on the guest shows a black box where
/// the gradient is.
pub(super) fn test_screenshot_unshared(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
windowrule = no_screen_share, match:title ^(two)$
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
bind = , S, exec, /bin/shot
";
    const BINDS: [(&str, &[&str]); 1] = [("S, which takes a screenshot", &["s"])];
    const BOXED: &str =
        "src/user/system/linux/compositor/render/tests/data/unshared-two-clients.xrle";
    let states = [
        TILED,
        ("still tiled, with a screenshot taken of it", TILED.1),
    ];
    let (screens, said) = boot_and_dump(
        arch,
        programs,
        CONFIG,
        &Wanted {
            states: &states,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &["shot: "],
        },
        &BINDS,
        args,
    )?;
    let Some(screen) = screens.first() else {
        return Err(Error::new(format!(
            "{arch}: the screenshot boot took no picture"
        )));
    };
    let want = expected(BOXED)?;
    let line = format!(
        "shot: {}x{} {:016x}",
        screen.width,
        screen.height,
        super::protocols::fnv1a(&want)
    );
    if !said.iter().any(|said| said_on_its_own(said) == line) {
        let printed = said
            .iter()
            .map(|said| said_on_its_own(said))
            .filter(|said| said.starts_with("shot: "))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(Error::new(format!(
            "{arch}: the guest's screenshot is not the window blacked out: it said `{printed}` \
             and the image is `{line}`"
        )));
    }
    println!(
        "  {arch}: the screen shows the window, and the guest's screenshot a black box where it \
         is"
    );
    Ok(())
}
