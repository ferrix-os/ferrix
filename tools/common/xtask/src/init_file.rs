//! `ferrix.init=` and `reboot(2)`'s commit, the kernel's half of the init
//! (`docs/INIT.md` §11, K0 and K7), checked by `test-shell` after its
//! built-in boot.
//!
//! # K0: the shell from a file
//!
//! The same shell and the same script as the built-in boot, started from
//! files instead: the shell at `/bin/sh`, the script at [`SCRIPT_FILE`] behind
//! a `#!/bin/sh` line, and `ferrix.init=/etc/shell-test` on the command line.
//! The kernel still has the shell and the script built in, as the fallback a
//! missing file falls back to, so the script's lines alone cannot tell which
//! ran. The kernel's own lines can: it must say it started
//! `/bin/sh /etc/shell-test`, that file must exit with the script's status,
//! and the fallback line must not be there.
//!
//! # K7: `reboot(2)` commits `/data` and `/home`
//!
//! Where the shell is busybox, whose `poweroff -f -n` makes the call with no
//! `sync` of its own first; zinc has no way to make it. Two boots of the same
//! two volumes, a fresh copy of the blank fixture attached at `/data` and of
//! the home fixture, which the kernel mounts at `/home` by its label:
//!
//! 1. `ferrix.init=/etc/k7-write`: write `/data/k7` and `/home/k7`, then
//!    `poweroff -f -n`.
//!    A test boot has no root disk and so no committer, so nothing but the
//!    call itself can commit the file before the power goes.
//! 2. `ferrix.init=/etc/k7-read ferrix.onexit=panic`: read both back and exit
//!    0, after which the kernel must panic with `FX-1501` -- §8.3's option,
//!    checked on the same boot because it costs nothing more.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::native;
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::{self, SUCCESS_MARKER};
use crate::{Error, Result, btrfs_disk, fat, initramfs, shell};

/// Where the script is, and what `ferrix.init=` names on the first boot.
pub(crate) const SCRIPT_FILE: &str = "/etc/shell-test";

/// The second and third boots' programs.
const K7_WRITE: &str = "/etc/k7-write";
const K7_READ: &str = "/etc/k7-read";

/// The volumes both K7 boots attach, under `build/`.
const K7_VOLUME: &str = "k7-data.img";
const K7_HOME: &str = "k7-home.img";

/// What the first K7 boot writes and the second must read back.
const K7_TEXT: &str = "committed by reboot(2)";

/// What the kernel says when a named file will not start.
const FELL_BACK: &str = "falling back to the built-in program";

/// What the kernel says once it has mounted a data disk.
const DATA_MOUNTED: &str = "mounted writable at /data";

/// What the kernel says once it has mounted the home disk.
const HOME_MOUNTED: &str = "mounted writable at /home";

/// The files the K7 boots write and read back.
const K7_FILES: [&str; 2] = ["/data/k7", "/home/k7"];

/// What `reboot(2)` says as it powers the machine off, Linux's line.
const POWER_DOWN: &str = "reboot: Power down";

/// The catalog code of the panic `ferrix.onexit=panic` asks for.
const INIT_EXITED: &str = "FX-1501";

/// What every boot here is made from: the built-in boot's loader, kernel and
/// native programs, the shell it ran, and what a dynamically linked shell
/// needs beside it.
///
/// The kernel is the one with the shell and the script built in, which is
/// what a named file that will not start falls back to.
#[derive(Debug)]
pub(crate) struct Parts<'a> {
    pub(crate) arch: Arch,
    pub(crate) loader: &'a Path,
    pub(crate) kernel: &'a crate::cargo::Kernel,
    pub(crate) natives: &'a [native::Built],
    pub(crate) program: &'a Path,
    pub(crate) carried: &'a [File],
}

