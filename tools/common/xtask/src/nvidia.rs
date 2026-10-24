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
//!
//! Before the destroy `nvrm` is asked to quiesce the card (`docs/NVIDIA.md`
//! §13), so that QEMU is not killed over a running GSP and a lit display:
//! `run-nvidia`'s script asks at its end, and a desktop with `--ssh` is
//! asked over it ([`quiesce`]). The wait is bounded, and the domain is
//! destroyed whatever the answer.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::native::{self, Built};
use crate::nvrm::{self, CORE_IN_VOLUME};
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, btrfs_disk, cargo, fat, test_disk};
use crate::{chrome, initramfs, ports, rustc, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "run-nvidia",
    help_at: 630,
    help: &[
        "run-nvidia    Boot Ferrix on the RTX 3060 in libvirt's ferrix-3060 domain (the patched QEMU as root, the sharing",
        "              domains shut off), nvrm as its driver with its core and the GSP firmware on the NVIDIA volume;",
        "              print the serial port until nvrm is up or stops, then destroy the domain (x86_64)",
    ],
    run: |_, args| run_nvidia(args),
}];

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
/// What a boot asks of the domain beyond the card and the disks.
struct Machine {
    /// Guest RAM, in MiB.
    memory: u32,
    /// Virtual processors.
    vcpus: u32,
    /// Devices added as they are, such as a network interface.
    devices: String,
    /// The volume's file in [`vm_dir`]: `--everything`'s is its own, kept.
    volume: &'static str,
    /// `--nazuna-layout` ([`Layout::Nazuna`]): the boot display is an
    /// emulated card's, not the 3060's, as nazuna's is its AMD iGPU's.
    layout: Layout,
}

/// Which card the firmware boots on: the domain's display layout.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// No display but the 3060, whose option ROM is not run: OVMF draws
    /// nowhere, and the kernel has no boot framebuffer.
    Card,
    /// nazuna's (`--nazuna-layout`): an emulated card (`bochs`) is the
    /// primary display, so OVMF's GOP framebuffer, the kernel's console and
    /// `efifb` are on it -- seen over VNC on the host's loopback, or with
    /// `virsh screenshot` -- as nazuna's are on its iGPU (`0e:00.0`,
    /// `boot_vga` 1). `rom`: the 3060's option ROM runs too
    /// (`FERRIX_NAZUNA_ROM=1`), as AMI may run every card's GOP, which
    /// shows whether the boot framebuffer stays the emulated card's and
    /// none of the 3060's BAR1 is withheld.
    Nazuna {
        /// Whether the 3060's option ROM runs.
        rom: bool,
    },
}

/// `run-nvidia`'s machine: the card, the disks and the serial port alone.
const GATE_MACHINE: Machine = Machine {
    memory: MEMORY,
    vcpus: 2,
    devices: String::new(),
    volume: VOLUME,
    layout: Layout::Card,
};

/// The NVIDIA volume's file in [`vm_dir`].
const VOLUME: &str = "nvidia.img";

/// `run-compositor --nvidia --everything`'s, kept from run to run.
const EVERYTHING_VOLUME: &str = "nvidia-everything.img";

/// The host processors the domain's virtual ones run on, virtual processor
/// *n* on the *n*th: four cores of nazuna's second CCD, both threads of
/// each, as the guest's topology says (`docs/GAME-PATH.md` §2.5, the rig's
/// R3). The game path's Linux guest, `tools/common/game/linux-3060.xml`, is
/// pinned the same, and a test holds the two equal. A machine has an even
/// number of processors, at most this many.
const VCPU_PINS: [u32; 8] = [6, 18, 7, 19, 8, 20, 9, 21];

/// Where QEMU's own threads run: the rest of that CCD, so that no virtual
/// processor shares a host one with the emulator or its I/O.
const EMULATOR_PINS: &str = "10-11,22-23";

/// Threads to a core in the guest's topology, as on the host.
const THREADS: u32 = 2;

/// The domain's `<cputune>`: [`VCPU_PINS`] and [`EMULATOR_PINS`].
fn cputune(vcpus: u32) -> String {
    let mut out = String::from("  <cputune>\n");
    for (vcpu, host) in VCPU_PINS.iter().take(vcpus as usize).enumerate() {
        out.push_str(&format!("    <vcpupin vcpu='{vcpu}' cpuset='{host}'/>\n"));
    }
    out.push_str(&format!(
        "    <emulatorpin cpuset='{EMULATOR_PINS}'/>\n  </cputune>\n"
    ));
    out
}

