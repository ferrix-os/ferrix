//! `test-compositor --boot tole-demo`: one of tole's demos
//! (`src/user/system/linux/tole/examples`, `docs/UI-FRAMEWORK.md`) in a
//! window of its own under hyprix, as the desktop comes up -- tole's
//! Wayland host on Ferrix with nothing of an app's around it.
//!
//! Required: the demo says it is up, and QEMU's screen shows it drawn --
//! the controls demo's ground (`Theme.bg2`, `#fafaf8`) over a large part of
//! the screen and its Save button's accent (`#2a62c9`). Then, by QEMU's
//! keyboard, what tole does with a desktop round it: the host field's
//! text copied and pasted back through hyprix's clipboard
//! (`ext_data_control_v1`), undone with Ctrl+Z, and the combo box's
//! list opened, taking the keys, and closed by Escape, giving them back.
//! The demo says where the keys are after each change.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::boot::{judge_still_running, judged_image, press, say_the_marker, with_the_transcript};
use super::{Carried, EITHER, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::Watching;
use crate::{Error, Result};

/// The boot's `hyprland.conf`: the demo as the desktop comes up.
const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --boot tole-demo`.
exec-once = /bin/tole-examples controls-light --window
";

/// What the demo says once its window has a size.
const UP: &str = "tole-examples: controls-light is up";

/// The demo's ground and its accent, red first.
const GROUND: [u8; 3] = [0xfa, 0xfa, 0xf8];
/// The accent: the highlighted Save button, the checked boxes.
const ACCENT: [u8; 3] = [0x2a, 0x62, 0xc9];

/// What the keys do, in order: the chords pressed, what the demo then
/// says, and what that shows.
const STEPS: &[(&[&[&str]], &str, &str)] = &[
    (
        &[&["ctrl", "a"], &["ctrl", "c"], &["end"], &["ctrl", "v"]],
        "keys on a text field holding \"nazuna.localnazuna.local\"; 0 popups open",
        "a copy went out to hyprix's clipboard and a paste came back from it",
    ),
    (
        &[&["ctrl", "z"]],
        "keys on a text field holding \"nazuna.local\"; 0 popups open",
        "Ctrl+Z undid the paste",
    ),
    (
        &[&["tab"], &["tab"], &["tab"], &["tab"]],
        "keys on an item; 0 popups open",
        "Tab reached the combo box",
    ),
    (
        &[&["spc"]],
        "keys on a popup; 1 popups open",
        "Space opened its list, which took the keys",
    ),
    (
        &[&["esc"]],
        "keys on an item; 0 popups open",
        "Escape closed it and gave the keys back to the box",
    ),
];

/// How long one step may take under emulation on a busy host.
const PATIENCE: Duration = Duration::from_secs(150);

/// Build the demos for `arch`: tole's workspace, its own target directory.
fn build_examples(arch: Arch) -> Result<PathBuf> {
    let target = crate::display::target(arch).ok_or_else(|| {
        Error::new(format!(
            "{arch} has no virtio-gpu in QEMU; the compositor test runs on x86_64 and aarch64"
        ))
    })?;
    let target_dir = paths::target_dir().join("tole");
    println!("  building tole's demos for {target}");
    let program = target_dir
        .join(target)
        .join("release")
        .join("tole-examples");
    crate::builds::Build::cargo(
        format!("cargo build (tole-examples) --target {target}"),
        paths::workspace_root().join("src/user/system/linux/tole"),
    )
    .args([
        "build",
        "--release",
        "-p",
        "tole-examples",
        "--target",
        target,
    ])
    .env("CARGO_TARGET_DIR", &target_dir)
    .output(&program)
    .run()?;
    Ok(program)
}

/// Build what the boot carries ahead of the first boot (`build_ahead`).
pub(super) fn build_ahead(arch: Arch) -> Result<()> {
    let _ = build_examples(arch)?;
    Ok(())
}

/// The demos' program and the tree's Inter, which they draw with.
fn carried(arch: Arch) -> Result<Carried> {
    let program = build_examples(arch)?;
    let bytes = std::fs::read(&program)
        .map_err(|error| Error::new(format!("{}: {error}", program.display())))?;
    let mut ports = vec![File {
        path: "bin/tole-examples".to_owned(),
        mode: 0o755,
        content: Content::Bytes(bytes),
    }];
    let fonts = paths::workspace_root().join("assets/fonts/inter");
    let listing = std::fs::read_dir(&fonts)
        .map_err(|error| Error::new(format!("{}: {error}", fonts.display())))?;
    for item in listing.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".ttf") {
            continue;
        }
        let bytes = std::fs::read(item.path())
            .map_err(|error| Error::new(format!("{}: {error}", item.path().display())))?;
        ports.push(File {
            path: format!("usr/share/fonts/inter/{name}"),
            mode: 0o644,
            content: Content::Bytes(bytes),
        });
    }
    Ok(Carried {
        ports,
        ..Carried::none()
    })
}

/// The demo's boot.
pub(super) fn test_tole_demo(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (image, kernel) = judged_image(arch, programs, CONFIG, carried(arch)?, None, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("tole-demo.ppm");
    let mut said = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        said = drive(arch, port, &dump, watching)?;
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    judge_still_running(arch, &said)?;
    let bytes =
        std::fs::read(&dump).map_err(|error| Error::new(format!("{}: {error}", dump.display())))?;
    let screen = crate::display::parse_ppm(&bytes)?;
    let count = |wanted: [u8; 3]| {
        screen
            .pixels
            .chunks_exact(3)
            .filter(|pixel| *pixel == wanted)
            .count()
    };
    let (ground, accent) = (count(GROUND), count(ACCENT));
    let whole = screen.width * screen.height;
    if ground * 5 < whole || accent < 300 {
        return Err(Error::new(format!(
            "{arch}: the screen does not show the controls demo: {ground} of {whole} pixels its ground, \
             {accent} its accent (the screen is {})",
            dump.display()
        )));
    }
    println!(
        "  {arch}: tole's controls demo is up in a window of its own: {ground} pixels of its ground, \
         {accent} of its accent (the screen is {})",
        dump.display()
    );
    Ok(())
}

/// Drive the boot, and answer everything the guest said.
fn drive(
    arch: Arch,
    port: u16,
    dump: &std::path::Path,
    watching: &mut Watching<'_>,
) -> Result<Vec<String>> {
    let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
    let up = watching.read_more(Instant::now() + SETTLE, |lines| {
        lines.iter().any(|line| line.contains(MARKER))
    })?;
    if !up && !watching.lines().iter().any(|line| line.contains(MARKER)) {
        return Err(Error::new(format!(
            "{arch}: the compositor never printed `{MARKER}`"
        )));
    }
    say_the_marker(watching, arch);
    let came = watching.read_more(Instant::now() + PATIENCE, |lines| {
        lines.iter().any(|line| line.contains(UP))
    })?;
    if !came {
        return Err(with_the_transcript(
            &Error::new(format!("{arch}: the demo never said `{UP}`")),
            watching,
        ));
    }
    println!("  {arch}: the demo is up");
    // A moment for its first frame to be shown.
    std::thread::sleep(Duration::from_secs(2));
    qmp.screendump(Some(DEVICE_ID), dump)?;
    for (keys, want, what) in STEPS {
        // Only what the demo says after the keys count: an earlier line
        // saying the same would pass a step that did nothing.
        let start = watching.after().len();
        for chord in *keys {
            press(&mut qmp, chord)?;
        }
        let said = watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().skip(start).any(|line| line.contains(want))
        })?;
        if !said {
            return Err(with_the_transcript(
                &Error::new(format!("{arch}: {what}: the demo never said `{want}`")),
                watching,
            ));
        }
        println!("  {arch}: {what}");
    }
    watching.read_what_was_said(Duration::from_secs(1))?;
    Ok(watching
        .lines()
        .iter()
        .chain(watching.after())
        .cloned()
        .collect())
}