/// Boot the shell from a file by `ferrix.init=` and judge it; then, where
/// `busybox` says the shell has a `poweroff`, the two K7 boots.
///
/// # Errors
///
/// A boot that panicked or timed out, and every line the judges below want
/// and did not get, with the serial log's path.
/// Verifies: `L.x86_64.98`, H.BOOT.6
pub(crate) fn test(parts: &Parts<'_>, args: &Args, busybox: bool) -> Result<()> {
    let (arch, kernel) = (parts.arch, parts.kernel);
    let program = parts.program;
    let shell = std::fs::read(program)
        .map_err(|error| Error::new(format!("reading {}: {error}", program.display())))?;

    println!("  {arch}: starting the shell from {SCRIPT_FILE}, as ferrix.init= names it");
    let script = format!("#!/bin/sh\n{}", shell::SCRIPT);
    let image = parts.image(
        &shell,
        SCRIPT_FILE,
        &script,
        &qemu::init_option(SCRIPT_FILE),
        false,
    )?;
    let lines = qemu::watch_to_power_off(arch, &image, kernel, args, SUCCESS_MARKER)?;
    judge_from_file(after_marker(&lines)).map_err(|why| failed(arch, &why))?;
    println!(
        "  {arch}: ferrix.init= started the shell from {SCRIPT_FILE}, and it exited with {}",
        shell::STATUS
    );

    if !busybox {
        println!(
            "  {arch}: reboot(2)'s commit is checked under busybox, whose poweroff zinc lacks"
        );
        return Ok(());
    }
    let volume = btrfs_disk::blank_copy(arch, K7_VOLUME)?;
    let mut with_volume = args.clone();
    with_volume.data_image = Some(volume);
    with_volume.data_image_kept = true;
    with_volume.home_image = Some(btrfs_disk::home_copy(arch, K7_HOME)?);
    println!("  {arch}: writing /data/k7 and /home/k7, then poweroff -f -n, which does not sync");
    let image = parts.image(
        &shell,
        K7_WRITE,
        &write_script(),
        &qemu::init_option(K7_WRITE),
        true,
    )?;
    let lines = qemu::watch_to_power_off(arch, &image, kernel, &with_volume, SUCCESS_MARKER)?;
    judge_k7_write(&lines).map_err(|why| failed(arch, &why))?;

    println!("  {arch}: reading /data/k7 and /home/k7 back, under ferrix.onexit=panic");
    with_volume.data_image_kept = false;
    let cmdline = format!("{} ferrix.onexit=panic", qemu::init_option(K7_READ));
    let image = parts.image(&shell, K7_READ, &read_script(), &cmdline, false)?;
    let deadline = Instant::now() + Duration::from_secs(args.timeout);
    let lines = qemu::watch_then(arch, &image, kernel, &with_volume, SUCCESS_MARKER, |at| {
        let _ = at.read_more(deadline, |lines| {
            lines.iter().any(|l| l.contains(INIT_EXITED))
        })?;
        Ok(())
    })?;
    judge_k7_read(after_marker(&lines)).map_err(|why| failed(arch, &why))?;
    println!(
        "  {arch}: /data/k7 and /home/k7 survived poweroff -f -n, and init's exit panicked with \
         {INIT_EXITED}"
    );
    Ok(())
}

impl Parts<'_> {
    /// An image whose initramfs carries `shell` at `/bin/sh` -- and at
    /// `/bin/poweroff` too when `poweroff` -- and `script` at `path`, with
    /// `options` as its command line.
    fn image(
        &self,
        shell: &[u8],
        path: &str,
        script: &str,
        options: &str,
        poweroff: bool,
    ) -> Result<PathBuf> {
        let mut files: Vec<File> = self.carried.to_vec();
        let mut names = vec!["bin/sh"];
        if poweroff {
            names.push("bin/poweroff");
        }
        for name in names {
            files.push(File {
                path: name.to_owned(),
                mode: 0o755,
                content: Content::Bytes(shell.to_vec()),
            });
        }
        files.push(File {
            path: path.trim_start_matches('/').to_owned(),
            mode: 0o755,
            content: Content::Bytes(script.as_bytes().to_vec()),
        });
        let archive = initramfs::build(None, self.natives, None, &files)?;
        fat::write_image_with(
            self.arch,
            self.loader,
            self.kernel,
            &archive,
            Some(&format!("{options}\n")),
        )
    }
}

