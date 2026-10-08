//! The Linux a Windows host already has: WSL.
//!
//! Two things this tool needs are Linux and nothing else. ferrousli's tests
//! compile C programs against the library and run them, and what they build
//! is a Linux executable that only a Linux kernel can start. And ARMv7-A boots
//! through U-Boot, which QEMU's Windows build does not ship and distributions
//! package for Linux. Everything else — the kernel, the loaders, QEMU, the
//! gateway — builds and runs natively.
//!
//! The downloaded btrfs volumes are Linux's too: the `tools/common/fetch/`
//! scripts that make them unpack Debian packages into a tree of symbolic
//! links and run `mkfs.btrfs` over it, and `run-compositor --everything`
//! merges two such trees into a third volume. So on Windows those scripts
//! run in WSL, the volumes are found in its home (`crate::paths::
//! volume_directory`), and the merge is xtask's own, built and run there.
//!
//! So on Windows those two go through WSL's default distribution: the one
//! `wsl.exe` starts with no `-d`, which is also the one a developer set up.
//! Only that one is looked at. Opening a path into a distribution starts it,
//! and a machine with Docker Desktop or Podman has distributions of theirs
//! that nobody wants booted to look for a firmware file.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{Error, Result};

/// Where Windows keeps the WSL distributions, for the user running this.
const LXSS: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Lxss";

/// The default distribution's name, if WSL has one.
///
/// Read from the registry with `reg`, which every Windows has, rather than
/// from `wsl.exe --list`, whose output is UTF-16 on some versions and UTF-8
/// on others and translated into the display language on all of them.
pub(crate) fn default_distribution() -> Option<String> {
    let default = reg_value(LXSS, "DefaultDistribution")?;
    reg_value(&format!(r"{LXSS}\{default}"), "DistributionName")
}

/// One string value from `reg query`, whose line for it reads
/// `    NAME    REG_SZ    VALUE` in every display language.
fn reg_value(key: &str, name: &str) -> Option<String> {
    let output = Command::new("reg")
        .args(["query", key, "/v", name])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next() == Some(name) && fields.next() == Some("REG_SZ"))
                .then(|| fields.collect::<Vec<_>>().join(" "))
        })
        .filter(|value| !value.is_empty())
}

