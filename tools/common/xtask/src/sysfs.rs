//! `test-sysfs`: sysfs as a running machine shows it, and `bind` and
//! `unbind` going to `devmgr` and back (`docs/SYSFS.md` §5 and §7).
//!
//! The kernel's boot check walks a sysfs before init runs, with the disks
//! and interfaces devmgr's drivers published. This is the half that needs a
//! shell and the devices only a desktop machine has. The gate boots zinc as
//! an interactive shell on a machine with a virtio-gpu, a keyboard, a tablet
//! and a network adapter, and types at it:
//!
//! 1. what libdrm reads to find a card -- `/sys/dev/char/226:0/device`'s
//!    vendor and device, and its `drm` directory -- and a connector's
//!    `status`;
//! 2. an input device's `name` and `capabilities/ev`, the network adapter's
//!    `address` and `operstate`, a disk's `size` and the processors online;
//! 3. a write to a read-only value, which `openat` refuses;
//! 4. the GPU's slot from `/sys/bus/pci/drivers/gpu`, written to `unbind`:
//!    devmgr stops the driver, the kernel says so, the card leaves `/dev/dri`
//!    and the device its `driver` link; `unbind` again is refused;
//! 5. the slot written to `bind`: devmgr starts the driver again, the card
//!    comes back and so does the link; `bind` again is refused, the device
//!    being bound;
//! 6. a line only a live shell can answer.
//!
//! x86-64 only, for the reason `test-restart` is: the shell's test and `echo`
//! are zinc's, but the image is built the same way, carrying uutils, which
//! are built for x86-64 alone (`docs/UUTILS.md` D3).

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, cargo, fat, initramfs, native, qemu, uutils, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-sysfs",
    help_at: 530,
    help: &[
        "test-sysfs    Boot a shell beside a card, input devices and a network adapter, read sysfs, and unbind and bind the card through it",
    ],
    run: |_, args| test_sysfs(args),
}];

/// How long to wait for the answer to one line.
const PATIENCE: Duration = Duration::from_secs(30);

/// What the kernel prints when a request through sysfs is done.
const ASKED: &str = "asked through sysfs: ";

/// What the kernel prints when a card is published.
const PUBLISHED: &str = "display  card0 is";

/// What the kernel prints when the first input device is published.
const KEYBOARD_PUBLISHED: &str = "input    event0 ";

/// A line typed at the shell and the tag its answer is printed after. Each
/// line prints `sysfs-gate-<tag>` with a quote in the middle of the tag, so
/// the console's echo of the typed line never matches it.
struct Ask {
    /// What is typed.
    line: String,
    /// What the answer is printed after.
    tag: &'static str,
}

impl Ask {
    fn new(line: &str, tag: &'static str) -> Ask {
        Ask {
            line: format!("{line}\n"),
            tag,
        }
    }
}

/// Boot a shell beside a card, a keyboard, a tablet and a network adapter,
/// read sysfs, and unbind and bind the card's driver through it.
///
/// # Errors
///
/// When the image cannot be built, when the boot fails or panics, or when an
/// answer is not what the device and devmgr say.
pub(crate) fn test_sysfs(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-sysfs runs on x86-64 only: the image carries uutils, which are built for \
             x86-64 alone (docs/UUTILS.md D3)",
        ));
    }
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose init is an interactive shell");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, "")?;
    let natives = native::build(arch, args.release)?;
    let utilities = uutils::carried(arch)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let carried = crate::apps::ported(arch, args)?;
    let archive = initramfs::build_with_utilities(
        Some(&shell),
        &natives,
        Some(&bytes),
        &utilities,
        &carried,
    )?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;

    // A card, and with it a keyboard and a tablet; a network adapter.
    let mut booted = args.clone();
    booted.display = true;
    booted.net = true;
    println!(
        "  {arch}: reading sysfs, and unbinding and binding the card's driver through it \
         (timeout {}s)",
        args.timeout
    );
    let mut failures: Vec<String> = Vec::new();
    let lines = qemu::watch_then(
        arch,
        &image,
        &kernel,
        &booted,
        qemu::SUCCESS_MARKER,
        |watching| {
            // The shell at its prompt is left there: nothing powers off.
            watching.stop_when_done();
            if let Err(failure) = ready(watching) {
                failures.push(failure);
                return Ok(());
            }
            if let Err(failure) = read_the_tree(watching) {
                failures.push(failure);
                return Ok(());
            }
            if let Err(failure) = unbind_and_bind(watching) {
                failures.push(failure);
            }
            Ok(())
        },
    )?;
    if let Some(panic) = lines.iter().find(|line| line.contains(qemu::PANIC_MARKER)) {
        failures.push(format!("the kernel panicked: {}", panic.trim()));
    }
    if !failures.is_empty() {
        let mut message = format!("{arch}: sysfs was not what the machine is:\n");
        for failure in &failures {
            message.push_str("    - ");
            message.push_str(failure);
            message.push('\n');
        }
        message.push_str("  The whole transcript is above and in the serial log.");
        return Err(Error::new(message));
    }
    println!(
        "  {arch}: sysfs shows the card, the input devices, the adapter and the disks, and a \
         driver unbound and bound through it went and came back"
    );
    Ok(())
}