/// The first K7 boot's script: write the files, and power off through
/// busybox without letting it sync.
fn write_script() -> String {
    let mut script = "#!/bin/sh\n".to_owned();
    for file in K7_FILES {
        script.push_str(&format!(
            "echo \"k7: writing {file}\"\necho \"{K7_TEXT}\" > {file}\n"
        ));
    }
    script.push_str("poweroff -f -n\necho \"k7: poweroff returned\"\nexit 1\n");
    script
}

/// The second K7 boot's script: read the files back with the shell's own
/// `read`, which needs no other program.
fn read_script() -> String {
    let mut script = "#!/bin/sh\n".to_owned();
    for file in K7_FILES {
        script.push_str(&format!(
            "if read line < {file}; then echo \"k7: read back {file}: $line\"; \
             else echo \"k7: {file} is not there\"; fi\n"
        ));
    }
    script.push_str("exit 0\n");
    script
}

/// The lines from the boot marker on: only what init did counts.
fn after_marker(lines: &[String]) -> &[String] {
    lines
        .iter()
        .position(|line| line.contains(SUCCESS_MARKER))
        .and_then(|at| lines.get(at..))
        .unwrap_or_default()
}

/// The error for a boot whose transcript failed a judge.
fn failed(arch: Arch, why: &str) -> Error {
    Error::new(format!(
        "{arch}: {why}.\n  Serial output is in {}",
        paths::build_dir(arch).join("serial.log").display()
    ))
}

/// Whether the shell ran the script from [`SCRIPT_FILE`], started by
/// `ferrix.init=` and not by the fallback.
fn judge_from_file(after: &[String]) -> std::result::Result<(), String> {
    if let Some(line) = after.iter().find(|line| line.contains(FELL_BACK)) {
        return Err(format!(
            "the kernel fell back to the built-in shell: `{}`",
            line.trim()
        ));
    }
    let started = format!("init     starting /bin/sh {SCRIPT_FILE}");
    let Some(at) = after.iter().position(|line| line.trim() == started) else {
        return Err(format!(
            "the kernel never said `{started}`, so ferrix.init= was not followed"
        ));
    };
    let mut remaining = after.iter().skip(at);
    for want in shell::EXPECTED {
        if !remaining.any(|line| line.trim_end() == *want) {
            return Err(format!(
                "the script's output is missing `{want}`, or it came out of order"
            ));
        }
    }
    let exited = format!("init     {SCRIPT_FILE} exited with {}", shell::STATUS);
    if !remaining.any(|line| line.trim() == exited) {
        return Err(format!("the kernel never said `{exited}`"));
    }
    Ok(())
}

/// Whether the first K7 boot wrote on `/data` and powered off through
/// `reboot(2)`, and not through init's exit.
fn judge_k7_write(lines: &[String]) -> std::result::Result<(), String> {
    for (mounted, at) in [(DATA_MOUNTED, "/data"), (HOME_MOUNTED, "/home")] {
        if !lines.iter().any(|line| line.contains(mounted)) {
            return Err(format!(
                "the volume was not mounted at {at}, so there was nothing to write"
            ));
        }
    }
    let after = after_marker(lines);
    for file in K7_FILES {
        let wrote = format!("k7: writing {file}");
        if !after.iter().any(|line| line.trim() == wrote) {
            return Err(format!("{K7_WRITE} never wrote {file}"));
        }
    }
    if !after.iter().any(|line| line.trim() == POWER_DOWN) {
        return Err(format!(
            "the kernel never said `{POWER_DOWN}`: poweroff -f -n did not reach reboot(2)"
        ));
    }
    Ok(())
}

