//! `devmgr`: the program that matches the devices the kernel found to the
//! drivers the initramfs carries, starts each driver in a job of its own with
//! exactly what `docs/ARCHITECTURE.md` §7 says a driver gets, and makes a
//! device safe again when its driver dies. `docs/DEVMGR.md` is the protocol.
//!
//! It drives nothing and reads one file: `/proc/cmdline`, only on a machine
//! with a GPU, for the boot option that chooses the one `nvrm` drives
//! ([`NVIDIA_OPTION`]). The kernel hands it, in DEVICES
//! messages on its bootstrap channel, a job, every device node twice and
//! every driver image as memory; `device_info` says what each device is; a
//! table here, and nowhere in the kernel, says which driver it takes.
//!
//! The exit status is the diagnosis: 0 never comes, since `devmgr` runs for
//! the life of the machine; every other number names the step that failed
//! (see [`Step`]).

#![no_std]
#![no_main]

mod arch;

use core::fmt;
use core::time::Duration;
use ferrix_rt::log::Level;

use ferrix_blkring::control::{Block, CONTROL_RIGHTS, DEVICE_RIGHTS, Message as Ring, Start};
use ferrix_blkring::identity::DiskName;
use ferrix_devmgr_proto::admission::{self, Admit, Admitted};
use ferrix_devmgr_proto::choice::{self, Chooser, Wanted};
use ferrix_devmgr_proto::gpu::{self, HandOver, SetRefused, Unanswered};
use ferrix_devmgr_proto::{
    ANSWER_BUSY, ANSWER_DONE, ANSWER_FAILED, ANSWER_NO_DEVICE, BUS_PCI, BUS_PLATFORM,
    DEVICES_MAX_BYTES, DevicesView, Message, NAME_BYTES, SHORT_BYTES,
};
use ferrix_drvupdate_proto::{self as update, Answer, Fingerprint, Outcome, REQUEST_BYTES};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::rights::{Requested, Rights};
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{
    CHANNEL_MAX_HANDLES, DEVICE_NOT_PCI, DEVICE_TREE_BLOCKS, DEVICE_VIRTIO_PCI, DeviceInfo,
    PROCESS_EXITED, PROCESS_KILLED, ProcessStatus, TREE_FIRMWARE_FB, TREE_GS201_DWC3,
    TREE_STM32_GPU, TREE_STM32_HDMI, TREE_STM32_SDMMC, TREE_STM32_USBH, TREE_X86_POWER,
};
use ferrix_netring::control::{
    CONTROL_RIGHTS as NET_CONTROL_RIGHTS, DEVICE_RIGHTS as NET_DEVICE_RIGHTS, MAX_MESSAGE,
    Message as NetRing, Start as NetStart,
};
use ferrix_restart::{Decision, Ended, Exit, Policy, Restart, Signal};
use ferrix_rt::native::channel::{self, Channel, ReadError};
use ferrix_rt::native::device::{Device, Limit};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::job::Job;
use ferrix_rt::native::pending::{self, Process};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};

ferrix_rt::entry!(main);

/// The most devices devmgr keeps, over every DEVICES message: ARMv7-A's
/// machine publishes 36.
const MAX_DEVICES: usize = 64;
/// The most drivers, as the protocol fixes it.
const MAX_DRIVERS: usize = ferrix_devmgr_proto::MAX_DRIVERS;

/// virtio's PCI vendor, and virtio-blk's and virtio-net's modern and
/// transitional device ids.
const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_BLK_IDS: [u16; 2] = [0x1042, 0x1001];
const VIRTIO_NET_IDS: [u16; 2] = [0x1041, 0x1000];
/// virtio-gpu has only a modern id.
const VIRTIO_GPU_IDS: [u16; 1] = [0x1050];
/// virtio-input, the same (`docs/DEVMGR.md`). QEMU's keyboard, mouse and
/// tablet are three functions of it, so one driver process starts per
/// device, as one blk driver starts per disk.
const VIRTIO_INPUT_IDS: [u16; 1] = [0x1052];

/// virtio-snd, which has only a modern id (`docs/AUDIO.md` §3.2): one
/// driver process per card.
const VIRTIO_SND_IDS: [u16; 1] = [0x1059];

/// virtio-console's modern PCI device id and its transitional one, which is
/// the pair `docs/CLIPBOARD.md` §3.1 names.
const VIRTIO_CONSOLE_IDS: [u16; 2] = [0x1043, 0x1003];

/// NVIDIA's PCI vendor: a GPU of its, of class 03 (a display controller),
/// is driven by `nvrm`, NVIDIA's own resource manager hosted in a ring-3
/// process (`docs/NVIDIA.md` §4.1).
const NVIDIA_VENDOR: u16 = 0x10DE;
/// The PCI base class of a display controller, in bits 23:16 of
/// `DeviceInfo::class`.
const DISPLAY_CLASS: u8 = 0x03;

/// QEMU's `pci-testdev` (Red Hat's vendor 0x1B36, device 0x0005): the test
/// device `cargo xtask test-nvrm` hands `nvrm` in place of a GPU, since QEMU
/// emulates no NVIDIA function and lets no device's vendor be overridden.
/// Like the GPU it sits on a VT-d unit and has a 64-bit BAR of gigabytes.
/// It is matched only to the driver image `nvrm-test`, which only that
/// gate's image carries, so no other boot starts anything on it.
///
/// That image is the whole guard: every x86-64 machine xtask boots carries
/// `pci-testdev,membar=8G`, so an `nvrm-test` image put into any other
/// image -- a test boot's, `run`'s, a release's -- would have devmgr hand
/// the pci-testdev over to it (marked, budgeted and started) on that boot.
/// Only `cargo xtask test-nvrm` may add it (`tools/common/xtask/src/nvrm.rs`).
const TEST_GPU_VENDOR: u16 = 0x1B36;
const TEST_GPU_IDS: [u16; 1] = [0x0005];

/// In [`PCI_DRIVERS`]' vendor column: a function of any vendor, matched by
/// what it is alone. No function has this vendor: it is what a slot with
/// nothing in it reads as.
const ANY_VENDOR: u16 = 0xFFFF;

/// The class code of an NVM Express controller: mass storage (`01`),
/// non-volatile memory (`08`), NVMe I/O (`02`). Every NVMe drive of every
/// vendor has it, and is driven by `nvme` (`docs/NVME.md`), wherever on the
/// bus it sits: on a root port, or below a switch as a chipset's slot is.
const NVME_INTERFACE: PciMatch = PciMatch::Interface(0x01, 0x08, 0x02);

/// The class code of an AHCI controller: mass storage (`01`), SATA (`06`),
/// AHCI 1.0 (`01`). Every vendor's, a chipset's or QEMU q35's ICH9, is
/// driven by `ahci` over a block ring, its first drive served as one disk.
const AHCI_INTERFACE: PciMatch = PciMatch::Interface(0x01, 0x06, 0x01);

/// Motorcomm's YT6801 PCIe gigabit adapter, driven by `yt6801` over a net
/// ring as virtio-net is (`docs/roadmap/networking.md`). The kernel holds
/// its MSI-X off and offers its one MSI vector (`docs/DEVMGR.md` §3).
/// Intel's PCI vendor id.
const INTEL_VENDOR: u16 = 0x8086;
/// The Alder Lake-N GPUs (`INTEL_ADLN_IDS`, Linux's `pciids.h`): the N150
/// box's is 0x46D4. Driven by `intel` as an engine while it serves no
/// subsystem (`docs/INTEL-GPU.md` §7, M1 and M2): the kernel lends the
/// function only under `ferrix.intelgpu` without `ferrix.efifb`, and its
/// isolated-interrupts mark is not set until the box's VT-d remaps
/// (intel-gpu-split.md, Q2).
const ADLN_IDS: [u16; 5] = [0x46D0, 0x46D1, 0x46D2, 0x46D3, 0x46D4];
const MOTORCOMM_VENDOR: u16 = 0x1F0A;
const YT6801_IDS: [u16; 1] = [0x6801];

/// How a PCI function that is not virtio is matched.
#[derive(Clone, Copy)]
enum PciMatch {
    /// Any function of the vendor whose base class is this.
    Class(u8),
    /// Any function of the vendor whose whole class code is this: base
    /// class, subclass and programming interface.
    Interface(u8, u8, u8),
    /// The vendor's functions with these device ids.
    Devices(&'static [u16]),
}

/// The PCI functions that are not virtio transports: which driver, by name
/// in the initramfs, drives which, and as which kind.
///
/// An xHCI USB host controller, class 0C:03:30 whoever made it, is a bus
/// host driven by `usbhid`, as the STM32MP15's EHCI is (`docs/XHCI.md`).
const PCI_DRIVERS: [(u16, PciMatch, &[u8], Kind); 7] = [
    (ANY_VENDOR, NVME_INTERFACE, b"nvme", Kind::Block),
    (ANY_VENDOR, AHCI_INTERFACE, b"ahci", Kind::Block),
    (
        MOTORCOMM_VENDOR,
        PciMatch::Devices(&YT6801_IDS),
        b"yt6801",
        Kind::Net,
    ),
    (
        NVIDIA_VENDOR,
        PciMatch::Class(DISPLAY_CLASS),
        b"nvrm",
        Kind::Gpu,
    ),
    (
        TEST_GPU_VENDOR,
        PciMatch::Devices(&TEST_GPU_IDS),
        b"nvrm-test",
        Kind::Gpu,
    ),
    (
        INTEL_VENDOR,
        PciMatch::Devices(&ADLN_IDS),
        b"intel",
        Kind::Engine,
    ),
    (
        ANY_VENDOR,
        PciMatch::Interface(0x0C, 0x03, 0x30),
        b"usbhid",
        Kind::Host,
    ),
];

/// Which kind of ring a driver serves its device over. The two rings are
/// separate protocols with separate kernel ends, and the only thing devmgr
/// does differently between them is which one it asks the kernel to make and
/// which START it writes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `docs/BLOCK-RING.md`: a disk, named `vda` and upwards.
    Block,
    /// `docs/NET-RING.md`: a network interface, named by the kernel.
    Net,
    /// `docs/DISPLAY.md`: a card, numbered by the kernel.
    Display,
    /// `docs/INPUT.md`: an input device, numbered by the kernel.
    Input,
    /// `docs/AUDIO.md`: a sound card, numbered by the kernel.
    Sound,
    /// `docs/CLIPBOARD.md` §5 and §6: a virtio-serial port, which has no
    /// kernel subsystem at all. The driver is handed its device and a plain
    /// channel to hear START on, and everything after that is a Unix socket
    /// it binds itself.
    Port,
    /// `docs/INPUT.md` §7: a bus host, whose devices are found only as its
    /// driver enumerates the bus -- perhaps none, perhaps long after boot.
    /// Handed its device and START as a port's driver is, it asks the input
    /// core for a channel of its own per keyboard or mouse, so there is no
    /// PUBLISHED for devmgr to wait for.
    Host,
    /// `docs/GPU.md` §6.3: a GPU's rendering engine that no kernel
    /// subsystem serves yet. Handed its device and START as a port's driver
    /// is; it publishes nothing, so it is started and taken at its word.
    Engine,
    /// `docs/vendor/google/pixel7/USB-HANDOVER.md`: a USB device controller, which
    /// presents a function to whatever it is plugged into -- the Pixel 7's
    /// serial port. Handed its device and START as a port's driver is; what
    /// it serves comes from the kernel's log through its own device's
    /// capability, so it publishes nothing and is taken at its word.
    Gadget,
    /// `docs/NVIDIA.md` §4.1: a GPU running firmware of its own, driven by
    /// `nvrm`, a ferrousli program. Handed only once its isolated-interrupts
    /// mark is set, its interrupts are isolated and its pin budget is set
    /// ([`hand_over_gpu`]); then its device and START as a port's driver is.
    /// It publishes nothing yet (the forwarding core is NVIDIA's N1e), so it
    /// is taken at its word, and it is not started again when it dies until
    /// `nvrm` can tear down a GSP it did not boot (N2).
    Gpu,
    /// `docs/POWERCTL.md`: the x86-64 power node, driven by `power`. Handed
    /// its device and START as a port's driver is; it claims the power core
    /// with `power_control_create` and publishes nothing, so it is taken at
    /// its word. Never started again when it dies: the kernel has put
    /// firmware's limit back, the safe state, and a driver that dies on
    /// every start is not retried into the register.
    Power,
}

