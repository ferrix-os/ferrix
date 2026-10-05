//! `cargo xtask test-input`: a key and a touch, put in at QEMU's far end and
//! read out of `/dev/input/eventN`.
//!
//! `docs/INPUT.md` L7. Everything below this has its own test -- the evdev
//! numbers against the UAPI headers, the virtio-input driver against a
//! simulated device, `src/lib/proto/inputctl`'s queue against `evdev.c`'s rules -- and
//! every one of them is a test of a part in isolation. This is the whole
//! path at once, and nothing in it is simulated: QEMU's `input-send-event`
//! puts an event into a real `virtio-keyboard-pci`, the ring-3 driver reads
//! it off the device's event queue, the kernel's input core assembles the
//! report, `/dev/input/event0` hands it to `src/user/system/linux/compositor/evecho`, and evecho
//! prints it on the serial port this reads.
//!
//! **The negative control.** The same boot is run again with evecho built
//! with `negative-control`, which reports every key as `KEY_RESERVED`. The
//! events still arrive and are still printed -- so the control does not pass
//! by the guest falling over -- but the line the check waits for never comes,
//! and the run must fail. A check that passed both ways would be reading the
//! shape of the output rather than the events in it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::display::{Qmp, free_port, json_string};
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// What evecho prints once it has opened every device.
const READY: &str = "evecho: ready";

/// The name QEMU's virtio-input keyboard gives itself, which the device's
/// line holds and which says which node is the keyboard.
const KEYBOARD: &str = "QEMU Virtio Keyboard";

/// The same for the tablet, the absolute pointer.
const TABLET: &str = "QEMU Virtio Tablet";

/// Where the pointer is put, in QEMU's absolute range, which is 0 to 0x7fff.
/// Two values that are not each other and not zero, so a line that carried
/// the wrong axis or a stuck one would show.
const POINTER: (i32, i32) = (0x4000, 0x2000);

/// How long to wait for the events after they have been sent. They cross a
/// virtqueue, a channel and a device node, and the guest is emulated.
const PATIENCE: Duration = Duration::from_secs(20);

/// Build `src/user/system/linux/compositor/evecho` for `arch`, with the negative control or
/// without, and say where the program is.
pub(crate) fn build_evecho(arch: Arch, negative: bool) -> Result<PathBuf> {
    let target = crate::display::target(arch).ok_or_else(|| {
        Error::new(format!(
            "{arch} has no virtio-input in QEMU's machine; the input test runs on x86_64 and \
             aarch64"
        ))
    })?;
    let flavour = if negative { "negative" } else { "plain" };
    // A directory of its own for each flavour: `test-input` builds both
    // before its first boot, and one would otherwise overwrite the other.
    let target_dir = paths::target_dir().join("compositor").join(if negative {
        "evecho-negative"
    } else {
        "evecho"
    });
    println!("  building src/user/system/linux/compositor/evecho ({flavour}) for {target}");
    let program = target_dir.join(target).join("release").join("evecho");
    let mut build = crate::builds::Build::cargo(
        format!(
            "cargo build (src/user/system/linux/compositor/evecho, {flavour}) --target {target}"
        ),
        paths::workspace_root().join("src/user/system/linux/compositor"),
    )
    .args([
        "build",
        "--release",
        "-p",
        "compositor-evecho",
        "--target",
        target,
    ])
    .env("CARGO_TARGET_DIR", &target_dir)
    .output(&program);
    if negative {
        build = build.args(["--features", "negative-control"]);
    }
    build.run()?;
    Ok(program)
}

/// One `InputEvent` of QMP's `input-send-event`, as its JSON.
pub(crate) fn key(name: &str, down: bool) -> String {
    format!(
        "{{\"type\":\"key\",\"data\":{{\"down\":{down},\"key\":\
         {{\"type\":\"qcode\",\"data\":{}}}}}}}",
        json_string(name)
    )
}

/// The same for a pointer button.
fn button(name: &str, down: bool) -> String {
    format!(
        "{{\"type\":\"btn\",\"data\":{{\"down\":{down},\"button\":{}}}}}",
        json_string(name)
    )
}

/// The same for an absolute axis.
fn absolute(axis: &str, value: i32) -> String {
    format!(
        "{{\"type\":\"abs\",\"data\":{{\"axis\":{},\"value\":{value}}}}}",
        json_string(axis)
    )
}