/// The domain, as libvirt defines it.
///
/// **Its processors are pinned** ([`VCPU_PINS`]): smoothness in a guest is
/// the host's to give, and the game path compares two guests of one shape.
///
/// **The processor carries `invtsc`.** `host-passthrough` alone hides the
/// invariant-TSC bit (`CPUID.80000007H:EDX[8]`), because a domain with it
/// cannot migrate; libvirt passes it only with `migratable='off'` and the
/// feature required, which QEMU gets as `-cpu host,migratable=off,invtsc=on`.
/// Without the bit the kernel's clock is the HPET (`arch/x86_64/clock.rs`),
/// which QEMU emulates in its own process: every reading of the clock is
/// three exits to QEMU under its one lock, and `gettimeofday` leaves the
/// vDSO. Measured 2026-10-10 on the same machine under plain KVM, Teeworlds
/// on llvmpipe at 1920x1080: 62 swaps a second on the HPET against 103 to 129
/// on the TSC, and an idle `teeworlds_srv` at 18 % of a processor against 2 %
/// (`~/.local/share/ferrix/steam-race/softgl-gap.md`). Every other boot has
/// had the bit since `qemu::x86_cpu` gave it `+invtsc`.
fn domain_xml(dir: &Path, machine: &Machine) -> String {
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
  <memory unit='MiB'>{memory}</memory>
  <vcpu placement='static'>{vcpus}</vcpu>
{cputune}  <os>
    <type arch='x86_64' machine='q35'>hvm</type>
    <loader readonly='yes' type='pflash' format='raw'>/usr/share/OVMF/OVMF_CODE_4M.fd</loader>
    <nvram template='/usr/share/OVMF/OVMF_VARS_4M.fd' templateFormat='raw' format='raw'>/var/lib/libvirt/qemu/nvram/{DOMAIN}_VARS.fd</nvram>
  </os>
  <features><acpi/><apic/><ioapic driver='qemu'/></features>
  <cpu mode='host-passthrough' check='none' migratable='off'>
    <topology sockets='1' dies='1' cores='{cores}' threads='{threads}'/>
    <maxphysaddr mode='passthrough'/>
    <feature policy='require' name='topoext'/>
    <feature policy='require' name='invtsc'/>
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
{video}
    <memballoon model='none'/>
{devices}    <hostdev mode='subsystem' type='pci' managed='no'>
      <source><address domain='0x0000' bus='0x01' slot='0x00' function='0x0'/></source>
      <rom bar='{rom}'/>
    </hostdev>
  </devices>
  <qemu:commandline>
    <qemu:arg value='-device'/>
    <qemu:arg value='isa-debug-exit,iobase=0xf4,iosize=0x04'/>
  </qemu:commandline>
</domain>
",
        shared = SHARED.join(", "),
        video = match machine.layout {
            Layout::Card => "    <video><model type='none'/></video>".to_owned(),
            Layout::Nazuna { .. } => "    <video><model type='bochs' vram='16384' primary='yes'/></video>\n    \
                 <graphics type='vnc' port='-1' autoport='yes' listen='127.0.0.1'/>"
                .to_owned(),
        },
        rom = if machine.layout == (Layout::Nazuna { rom: true }) {
            "on"
        } else {
            "off"
        },
        memory = machine.memory,
        vcpus = machine.vcpus,
        cputune = cputune(machine.vcpus),
        cores = machine.vcpus.div_ceil(THREADS),
        threads = THREADS,
        devices = machine.devices,
        image = file("ferrix.img"),
        vda = virtio("pattern.img", 8, true),
        vdb = virtio("btrfs.img", 9, true),
        vdc = virtio("btrfs-write.img", 10, false),
        vdd = virtio(machine.volume, 11, false),
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

/// The serial port, open, and the log its lines go to.
struct Serial {
    reader: BufReader<TcpStream>,
    out: std::fs::File,
}

/// Read the serial port into `log` and onto the output, until a line of
/// [`DONE`] and three seconds after it, or `deadline`. The lines read.
fn capture(log: &Path, deadline: Instant) -> Result<Vec<String>> {
    capture_keeping(log, deadline).0
}

/// [`capture`], and the port when it was still open at the end, for
/// [`quiesce`] to read on.
fn capture_keeping(log: &Path, deadline: Instant) -> (Result<Vec<String>>, Option<Serial>) {
    match capture_open(log, deadline) {
        Ok((lines, serial)) => (lines, serial),
        Err(error) => (Err(error), None),
    }
}

fn capture_open(log: &Path, deadline: Instant) -> Result<(Result<Vec<String>>, Option<Serial>)> {
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
    let mut open = true;
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
            Err(_) => {
                open = false;
                break;
            }
        }
    }
    let serial = open.then_some(Serial { reader, out });
    let lines = if ended {
        Ok(lines)
    } else {
        Err(Error::new(format!(
            "no end line within the timeout; serial output is in {}",
            log.display()
        )))
    };
    Ok((lines, serial))
}

/// The file whose appearance asks `nvrm` to quiesce its GPU
/// (`nvrm/src/main.c`, `QUIESCE_ASK`).
const QUIESCE_ASK: &str = "/run/nvrm-quiesce";

/// `nvrm`'s last line of a quiesce, and its watchdog's when it is stuck.
const QUIESCE_DONE: &str = "nvrm: quiesce: done in ";
const QUIESCE_STUCK: &str = "nvrm: quiesce: not done after ";

/// How long the destroy waits for `nvrm` to say it is done: a little more
/// than the 20 s after which `nvrm` itself says where it is stuck.
const QUIESCE_WAIT: Duration = Duration::from_secs(25);

/// What a quiesce's serial lines came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quiesced {
    /// `nvrm` said it is done, and whether it said the adapter is shut down.
    Done { adapter_down: bool },
    /// `nvrm` said it is stuck.
    Stuck,
    /// Neither yet.
    Pending,
}

/// Judge the lines read since `nvrm` was asked.
fn quiesced(lines: &[String]) -> Quiesced {
    if lines.iter().any(|line| line.contains(QUIESCE_DONE)) {
        Quiesced::Done {
            adapter_down: lines
                .iter()
                .any(|line| line.contains("nvrm: quiesce: the adapter is shut down")),
        }
    } else if lines.iter().any(|line| line.contains(QUIESCE_STUCK)) {
        Quiesced::Stuck
    } else {
        Quiesced::Pending
    }
}

/// What [`quiesce`] came to, in its line: `refused` when the ssh that asked
/// failed, `after` the seconds it waited.
fn say_quiesced(verdict: Quiesced, refused: bool, after: f32) {
    match verdict {
        Quiesced::Done { adapter_down: true } => {
            println!("  {DOMAIN}: nvrm shut the adapter down in {after:.1} s");
        }
        Quiesced::Done {
            adapter_down: false,
        } => println!(
            "  {DOMAIN}: nvrm ended its quiesce in {after:.1} s WITHOUT shutting the adapter down"
        ),
        Quiesced::Stuck => {
            println!("  {DOMAIN}: nvrm is stuck in its quiesce after {after:.1} s; destroying");
        }
        Quiesced::Pending if refused => {
            println!("  {DOMAIN}: ssh to the guest failed, so nvrm was not asked; destroying");
        }
        Quiesced::Pending => {
            println!("  {DOMAIN}: nvrm did not answer the quiesce within {after:.1} s; destroying");
        }
    }
}

