//! `test-wine-socket`: a Windows program's sockets in GE-Proton's Wine make
//! the calls .NET's `TcpClient` makes, and every one answers as on Linux.
//!
//! Arknights: Endfield (Unity IL2CPP) logged in, picked its server and then
//! failed every connect to its game server with `SocketException: Invalid
//! arguments` (`WSAEINVAL`). Wine reads a Windows program's `getsockopt` of
//! `SO_LINGER`, `SO_OOBINLINE`, TCP's keep-alive timing and Don't Fragment
//! from the Linux socket, and maps `ENOPROTOOPT` to `WSAEINVAL`; Ferrix's
//! sockets refused all of them. The kernel now keeps them as Linux does
//! (`ferrix_linux_abi::inet::InetTunables`).
//!
//! The gate attaches Proton's volume (`fetch-proton.sh`) with no display and
//! carries `tools/common/test/dual-socket.c`, built here with mingw-w64. It
//! connects an `AF_INET` and a dual-mode `AF_INET6` socket to its own
//! listener on the loopback, three ways each (non-blocking, `ConnectEx`,
//! blocking), reads back every option .NET's properties read before and
//! after, and prints `DUAL-SOCKET PASS` when all of it answered. Before the
//! fix it failed at `SO_LINGER` with 10022.

use crate::args::Args;
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, busybox, cargo, fat, initramfs, native, qemu, rustc, shell, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-wine-socket",
    help_at: 776,
    help: &[
        "test-wine-socket  Attach the volume tools/common/fetch/fetch-proton.sh makes, and require a Windows program's",
        "              IPv4 and dual-mode IPv6 sockets in GE-Proton's Wine to connect every way .NET does and read back",
        "              every option .NET's properties read (tools/common/test/dual-socket.c)",
    ],
    run: |_, args| test_wine_socket(args),
}];

/// Where [`SCRIPT`] is in the image.
const RUN_PATH: &str = "bin/wine-socket";

/// Where the probe is in the image.
const PROBE: &str = "etc/dual-socket.exe";

/// What the probe prints when everything answered.
const PASS: &str = "DUAL-SOCKET PASS";

/// What the script prints last, whatever happened.
const END: &str = "socket: end";

/// The script, after Proton's own lines (`Wine::prelude`): a prefix, then
/// the probe, its output each line prefixed.
const SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp/wine XDG_RUNTIME_DIR=/tmp
export WINEPREFIX=/tmp/wine/prefix WINEDEBUG=-all
unset DISPLAY WAYLAND_DISPLAY
mkdir -p /tmp/wine
$WINE wineboot -i > /tmp/wine/wineboot.log 2>&1
echo "socket: wineboot exited $?"
$WINE /etc/dual-socket.exe > /tmp/wine/probe.txt 2> /tmp/wine/probe.err
echo "socket: probe exited $?"
sed 's/^/socket: probe: /' /tmp/wine/probe.txt
$WINESERVER -k
echo "socket: end"
"#;

/// The script the shell runs: [`SCRIPT`] behind Proton's lines.
const INIT: &str = r#"export PATH=/bin HOME=/tmp
[ -x /data/proton/files/bin/wine ] || exit 3
busybox sh /bin/wine-socket
exit 17
"#;

/// What [`INIT`] exits with once the script has run.
const STATUS: i32 = 17;

/// Megabytes of guest RAM: a prefix written through the page cache.
const MEMORY: u32 = 6144;

/// Seconds the boot is given unless `--timeout` says otherwise.
const TIMEOUT_SECONDS: u64 = 600;

/// The verdict on the script's lines: the probe's pass line, or what it said
/// instead.
fn judge(arch: Arch, lines: &[String]) -> Result<String> {
    if let Some(passed) = lines.iter().find_map(|line| {
        line.split_once(PASS)
            .map(|(_, rest)| format!("{PASS}{}", rest.trim_end()))
    }) {
        return Ok(passed);
    }
    let said = lines
        .iter()
        .find(|line| line.contains("DUAL-SOCKET FAIL"))
        .or_else(|| lines.iter().rfind(|line| line.contains("socket: probe: ")))
        .and_then(|line| line.split_once("socket: probe: "))
        .map_or("nothing", |(_, said)| said.trim());
    Err(Error::new(format!(
        "{arch}: a Windows program's sockets did not answer as on Linux: the probe said {said}"
    )))
}

/// The gate.
pub(crate) fn test_wine_socket(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        _ => return Err(Error::new("test-wine-socket runs on x86-64")),
    };
    let mut args = args.clone();
    args.data_image = Some(crate::wine_sync::volume()?);
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT_SECONDS;
    }
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose shell runs a socket probe in Proton's Wine");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, INIT)?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let busybox = busybox::program(arch)?;
    let links: Vec<(&str, &str)> = crate::yserver::LINKS
        .iter()
        .chain(crate::wine::LINKS)
        .chain(crate::compositor::endfield::PROTON_LINKS)
        .copied()
        .collect();
    let mut files = rustc::files(&links);
    let script = format!(
        "{}{SCRIPT}",
        crate::compositor::endfield::Wine::Proton.prelude()
    );
    files.push(crate::ports::File {
        path: RUN_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(script.into_bytes()),
    });
    files.push(crate::ports::File {
        path: PROBE.to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(crate::wine::windows_program(
            "x86_64-w64-mingw32-gcc",
            "dual-socket",
            &["-lws2_32"],
        )?),
    });
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &files)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    println!(
        "  {arch}: running the probe in Proton's Wine on Ferrix with {} MiB (timeout {}s)",
        args.memory, args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    let exited = lines
        .iter()
        .find_map(|line| line.trim().strip_prefix(shell::EXITED))
        .map(str::trim);
    match exited {
        Some(status) if status == STATUS.to_string() => {}
        Some("3") => {
            return Err(Error::new(format!(
                "{arch}: /data/proton/files/bin/wine is not there: is Proton's volume attached?"
            )));
        }
        other => {
            return Err(Error::new(format!(
                "{arch}: the socket script ended with {other:?}"
            )));
        }
    }
    if !lines.iter().any(|line| line.contains(END)) {
        return Err(Error::new(format!("{arch}: the socket script never ended")));
    }
    let passed = judge(arch, &lines)?;
    println!(
        "  {arch}: a Windows program's sockets connected every way .NET does and read back every \
         option: {passed}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_passes_and_a_failure_names_its_step() {
        let passed = vec![" 39.61 | socket: probe: DUAL-SOCKET PASS".to_owned()];
        assert_eq!(
            judge(Arch::X86_64, &passed).ok().as_deref(),
            Some("DUAL-SOCKET PASS")
        );
        let failed = vec![
            "socket: probe: dual: getsockopt SO_LINGER refused".to_owned(),
            "socket: probe: DUAL-SOCKET FAIL: SO_LINGER 10022".to_owned(),
        ];
        let said = judge(Arch::X86_64, &failed)
            .err()
            .map(|error| error.to_string());
        assert!(said.is_some_and(|said| said.contains("SO_LINGER 10022")));
    }

    #[test]
    fn the_script_says_what_the_gate_reads() {
        assert!(SCRIPT.contains(PROBE.trim_start_matches("etc/")));
        assert!(SCRIPT.contains(END));
        assert!(RUN_PATH.ends_with("wine-socket") && INIT.contains("/bin/wine-socket"));
    }
}