/// The node a device's line names, for the device whose name holds `what`.
///
/// A line reads `evecho: event0 QEMU Virtio Keyboard [0x...] EV_SYN ...`, so
/// the node is the second word.
pub(crate) fn node_of(lines: &[String], what: &str) -> Option<String> {
    lines
        .iter()
        .find(|line| line.contains(what) && line.contains("evecho: event"))
        .and_then(|line| {
            let at = line.find("evecho: ")? + "evecho: ".len();
            let rest = line.get(at..)?;
            rest.split_whitespace().next().map(str::to_owned)
        })
}

/// Boot evecho with the input devices, send the events, and give every line
/// the guest printed after it said it was ready.
fn boot_and_send(arch: Arch, program: &Path, args: &Args) -> Result<(Vec<String>, Vec<String>)> {
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel = crate::cargo::build_kernel_with_init(arch, args.release, program, "")?;
    let natives = crate::native::build(arch, args.release)?;
    let initramfs = crate::initramfs::build(None, &natives, None, &[])?;
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;

    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.input = true;
    qemu_args.qmp_port = Some(port);

    let mut opened = Vec::new();
    let mut echoed = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        opened = watching.lines().to_vec();
        let keyboard = node_of(&opened, KEYBOARD)
            .ok_or_else(|| Error::new(format!("{arch}: evecho opened no `{KEYBOARD}`")))?;
        let tablet = node_of(&opened, TABLET)
            .ok_or_else(|| Error::new(format!("{arch}: evecho opened no `{TABLET}`")))?;
        println!("  {arch}: keyboard on {keyboard}, tablet on {tablet}");

        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        // One press and release of `a`, then the pointer put somewhere and
        // its left button pressed and released. Each call is one QMP command
        // and so one report, which QEMU ends with a `SYN_REPORT` of its own.
        for events in [
            vec![key("a", true)],
            vec![key("a", false)],
            vec![absolute("x", POINTER.0), absolute("y", POINTER.1)],
            vec![button("left", true)],
            vec![button("left", false)],
        ] {
            qmp.input_send_event(&events)?;
        }

        let wanted = wanted_lines(&keyboard, &tablet);
        let _ = watching.read_more(Instant::now() + PATIENCE, |seen| {
            wanted
                .iter()
                .all(|want| seen.iter().any(|line| line.contains(want.as_str())))
        })?;
        echoed = watching.after().to_vec();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, READY, hook)?;
    Ok((opened, echoed))
}

/// The lines the events must produce, in the nodes they must come from.
fn wanted_lines(keyboard: &str, tablet: &str) -> Vec<String> {
    vec![
        format!("evecho: {keyboard} EV_KEY KEY_A 1"),
        format!("evecho: {keyboard} EV_KEY KEY_A 0"),
        format!("evecho: {tablet} EV_ABS ABS_X {}", POINTER.0),
        format!("evecho: {tablet} EV_ABS ABS_Y {}", POINTER.1),
        format!("evecho: {tablet} EV_KEY BTN_LEFT 1"),
        format!("evecho: {tablet} EV_KEY BTN_LEFT 0"),
    ]
}

/// Which of `wanted` are missing from `seen`.
fn missing(wanted: &[String], seen: &[String]) -> Vec<String> {
    wanted
        .iter()
        .filter(|want| !seen.iter().any(|line| line.contains(want.as_str())))
        .cloned()
        .collect()
}