/// Before the domain is destroyed: ask `nvrm`, over the guest's sshd on
/// `port`, to shut its adapter down, and read the serial port until it says
/// it is done or stuck, or [`QUIESCE_WAIT`] has passed. Nothing here fails
/// the command or holds the destroy longer than that: a guest that is
/// gone, has no sshd or does not answer is destroyed as before, and said.
fn quiesce(port: Option<u16>, serial: Option<Serial>) {
    if no_quiesce() {
        println!("  {DOMAIN}: {NO_QUIESCE} is set, so nvrm is not asked to quiesce");
        return;
    }
    let Some(port) = port else {
        println!("  {DOMAIN}: no --ssh, so nvrm is not asked to quiesce before the destroy");
        return;
    };
    let Some(mut serial) = serial else {
        println!("  {DOMAIN}: the guest stopped by itself; nothing to quiesce");
        return;
    };
    let key = PathBuf::from(home()).join(".local/share/ferrix/ssh/id_ed25519");
    let touch = format!(
        "/bin/busybox mkdir -p /run; \
         /bin/busybox touch {QUIESCE_ASK} || touch {QUIESCE_ASK} || : > {QUIESCE_ASK}"
    );
    let asked = Command::new("ssh")
        .args(["-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none"])
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
        ])
        .args([
            "-o",
            "LogLevel=ERROR",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
        ])
        .arg("-i")
        .arg(&key)
        .arg("-p")
        .arg(port.to_string())
        .arg("root@127.0.0.1")
        .arg(&touch)
        .stdin(std::process::Stdio::null())
        .spawn();
    let mut asked = match asked {
        Ok(asked) => asked,
        Err(error) => {
            println!("  {DOMAIN}: nvrm not asked to quiesce: running ssh: {error}");
            return;
        }
    };
    println!(
        "  {DOMAIN}: asked nvrm to quiesce ({QUIESCE_ASK} over ssh); waiting up to {} s",
        QUIESCE_WAIT.as_secs()
    );
    let began = Instant::now();
    let mut lines: Vec<String> = Vec::new();
    let mut verdict = Quiesced::Pending;
    let mut refused = false;
    while began.elapsed() < QUIESCE_WAIT {
        match read_line(&mut serial.reader, &mut serial.out) {
            Ok(Some(line)) => lines.push(line),
            Ok(None) => {}
            Err(_) => break,
        }
        verdict = quiesced(&lines);
        if verdict != Quiesced::Pending {
            break;
        }
        // An ssh that failed asked nothing: no answer will come.
        if let Ok(Some(status)) = asked.try_wait()
            && !status.success()
        {
            refused = true;
            break;
        }
    }
    let _ = asked.kill();
    let _ = asked.wait();
    say_quiesced(verdict, refused, began.elapsed().as_secs_f32());
    // A moment for what the guest says after its last line.
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        if read_line(&mut serial.reader, &mut serial.out).is_err() {
            break;
        }
    }
}

/// Where the static musl busybox is (`tools/common/xtask` uses Alpine's for
/// `test-shell --init`); the script runs its `sleep`, which zinc lacks.
const BUSYBOX: &str = "~/.local/share/ferrix/busybox/x86_64/bin/busybox.static";

/// What init runs: wait for `nvrm` to register `/dev/nvidiactl`, then run
/// NVIDIA's own `nvidia-smi` from the volume, through glibc. At its end it
/// asks `nvrm` to quiesce the card (`docs/NVIDIA.md` §13) and waits, a
/// bounded while, for it to say it has ([`SCRIPT_QUIESCE`]): the domain is
/// destroyed three seconds after the shell exits.
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
if [ -e /bin/preload.so ]; then
  LD_PRELOAD=/bin/preload.so /data/usr/bin/vulkaninfo --summary
else
  /data/usr/bin/vulkaninfo --summary
fi
echo "nvidia-gate: vulkaninfo exited $?"
/bin/vk-offscreen
echo "nvidia-gate: vk-offscreen exited $?"
/bin/vk-dmabuf
echo "nvidia-gate: vk-dmabuf exited $?"
if [ -e /bin/extra/extra.sh ]; then
  /bin/busybox sh /bin/extra/extra.sh
  echo "nvidia-gate: extra.sh exited $?"
fi
if [ -x /data/chrome/chrome-headless-shell ]; then
  /data/chrome/chrome-headless-shell --no-sandbox --use-angle=vulkan --enable-features=Vulkan,DefaultANGLEVulkan,VulkanFromANGLE --ignore-gpu-blocklist --enable-gpu-rasterization --dump-dom 'data:text/html,<p>webgl_renderer=<b id=r>none</b></p><script>var%20g=document.createElement("canvas").getContext("webgl");var%20e=g&&g.getExtension("WEBGL_debug_renderer_info");document.getElementById("r").textContent=g?g.getParameter(e?e.UNMASKED_RENDERER_WEBGL:g.RENDERER):"no_webgl";</script>'
  echo "nvidia-gate: chrome exited $?"
fi
"#;

/// The script's end: ask `nvrm` to quiesce, and wait for it.
const SCRIPT_QUIESCE: &str = r#"/bin/busybox mkdir -p /run
/bin/busybox touch /run/nvrm-quiesce
i=0
while [ ! -e /run/nvrm-quiesced ]; do
  i=$((i + 1))
  if [ $i -gt 25 ]; then echo "nvidia-gate: nvrm did not quiesce within 25 s"; exit 16; fi
  /bin/busybox sleep 1
done
echo "nvidia-gate: nvrm quiesced"
"#;

/// The script's last line: the status `run-nvidia` has always ended with.
const SCRIPT_EXIT: &str = "exit 16\n";

/// Set to anything to leave the card as every boot before §13 left it: no
/// request to quiesce from `run-nvidia`'s script or before a destroy. For
/// telling a fault of the quiesce from a fault it was meant to prevent.
const NO_QUIESCE: &str = "FERRIX_NVIDIA_NO_QUIESCE";

/// Whether [`NO_QUIESCE`] is set.
fn no_quiesce() -> bool {
    std::env::var_os(NO_QUIESCE).is_some()
}

/// `vk-offscreen` (`nvrm/test/vk-offscreen.c`): a clear on the GPU read
/// back, N2's smallest proof of work. Built with the host's compiler against
/// its Vulkan headers and loader, and run on Ferrix with Debian's loader and
/// NVIDIA's ICD from the volume; it asks for nothing newer than glibc 2.34.
fn vk_offscreen(dir: &Path) -> Result<Vec<u8>> {
    vk_test(dir, "vk-offscreen")
}

/// `nvrm/test/<name>.c`, built as [`vk_offscreen`] is: `vk-offscreen`, and
/// `vk-dmabuf`, a block-linear image in video memory shared between two
/// processes as a name-only dmabuf and its pixels compared (N3b).
pub(crate) fn vk_test(dir: &Path, name: &str) -> Result<Vec<u8>> {
    let source = crate::paths::workspace_root()
        .join(format!("src/user/system/linux/drivers/nvrm/test/{name}.c"));
    let out = dir.join(name);
    let built = Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-o"])
        .arg(&out)
        .arg(&source)
        .arg("-lvulkan")
        .status()
        .map_err(|error| Error::new(format!("running cc: {error}")))?;
    if !built.success() {
        return Err(Error::new(format!(
            "building {} failed ({built}); it needs the host's Vulkan headers and libvulkan",
            source.display()
        )));
    }
    std::fs::read(&out).map_err(|error| Error::new(format!("reading {}: {error}", out.display())))
}

