//! `test-usb`: a PC's USB bus, as far as QEMU has one.
//!
//! `docs/XHCI.md` §7. The boot has a `qemu-xhci` controller with a hub on
//! its first port, a keyboard behind the hub, a mouse on its second port and
//! a memory stick on its first USB 3 port -- emulated devices all, never the
//! host's. Before `test-restart`'s two kills of the driver, the gate
//! requires:
//!
//! 1. the driver's own lines for the controller, the hub and the stick, the
//!    stick's with its size and a sum of its first block that the gate
//!    worked out from the image it attached;
//! 2. a key pressed and the mouse moved and clicked through QMP, arriving at
//!    the nodes `evecho` found the keyboard and the mouse on;
//! 3. the keyboard unplugged through QMP gone from the input core, and one
//!    plugged in again found and typing.
//!
//! After each kill the restarted driver must find all of it again, and a
//! key must arrive.
//!
//! **The negative control** is a second boot with two things wrong on
//! purpose: the stick is a different image from the one the gate holds the
//! sum of, and `evecho` is the build that reports every key as
//! `KEY_RESERVED`. Both checks must fail, each with the line that shows why
//! -- a stick read, with another sum; a key delivered, under another name --
//! so neither passes by reading the shape of the output.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{EVECHO, EVECHO_READY, PATIENCE, seen};
use crate::display::Qmp;
use crate::paths::{self, Arch};
use crate::{Error, Result, qemu};

/// The keyboard's and the mouse's names, as QEMU's devices give their
/// product strings.
pub(super) const KEYBOARD: &str = "QEMU USB Keyboard";
const MOUSE: &str = "QEMU USB Mouse";

/// Where the control's `evecho` is carried.
pub(super) const EVECHO_CONTROL: &str = "/bin/evecho-control";

/// What the driver says once each thing on the bus is found: what a boot,
/// and each restart, must show.
pub(super) const FOUND: [&str; 4] = [
    "usbhid: xHCI running",
    "USB Keyboard is event",
    "USB Mouse is event",
    "usbhid: disk 0 ",
];

/// The stick's size: enough blocks that a wrong count shows, small enough
/// to write on every run.
const STICK_BYTES: usize = 8 * 1024 * 1024;
const BLOCK: usize = 512;

/// The stick's image, and what the driver must say of it.
pub(super) struct Stick {
    pub(super) path: PathBuf,
    /// The driver's words for its size and its first block.
    said: String,
}

impl Stick {
    /// Write the image for `seed` and work out what the driver must say of
    /// the one for `expected`: the same seed, but for the control.
    pub(super) fn write(arch: Arch, seed: u64, expected: u64) -> Result<Stick> {
        let path = paths::build_dir(arch).join(format!("usb-stick-{seed}.img"));
        std::fs::create_dir_all(paths::build_dir(arch))?;
        std::fs::write(&path, image(seed))
            .map_err(|error| Error::new(format!("writing {}: {error}", path.display())))?;
        let wanted = image(expected);
        let sum = fnv(wanted.get(..BLOCK).unwrap_or_default());
        Ok(Stick {
            path,
            said: format!(
                "{} blocks of {BLOCK} bytes, block 0 fnv {sum:016x}",
                STICK_BYTES / BLOCK
            ),
        })
    }
}

