//! `test-steam-bootstrap`: the Steam client's bootstrapper on Ferrix, as far
//! as it goes before it needs a display (`docs/I386.md`, I5b).
//!
//! Valve's `steam.sh` is what `/usr/bin/steam` runs: a bash script that sets
//! up the Steam Runtime ("scout") beside it and starts the 32-bit
//! `ubuntu12_32/steam`. That program downloads the client proper from Valve,
//! installs it and exits 42, and `steam.sh` starts again: unpacks the new
//! runtime with GNU `tar`, lets scout's 64-bit helpers check the machine,
//! starts the client again, which verifies what it installed, loads
//! `steamui.so`, and asks for an X display. Headless, there is none, and the
//! client says "Unable to open X11 display" and exits: that line is what
//! this gate waits for, and it is where the X server (`docs/YSERVER.md`)
//! takes over.
//!
//! # The volume
//!
//! `tools/common/fetch/fetch-steam.sh` makes it: the bootstrap under `steam/`, an
//! empty home, and from Debian 13 the i386 and amd64 C libraries, the four
//! GL libraries `steamui.so` needs that scout does not carry, and the shell
//! tools `steam.sh` runs (`bash`, GNU `tar` and `xz`, `grep`, `sed`, `mawk`,
//! `find`, `which`, `ldd`). It is attached under QEMU's `snapshot=on`, as
//! steamcmd's is, so every run starts from the bootstrap and downloads the
//! client again.
//!
//! # `uname`
//!
//! `steam.sh` stops with "Unsupported Operating System" unless `uname` says
//! `Linux`, and Ferrix's says `Ferrix` (the customer's choice, 2026-09-13).
//! The image puts [`UNAME`] first in `PATH`, which says `Linux` to the two
//! questions the scripts ask by name and passes every other one to busybox.
//!
//! # What stops it today, and the two things the gate sets aside
//!
//! * Scout's `steam-runtime-check-requirements` refuses a kernel without
//!   user namespaces ("Steam now requires user namespaces to be enabled",
//!   exit 71), which `steam.sh` treats as fatal, and Ferrix has none. The
//!   script runs the check once and prints its verdict, then makes it not
//!   executable, which `steam.sh` answers with "continuing anyway" -- and
//!   again whenever it is executable once more, since the client's update
//!   unpacks a new runtime over the old one and `steam.sh` checks again.
//! * System V semaphores are `ENOSYS`, and the client then prints "Thread
//!   synchronization object is unuseable" and waits forever, before it has
//!   downloaded anything. The script stops there, and the gate fails saying
//!   so. Whether Ferrix gets them is the customer's decision
//!   (`docs/BACKLOG.md`, *Waiting on the customer*). To see past it,
//!   `FERRIX_STEAM_PRELOAD` may name an i386 shared object the image carries
//!   at [`PRELOAD_PATH`] and preloads into every program, and the verdict
//!   then names it: a diagnostic stand-in, not a pass.
//!
//! # Under KVM, and on the internet
//!
//! As `test-steamcmd`: the client reads `cpu MHz`, which Ferrix prints only
//! when its clock is the TSC, and it downloads from Valve, so the gate is run
//! on demand and not in the image row.

use crate::args::Args;
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::{Error, Result, busybox, cargo, fat, initramfs, native, qemu, rustc, shell, zinc};

/// The paths the volume's programs name absolutely, each a link into it:
/// both loaders and both of glibc's directories, the certificate bundle,
/// `bash` where `ldd`'s `#!` names it, and `env` where `steam.sh`'s does.
pub(crate) const LINKS: &[(&str, &str)] = &[
    (
        "lib/ld-linux.so.2",
        "/data/usr/lib/i386-linux-gnu/ld-linux.so.2",
    ),
    ("lib/i386-linux-gnu", "/data/usr/lib/i386-linux-gnu"),
    ("usr/lib/i386-linux-gnu", "/data/usr/lib/i386-linux-gnu"),
    ("lib64", "/data/usr/lib64"),
    ("lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
    ("usr/lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
    (
        "etc/ssl/certs/ca-certificates.crt",
        "/data/etc/ssl/certs/ca-certificates.crt",
    ),
    ("bin/bash", "/data/usr/bin/bash"),
    ("usr/bin/env", "/bin/env"),
    ("sbin/ldconfig", "/data/usr/sbin/ldconfig"),
];

/// Where [`UNAME`] is in the image: a directory of its own, first in `PATH`.
pub(crate) const UNAME_PATH: &str = "usr/local/bin/uname";

/// `uname` for `steam.sh`: `Linux` for the bare `uname` and `uname -s`, the
/// two ways it and scout ask for the system's name; busybox's answer to
/// anything else, `uname -m` among them.
pub(crate) const UNAME: &str = r#"#!/bin/sh
case "$*" in
    '' | -s) echo Linux ;;
    *) exec /bin/busybox uname "$@" ;;
