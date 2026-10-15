//! Network boot for a bare-metal test machine (`docs/NETBOOT.md`).
//!
//! A machine with no serial port and a disk nobody wants to reflash boots,
//! once and for all, from a USB stick holding iPXE; iPXE asks the network's
//! own DHCP server for an address and fetches everything else from this
//! host, so every boot runs whatever this tree last built.
//!
//! * `netboot-stick --server HOST[:PORT]` writes
//!   `build/x86_64/ferrix-netboot-stick.img`: a GUID partition table whose
//!   EFI system partition holds iPXE's `snp.efi` -- iPXE driving the network
//!   card through the firmware's own UEFI driver, so it needs no driver of
//!   its own for the card -- as `EFI/BOOT/BOOTX64.EFI`, with a script built
//!   in that runs `dhcp` and chains `http://HOST:PORT/ferrix/boot.ipxe`.
//!   iPXE is built from its pinned release source (`fetch-ipxe.sh`).
//! * `netboot [--listen ADDR:PORT]` builds the image as `build` does and
//!   serves its files, and a `boot.ipxe` that fetches them, over HTTP until
//!   it is stopped. It says the commit it serves and logs every request.
//! * `test-netboot` proves the two together under QEMU: OVMF boots the
//!   stick, iPXE takes an address from the gateway's DHCP (`--net`) and
//!   fetches from a server this command runs, and the kernel must reach its
//!   boot marker. Then the negative control: the same stick against the
//!   server serving under another path must fail with iPXE's own error.
//!
//! # How the loader reads its files
//!
//! The loader reads `\FERRIX\KERNEL.ELF`, `INITRD.IMG` and `CMDLINE.TXT`
//! through the `SimpleFileSystem` of the device it was loaded from. When
//! iPXE starts an EFI program it installs one of its own on that device,
//! listing every image it has fetched by its name, and a name may hold a
//! backslash (`efi_file.c` compares the whole of what follows the leading
//! one). So `boot.ipxe` fetches each file under the name of its path on the
//! image's volume, and the loader finds them where it always looks.
//!
//! # The boot log comes back
//!
//! Nothing can be written there, nor to the machine's disk, so the loader
//! sends its boot log to this server instead (`docs/NETBOOT.md` §5):
//! `boot.ipxe` fetches `FERRIX\REPORT.TXT`, the URL to send to, from
//! `report/hello/<MAC>`, and the loader sends the log through iPXE as GETs
//! to `report/box/<MAC>/BOOTLOG.TXT`, which `report.rs` checks, puts
//! together, keeps in a directory per machine and prints. `test-netboot`
//! checks that it arrives and matches the guest's console, and that a
//! server refusing it does not stop the boot.

mod http;
mod report;
mod volume;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fs::File;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ferrix_partition::{ESP, Guid, Partition, write_table};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result, fat, qemu};

/// The iPXE release the stick carries.
const IPXE_VERSION: &str = "2.0.0";
/// Its source tarball's SHA-256, as `tools/common/fetch/fetch-ipxe.sh`
/// pins it.
const IPXE_SHA256: &str = "9ed6d029be901a0ccc87cb2e5f9c774620f30f84ebdd507c6dd3e1e6229b7bd5";
/// The program built: iPXE over the firmware's Simple Network Protocol.
const IPXE_PROGRAM: &str = "bin-x86_64-efi/snp.efi";

/// The port served on when none is named: unprivileged, and not one of
/// QEMU's, VNC's or the gateway's.
const DEFAULT_PORT: u16 = 8086;
/// Every URL served is under this path.
const PREFIX: &str = "/ferrix/";
/// The script the stick chains, under [`PREFIX`].
const SCRIPT_NAME: &str = "boot.ipxe";
/// Where `boot.ipxe` sends what it finds on the PCI bus, under [`PREFIX`]:
/// answered with nothing, logged by the server.
const REPORT: &str = "report/";