/// A stick's bytes: a xorshift stream from `seed`, so no block is another's
/// copy and no two seeds share a first block.
fn image(seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut bytes = Vec::with_capacity(STICK_BYTES);
    while bytes.len() < STICK_BYTES {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes
}

/// FNV-1a, as the driver sums the block it read.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |sum, &byte| {
        (sum ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

/// Everything before the first kill: what the driver found, a key and the
/// mouse, and the keyboard unplugged and plugged in again.
pub(super) fn before_the_kills(
    watching: &mut qemu::Watching<'_>,
    stick: &Stick,
    port: u16,
) -> std::result::Result<(), String> {
    found_at_boot(watching.lines(), stick)?;
    types_and_points(watching, port, EVECHO)?;
    unplug_and_replug(watching, port)
}

/// The driver's lines for the hub and the stick.
fn found_at_boot(lines: &[String], stick: &Stick) -> std::result::Result<(), String> {
    if !seen(lines, "a hub of 8 ports") {
        return Err("the driver never said it found QEMU's hub of 8 ports".to_owned());
    }
    let Some(disk) = lines.iter().find(|line| line.contains("usbhid: disk 0 ")) else {
        return Err("the driver never said it found the stick".to_owned());
    };
    if disk.contains(&stick.said) {
        Ok(())
    } else {
        Err(format!(
            "the stick the gate attached is `{}`, and the driver said `{}`",
            stick.said,
            disk.trim()
        ))
    }
}

fn press(qmp: &mut Qmp) -> Result<()> {
    for down in [true, false] {
        qmp.input_send_event(&[crate::input::key("a", down)])?;
    }
    Ok(())
}

/// A relative movement, as QMP's `input-send-event` takes it.
fn relative(axis: &str, value: i32) -> String {
    format!("{{\"type\":\"rel\",\"data\":{{\"axis\":\"{axis}\",\"value\":{value}}}}}")
}

fn button(down: bool) -> String {
    format!("{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":\"left\"}}}}")
}

/// Start `evecho`, press `a`, move the mouse and click it, and require each
/// on the node evecho found its device on.
fn types_and_points(
    watching: &mut qemu::Watching<'_>,
    port: u16,
    evecho: &str,
) -> std::result::Result<(), String> {
    let io = |error: Error| error.to_string();
    let before = watching.after().len();
    watching
        .type_in(format!("{evecho} &\n").as_bytes())
        .map_err(io)?;
    let ready = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            seen(lines.get(before..).unwrap_or_default(), EVECHO_READY)
        })
        .map_err(io)?;
    if !ready {
        return Err(format!("{evecho} never opened the USB devices"));
    }
    arrive(watching, port, before, evecho)
}

/// Press `a`, move the mouse and click it, and require each on the node the
/// `evecho` that started after line `before` found its device on.
fn arrive(
    watching: &mut qemu::Watching<'_>,
    port: u16,
    before: usize,
    evecho: &str,
) -> std::result::Result<(), String> {
    let io = |error: Error| error.to_string();
    let opened = watching.after().get(before..).unwrap_or_default().to_vec();
    let keyboard = crate::input::node_of(&opened, KEYBOARD)
        .ok_or_else(|| format!("{evecho} opened no `{KEYBOARD}`"))?;
    let mouse = crate::input::node_of(&opened, MOUSE)
        .ok_or_else(|| format!("{evecho} opened no `{MOUSE}`"))?;

    let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10)).map_err(io)?;
    press(&mut qmp).map_err(io)?;
    for events in [
        vec![relative("x", 7), relative("y", -3)],
        vec![button(true)],
        vec![button(false)],
    ] {
        qmp.input_send_event(&events).map_err(io)?;
    }
    let wanted = [
        format!("evecho: {keyboard} EV_KEY KEY_A 1"),
        format!("evecho: {keyboard} EV_KEY KEY_A 0"),
        format!("evecho: {keyboard} EV_SYN SYN_REPORT 0"),
        format!("evecho: {mouse} EV_REL REL_X 7"),
        format!("evecho: {mouse} EV_REL REL_Y -3"),
        format!("evecho: {mouse} EV_KEY BTN_LEFT 1"),
        format!("evecho: {mouse} EV_KEY BTN_LEFT 0"),
    ];
    let missing = |lines: &[String]| -> Vec<String> {
        let lines = lines.get(before..).unwrap_or_default();
        wanted
            .iter()
            .filter(|want| !seen(lines, want))
            .cloned()
            .collect()
    };
    let _ = watching
        .read_more(Instant::now() + PATIENCE, |lines| missing(lines).is_empty())
        .map_err(io)?;
    let lost = missing(watching.after());
    if lost.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} of {} events put in through QMP never reached {keyboard} and {mouse}; the \
             first missing: `{}`",
            lost.len(),
            wanted.len(),
            lost.first().map_or("", String::as_str)
        ))
    }
}

