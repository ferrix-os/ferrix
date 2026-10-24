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
//!
//! The demos are a static program, as every Ferrix client is, and are
//! asked to draw on the GPU (`TOLE_RENDERER=gpu`): a static program
//! cannot load Vulkan, so the boot also requires that it said so and drew
//! on the CPU -- the fallback a program without a GPU takes.
//!
//! `test-compositor --boot tole-demo-gpu` is the same demo drawn by tole's
//! GPU renderer, on Mesa's lavapipe: built as a glibc program against the
//! yserver volume's Debian sysroot and run from the volume (`/data`) by its
//! loader, as yserver is, with lavapipe's ICD. It requires the demo to say
//! it drew on lavapipe, and the same screen and keys.

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
env = TOLE_RENDERER,gpu
exec-once = /bin/tole-examples controls-light --window
";

/// The GPU boot's `hyprland.conf`: the glibc demos on the volume's loader,
/// lavapipe their Vulkan.
const CONFIG_GPU: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --boot tole-demo-gpu`.
env = TOLE_RENDERER,gpu
env = VK_ICD_FILENAMES,/data/usr/share/vulkan/icd.d/lvp_icd.json
exec-once = /data/usr/lib64/ld-linux-x86-64.so.2 --library-path /data/usr/lib/x86_64-linux-gnu /bin/tole-examples controls-light --window
";

/// What the static demos say when asked for the GPU.
const FELL_BACK: &str = "tole: drawing on the CPU: no Vulkan library";

/// What the demo says it drew with: the static one on the CPU, the glibc
/// one on lavapipe.
const DRAWN_ON_CPU: &str = "drawn by cpu";
/// The same, on the GPU boot.
const DRAWN_ON_LAVAPIPE: &str = "drawn by gpu: llvmpipe";

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

/// Build the demos as an x86-64 glibc program against the yserver volume's
/// Debian sysroot, so they run on its glibc and load its Mesa.
fn build_examples_gnu() -> Result<PathBuf> {
    let sysroot = paths::volume_directory("yserver")?.join("sysroot");
    if !sysroot.join("usr/lib/x86_64-linux-gnu/libc.so.6").is_file() {
        return Err(Error::new(format!(
            "{} has no glibc: tools/common/fetch/fetch-yserver.sh makes the sysroot",
            sysroot.display()
        )));
    }
    let target = "x86_64-unknown-linux-gnu";
    let target_dir = paths::target_dir().join("tole-gnu");
    println!(
        "  building tole's demos for {target} against {}",
        sysroot.display()
    );
    let program = target_dir
        .join(target)
        .join("release")
        .join("tole-examples");
    // The sysroot's libraries first: the C compiler adds its own install's
    // library directories outside the sysroot, which hold the host's
    // glibc, newer than the volume's.
    let flags = format!(
        "-C link-arg=--sysroot={0} -L native={0}/usr/lib/x86_64-linux-gnu",
        sysroot.display()
    );
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
    .env("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS", flags)
    .output(&program)
    .run()?;
    Ok(program)
}

/// Build what the GPU boot carries ahead of the first boot, when it will
/// run.
pub(super) fn build_ahead_gpu(arch: Arch) -> Result<()> {
    if gpu_volume(arch).is_some() {
        let _ = build_examples_gnu()?;
    }
    Ok(())
}

/// The yserver volume, which holds lavapipe, when the GPU boot can run on
/// `arch`; why not said when it cannot.
fn gpu_volume(arch: Arch) -> Option<PathBuf> {
    if arch != Arch::X86_64 {
        println!(
            "  {arch}: tole-demo-gpu skipped: the yserver volume, which holds lavapipe, is x86-64"
        );
        return None;
    }
    // Only the volume's glibc and Mesa are used, which every yserver
    // volume holds, so it need not hold the yserver commit pinned.
    let directory = match std::env::var_os("FERRIX_YSERVER_VOLUME") {
        Some(directory) => Ok(PathBuf::from(directory)),
        None => paths::volume_directory("yserver"),
    };
    match directory.map(|d| d.join("yserver.img")) {
        Ok(image) if image.is_file() => Some(image),
        Ok(image) => {
            println!(
                "  {arch}: tole-demo-gpu skipped: {} is not there (tools/common/fetch/fetch-yserver.sh makes it)",
                image.display()
            );
            None
        }
        Err(why) => {
            println!("  {arch}: tole-demo-gpu skipped: {why}");
            None
        }
    }
}

/// Build what the boot carries ahead of the first boot (`build_ahead`).
pub(super) fn build_ahead(arch: Arch) -> Result<()> {
    let _ = build_examples(arch)?;
    Ok(())
}

/// The demos' program and the tree's Inter, which they draw with.
fn carried(arch: Arch) -> Result<Carried> {
    carried_program(&build_examples(arch)?)
}

/// `program` as `/bin/tole-examples`, and the tree's Inter.
fn carried_program(program: &std::path::Path) -> Result<Carried> {
    let bytes = std::fs::read(program)
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

/// The demo's boot: the static demos, asked for the GPU, drawing on the
/// CPU.
pub(super) fn test_tole_demo(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (image, kernel) = judged_image(arch, programs, CONFIG, carried(arch)?, None, args)?;
    let said = run_demo(arch, &image, &kernel, args)?;
    if !said.iter().any(|line| line.contains(FELL_BACK)) {
        return Err(Error::new(format!(
            "{arch}: the static demo, asked for the GPU, never said `{FELL_BACK}`"
        )));
    }
    judge_drawn_by(arch, &said, DRAWN_ON_CPU)?;
    println!("  {arch}: asked for the GPU, the static demo said why not and drew on the CPU");
    Ok(())
}

/// The GPU boot: the glibc demos on lavapipe from the yserver volume.
///
/// Skipped, and said, where it cannot run: on an architecture other than
/// x86-64, whose volume it is, or on a machine without the volume.
pub(super) fn test_tole_demo_gpu(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let Some(volume) = gpu_volume(arch) else {
        return Ok(());
    };
    let mut args = args.clone();
    args.data_image = Some(volume);
    let carried = carried_program(&build_examples_gnu()?)?;
    let (image, kernel) = judged_image(arch, programs, CONFIG_GPU, carried, None, &args)?;
    let said = run_demo(arch, &image, &kernel, &args)?;
    judge_drawn_by(arch, &said, DRAWN_ON_LAVAPIPE)?;
    println!("  {arch}: tole's GPU renderer drew the demo on lavapipe");
    Ok(())
}

/// What the demo said it drew with must hold `wanted`.
fn judge_drawn_by(arch: Arch, said: &[String], wanted: &str) -> Result<()> {
    match said.iter().find(|line| line.contains(UP)) {
        Some(line) if line.contains(wanted) => {
            println!("  {arch}: {}", line.trim());
            Ok(())
        }
        Some(line) => Err(Error::new(format!(
            "{arch}: the demo was not {wanted}: {}",
            line.trim()
        ))),
        None => Err(Error::new(format!("{arch}: the demo never said `{UP}`"))),
    }
}

/// Boot `image`, drive the demo and judge its screen; answers what the
/// guest said.
fn run_demo(
    arch: Arch,
    image: &std::path::Path,
    kernel: &std::path::Path,
    args: &Args,
) -> Result<Vec<String>> {
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
    let _ = crate::qemu::watch_then(arch, image, kernel, &qemu_args, EITHER, hook)?;
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
    Ok(said)
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