/// A driver about to be started: its kind, carrying what only that kind
/// needs. The disk name is in here rather than beside it because a function
/// that took both would take one argument too many, and because a net driver
/// with a disk name would be a thing the table could say and the protocol
/// could not carry.
#[derive(Clone, Copy)]
enum Plan {
    /// A disk, to be `vda` or whichever letter is next.
    Block(DiskName),
    /// A network interface, whose name the kernel chooses.
    Net,
    /// A card, whose number the kernel chooses.
    Display,
    /// An input device, whose number the kernel chooses.
    Input,
    /// A sound card, whose number the kernel chooses.
    Sound,
    /// A virtio-serial port, which publishes nowhere.
    Port,
    /// A bus host, which publishes nothing itself.
    Host,
    /// A rendering engine, which publishes nowhere yet, driven by the
    /// program named.
    Engine(&'static str),
    /// A USB device controller's function.
    Gadget,
    /// A GPU, driven by the program named.
    Gpu(&'static str),
    /// The x86-64 power node.
    Power,
}

/// The table: which driver, by name in the initramfs, drives which device,
/// and over which ring.
const DRIVERS: [(u16, &[u16], &[u8], Kind); 6] = [
    (VIRTIO_VENDOR, &VIRTIO_BLK_IDS, b"blk", Kind::Block),
    (VIRTIO_VENDOR, &VIRTIO_NET_IDS, b"net", Kind::Net),
    (VIRTIO_VENDOR, &VIRTIO_GPU_IDS, b"gpu", Kind::Display),
    (VIRTIO_VENDOR, &VIRTIO_INPUT_IDS, b"input", Kind::Input),
    (VIRTIO_VENDOR, &VIRTIO_CONSOLE_IDS, b"vport", Kind::Port),
    (VIRTIO_VENDOR, &VIRTIO_SND_IDS, b"snd", Kind::Sound),
];

/// The device tree bindings the kernel publishes nodes for, by the number
/// `device_info` gives each (`DEVICE_TREE_BLOCKS`): an STM32MP15 DK board's
/// HDMI output is a card, driven by `ltdc` (`docs/DISPLAY.md` §6).
///
/// Its USB host is a bus host, driven by `usbhid` (`docs/INPUT.md` §7).
///
/// Its GPU, a Vivante GC400T, is an engine, driven by `gc400`
/// (`docs/GPU.md` §6.3).
///
/// The Pixel 7's USB device controller is a gadget, driven by `usbdev`
/// (`docs/vendor/google/pixel7/USB-HANDOVER.md`).
///
/// An STM32MP15 board's SDMMC1, the SD card, is a disk, driven by `sdmmc`
/// (`docs/CHROME.md` §10). Its HELLO and PUBLISHED name it by the location
/// word `DEVICE_NOT_PCI`, as the HDMI card's do: [`await_published`] keys on
/// that word alone, which is sound only because drivers are started one at
/// a time, so no other tree device's PUBLISHED can be in flight meanwhile.
///
/// The firmware's framebuffer, which the kernel publishes as a node only
/// under `ferrix.efifb`, is a card, driven by `efifb` (`docs/DISPLAY.md` §8).
///
/// The x86-64 power node, which the kernel publishes only under
/// `ferrix.powerctl` on a processor its table names, is driven by `power`
/// (`docs/POWERCTL.md`).
const TREE_DRIVERS: [(u16, &[u8], Kind); 7] = [
    (TREE_STM32_HDMI, b"ltdc", Kind::Display),
    (TREE_STM32_USBH, b"usbhid", Kind::Host),
    (TREE_STM32_GPU, b"gc400", Kind::Engine),
    (TREE_GS201_DWC3, b"usbdev", Kind::Gadget),
    (TREE_STM32_SDMMC, b"sdmmc", Kind::Block),
    (TREE_FIRMWARE_FB, b"efifb", Kind::Display),
    (TREE_X86_POWER, b"power", Kind::Power),
];

/// Where devmgr gave up, as the exit status.
#[repr(i32)]
enum Step {
    /// Started with no bootstrap channel.
    NoBootstrap = 1,
    /// A DEVICES message could not be read.
    ReadDevices = 2,
    /// DEVICES did not decode, its handles did not match its counts, it
    /// named more than devmgr keeps, or no message carried the job.
    Devices = 3,
    /// No port for the drivers' ends.
    Port = 4,
    /// REPORT could not be sent.
    Report = 5,
    /// The port wait failed, which only a killed devmgr sees.
    Wait = 6,
}

/// How many times one device's driver is started again after it dies.
///
/// A native program has no clock, so the budget is a count rather than a
/// rate: a driver that dies on every start stops being started after this
/// many, and its device stays quiesced as a device with no restart does.
const MAX_RESTARTS: u32 = 8;

/// A device's restart policy, the service manager's own (`src/lib/init/restart`,
/// `docs/INIT.md` §5.4): `Restart=always` for a kind that is
/// [`restarted`] and `Restart=no` for the rest, no delay, and a start limit of
/// [`MAX_RESTARTS`] which, with no clock to renew it, is a count.
fn policy_for(kind: Kind, location: u32) -> Policy {
    let restart = if restarted(kind, location) {
        Restart::Always
    } else {
        Restart::No
    };
    Policy::new(restart, Duration::ZERO, MAX_RESTARTS, Duration::MAX)
}

/// How a dead driver ended, as `process_status` (K6) says. A status that
/// cannot be read counts as a `SIGKILL`, which is what devmgr said of every
/// death before it could ask.
fn exit_of(process: &Process<Kernel>) -> Exit {
    let killed = |signal| Exit::Signal {
        signal: Signal(signal),
        core: false,
    };
    match process.status() {
        Ok(ProcessStatus {
            state: PROCESS_EXITED,
            value,
        }) => Exit::Code(i32::try_from(value).unwrap_or(i32::MAX)),
        Ok(ProcessStatus {
            state: PROCESS_KILLED,
            value,
        }) => killed(u8::try_from(value).unwrap_or(u8::MAX)),
        _ => killed(9),
    }
}

/// DIED's status, as a shell reads one: the exit code, or 128 and the signal.
fn died_status(exit: Exit) -> i32 {
    match exit {
        Exit::Code(code) => code,
        Exit::Signal { signal, .. } => 128 + i32::from(signal.0),
    }
}

/// Whether a driver of `kind` is started again when it dies
/// (`docs/DEVMGR.md` §4).
///
/// A display, sound, network, input or disk driver. Each of their cores
/// waits for a dead driver's claim to go (`src/kernel/src/claim.rs`), gives the
/// device back as it was -- the card or event node under the lowest free
/// number, the network interface parked with its addresses, the disk parked
/// with its requests queued -- and gives a dead driver's quarantined pins
/// back only once the next one has reset the device and sent HELLO
/// (`src/kernel/src/object/pin.rs`). The serial port has no core to wait for,
/// and a board's USB host, the GPU engine and the gadget are not restarted yet.
///
/// A bus host on PCI -- an xHCI controller -- is started again too
/// (`docs/XHCI.md` §5): its driver resets the controller as it starts, the
/// input core waits for every one of the dead driver's claims and takes its
/// keyboards and mice back as the new driver finds them. A board's host is
/// not, until that has been tried on the board.
///
/// So is a GPU engine on PCI -- the Intel integrated GPU's `intel`
/// (`docs/INTEL-GPU.md` §10, a driver started again): the chardev and
/// display cores wait out the dead driver's claims as any core does, the
/// new driver resets every engine its predecessor left pointed at a status
/// page before it brings the GT up and sends HELLO, and only then is the
/// dead driver's quarantine given back. Its clients' files of the old
/// render node answer `ENODEV` for good, which Mesa reports as
/// `VK_ERROR_DEVICE_LOST` at the next submit, and the new `renderD128` and
/// `card0` are published afresh. The DK1's GC400 is not, until that has
/// been tried on the board.
const fn restarted(kind: Kind, location: u32) -> bool {
    matches!(
        kind,
        Kind::Display | Kind::Sound | Kind::Net | Kind::Input | Kind::Block
    ) || (matches!(kind, Kind::Host | Kind::Engine) && location != DEVICE_NOT_PCI)
}

/// A driver devmgr started, and what it keeps of it.
struct Started {
    /// The device's place among the DEVICES the kernel sent: what BOUND,
    /// UNBOUND, BIND and UNBIND name it by.
    index: u32,
    /// Whether it was started only under `ferrix.unisolated`, for the count
    /// [`report`] says.
    unisolated: bool,
    /// The driver's place among DEVICES' names.
    driver: u16,
    /// The device's PCI address word, as START and HELLO carry it.
    location: u32,
    /// What the device is, as `device_info` said, and which driver it takes:
    /// what starting it again needs.
    info: DeviceInfo,
    kind: Kind,
    /// How it was started, disk name and all, so a bind starts it the same
    /// way again.
    plan: Plan,
    /// Whether the last quiesce succeeded: a device that is still on is
    /// never handed to a driver again.
    quiesced: bool,
    /// A write to `unbind` asked for this driver to go, and the token its
    /// DONE carries once the death is seen and the device quiesced.
    unbinding: Option<u16>,
    /// Whether its driver is started again when it dies, and how many times
    /// it has been.
    policy: Policy,
    /// The device, for the quiesce when the driver dies.
    device: Device<Kernel>,
    /// The driver's job, killed if it never publishes.
    job: Job<Kernel>,
    /// The driver.
    process: Process<Kernel>,
    /// Whether this driver is up: that the kernel has said it published to
    /// its subsystem, or -- for [`Kind::Port`], which has none -- simply that
    /// it started, since there is no PUBLISHED it could ever send.
    published: bool,
    /// Whether it has ended.
    dead: bool,
    /// How many times a driver has been started on it: the high half of
    /// the port key its death is watched under ([`key_of`]), so the death of
    /// a driver an update stopped, which arrives after the next one started,
    /// is told apart and ignored.
    generation: u32,
    /// The image an update put on it (`docs/DEVMGR.md` §4.1): `devmgr`'s
    /// own copy, which every later start uses. `None` is the initramfs's.
    image: Option<Image>,
}

/// The port key a driver of the device in `slot` is watched under, started
/// for the `generation`th time.
fn key_of(slot: usize, generation: u32) -> u64 {
    (slot as u64 & 0xFFFF_FFFF) | (u64::from(generation) << 32)
}

/// The slot and generation a port key names.
fn slot_of(key: u64) -> (usize, u32) {
    ((key & 0xFFFF_FFFF) as usize, (key >> 32) as u32)
}

/// What the kernel handed over: the job, the devices twice, the images.
struct Given {
    /// The root job, from the first message.
    job: Option<Job<Kernel>>,
    /// Every device, twice: one to give a driver, one to keep.
    devices: [Option<(Device<Kernel>, Device<Kernel>)>; MAX_DEVICES],
    /// How many of `devices` are filled.
    count: usize,
    /// The driver images, from the first message.
    images: [Option<Vmo<Kernel>>; MAX_DRIVERS],
    /// Their names, NUL-padded.
    names: [[u8; NAME_BYTES]; MAX_DRIVERS],
    /// How many of `images` and `names` are filled.
    drivers: usize,
    /// Whether the boot carries `ferrix.unisolated`, from the first
    /// message's flags (`ferrix_devmgr_proto::admission`).
    unisolated_allowed: bool,
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(channel) = bootstrap else {
        return Step::NoBootstrap as i32;
    };
    match run(&channel) {
        Ok(()) => 0,
        Err(step) => step as i32,
    }
}

/// Whether the machine is a board: one with tree nodes. `usbhid` drives a
/// board's EHCI node and a PC's xHCI function, and a driver has one bus in
/// sysfs, the first it is announced on; on a board it stays a platform
/// driver.
fn is_board(devices: &[Option<(Device<Kernel>, Device<Kernel>)>]) -> bool {
    devices.iter().flatten().any(|(device, _)| {
        device
            .info()
            .is_ok_and(|info| info.virtio == DEVICE_TREE_BLOCKS)
    })
}

/// Everything, in `docs/DEVMGR.md`'s order: read DEVICES, start a driver per
/// match one at a time, REPORT, then serve deaths for ever.
fn run(channel: &Channel<Kernel>) -> Result<(), Step> {
    let given = receive(channel)?;
    let Some(job) = given.job else {
        return Err(Step::Devices);
    };
    let port = port::create(Kernel).map_err(|_| Step::Port)?;
    announce_drivers(channel, &given.names, given.drivers, &given.devices);
    let mut inbox = Inbox::default();
    let mut started: [Option<Started>; MAX_DEVICES] = [const { None }; MAX_DEVICES];
    let mut count = 0_u32;
    let mut failed = 0_u32;
    let mut disks = 0_u32;
    // Which GPU `nvrm` drives, chosen at the first one met: read from the
    // command line only on a machine that has one.
    let mut gpus: Option<Chooser> = None;
    let mut drv = Drivers::new(&job, &given.names, given.drivers, &given.images);
    drv.unisolated_allowed = given.unisolated_allowed;
    for (index, pair) in given.devices.into_iter().take(given.count).enumerate() {
        let Some((device, keep)) = pair else {
            continue;
        };
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        let Ok(info) = device.info() else {
            ferrix_rt::warn!("devmgr   device {index}: no info; not started");
            failed += 1;
            continue;
        };
        let Some((image, kind, driver)) =
            driver_for(&info, &given.names, given.drivers, &given.images)
        else {
            // A device nobody drives: both handles close here.
            continue;
        };
        let name = driver_name(&given.names, driver);
        let Some(plan) = plan_for(kind, &info, &mut disks) else {
            say_unstarted(Level::Notice, name, info.location, UNPLANNED);
            failed += 1;
            continue;
        };
        let Some(slot) = started.get_mut(count as usize) else {
            say_unstarted(Level::Warning, name, info.location, NO_SLOT);
            failed += 1;
            continue;
        };
        // `nvrm` drives one GPU, never two: one not chosen stays unstarted,
        // with its line, as a device nobody drives (not a failure).
        if matches!(plan, Plan::Gpu(_)) && !chosen_gpu(gpus.get_or_insert_with(gpu_chooser), &info)
        {
            continue;
        }
        // Every device handed to a driver passes the one admission rule,
        // isolated or under the opt-out (`docs/DMA-GATE.md`), and a GPU its
        // hand-over first; one that does not stays unstarted, with its line,
        // and counts as failed.
        let Some(admitted) = admit_first(&keep, &info, plan, &drv) else {
            failed += 1;
            continue;
        };
        let alone = admitted.unisolated();
        let key = u64::from(count);
        match start(&drv, admitted, device, image, &info, plan, &port, key) {
            Ok((job, process, bootstrap)) => {
                // A bus host says when the devices plugged in at boot are
                // published, and init -- which the kernel starts at REPORT --
                // should find them: a compositor reads /dev/input once.
                if kind == Kind::Host {
                    await_settled(&bootstrap);
                }
                drop(bootstrap);
                let published = settle(channel, &port, kind, info.location, key, &mut inbox);
                let idle = !published && stopped_idle(&process);
                say_started(name, &info, published, idle, &process);
                if idle {
                    // Nothing to serve: neither started nor failed, and the
                    // slot stays free, as for a device nobody drives.
                    continue;
                }
                if published {
                    bound(channel, index, driver);
                }
                *slot = Some(Started {
                    index,
                    unisolated: alone,
                    driver,
                    location: info.location,
                    info,
                    kind,
                    plan,
                    quiesced: false,
                    unbinding: None,
                    policy: policy_for(kind, info.location),
                    device: keep,
                    job,
                    process,
                    published,
                    dead: false,
                    generation: 0,
                    image: None,
                });
                count += 1;
            }
            Err(()) => {
                say_start_failed(name, info.location, &plan);
                failed += 1;
            }
        }
    }
    report(channel, &mut started, failed)?;
    serve(channel, &port, &mut started, &drv, &mut inbox)
}

/// Whether the driver of `kind` just started at `location` published.
///
/// One at a time: the kernel's PUBLISHED for this disk before the next
/// driver starts, so disks register in PCI order and two drivers never race
/// to be vda.
///
/// A port driver and an engine's publish to no subsystem, and a bus host's
/// devices publish when they are found, so waiting for any of them would
/// wait for ever and the kill after would count a working driver failed. It
/// is started and taken at its word.
fn settle(
    channel: &Channel<Kernel>,
    port: &Port<Kernel>,
    kind: Kind,
    location: u32,
    key: u64,
    inbox: &mut Inbox,
) -> bool {
    matches!(
        kind,
        Kind::Port | Kind::Host | Kind::Engine | Kind::Gadget | Kind::Gpu | Kind::Power
    ) || await_boot(channel, port, location, key, inbox)
}

/// How long a driver that ended before it published is given to be
/// recorded as exited: its bootstrap channel closes as it ends, a moment
/// before its status can be read.
const ENDED_PATIENCE_NANOS: u64 = 1_000_000_000;

/// Whether a driver that ended before it published stopped cleanly with
/// nothing to serve: exited with status 0, `ferrix_driver::Step::Idle` (a
/// controller with no disk behind it). One whose exit cannot be seen in
/// time, or with no clock to bound the wait, did not.
fn stopped_idle(process: &Process<Kernel>) -> bool {
    let Ok(now) = ferrix_rt::linux::monotonic_nanos() else {
        return false;
    };
    let deadline = Deadline::At(now.saturating_add(ENDED_PATIENCE_NANOS));
    process.wait_one(Signals::TERMINATED, deadline).is_ok()
        && matches!(exit_of(process), Exit::Code(0))
}

/// The console line for a driver started at boot, and its exit status when
/// it ended before it published and was not idle.
fn say_started(
    name: &str,
    info: &DeviceInfo,
    published: bool,
    idle: bool,
    process: &Process<Kernel>,
) {
    let level = if published || idle {
        Level::Info
    } else {
        Level::Err
    };
    say(
        level,
        format_args!(
            "devmgr   {name} {:#010x} {:04x}:{:04x}: {}",
            info.location,
            info.vendor_id,
            info.device_id,
            if published {
                "started"
            } else if idle {
                "nothing to serve; stopped"
            } else {
                "ended before it published"
            }
        ),
    );
    if !published && !idle {
        ferrix_rt::err!(
            "devmgr   {name} {:#010x}: exit status {}",
            info.location,
            died_status(exit_of(process))
        );
    }
}

/// Tell the kernel driver image `driver` is bound to device `index`.
fn bound(channel: &Channel<Kernel>, index: u32, driver: u16) {
    let _ = channel.write(
        &Message::Bound {
            device: index,
            driver: u32::from(driver),
        }
        .encode(),
    );
}

/// The console lines for a driver that could not be started, a GPU's own
/// line besides.
fn say_start_failed(name: &str, location: u32, plan: &Plan) {
    say_unstarted(Level::Err, name, location, "could not be started");
    if let Plan::Gpu(program) = plan {
        ferrix_rt::err!(
            "devmgr   gpu {} not started: {program} could not be started",
            gpu::Place(location)
        );
    }
}

/// Why a device's driver was not started: no plan names it.
const UNPLANNED: &str = "no plan for the device; not started";

/// Why a device's driver was not started: every slot is taken.
const NO_SLOT: &str = "no slot left; not started";

/// The console line for a device whose driver was not started, and `why`.
fn say_unstarted(level: Level, name: &str, location: u32, why: &str) {
    say(
        level,
        format_args!("devmgr   {name} {location:#010x}: {why}"),
    );
}

/// [`await_published`] for a driver started at boot, which has no deadline.
fn await_boot(
    channel: &Channel<Kernel>,
    port: &Port<Kernel>,
    location: u32,
    key: u64,
    inbox: &mut Inbox,
) -> bool {
    await_published(channel, port, location, key, Deadline::Never, inbox).published
}

/// The helper's name among the images: an image that carries it takes
/// updates (`docs/DEVMGR.md` §4.1).
const UPDATER: &[u8] = b"drvupdated";

/// `drvupdated`, running: `devmgr`'s end of its channel, and its job and
/// process, kept for as long as it runs.
struct Updater {
    channel: Channel<Kernel>,
    _job: Job<Kernel>,
    _process: Process<Kernel>,
}

/// The port key the helper's channel is watched under: below the kernel's,
/// above every driver's, whose low half is a slot under [`MAX_DEVICES`].
const KEY_UPDATE: u64 = u64::MAX - 1;

/// Start `drvupdated` if the initramfs carries it, in a job of its own, and
/// answer `devmgr`'s end of its channel, watched on `port`. An image
/// without it takes no updates, and says nothing.
fn spawn_updater(drivers: &Drivers<'_>, port: &Port<Kernel>) -> Option<Updater> {
    let index = image_index(drivers.names, drivers.count, UPDATER)?;
    let image = drivers
        .images
        .get(usize::from(index))
        .and_then(Option::as_ref)?;
    let started = (|| {
        let job = drivers.job.create_sealed_child().ok()?;
        let process = pending::create_process(&job, image, "drvupdated").ok()?;
        let (near, far) = channel::create(Kernel).ok()?;
        process.start(far.into_owned()).ok()?;
        near.wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_UPDATE)
            .ok()?;
        Some(Updater {
            channel: near,
            _job: job,
            _process: process,
        })
    })();
    if started.is_none() {
        ferrix_rt::warn!("devmgr   drvupdated could not be started: no driver updates");
    } else {
        ferrix_rt::info!("devmgr   drvupdated started: driver updates are taken");
    }
    started
}

/// The events helper's name among the images: an image that carries it
/// publishes drivers' deaths and restarts to programs (`docs/DESKTOP.md`
/// §4.2).
const EVENTS: &[u8] = b"devmgr-events";

/// `devmgr-events`, running: `devmgr`'s end of its channel, written to and
/// never read, and its job and process.
struct Events {
    channel: Channel<Kernel>,
    _job: Job<Kernel>,
    _process: Process<Kernel>,
}

impl Events {
    /// Tell the helper what happened to `entry`'s driver. A full channel or
    /// a helper that has gone loses the event: a death's handling never
    /// waits on it.
    fn tell(
        events: Option<&Self>,
        what: ferrix_devmgr_events_proto::What,
        entry: &Started,
        value: u32,
        drivers: &Drivers<'_>,
    ) {
        let Some(events) = events else {
            return;
        };
        let name = drivers
            .names
            .get(usize::from(entry.driver))
            .map(|name| {
                let end = name
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(name.len());
                name.get(..end).unwrap_or_default()
            })
            .unwrap_or_default();
        let event = ferrix_devmgr_events_proto::Event::new(what, entry.location, value, name);
        let _ = events.channel.write(&event.encode());
    }
}

/// Start `devmgr-events` if the initramfs carries it, in a job of its own.
/// An image without it publishes nothing, and says nothing.
fn spawn_events(drivers: &Drivers<'_>) -> Option<Events> {
    let index = image_index(drivers.names, drivers.count, EVENTS)?;
    let image = drivers
        .images
        .get(usize::from(index))
        .and_then(Option::as_ref)?;
    let started = (|| {
        let job = drivers.job.create_child().ok()?;
        let process = pending::create_process(&job, image, "devmgr-events").ok()?;
        let (near, far) = channel::create(Kernel).ok()?;
        process.start(far.into_owned()).ok()?;
        Some(Events {
            channel: near,
            _job: job,
            _process: process,
        })
    })();
    let level = if started.is_some() {
        Level::Info
    } else {
        Level::Warning
    };
    say(
        level,
        format_args!(
            "devmgr   devmgr-events {}",
            if started.is_some() {
                "started: drivers' deaths and restarts are published"
            } else {
                "could not be started: nothing is published"
            }
        ),
    );
    started
}

/// How a driver of `kind` is started on the device `info` describes, or
/// `None` when there are no more disk names.
///
/// Only a disk is named here, and only disks are counted, so a net driver
/// between two disks does not shift the second one's letter.
fn plan_for(kind: Kind, info: &DeviceInfo, disks: &mut u32) -> Option<Plan> {
    Some(match kind {
        Kind::Block => {
            let name = DiskName::for_index(*disks)?;
            *disks += 1;
            Plan::Block(name)
        }
        Kind::Net => Plan::Net,
        Kind::Display => Plan::Display,
        Kind::Input => Plan::Input,
        Kind::Sound => Plan::Sound,
        Kind::Port => Plan::Port,
        Kind::Host => Plan::Host,
        Kind::Engine => Plan::Engine(engine_program(info)),
        Kind::Gadget => Plan::Gadget,
        Kind::Gpu => Plan::Gpu(gpu_program(info)),
        Kind::Power => Plan::Power,
    })
}

/// REPORT: every driver that published counts as started, and every one
/// that did not is ended and counts as failed, beside the `failed` that
/// never started.
fn report(
    channel: &Channel<Kernel>,
    started: &mut [Option<Started>; MAX_DEVICES],
    mut failed: u32,
) -> Result<(), Step> {
    let unisolated = started
        .iter()
        .flatten()
        .filter(|entry| entry.unisolated)
        .count();
    if unisolated != 0 {
        ferrix_rt::warn!(
            "devmgr   {unisolated} devices started under ferrix.unisolated: their drivers are \
             trusted with all of memory, and this boot carries no claim"
        );
    }
    let mut published = 0_u32;
    for entry in started.iter_mut().flatten() {
        if entry.published {
            published += 1;
        } else {
            let _ = entry.job.kill();
            entry.dead = true;
            failed += 1;
        }
    }
    channel
        .write(
            &Message::Report {
                started: published,
                failed,
            }
            .encode(),
        )
        .map_err(|_| Step::Report)
}

/// Tell the kernel every driver the table has an image for, and the bus of
/// the devices it drives: sysfs lists them under `/sys/bus/*/drivers`.
fn announce_drivers(
    channel: &Channel<Kernel>,
    names: &[[u8; NAME_BYTES]; MAX_DRIVERS],
    count: usize,
    devices: &[Option<(Device<Kernel>, Device<Kernel>)>],
) {
    let board = is_board(devices);
    let pci = DRIVERS
        .iter()
        .map(|(_, _, name, _)| (*name, BUS_PCI))
        .chain(PCI_DRIVERS.iter().map(|(_, _, name, _)| (*name, BUS_PCI)));
    let platform = TREE_DRIVERS
        .iter()
        .map(|(_, name, _)| (*name, BUS_PLATFORM));
    // The kernel keeps the first bus a driver is announced on.
    let mut ordered = [None; DRIVERS.len() + PCI_DRIVERS.len() + TREE_DRIVERS.len()];
    let (first, second) = ordered.split_at_mut(if board {
        TREE_DRIVERS.len()
    } else {
        DRIVERS.len() + PCI_DRIVERS.len()
    });
    let (pci_slots, platform_slots) = if board {
        (second, first)
    } else {
        (first, second)
    };
    for (slot, row) in pci_slots.iter_mut().zip(pci) {
        *slot = Some(row);
    }
    for (slot, row) in platform_slots.iter_mut().zip(platform) {
        *slot = Some(row);
    }
    for (wanted, bus) in ordered.into_iter().flatten() {
        if let Some(driver) = image_index(names, count, wanted) {
            let _ = channel.write(
                &Message::Driver {
                    driver: u32::from(driver),
                    bus,
                }
                .encode(),
            );
        }
    }
}

/// The most requests held while devmgr waits for a driver to publish.
const INBOX: usize = 8;

/// BIND and UNBIND that arrived while devmgr was waiting for something else,
/// answered as soon as it is not.
#[derive(Default)]
struct Inbox {
    /// The requests, oldest first.
    held: [Option<Message>; INBOX],
}

impl Inbox {
    /// Keep `request` for later. With no room it is answered at once as
    /// failed, rather than dropped and left for the writer to time out on.
    fn keep(&mut self, channel: &Channel<Kernel>, request: Message) {
        if let Some(slot) = self.held.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(request);
        } else if let Message::Bind { token, .. } | Message::Unbind { token, .. } = request {
            done(channel, token, ANSWER_FAILED);
        }
    }

