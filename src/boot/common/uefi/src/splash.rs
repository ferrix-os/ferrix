//! The boot splash's first picture: the FX mark on ink, its slash lit a
//! little, painted over firmware's text as the loader leaves
//! (`docs/SPLASH.md`).
//!
//! Asked for with `ferrix.splash` on the command line, which the desktop's
//! images carry in `DEFAULTS.TXT`. The picture is `ferrix_splash`'s, the one
//! `/bin/splash` draws again once a display driver has the screen, so the
//! two are the same pixels.
//!
//! It is painted on every graphics output, through firmware's `Blt`, not
//! into the framebuffer the kernel is handed. A machine can have more than
//! one output, and the one the kernel keeps for a panic is chosen for where
//! its memory is, not for being the one a person looks at: on QEMU that is
//! the VGA, while the window shows the virtio-gpu, which has no framebuffer
//! to write to at all. It is painted once, after the loader's last line and
//! while boot services are up, and nothing redraws it afterwards: the kernel
//! draws on a screen only for `ferrix.fbcon` and a panic.

use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_splash::{Canvas, INK, LOADED, Mark};

use crate::services::{Output, Services};
use crate::stripes;

/// The command-line word that asks for it.
const FLAG: &str = "ferrix.splash";

/// Whether the command line asked.
static ASKED: AtomicBool = AtomicBool::new(false);

/// Read `cmdline` for the flag; with it, the hand-over's progress bars stay
/// off the screen.
pub(crate) fn read(cmdline: &str) {
    if ferrix_bootinfo::flag_in(cmdline, FLAG) {
        ASKED.store(true, Ordering::Release);
        stripes::quiet();
    }
}

/// One output, as the picture is drawn on it.
struct Screen<'a>(&'a Output);

impl Canvas for Screen<'_> {
    fn span(&mut self, y: u32, from: u32, to: u32, rgb: u32) {
        self.0.fill((from, y), (to.saturating_sub(from), 1), rgb);
    }
}

/// Paint the splash on every output it fits, if it was asked for.
pub(crate) fn show(services: &Services) {
    if !ASKED.load(Ordering::Acquire) {
        return;
    }
    services.graphics_outputs(|output| {
        let Some(mark) = Mark::centred(output.width, output.height) else {
            return;
        };
        output.fill((0, 0), (output.width, output.height), INK);
        mark.draw(&mut Screen(output), LOADED);
    });
}