/// A path inside the default distribution, as Windows opens it:
/// `\\wsl.localhost\<name>\<path>`. `None` without WSL.
pub(crate) fn path(linux: &str) -> Option<PathBuf> {
    let name = default_distribution()?;
    let relative = linux.trim_start_matches('/').replace('/', r"\");
    Some(PathBuf::from(format!(r"\\wsl.localhost\{name}\{relative}")))
}

/// The target of a symbolic link WSL made, read in WSL. Windows cannot read
/// one: inside the default distribution, at a path [`path`] made, it is a
/// link whose `read_link` fails with "Incorrect function"; on a Windows
/// drive, where a script run in WSL wrote it, Windows cannot even open it
/// ("Unsupported reparse point type"). `None` when it is no such link, or
/// without WSL.
pub(crate) fn read_link(windows: &Path) -> Option<String> {
    let name = default_distribution()?;
    let text = windows.to_str()?;
    let text = text
        .strip_prefix(r"\\?\")
        .unwrap_or(text)
        .replace('/', r"\");
    let (script, linux) = match text.strip_prefix(&format!(r"\\wsl.localhost\{name}\")) {
        Some(rest) => (
            "readlink -- \"$1\"",
            format!("/{}", rest.replace('\\', "/")),
        ),
        None => ("readlink -- \"$(wslpath -u \"$1\")\"", text.clone()),
    };
    let output = Command::new("wsl.exe")
        .args(["--exec", "sh", "-c", script, "sh"])
        .arg(linux)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let target = String::from_utf8(output.stdout).ok()?;
    (output.status.success() && !target.is_empty())
        .then(|| target.trim_end_matches('\n').to_owned())
}

/// The default distribution's `$HOME`, as Windows opens it, asked once per
/// run. `None` without WSL.
pub(crate) fn home() -> Option<PathBuf> {
    static HOME: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        // Opening no distribution but the default one: see above.
        let _ = default_distribution()?;
        let output = Command::new("wsl.exe")
            .args(["--exec", "sh", "-c", "printf %s \"$HOME\""])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        let home = String::from_utf8(output.stdout).ok()?;
        (output.status.success() && home.starts_with('/')).then(|| path(&home))?
    })
    .clone()
}

/// `cargo` with `arguments`, run in the default distribution in `dir`.
///
/// The target directory is on the distribution's own filesystem, under
/// `~/.cache/ferrix/`, rather than the checkout's `target/`: a Windows build
/// already lives there, cargo's fingerprints for one host mean nothing to the
/// other, and a build over WSL's view of an NTFS drive is several times slower
/// than one on ext4. Each checkout gets its own, named after its path, so two
/// worktrees never wait on each other's lock or rebuild each other's crates.
///
/// Through a login shell, so that `~/.cargo/env` has put `cargo` on `PATH`,
/// and started with `--exec`: after a plain `--`, `wsl.exe` joins the words
/// back into one line for the user's own shell to split again, and a script
/// and its `"$@"` do not survive that.
pub(crate) fn cargo(dir: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new("wsl.exe");
    let _ = command
        .arg("--cd")
        .arg(dir)
        .args(["--exec", "bash", "-lc"])
        .arg(format!(
            "export CARGO_TARGET_DIR=\"$HOME/.cache/ferrix/target/{}\"; exec cargo \"$@\"",
            target_name(dir)
        ))
        // `$0` for the script, and then the arguments as `$@`, so that none of
        // them is ever parsed by the shell.
        .arg("cargo")
        .args(arguments);
    command
}

/// `script` run by bash in the default distribution in `dir`, with
/// `CARGO_TARGET_DIR` set as [`cargo`] sets it and `arguments` as `"$@"`.
pub(crate) fn bash(dir: &Path, script: &str, arguments: &[&str]) -> Command {
    bash_building(dir, dir, script, arguments)
}

/// As [`bash`], with the target directory [`cargo`] gives `builds` rather
/// than `dir`'s: every script app builds ferrousli, and with one target
/// directory per app each of them compiled it from nothing.
pub(crate) fn bash_building(
    dir: &Path,
    builds: &Path,
    script: &str,
    arguments: &[&str],
) -> Command {
    let mut command = Command::new("wsl.exe");
    let _ = command
        .arg("--cd")
        .arg(dir)
        .args(["--exec", "bash", "-lc"])
        .arg(format!(
            "export CARGO_TARGET_DIR=\"$HOME/.cache/ferrix/target/{}\"; {script}",
            target_name(builds)
        ))
        .arg("bash")
        .args(arguments);
    command
}

/// A directory name for `dir`'s build: its path with everything but letters,
/// digits and hyphens made a hyphen, so it is one path component on Linux and
/// the same on every run.
fn target_name(dir: &Path) -> String {
    dir.to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_owned()
}

/// Refuse early, and say what to install, if the default distribution cannot
/// build what `need` describes: no WSL, or no `cargo` or `cc` in it.
///
/// `need` is the half-sentence before "which on Windows needs WSL", because
/// three gates want this and they want it for different reasons: ferrousli
/// builds C programs a Linux kernel has to start, zinc drives a
/// pseudoterminal, and the compositor opens render nodes and Unix sockets
/// that Windows has no equivalent of. A person without a distribution should
/// be told which of the three they are being stopped by.
pub(crate) fn require_toolchain(need: &str) -> Result<()> {
    let Some(name) = default_distribution() else {
        return Err(Error::new(format!(
            "{need}, which on Windows needs WSL, and WSL has no default distribution here.\n  \
             Install one: `wsl --install -d Ubuntu`, then inside it rustup and `build-essential`."
        )));
    };
    let status = Command::new("wsl.exe")
        .args(["--exec", "bash", "-lc", "command -v cargo && command -v cc"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| Error::new(format!("could not run wsl.exe: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "WSL's default distribution, {name}, has no `cargo` or no `cc`.\n  \
             Inside it: install rustup (https://rustup.rs) and `sudo apt install build-essential`."
        )))
    }
}

/// Open a volume inside WSL and read its first block before QEMU is started
/// on it, waiting out a file server that does not answer yet; nothing for a
/// path outside WSL.
///
/// Windows reaches `\\wsl.localhost` through WSL's own file server, which
/// is slow to answer while its machine is starting or busy, and an open that
/// waits too long fails with `ERROR_SEM_TIMEOUT`. QEMU opens each drive once
/// and gives up: on the Windows PC on 2026-10-08 two `run-compositor` boots
/// of nine died as "Could not open '//wsl.localhost/.../rustc.img': The
/// semaphore timeout period has expired." Once one open has been answered,
/// the next comes at once.
///
/// # Errors
///
/// When the volume still cannot be read after the last try.
pub(crate) fn wake(volume: &Path) -> Result<()> {
    use std::io::Read as _;

    /// `ERROR_SEM_TIMEOUT`.
    const SEMAPHORE_TIMEOUT: i32 = 121;
    const TRIES: u32 = 5;

    let text = volume.to_string_lossy().replace('/', r"\");
    if !text
        .trim_start_matches(r"\\?\")
        .to_ascii_lowercase()
        .starts_with(r"\\wsl.localhost\")
    {
        return Ok(());
    }
    let mut block = [0u8; 4096];
    for attempt in 1..=TRIES {
        let read = std::fs::File::open(volume).and_then(|mut file| file.read(&mut block));
        match read {
            Ok(_) => return Ok(()),
            Err(error) if error.raw_os_error() == Some(SEMAPHORE_TIMEOUT) && attempt < TRIES => {
                println!(
                    "  wsl: {} did not answer in time ({attempt} of {TRIES}); asking again",
                    volume.display()
                );
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            Err(error) => {
                return Err(Error::new(format!("{}: {error}", volume.display())));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::target_name;

    #[test]
    fn a_target_name_is_one_stable_path_component() {
        assert_eq!(
            target_name(Path::new(
                r"F:\Dokumente\projekte\os\.claude\worktrees\a b\ferrousli"
            )),
            "F--Dokumente-projekte-os--claude-worktrees-a-b-ferrousli"
        );
    }
}