/// `$HOME`.
fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Merge Chrome's tree, or `--everything`'s whole one `beside`, into the
/// NVIDIA volume's `tree`.
fn merge_beside(tree: &Path, beside: Option<&Path>) -> Result<()> {
    // Chrome's tree beside it, when fetch-chrome.sh has made it: the two are
    // Debian-shaped and pin the same glibc, so they merge, and Chrome's GPU
    // process finds NVIDIA's ICD where its Vulkan loader looks (N4). Or,
    // for `--everything`, the whole merged tree `beside` (Chrome's among
    // them): yserver, Steam, the compiler.
    let chrome = match beside {
        Some(tree) => Ok(tree.join("everything.img")),
        None => chrome::volume(),
    };
    if let Ok(chrome) = chrome {
        let chrome_tree = chrome.with_file_name("tree");
        if chrome_tree.is_dir() {
            usr_merge(tree, &chrome_tree)?;
            let merged = Command::new("cp")
                .arg("-a")
                .arg("--link")
                .arg("--remove-destination")
                .arg(format!("{}/.", chrome_tree.display()))
                .arg(tree)
                .status()
                .map_err(|error| Error::new(format!("running cp: {error}")))?;
            if !merged.success() {
                return Err(Error::new(format!(
                    "merging {} into {}: {merged}",
                    chrome_tree.display(),
                    tree.display()
                )));
            }
            println!(
                "  volume: Chrome's tree merged from {}",
                chrome_tree.display()
            );
        }
    }
    // virtio-gpu's Vulkan driver, for a card this domain has not. lavapipe
    // stays: yserver renders on it.
    if beside.is_some() {
        let _ = std::fs::remove_file(tree.join("usr/share/vulkan/icd.d/virtio_icd.json"));
        extras(tree)?;
    }
    Ok(())
}

/// `--everything`'s extras on the 3060: `FERRIX_NVIDIA_YSERVER`, a yserver
/// binary in the volume's one's place (a dev build, as ydev.sh swaps one),
/// and `FERRIX_NVIDIA_TEEWORLDS`, a directory (Teeworlds' bundle: `lib/`,
/// `tw/`, `tw.sh`) at `/data/tw`, which the desktop copies to `/tmp/tw` and
/// starts a match from.
fn extras(tree: &Path) -> Result<()> {
    let copy = |from: &std::ffi::OsStr, to: &Path, recursive: bool| -> Result<()> {
        let mut cp = Command::new("cp");
        let _ = cp.arg("--remove-destination");
        if recursive {
            let _ = cp.arg("-rL");
        }
        let done = cp
            .arg(from)
            .arg(to)
            .status()
            .map_err(|error| Error::new(format!("running cp: {error}")))?;
        if done.success() {
            println!(
                "  volume: {} at {}",
                Path::new(from).display(),
                to.display()
            );
            Ok(())
        } else {
            Err(Error::new(format!(
                "copying {} to {}: {done}",
                Path::new(from).display(),
                to.display()
            )))
        }
    };
    if let Some(yserver) = std::env::var_os("FERRIX_NVIDIA_YSERVER") {
        copy(&yserver, &tree.join("yserver/yserver"), false)?;
    }
    if let Some(teeworlds) = std::env::var_os("FERRIX_NVIDIA_TEEWORLDS") {
        let to = tree.join("tw");
        let _ = std::fs::remove_dir_all(&to);
        copy(&teeworlds, &to, true)?;
    }
    Ok(())
}

/// Where the tree merged in has a top-level directory as a link into
/// `usr` (`lib -> usr/lib`, Debian's merged /usr) and the NVIDIA tree has
/// it as a directory (`lib/firmware`), move the directory's entries to
/// where the link points, so `cp` can put the link in its place.
fn usr_merge(tree: &Path, beside: &Path) -> Result<()> {
    for name in ["bin", "lib", "lib64", "sbin"] {
        let ours = tree.join(name);
        let Ok(target) = std::fs::read_link(beside.join(name)) else {
            continue;
        };
        if !ours.is_dir() || ours.is_symlink() {
            continue;
        }
        let moved = Command::new("cp")
            .arg("-al")
            .arg(format!("{}/.", ours.display()))
            .arg(tree.join(&target))
            .status()
            .map_err(|error| Error::new(format!("running cp: {error}")))?;
        if !moved.success() {
            return Err(Error::new(format!(
                "moving {} into {}: {moved}",
                ours.display(),
                target.display()
            )));
        }
        std::fs::remove_dir_all(&ours)
            .map_err(|error| Error::new(format!("removing {}: {error}", ours.display())))?;
    }
    Ok(())
}

/// What a kept `--everything` volume was made from: the core's and the
/// everything volume's sizes and times.
fn volume_stamp(core: &Path, everything: &Path) -> String {
    let seen = |path: &Path| {
        std::fs::metadata(path).map_or_else(
            |_| "-".to_owned(),
            |meta| format!("{} {:?}", meta.len(), meta.modified().ok()),
        )
    };
    let named = |name: &str| std::env::var(name).unwrap_or_default();
    format!(
        "{}\n{}\n{}\n{}\n",
        seen(core),
        seen(&everything.join("everything.img")),
        named("FERRIX_NVIDIA_YSERVER"),
        named("FERRIX_NVIDIA_TEEWORLDS")
    )
}

