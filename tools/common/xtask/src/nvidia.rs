//! `run-nvidia`: Ferrix on the RTX 3060, in libvirt's `ferrix-3060` domain
//! (`docs/NVIDIA.md` §2.3, §7's N0e, §12.3's emulator).
//!
//! The card is shared with the customer's own domains, so before anything
//! starts:
//!
//! * every domain that shares it (`SHARED`) must be shut off, and
//!   `ferrix-3060` itself must not be running already;
//! * the 3060's function 0, `0000:01:00.0`, must be bound to `vfio-pci` on
//!   the host. Nothing here names the RTX 3090 (`03:00.0`), and the domain
//!   is defined with the 3060's function 0 alone;
//! * the card's option ROM is not run (`<rom bar='off'/>`): with a monitor
//!   attached, OVMF's GOP would put a boot framebuffer in BAR1, which the
//!   kernel keeps for its console and so withholds from `nvrm`;
//! * the emulator must be the patched QEMU installed as root at
//!   [`EMULATOR`], whose `--version` says `ferrix-cfi`, so that interrupt
//!   remapping blocks compatibility-format messages (F-57).
//!
//! Then it builds the x86-64 image with `nvrm` as the NVIDIA display
//! controller's driver and `nvrm-hold` as init, and the NVIDIA volume with
//! `nvrm`'s core and the GSP firmware. The image, the volume and the three
//! fixture disks every boot carries (the pattern disk, the btrfs disk and
//! the writable btrfs disk, so that the volume is `vdd`, the first disk the
//! kernel mounts at `/data`) are copied to [`vm_dir`], which libvirt's QEMU
//! can read. The domain is defined from [`domain_xml`], started, its serial
//! port read over TCP into a log and onto this command's output until
//! `nvrm` is up or stops, the kernel panics, or `--timeout` passes, and
//! then destroyed, whatever happened.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::native::{self, Built};
use crate::nvrm::{self, CORE_IN_VOLUME};
use crate::paths::Arch;
use crate::{Error, Result, btrfs_disk, cargo, fat, test_disk};
use crate::{chrome, initramfs, ports, rustc, zinc};

/// The libvirt connection.
const CONNECTION: &str = "qemu:///system";
/// The domain this command defines and starts.
const DOMAIN: &str = "ferrix-3060";
/// The customer's domains that pass the same card through: each must be
/// shut off before `ferrix-3060` starts.
const SHARED: &[&str] = &[
    "GameLab",
    "win11",
    "manjaro",
    "manjaro-kde-test",
    "manjaro-xfce-test",
    "manjaro-sway-test",
];
/// The 3060's function 0, the one function passed through.
const GPU: &str = "0000:01:00.0";
/// The patched QEMU, installed as root where libvirt's QEMU user may run it.
const EMULATOR: &str = "/usr/local/lib/ferrix/qemu/bin/qemu-system-x86_64";
/// The domain's UUID, which libvirt keeps it by: the one it was first
/// defined with (`~/ferrix-nvidia-vm/ferrix-3060.xml`).
const UUID: &str = "daa6c031-6207-4aa4-8fbc-1bfd92bec29f";
/// The serial port's TCP socket.
const SERIAL: &str = "127.0.0.1:47060";
/// Where the GSP firmware is, in the fetched tree and on the volume.
const GSP_FIRMWARE: &str = "lib/firmware/nvidia/580.173.02/gsp_ga10x.bin";
/// The seconds a boot may take when `--timeout` does not say.
const TIMEOUT: u64 = 420;
/// Guest RAM, in MiB.
const MEMORY: u32 = 8192;

/// The lines after which the boot has shown what it can.
const DONE: &[&str] = &[
    "init     the shell exited with",
    "nvrm: stopped",
    "nvos: core refused: ",
    "FERRIX-PANIC",
    " not started: ",
];

/// `~/ferrix-nvidia-vm`, which libvirt's QEMU can traverse.
fn vm_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("ferrix-nvidia-vm")
}