    /// The oldest request kept. `keep` fills the first free slot, so the
    /// held requests are always the front of the array, oldest first, and
    /// taking the first and rotating keeps them so.
    fn take(&mut self) -> Option<Message> {
        let request = self.held.first_mut().and_then(Option::take);
        self.held.rotate_left(1);
        request
    }
}

/// Answer the request `token` with `answer`.
fn done(channel: &Channel<Kernel>, token: u16, answer: u32) {
    let _ = channel.write(&Message::Done { token, answer }.encode());
}

/// What starting a driver again needs of what the kernel handed over.
struct Drivers<'a> {
    job: &'a Job<Kernel>,
    names: &'a [[u8; NAME_BYTES]; MAX_DRIVERS],
    count: usize,
    images: &'a [Option<Vmo<Kernel>>; MAX_DRIVERS],
    /// The Intel GPU's worker image, copied into a VMO of devmgr's own:
    /// the kernel hands images without `DUPLICATE`, and a keeper is given
    /// a read-only duplicate of this at every start.
    worker: Option<Vmo<Kernel>>,
    /// Whether the boot carries `ferrix.unisolated`.
    unisolated_allowed: bool,
}

impl<'a> Drivers<'a> {
    fn new(
        job: &'a Job<Kernel>,
        names: &'a [[u8; NAME_BYTES]; MAX_DRIVERS],
        count: usize,
        images: &'a [Option<Vmo<Kernel>>; MAX_DRIVERS],
    ) -> Self {
        let worker = image_index(names, count, WORKER_NAME.as_bytes())
            .and_then(|at| images.get(usize::from(at)))
            .and_then(Option::as_ref)
            .and_then(|given| copy_image(given, given.size().ok()?))
            .map(|image| image.vmo);
        Drivers {
            job,
            names,
            count,
            images,
            worker,
            unisolated_allowed: false,
        }
    }
}