/// The NVIDIA volume: the fetched release's whole tree, hard-linked into
/// [`vm_dir`] (the same filesystem), with `nvrm`'s core beside it at
/// [`CORE_IN_VOLUME`], made into a btrfs image.
fn whole_volume(dir: &Path, core: &Path, beside: Option<&Path>) -> Result<()> {
    // `--everything`'s volume is kept from run to run while the core and the
    // everything volume are the ones it was made from: what Steam installed
    // and signed in to is on it. FERRIX_NVIDIA_FRESH_VOLUME makes it anew.
    let image = dir.join(if beside.is_some() {
        EVERYTHING_VOLUME
    } else {
        VOLUME
    });
    let kept = image.with_extension("kept");
    let stamp = beside.map(|everything| volume_stamp(core, everything));
    if let Some(stamp) = &stamp
        && std::env::var_os("FERRIX_NVIDIA_FRESH_VOLUME").is_none()
        && image.is_file()
        && std::fs::read_to_string(&kept).ok().as_deref() == Some(stamp.as_str())
    {
        println!(
            "  volume: {} kept, with what Steam put on it",
            image.display()
        );
        return Ok(());
    }
    let _ = std::fs::remove_file(&kept);
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
    merge_beside(&tree, beside)?;
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
    let bytes = tree_bytes(&tree);
    let _ = std::fs::remove_file(&image);
    let file = std::fs::File::create(&image)
        .map_err(|error| Error::new(format!("creating {}: {error}", image.display())))?;
    // Room for btrfs's own trees beside the files, and for what programs
    // write to /data while they run.
    // `--everything` has Steam's client and games to install: 16 GiB more,
    // sparse until written.
    let spare: u64 = if beside.is_some() {
        16 << 30
    } else {
        512 << 20
    };
    file.set_len(bytes + bytes / 4 + spare)
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
    if let Some(stamp) = stamp {
        let _ = std::fs::write(&kept, stamp);
    }
    Ok(())
}

/// The bytes of the regular files under `tree`, each hard link once.
fn tree_bytes(tree: &Path) -> u64 {
    let ran = Command::new("du").arg("-sb").arg(tree).output();
    ran.ok()
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or(4 << 30)
}

/// What the initramfs carries beside the drivers and zinc: the links that
/// put the volume's libraries and manifests where glibc, the Vulkan loader
/// and libEGL look, `vk-offscreen`, the static busybox the script sleeps
/// with, and an optional preload.
fn initramfs_files(dir: &Path) -> Result<Vec<ports::File>> {
    let busybox = std::fs::read(BUSYBOX.replace('~', &home())).map_err(|error| {
        Error::new(format!(
            "reading the static busybox at {BUSYBOX} ({error}): the script sleeps with it"
        ))
    })?;
    let mut files = rustc::files(chrome::LINKS);
    // The Vulkan loader's ICD manifests; glvnd's EGL vendor and NVIDIA's EGL
    // platform manifests, which NVIDIA's Vulkan driver reads through libEGL
    // when no display is set; and NVIDIA's application profiles.
    files.extend(rustc::files(&[
        ("usr/share/vulkan", "/data/usr/share/vulkan"),
        ("usr/share/glvnd", "/data/usr/share/glvnd"),
        ("usr/share/egl", "/data/usr/share/egl"),
        ("usr/share/nvidia", "/data/usr/share/nvidia"),
    ]));
    files.push(ports::File {
        path: "bin/vk-offscreen".to_owned(),
        mode: 0o755,
        content: ports::Content::Bytes(vk_offscreen(dir)?),
    });
    files.push(ports::File {
        path: "bin/vk-dmabuf".to_owned(),
        mode: 0o755,
        content: ports::Content::Bytes(vk_test(dir, "vk-dmabuf")?),
    });
    files.push(ports::File {
        path: "bin/busybox".to_owned(),
        mode: 0o755,
        content: ports::Content::Bytes(busybox),
    });
    // A bring-up aid: `FERRIX_RUN_NVIDIA_PRELOAD` names a shared library
    // the script preloads into vulkaninfo, such as a tracer of its calls.
    if let Some(preload) = std::env::var_os("FERRIX_RUN_NVIDIA_PRELOAD") {
        let bytes = std::fs::read(&preload).map_err(|error| {
            Error::new(format!(
                "reading {}: {error}",
                Path::new(&preload).display()
            ))
        })?;
        files.push(ports::File {
            path: "bin/preload.so".to_owned(),
            mode: 0o755,
            content: ports::Content::Bytes(bytes),
        });
    }
    // Another: `FERRIX_RUN_NVIDIA_EXTRA` names a directory whose files go to
    // `/bin/extra`; the script runs its `extra.sh` after vk-offscreen.
    if let Some(extra) = std::env::var_os("FERRIX_RUN_NVIDIA_EXTRA") {
        let entries = std::fs::read_dir(&extra).map_err(|error| {
            Error::new(format!("reading {}: {error}", Path::new(&extra).display()))
        })?;
        for entry in entries {
            let entry = entry
                .map_err(|error| Error::new(format!("reading the extra directory: {error}")))?;
            let bytes = std::fs::read(entry.path()).map_err(|error| {
                Error::new(format!("reading {}: {error}", entry.path().display()))
            })?;
            files.push(ports::File {
                path: format!("bin/extra/{}", entry.file_name().to_string_lossy()),
                mode: 0o755,
                content: ports::Content::Bytes(bytes),
            });
        }
    }
    Ok(files)
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
    let loader = cargo::build_loader(arch, args.release)?;
    let script = if no_quiesce() {
        println!("  {NO_QUIESCE} is set: the script does not ask nvrm to quiesce");
        format!("{SCRIPT}{SCRIPT_EXIT}")
    } else {
        format!("{SCRIPT}{SCRIPT_QUIESCE}{SCRIPT_EXIT}")
    };
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, &script)?;
    let mut natives = native::build(arch, args.release)?;
    natives.push(Built {
        name: "nvrm",
        directory: native::DRIVERS,
        bytes: nvrm_bytes,
    });
    let files = initramfs_files(&dir)?;
    let archive = initramfs::build(None, &natives, Some(&shell_bytes), &files)?;
    // The boot's self-checks are `test-boot`'s evidence, not this domain's;
    // two vCPUs on a loaded host make the timing ones flake here.
    let image = fat::write_image_with(
        arch,
        &loader,
        &kernel,
        &archive,
        Some("ferrix.checks=skip\n"),
    )?;
    place(&image, &dir, "ferrix.img")?;
    place(&test_disk::ensure()?, &dir, "pattern.img")?;
    place(&btrfs_disk::ensure()?, &dir, "btrfs.img")?;
    place(&btrfs_disk::ensure_blank(arch)?, &dir, "btrfs-write.img")?;
    whole_volume(&dir, &programs.path("nvrm-core"), None)?;
    println!(
        "  {}: image, fixture disks and the NVIDIA volume",
        dir.display()
    );

    let xml = dir.join(format!("{DOMAIN}.run.xml"));
    std::fs::write(&xml, domain_xml(&dir, &GATE_MACHINE))
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