/// `virsh -c qemu:///system` with `arguments`: its standard output, or why
/// it failed.
fn virsh(arguments: &[&str]) -> Result<String> {
    let ran = Command::new("virsh")
        .arg("-c")
        .arg(CONNECTION)
        .args(arguments)
        .output()
        .map_err(|error| Error::new(format!("running virsh: {error}")))?;
    if !ran.status.success() {
        return Err(Error::new(format!(
            "virsh {}: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&ran.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&ran.stdout).into_owned())
}

/// Refuse unless the card is free for this domain: every sharing domain
/// shut off, `ferrix-3060` not running, the 3060 on `vfio-pci`, and the
/// patched emulator installed.
fn guard() -> Result<()> {
    let running = virsh(&["list", "--name", "--state-running"])?;
    let running: Vec<&str> = running
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if let Some(domain) = running.iter().find(|domain| SHARED.contains(domain)) {
        return Err(Error::new(format!(
            "{domain} is running and shares the RTX 3060: shut it off first"
        )));
    }
    if running.contains(&DOMAIN) {
        return Err(Error::new(format!(
            "{DOMAIN} is already running: another session has the card"
        )));
    }
    println!(
        "  card: {} shut off, {DOMAIN} not running",
        SHARED.join(", ")
    );
    let driver = std::fs::read_link(format!("/sys/bus/pci/devices/{GPU}/driver"))
        .map_err(|error| Error::new(format!("{GPU} has no driver bound: {error}")))?;
    if driver.file_name().and_then(|name| name.to_str()) != Some("vfio-pci") {
        return Err(Error::new(format!(
            "{GPU} is bound to {}, not vfio-pci",
            driver.display()
        )));
    }
    let version = Command::new(EMULATOR)
        .arg("--version")
        .output()
        .map_err(|error| {
            Error::new(format!(
                "{EMULATOR} cannot run ({error}): install the patched QEMU as root \
                 (docs/NVIDIA.md §12.3, \"The ferrix-3060 domain's <emulator>\")"
            ))
        })?;
    let version = String::from_utf8_lossy(&version.stdout);
    if !version.contains("ferrix-cfi") {
        return Err(Error::new(format!(
            "{EMULATOR} is not the patched QEMU: `{}`",
            version.lines().next().unwrap_or_default()
        )));
    }
    println!(
        "  card: {GPU} on vfio-pci; emulator {}",
        version.lines().next().unwrap_or_default()
    );
    Ok(())
}

/// The domain: q35 with OVMF and KVM, a VT-d unit remapping interrupts on a
/// split interrupt controller, the 3060's function 0 behind a root port,
/// the image on SATA, the three fixture disks and the volume on virtio
/// through the IOMMU, serial over TCP.
fn domain_xml(dir: &Path) -> String {
    let file = |name: &str| dir.join(name).display().to_string();
    let virtio = |name: &str, slot: u32, readonly: bool| {
        format!(
            "    <disk type='file' device='disk' model='virtio-non-transitional'>\n\
             \x20     <driver name='qemu' type='raw' iommu='on'/>\n\
             \x20     <source file='{}'/>\n\
             \x20     <target dev='vd{}' bus='virtio'/>{}\n\
             \x20     <address type='pci' domain='0x0000' bus='0x00' slot='{slot:#04x}' function='0x0'/>\n\
             \x20   </disk>\n",
            file(name),
            char::from(b'a' + u8::try_from(slot - 8).unwrap_or(0)),
            if readonly { "\n      <readonly/>" } else { "" },
        )
    };
    format!(
        "<domain type='kvm' xmlns:qemu='http://libvirt.org/schemas/domain/qemu/1.0'>
  <name>{DOMAIN}</name>
  <uuid>{UUID}</uuid>
  <description>Ferrix on the RTX 3060 (cargo xtask run-nvidia). Shares the card with {shared}: start only when those are shut off.</description>
  <memory unit='MiB'>{MEMORY}</memory>
  <vcpu>2</vcpu>
  <os>
    <type arch='x86_64' machine='q35'>hvm</type>
    <loader readonly='yes' type='pflash' format='raw'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader>
    <nvram template='/usr/share/OVMF/OVMF_VARS_4M.fd' templateFormat='raw' format='raw'>/var/lib/libvirt/qemu/nvram/{DOMAIN}_VARS.fd</nvram>
  </os>
  <features><acpi/><apic/><ioapic driver='qemu'/></features>
  <cpu mode='host-passthrough' check='none'>
    <maxphysaddr mode='passthrough'/>
  </cpu>
  <clock offset='utc'/>
  <on_poweroff>destroy</on_poweroff>
  <on_reboot>destroy</on_reboot>
  <on_crash>destroy</on_crash>
  <devices>
    <emulator>{EMULATOR}</emulator>
    <disk type='file' device='disk'>
      <driver name='qemu' type='raw'/>
      <source file='{image}'/>
      <target dev='sda' bus='sata'/>
      <boot order='1'/>
    </disk>
{vda}{vdb}{vdc}{vdd}    <serial type='tcp'>
      <source mode='bind' host='127.0.0.1' service='47060'/>
      <protocol type='raw'/>
      <target port='0'/>
    </serial>
    <rng model='virtio-non-transitional'><backend model='random'>/dev/urandom</backend><driver iommu='on'/><address type='pci' domain='0x0000' bus='0x00' slot='0x07' function='0x0'/></rng>
    <iommu model='intel'>
      <driver intremap='on' caching_mode='on' eim='off' aw_bits='48'/>
    </iommu>
    <video><model type='none'/></video>
    <memballoon model='none'/>
    <hostdev mode='subsystem' type='pci' managed='no'>
      <source><address domain='0x0000' bus='0x01' slot='0x00' function='0x0'/></source>
      <rom bar='off'/>
    </hostdev>
  </devices>
  <qemu:commandline>
    <qemu:arg value='-device'/>
    <qemu:arg value='isa-debug-exit,iobase=0xf4,iosize=0x04'/>
  </qemu:commandline>
</domain>
",
        shared = SHARED.join(", "),
        image = file("ferrix.img"),
        vda = virtio("pattern.img", 8, true),
        vdb = virtio("btrfs.img", 9, true),
        vdc = virtio("btrfs-write.img", 10, false),
        vdd = virtio("nvidia.img", 11, false),
    )
}

/// Copy `from` to `name` in `dir`, readable by libvirt's QEMU.
fn place(from: &Path, dir: &Path, name: &str) -> Result<()> {
    let to = dir.join(name);
    let _ = std::fs::remove_file(&to);
    let _ = std::fs::copy(from, &to).map_err(|error| {
        Error::new(format!(
            "copying {} to {}: {error}",
            from.display(),
            to.display()
        ))
    })?;
    Ok(())
}

/// Destroys the domain when dropped, whatever the boot did.
struct Running;

impl Drop for Running {
    fn drop(&mut self) {
        match virsh(&["destroy", DOMAIN]) {
            Ok(_) => println!("  {DOMAIN}: destroyed"),
            Err(why) => println!("  {DOMAIN}: {why}"),
        }
    }
}

/// One line from the serial port, written to `out` and printed: `Ok(None)`
/// when the read timed out, an error when the port closed or failed.
fn read_line(
    reader: &mut BufReader<TcpStream>,
    out: &mut std::fs::File,
) -> std::io::Result<Option<String>> {
    let mut raw = Vec::new();
    match reader.read_until(b'\n', &mut raw) {
        Ok(0) => Err(std::io::ErrorKind::UnexpectedEof.into()),
        Ok(_) => {
            let _ = out.write_all(&raw);
            let line = String::from_utf8_lossy(&raw).trim_end().to_owned();
            println!("  | {line}");
            Ok(Some(line))
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Read the serial port into `log` and onto the output, until a line of
/// [`DONE`] and three seconds after it, or `deadline`. The lines read.
fn capture(log: &Path, deadline: Instant) -> Result<Vec<String>> {
    let stream = loop {
        match TcpStream::connect(SERIAL) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => return Err(Error::new(format!("no serial port at {SERIAL}: {error}"))),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| Error::new(format!("serial port: {error}")))?;
    let mut out = std::fs::File::create(log)
        .map_err(|error| Error::new(format!("creating {}: {error}", log.display())))?;
    let mut reader = BufReader::new(stream);
    let mut lines: Vec<String> = Vec::new();
    let mut until = deadline;
    let mut ended = false;
    while Instant::now() < until {
        match read_line(&mut reader, &mut out) {
            Ok(Some(line)) => {
                if !ended && DONE.iter().any(|marker| line.contains(marker)) {
                    // A moment for what follows the line that ended it.
                    ended = true;
                    until = Instant::now() + Duration::from_secs(3);
                }
                lines.push(line);
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
    if ended {
        Ok(lines)
    } else {
        Err(Error::new(format!(
            "no end line within the timeout; serial output is in {}",
            log.display()
        )))
    }
}

/// Where the static musl busybox is (`tools/common/xtask` uses Alpine's for
/// `test-shell --init`); the script runs its `sleep`, which zinc lacks.
const BUSYBOX: &str = "~/.local/share/ferrix/busybox/x86_64/bin/busybox.static";

/// What init runs: wait for `nvrm` to register `/dev/nvidiactl`, then run
/// NVIDIA's own `nvidia-smi` from the volume, through glibc.
const SCRIPT: &str = r#"export PATH=/bin HOME=/tmp
cd /tmp
i=0
while [ ! -e /dev/nvidiactl ]; do
  i=$((i + 1))
  if [ $i -gt 240 ]; then echo "nvidia-gate: no /dev/nvidiactl after 240 s"; exit 3; fi
  /bin/busybox sleep 1
done
echo "nvidia-gate: /dev/nvidiactl and /dev/nvidia0 are there"
/data/usr/bin/nvidia-smi
echo "nvidia-gate: nvidia-smi exited $?"
/data/usr/bin/nvidia-smi -L
echo "nvidia-gate: nvidia-smi -L exited $?"
exit 16
"#;

/// `$HOME`.
fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// The NVIDIA volume: the fetched release's whole tree, hard-linked into
/// [`vm_dir`] (the same filesystem), with `nvrm`'s core beside it at
/// [`CORE_IN_VOLUME`], made into a btrfs image.
fn whole_volume(dir: &Path, core: &Path) -> Result<()> {
    let tree = dir.join("nvidia.tree");
    if tree.exists() {
        std::fs::remove_dir_all(&tree)
            .map_err(|error| Error::new(format!("clearing {}: {error}", tree.display())))?;
    }
    let source = nvrm::fetched()?.join("tree");
    let linked = Command::new("cp")
        .arg("-al")
        .arg(&source)
        .arg(&tree)
        .status()
        .map_err(|error| Error::new(format!("running cp: {error}")))?;
    if !linked.success() {
        return Err(Error::new(format!(
            "cp -al {} {}: {linked}",
            source.display(),
            tree.display()
        )));
    }
    let to = tree.join(CORE_IN_VOLUME);
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Error::new(format!("making {}: {error}", parent.display())))?;
    }
    let _ = std::fs::copy(core, &to).map_err(|error| {
        Error::new(format!(
            "copying {} to {}: {error}",
            core.display(),
            to.display()
        ))
    })?;
    if !tree.join(GSP_FIRMWARE).is_file() {
        return Err(Error::new(format!(
            "the fetched tree has no {GSP_FIRMWARE}"
        )));
    }
    let image = dir.join("nvidia.img");
    let _ = std::fs::remove_file(&image);
    let file = std::fs::File::create(&image)
        .map_err(|error| Error::new(format!("creating {}: {error}", image.display())))?;
    // The tree is about 1 GB; room for btrfs's own trees beside it.
    file.set_len(1600 << 20)
        .map_err(|error| Error::new(format!("sizing {}: {error}", image.display())))?;
    drop(file);
    let made = Command::new("mkfs.btrfs")
        .arg("-q")
        .arg("--rootdir")
        .arg(&tree)
        .arg(&image)
        .status()
        .map_err(|error| Error::new(format!("running mkfs.btrfs: {error}")))?;
    if !made.success() {
        return Err(Error::new(format!(
            "mkfs.btrfs {}: {made}",
            image.display()
        )));
    }
    Ok(())
}

/// The command.
pub(crate) fn run_nvidia(args: &Args) -> Result<()> {
    if args.arches()? != [Arch::X86_64] {
        return Err(Error::new(
            "run-nvidia runs on x86_64 only: nvrm is built for x86-64",
        ));
    }
    let arch = Arch::X86_64;
    guard()?;
    let dir = vm_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|error| Error::new(format!("making {}: {error}", dir.display())))?;

    let programs = nvrm::build()?;
    let nvrm_bytes = std::fs::read(programs.path("nvrm"))
        .map_err(|error| Error::new(format!("reading nvrm: {error}")))?;
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    let shell_bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let busybox = std::fs::read(BUSYBOX.replace('~', &home())).map_err(|error| {
        Error::new(format!(
            "reading the static busybox at {BUSYBOX} ({error}): the script sleeps with it"
        ))
    })?;
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, SCRIPT)?;
    let mut natives = native::build(arch, args.release)?;
    natives.push(Built {
        name: "nvrm",
        directory: native::DRIVERS,
        bytes: nvrm_bytes,
    });
    let mut files = rustc::files(chrome::LINKS);
    files.push(ports::File {
        path: "bin/busybox".to_owned(),
        mode: 0o755,
        content: ports::Content::Bytes(busybox),
    });
    let archive = initramfs::build(None, &natives, Some(&shell_bytes), &files)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    place(&image, &dir, "ferrix.img")?;
    place(&test_disk::ensure()?, &dir, "pattern.img")?;
    place(&btrfs_disk::ensure()?, &dir, "btrfs.img")?;
    place(&btrfs_disk::ensure_blank(arch)?, &dir, "btrfs-write.img")?;
    whole_volume(&dir, &programs.path("nvrm-core"))?;
    println!(
        "  {}: image, fixture disks and the NVIDIA volume",
        dir.display()
    );

    let xml = dir.join(format!("{DOMAIN}.run.xml"));
    std::fs::write(&xml, domain_xml(&dir))
        .map_err(|error| Error::new(format!("writing {}: {error}", xml.display())))?;
    let _ = virsh(&["define", &xml.display().to_string()])?;
    guard()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let log = dir.join(format!("serial-{stamp}.log"));
    let timeout = if args.timeout_given {
        args.timeout
    } else {
        TIMEOUT
    };
    let _ = virsh(&["start", DOMAIN])?;
    let running = Running;
    println!("  {DOMAIN}: started; serial in {}", log.display());
    let lines = capture(&log, Instant::now() + Duration::from_secs(timeout));
    drop(running);
    let lines = lines?;
    if let Some(line) = lines
        .iter()
        .find(|line| line.contains("nvrm: device_isolation"))
    {
        println!("  recorded: {line}");
    }
    match lines.iter().find(|line| {
        line.contains("FERRIX-PANIC")
            || line.contains("nvrm: stopped")
            || line.contains("core refused")
    }) {
        Some(line) => Err(Error::new(format!(
            "the boot ended with `{line}`; serial in {}",
            log.display()
        ))),
        None => Ok(()),
    }
}