/// Read every DEVICES message the kernel wrote: the first with the job and
/// the images, the rest with devices only, until none are still to come.
fn receive(channel: &Channel<Kernel>) -> Result<Given, Step> {
    let mut given = Given {
        job: None,
        devices: [const { None }; MAX_DEVICES],
        count: 0,
        images: [const { None }; MAX_DRIVERS],
        names: [[0; NAME_BYTES]; MAX_DRIVERS],
        unisolated_allowed: false,
        drivers: 0,
    };
    loop {
        let mut bytes = [0_u8; DEVICES_MAX_BYTES];
        let mut handles = [Handle::INVALID; CHANNEL_MAX_HANDLES];
        let _ = channel
            .wait_one(Signals::READABLE | Signals::PEER_CLOSED, Deadline::Never)
            .map_err(|_| Step::ReadDevices)?;
        let received = channel
            .read(&mut bytes, &mut handles)
            .map_err(|_| Step::ReadDevices)?;
        let view = DevicesView::decode(bytes.get(..received.bytes).unwrap_or(&[]))
            .map_err(|_| Step::Devices)?;
        let devices = view.devices as usize;
        let drivers = view.drivers as usize;
        if received.handles != view.handles()
            || given.count + devices > MAX_DEVICES
            || drivers > MAX_DRIVERS
        {
            return Err(Step::Devices);
        }
        let owned = |index: usize| {
            OwnedHandle::from_raw(
                Kernel,
                handles.get(index).copied().unwrap_or(Handle::INVALID),
            )
        };
        let base = usize::from(view.first);
        if view.first {
            given.unisolated_allowed = view.unisolated_allowed;
            given.job = Some(Job::from_owned(owned(0)));
            given.drivers = drivers;
            for j in 0..drivers {
                if let (Some(image), Some(name)) = (given.images.get_mut(j), given.names.get_mut(j))
                {
                    *image = Some(Vmo::from_owned(owned(base + 2 * devices + j)));
                    *name = view.name(j).unwrap_or([0; NAME_BYTES]);
                }
            }
        }
        for i in 0..devices {
            if let Some(slot) = given.devices.get_mut(given.count + i) {
                *slot = Some((
                    Device::from_owned(owned(base + 2 * i)),
                    Device::from_owned(owned(base + 2 * i + 1)),
                ));
            }
        }
        given.count += devices;
        if view.more == 0 {
            return Ok(given);
        }
    }
}

/// The port key the kernel's channel is watched under while a driver starts:
/// above every driver's, which is its index.
const KEY_KERNEL: u64 = u64::MAX;

/// Wait for the kernel's PUBLISHED for the device at `location`, or for its
/// driver, watched on `port` under `key`, to exit first: a driver refused
/// before READY (`docs/DISPLAY.md` §2.4) or failing before it serves never
/// publishes, and the boot goes on without that device (os-02's review). A
/// native program has no clock, so a driver that neither publishes nor exits
/// holds devmgr here until the kernel's patience for REPORT runs out. A
/// channel that closes or says something else answers `false`.
///
/// A BIND or UNBIND that arrives meanwhile is kept in `inbox`, to be
/// answered once this driver is settled.
///
/// An update's start waits until `deadline` and no longer
/// (`docs/DEVMGR.md` §4.1): every other start waits for ever.
fn await_published(
    channel: &Channel<Kernel>,
    port: &Port<Kernel>,
    location: u32,
    key: u64,
    deadline: Deadline,
    inbox: &mut Inbox,
) -> Awaited {
    // Other drivers' deaths arrive here too; they are queued again after, for
    // `serve_deaths`.
    let mut deaths = [None::<u64>; MAX_DEVICES];
    let mut held = 0_usize;
    let mut exited = false;
    let published = loop {
        let mut short = [0_u8; SHORT_BYTES];
        match channel.read(&mut short, &mut []) {
            Ok(got) => match Message::decode(short.get(..got.bytes).unwrap_or(&[])) {
                Ok(Message::Published { location: at }) if at == location => break true,
                Ok(Message::Published { .. }) => continue,
                Ok(request @ (Message::Bind { .. } | Message::Unbind { .. })) => {
                    inbox.keep(channel, request);
                    continue;
                }
                _ => break false,
            },
            // A driver that published and then died is still published: the
            // channel is read once more after its exit before giving up.
            Err(ReadError::Failed(Error::ShouldWait)) if exited => break false,
            Err(ReadError::Failed(Error::ShouldWait)) => {}
            Err(_) => break false,
        }
        if channel
            .wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_KERNEL)
            .is_err()
        {
            break false;
        }
        let Ok(packet) = port.wait(deadline) else {
            // The deadline passed, or the port failed: not published.
            break false;
        };
        if packet.key == key {
            exited = true;
            continue;
        }
        if packet.key != KEY_KERNEL
            && let Some(slot) = deaths.get_mut(held)
        {
            *slot = Some(packet.key);
            held += 1;
        }
    };
    for died in deaths.into_iter().flatten() {
        let _ = port.queue(died, [0, 0]);
    }
    // A driver that published and then died is dead all the same: its death
    // was taken here, so it is queued again for `serve_deaths`, or nothing
    // would ever quiesce the device or start the driver again.
    if published && exited {
        let _ = port.queue(key, [0, 0]);
    }
    Awaited { published, exited }
}

/// How a wait for a driver to publish ended.
#[derive(Clone, Copy)]
struct Awaited {
    /// The kernel said it published.
    published: bool,
    /// Its death was seen during the wait. Unless it also published, that
    /// death's packet was taken and will not come again; a driver that
    /// neither published nor died ran past the deadline and still runs.
    exited: bool,
}