/// `nvrm`, as devmgr starts it, for an image built elsewhere:
/// `run-compositor --nvidia`'s desktop.
///
/// # Errors
///
/// As [`nvrm::build`].
pub(crate) fn nvrm_native() -> Result<Built> {
    let programs = nvrm::build()?;
    let bytes = std::fs::read(programs.path("nvrm"))
        .map_err(|error| Error::new(format!("reading nvrm: {error}")))?;
    Ok(Built {
        name: "nvrm",
        directory: native::DRIVERS,
        bytes,
    })
}

/// The links NVIDIA's userspace finds its data through, from the volume at
/// `/data`: the Vulkan loader's ICD manifests; glvnd's EGL vendor and
/// NVIDIA's EGL platform manifests, which NVIDIA's Vulkan driver reads
/// through libEGL; and NVIDIA's application profiles.
pub(crate) fn data_links() -> Vec<ports::File> {
    let mut files = rustc::files(&[
        ("usr/share/vulkan", "/data/usr/share/vulkan"),
        ("usr/share/glvnd", "/data/usr/share/glvnd"),
        ("usr/share/egl", "/data/usr/share/egl"),
        ("usr/share/nvidia", "/data/usr/share/nvidia"),
    ]);
    files.push(ports::File {
        path: WEBGL_PAGE.trim_start_matches("file:///").to_owned(),
        mode: 0o644,
        content: ports::Content::Bytes(include_bytes!("nvidia/webgl.html").to_vec()),
    });
    files
}

/// The page `run-compositor --nvidia`'s Chrome opens: which GPU renders
/// WebGL, said large, over a cube spun on it, with its frame rate.
pub(crate) const WEBGL_PAGE: &str = "file:///etc/ferrix/nvidia-webgl.html";

/// The host's keyboards and mice the desktop on the 3060's monitor is
/// driven with: `FERRIX_NVIDIA_INPUT`, a colon-separated list of evdev
/// devices (`/dev/input/by-id/...-event-kbd`, `...-event-mouse`), each
/// grabbed for the guest alone while it runs. Devices kept for the 3060's
/// monitor, not the host's own: a grabbed device types into the guest only.
/// The guest gets a virtio keyboard and mouse, which the host's events
/// reach; none without the variable.
fn input_devices() -> String {
    let Some(list) = std::env::var_os("FERRIX_NVIDIA_INPUT") else {
        return String::new();
    };
    let mut xml = String::from(
        "    <input type='keyboard' bus='virtio'>\n\
         \x20     <driver iommu='on'/>\n\
         \x20   </input>\n\
         \x20   <input type='mouse' bus='virtio'>\n\
         \x20     <driver iommu='on'/>\n\
         \x20   </input>\n",
    );
    for device in std::env::split_paths(&list) {
        if !device.exists() {
            println!("  input: {} is not there; left out", device.display());
            continue;
        }
        println!("  input: {} grabbed for the guest", device.display());
        xml.push_str(&format!(
            "    <input type='evdev'>\n\
             \x20     <source dev='{}' grab='all' repeat='on'/>\n\
             \x20   </input>\n",
            device.display()
        ));
    }
    xml
}

/// How long `run-compositor --nvidia` keeps the desktop up when `--timeout`
/// does not say: an hour.
const DESKTOP_TIMEOUT: u64 = 3600;

/// The desktop's machine: `--everything`'s memory and volume when
/// `everything`, the layout `--nazuna-layout` asks for, and the network and
/// the input devices passed through.
fn desktop_machine(args: &Args, everything: bool) -> Machine {
    Machine {
        memory: if everything {
            crate::compositor::steam_window::MEMORY
        } else {
            MEMORY
        },
        volume: if everything {
            EVERYTHING_VOLUME
        } else {
            VOLUME
        },
        vcpus: 8,
        layout: if args.nazuna_layout {
            Layout::Nazuna {
                rom: std::env::var_os("FERRIX_NAZUNA_ROM").is_some(),
            }
        } else {
            Layout::Card
        },
        devices: format!(
            "    <interface type='network'>\n\
             \x20     <mac address='{CARDVM_MAC}'/>\n\
             \x20     <source network='default'/>\n\
             \x20     <model type='virtio-non-transitional'/>\n\
             \x20     <driver iommu='on'/>\n\
             \x20   </interface>\n{}",
            input_devices()
        ),
    }
}

/// `run-compositor --nvidia`: the desktop `image` (hyprix, Chrome, nvrm) in
/// the 3060's domain, on the card's own monitor. The disks are
/// `run-nvidia`'s: the fixtures the kernel's early stages expect, then the
/// NVIDIA volume, which carries Chrome's tree too, at `/data`. The root is
/// the initramfs, as a desktop without `--persistent` has it. libvirt's
/// default network gives Chrome somewhere to browse. The serial console
/// is followed until `--timeout`, an hour by default, or a panic, and the
/// domain destroyed after.
///
/// # Errors
///
/// The guard's, a volume or disk that cannot be made, or libvirt refusing.
pub(crate) fn run_desktop(image: &Path, args: &Args) -> Result<()> {
    // `FERRIX_N3C_BUILD_ONLY`: the image is built, and the card is not booted.
    if std::env::var_os("FERRIX_N3C_BUILD_ONLY").is_some() {
        println!("  n3c: built {}; not booting", image.display());
        return Ok(());
    }
    guard()?;
    let dir = vm_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|error| Error::new(format!("making {}: {error}", dir.display())))?;
    let programs = nvrm::build()?;
    place(image, &dir, "ferrix.img")?;
    place(&test_disk::ensure()?, &dir, "pattern.img")?;
    place(&btrfs_disk::ensure()?, &dir, "btrfs.img")?;
    place(
        &btrfs_disk::ensure_blank(Arch::X86_64)?,
        &dir,
        "btrfs-write.img",
    )?;
    // `--everything`: its volume's tree (yserver, Steam, the compiler,
    // Chrome) merged with NVIDIA's, and Steam's memory.
    let everything = if args.everything {
        args.data_image.as_deref().and_then(Path::parent)
    } else {
        None
    };
    whole_volume(&dir, &programs.path("nvrm-core"), everything)?;
    let machine = desktop_machine(args, everything.is_some());
    let xml = dir.join(format!("{DOMAIN}.desktop.xml"));
    std::fs::write(&xml, domain_xml(&dir, &machine))
        .map_err(|error| Error::new(format!("writing {}: {error}", xml.display())))?;
    let _ = virsh(&["define", &xml.display().to_string()])?;
    guard()?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let log = dir.join(format!("desktop-{stamp}.log"));
    let timeout = if args.timeout_given {
        args.timeout
    } else {
        DESKTOP_TIMEOUT
    };
    // `--ssh` without `--timeout`: the long-lived card VM (`tools/common/nvidia/card.sh`).
    let kept = args.ssh.is_some() && !args.timeout_given;
    let stop = cardvm_file(CARDVM_STOP);
    let state = cardvm_file(CARDVM_STATE);
    let _ = std::fs::remove_file(&stop);
    let _ = std::fs::remove_file(&state);
    let _ = virsh(&["start", DOMAIN])?;
    let running = Running;
    if let Some(port) = args.ssh {
        forward_ssh(port, log.clone());
    }
    if kept {
        println!(
            "  {DOMAIN}: the card VM is starting on the 3060's monitor; serial in {}; \
             kept until {} appears (`tools/common/nvidia/card.sh down`)",
            log.display(),
            stop.display()
        );
        let followed = follow(&log, &stop);
        let followed = match followed {
            Ok(serial) => {
                quiesce(args.ssh, serial);
                Ok(())
            }
            Err(error) => Err(error),
        };
        let _ = std::fs::remove_file(&state);
        drop(running);
        let _ = std::fs::remove_file(&stop);
        return followed;
    }
    println!(
        "  {DOMAIN}: the desktop is starting on the 3060's monitor; serial in {} for {timeout} s",
        log.display()
    );
    // A desktop has no end line: the timeout, or the port closing, is the end.
    let (_, serial) = capture_keeping(&log, Instant::now() + Duration::from_secs(timeout));
    quiesce(args.ssh, serial);
    drop(running);
    Ok(())
}

