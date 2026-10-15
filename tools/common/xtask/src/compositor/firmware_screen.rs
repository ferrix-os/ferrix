//! The boot with no GPU driver: hyprix on the firmware's framebuffer
//! (`docs/DISPLAY.md` §8, `docs/INSTALLER.md` §6's H6).
//!
//! The machine has no virtio-gpu. Its one screen is a VGA whose EDID
//! prefers 1024x768, which OVMF takes for its graphics output, so the loader
//! hands the kernel a 1024x768 framebuffer that firmware chose. The command
//! line says `ferrix.efifb`: the kernel lends the framebuffer to a node of
//! its own, `devmgr` starts `efifb` on it, and the card it serves is the only
//! one. The compositor tiles its two clients there, and the VGA's screendump
//! must be the renderer's blessed picture of them, every pixel.
//!
//! What the guest said must show the path it took: the framebuffer lent,
//! `efifb` at the firmware's size, and a card that copies frames.

use super::boot::{Wanted, boot_and_dump_carrying};
use super::{Carried, Programs};
use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result};

/// The two clients tiled, as the first boot's first picture.
const EXPECTED: [(&str, &str); 1] = [(
    "two clients tiled on the firmware's framebuffer",
    "src/user/system/linux/compositor/render/tests/data/dwindle-two-clients.xrle",
)];

/// The configuration: the two clients, nothing else.
const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
exec-once = /bin/pattern checkerboard one
exec-once = /bin/pattern gradient two --after one
";

/// What the kernel's command line says after init's: the framebuffer lent,
/// and the boot console drawn on it first, so that the console's stop is
/// tested too -- a line it drew after `efifb` started would be in the picture.
const OPTION: &str = "ferrix.efifb ferrix.fbcon";

/// The lines the guest must have said, each with what it shows.
const SAID: [(&str, &str); 4] = [
    (
        "config   1 function(s) holding the lent firmware framebuffer answered no read",
        "the VGA function that holds the framebuffer is driven by nobody",
    ),
    (
        "display  firmware framebuffer 1024x768 lent to a driver",
        "the kernel lent the framebuffer",
    ),
    (
        "efifb: 1024x768 firmware framebuffer",
        "efifb started at the firmware's mode",
    ),
    (
        "copies frames: its driver maps the card read-only",
        "the card copies frames",
    ),
];

pub(super) fn test_firmware_screen(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    if arch != Arch::X86_64 {
        println!("  {arch}: the firmware framebuffer is lent on x86-64 alone; skipped");
        return Ok(());
    }
    let mut args = args.clone();
    args.firmware_screen = true;
    let (screens, said) = boot_and_dump_carrying(
        arch,
        programs,
        CONFIG,
        (Carried::none(), Some(OPTION)),
        &Wanted {
            states: &EXPECTED,
            others: &[],
            moving: None,
            pointer: None,
            awaiting: &[],
        },
        &[],
        &args,
    )?;
    let Some(screen) = screens.first() else {
        return Err(Error::new(format!(
            "{arch}: the firmware-screen boot took no picture"
        )));
    };
    for (line, what) in SAID {
        if !said.iter().any(|said| said.contains(line)) {
            return Err(Error::new(format!(
                "{arch}: the guest never said `{line}`, which is how it shows {what}"
            )));
        }
    }
    println!(
        "  {arch}: hyprix on the firmware's framebuffer with no GPU driver, every one of {} \
         pixels as the renderer draws them",
        screen.width * screen.height
    );
    Ok(())
}
