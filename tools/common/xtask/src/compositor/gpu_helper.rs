//! `test-compositor --boot gpu-helper`: hyprix-gpu, the compositor's
//! Vulkan helper (`docs/GPU-COMPOSITOR.md`), on Ferrix.
//!
//! Two things are run as the desktop comes up, both from `exec-once`:
//!
//! * the helper's own tests (`src/user/system/linux/hyprix-gpu`, built as a
//!   glibc test program against the yserver volume's sysroot), on lavapipe
//!   from that volume at `/data`, run by its loader, as tole's GPU demo is
//!   (`compositor::tole_demo`). They draw every painter call and compare it
//!   with the software painter, and draw a frame sent over a socket into an
//!   output that is then mapped and read -- a memfd, since lavapipe on
//!   Ferrix exports no dmabufs -- on Ferrix's kernel, its memfds and its
//!   sockets. Required: libtest's
//!   `test result: ok. 3 passed`, and no test skipped.
//! * the helper as an image carries it (`crate::hyprix_gpu`, under
//!   `/usr/lib/hyprix-gpu`, its own loader and anv): with no Intel GPU in
//!   the guest it can only say it found no device, which it does once its
//!   loader has found every library it needs. Required: `hyprix-gpu: no
//!   device`. Skipped, and said, on a machine without the intel-uapi
//!   volume.
//!
//! Skipped, and said, on an architecture other than x86-64, or without the
//! yserver volume.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::boot::{judge_still_running, judged_image, say_the_marker, with_the_transcript};
use super::{Carried, EITHER, MARKER, Programs};
use crate::args::Args;
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::{Error, Result};

/// The tests' program in the image.
const TESTS: &str = "bin/hyprix-gpu-tests";

/// What libtest says when both tests passed.
const PASSED: &str = "test result: ok. 3 passed";

/// What a test that found no lavapipe says.
const SKIPPED: &str = "; skipped";

/// What the packaged helper says when its loader found its libraries and
/// Vulkan found no device for it.
const NO_DEVICE: &str = "hyprix-gpu: no device";

/// How long the tests may take: lavapipe's first pipelines under TCG.
const PATIENCE: Duration = Duration::from_secs(300);