/// What a boot that could not fetch its files prints, from either script.
const FAILED: &str = "FERRIX-NETBOOT-FAILED";
/// What the stick prints before `ifstat`'s list of the cards it drives:
/// an empty list is a firmware whose network stack is off (`docs/NETBOOT.md`).
const NO_CARD: &str = "Network cards iPXE drives (none: the firmware's UEFI network stack is off):";
/// Where `boot.ipxe` fetches the URL the loader sends its boot log to, on
/// iPXE's file system (`netreport.rs` in the loader reads it there).
const REPORT_FILE: &str = "FERRIX\\REPORT.TXT";
/// What the loader prints once its boot log reached the server.
const REPORT_SENT: &str = "report   boot log sent to the netboot server";
/// What the loader prints when it did not.
const REPORT_NOT_SENT: &str = "report   boot log not sent";

/// What `boot.ipxe` prints first, so a log says the served script ran.
const SERVED: &str = "Ferrix netboot: commit";

/// The stick's EFI system partition's name.
const STICK_NAME: &str = "FERRIX-NETBOOT";
/// The stick's FAT volume: 64 MiB, the smallest round size FAT32 takes.
const STICK_BYTES: usize = 64 << 20;
/// Where the partition starts: 1 MiB, as every partitioning tool aligns it.
const STICK_FIRST: u64 = 2048;
/// Bytes in a sector of the stick.
const SECTOR: u64 = 512;
/// The stick's disk and partition GUIDs, fixed so that two builds with one
/// server write the same image.
const STICK_DISK_GUID: Guid =
    Guid::from_fields(0x6e62_6f6f, 0x7400, 0x4e42, 0x8a01, 0x4652_5249_584e);
const STICK_PART_GUID: Guid =
    Guid::from_fields(0x6e62_6f6f, 0x7400, 0x4e42, 0x8a02, 0x4652_5249_584e);

/// How long `test-netboot` gives each boot unless `--timeout` says: the
/// files come through iPXE and the gateway before the kernel starts.
const TEST_TIMEOUT: u64 = 300;

/// `netboot`, `netboot-stick` and `test-netboot`.
pub(crate) fn run(command: &str, args: &Args) -> Result<()> {
    if args.arches()?.iter().any(|&arch| arch != Arch::X86_64) {
        return Err(Error::new(format!(
            "{command} is x86-64 only: the stick is an x86-64 UEFI program (docs/NETBOOT.md)"
        )));
    }
    match command {
        "netboot-stick" => {
            let server = args.server.as_deref().ok_or_else(|| {
                Error::new(
                    "netboot-stick needs --server HOST[:PORT]: the address the machine \
                     reaches this host's `cargo xtask netboot` at (docs/NETBOOT.md)",
                )
            })?;
            let server = server_of(server)?;
            let path = paths::build_dir(Arch::X86_64).join("ferrix-netboot-stick.img");
            write_stick(&server, &path)?;
            println!(
                "  write it to a USB stick whole (docs/NETBOOT.md §2); it boots \
                 http://{server}{PREFIX}{SCRIPT_NAME}"
            );
            Ok(())
        }
        "test-netboot" => test(args),
        _ => serve(args),
    }
}

/// What `netboot` serves: the image's files and the script that fetches
/// them, by their paths under [`PREFIX`].
#[derive(Debug)]
struct Served {
    files: BTreeMap<String, Vec<u8>>,
    /// The commit they were built from.
    commit: String,
    /// The kernel with its symbols, for a panic's addresses.
    kernel: PathBuf,
}

/// Build the image as `build` does, with the kernel stripped as `flash`
/// strips it, and take its files out of it.
///
/// Stripped because every byte comes through the network at every boot,
/// and the loader reads the whole ELF before placing it: some 6 MB rather
/// than 70 to 130. The kernel with its symbols stays in `target/`.
fn build_served(args: &Args) -> Result<Served> {
    fat::set_strip_kernel(true);
    let (image, kernel) = crate::build_image(Arch::X86_64, args)?;
    let mut files = volume::files(&image)?;
    let commit = commit();
    let script = boot_script(&files, &commit)?;
    let _ = files.insert(SCRIPT_NAME.to_owned(), script.into_bytes());
    Ok(Served {
        files,
        commit,
        kernel,
    })
}