/// For the life of the machine: deaths -- quiesce the device, tell the
/// kernel, start a driver of a kind that is [`restarted`] again -- and the
/// BIND and UNBIND a write to sysfs sends (`docs/SYSFS.md` §5).
fn serve(
    channel: &Channel<Kernel>,
    port: &Port<Kernel>,
    started: &mut [Option<Started>; MAX_DEVICES],
    drivers: &Drivers<'_>,
    inbox: &mut Inbox,
) -> Result<(), Step> {
    // The helper is started once every driver has reported, so an update
    // never meets a boot still starting drivers.
    let mut updates = spawn_updater(drivers, port);
    let events = spawn_events(drivers);
    let _ = channel.wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_KERNEL);
    loop {
        // What arrived while a driver was being waited for comes first.
        while let Some(request) = inbox.take() {
            answer(channel, port, started, drivers, inbox, request);
        }
        let packet = port.wait(Deadline::Never).map_err(|_| Step::Wait)?;
        let key = packet.key;
        if key == KEY_KERNEL {
            take_requests(channel, inbox);
            let _ = channel.wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_KERNEL);
            continue;
        }
        if key == KEY_UPDATE {
            take_updates(channel, port, started, drivers, inbox, &mut updates);
            continue;
        }
        let (slot, generation) = slot_of(key);
        let Some(entry) = started.get_mut(slot).and_then(Option::as_mut) else {
            continue;
        };
        // A driver an update stopped dies after the next one started: its
        // death is not this driver's.
        if entry.dead || entry.generation != generation {
            continue;
        }
        entry.dead = true;
        // A keeper's worker lives in its job: the device is quiesced, and
        // another driver started, only once that whole tree has ended (the
        // consultant's condition 1, ledger 3156).
        let keeper = worker_image(entry.plan, drivers).is_some();
        let tree_ended = !keeper || end_job_tree(&entry.job, entry.location);
        entry.quiesced = tree_ended && quiesce(&entry.device);
        let _ = channel.write(
            &Message::Unbound {
                device: entry.index,
            }
            .encode(),
        );
        // An unbind asked for this death: it is answered, and nothing is
        // started again.
        if let Some(token) = entry.unbinding.take() {
            let answer = if entry.quiesced {
                ANSWER_DONE
            } else {
                ANSWER_FAILED
            };
            done(channel, token, answer);
            continue;
        }
        let exit = exit_of(&entry.process);
        let _ = channel.write(
            &Message::Died {
                location: entry.location,
                status: died_status(exit),
            }
            .encode(),
        );
        Events::tell(
            events.as_ref(),
            ferrix_devmgr_events_proto::What::Died,
            entry,
            died_status(exit).cast_unsigned(),
            drivers,
        );
        // Only a device that was quiesced is handed on: until then the dead
        // driver's core may still hold it, and the new one would be refused.
        // The policy counts the restart it decides on.
        let restarts =
            |entry: &Started| u32::try_from(entry.policy.budget.used()).unwrap_or(u32::MAX);
        if entry.quiesced
            && entry.policy.decide(Ended::of(exit), None) == Decision::Now
            && launch_again(channel, port, slot, entry, drivers, inbox, Deadline::Never)
        {
            let _ = channel.write(
                &Message::Restarted {
                    location: entry.location,
                    restarts: restarts(entry),
                }
                .encode(),
            );
            Events::tell(
                events.as_ref(),
                ferrix_devmgr_events_proto::What::Restarted,
                entry,
                restarts(entry),
                drivers,
            );
        } else if entry.policy.restart != Restart::No {
            // A kind that is started again, and was not: its budget is
            // spent, or its device could not be quiesced, and it stays dark.
            Events::tell(
                events.as_ref(),
                ferrix_devmgr_events_proto::What::GaveUp,
                entry,
                restarts(entry),
                drivers,
            );
        }
    }
}

/// Everything the kernel has written, kept in `inbox` if it is a request. A
/// watch left armed by a wait for a driver fires in [`serve`] too, finding
/// nothing or what this one would have: reading is idempotent.
fn take_requests(channel: &Channel<Kernel>, inbox: &mut Inbox) {
    loop {
        let mut short = [0_u8; SHORT_BYTES];
        let Ok(got) = channel.read(&mut short, &mut []) else {
            return;
        };
        if let Ok(request @ (Message::Bind { .. } | Message::Unbind { .. })) =
            Message::decode(short.get(..got.bytes).unwrap_or(&[]))
        {
            inbox.keep(channel, request);
        }
    }
}

/// Quiesce `device`: a core that has not let go yet answers `TIMED_OUT` and
/// is asked again; a live driver's `BAD_STATE` cannot happen for a dead one.
/// Answers whether it is quiesced.
///
/// `BAD_STATE` for a dead driver's device is the kernel refusing the node:
/// the quiesce read the registers the kernel owns back, found one rewritten
/// -- by the driver through a configuration mirror in a BAR, or by the
/// device's firmware -- and turned its bus mastering off for good
/// (`docs/NVIDIA.md` §12.1, SAFETY-MANUAL AoU-22). Not quiesced, so no driver
/// is started on it again; the kernel's line names the register.
fn quiesce(device: &Device<Kernel>) -> bool {
    for _ in 0..8 {
        match device.quiesce() {
            Err(Error::TimedOut) => {}
            result => return result.is_ok(),
        }
    }
    false
}

/// Answer a BIND or UNBIND.
///
/// An UNBIND of a live driver kills its job and is answered when its death
/// comes round in [`serve`], once the device is quiesced, as Linux's
/// `unbind` returns once the driver's `remove` has. A BIND starts the driver
/// as it was started at boot and is answered once it has published. The
/// kernel checked the device and the driver exist; what only devmgr knows --
/// whether this driver drives this device, whether it is up -- is checked
/// here.
fn answer(
    channel: &Channel<Kernel>,
    port: &Port<Kernel>,
    started: &mut [Option<Started>; MAX_DEVICES],
    drivers: &Drivers<'_>,
    inbox: &mut Inbox,
    request: Message,
) {
    let (Message::Bind {
        device,
        driver,
        token,
    }
    | Message::Unbind {
        device,
        driver,
        token,
    }) = request
    else {
        return;
    };
    let found = started
        .iter_mut()
        .enumerate()
        .find(|(_, slot)| slot.as_ref().is_some_and(|entry| entry.index == device));
    let Some((key, Some(entry))) = found else {
        // devmgr never started a driver on it, so its table takes nothing
        // for it: nothing can be bound or unbound.
        done(channel, token, ANSWER_NO_DEVICE);
        return;
    };
    if entry.driver != driver {
        done(channel, token, ANSWER_NO_DEVICE);
        return;
    }
    match request {
        Message::Unbind { .. } => {
            if entry.dead || entry.unbinding.is_some() {
                done(channel, token, ANSWER_NO_DEVICE);
                return;
            }
            entry.unbinding = Some(token);
            // The death packet comes to `serve`, which answers.
            let _ = entry.job.kill();
        }
        _ => {
            if !entry.dead {
                done(channel, token, ANSWER_BUSY);
                return;
            }
            if !entry.quiesced {
                done(channel, token, ANSWER_FAILED);
                return;
            }
            let answered =
                if launch_again(channel, port, key, entry, drivers, inbox, Deadline::Never) {
                    ANSWER_DONE
                } else {
                    ANSWER_FAILED
                };
            done(channel, token, answered);
        }
    }
}

/// How long an update's new driver has to publish before the old image is
/// started again (`docs/DEVMGR.md` §4.1).
const UPDATE_PATIENCE_NANOS: u64 = 15_000_000_000;

/// How long the image's trial load may take to be gone again.
const TRIAL_PATIENCE_NANOS: u64 = 5_000_000_000;

/// Bytes copied at a time from the helper's VMO into `devmgr`'s own.
const COPY_CHUNK: usize = 1024;

/// An image an update put on a device: `devmgr`'s copy, and what its line
/// says of it.
struct Image {
    vmo: Vmo<Kernel>,
    length: u64,
    fingerprint: u64,
}

/// Every request the helper has written, each answered on its channel; then
/// the watch armed again. A helper that has gone is said and forgotten:
/// there are no updates after it.
fn take_updates(
    channel: &Channel<Kernel>,
    port: &Port<Kernel>,
    started: &mut [Option<Started>; MAX_DEVICES],
    drivers: &Drivers<'_>,
    inbox: &mut Inbox,
    updates: &mut Option<Updater>,
) {
    let gone = {
        let Some(updater) = updates.as_ref() else {
            return;
        };
        loop {
            let mut bytes = [0_u8; REQUEST_BYTES];
            let mut handles = [Handle::INVALID; 1];
            match updater.channel.read(&mut bytes, &mut handles) {
                Ok(got) => {
                    // Owned at once, so a handle that came with a bad
                    // request is closed with it.
                    let image = (got.handles == 1).then(|| {
                        Vmo::from_owned(OwnedHandle::from_raw(
                            Kernel,
                            handles.first().copied().unwrap_or(Handle::INVALID),
                        ))
                    });
                    let request = update::Request::decode(bytes.get(..got.bytes).unwrap_or(&[]));
                    let answer = match (request, image) {
                        (Ok(request), Some(image)) => {
                            update_driver(channel, port, started, drivers, inbox, &request, &image)
                        }
                        _ => Answer::of(Outcome::Malformed),
                    };
                    let _ = updater.channel.write(&answer.encode());
                }
                Err(ReadError::Failed(Error::ShouldWait)) => break false,
                Err(_) => break true,
            }
        }
    };
    if gone {
        ferrix_rt::warn!("devmgr   drvupdated has ended: no more driver updates");
        *updates = None;
    } else if let Some(updater) = updates.as_ref() {
        let _ =
            updater
                .channel
                .wait_async(port, Signals::READABLE | Signals::PEER_CLOSED, KEY_UPDATE);
    }
}

/// One update, as `docs/DEVMGR.md` §4.1 orders it: the request checked
/// against the table, the image copied into `devmgr`'s own memory and
/// fingerprinted, its trial load, then each device it names swapped, one at
/// a time, with the old image started again for one whose new driver does
/// not publish. Nothing the helper parsed is trusted: the request is read
/// again here, and the image is `devmgr`'s copy.
fn update_driver(
    channel: &Channel<Kernel>,
    port: &Port<Kernel>,
    started: &mut [Option<Started>; MAX_DEVICES],
    drivers: &Drivers<'_>,
    inbox: &mut Inbox,
    request: &update::Request,
    given: &Vmo<Kernel>,
) -> Answer {
    let name = request.name();
    let Ok(program) = core::str::from_utf8(name) else {
        return Answer::of(Outcome::Malformed);
    };
    // The helper is an image too, but no device's driver.
    let Some(driver) = image_index(drivers.names, drivers.count, name).filter(|_| name != UPDATER)
    else {
        return Answer::of(Outcome::NoDevice);
    };
    let named = |entry: &Started| {
        entry.driver == driver
            && (request.location == update::ANY || entry.location == request.location)
    };
    // Every device it names must be one that can be swapped before any is.
    let mut tried = 0_u32;
    for entry in started.iter().flatten().filter(|entry| named(entry)) {
        tried += 1;
        // A keeper and its worker are one driver (the consultant's
        // condition 5, ledger 3156): replacing one alone is refused until
        // the updater carries both.
        let keeper = worker_image(entry.plan, drivers).is_some();
        if keeper {
            ferrix_rt::warn!(
                "devmgr   an update of {program} alone is refused: it runs as a keeper over {WORKER_NAME}, and the two are replaced together or not at all"
            );
        }
        let refused = if keeper || !restarted(entry.kind, entry.location) {
            Outcome::NotRestarted
        } else if entry.dead || !entry.published || entry.unbinding.is_some() {
            Outcome::Busy
        } else {
            continue;
        };
        return Answer {
            outcome: refused,
            updated: 0,
            tried,
        };
    }
    if tried == 0 {
        return Answer::of(Outcome::NoDevice);
    }
    let Some(image) = copy_image(given, request.length) else {
        return Answer {
            outcome: Outcome::Malformed,
            updated: 0,
            tried,
        };
    };
    if !loads(drivers.job, &image.vmo, program) {
        ferrix_rt::warn!(
            "devmgr   update of {program} refused: the kernel's loader did not take {} bytes, fnv64 {:016x}, or its trial did not end",
            image.length,
            image.fingerprint
        );
        return Answer {
            outcome: Outcome::Refused,
            updated: 0,
            tried,
        };
    }
    let mut updated = 0_u32;
    for (slot, entry) in started.iter_mut().enumerate() {
        let Some(entry) = entry.as_mut().filter(|entry| named(entry)) else {
            continue;
        };
        let outcome = swap(channel, port, slot, entry, drivers, inbox, &image, program);
        if outcome != Outcome::Updated {
            return Answer {
                outcome,
                updated,
                tried,
            };
        }
        updated += 1;
    }
    Answer {
        outcome: Outcome::Updated,
        updated,
        tried,
    }
}

/// `length` bytes of `given` in a VMO of `devmgr`'s own, so nothing the
/// helper still holds can change them between the checks and
/// `process_create`, and their fingerprint. `None` when `given` is shorter,
/// `length` is past [`update::MAX_IMAGE`], or there is no memory.
fn copy_image(given: &Vmo<Kernel>, length: u64) -> Option<Image> {
    if length == 0 || length > update::MAX_IMAGE || given.size().ok()? < length {
        return None;
    }
    let vmo = vmo::create(Kernel, usize::try_from(length).ok()?).ok()?;
    let mut fingerprint = Fingerprint::new();
    let mut chunk = [0_u8; COPY_CHUNK];
    let mut offset = 0_u64;
    while offset < length {
        let take = usize::try_from(length - offset)
            .unwrap_or(COPY_CHUNK)
            .min(COPY_CHUNK);
        let bytes = chunk.get_mut(..take)?;
        given.read(bytes, offset).ok()?;
        vmo.write(bytes, offset).ok()?;
        fingerprint.update(bytes);
        offset += take as u64;
    }
    Some(Image {
        vmo,
        length,
        fingerprint: fingerprint.value(),
    })
}