/// `test-input` on each architecture asked for that has virtio-input.
///
/// # Errors
///
/// An event that never arrived, a report the guest never finished, or a
/// negative control that passed the check it is built to fail.
pub(crate) fn test_input(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if crate::display::target(arch).is_none() {
            println!("  {arch}: no virtio-input in QEMU's machine; skipped");
            continue;
        }

        // Every build before the first boot, so a plan run, which stops at
        // the first boot, records the negative control too.
        let plain = build_evecho(arch, false)?;
        let negative = build_evecho(arch, true)?;
        let (opened, echoed) = boot_and_send(arch, &plain, args)?;
        let keyboard = node_of(&opened, KEYBOARD).unwrap_or_default();
        let tablet = node_of(&opened, TABLET).unwrap_or_default();
        let wanted = wanted_lines(&keyboard, &tablet);
        let lost = missing(&wanted, &echoed);
        if !lost.is_empty() {
            return Err(Error::new(format!(
                "{arch}: {} of {} events never reached a node; the first missing: `{}`",
                lost.len(),
                wanted.len(),
                lost.first().map_or("", String::as_str)
            )));
        }
        // A report ends with `SYN_REPORT`, which is how a reader knows the
        // events it has are a whole one; a path that delivered the events
        // and swallowed the sync would pass every check above.
        let synced = echoed
            .iter()
            .any(|line| line.contains(&format!("evecho: {keyboard} EV_SYN SYN_REPORT 0")));
        if !synced {
            return Err(Error::new(format!(
                "{arch}: the key arrived but no report was ever finished: no `SYN_REPORT` from \
                 {keyboard}"
            )));
        }
        println!(
            "  {arch}: a key and a touch put in at QEMU arrived whole at {keyboard} and {tablet}"
        );

        let (opened, echoed) = boot_and_send(arch, &negative, args)?;
        let keyboard = node_of(&opened, KEYBOARD).unwrap_or_default();
        let tablet = node_of(&opened, TABLET).unwrap_or_default();
        let lost = missing(&wanted_lines(&keyboard, &tablet), &echoed);
        if lost.is_empty() {
            return Err(Error::new(format!(
                "{arch}: the negative control passed the check it is built to fail"
            )));
        }
        // It must fail for the right reason: the events arrived, and were
        // reported under the wrong name.
        let wrong = format!("evecho: {keyboard} EV_KEY KEY_RESERVED 1");
        if !echoed.iter().any(|line| line.contains(&wrong)) {
            return Err(Error::new(format!(
                "{arch}: the negative control failed without ever reporting a key: it proves \
                 nothing about the check"
            )));
        }
        println!(
            "  {arch}: the negative control reported the key as KEY_RESERVED and failed the check"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{POINTER, absolute, button, key, missing, node_of, wanted_lines};

    #[test]
    fn a_devices_node_is_read_from_the_line_that_names_it() {
        let lines = [
            "  1.20 | evecho: event0 QEMU Virtio Keyboard [0x0006:0x0000] EV_SYN EV_KEY".to_owned(),
            "  1.21 | evecho: event1 QEMU Virtio Tablet [0x0006:0x0000] EV_SYN EV_ABS".to_owned(),
        ];
        assert_eq!(
            node_of(&lines, "QEMU Virtio Keyboard").as_deref(),
            Some("event0")
        );
        assert_eq!(
            node_of(&lines, "QEMU Virtio Tablet").as_deref(),
            Some("event1")
        );
        assert_eq!(node_of(&lines, "QEMU Virtio Mouse"), None);
    }

    /// A line naming the device in its own text, but not as an opened node,
    /// is not a node: the failure line `evecho: /dev/input/event3 failed` is
    /// one such.
    #[test]
    fn a_line_that_is_not_an_opened_node_is_not_one() {
        let lines = ["evecho: /dev/input/event3 failed: QEMU Virtio Keyboard".to_owned()];
        assert_eq!(node_of(&lines, "QEMU Virtio Keyboard"), None);
    }

    #[test]
    fn what_is_missing_is_what_no_line_holds() {
        let wanted = wanted_lines("event0", "event1");
        assert_eq!(missing(&wanted, &[]).len(), wanted.len());
        let seen: Vec<String> = wanted
            .iter()
            .map(|want| format!("  3.40 | {want}"))
            .collect();
        assert!(missing(&wanted, &seen).is_empty());
        assert_eq!(
            missing(&wanted, seen.get(1..).unwrap_or_default()),
            vec![wanted.first().cloned().unwrap_or_default()]
        );
    }

    #[test]
    fn an_event_is_the_json_qmp_takes() {
        assert_eq!(
            key("a", true),
            "{\"type\":\"key\",\"data\":{\"down\":true,\"key\":{\"type\":\"qcode\",\"data\":\"a\"}}}"
        );
        assert_eq!(
            button("left", false),
            "{\"type\":\"btn\",\"data\":{\"down\":false,\"button\":\"left\"}}"
        );
        assert_eq!(
            absolute("x", POINTER.0),
            "{\"type\":\"abs\",\"data\":{\"axis\":\"x\",\"value\":16384}}"
        );
    }
}