/// Unplug the keyboard and require the input core to lose its node; plug
/// one in again and require it found, then a key from it.
fn unplug_and_replug(
    watching: &mut qemu::Watching<'_>,
    port: u16,
) -> std::result::Result<(), String> {
    let io = |error: Error| error.to_string();
    let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10)).map_err(io)?;
    let before = watching.after().len();
    qmp.device_del(qemu::USB_KEYBOARD_ID).map_err(io)?;
    let gone = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            let lines = lines.get(before..).unwrap_or_default();
            seen(lines, "unplugged") && seen(lines, "is gone")
        })
        .map_err(io)?;
    if !gone {
        return Err(
            "the keyboard unplugged through QMP was never said gone: no `unplugged` from \
             the driver and `is gone` from the input core"
                .to_owned(),
        );
    }
    let before = watching.after().len();
    qmp.device_add(&qemu::usb_keyboard("usbkbd2")).map_err(io)?;
    let back = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            seen(lines.get(before..).unwrap_or_default(), FOUND[1])
        })
        .map_err(io)?;
    if !back {
        return Err("the keyboard plugged in again through QMP was never found".to_owned());
    }
    drop(qmp);
    super::a_key_arrives(watching, 0, Some(port), KEYBOARD)
}

/// The control boot's verdict: with another stick attached and the `evecho`
/// that misnames keys, both checks must fail, each for its reason.
pub(super) fn control(
    watching: &mut qemu::Watching<'_>,
    stick: &Stick,
    port: u16,
) -> std::result::Result<(), String> {
    match found_at_boot(watching.lines(), stick) {
        Ok(()) => {
            return Err(
                "the control's stick passed the check it is built to fail: the sum is not \
                 of what was read"
                    .to_owned(),
            );
        }
        Err(why) if why.contains("block 0 fnv") => {}
        Err(why) => {
            return Err(format!(
                "the control's stick failed without ever being read, which proves nothing \
                 about the check: {why}"
            ));
        }
    }
    let before = watching.after().len();
    if types_and_points(watching, port, EVECHO_CONTROL).is_ok() {
        return Err("the control's keys passed the check they are built to fail".to_owned());
    }
    let misnamed = seen(
        watching.after().get(before..).unwrap_or_default(),
        "EV_KEY KEY_RESERVED 1",
    );
    if misnamed {
        Ok(())
    } else {
        Err(
            "the control failed without ever reporting a key: it proves nothing about the \
             check"
                .to_owned(),
        )
    }
}

/// The stick as `test-usb` attaches it.
pub(super) fn attached(stick: &Path) -> Option<PathBuf> {
    Some(stick.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::{BLOCK, STICK_BYTES, Stick, fnv, found_at_boot, image};

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn the_sum_is_fnv_1a() {
        // The empty input's offset basis, and the published vector for "a".
        assert_eq!(fnv(b""), 0xCBF2_9CE4_8422_2325);
        assert_eq!(fnv(b"a"), 0xAF63_DC4C_8601_EC8C);
    }

    #[test]
    fn two_seeds_are_two_sticks_and_no_block_repeats_the_first() {
        let (one, two) = (image(1), image(2));
        assert_eq!(one.len(), STICK_BYTES);
        assert_ne!(one.get(..BLOCK), two.get(..BLOCK));
        assert_ne!(one.get(..BLOCK), one.get(BLOCK..2 * BLOCK));
    }

    #[test]
    fn the_stick_s_line_must_name_its_size_and_its_first_block() {
        let stick = Stick {
            path: "unused".into(),
            said: "16384 blocks of 512 bytes, block 0 fnv 0123456789abcdef".to_owned(),
        };
        let hub = "usbhid: device 1 0409:55aa Full speed, a hub of 8 ports";
        let right = "usbhid: disk 0 QEMU QEMU HARDDISK: 16384 blocks of 512 bytes, block 0 fnv \
                     0123456789abcdef";
        let wrong = "usbhid: disk 0 QEMU QEMU HARDDISK: 16384 blocks of 512 bytes, block 0 fnv \
                     fedcba9876543210";
        assert_eq!(found_at_boot(&lines(&[hub, right]), &stick), Ok(()));
        assert!(
            found_at_boot(&lines(&[hub, wrong]), &stick)
                .is_err_and(|why| why.contains("fedcba9876543210"))
        );
        assert!(found_at_boot(&lines(&[right]), &stick).is_err_and(|why| why.contains("hub")));
        assert!(found_at_boot(&lines(&[hub]), &stick).is_err_and(|why| why.contains("stick")));
    }
}