/// Whether the kernel's loader takes `image` as `program`: a process made
/// from it in a scratch job under `job`, never started, holding no handle,
/// and gone again -- or the answer is no -- before anything is stopped (the certification
/// consultant's C7, 2026-10-07).
fn loads(job: &Job<Kernel>, image: &Vmo<Kernel>, program: &str) -> bool {
    let Ok(scratch) = job.create_sealed_child() else {
        return false;
    };
    let loaded = match pending::create_process(&scratch, image, program) {
        Ok(process) => {
            let _ = scratch.kill();
            let deadline = ferrix_rt::linux::monotonic_nanos().map_or(Deadline::Never, |now| {
                Deadline::At(now.saturating_add(TRIAL_PATIENCE_NANOS))
            });
            // A trial that is not gone in time refuses the update with
            // nothing stopped (the certification consultant's K2).
            process.wait_one(Signals::TERMINATED, deadline).is_ok()
        }
        Err(_) => false,
    };
    let _ = scratch.kill();
    loaded
}

/// Put `image` on the device `entry` drives: stop its driver, quiesce the
/// device, start the image and give it [`UPDATE_PATIENCE_NANOS`] to publish;
/// when it does not, start the image the device had again.
#[expect(
    clippy::too_many_arguments,
    reason = "the swap needs everything a restart needs, and the image"
)]
fn swap(
    channel: &Channel<Kernel>,
    port: &Port<Kernel>,
    slot: usize,
    entry: &mut Started,
    drivers: &Drivers<'_>,
    inbox: &mut Inbox,
    image: &Image,
    program: &str,
) -> Outcome {
    let place = gpu::Place(entry.location);
    // The clock and the device's own copy first: nothing is stopped for an
    // update that could not be timed or kept. A copy rather than a second
    // handle, so no handle in devmgr is passed on with the rights it holds
    // (P6, `budget_tests`), and each device keeps its image apart.
    let Ok(now) = ferrix_rt::linux::monotonic_nanos() else {
        return Outcome::Unanswered;
    };
    let Some(mine) = copy_image(&image.vmo, image.length) else {
        return Outcome::Unanswered;
    };
    let was = match entry.image.as_ref() {
        Some(old) => Was::Update(old.length, old.fingerprint),
        None => Was::Initramfs,
    };
    // The running driver goes as a death does, but no DIED: nothing died
    // that devmgr did not stop. Its death packet, still to come, carries
    // this generation, which the next start leaves behind.
    let _ = entry.job.kill();
    let _ = entry.process.wait_one(Signals::TERMINATED, Deadline::Never);
    entry.dead = true;
    entry.quiesced = quiesce(&entry.device);
    let _ = channel.write(
        &Message::Unbound {
            device: entry.index,
        }
        .encode(),
    );
    if !entry.quiesced {
        ferrix_rt::err!("devmgr   {program} {place} update failed: the device would not quiesce");
        return Outcome::Failed;
    }
    let previous = entry.image.replace(mine);
    let deadline = Deadline::At(now.saturating_add(UPDATE_PATIENCE_NANOS));
    if launch_again(channel, port, slot, entry, drivers, inbox, deadline) {
        ferrix_rt::notice!(
            "devmgr   {program} {place} updated: {} bytes, fnv64 {:016x} (was {was})",
            image.length,
            image.fingerprint
        );
        return Outcome::Updated;
    }
    // Back to the image it had, kept until now for this. A new driver that
    // published just after its deadline left a PUBLISHED for this location
    // on the kernel's channel; it was killed and the device quiesced since,
    // so no HELLO can come from it again, and what it left is read off here
    // -- requests kept, anything else dropped -- or the wait for the old
    // image would take it for that one's (the certification consultant's K4).
    take_requests(channel, inbox);
    entry.image = previous;
    if entry.quiesced && launch_again(channel, port, slot, entry, drivers, inbox, Deadline::Never) {
        ferrix_rt::warn!(
            "devmgr   {program} {place} rolled back: {} bytes, fnv64 {:016x} did not publish; {was} drives it again",
            image.length,
            image.fingerprint
        );
        return Outcome::RolledBack;
    }
    ferrix_rt::err!(
        "devmgr   {program} {place} update failed: neither image published; the device stays quiesced"
    );
    Outcome::Failed
}

/// The image a device had before an update, as its line says it.
#[derive(Clone, Copy)]
enum Was {
    /// The initramfs's.
    Initramfs,
    /// An earlier update's: its length and fingerprint.
    Update(u64, u64),
}

impl fmt::Display for Was {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Was::Initramfs => f.write_str("the initramfs image"),
            Was::Update(length, fingerprint) => {
                write!(f, "{length} bytes, fnv64 {fingerprint:016x}")
            }
        }
    }
}

/// Start `entry`'s driver again, in a job of its own, on a duplicate of the
/// device handle devmgr keeps, the way it was started at boot, and wait for
/// it to publish as at boot, or until `deadline` for an update's start; tell
/// the kernel it is bound. `false`, with the device left quiesced, when it
/// could not be started or did not publish.
///
/// The image is the one an update put on the device, or the initramfs's.
/// Each start is a new generation of the device's port key, so a death
/// still on its way from an earlier driver is not taken for this one's.
fn launch_again(
    channel: &Channel<Kernel>,
    port: &Port<Kernel>,
    slot: usize,
    entry: &mut Started,
    drivers: &Drivers<'_>,
    inbox: &mut Inbox,
    deadline: Deadline,
) -> bool {
    // The dead driver's job, and anything it left running in it, goes first,
    // and a keeper's is waited out whole.
    let _ = entry.job.kill();
    if worker_image(entry.plan, drivers).is_some() && !end_job_tree(&entry.job, entry.location) {
        return false;
    }
    let Some((initramfs, _, _)) =
        driver_for(&entry.info, drivers.names, drivers.count, drivers.images)
    else {
        return false;
    };
    let image = entry.image.as_ref().map_or(initramfs, |image| &image.vmo);
    // A GPU is handed over again, as at boot: the mark stays set, and the
    // budget is set again under no live pins, since the dead driver's are
    // quarantined, not live.
    if matches!(entry.plan, Plan::Gpu(_)) && !hand_over_gpu(&entry.device, &entry.info) {
        return false;
    }
    let Ok(device) = entry.device.duplicate(Requested::Exactly(DEVICE_RIGHTS)) else {
        return false;
    };
    let generation = entry.generation.wrapping_add(1);
    let key = key_of(slot, generation);
    let Some(admitted) = admit(
        &entry.device,
        &entry.info,
        plan_name(entry.plan),
        drivers.unisolated_allowed,
    ) else {
        return false;
    };
    let Ok((job, process, _bootstrap)) = start(
        drivers,
        admitted,
        device,
        image,
        &entry.info,
        entry.plan,
        port,
        key,
    ) else {
        return false;
    };
    entry.generation = generation;
    entry.job = job;
    entry.process = process;
    entry.dead = false;
    entry.quiesced = false;
    let awaited = if matches!(
        entry.kind,
        Kind::Port | Kind::Host | Kind::Engine | Kind::Gadget | Kind::Gpu | Kind::Power
    ) {
        Awaited {
            published: true,
            exited: false,
        }
    } else {
        await_published(channel, port, entry.location, key, deadline, inbox)
    };
    if awaited.published {
        entry.published = true;
        let _ = channel.write(
            &Message::Bound {
                device: entry.index,
                driver: u32::from(entry.driver),
            }
            .encode(),
        );
        return true;
    }
    // It died before publishing, and the wait above took its death; or it
    // ran past an update's deadline, and is ended here and waited for, since
    // a device is quiesced only once its driver's handles are closed.
    let _ = entry.job.kill();
    if !awaited.exited {
        let _ = entry.process.wait_one(Signals::TERMINATED, Deadline::Never);
    }
    entry.dead = true;
    entry.quiesced = quiesce(&entry.device);
    false
}

/// The image of the driver for `info`, by the table, if the initramfs
/// carries it, with the driver's kind and its place among the names.
fn driver_for<'a>(
    info: &DeviceInfo,
    names: &[[u8; NAME_BYTES]; MAX_DRIVERS],
    drivers: usize,
    images: &'a [Option<Vmo<Kernel>>; MAX_DRIVERS],
) -> Option<(&'a Vmo<Kernel>, Kind, u16)> {
    let (wanted, kind) = match info.virtio {
        DEVICE_VIRTIO_PCI => DRIVERS
            .iter()
            .find(|(vendor, ids, _, _)| *vendor == info.vendor_id && ids.contains(&info.device_id))
            .map(|(_, _, wanted, kind)| (wanted, kind))?,
        DEVICE_TREE_BLOCKS => TREE_DRIVERS
            .iter()
            .find(|(binding, _, _)| *binding == info.device_id)
            .map(|(_, wanted, kind)| (wanted, kind))?,
        _ if info.location != DEVICE_NOT_PCI => PCI_DRIVERS
            .iter()
            .find(|(vendor, matched, _, _)| {
                (*vendor == ANY_VENDOR || *vendor == info.vendor_id) && pci_matches(*matched, info)
            })
            .map(|(_, _, wanted, kind)| (wanted, kind))?,
        _ => return None,
    };
    let driver = image_index(names, drivers, wanted)?;
    let image = images.get(usize::from(driver)).and_then(Option::as_ref)?;
    Some((image, *kind, driver))
}

/// The place among the first `drivers` names of the one called `wanted`.
fn image_index(
    names: &[[u8; NAME_BYTES]; MAX_DRIVERS],
    drivers: usize,
    wanted: &[u8],
) -> Option<u16> {
    let at = (0..drivers).find(|&j| {
        names.get(j).is_some_and(|name| {
            let end = name
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(name.len());
            name.get(..end) == Some(wanted)
        })
    })?;
    u16::try_from(at).ok()
}

/// The name of image `driver`, for a line on the console.
fn driver_name(names: &[[u8; NAME_BYTES]; MAX_DRIVERS], driver: u16) -> &str {
    let name = names.get(usize::from(driver)).map_or(&[][..], |name| {
        let end = name
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(name.len());
        name.get(..end).unwrap_or(&[])
    });
    core::str::from_utf8(name).unwrap_or("?")
}

/// Start `image` on `device`: a ring of the kind the table names, a job, a
/// process, START over its bootstrap, a watch on its end, and go.
#[expect(
    clippy::too_many_arguments,
    reason = "one driver's start, every part of it named, its admission among them"
)]
fn start(
    drivers: &Drivers<'_>,
    admitted: Admitted,
    device: Device<Kernel>,
    image: &Vmo<Kernel>,
    info: &DeviceInfo,
    plan: Plan,
    port: &Port<Kernel>,
    key: u64,
) -> Result<Launched, ()> {
    // Taken by value, so no start is written without an admission, and it
    // must be this device's: one admission starts one device (ledger 4153
    // (1)). What it admitted is said where it was decided (`admit`).
    if !admitted.starts(info.location) {
        return Err(());
    }
    let job = drivers.job;
    if let (Plan::Engine(program), Some(worker)) = (plan, worker_image(plan, drivers)) {
        return start_keeper(job, device, image, info, port, key, program, worker);
    }
    match plan {
        Plan::Block(name) => start_block(job, device, image, info, name, port, key),
        Plan::Net => start_net(job, device, image, info, port, key),
        Plan::Display => start_display(job, device, image, info, port, key),
        Plan::Input => start_input(job, device, image, info, port, key),
        Plan::Sound => start_sound(job, device, image, info, port, key),
        Plan::Port => start_plain(job, device, image, info, port, key, "vport"),
        Plan::Host => start_plain(job, device, image, info, port, key, "usbhid"),
        Plan::Engine(program) => start_plain(job, device, image, info, port, key, program),
        Plan::Gadget => start_plain(job, device, image, info, port, key, "usbdev"),
        Plan::Gpu(program) => start_plain(job, device, image, info, port, key, program),
        Plan::Power => start_plain(job, device, image, info, port, key, "power"),
    }
}