/// `boot.ipxe`: report the PCI bus, fetch every file of the image under the
/// name of its path, then the URL the loader sends its boot log to as
/// `FERRIX\REPORT.TXT` (`docs/NETBOOT.md` §5), and start the loader.
///
/// Relative URLs, which iPXE resolves against the script's own: the server
/// need not know the address it is reached at.
fn boot_script(files: &BTreeMap<String, Vec<u8>>, commit: &str) -> Result<String> {
    let loader = format!("EFI/BOOT/{}", Arch::X86_64.removable_boot_name());
    if !files.contains_key(&loader) {
        return Err(Error::new(format!("the image has no {loader}")));
    }
    let mut script = format!("#!ipxe\necho {SERVED} {commit}\n{PCI_REPORT}");
    for path in files.keys().filter(|&path| *path != loader) {
        script.push_str(&format!(
            "imgfetch --name {} {path} || goto failed\n",
            path.replace('/', "\\")
        ));
    }
    // Not fatal: without it the loader boots and keeps its log to itself.
    script.push_str(&format!(
        "imgfetch --name {REPORT_FILE} {REPORT}hello/${{netX/mac:hexhyp}} || \
         echo Ferrix netboot: no report URL, the boot log stays on the screen\n"
    ));
    script.push_str(&format!(
        "chain --name {} {loader} || goto failed\n\
         :failed\n\
         echo {FAILED}: boot.ipxe could not fetch or start the files above\n\
         exit 1\n",
        loader.replace('/', "\\")
    ));
    Ok(script)
}

/// The part of `boot.ipxe` that walks the PCI bus: each function's
/// location, vendor and device, and class, on the screen and to the
/// server, which logs them. Every boot does it, because it costs a few
/// requests and says which network and storage chips a machine has before
/// Ferrix has a driver for either.
///
/// `pciscan` leaves the location as a number, which `${pci/N.offset.len}`
/// reads configuration space at; `hexraw` prints the bytes without colons.
const PCI_REPORT: &str = "echo PCI bus: location vendor:device class\n\
:pci\n\
pciscan pcidev || goto pcidone\n\
set pciid ${pci/${pcidev}.0.2:hexraw}:${pci/${pcidev}.2.2:hexraw}\n\
set pciclass ${pci/${pcidev}.9.3:hexraw}\n\
echo PCI ${pcidev:busdevfn} ${pciid} class ${pciclass}\n\
imgfetch --name pcireport report/pci/${pcidev:busdevfn}/${pciid}/${pciclass} || goto pci\n\
imgfree pcireport\n\
goto pci\n\
:pcidone\n";

/// The script built into the stick's iPXE: take an address, chain the
/// served script, and on failure say so and try again, for ever, so that a
/// machine switched on before the server is started boots once it is.
fn embedded_script(server: &str) -> String {
    format!(
        "#!ipxe\n\
         echo Ferrix netboot stick (iPXE {IPXE_VERSION}): http://{server}{PREFIX}{SCRIPT_NAME}\n\
         :retry\n\
         dhcp || goto failed\n\
         chain http://{server}{PREFIX}{SCRIPT_NAME} || goto failed\n\
         :failed\n\
         echo {NO_CARD}\n\
         ifstat ||\n\
         echo {FAILED}: the error is above; trying again in 10 seconds\n\
         sleep 10\n\
         goto retry\n"
    )
}

/// `HOST[:PORT]` as the stick writes it into a URL: a name or an IPv4
/// address, and [`DEFAULT_PORT`] when no port is given.
fn server_of(raw: &str) -> Result<String> {
    let (host, port) = match raw.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .map_err(|_| Error::new(format!("--server {raw}: `{port}` is not a port")))?,
        ),
        None => (raw, DEFAULT_PORT),
    };
    let fits = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-';
    if host.is_empty() || !host.bytes().all(fits) || port == 0 {
        return Err(Error::new(format!(
            "--server {raw}: give a host name or an IPv4 address, and a port if not \
             {DEFAULT_PORT}"
        )));
    }
    Ok(format!("{host}:{port}"))
}