/// The card VM's network address: fixed, so that its lease is found by it.
const CARDVM_MAC: &str = "52:54:00:fe:30:60";

/// The file whose appearance ends a kept card VM (`card.sh down`).
const CARDVM_STOP: &str = "cardvm-stop";

/// Where a kept card VM says how to reach it: shell assignments `PORT=`,
/// `ADDRESS=`, `LOG=`, `PID=` (`card.sh` sources it).
const CARDVM_STATE: &str = "cardvm.env";

/// `name` in `~/.local/share/ferrix/nvidia`, beside the card lock.
fn cardvm_file(name: &str) -> PathBuf {
    PathBuf::from(home())
        .join(".local/share/ferrix/nvidia")
        .join(name)
}

/// The guest's IPv4 address from libvirt's DHCP lease for [`CARDVM_MAC`].
fn guest_address() -> Option<String> {
    let leases = virsh(&["domifaddr", DOMAIN, "--source", "lease"]).ok()?;
    leases
        .lines()
        .filter(|line| line.contains(CARDVM_MAC) && line.contains("ipv4"))
        .find_map(|line| line.split_whitespace().last())
        .and_then(|address| address.split('/').next())
        .map(str::to_owned)
}

/// `--ssh <port>` on the 3060: the guest is on libvirt's NAT network, so
/// `127.0.0.1:<port>` is forwarded to its port 22 here, by this process,
/// once its lease is there -- the same address and port a QEMU boot's
/// `--ssh` gives. The state file is written when the forward listens.
fn forward_ssh(port: u16, log: PathBuf) {
    let _ = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(900);
        let address = loop {
            if let Some(address) = guest_address() {
                break address;
            }
            if Instant::now() > deadline {
                println!("  cardvm: no DHCP lease for {CARDVM_MAC} after 900 s; no ssh");
                return;
            }
            std::thread::sleep(Duration::from_secs(2));
        };
        let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => listener,
            Err(error) => {
                println!("  cardvm: cannot listen on 127.0.0.1:{port}: {error}");
                return;
            }
        };
        let target = format!("{address}:{}", crate::ssh::GUEST_PORT);
        let state = format!(
            "PORT={port}\nADDRESS={address}\nLOG={}\nPID={}\n",
            log.display(),
            std::process::id()
        );
        let _ = std::fs::write(cardvm_file(CARDVM_STATE), state);
        println!(
            "  cardvm: the guest is {address}; 127.0.0.1:{port} forwards to its sshd \
             (tools/common/nvidia/card.sh ssh <command>)"
        );
        for client in listener.incoming().flatten() {
            let target = target.clone();
            let _ = std::thread::spawn(move || {
                let Ok(server) = TcpStream::connect(&target) else {
                    return;
                };
                let (Ok(mut client_read), Ok(mut server_read)) =
                    (client.try_clone(), server.try_clone())
                else {
                    return;
                };
                let (mut client, mut server) = (client, server);
                let up = std::thread::spawn(move || {
                    let _ = std::io::copy(&mut client_read, &mut server);
                    let _ = server.shutdown(std::net::Shutdown::Write);
                });
                let _ = std::io::copy(&mut server_read, &mut client);
                let _ = client.shutdown(std::net::Shutdown::Write);
                let _ = up.join();
            });
        }
    });
}