/// Wait for what the first keystroke needs: the card and the keyboard the
/// tree is read for published, which the kernel says as each driver's READY
/// goes out, and the shell reading the console.
fn ready(watching: &mut qemu::Watching<'_>) -> std::result::Result<(), String> {
    let deadline = Instant::now() + PATIENCE;
    let said = |lines: &[String], want: &str| lines.iter().any(|line| line.contains(want));
    for want in [PUBLISHED, KEYBOARD_PUBLISHED] {
        if said(watching.lines(), want) {
            continue;
        }
        let published = watching
            .read_more(deadline, |lines| said(lines, want))
            .map_err(|error| error.to_string())?;
        if !published {
            return Err(format!("the kernel never said `{want}`"));
        }
    }
    if watching
        .wait_for_shell(deadline)
        .map_err(|error| error.to_string())?
    {
        Ok(())
    } else {
        Err("the shell never answered at the console".to_owned())
    }
}

/// Type `ask`'s line and wait for its answer: the rest of the first line
/// printed after its tag.
fn answer(watching: &mut qemu::Watching<'_>, ask: &Ask) -> std::result::Result<String, String> {
    let before = watching.after().len();
    let tag = format!("sysfs-gate-{}", ask.tag);
    watching
        .type_in(ask.line.as_bytes())
        .map_err(|error| error.to_string())?;
    let found = |lines: &[String]| {
        lines.iter().find_map(|line| {
            let (_, rest) = line.split_once(&tag)?;
            Some(rest.trim_end().to_owned())
        })
    };
    let answered = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            found(lines.get(before..).unwrap_or_default()).is_some()
        })
        .map_err(|error| error.to_string())?;
    let said = answered
        .then(|| found(watching.after().get(before..).unwrap_or_default()))
        .flatten();
    said.ok_or_else(|| format!("`{}` was never answered", ask.line.trim_end()))
}

/// Require `ask`'s answer to satisfy `good`, which names what it wanted.
fn expect(
    watching: &mut qemu::Watching<'_>,
    ask: &Ask,
    good: impl Fn(&str) -> bool,
    wanted: &str,
) -> std::result::Result<String, String> {
    let said = answer(watching, ask)?;
    if good(&said) {
        Ok(said)
    } else {
        Err(format!(
            "`{}` answered {said:?}, where {wanted} was wanted",
            ask.line.trim_end()
        ))
    }
}

/// Steps 1 to 3: the tree as the drivers and devmgr published it.
fn read_the_tree(watching: &mut qemu::Watching<'_>) -> std::result::Result<(), String> {
    let reads = [
        (
            Ask::new(
                "read v < /sys/dev/char/226:0/device/vendor; \
                 read d < /sys/dev/char/226:0/device/device; echo sysfs-gate-'card='$v:$d",
                "card=",
            ),
            "0x1af4:0x1050, a virtio-gpu's vendor and device",
        ),
        (
            Ask::new(
                "for f in /sys/dev/char/226:0/device/drm/*; do \
                 echo sysfs-gate-'drm='${f#/sys/dev/char/226:0/device/drm/}; done",
                "drm=",
            ),
            "card0 in the card's device's drm directory",
        ),
        (
            Ask::new(
                "read s < /sys/class/drm/card0-Virtual-1/status; echo sysfs-gate-'connector='$s",
                "connector=",
            ),
            "connected",
        ),
        (
            Ask::new(
                "read n < /sys/class/input/event0/device/name; echo sysfs-gate-'input='$n",
                "input=",
            ),
            "the name QEMU gives a virtio input device",
        ),
        (
            Ask::new(
                "read e < /sys/class/input/event0/device/capabilities/ev; \
                 echo sysfs-gate-'ev='$e",
                "ev=",
            ),
            "a bitmap with EV_SYN set",
        ),
        (
            Ask::new(
                "read a < /sys/class/net/eth0/address; read o < /sys/class/net/eth0/operstate; \
                 echo sysfs-gate-'net='$a/$o",
                "net=",
            ),
            "the adapter's address, and up",
        ),
        (
            Ask::new(
                "read s < /sys/block/vda/size; echo sysfs-gate-'size='$s",
                "size=",
            ),
            "vda's size in sectors",
        ),
        (
            Ask::new(
                "read c < /sys/devices/system/cpu/online; echo sysfs-gate-'cpus='$c",
                "cpus=",
            ),
            "the processors online, from 0",
        ),
        (
            Ask::new(
                "echo 1 > /sys/devices/system/cpu/online || echo sysfs-gate-'readonly=refused'",
                "readonly=",
            ),
            "the write refused",
        ),
    ];
    let checks: [&dyn Fn(&str) -> bool; 9] = [
        &|said| said == "0x1af4:0x1050",
        &|said| said == "card0",
        &|said| said == "connected",
        &|said| said.starts_with("QEMU Virtio"),
        &|said| u64::from_str_radix(said, 16).is_ok_and(|bits| bits & 1 == 1),
        &|said| said == format!("{}/up", qemu::GUEST_MAC),
        &|said| said.parse::<u64>().is_ok_and(|sectors| sectors > 0),
        &|said| said.starts_with('0'),
        &|said| said == "refused",
    ];
    for ((ask, wanted), good) in reads.iter().zip(checks) {
        let _ = expect(watching, ask, good, wanted)?;
    }
    Ok(())
}