/// Where `netboot` listens: `--listen`'s address, or a bare port on this
/// host's address on its default route, or [`DEFAULT_PORT`] there.
fn listen_of(raw: Option<&str>) -> Result<SocketAddr> {
    if let Some(raw) = raw
        && let Ok(address) = raw.parse::<SocketAddr>()
    {
        return Ok(address);
    }
    let port = match raw {
        Some(raw) => raw.parse::<u16>().map_err(|_| {
            Error::new(format!(
                "--listen {raw}: give ADDRESS:PORT, or a port for this host's own address"
            ))
        })?,
        None => DEFAULT_PORT,
    };
    let address = route_address()
        .ok_or_else(|| Error::new("this host has no default route: give --listen ADDRESS:PORT"))?;
    Ok(SocketAddr::new(IpAddr::V4(address), port))
}

/// This host's address on its default route: the source address the
/// kernel picks for a documentation address (TEST-NET-1, RFC 5737). A UDP
/// `connect` sends nothing; it only chooses the route.
fn route_address() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(address) if !address.is_unspecified() => Some(address),
        _ => None,
    }
}

/// The commit this tree is at, with `+changes` when its tracked files
/// differ from it.
fn commit() -> String {
    let root = paths::workspace_root();
    let git = |arguments: &[&str]| {
        Command::new("git")
            .current_dir(&root)
            .args(arguments)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let head = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    let changed = git(&["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|status| !status.is_empty());
    if changed {
        format!("{head}+changes")
    } else {
        head
    }
}

/// `netboot`: build, then serve until stopped.
fn serve(args: &Args) -> Result<()> {
    let listen = listen_of(args.listen.as_deref())?;
    let served = build_served(args)?;
    let total: usize = served.files.values().map(Vec::len).sum();
    let server = http::Server::start(listen, PREFIX, served.files)?;
    let reports = paths::volume_directory("netboot")?.join("reports");
    server.keep_reports_in(reports.clone());
    let address = server.address();
    println!(
        "netboot: serving commit {} ({} KiB)",
        served.commit,
        total / 1024
    );
    println!("  on http://{address}{PREFIX}{SCRIPT_NAME}, until stopped (Ctrl-C)");
    println!(
        "  a stick for it: cargo xtask netboot-stick --server {address} (docs/NETBOOT.md); \
         the kernel's symbols are in {}",
        served.kernel.display()
    );
    println!(
        "  each machine's loader sends its boot log here; kept in {}/<MAC>/",
        reports.display()
    );
    server.wait();
    Ok(())
}

/// Build iPXE with `script` built in and answer the program.
///
/// From the pinned tarball, checked here as well as by the script that
/// fetched it, unpacked once into this target directory and built there:
/// a gate slot's build never touches another's, and nothing is built in the
/// download's directory.
fn ipxe(script: &str) -> Result<Vec<u8>> {
    if !cfg!(unix) {
        return Err(Error::new(
            "iPXE is built with make and gcc: run netboot-stick and test-netboot on a Linux host",
        ));
    }
    let tarball = paths::volume_directory("netboot")?
        .join("pool")
        .join(format!("ipxe-{IPXE_VERSION}.tar.gz"));
    let bytes = std::fs::read(&tarball).map_err(|error| {
        Error::new(format!(
            "{}: {error}\n  tools/common/fetch/fetch-ipxe.sh fetches it",
            tarball.display()
        ))
    })?;
    if crate::sha256::hex(&bytes) != IPXE_SHA256 {
        return Err(Error::new(format!(
            "{} is not the pinned iPXE {IPXE_VERSION}: delete it and run \
             tools/common/fetch/fetch-ipxe.sh",
            tarball.display()
        )));
    }
    let work = paths::target_dir().join("netboot");
    let source = work.join(format!("ipxe-{IPXE_VERSION}")).join("src");
    if !source.join("Makefile").is_file() {
        std::fs::create_dir_all(&work)?;
        let status = Command::new("tar")
            .arg("-xzf")
            .arg(&tarball)
            .arg("-C")
            .arg(&work)
            .status()
            .map_err(|error| Error::new(format!("could not run tar: {error}")))?;
        if !status.success() || !source.join("Makefile").is_file() {
            return Err(Error::new(format!(
                "unpacking {} into {} failed",
                tarball.display(),
                work.display()
            )));
        }
    }

    // Written only when it changed, so that make rebuilds the one object
    // that embeds it, and nothing when it did not.
    let embedded = work.join("embedded.ipxe");
    if std::fs::read_to_string(&embedded).ok().as_deref() != Some(script) {
        std::fs::write(&embedded, script)?;
    }
    let log = work.join("make.log");
    let output = File::create(&log)?;
    let errors = output.try_clone()?;
    let jobs = std::thread::available_parallelism().map_or(4, std::num::NonZero::get);
    println!(
        "  building iPXE {IPXE_VERSION} {IPXE_PROGRAM} (log {})",
        log.display()
    );
    let status = Command::new("make")
        .current_dir(&source)
        .arg(format!("-j{jobs}"))
        .arg(IPXE_PROGRAM)
        .arg(format!("EMBED={}", embedded.display()))
        .stdin(Stdio::null())
        .stdout(output)
        .stderr(errors)
        .status()
        .map_err(|error| Error::new(format!("could not run make: {error}")))?;
    if !status.success() {
        return Err(Error::new(format!(
            "building iPXE failed (make needs gcc and perl); its output is in {}",
            log.display()
        )));
    }
    let program = std::fs::read(source.join(IPXE_PROGRAM))?;
    // The script is carried as it was written; a program without it is a
    // build make thought was already done.
    if !program
        .windows(script.len())
        .any(|window| window == script.as_bytes())
    {
        return Err(Error::new(format!(
            "{} does not carry the script it was built with",
            source.join(IPXE_PROGRAM).display()
        )));
    }
    Ok(program)
}

/// Write the stick for `server` at `path`.
fn write_stick(server: &str, path: &Path) -> Result<()> {
    let program = ipxe(&embedded_script(server))?;
    let note = format!(
        "Ferrix netboot stick: iPXE {IPXE_VERSION} ({IPXE_PROGRAM}) as EFI/BOOT/BOOTX64.EFI.\r\n\
         It takes an address by DHCP and boots http://{server}{PREFIX}{SCRIPT_NAME},\r\n\
         which `cargo xtask netboot` serves. See docs/NETBOOT.md.\r\n"
    );
    let mut volume = fat::Fat32::new(STICK_BYTES)?;
    volume.add_file(
        &format!("EFI/BOOT/{}", Arch::X86_64.removable_boot_name()),
        &program,
    )?;
    volume.add_file("NETBOOT.TXT", note.as_bytes())?;
    let medium = lay_out(volume.finish())?;
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    std::fs::write(path, &medium)
        .map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
    println!(
        "  netboot stick {} ({} MiB: a GUID partition table and the EFI system partition \
         {STICK_NAME} holding iPXE {IPXE_VERSION}, {} KiB, for {server})",
        path.display(),
        medium.len() >> 20,
        program.len() / 1024
    );
    Ok(())
}

/// The whole stick: a GUID partition table whose one partition, the EFI
/// system partition, holds `volume` from [`STICK_FIRST`].
fn lay_out(mut volume: Vec<u8>) -> Result<Vec<u8>> {
    crate::installer::hide(&mut volume, STICK_FIRST)?;
    let bytes = volume.len() as u64;
    let last = STICK_FIRST + bytes / SECTOR - 1;
    // The partition, then the backup table's 33 sectors, rounded up to a MiB.
    let sectors = (last + 1 + 33).next_multiple_of(STICK_FIRST);
    let partition = Partition::new(ESP, STICK_PART_GUID, STICK_FIRST, last, STICK_NAME);
    let table = write_table(sectors, SECTOR as u32, STICK_DISK_GUID, &[partition])
        .map_err(|error| Error::new(format!("the stick's partition table: {error}")))?;
    let size =
        usize::try_from(sectors * SECTOR).map_err(|_| Error::new("a stick larger than memory"))?;
    let mut medium = vec![0_u8; size];
    let place = |medium: &mut Vec<u8>, at: u64, data: &[u8]| -> Result<()> {
        let at = usize::try_from(at).map_err(|_| Error::new("an offset past memory"))?;
        medium
            .get_mut(at..at + data.len())
            .ok_or_else(|| Error::new("the stick's layout runs past its end"))?
            .copy_from_slice(data);
        Ok(())
    };
    place(&mut medium, STICK_FIRST * SECTOR, &volume)?;
    for (lba, sector) in table {
        place(&mut medium, lba * SECTOR, &sector)?;
    }
    Ok(medium)
}

/// `test-netboot`: see the module documentation.
fn test(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    let served = build_served(args)?;
    let names: Vec<String> = served.files.keys().cloned().collect();
    let server = http::Server::start(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        PREFIX,
        served.files,
    )?;
    // The guest reaches the host's loopback at the gateway's own address.
    let address = format!("{}:{}", crate::gateway::GATEWAY_IP, server.address().port());
    let stick = paths::build_dir(arch).join("netboot-test-stick.img");
    write_stick(&address, &stick)?;

    let mut booted = args.clone();
    booted.net = true;
    // OVMF connects drivers only to the devices in QEMU's boot order, so a
    // card outside it has no Simple Network Protocol and the stick's
    // `snp.efi` finds no network at all (`docs/NETBOOT.md` §5).
    booted.net_boot = true;
    if !booted.timeout_given {
        booted.timeout = TEST_TIMEOUT;
    }
    println!(
        "  {arch}: commit {} served on {}",
        served.commit,
        server.address()
    );
    let reports = paths::target_dir().join("netboot").join("test-reports");
    let _ = std::fs::remove_dir_all(&reports);
    server.keep_reports_in(reports);
    let lines = qemu::test_boot_lines(arch, &stick, &served.kernel, &booted)?;
    let requests = server.requests();
    check_booted(&lines, &requests, &names)?;
    println!(
        "  {arch}: booted from the network: the stick's iPXE took an address by DHCP, \
         ran boot.ipxe and fetched all {} files of the image",
        names.len()
    );
    let report = check_reported(&lines, &server.reports())?;
    println!("  {arch}: {report}");

    // A server that takes no report: the boot goes on, and says why.
    server.refuse_reports(true);
    let lines = qemu::test_boot_lines(arch, &stick, &served.kernel, &booted)?;
    let refused = check_report_refused(&lines, &server.reports())?;
    println!("  {arch}: reports answered 404: the kernel booted, and the loader said: {refused}");
    server.refuse_reports(false);

    // The negative control: the same stick, the same server, every file
    // now under another path.
    server.serve_under("/wrong/");
    let lines = qemu::watch_lines(arch, &stick, &served.kernel, &booted, FAILED)?;
    let requests = server.stop();
    let shown = check_refused(&lines, &requests)?;
    println!("  {arch}: negative control: served under /wrong/, iPXE said: {shown}");
    Ok(())
}

/// What the first boot of `test-netboot` must show: the served script ran,
/// the PCI bus came back, and every file was fetched.
fn check_booted(lines: &[String], requests: &[http::Request], names: &[String]) -> Result<()> {
    if !lines.iter().any(|line| line.contains(SERVED)) {
        return Err(Error::new(format!(
            "the kernel booted, but boot.ipxe never said `{SERVED}`: it did not come \
             through iPXE"
        )));
    }
    for name in names {
        let path = format!("{PREFIX}{name}");
        if !requests
            .iter()
            .any(|request| request.path == path && request.status == 200)
        {
            return Err(Error::new(format!("{path} was never fetched")));
        }
    }
    // QEMU's virtio-net, which the boot came through: the report found it.
    let reported = requests
        .iter()
        .filter(|request| request.path.starts_with(&format!("{PREFIX}{REPORT}pci/")))
        .count();
    if !requests
        .iter()
        .any(|request| request.path.contains("/1af4:1041/02") && request.status == 200)
    {
        return Err(Error::new(format!(
            "boot.ipxe reported {reported} PCI functions, but not the virtio-net card \
             (1af4:1041, class 02) it booted through"
        )));
    }
    println!("  x86_64: boot.ipxe reported {reported} PCI functions to the server");
    Ok(())
}

/// What the first boot's report must be: one boot log, kept on the host,
/// that starts with the loader's first line and whose every line the guest
/// printed on its console, in the same order. Answers what was found.
fn check_reported(lines: &[String], reports: &[report::Report]) -> Result<String> {
    let [report] = reports else {
        return Err(Error::new(format!(
            "the loader's boot log never reached the server: {} reports arrived whole, \
             wanted 1; the loader said: {}",
            reports.len(),
            said(lines, "report   ")
        )));
    };
    let text = String::from_utf8_lossy(&report.bytes);
    let sent: Vec<&str> = text
        .split("\r\n")
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if !sent
        .first()
        .is_some_and(|line| line.starts_with("Ferrix loader"))
    {
        return Err(Error::new(format!(
            "the boot log that reached the server does not start with `Ferrix loader`: {:?}",
            sent.first()
        )));
    }
    let mut console = lines.iter();
    for line in &sent {
        if !console.any(|printed| printed.contains(line)) {
            return Err(Error::new(format!(
                "the boot log that reached the server does not match the guest's console: \
                 `{line}` was not printed there, or not in this order"
            )));
        }
    }
    let kept = report
        .path
        .as_ref()
        .ok_or_else(|| Error::new("the boot log arrived but was not kept on the host"))?;
    if std::fs::read(kept)? != report.bytes {
        return Err(Error::new(format!(
            "{} is not the boot log that arrived",
            kept.display()
        )));
    }
    if !lines.iter().any(|line| line.contains(REPORT_SENT)) {
        return Err(Error::new(format!("the loader never said `{REPORT_SENT}`")));
    }
    Ok(format!(
        "the loader's boot log reached the server from {}: {} bytes, {} lines, each printed \
         on the guest's console in order; kept in {}",
        report.machine,
        report.bytes.len(),
        sent.len(),
        kept.display()
    ))
}

/// What a boot against a server that refuses reports must show: the
/// kernel booted (the caller waited for it), no report arrived, and the
/// loader said it could not send. Answers the loader's line.
fn check_report_refused(lines: &[String], reports: &[report::Report]) -> Result<String> {
    if reports.len() != 1 {
        return Err(Error::new(format!(
            "a report arrived from a boot whose server refused reports ({} in all)",
            reports.len()
        )));
    }
    lines
        .iter()
        .find(|line| line.contains(REPORT_NOT_SENT))
        .map(|line| line.trim().to_owned())
        .ok_or_else(|| {
            Error::new(format!(
                "the server refused the report, but the loader never said `{REPORT_NOT_SENT}`"
            ))
        })
}

/// The guest's lines holding `what`, joined, or `nothing`.
fn said(lines: &[String], what: &str) -> String {
    let found: Vec<&str> = lines
        .iter()
        .filter(|line| line.contains(what))
        .map(|line| line.trim())
        .collect();
    if found.is_empty() {
        "nothing".to_owned()
    } else {
        found.join(" / ")
    }
}

/// What the negative control must show: iPXE's own error for the script it
/// could not fetch, the stick's failure line, the server's 404, and no
/// kernel. Answers the error line.
fn check_refused(lines: &[String], requests: &[http::Request]) -> Result<String> {
    if lines.iter().any(|line| line.contains(qemu::SUCCESS_MARKER)) {
        return Err(Error::new(
            "the negative control booted a kernel from a server serving nothing at /ferrix/",
        ));
    }
    let script = format!("{PREFIX}{SCRIPT_NAME}");
    if !requests
        .iter()
        .any(|request| request.path == script && request.status == 404)
    {
        return Err(Error::new(format!(
            "the negative control never asked for {script}, or was not refused it"
        )));
    }
    // iPXE's error for the script itself: a startup line such as
    // `file:autoexec.ipxe... Not found (https://ipxe.org/...)` is no answer.
    lines
        .iter()
        .find(|line| line.contains(&script) && ipxe_error(line))
        .map(|line| line.trim().to_owned())
        .ok_or_else(|| {
            Error::new(format!(
                "the negative control failed without iPXE's error for {script} \
                 (no `{script}... <error> (https://ipxe.org/<code>)` line)"
            ))
        })
}

/// Whether `line` carries an iPXE error: its explanation's URL, which ends
/// in the error's eight hex digits.
fn ipxe_error(line: &str) -> bool {
    line.split("ipxe.org/")
        .skip(1)
        .any(|after| after.len() >= 8 && after.bytes().take(8).all(|byte| byte.is_ascii_hexdigit()))
}