/// Whether `keep`'s device may be handed to its driver: the admission
/// rule on what `device_isolation` answers (`ferrix_devmgr_proto::
/// admission`), translation alone on AArch64 (the customer's G4), the
/// opt-out when the boot carries `ferrix.unisolated`. A device refused, or
/// admitted only by the opt-out, is said on its line.
fn admit(
    keep: &Device<Kernel>,
    info: &DeviceInfo,
    what: &str,
    unisolated_allowed: bool,
) -> Option<Admitted> {
    let bits = keep.isolation().ok();
    let decided = admission::admit(bits, arch::AARCH64, unisolated_allowed, info.location);
    if let Some(line) = decided.line(what, info.location) {
        let level = if matches!(decided, Admit::Start(_)) {
            Level::Warning
        } else {
            Level::Notice
        };
        say(level, format_args!("{line}"));
    }
    match decided {
        Admit::Start(admitted) => Some(admitted),
        Admit::Refused { .. } | Admit::Unread => None,
    }
}

/// [`admit`] for `keep`'s device under `plan`, a GPU's hand-over first: a
/// GPU is handed over only once marked, isolated and budgeted, through the
/// handle devmgr keeps, which alone has `SET_LIMIT`.
fn admit_first(
    keep: &Device<Kernel>,
    info: &DeviceInfo,
    plan: Plan,
    drivers: &Drivers<'_>,
) -> Option<Admitted> {
    if matches!(plan, Plan::Gpu(_)) && !hand_over_gpu(keep, info) {
        return None;
    }
    admit(keep, info, plan_name(plan), drivers.unisolated_allowed)
}

/// A plan's driver name, for its admission's line.
const fn plan_name(plan: Plan) -> &'static str {
    match plan {
        Plan::Block(_) => "disk",
        Plan::Net => "net",
        Plan::Display => "card",
        Plan::Input => "input",
        Plan::Sound => "sound",
        Plan::Port => "port",
        Plan::Host => "host",
        Plan::Engine(program) => program,
        Plan::Gpu(_) => "gpu",
        Plan::Gadget => "gadget",
        Plan::Power => "power",
    }
}

/// Whether a PCI function that is not virtio is the one `matched` names.
fn pci_matches(matched: PciMatch, info: &DeviceInfo) -> bool {
    match matched {
        PciMatch::Class(base) => (info.class >> 16) as u8 == base,
        PciMatch::Interface(base, sub, interface) => {
            info.class & 0x00FF_FFFF
                == (u32::from(base) << 16) | (u32::from(sub) << 8) | u32::from(interface)
        }
        PciMatch::Devices(ids) => ids.contains(&info.device_id),
    }
}

/// The program an engine's table entry named: `intel` for Intel's GPUs,
/// `gc400` for the STM32MP157's.
fn engine_program(info: &DeviceInfo) -> &'static str {
    if info.vendor_id == INTEL_VENDOR {
        "intel"
    } else {
        "gc400"
    }
}

/// The worker image a keeper is handed at START: `intel-gt` for the Intel
/// GPU's driver, when the initramfs carries it (`docs/INTEL-GPU.md` §12.3).
/// Without it `intel` is started as one process, as before.
fn worker_image<'a>(plan: Plan, drivers: &'a Drivers<'_>) -> Option<&'a Vmo<Kernel>> {
    if !matches!(plan, Plan::Engine("intel")) {
        return None;
    }
    drivers.worker.as_ref()
}

/// The Intel GPU's worker, in the initramfs.
const WORKER_NAME: &str = "intel-gt";

/// How long a keeper's job tree may take to end after its kill.
const TREE_NANOS: u64 = 5_000_000_000;

/// Kill `job` and wait, up to [`TREE_NANOS`], until no process of it or of
/// any job beneath it is left. `false`, said, when one is: the device is
/// then not quiesced and nothing is started on it again.
fn end_job_tree(job: &Job<Kernel>, location: u32) -> bool {
    let _ = job.kill();
    let now = ferrix_rt::linux::monotonic_nanos().unwrap_or(0);
    let ended = job
        .wait_one(Signals::EMPTY, Deadline::At(now.saturating_add(TREE_NANOS)))
        .is_ok();
    if ended {
        ferrix_rt::info!(
            "devmgr   the driver of {location:#010x} and every process in its job have ended"
        );
    } else {
        ferrix_rt::err!(
            "devmgr   the driver of {location:#010x} left a process in its job {} s after the kill; the device is not handed on",
            TREE_NANOS / 1_000_000_000
        );
    }
    ended
}

/// The rights a keeper holds on its own job: start a worker in a child job
/// and end it, wait on it, and hand it on; never set its limits.
const KEEPER_JOB_RIGHTS: Rights = Rights(Rights::TRANSFER.0 | Rights::WAIT.0 | Rights::MANAGE.0);

/// The rights a keeper holds on its worker's image: enough to start it.
const WORKER_IMAGE_RIGHTS: Rights = Rights(Rights::READ.0 | Rights::TRANSFER.0);

/// Start the Intel GPU's keeper: START as an engine's, with two more
/// handles after the device, its own job and the worker's image
/// (`docs/DEVMGR.md` §3).
#[expect(
    clippy::too_many_arguments,
    reason = "start_plain's, and the worker's image"
)]
fn start_keeper(
    job: &Job<Kernel>,
    device: Device<Kernel>,
    image: &Vmo<Kernel>,
    info: &DeviceInfo,
    port: &Port<Kernel>,
    key: u64,
    program: &str,
    worker: &Vmo<Kernel>,
) -> Result<Launched, ()> {
    let child_job = job.create_sealed_child().map_err(|_| ())?;
    let own = child_job
        .duplicate(Requested::Exactly(KEEPER_JOB_RIGHTS))
        .map_err(|_| ())?;
    let worker = worker
        .duplicate(Requested::Exactly(WORKER_IMAGE_RIGHTS))
        .map_err(|_| ())?;
    let device = device
        .into_owned()
        .replace(Requested::Exactly(DEVICE_RIGHTS))
        .map_err(|_| ())?;
    let encoded = Ring::Start(plain_start(info, program)).encode();
    launch_in(
        child_job,
        image,
        program,
        port,
        key,
        encoded.as_bytes(),
        [device, own.into_owned(), worker.into_owned()],
    )
}

/// The program a GPU's table entry named: `nvrm` for NVIDIA's, and
/// `nvrm-test` for the test device.
fn gpu_program(info: &DeviceInfo) -> &'static str {
    if info.vendor_id == NVIDIA_VENDOR {
        "nvrm"
    } else {
        "nvrm-test"
    }
}

/// The calls [`gpu::hand_over`] makes, on the device handle devmgr keeps,
/// which has `SET_LIMIT`.
struct GpuNode<'a>(&'a Device<Kernel>);

impl gpu::Node for GpuNode<'_> {
    fn mark(&self) -> Result<(), Unanswered> {
        self.0
            .set_limit(Limit::IsolatedInterrupts, 1)
            .map_err(|_| Unanswered)
    }

    fn isolation(&self) -> Result<u64, Unanswered> {
        self.0.isolation().map_err(|_| Unanswered)
    }

    fn ceiling(&self) -> Result<u64, Unanswered> {
        self.0
            .limit(Limit::PinCeiling)
            .map(|pages| pages as u64)
            .map_err(|_| Unanswered)
    }

    fn room(&self) -> Result<u64, Unanswered> {
        self.0
            .limit(Limit::PinRoom)
            .map(|pages| pages as u64)
            .map_err(|_| Unanswered)
    }

    fn set_budget(&self, pages: u64) -> Result<(), SetRefused> {
        let pages = usize::try_from(pages).map_err(|_| SetRefused::PastCeiling)?;
        self.0
            .set_limit(Limit::PinPages, pages)
            .map_err(|error| match error {
                Error::NoMemory => SetRefused::PastCeiling,
                _ => SetRefused::Other,
            })
    }
}

/// The boot option that names the GPU `nvrm` drives, as `ssss:bb:dd.f` or
/// `bb:dd.f` (`ferrix_devmgr_proto::choice`); without it, the first GPU
/// met. Read by this program from `/proc/cmdline`, never by the kernel.
const NVIDIA_OPTION: &str = "ferrix.nvidia";
/// [`NVIDIA_OPTION`] as it starts its word.
const NVIDIA_WORD: &[u8] = b"ferrix.nvidia=";

/// The chooser of the one GPU, from the command line. Unreadable, it is
/// said, and the first GPU met is the one.
fn gpu_chooser() -> Chooser {
    let mut line = [0_u8; 2048];
    let wanted = match read_cmdline(&mut line) {
        Some(len) => choice::wanted(line.get(..len).unwrap_or_default(), NVIDIA_WORD),
        None => {
            say(
                Level::Warning,
                format_args!(
                    "devmgr   /proc/cmdline unreadable: no {NVIDIA_OPTION}, the first GPU met is nvrm's"
                ),
            );
            Wanted::First
        }
    };
    Chooser::new(wanted, NVIDIA_OPTION)
}

/// Whether the GPU `info` names is the one `nvrm` drives, said in its line.
fn chosen_gpu(chooser: &mut Chooser, info: &DeviceInfo) -> bool {
    let pick = chooser.pick(info.location);
    let level = if pick.takes() {
        Level::Info
    } else {
        Level::Notice
    };
    say(level, format_args!("{}", chooser.line(pick, info.location)));
    pick.takes()
}

/// `/proc/cmdline` into `line`: its length, or `None`.
fn read_cmdline(line: &mut [u8]) -> Option<usize> {
    use ferrix_rt::linux::{self, numbers};
    const AT_FDCWD: usize = (-100_isize) as usize;
    const O_RDONLY: usize = 0;
    let path = b"/proc/cmdline\0";
    // SAFETY: `path` is NUL-terminated and borrowed for the call.
    let fd = unsafe {
        linux::call(
            numbers::OPENAT,
            [AT_FDCWD, path.as_ptr().addr(), O_RDONLY, 0, 0, 0],
        )
    }
    .ok()?;
    let mut len = 0;
    while let Some(rest) = line.get_mut(len..) {
        if rest.is_empty() {
            break;
        }
        match linux::read(fd, rest) {
            Ok(0) => break,
            Ok(got) => len += got,
            Err(_) => {
                let _ = linux::close(fd);
                return None;
            }
        }
    }
    let _ = linux::close(fd);
    Some(len)
}

/// Hand the GPU `keep` names over to its driver, or say why not
/// (`docs/NVIDIA.md` §12.2 and §12.3): its isolated-interrupts mark, then its
/// isolation, then its pin budget, each said in its line on the console.
/// `true` when the driver may be started.
fn hand_over_gpu(keep: &Device<Kernel>, info: &DeviceInfo) -> bool {
    let outcome = gpu::hand_over(&GpuNode(keep));
    let level = if outcome.starts() {
        Level::Info
    } else {
        Level::Warning
    };
    say(level, format_args!("{}", outcome.line(info.location)));
    if let HandOver::Start { budget, .. } = outcome {
        ferrix_rt::info!("{}", budget.line(info.location));
    }
    outcome.starts()
}

/// A line of devmgr's log at `level`: a record of its own on `/dev/kmsg`
/// (`ferrix_rt::log`), which the console prints and lode stores with its
/// level as stated -- how devmgr's own decisions reach the boot log, since
/// the kernel prints only what the bootstrap protocol carries.
fn say(level: Level, arguments: fmt::Arguments<'_>) {
    ferrix_rt::log::line(level, arguments);
}

/// A disk's driver -- virtio-blk's, an NVMe or AHCI controller's, or an
/// STM32MP15's SD card's -- over a block ring, for the disk to be `name`.
/// START's register blocks are virtio's or the tree node's; an NVMe or AHCI
/// controller has neither, and its driver finds its BAR through
/// `device_aperture`.
fn start_block(
    job: &Job<Kernel>,
    device: Device<Kernel>,
    image: &Vmo<Kernel>,
    info: &DeviceInfo,
    name: DiskName,
    port: &Port<Kernel>,
    key: u64,
) -> Result<Launched, ()> {
    let control = device.block_ring().map_err(|_| ())?;
    let block = |block: ferrix_native_abi::types::DeviceBlock| Block {
        phys: block.phys,
        offset: block.offset,
        length: block.length,
    };
    let start = Start {
        common: block(info.common),
        notify: block(info.notify),
        isr: block(info.isr),
        device: block(info.device),
        notify_off_multiplier: info.notify_off_multiplier,
        msix_table_size: info.msix_table_size,
        pci_device_id: info.device_id,
        location: info.location,
        name: *name.as_bytes(),
        pci_subsystem_vendor_id: info.subsystem_vendor_id,
        pci_subsystem_id: info.subsystem_id,
    };
    // Exactly the rights the driver may hold: the kernel handed the device
    // with DEVICE_RIGHTS and the control end with CONTROL_RIGHTS already, and
    // replacing to the same set is what makes that a fact rather than a hope.
    let device = device
        .into_owned()
        .replace(Requested::Exactly(DEVICE_RIGHTS))
        .map_err(|_| ())?;
    let control = control
        .into_owned()
        .replace(Requested::Exactly(CONTROL_RIGHTS))
        .map_err(|_| ())?;
    let encoded = Ring::Start(start).encode();
    // The process's name: the table chose the image already.
    let program = match info.virtio {
        DEVICE_TREE_BLOCKS => "sdmmc",
        DEVICE_VIRTIO_PCI => "blk",
        _ if pci_matches(AHCI_INTERFACE, info) => "ahci",
        _ => "nvme",
    };
    launch(
        job,
        image,
        program,
        port,
        key,
        encoded.as_bytes(),
        [device, control],
    )
}