/// The boot's `hyprland.conf`: the tests on lavapipe, and the packaged
/// helper, which finds no Intel GPU.
const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --boot gpu-helper`.
env = VK_ICD_FILENAMES,/data/usr/share/vulkan/icd.d/lvp_icd.json
exec-once = /data/usr/lib64/ld-linux-x86-64.so.2 --library-path /data/usr/lib/x86_64-linux-gnu /bin/hyprix-gpu-tests --test-threads=1
";

/// The line that starts the packaged helper.
const PACKAGED: &str = "exec-once = /usr/libexec/hyprix-gpu\n";

/// Build the helper's tests as a glibc program; the test program's path.
fn build_tests() -> Result<PathBuf> {
    let sysroot = paths::volume_directory("yserver")?.join("sysroot");
    let target = "x86_64-unknown-linux-gnu";
    let target_dir = paths::target_dir().join("hyprix-gpu");
    println!("  building hyprix-gpu's tests for {target}");
    let flags = format!(
        "-C link-arg=--sysroot={0} -L native={0}/usr/lib/x86_64-linux-gnu",
        sysroot.display()
    );
    let out = std::process::Command::new(crate::cargo::cargo())
        .current_dir(paths::workspace_root().join("src/user/system/linux/hyprix-gpu"))
        .args([
            "test",
            "--release",
            "--lib",
            "--no-run",
            "--message-format=json-render-diagnostics",
            "--target",
            target,
        ])
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS", flags)
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|error| Error::new(format!("cargo test (hyprix-gpu): {error}")))?;
    if !out.status.success() {
        return Err(Error::new("cargo test --no-run (hyprix-gpu) failed"));
    }
    let said = String::from_utf8_lossy(&out.stdout);
    said.lines()
        .filter_map(|line| line.split("\"executable\":\"").nth(1))
        .filter_map(|rest| rest.split('"').next())
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .ok_or_else(|| Error::new("cargo test --no-run (hyprix-gpu) named no test program"))
}

/// Build what the boot carries ahead of the first boot.
pub(super) fn build_ahead(arch: Arch) -> Result<()> {
    if super::tole_demo::gpu_volume(arch).is_some() {
        let _ = build_tests()?;
    }
    Ok(())
}

/// The boot.
pub(super) fn test_gpu_helper(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let Some(volume) = super::tole_demo::gpu_volume(arch) else {
        return Ok(());
    };
    let mut args = args.clone();
    args.data_image = Some(volume);
    // The card the compositor needs to start at all.
    args.display = true;
    let tests = build_tests()?;
    let bytes = std::fs::read(&tests)
        .map_err(|error| Error::new(format!("{}: {error}", tests.display())))?;
    let mut ports = vec![File {
        path: TESTS.to_owned(),
        mode: 0o755,
        content: Content::Bytes(bytes),
    }];
    let mut config = CONFIG.to_owned();
    let packaged = match crate::intel_uapi::volume() {
        Ok(_) => {
            ports.extend(crate::hyprix_gpu::carried()?);
            config.push_str(PACKAGED);
            true
        }
        Err(why) => {
            println!("  {arch}: the packaged helper is not carried: {why}");
            false
        }
    };
    let carried = Carried {
        ports,
        ..Carried::none()
    };
    let (image, kernel) = judged_image(arch, programs, &config, carried, None, &args)?;
    let mut said = Vec::new();
    let hook = |watching: &mut crate::qemu::Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let up = watching.read_more(Instant::now() + super::SETTLE, |lines| {
            lines.iter().any(|line| line.contains(MARKER))
        })?;
        if !up && !watching.lines().iter().any(|line| line.contains(MARKER)) {
            return Err(Error::new(format!(
                "{arch}: the compositor never printed `{MARKER}`"
            )));
        }
        say_the_marker(watching, arch);
        let done = watching.read_more(Instant::now() + PATIENCE, |lines| {
            let tests = lines.iter().any(|line| line.contains("test result:"));
            let helper = !packaged || lines.iter().any(|line| line.contains(NO_DEVICE));
            tests && helper
        })?;
        if !done {
            return Err(with_the_transcript(
                &Error::new(format!(
                    "{arch}: the helper's tests or the packaged helper never finished"
                )),
                watching,
            ));
        }
        watching.read_what_was_said(Duration::from_secs(1))?;
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &args, EITHER, hook)?;
    judge_still_running(arch, &said)?;
    judge(arch, &said, packaged)
}

/// What the guest said must hold both tests passed, none skipped, and --
/// when it was carried -- the packaged helper's word that it ran.
fn judge(arch: Arch, said: &[String], packaged: bool) -> Result<()> {
    if let Some(line) = said.iter().find(|line| line.contains(SKIPPED)) {
        return Err(Error::new(format!(
            "{arch}: a helper test was skipped: {}",
            line.trim()
        )));
    }
    if !said.iter().any(|line| line.contains(PASSED)) {
        let result = said
            .iter()
            .find(|line| line.contains("test result:"))
            .map_or("no result", |line| line.trim());
        return Err(Error::new(format!(
            "{arch}: the helper's tests on lavapipe did not pass: {result}"
        )));
    }
    println!("  {arch}: hyprix-gpu's tests passed on lavapipe on Ferrix");
    if packaged {
        if !said.iter().any(|line| line.contains(NO_DEVICE)) {
            return Err(Error::new(format!(
                "{arch}: the packaged helper never said `{NO_DEVICE}`: its loader or a library is missing"
            )));
        }
        println!("  {arch}: the packaged helper started from its own loader and libraries");
    }
    Ok(())
}

/// What hyprix says when a screen's frames are drawn by the helper on
/// lavapipe.
const DRAWN_BY_HELPER: &str = "frames are drawn by hyprix-gpu on llvmpipe";

/// `test-compositor --boot helper-desktop`'s lines beside the GPU boot's
/// own (`drawing::GPU_CONFIG`): hyprix asked for the helper, and the
/// helper given the yserver volume's lavapipe through the environment
/// hyprix hands it -- its own loader and libraries are the image's
/// (`crate::hyprix_gpu`), lavapipe's and LLVM's are found on the volume.
const HELPER_LINES: &str = "\
hyprix:gpu = on
env = VK_ICD_FILENAMES,/data/usr/share/vulkan/icd.d/lvp_icd.json
env = LD_LIBRARY_PATH,/data/usr/lib/x86_64-linux-gnu
";

/// `test-compositor --boot helper-desktop`: hyprix drawing its frames
/// through hyprix-gpu end to end (`docs/GPU-COMPOSITOR.md`), on lavapipe
/// in the guest -- the protocol, the arena, the helper's Vulkan backend,
/// its memfd outputs and hyprix's reading them back -- judged as the
/// virgl boot is (`test-compositor --gl`): the guest's own screenshot of
/// the decorated pair against the picture the software renderer blesses,
/// hyprix saying the helper draws, and never that it fell back.
///
/// Skipped, and said, where the yserver or intel-uapi volume is missing,
/// or on an architecture other than x86-64.
pub(super) fn test_helper_desktop(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let Some(volume) = super::tole_demo::gpu_volume(arch) else {
        return Ok(());
    };
    if let Err(why) = crate::intel_uapi::volume() {
        println!("  {arch}: helper-desktop skipped: {why}");
        return Ok(());
    }
    let mut args = args.clone();
    args.data_image = Some(volume);
    let config = format!("{HELPER_LINES}{}", super::drawing::GPU_CONFIG);
    super::drawing::gpu_picture(
        arch,
        programs,
        &args,
        (&config, crate::hyprix_gpu::carried()?),
        &[],
        DRAWN_BY_HELPER,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn both_passed_and_the_helper_ran_is_a_pass() {
        let said = lines(&[
            "test result: ok. 3 passed; 0 failed; 0 ignored",
            "hyprix-gpu: no device: no Vulkan 1.3 device",
        ]);
        assert!(judge(Arch::X86_64, &said, true).is_ok());
    }

    #[test]
    fn a_skipped_test_fails_the_boot() {
        let said = lines(&[
            "the_scene: no lavapipe (no Vulkan driver); skipped",
            "test result: ok. 3 passed; 0 failed",
        ]);
        assert!(judge(Arch::X86_64, &said, false).is_err());
    }

    #[test]
    fn a_failed_test_or_a_silent_helper_fails_the_boot() {
        let failed = lines(&["test result: FAILED. 1 passed; 1 failed"]);
        assert!(judge(Arch::X86_64, &failed, false).is_err());
        let silent = lines(&["test result: ok. 3 passed; 0 failed"]);
        assert!(judge(Arch::X86_64, &silent, true).is_err());
    }
}
