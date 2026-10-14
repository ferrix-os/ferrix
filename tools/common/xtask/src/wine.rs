//! `test-wine`: Debian's 64-bit Wine on Ferrix (stage 23, step 1;
//! docs/WINE.md).
//!
//! Wine is fetched, not built: `tools/common/fetch/fetch-wine.sh` unpacks
//! Debian 13's `wine64` and `libwine` over yserver's tree and makes a btrfs
//! volume of it. The gate attaches it at `/data` under QEMU's `snapshot=on`,
//! as `test-yserver` attaches yserver's, and [`RUN`], carried in the image,
//! makes a prefix with `wineboot`, then runs `cmd /c ver`, which must print
//! the Windows version Wine reports, and the same from the 32-bit `cmd` in
//! `syswow64`, which runs in compatibility mode inside the 64-bit process
//! (Wine's new `WoW64`; docs/WINE.md, section 7). A second boot, of hyprix
//! with yserver, must show Wine's notepad in a window
//! (`crate::compositor::wine`).
//!
//! The gate needs no network. It attaches a volume, so it runs on demand as
//! `test-yserver` does and is not in the image row.

use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result, busybox, cargo, fat, initramfs, native, qemu, rustc, shell, zinc};

/// The paths Debian's Wine names absolutely, each a link into the volume,
/// beside glibc's and X's ([`crate::yserver::LINKS`]): its loader and server,
/// and its data files (`wine.inf`, the fonts).
pub(crate) const LINKS: &[(&str, &str)] = &[
    ("usr/lib/wine", "/data/usr/lib/wine"),
    ("usr/share/wine", "/data/usr/share/wine"),
];

/// Build the Windows program `tools/common/test/<name>.c` with mingw-w64's
/// `compiler` (`i686-w64-mingw32-gcc` for a 32-bit program,
/// `x86_64-w64-mingw32-gcc` for a 64-bit one), linked with `libraries`,
/// into the build directory, and answer its bytes. Stripped: the gates
/// carry it in the image.
///
/// # Errors
///
/// No such compiler on the host, or a build that fails.
pub(crate) fn windows_program(compiler: &str, name: &str, libraries: &[&str]) -> Result<Vec<u8>> {
    let source = crate::paths::workspace_root().join(format!("tools/common/test/{name}.c"));
    let out_dir = crate::paths::build_dir(Arch::X86_64).join("windows");
    std::fs::create_dir_all(&out_dir)
        .map_err(|error| Error::new(format!("creating {}: {error}", out_dir.display())))?;
    let suffix = compiler.split('-').next().unwrap_or(compiler);
    let out = out_dir.join(format!("{name}-{suffix}.exe"));
    let built = std::process::Command::new(compiler)
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-s", "-o"])
        .arg(&out)
        .arg(&source)
        .args(libraries)
        .status()
        .map_err(|error| {
            Error::new(format!(
                "running {compiler} (mingw-w64, which builds {name}.exe): {error}"
            ))
        })?;
    if !built.success() {
        return Err(Error::new(format!(
            "building {} with {compiler} failed ({built})",
            source.display()
        )));
    }
    std::fs::read(&out).map_err(|error| Error::new(format!("reading {}: {error}", out.display())))
}

/// Where [`RUN`] is in the image.
const RUN_PATH: &str = "bin/wine-test";

/// What `cmd /c ver` prints under Wine 10.0, which reports Windows 10.
pub(crate) const VERSION: &str = "Microsoft Windows 10.0";

/// A prefix made with `wineboot`, then `cmd /c ver` from the 64-bit `cmd` and
/// from the 32-bit one. Run by busybox's `sh`; its status is 0 only when
/// both printed [`VERSION`], and the line [`NO_CMD32`] says the 32-bit one
/// was the one that did not.
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
echo "wine-gate: cmd32"
/usr/lib/wine/wine64 'C:\windows\syswow64\cmd.exe' /c ver > /tmp/ver32.txt 2> /tmp/ver32.err
echo "wine-gate: cmd32 exited $?"
cat /tmp/ver32.err
sed 's/^/wine-gate: cmd32: /' /tmp/ver32.txt
/usr/lib/wine/wineserver64 -k
grep -q "Microsoft Windows 10.0" /tmp/ver.txt || exit 1
grep -q "Microsoft Windows 10.0" /tmp/ver32.txt || { echo "wine-gate: no 32-bit cmd"; exit 2; }
"#;

/// What [`RUN`] prints when the 64-bit `cmd` answered and the 32-bit one did
/// not.
const NO_CMD32: &str = "wine-gate: no 32-bit cmd";

/// What [`RUN`] puts before each line the 32-bit `cmd` printed.
const CMD32: &str = "wine-gate: cmd32: ";

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
pub(crate) const MEMORY: u32 = 6144;

/// The volume `tools/common/fetch/fetch-wine.sh` makes.
///
/// It carries yserver, and must carry the one `fetch-yserver.sh` pins, as
/// yserver's own volume must ([`crate::yserver::volume`]): one made before
/// the pin moved runs notepad, but without what later commits gave it --
/// before `WM_STATE`, at the size notepad asked for and not at its tile's.
///
/// # Errors
///
/// The volume has not been made, or was made with another yserver.
pub(crate) fn volume() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("FERRIX_WINE_VOLUME") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => crate::paths::volume_directory("wine")?,
    };
    let image = directory.join("wine.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-wine.sh makes it",
            image.display()
        )));
    }
    let stamp = directory.join("yserver.commit");
    let carried = std::fs::read_to_string(&stamp).unwrap_or_default();
    match carried.trim() {
        commit if commit == crate::yserver::pinned() => Ok(image),
        "" => Err(Error::new(format!(
            "{} does not say which yserver it carries ({} is not there): \
             tools/common/fetch/fetch-wine.sh makes it again",
            image.display(),
            stamp.display()
        ))),
        commit => Err(Error::new(format!(
            "{} carries yserver {commit}: tools/common/fetch/fetch-wine.sh makes it again with \
             {}, the commit tools/common/fetch/fetch-yserver.sh pins",
            image.display(),
            crate::yserver::pinned()
        ))),
    }
}

/// Boot a shell whose script makes a Wine prefix and runs `cmd /c ver` in
/// it, then the desktop with notepad.
///
/// # Errors
///
/// When the volume is missing, an image cannot be built, a boot fails,
/// `cmd` did not print the version, or notepad's window did not come.
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
            let version32 = lines
                .iter()
                .filter_map(|line| line.trim().strip_prefix(CMD32))
                .find(|line| line.contains(VERSION))
                .unwrap_or(VERSION);
            println!(
                "  {arch}: Wine made a prefix on Ferrix, its cmd answered `{version}` and its \
                 32-bit cmd `{version32}`"
            );
            crate::compositor::wine::test_wine_window(&args, volume()?)
        }
        Some("3") => Err(Error::new(format!(
            "{arch}: /data/usr/lib/wine/wine64 is not there: is the volume attached?"
        ))),
        Some("4") if lines.iter().any(|line| line.contains(NO_CMD32)) => Err(Error::new(format!(
            "{arch}: Wine's 64-bit cmd answered and its 32-bit cmd in syswow64 did not \
                 print `{VERSION}`; the lines after `wine-gate: cmd32` say how far it came"
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
