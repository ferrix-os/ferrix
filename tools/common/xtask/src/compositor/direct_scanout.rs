//! The boot where hyprix shows a client's own buffer: direct scanout on a
//! copying card (`docs/DISPLAY.md` §8.2).
//!
//! The machine is the firmware-screen boot's -- no virtio-gpu, a VGA whose
//! framebuffer `ferrix.efifb` lends to `efifb`, which copies each frame --
//! with the Intel stand-in beside it (`crate::intel_uapi::STANDIN`), driven
//! by `intel-fake` under `ferrix.intelgpu`, so there is an i915 render node
//! whose buffers are chardev dmabufs, as a game's are on the N150. One
//! pattern client makes its buffers there (`pattern --dmabuf`, GEM objects
//! by hand) and draws every frame, as a game does (`--busy`), and a window
//! rule makes it fullscreen. The frame lines hyprix says are printed: its
//! per-frame times with the client's buffer shown directly.
//!
//! hyprix must say it scans the client's surface out directly, and the VGA's
//! screendump must be the client's checkerboard, every pixel: the card was
//! handed the client's buffer through `PRIME_FD_TO_HANDLE` and `ADDFB2`,
//! and `efifb` copied it to the screen with nothing copied in between.
//!
//! The negative control (the consultant's condition 8, ledger line 3108)
//! removes the kernel's accept branch for a chardev dmabuf: the picture is
//! the same, since hyprix then copies the window, and the boot fails on the
//! missing line, with hyprix saying why it copies.

use super::boot::{Wanted, boot_and_dump_carrying};
use super::{Carried, Programs};
use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result};

/// The client fullscreen, as the boot's one picture.
const EXPECTED: [(&str, &str); 1] = [(
    "one client's own buffer shown fullscreen",
    "src/user/system/linux/compositor/render/tests/data/fullscreen-checkerboard.xrle",
)];

/// The configuration: one client on the Intel node, fullscreen.
const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
windowrule = fullscreen, match:class ^(rocks.magical.pattern)$
exec-once = /bin/pattern checkerboard scanout --dmabuf --busy
";

/// The framebuffer lent to `efifb`, and the Intel stand-in driven.
const OPTION: &str = "ferrix.efifb ferrix.intelgpu";

/// What hyprix says when it hands the card the client's buffer.
pub(crate) const SCANNING: &str = "directly";

/// hyprix's once-a-second line on the frames it drew, which the boot waits
/// for and prints: the per-frame times with the client's buffer shown
/// directly.
const FRAMES_LINE: &str = "hyprix: frames ";

/// The lines the guest must have said, each with what it shows.
const SAID: [(&str, &str); 5] = [
    (
        "copies frames: its driver maps the card read-only",
        "card0 is efifb's copying card",
    ),
    (
        "intel: render node published",
        "the Intel stand-in's render node is there",
    ),
    (
        "pattern: GEM buffers on an i915 render node",
        "the client made its buffers on the Intel node",
    ),
    (
        "pattern: scanout presents through zwp_linux_dmabuf_v1",
        "the client handed them over as dmabufs",
    ),
    (
        "hyprix: Virtual-1: scanning out client",
        "hyprix showed the client's buffer itself",
    ),
];

pub(super) fn test_direct_scanout(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    if arch != Arch::X86_64 {
        println!("  {arch}: the firmware framebuffer is lent on x86-64 alone; skipped");
        return Ok(());
    }
    crate::intel_uapi::refuse_unpatched_qemu()?;
    let mut args = args.clone();
    args.firmware_screen = true;
    args.intel_gpu_standin = true;
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
            // A second of frames, for their times: hyprix's once-a-second
            // lines.
            awaiting: &[FRAMES_LINE],
        },
        &[],
        &args,
    )?;
    let Some(screen) = screens.first() else {
        return Err(Error::new(format!(
            "{arch}: the direct-scanout boot took no picture"
        )));
    };
    for said in said.iter().filter(|said| {
        said.contains("hyprix: Virtual-1: copying the fullscreen window")
            || said.contains("hyprix: Virtual-1: compositing again")
            || said.contains("hyprix: direct scanout frames")
            || said.contains("hyprix: frames ")
    }) {
        println!("  {arch}: the guest said: {}", said.trim());
    }
    for (line, what) in SAID {
        if !said.iter().any(|said| {
            said.contains(line) && (!line.starts_with("hyprix") || said.contains(SCANNING))
        }) {
            return Err(Error::new(format!(
                "{arch}: the guest never said `{line}`, which is how it shows {what}"
            )));
        }
    }
    println!(
        "  {arch}: a client's own buffer shown directly on a copying card, every one of {} \
         pixels the client's",
        screen.width * screen.height
    );
    Ok(())
}