/// A net driver -- virtio-net's `net`, or a YT6801's `yt6801` -- over a
/// net ring. The kernel names the interface, so START carries no name: only
/// which device it is, and index zero for "choose one".
fn start_net(
    job: &Job<Kernel>,
    device: Device<Kernel>,
    image: &Vmo<Kernel>,
    info: &DeviceInfo,
    port: &Port<Kernel>,
    key: u64,
) -> Result<Launched, ()> {
    let control = device.net_ring().map_err(|_| ())?;
    let start = NetStart {
        index: 0,
        location: info.location,
    };
    let device = device
        .into_owned()
        .replace(Requested::Exactly(NET_DEVICE_RIGHTS))
        .map_err(|_| ())?;
    let control = control
        .into_owned()
        .replace(Requested::Exactly(NET_CONTROL_RIGHTS))
        .map_err(|_| ())?;
    let mut bytes = [0_u8; MAX_MESSAGE];
    let written = NetRing::Start(start).encode(&mut bytes).map_err(|_| ())?;
    let program = if info.vendor_id == MOTORCOMM_VENDOR {
        "yt6801"
    } else {
        "net"
    };
    launch(
        job,
        image,
        program,
        port,
        key,
        bytes.get(..written).unwrap_or_default(),
        [device, control],
    )
}

/// A virtio-gpu driver, over the display control channel
/// (`docs/DISPLAY.md` §2.2). START is the block driver's: where the device's
/// virtio register blocks are, which a virtio-gpu driver needs the same way;
/// its name field is unused, since the kernel numbers the card.
///
/// A board's HDMI output (`docs/DISPLAY.md` §6) takes the same START with
/// its two register windows where the virtio blocks would be: the LTDC's in
/// `common`, the bridge's I2C controller's in `device`.
fn start_display(
    job: &Job<Kernel>,
    device: Device<Kernel>,
    image: &Vmo<Kernel>,
    info: &DeviceInfo,
    port: &Port<Kernel>,
    key: u64,
) -> Result<Launched, ()> {
    let (program, program_name) =
        if info.virtio == DEVICE_TREE_BLOCKS && info.device_id == TREE_FIRMWARE_FB {
            ("efifb", &[b'e', b'f', b'i', b'f', b'b', 0, 0, 0])
        } else if info.virtio == DEVICE_TREE_BLOCKS {
            ("ltdc", &[b'l', b't', b'd', b'c', 0, 0, 0, 0])
        } else {
            ("gpu", &[b'g', b'p', b'u', 0, 0, 0, 0, 0])
        };
    let control = device.display_control().map_err(|_| ())?;
    let block = |block: ferrix_native_abi::types::DeviceBlock| Block {
        phys: block.phys,
        offset: block.offset,
        length: block.length,
    };
    let start = Start {
        common: block(info.common),
        notify: block(info.notify),
        isr: block(info.isr),
        device: block(info.device),
        notify_off_multiplier: info.notify_off_multiplier,
        msix_table_size: info.msix_table_size,
        pci_device_id: info.device_id,
        location: info.location,
        name: *program_name,
        pci_subsystem_vendor_id: info.subsystem_vendor_id,
        pci_subsystem_id: info.subsystem_id,
    };
    let device = device
        .into_owned()
        .replace(Requested::Exactly(DEVICE_RIGHTS))
        .map_err(|_| ())?;
    let control = control
        .into_owned()
        .replace(Requested::Exactly(CONTROL_RIGHTS))
        .map_err(|_| ())?;
    let encoded = Ring::Start(start).encode();
    launch(
        job,
        image,
        program,
        port,
        key,
        encoded.as_bytes(),
        [device, control],
    )
}

/// A virtio-input driver, over an input control channel.
///
/// The same shape as the display's: the device, the channel, and where the
/// register blocks are. The name in START is the program's, which is what a
/// driver checks its device id against.
fn start_input(
    job: &Job<Kernel>,
    device: Device<Kernel>,
    image: &Vmo<Kernel>,
    info: &DeviceInfo,
    port: &Port<Kernel>,
    key: u64,
) -> Result<Launched, ()> {
    let control = device.input_control().map_err(|_| ())?;
    let block = |block: ferrix_native_abi::types::DeviceBlock| Block {
        phys: block.phys,
        offset: block.offset,
        length: block.length,
    };
    let start = Start {
        common: block(info.common),
        notify: block(info.notify),
        isr: block(info.isr),
        device: block(info.device),
        notify_off_multiplier: info.notify_off_multiplier,
        msix_table_size: info.msix_table_size,
        pci_device_id: info.device_id,
        location: info.location,
        name: [b'i', b'n', b'p', b'u', b't', 0, 0, 0],
        pci_subsystem_vendor_id: info.subsystem_vendor_id,
        pci_subsystem_id: info.subsystem_id,
    };
    let device = device
        .into_owned()
        .replace(Requested::Exactly(DEVICE_RIGHTS))
        .map_err(|_| ())?;
    let control = control
        .into_owned()
        .replace(Requested::Exactly(CONTROL_RIGHTS))
        .map_err(|_| ())?;
    let encoded = Ring::Start(start).encode();
    launch(
        job,
        image,
        "input",
        port,
        key,
        encoded.as_bytes(),
        [device, control],
    )
}

/// A virtio-snd driver, over the sound control channel (`docs/AUDIO.md`
/// §3.2), with START in blk's layout, as input's.
fn start_sound(
    job: &Job<Kernel>,
    device: Device<Kernel>,
    image: &Vmo<Kernel>,
    info: &DeviceInfo,
    port: &Port<Kernel>,
    key: u64,
) -> Result<Launched, ()> {
    let control = device.sound_control().map_err(|_| ())?;
    let block = |block: ferrix_native_abi::types::DeviceBlock| Block {
        phys: block.phys,
        offset: block.offset,
        length: block.length,
    };
    let start = Start {
        common: block(info.common),
        notify: block(info.notify),
        isr: block(info.isr),
        device: block(info.device),
        notify_off_multiplier: info.notify_off_multiplier,
        msix_table_size: info.msix_table_size,
        pci_device_id: info.device_id,
        location: info.location,
        name: [b's', b'n', b'd', 0, 0, 0, 0, 0],
        pci_subsystem_vendor_id: info.subsystem_vendor_id,
        pci_subsystem_id: info.subsystem_id,
    };
    let device = device
        .into_owned()
        .replace(Requested::Exactly(DEVICE_RIGHTS))
        .map_err(|_| ())?;
    let control = control
        .into_owned()
        .replace(Requested::Exactly(CONTROL_RIGHTS))
        .map_err(|_| ())?;
    let encoded = Ring::Start(start).encode();
    launch(
        job,
        image,
        "snd",
        port,
        key,
        encoded.as_bytes(),
        [device, control],
    )
}

/// A driver given its device and nothing else, as `program`: a virtio-serial
/// port's or a rendering engine's, which serve no kernel subsystem, or a bus
/// host's, which asks for its channels itself.
///
/// Every other kind asks the device for a control channel of its subsystem's
/// kind, and the kernel learns from that what the driver is for. A port has
/// no subsystem to name (`docs/CLIPBOARD.md` §5), nor has the GC400 yet
/// (`docs/GPU.md` §6.3), and a USB host has one input device per keyboard or
/// mouse it finds, which only it can count (`docs/INPUT.md` §7), so `launch`
/// makes the bootstrap channel START travels on, as it does for all of them,
/// and the driver makes the rest. Nothing here waits for it: a port or an
/// engine has no PUBLISHED it could ever send, and a host's devices publish
/// whenever they are plugged in.
fn start_plain(
    job: &Job<Kernel>,
    device: Device<Kernel>,
    image: &Vmo<Kernel>,
    info: &DeviceInfo,
    port: &Port<Kernel>,
    key: u64,
    program: &str,
) -> Result<Launched, ()> {
    let device = device
        .into_owned()
        .replace(Requested::Exactly(DEVICE_RIGHTS))
        .map_err(|_| ())?;
    let encoded = Ring::Start(plain_start(info, program)).encode();
    launch(job, image, program, port, key, encoded.as_bytes(), [device])
}

/// START for a driver that gets its device alone, or a keeper's.
fn plain_start(info: &DeviceInfo, program: &str) -> Start {
    let block = |block: ferrix_native_abi::types::DeviceBlock| Block {
        phys: block.phys,
        offset: block.offset,
        length: block.length,
    };
    Start {
        common: block(info.common),
        notify: block(info.notify),
        isr: block(info.isr),
        device: block(info.device),
        notify_off_multiplier: info.notify_off_multiplier,
        msix_table_size: info.msix_table_size,
        pci_device_id: info.device_id,
        location: info.location,
        name: start_name(program),
        pci_subsystem_vendor_id: info.subsystem_vendor_id,
        pci_subsystem_id: info.subsystem_id,
    }
}

/// START's name for `program`: its first eight bytes, NUL-padded.
fn start_name(program: &str) -> [u8; 8] {
    let mut name = [0_u8; 8];
    for (slot, byte) in name.iter_mut().zip(program.bytes()) {
        *slot = byte;
    }
    name
}

/// How long a bus host may take to settle its first enumeration before
/// devmgr reports without it: the DK board's hub, mouse and keyboard take a
/// second (`docs/INPUT.md` §7.3).
const SETTLE_NANOS: u64 = 5_000_000_000;

/// Wait for a bus host's driver to say its first enumeration has settled --
/// any message on its bootstrap channel -- or to die, or for
/// [`SETTLE_NANOS`]: whichever comes first. What it says is not read; a
/// driver that says nothing costs the boot the wait and no more.
fn await_settled(bootstrap: &Channel<Kernel>) {
    let Ok(now) = ferrix_rt::linux::monotonic_nanos() else {
        return;
    };
    let _ = bootstrap.wait_one(
        Signals::READABLE | Signals::PEER_CLOSED,
        Deadline::At(now.saturating_add(SETTLE_NANOS)),
    );
}

/// The job, the process, the bootstrap channel and the watch every driver
/// starts with: only START's bytes and handles differ between the rings.
fn launch<const N: usize>(
    job: &Job<Kernel>,
    image: &Vmo<Kernel>,
    program: &str,
    port: &Port<Kernel>,
    key: u64,
    start: &[u8],
    handles: [OwnedHandle<Kernel>; N],
) -> Result<Launched, ()> {
    let child_job = job.create_sealed_child().map_err(|_| ())?;
    launch_in(child_job, image, program, port, key, start, handles)
}

/// [`launch`] in `child_job`, which the caller made.
fn launch_in<const N: usize>(
    child_job: Job<Kernel>,
    image: &Vmo<Kernel>,
    program: &str,
    port: &Port<Kernel>,
    key: u64,
    start: &[u8],
    handles: [OwnedHandle<Kernel>; N],
) -> Result<Launched, ()> {
    let process = pending::create_process(&child_job, image, program).map_err(|_| ())?;
    let (near, far) = channel::create(Kernel).map_err(|_| ())?;
    near.write_with(start, handles).map_err(|_| ())?;
    process.notify_on_exit(port, key).map_err(|_| ())?;
    process.start(far.into_owned()).map_err(|_| ())?;
    Ok((child_job, process, near))
}

/// A driver as `launch` leaves it: its job, the process, and devmgr's end of
/// the channel START went down, which only a bus host's driver answers on.
type Launched = (Job<Kernel>, Process<Kernel>, Channel<Kernel>);