esac
"#;

/// Where the object `FERRIX_STEAM_PRELOAD` names is carried.
const PRELOAD_PATH: &str = "usr/local/lib/steam-preload.so";

/// The script: a lease for `eth0`; scout's requirements check on its own,
/// whose verdict is printed and which is set aside when it refuses (see the
/// module's header); then `steam.sh` as `/usr/bin/steam` runs it, watched:
/// its output and the client's log, `logs/console-linux.txt`, are read every
/// few seconds for the line that asks for a display and the one that says
/// the semaphores failed. Then the client's logs.
const SCRIPT: &str = r#"export PATH=/usr/local/bin:/data/usr/bin:/bin HOME=/data/home USER=root
dhcpc -i eth0 -n -q -t 5 -T 2 || exit 5
[ -x /data/steam/steam.sh ] || exit 3
cd /data/steam || exit 3
preload=/usr/local/lib/steam-preload.so
if [ -f $preload ]; then
    export LD_PRELOAD=$preload
    echo "steam-gate: preloading $preload, a diagnostic stand-in"
fi
check=/data/steam/ubuntu12_32/steam-runtime/amd64/usr/bin/steam-runtime-check-requirements
$check > /tmp/requirements.txt 2>&1
status=$?
sed 's/^/steam-gate: requirements: /' /tmp/requirements.txt | grep -v 'wrong ELF class'
echo "steam-gate: the requirements check exited $status"
aside=
if [ $status -ne 0 ]; then
    (while :; do
        if [ -x $check ]; then
            chmod -x $check && echo "steam-gate: the requirements check is set aside"
        fi
        sleep 1
    done) &
    aside=$!
fi
(bash ./steam.sh < /dev/null; echo "steam-gate: steam.sh exited $?") 2>&1 | tee /tmp/steam.txt &
verdict=
stuck=0
while [ -z "$verdict" ]; do
    sleep 5
    seen=$(cat /tmp/steam.txt logs/console-linux.txt 2>/dev/null)
    case "$seen" in
        *'Unable to open X11 display'*) verdict=display ;;
        *'steam-gate: steam.sh exited'*) verdict=ended ;;
        *'Thread synchronization object is unuseable'*)
            stuck=$((stuck + 5))
            [ $stuck -ge 60 ] && verdict=semaphores ;;
    esac
done
echo "steam-gate: stopped watching: $verdict"
[ -n "$aside" ] && kill $aside
for log in bootstrap_log.txt console-linux.txt; do
    [ -f logs/$log ] && sed "s/^/steam-log: $log: /" logs/$log | grep -v '%   ' | tail -n 80
done
case $verdict in
    display) echo steam-gate: the client asked for a display; exit 17 ;;
    semaphores) exit 7 ;;
    *) exit 6 ;;
esac
"#;

/// What the script exits with when the client got as far as the display.
const STATUS: i32 = 17;

/// The script's own line when it did.
const ASKED: &str = "steam-gate: the client asked for a display";

/// Memory for the guest: the client downloads and unpacks half a gigabyte.
const MEMORY: u32 = 4096;

/// Seconds for the boot: the download, the unpacking of 2 GB onto btrfs,
/// and the runtime's unpacking after it.
const TIMEOUT: u64 = 3600;

/// Where `tools/common/fetch/fetch-steam.sh` writes, unless
/// `FERRIX_STEAM_VOLUME` names another directory.
fn volume() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("FERRIX_STEAM_VOLUME") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => crate::paths::volume_directory("steam-bootstrap")?,
    };
    let image = directory.join("steam.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-steam.sh makes it",
            image.display()
        )));
    }
    Ok(image)
}

/// What the image carries beside busybox and zinc: [`LINKS`], [`UNAME`],
/// and `preload` at [`PRELOAD_PATH`] when given.
fn files(preload: Option<Vec<u8>>) -> Vec<File> {
    let mut files = rustc::files(LINKS);
    files.push(File {
        path: UNAME_PATH.to_owned(),
        mode: 0o755,
        content: Content::Bytes(UNAME.as_bytes().to_vec()),
    });
    if let Some(bytes) = preload {
        files.push(File {
            path: PRELOAD_PATH.to_owned(),
            mode: 0o755,
            content: Content::Bytes(bytes),
        });
    }
    files
}