/// Whether the second K7 boot read back what the first wrote, and init's
/// exit then panicked.
///
/// Verifies: L.console.14
fn judge_k7_read(after: &[String]) -> std::result::Result<(), String> {
    for file in K7_FILES {
        let wanted = format!("k7: read back {file}: {K7_TEXT}");
        if !after.iter().any(|line| line.trim() == wanted) {
            let asked = format!("k7: {file} ");
            let said = after
                .iter()
                .find(|line| line.trim_start().starts_with(&asked))
                .map_or("nothing", |line| line.trim());
            let volume = file.trim_end_matches("/k7");
            return Err(format!(
                "{file} did not survive poweroff -f -n: reboot(2) powered off without \
                 committing {volume} (the read said {said})"
            ));
        }
    }
    let exited = format!("init     {K7_READ} exited with 0");
    let Some(at) = after.iter().position(|line| line.trim() == exited) else {
        return Err(format!("the kernel never said `{exited}`"));
    };
    if !after.iter().skip(at).any(|line| line.contains(INIT_EXITED)) {
        return Err(format!(
            "init exited under ferrix.onexit=panic and the kernel did not panic with {INIT_EXITED}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    fn from_file(start: &str, exit: &str) -> Vec<String> {
        let mut lines = transcript(&[SUCCESS_MARKER, start]);
        lines.extend(shell::EXPECTED.iter().map(|line| (*line).to_owned()));
        lines.push(exit.to_owned());
        lines
    }

    #[test]
    fn a_shell_started_from_the_file_passes() {
        let lines = from_file(
            "  init     starting /bin/sh /etc/shell-test",
            "  init     /etc/shell-test exited with 7",
        );
        assert_eq!(judge_from_file(&lines), Ok(()));
    }

    #[test]
    fn the_built_in_shell_after_a_fallback_fails() {
        let mut lines = transcript(&[
            SUCCESS_MARKER,
            "  init     ferrix.init=/etc/nope could not be started: errno 2; falling back to \
             the built-in program",
            "  init     1024 KiB program built in, starting `sh -c` with a built-in script",
        ]);
        lines.extend(shell::EXPECTED.iter().map(|line| (*line).to_owned()));
        lines.push("  init     the shell exited with 7".to_owned());
        let why = judge_from_file(&lines).unwrap_err();
        assert!(why.contains("fell back"), "{why}");
    }

    #[test]
    fn the_wrong_status_fails() {
        let lines = from_file(
            "  init     starting /bin/sh /etc/shell-test",
            "  init     /etc/shell-test exited with 0",
        );
        assert!(judge_from_file(&lines).is_err());
    }

    #[test]
    fn the_write_boot_must_power_off_through_reboot() {
        let good = transcript(&[
            "  data     vde mounted writable at /data",
            "  home     vdf mounted writable at /home",
            SUCCESS_MARKER,
            "k7: writing /data/k7",
            "k7: writing /home/k7",
            "reboot: Power down",
        ]);
        assert_eq!(judge_k7_write(&good), Ok(()));
        let exited = transcript(&[
            "  data     vde mounted writable at /data",
            "  home     vdf mounted writable at /home",
            SUCCESS_MARKER,
            "k7: writing /data/k7",
            "k7: writing /home/k7",
            "  init     /etc/k7-write exited with 1",
        ]);
        assert!(judge_k7_write(&exited).unwrap_err().contains(POWER_DOWN));
        let homeless: Vec<String> = good
            .iter()
            .filter(|line| !line.contains("/home"))
            .cloned()
            .collect();
        assert!(judge_k7_write(&homeless).unwrap_err().contains("/home"));
    }

    #[test]
    fn a_lost_file_is_named_as_reboots_fault() {
        let lost = transcript(&[
            SUCCESS_MARKER,
            "k7: read back /data/k7: committed by reboot(2)",
            "k7: /home/k7 is not there",
            "  init     /etc/k7-read exited with 0",
            "  code      FX-1501  init exited, and ferrix.onexit=panic asked for a panic",
        ]);
        let why = judge_k7_read(&lost).unwrap_err();
        assert!(why.contains("/home/k7 did not survive"), "{why}");
        assert!(why.contains("committing /home"), "{why}");
        assert!(why.contains("is not there"), "{why}");
    }

    #[test]
    fn the_read_boot_must_end_in_the_panic_asked_for() {
        let kept = transcript(&[
            SUCCESS_MARKER,
            "k7: read back /data/k7: committed by reboot(2)",
            "k7: read back /home/k7: committed by reboot(2)",
            "  init     /etc/k7-read exited with 0",
        ]);
        assert!(judge_k7_read(&kept).unwrap_err().contains(INIT_EXITED));
        let mut panicked = kept;
        panicked.push("  code      FX-1501  init exited".to_owned());
        assert_eq!(judge_k7_read(&panicked), Ok(()));
    }
}
