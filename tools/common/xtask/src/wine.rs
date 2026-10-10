//! `test-wine`: Debian's 64-bit Wine on Ferrix (stage 23, step 1;
//! docs/WINE.md).
//!
//! Wine is fetched, not built: `tools/common/fetch/fetch-wine.sh` unpacks
//! Debian 13's `wine64` and `libwine` over yserver's tree and makes a btrfs
//! volume of it. The gate attaches it at `/data` under QEMU's `snapshot=on`,
//! as `test-yserver` attaches yserver's, and [`RUN`], carried in the image,
//! makes a prefix on it with `wineboot`, then runs `cmd /c ver`. It passes
//! when `cmd` printed the Windows version Wine reports.
//!
//! The gate needs no network. It attaches a volume, so it runs on demand as
//! `test-yserver` does and is not in the image row.

use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result, busybox, cargo, fat, initramfs, native, qemu, rustc, shell, zinc};

/// The paths Debian's Wine names absolutely, each a link into the volume,
/// beside glibc's and X's ([`crate::yserver::LINKS`]): its loader and server,
/// and its data files (`wine.inf`, the fonts).
const LINKS: &[(&str, &str)] = &[
    ("usr/lib/wine", "/data/usr/lib/wine"),
    ("usr/share/wine", "/data/usr/share/wine"),
];

/// Where [`RUN`] is in the image.
const RUN_PATH: &str = "bin/wine-test";

/// What `cmd /c ver` prints under Wine 10.0, which reports Windows 10.
const VERSION: &str = "Microsoft Windows 10.0";

/// A prefix made with `wineboot`, then `cmd /c ver`. Run by busybox's `sh`;
/// its status is 0 only when `cmd` printed [`VERSION`].
///
/// The server is named directly: Debian's `/usr/lib/wine/wineserver` is a
/// shell script that picks the 64-bit or the 32-bit one, and there is only
/// the first. `wineserver -w` waits for the prefix's programs to end, so the
/// registry is written before `cmd` starts a server again.
const RUN: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp/wine XDG_RUNTIME_DIR=/tmp
export WINEPREFIX=/tmp/wine/prefix WINESERVER=/usr/lib/wine/wineserver64
export WINEDEBUG="${WINEDEBUG:-err+all,fixme-all}"
mkdir -p /tmp/wine
echo "wine-gate: wineboot"
/usr/lib/wine/wine64 wineboot -i 2>&1
echo "wine-gate: wineboot exited $?"
/usr/lib/wine/wineserver64 -w
echo "wine-gate: the server ended $?"
ls /tmp/wine/prefix
df -h /tmp /data 2>&1
echo "wine-gate: cmd"
/usr/lib/wine/wine64 cmd /c ver > /tmp/ver.txt 2> /tmp/ver.err
echo "wine-gate: cmd exited $?"
cat /tmp/ver.err
cat /tmp/ver.txt
/usr/lib/wine/wineserver64 -k
grep -q "Microsoft Windows 10.0" /tmp/ver.txt
"#;

/// The script: [`RUN`], whose status says whether `cmd` answered.
const SCRIPT: &str = r#"export PATH=/bin HOME=/tmp
[ -x /data/usr/lib/wine/wine64 ] || exit 3
busybox sh /bin/wine-test || exit 4
exit 17
"#;

/// What the script exits with when `cmd` printed the version.
const STATUS: i32 = 17;

/// Megabytes of guest RAM: Wine's own programs are small, but a prefix is
/// about 800 MB of files written through the page cache.
const MEMORY: u32 = 6144;

/// The volume `tools/common/fetch/fetch-wine.sh` makes.
///
/// # Errors
///
/// The volume has not been made.
fn volume() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("FERRIX_WINE_VOLUME") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => crate::paths::volume_directory("wine")?,
    };
    let image = directory.join("wine.img");
    if image.is_file() {
        Ok(image)
    } else {
        Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-wine.sh makes it",
            image.display()
        )))
    }
}

/// Boot a shell whose script makes a Wine prefix on the volume and runs
/// `cmd /c ver` in it.
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, the boot fails, or
/// `cmd` did not print the version.
pub(crate) fn test_wine(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => return Err(Error::new("test-wine runs on x86-64")),
    };
    let mut args = args.clone();
    args.data_image = Some(volume()?);
    if !args.memory_given {
        args.memory = MEMORY;
    }

    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose shell starts Wine from the volume");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let busybox = busybox::program(arch)?;
    let links: Vec<(&str, &str)> = crate::yserver::LINKS.iter().chain(LINKS).copied().collect();
    let mut files = rustc::files(&links);
    files.push(crate::ports::File {
        path: RUN_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(RUN.as_bytes().to_vec()),
    });
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &files)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;

    println!(
        "  {arch}: running Wine on Ferrix with {} MiB (timeout {}s)",
        args.memory, args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    let exited = lines
        .iter()
        .find_map(|line| line.trim().strip_prefix(shell::EXITED))
        .map(str::trim);
    match exited {
        Some(status) if status == STATUS.to_string() => {
            let version = lines
                .iter()
                .find(|line| line.contains(VERSION))
                .map_or(VERSION, |line| line.trim());
            println!("  {arch}: Wine made a prefix on Ferrix and cmd answered `{version}`");
            Ok(())
        }
        Some("3") => Err(Error::new(format!(
            "{arch}: /data/usr/lib/wine/wine64 is not there: is the volume attached?"
        ))),
        Some("4") => Err(Error::new(format!(
            "{arch}: Wine's cmd did not print `{VERSION}`; the lines after `wine-gate:` say \
             how far it came"
        ))),
        other => Err(Error::new(format!(
            "{arch}: the Wine script ended with {other:?}"
        ))),
    }
}
