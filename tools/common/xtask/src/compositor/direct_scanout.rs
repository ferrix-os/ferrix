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
//! missing line, with hyprix saying why it does not show the buffer
//! directly. The second boot has the window floating at the screen's size
//! with rounded corners configured, as an X game is on the N150: hyprix
//! draws it as legacy fullscreen and shows its buffer the same way.

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

/// The configuration: one client on the Intel node, fullscreen, and lode's
/// level table once the drivers have spoken ([`LEVELS`]).
const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
windowrule = fullscreen, match:class ^(rocks.magical.pattern)$
exec-once = /bin/pattern checkerboard scanout --dmabuf --busy
exec-once = /bin/busybox sh -c '/bin/busybox sleep 5; /bin/lode levels -b | /bin/busybox sed \"s/^/lode-level\"\"s: /\"; echo lode-level\"\"s: table ends'
";

/// What each line of `lode levels -b` is said after, in [`CONFIG`]'s boot.
/// The command spells it `lode-level""s` so that hyprix's line naming the
/// command it started is never taken for the table.
const LEVELS: &str = "lode-levels: ";

/// The line after the table.
const LEVELS_END: &str = "lode-levels: table ends";

/// The sources -- `SYSLOG_IDENTIFIER`s -- of the ring-3 drivers and devmgr
/// that this boot runs, each of whose lines must reach lode with the level
/// its writer gave it (`ferrix_rt::log`): none unspecified, none derived
/// from its words. `intel` is the fake's front end and `intel-fake` its
/// facts; `efifb` the copying card's, `ahci` the machine's SATA controller's.
const DRIVERS: [&str; 5] = ["intel", "intel-fake", "devmgr", "efifb", "ahci"];

/// The configuration of the second boot: the same client floating at the
/// screen's size and place, as an X game is on the N150 (yserver passes no
/// `_NET_WM_STATE_FULLSCREEN` on), with rounded corners and shadows that a
/// window drawn as legacy fullscreen does not have.
const FLOATING_CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor`.
decoration {
    rounding = 10
}
windowrule = float, match:class ^(rocks.magical.pattern)$
windowrule = size 1024 768, match:class ^(rocks.magical.pattern)$
windowrule = move 0 0, match:class ^(rocks.magical.pattern)$
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
    run(arch, programs, args, CONFIG, "fullscreen")
}

/// The same with the window floating at the screen's size: hyprix draws it
/// as legacy fullscreen (`state::draw::legacy_fullscreen`) and shows its
/// buffer directly all the same.
pub(super) fn test_direct_scanout_floating(
    arch: Arch,
    programs: &Programs,
    args: &Args,
) -> Result<()> {
    run(
        arch,
        programs,
        args,
        FLOATING_CONFIG,
        "floating at the screen's size",
    )
}

/// Judge the level table [`CONFIG`] has the guest say: every source in
/// [`DRIVERS`] there, with no record unspecified or derived. `Ok` with the
/// table's lines, for the log.
fn judge_levels(said: &[String]) -> std::result::Result<Vec<String>, String> {
    let table: Vec<String> = said
        .iter()
        .filter_map(|line| {
            let at = line.find(LEVELS)?;
            Some(line.get(at + LEVELS.len()..)?.trim_end().to_owned())
        })
        .filter(|row| row != "table ends")
        .collect();
    for source in DRIVERS {
        let Some(row) = table
            .iter()
            .find(|row| row.split_whitespace().next() == Some(source))
        else {
            return Err(format!(
                "lode's level table has no row for {source}: its lines never reached lode"
            ));
        };
        let counts: Vec<u64> = row
            .split_whitespace()
            .skip(1)
            .filter_map(|count| count.parse().ok())
            .collect();
        match counts.as_slice() {
            [_, 0, 0] => {}
            [records, unspecified, derived] => {
                return Err(format!(
                    "{source}: {unspecified} of {records} records reached lode with no level, \
                     {derived} with one read from their words: a driver's lines carry the level \
                     it gave them (ferrix_rt::log)"
                ));
            }
            _ => {
                return Err(format!(
                    "lode's level table's row for {source} reads `{row}`"
                ));
            }
        }
    }
    Ok(table)
}

fn run(arch: Arch, programs: &Programs, args: &Args, config: &str, how: &str) -> Result<()> {
    if arch != Arch::X86_64 {
        println!("  {arch}: the firmware framebuffer is lent on x86-64 alone; skipped");
        return Ok(());
    }
    crate::intel_uapi::refuse_unpatched_qemu()?;
    let mut args = args.clone();
    args.firmware_screen = true;
    args.intel_gpu_standin = true;
    // The level table's shell, sleep and sed: this machine's busybox.
    let carried = if config == CONFIG {
        let busybox = crate::busybox::installed_program(arch).ok_or_else(|| {
            Error::new(format!(
                "{arch}: no busybox for the level table's shell: `cargo xtask busybox` builds one"
            ))
        })?;
        Carried {
            busybox: Some(busybox),
            ..Carried::none()
        }
    } else {
        Carried::none()
    };
    let (screens, said) = boot_and_dump_carrying(
        arch,
        programs,
        config,
        (carried, Some(OPTION)),
        &Wanted {
            states: &EXPECTED,
            others: &[],
            moving: None,
            pointer: None,
            // A second of frames, for their times: hyprix's once-a-second
            // lines; and in the fullscreen boot lode's level table.
            awaiting: if config == CONFIG {
                &[FRAMES_LINE, LEVELS_END]
            } else {
                &[FRAMES_LINE]
            },
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
        said.contains("hyprix: Virtual-1: not scanning out directly")
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
    if config == CONFIG {
        let table = judge_levels(&said).map_err(|why| Error::new(format!("{arch}: {why}")))?;
        for row in &table {
            println!("  {arch}: lode levels: {row}");
        }
        println!(
            "  {arch}: every line of {} reached lode with its writer's level",
            DRIVERS.join(", ")
        );
    }
    println!(
        "  {arch}: a client's own buffer, {how}, shown directly on a copying card, every one \
         of {} pixels the client's",
        screen.width * screen.height
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::judge_levels;

    fn said(rows: &[&str]) -> Vec<String> {
        rows.iter()
            .map(|row| format!("[   12.5] hyprix: lode-levels: {row}"))
            .collect()
    }

    #[test]
    fn the_drivers_rows_must_have_no_unspecified_and_no_derived() {
        let good = [
            "SOURCE                    RECORDS  UNSPECIFIED  DERIVED",
            "kernel                        900          880       20",
            "intel                          12            0        0",
            "intel-fake                      1            0        0",
            "devmgr                         20            0        0",
            "efifb                           5            0        0",
            "ahci                            6            0        0",
            "table ends",
        ];
        assert_eq!(judge_levels(&said(&good)).map(|table| table.len()), Ok(7));
        let mut unspecified = good;
        unspecified[2] = "intel                          12            3        0";
        assert!(judge_levels(&said(&unspecified)).is_err_and(|why| why.contains("3 of 12")));
        let mut derived = good;
        derived[4] = "devmgr                         20            0        2";
        assert!(judge_levels(&said(&derived)).is_err());
        let missing = &good[..5];
        assert!(judge_levels(&said(missing)).is_err_and(|why| why.contains("efifb")));
    }
}