/// The object `FERRIX_STEAM_PRELOAD` names, read, and its path for the
/// verdict; `None` when the variable is not set.
fn preload() -> Result<Option<(String, Vec<u8>)>> {
    let Some(path) = std::env::var_os("FERRIX_STEAM_PRELOAD") else {
        return Ok(None);
    };
    let path = std::path::PathBuf::from(path);
    let bytes = std::fs::read(&path)
        .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
    Ok(Some((path.display().to_string(), bytes)))
}

/// Boot a shell whose script runs `steam.sh` until the client asks for a
/// display.
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, the boot fails, or
/// the client stopped before it asked for a display.
pub(crate) fn test_steam_bootstrap(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => {
            return Err(Error::new(
                "test-steam-bootstrap runs on x86-64: the Steam client is a 32-bit x86 program",
            ));
        }
    };
    let mut args = args.clone();
    args.data_image = Some(volume()?);
    args.net = true;
    // Under KVM unless told otherwise, for `cpu MHz` (the module's header).
    if args.accel.is_none() {
        args.accel = Some("kvm".to_owned());
    }
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }

    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose shell runs Valve's steam.sh");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let busybox = busybox::program(arch)?;
    let preload = preload()?;
    let stand_in = preload.as_ref().map(|(path, _)| path.clone());
    let mut carried = files(preload.map(|(_, bytes)| bytes));
    // The DHCP client the script takes its lease with (`crate::dhcp`).
    carried.extend(crate::dhcp::carried(arch)?);
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &carried)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;

    println!(
        "  {arch}: running the Steam bootstrapper on Ferrix with {} MiB (timeout {}s), on the network",
        args.memory, args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    judge(arch, &lines, stand_in.as_deref())
}

/// Whether the script's lines say the client got as far as the display.
/// With a `stand_in` preloaded, getting there is reported and still fails:
/// the stand-in is not Ferrix.
fn judge(arch: Arch, lines: &[String], stand_in: Option<&str>) -> Result<()> {
    let after_boot = lines
        .iter()
        .position(|line| line.contains(qemu::SUCCESS_MARKER))
        .and_then(|at| lines.get(at..))
        .unwrap_or_default();
    let exited = after_boot
        .iter()
        .find_map(|line| line.trim().strip_prefix(shell::EXITED))
        .map(str::trim);
    let asked = after_boot
        .iter()
        .any(|line| line.trim_end().ends_with(ASKED));
    match (exited, stand_in) {
        (Some(status), None) if status == STATUS.to_string() && asked => {
            println!(
                "  {arch}: the Steam client updated itself, loaded steamui.so and asked for an X \
                 display, which is where the X server takes over"
            );
            Ok(())
        }
        (Some(status), Some(stand_in)) if status == STATUS.to_string() && asked => {
            Err(Error::new(format!(
                "{arch}: the Steam client asked for an X display, but only with {stand_in} \
                 preloaded, which is a stand-in and not Ferrix"
            )))
        }
        (Some("3"), _) => Err(Error::new(format!(
            "{arch}: /data/steam/steam.sh is not there: is the volume attached?"
        ))),
        (Some("5"), _) => Err(Error::new(format!(
            "{arch}: dhcpc got no lease for eth0 from the gateway"
        ))),
        (Some("7"), _) => Err(Error::new(format!(
            "{arch}: the Steam client stopped at its System V semaphores, which Ferrix answers \
             ENOSYS: \"Thread synchronization object is unuseable\", and then it waits forever"
        ))),
        (Some("6"), _) => Err(Error::new(format!(
            "{arch}: steam.sh ended before the client asked for a display; the `steam-log:` \
             lines say where"
        ))),
        (other, _) => Err(Error::new(format!(
            "{arch}: the steam script ended with {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `uname` is carried where `PATH` finds it first, and every link has a
    /// place to point.
    #[test]
    fn the_image_carries_uname_first_and_the_links() {
        let files = files(Some(vec![0x7f]));
        let uname = files
            .iter()
            .find(|file| file.path == UNAME_PATH)
            .expect("the uname shim");
        assert_eq!(uname.mode, 0o755);
        let first = SCRIPT
            .lines()
            .next()
            .and_then(|line| line.split("PATH=").nth(1))
            .and_then(|path| path.split(':').next())
            .expect("PATH");
        assert_eq!(format!("/{UNAME_PATH}"), format!("{first}/uname"));
        assert_eq!(files.len(), LINKS.len() + 2);
        assert!(files.iter().any(|file| file.path == PRELOAD_PATH));
        assert!(SCRIPT.contains(&format!("preload=/{PRELOAD_PATH}")));
        assert_eq!(super::files(None).len(), LINKS.len() + 1);
    }
}