/// A kept card VM's serial console, into `log` and onto the output, until
/// `stop` appears or the port closes (the guest powered off or rebooted;
/// the domain destroys itself on either). The port, unless it closed.
fn follow(log: &Path, stop: &Path) -> Result<Option<Serial>> {
    let stream = loop {
        match TcpStream::connect(SERIAL) {
            Ok(stream) => break stream,
            Err(_) if !stop.exists() => std::thread::sleep(Duration::from_millis(100)),
            Err(error) => return Err(Error::new(format!("no serial port at {SERIAL}: {error}"))),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| Error::new(format!("serial port: {error}")))?;
    let out = std::fs::File::create(log)
        .map_err(|error| Error::new(format!("creating {}: {error}", log.display())))?;
    let mut serial = Serial {
        reader: BufReader::new(stream),
        out,
    };
    let mut checked = Instant::now();
    loop {
        if stop.exists() {
            println!("  cardvm: {} is there; stopping", stop.display());
            return Ok(Some(serial));
        }
        // The card protocol: a want-* file is an agent queued for a boot of
        // its own (a kernel or nvrm change), and the card VM yields to it.
        if checked.elapsed() >= Duration::from_secs(2) {
            checked = Instant::now();
            if let Some(want) = wanted(stop) {
                println!("  cardvm: {want} wants the card; stopping");
                return Ok(Some(serial));
            }
        }
        if read_line(&mut serial.reader, &mut serial.out).is_err() {
            println!("  cardvm: the serial port closed: the guest stopped");
            return Ok(None);
        }
    }
}

/// A `want-*` file beside `stop` other than the card VM's own, if any.
fn wanted(stop: &Path) -> Option<String> {
    std::fs::read_dir(stop.parent()?)
        .ok()?
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .find(|name| name.starts_with("want-") && name != "want-cardvm")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The card's domain is defined with the invariant-TSC bit, for the
    /// gate's machine and the desktop's alike: without it the kernel's clock
    /// is the HPET, which QEMU emulates in its own process.
    #[test]
    fn the_domain_has_an_invariant_tsc() {
        let xml = domain_xml(Path::new("/vm"), &GATE_MACHINE);
        assert!(xml.contains("<cpu mode='host-passthrough' check='none' migratable='off'>"));
        assert!(xml.contains("<feature policy='require' name='invtsc'/>"));
    }

    /// The part of a domain between `open` and `close`, both included.
    fn between<'a>(xml: &'a str, open: &str, close: &str) -> &'a str {
        let (_, from) = xml.split_once(open).expect("the opening tag is there");
        let (inside, _) = from.split_once(close).expect("the closing tag is there");
        let start = xml.len() - from.len() - open.len();
        xml.get(start..start + open.len() + inside.len() + close.len())
            .expect("on character boundaries")
    }

    /// The game path's two guests are the same shape (`docs/GAME-PATH.md`
    /// §2.5): the desktop's machine has the Linux guest's processor, its
    /// topology and its pinning, as `tools/common/game/linux-3060.xml`
    /// defines them, word for word.
    #[test]
    fn the_desktop_is_pinned_as_the_linux_guest_is() {
        let linux = include_str!("../../game/linux-3060.xml");
        let ferrix = domain_xml(
            Path::new("/vm"),
            &Machine {
                memory: MEMORY,
                vcpus: 8,
                devices: String::new(),
                volume: VOLUME,
                layout: Layout::Card,
            },
        );
        for (open, close) in [("<cputune>", "</cputune>"), ("<cpu ", "</cpu>")] {
            assert_eq!(between(&ferrix, open, close), between(linux, open, close));
        }
        assert!(linux.contains("<vcpu placement='static'>8</vcpu>"));
        assert!(ferrix.contains("<vcpu placement='static'>8</vcpu>"));
    }

    /// The gate's two processors are one core's two threads.
    #[test]
    fn the_gate_machine_is_one_core_pinned() {
        let xml = domain_xml(Path::new("/vm"), &GATE_MACHINE);
        assert!(xml.contains("<vcpupin vcpu='0' cpuset='6'/>"));
        assert!(xml.contains("<vcpupin vcpu='1' cpuset='18'/>"));
        assert!(!xml.contains("<vcpupin vcpu='2'"));
        assert!(xml.contains("<topology sockets='1' dies='1' cores='1' threads='2'/>"));
    }
    /// nazuna's layout puts the boot display on an emulated card, seen on
    /// the host's loopback alone, and leaves the 3060's option ROM off
    /// unless asked; the card's own layout has no display but the 3060.
    #[test]
    fn nazunas_layout_boots_on_an_emulated_card() {
        let machine = |layout| Machine {
            memory: MEMORY,
            vcpus: 8,
            devices: String::new(),
            volume: VOLUME,
            layout,
        };
        let card = domain_xml(Path::new("/vm"), &machine(Layout::Card));
        assert!(card.contains("<video><model type='none'/></video>"));
        assert!(!card.contains("<graphics"));
        assert!(card.contains("<rom bar='off'/>"));
        let nazuna = domain_xml(Path::new("/vm"), &machine(Layout::Nazuna { rom: false }));
        assert!(nazuna.contains("<model type='bochs' vram='16384' primary='yes'/>"));
        assert!(
            nazuna.contains("<graphics type='vnc' port='-1' autoport='yes' listen='127.0.0.1'/>")
        );
        assert!(nazuna.contains("<rom bar='off'/>"));
        let rom = domain_xml(Path::new("/vm"), &machine(Layout::Nazuna { rom: true }));
        assert!(rom.contains("<rom bar='on'/>"));
    }

    fn lines(transcript: &str) -> Vec<String> {
        transcript.lines().map(str::to_owned).collect()
    }

    // As nvrm prints a quiesce (`nvrm/src/main.c`), between other lines.
    const DONE: &str = "\
nvrm: quiesce: asked by /run/nvrm-quiesce
hyprix: frames 12 slowest of the last 12 100313 us
nvrm: quiesce: the device files refuse requests; 7 open file(s) closed
nvrm: quiesce: NVKMS: 1 device(s) released and freed
nvrm: quiesce: the adapter is shut down
nvrm: quiesce: NV_PMC_BOOT_0 reads 0xb76000a1
nvrm: quiesce: done in 1840 ms
";

    #[test]
    fn a_quiesce_is_done_only_at_its_last_line() {
        assert_eq!(
            quiesced(&lines(DONE)),
            Quiesced::Done { adapter_down: true }
        );
        let cut = DONE.replace("nvrm: quiesce: done in 1840 ms\n", "");
        assert_eq!(quiesced(&lines(&cut)), Quiesced::Pending);
        assert_eq!(quiesced(&[]), Quiesced::Pending);
    }

    #[test]
    fn a_quiesce_that_left_the_adapter_up_or_is_stuck_is_told_apart() {
        let left = DONE.replace(
            "nvrm: quiesce: the adapter is shut down\n",
            "nvrm: quiesce: the adapter was NOT shut down: requests are still under way, and NVKMS and the adapter are left as they are\n",
        );
        assert_eq!(
            quiesced(&lines(&left)),
            Quiesced::Done {
                adapter_down: false
            }
        );
        let skeleton = "nvrm: quiesce: no GPU is started: nothing to shut down\n\
                        nvrm: quiesce: done in 0 ms\n";
        assert_eq!(
            quiesced(&lines(skeleton)),
            Quiesced::Done {
                adapter_down: false
            }
        );
        let stuck = "nvrm: quiesce: asked by /run/nvrm-quiesce\n\
                     nvrm: quiesce: not done after 20 s, in NVKMS (freeing its device); the GPU is left as it is\n";
        assert_eq!(quiesced(&lines(stuck)), Quiesced::Stuck);
    }
}