/// Wait for the kernel's line that a request through sysfs is done, or
/// refused, after `before`.
fn kernel_said(
    watching: &mut qemu::Watching<'_>,
    before: usize,
    what: &str,
) -> std::result::Result<String, String> {
    let wanted = |lines: &[String]| {
        lines.iter().find_map(|line| {
            let (_, rest) = line.split_once(ASKED)?;
            line.contains(what).then(|| rest.trim_end().to_owned())
        })
    };
    let heard = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            wanted(lines.get(before..).unwrap_or_default()).is_some()
        })
        .map_err(|error| error.to_string())?;
    heard
        .then(|| wanted(watching.after().get(before..).unwrap_or_default()))
        .flatten()
        .ok_or_else(|| format!("the kernel never said how the {what} through sysfs ended"))
}

/// Steps 4 to 6: the card's driver unbound and bound again through sysfs.
fn unbind_and_bind(watching: &mut qemu::Watching<'_>) -> std::result::Result<(), String> {
    let slot = expect(
        watching,
        &Ask::new(
            "for d in /sys/bus/pci/drivers/gpu/0*; do \
             echo sysfs-gate-'slot='${d#/sys/bus/pci/drivers/gpu/}; done",
            "slot=",
        ),
        |said| said.len() == 12 && said.contains(':'),
        "the card's PCI slot, listed in gpu's driver directory",
    )?;
    let unbind = "/sys/bus/pci/drivers/gpu/unbind";
    let bind = "/sys/bus/pci/drivers/gpu/bind";

    let before = watching.after().len();
    let _ = expect(
        watching,
        &Ask::new(
            &format!("echo {slot} > {unbind} && echo sysfs-gate-'unbind=done'"),
            "unbind=",
        ),
        |said| said == "done",
        "the unbind done",
    )?;
    let said = kernel_said(watching, before, "unbind of gpu")?;
    if said != "done" {
        return Err(format!("the kernel said the unbind ended {said:?}"));
    }
    let _ = expect(
        watching,
        &Ask::new(
            &format!(
                "[ -e /dev/dri/card0 ] || [ -e /sys/bus/pci/devices/{slot}/driver ] || \
                 echo sysfs-gate-'after-unbind=gone'"
            ),
            "after-unbind=",
        ),
        |said| said == "gone",
        "card0 and the driver link gone",
    )?;
    let _ = expect(
        watching,
        &Ask::new(
            &format!("echo {slot} > {unbind} || echo sysfs-gate-'unbind-again=refused'"),
            "unbind-again=",
        ),
        |said| said == "refused",
        "a second unbind refused, the device having no driver",
    )?;

    let before = watching.after().len();
    let _ = expect(
        watching,
        &Ask::new(
            &format!("echo {slot} > {bind} && echo sysfs-gate-'bind=done'"),
            "bind=",
        ),
        |said| said == "done",
        "the bind done",
    )?;
    let said = kernel_said(watching, before, "bind of gpu")?;
    if said != "done" {
        return Err(format!("the kernel said the bind ended {said:?}"));
    }
    let published = watching
        .after()
        .get(before..)
        .unwrap_or_default()
        .iter()
        .any(|line| line.contains(PUBLISHED));
    if !published {
        return Err("the card was not published again after the bind".to_owned());
    }
    let _ = expect(
        watching,
        &Ask::new(
            &format!(
                "[ -e /dev/dri/card0 ] && [ -e /sys/bus/pci/devices/{slot}/driver ] && \
                 echo sysfs-gate-'after-bind=back'"
            ),
            "after-bind=",
        ),
        |said| said == "back",
        "card0 and the driver link back",
    )?;
    let _ = expect(
        watching,
        &Ask::new(
            &format!("echo {slot} > {bind} || echo sysfs-gate-'bind-again=refused'"),
            "bind-again=",
        ),
        |said| said == "refused",
        "a second bind refused, the device being bound",
    )?;
    let _ = expect(
        watching,
        &Ask::new("echo sysfs-gate-'alive='$((6 * 7))", "alive="),
        |said| said == "42",
        "42, from a live shell",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Ask;

    #[test]
    fn a_typed_line_does_not_carry_its_own_answer() {
        // The console echoes what was typed; every tag is split by a quote.
        for ask in [
            Ask::new("echo sysfs-gate-'alive='$((6 * 7))", "alive="),
            Ask::new(
                "read s < /sys/block/vda/size; echo sysfs-gate-'size='$s",
                "size=",
            ),
        ] {
            assert!(!ask.line.contains(&format!("sysfs-gate-{}", ask.tag)));
        }
    }
}
