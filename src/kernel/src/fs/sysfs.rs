//! sysfs: the machine's devices, their drivers and its processors, as the
//! Linux ABI shows them (`docs/SYSFS.md`).
//!
//! # A view, fed by the services that own each fact
//!
//! Nothing is stored here. Every directory is computed when it is looked in
//! and every file rendered when it is opened, from whoever owns the fact it
//! shows:
//!
//! * **the kernel's enumeration** -- `device::devices()`: what each PCI
//!   function's configuration space said, and which device tree nodes exist;
//! * **the cores the drivers publish into** -- the disks the block rings
//!   registered, the interfaces the net rings added, the cards, render nodes
//!   and input devices their cores accepted -- each of which now records the
//!   device node its driver serves, so sysfs can put it inside that node;
//! * **`devmgr`** -- which drivers it can start and which one drives which
//!   device, as it said in DRIVER, BOUND and UNBOUND (`src/kernel/src/discovery/devmgr.rs`).
//!   A write to a driver's `bind` or `unbind` is not done here: it is a
//!   request to `devmgr`, which decides, starts or stops the driver, and
//!   answers.
//!
//! So the drivers stay processes and `devmgr` stays the one that matches and
//! starts them; the kernel only puts what they told it where Linux programs
//! look.
//!
//! # Rendered at open, listed by hash
//!
//! A file is rendered when it is opened, as procfs's are, so one open reads
//! one snapshot. A directory lists its names in the order of a hash of each
//! name and resumes after the last hash it gave (`ferrix_sysfs::order`), so
//! a device coming or going between two `getdents64` calls neither repeats
//! nor hides a name that stayed. Inode numbers are hashes of paths: the same
//! node has the same number every time it is looked at.
//!
//! # Caching
//!
//! The directories whose names never change once the machine has booted --
//! `/sys` itself, `bus`, `class`, `dev`, `devices`, `fs` and a few more --
//! let the VFS remember their lookups, which is what lets cgroup2 be mounted
//! on `/sys/fs/cgroup`. Every directory whose names follow the drivers asks
//! afresh on every walk, as `/proc` and `/dev` do.
//!
//! # What is left out
//!
//! `/sys/kernel`, `/sys/firmware`, `/sys/power` and `/sys/module`, which
//! Ferrix has nothing true to put in; a PCI function's `resource`, `config`
//! and `irq`, which enumeration does not keep; a processor's `topology`,
//! which firmware's tables do not say reliably enough to print; and uevents
//! themselves -- the `uevent` files say what an event would carry, and no
//! event is sent. `docs/SYSFS.md` §6 says what each would take.

pub(crate) mod check;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;

use ferrix_sysfs::input::{self as input_text, Map};
use ferrix_sysfs::name::{self, Slot};
use ferrix_sysfs::{attr, drm as drm_text, net as net_text, order, path, pci as pci_text, uevent};
use ferrix_vfs::{
    DirEntry, Errno, FileSystem, FileType, Inode, Metadata, NewNode, OpenFile, Result, StatFs,
    Timespec,
};

use crate::device::{self, Location};
use crate::discovery::devmgr::{self, Bus, Request};
use crate::fs::devfs::{self, DiskInfo};
use crate::fs::{self, procfs};
use crate::interfaces::{display, input, net_ring, render};
use crate::{net, smp};

/// `SYSFS_MAGIC`, from `include/uapi/linux/magic.h`: how a program tells a
/// real sysfs from a directory somebody made.
const SYSFS_MAGIC: u64 = 0x6265_6572;

/// The PCI major for DRM nodes, and the base of the input core's minors.
const DRM_MAJOR: u32 = display::DRM_MAJOR;

/// A sysfs instance.
#[derive(Debug)]
pub(crate) struct Sysfs {
    /// What every node shares.
    shared: Arc<Shared>,
}

/// What every node of one instance shares.
#[derive(Debug)]
struct Shared {
    /// `st_dev`.
    device: u64,
    /// Every timestamp: when it was mounted.
    made: Timespec,
}

impl Sysfs {
    /// A sysfs, stamped with the time it was made.
    pub(crate) fn new() -> Sysfs {
        Sysfs {
            shared: Arc::new(Shared {
                device: fs::anonymous_device(),
                made: fs::clock().now(),
            }),
        }
    }
}

impl FileSystem for Sysfs {
    fn root(&self) -> Arc<dyn Inode> {
        Arc::new(Node {
            shared: Arc::clone(&self.shared),
            path: Vec::new(),
            what: What::Dir(Dir::Root),
        })
    }

    fn name(&self) -> &'static str {
        "sysfs"
    }

    fn device(&self) -> u64 {
        self.shared.device
    }

    fn statfs(&self) -> StatFs {
        StatFs {
            magic: SYSFS_MAGIC,
            block_size: 4096,
            name_max: 255,
            ..StatFs::default()
        }
    }
}

// -- The tree ------------------------------------------------------------------

/// The classes sysfs groups devices in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Disks.
    Block,
    /// Cards, render nodes and connectors.
    Drm,
    /// Input devices and their event nodes.
    Input,
    /// The memory devices: `null`, `zero` and the rest.
    Mem,
    /// Network interfaces.
    Net,
    /// The terminal devices the kernel has: `tty`, `console`, `ptmx`.
    Tty,
}

impl Class {
    /// Every class, in `/sys/class`'s order.
    const ALL: [Class; 6] = [
        Class::Block,
        Class::Drm,
        Class::Input,
        Class::Mem,
        Class::Net,
        Class::Tty,
    ];

    /// Its name.
    const fn name(self) -> &'static [u8] {
        match self {
            Class::Block => b"block",
            Class::Drm => b"drm",
            Class::Input => b"input",
            Class::Mem => b"mem",
            Class::Net => b"net",
            Class::Tty => b"tty",
        }
    }
}

/// A bus's name under `/sys/bus`.
const fn bus_name(bus: Bus) -> &'static [u8] {
    match bus {
        Bus::Pci => b"pci",
        Bus::Platform => b"platform",
    }
}

/// A directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    /// `/sys`.
    Root,
    /// `/sys/block`: a link per disk.
    Block,
    /// `/sys/bus`.
    Bus,
    /// `/sys/bus/<bus>`.
    BusOf(Bus),
    /// `/sys/bus/<bus>/devices`: a link per device on it.
    BusDevices(Bus),
    /// `/sys/bus/<bus>/drivers`: a directory per driver `devmgr` has for it.
    BusDrivers(Bus),
    /// `/sys/bus/<bus>/drivers/<driver>`.
    Driver(Bus, u16),
    /// `/sys/class`.
    Class,
    /// `/sys/class/<class>`: a link per device in it.
    ClassOf(Class),
    /// `/sys/dev`.
    Dev,
    /// `/sys/dev/char`: a link per character device number.
    DevChar,
    /// `/sys/dev/block`: a link per disk number.
    DevBlock,
    /// `/sys/devices`.
    Devices,
    /// `/sys/devices/platform`: the device tree nodes.
    Platform,
    /// `/sys/devices/system`.
    System,
    /// `/sys/devices/system/cpu`.
    Cpus,
    /// `/sys/devices/system/cpu/cpu<N>`.
    Cpu(u32),
    /// `/sys/devices/virtual`.
    Virtual,
    /// `/sys/devices/virtual/<class>`: devices of a class no node backs.
    VirtualOf(Class),
    /// `/sys/devices/pci<segment>:<bus>`: a root bus.
    PciRoot(u16, u8),
    /// A device node's directory, by its index in `device::devices()`.
    Device(usize),
    /// `<device>/<class>`: the class devices a node's driver published.
    DeviceClass(usize, Class),
    /// `card<N>`.
    Card(u32),
    /// `card<N>-<type>-<M>`: head `M - 1` of card `N`.
    Connector(u32, u32),
    /// `renderD<N>`.
    Render(u32),
    /// A disk, by its registration serial.
    Disk(u64),
    /// Its `queue` directory.
    DiskQueue(u64),
    /// An interface, by its index.
    Net(u32),
    /// Its `statistics` directory.
    NetStats(u32),
    /// `input<N>`.
    Input(u32),
    /// Its `id` directory.
    InputId(u32),
    /// Its `capabilities` directory.
    InputCaps(u32),
    /// `event<N>`, inside `input<N>`.
    Event(u32),
    /// One of devfs's static nodes, by its place in the table.
    Char(usize),
    /// `/sys/fs`.
    Fs,
    /// `/sys/fs/cgroup`: an empty directory for cgroup2 to be mounted on.
    FsCgroup,
}

impl Dir {
    /// Whether the directory's names never change once the machine is up,
    /// so the VFS may remember its lookups; see the module documentation.
    const fn fixed(self) -> bool {
        matches!(
            self,
            Dir::Root
                | Dir::Bus
                | Dir::BusOf(_)
                | Dir::Class
                | Dir::Dev
                | Dir::Devices
                | Dir::Platform
                | Dir::System
                | Dir::Cpus
                | Dir::Virtual
                | Dir::Fs
                | Dir::FsCgroup
        )
    }
}

/// A counter under an interface's `statistics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Counter {
    RxPackets,
    RxBytes,
    RxErrors,
    RxDropped,
    TxPackets,
    TxBytes,
    TxErrors,
    TxDropped,
}

impl Counter {
    const ALL: [Counter; 8] = [
        Counter::RxBytes,
        Counter::RxDropped,
        Counter::RxErrors,
        Counter::RxPackets,
        Counter::TxBytes,
        Counter::TxDropped,
        Counter::TxErrors,
        Counter::TxPackets,
    ];
}

/// A capability bitmap under an input device's `capabilities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cap {
    Ev,
    Key,
    Rel,
    Abs,
    Msc,
    Led,
    Snd,
    Ff,
    Sw,
}

impl Cap {
    const ALL: [Cap; 9] = [
        Cap::Ev,
        Cap::Key,
        Cap::Rel,
        Cap::Abs,
        Cap::Msc,
        Cap::Led,
        Cap::Snd,
        Cap::Ff,
        Cap::Sw,
    ];
}

/// A file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attr {
    Vendor,
    Device,
    SubsystemVendor,
    SubsystemDevice,
    Class,
    Revision,
    Modalias,
    Uevent,
    Dev,
    Size,
    Ro,
    Removable,
    Range,
    Serial,
    LogicalBlockSize,
    PhysicalBlockSize,
    HwSectorSize,
    Address,
    AddrLen,
    Broadcast,
    Carrier,
    Flags,
    Ifindex,
    Iflink,
    Mtu,
    Operstate,
    Type,
    Stat(Counter),
    Status,
    Enabled,
    Modes,
    /// A connector's EDID, the bytes as its `EDID` property's blob holds
    /// them, and empty where it has none, as Linux's `edid` file is.
    Edid,
    Version,
    Name,
    Phys,
    Uniq,
    Properties,
    Bustype,
    Product,
    Cap(Cap),
    Online,
    Possible,
    Present,
    Offline,
    KernelMax,
    Bind,
    Unbind,
}

impl Attr {
    /// Its name.
    const fn name(self) -> &'static [u8] {
        match self {
            Attr::Vendor => b"vendor",
            Attr::Device => b"device",
            Attr::SubsystemVendor => b"subsystem_vendor",
            Attr::SubsystemDevice => b"subsystem_device",
            Attr::Class => b"class",
            Attr::Revision => b"revision",
            Attr::Modalias => b"modalias",
            Attr::Uevent => b"uevent",
            Attr::Dev => b"dev",
            Attr::Size => b"size",
            Attr::Ro => b"ro",
            Attr::Removable => b"removable",
            Attr::Range => b"range",
            Attr::Serial => b"serial",
            Attr::LogicalBlockSize => b"logical_block_size",
            Attr::PhysicalBlockSize => b"physical_block_size",
            Attr::HwSectorSize => b"hw_sector_size",
            Attr::Address => b"address",
            Attr::AddrLen => b"addr_len",
            Attr::Broadcast => b"broadcast",
            Attr::Carrier => b"carrier",
            Attr::Flags => b"flags",
            Attr::Ifindex => b"ifindex",
            Attr::Iflink => b"iflink",
            Attr::Mtu => b"mtu",
            Attr::Operstate => b"operstate",
            Attr::Type => b"type",
            Attr::Stat(counter) => match counter {
                Counter::RxPackets => b"rx_packets",
                Counter::RxBytes => b"rx_bytes",
                Counter::RxErrors => b"rx_errors",
                Counter::RxDropped => b"rx_dropped",
                Counter::TxPackets => b"tx_packets",
                Counter::TxBytes => b"tx_bytes",
                Counter::TxErrors => b"tx_errors",
                Counter::TxDropped => b"tx_dropped",
            },
            Attr::Status => b"status",
            Attr::Enabled => b"enabled",
            Attr::Modes => b"modes",
            Attr::Edid => b"edid",
            Attr::Version => b"version",
            Attr::Name => b"name",
            Attr::Phys => b"phys",
            Attr::Uniq => b"uniq",
            Attr::Properties => b"properties",
            Attr::Bustype => b"bustype",
            Attr::Product => b"product",
            Attr::Cap(cap) => match cap {
                Cap::Ev => b"ev",
                Cap::Key => b"key",
                Cap::Rel => b"rel",
                Cap::Abs => b"abs",
                Cap::Msc => b"msc",
                Cap::Led => b"led",
                Cap::Snd => b"snd",
                Cap::Ff => b"ff",
                Cap::Sw => b"sw",
            },
            Attr::Online => b"online",
            Attr::Possible => b"possible",
            Attr::Present => b"present",
            Attr::Offline => b"offline",
            Attr::KernelMax => b"kernel_max",
            Attr::Bind => b"bind",
            Attr::Unbind => b"unbind",
        }
    }

    /// Whether a write goes anywhere: only `bind` and `unbind` take one.
    const fn writable(self) -> bool {
        matches!(self, Attr::Bind | Attr::Unbind)
    }

    /// Whether it can be read: everything but `bind` and `unbind`, which
    /// have nothing to show.
    const fn readable(self) -> bool {
        !self.writable()
    }

    /// Its permission bits, as Linux gives each.
    const fn permissions(self) -> u32 {
        if self.writable() { 0o200 } else { 0o444 }
    }
}

/// What a name in a directory is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum What {
    /// A directory.
    Dir(Dir),
    /// A file of a directory.
    File(Dir, Attr),
    /// A link, to the node at this path beneath the mount's root.
    Link(Vec<Vec<u8>>),
}

impl What {
    /// The kind of object it is.
    const fn kind(&self) -> FileType {
        match self {
            What::Dir(_) => FileType::Directory,
            What::File(..) => FileType::Regular,
            What::Link(_) => FileType::Symlink,
        }
    }
}

/// One name in a directory.
#[derive(Debug)]
struct Entry {
    /// The name.
    name: Vec<u8>,
    /// What it is.
    what: What,
}

/// Build a directory's entries.
#[derive(Debug, Default)]
struct Listing(Vec<Entry>);

impl Listing {
    fn dir(&mut self, name: &[u8], dir: Dir) {
        self.0.push(Entry {
            name: name.to_vec(),
            what: What::Dir(dir),
        });
    }

    fn file(&mut self, of: Dir, attr: Attr) {
        self.0.push(Entry {
            name: attr.name().to_vec(),
            what: What::File(of, attr),
        });
    }

    fn files(&mut self, of: Dir, attrs: &[Attr]) {
        for &attr in attrs {
            self.file(of, attr);
        }
    }

    /// A link to `target`, if it is still somewhere.
    fn link(&mut self, name: &[u8], target: Option<Vec<Vec<u8>>>) {
        if let Some(target) = target {
            self.0.push(Entry {
                name: name.to_vec(),
                what: What::Link(target),
            });
        }
    }

    /// A link to the directory `dir`, if it is still somewhere.
    fn link_to(&mut self, name: &[u8], dir: Dir) {
        self.link(name, path_of(dir));
    }
}

// -- What the owners say -------------------------------------------------------

/// A path, from its components.
fn path(components: &[&[u8]]) -> Vec<Vec<u8>> {
    components.iter().map(|part| part.to_vec()).collect()
}

/// A number with a prefix: `card0`, `cpu3`.
fn numbered(prefix: &str, number: u32) -> Vec<u8> {
    alloc::format!("{prefix}{number}").into_bytes()
}

/// Where a PCI function sits: on a root bus, or behind a bridge that is
/// itself a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parent {
    /// `pci<segment>:<bus>`.
    Root(u16, u8),
    /// The bridge whose secondary bus the function is on.
    Bridge(usize),
    /// A device tree node: `platform`.
    Platform,
}

/// The parent of device node `index`.
fn parent_of(index: usize) -> Option<Parent> {
    let nodes = device::devices();
    let node = nodes.get(index)?;
    let Location::Pci(address) = node.location() else {
        return Some(Parent::Platform);
    };
    let bridge = nodes.iter().position(|other| {
        other.index() != index
            && matches!(other.location(), Location::Pci(at) if at.segment() == address.segment())
            && other
                .pci_function()
                .is_some_and(|function| function.secondary_bus == Some(address.bus()))
    });
    Some(match bridge {
        Some(bridge) => Parent::Bridge(bridge),
        None => Parent::Root(address.segment(), address.bus()),
    })
}

/// A device node's name: its slot for a PCI function, and for a device tree
/// node its address and what Linux names its node.
fn device_name(index: usize) -> Option<Vec<u8>> {
    let node = device::devices().get(index)?;
    let mut out = Vec::new();
    match node.location() {
        Location::Pci(address) => Slot {
            segment: address.segment(),
            bus: address.bus(),
            device: address.device(),
            function: address.function(),
        }
        .name(&mut out),
        Location::VirtioMmio(base) => name::platform(&mut out, base, "virtio_mmio"),
        Location::Tree(base) => {
            let kind = match node.binding() {
                ferrix_native_abi::types::TREE_STM32_HDMI => "display-controller",
                ferrix_native_abi::types::TREE_STM32_USBH => "usbh-ehci",
                ferrix_native_abi::types::TREE_GS201_DWC3 => "dwc3",
                _ => "device",
            };
            name::platform(&mut out, base, kind);
        }
    }
    Some(out)
}

/// Which bus a device node is on.
fn bus_of(index: usize) -> Option<Bus> {
    let node = device::devices().get(index)?;
    Some(match node.location() {
        Location::Pci(_) => Bus::Pci,
        Location::VirtioMmio(_) | Location::Tree(_) => Bus::Platform,
    })
}

/// The device node on `bus` called `wanted`.
fn device_named(bus: Bus, wanted: &[u8]) -> Option<usize> {
    (0..device::devices().len())
        .find(|&index| bus_of(index) == Some(bus) && device_name(index).as_deref() == Some(wanted))
}

/// A device node's directory. Bridges nest, as far as eight deep: a loop in
/// what firmware said about secondary buses ends there rather than for ever.
fn device_path(index: usize) -> Option<Vec<Vec<u8>>> {
    let mut chain = vec![device_name(index)?];
    let mut at = index;
    for _ in 0..8 {
        match parent_of(at)? {
            Parent::Bridge(bridge) => {
                chain.push(device_name(bridge)?);
                at = bridge;
            }
            Parent::Root(segment, bus) => {
                let mut root = Vec::new();
                name::pci_root(&mut root, segment, bus);
                chain.push(root);
                chain.push(b"devices".to_vec());
                chain.reverse();
                return Some(chain);
            }
            Parent::Platform => {
                chain.push(b"platform".to_vec());
                chain.push(b"devices".to_vec());
                chain.reverse();
                return Some(chain);
            }
        }
    }
    None
}

/// The directory a class device of node `node` goes in: `<node>/<class>`,
/// or `devices/virtual/<class>` with no node.
fn class_dir_path(node: Option<usize>, class: Class) -> Option<Vec<Vec<u8>>> {
    match node {
        Some(node) => {
            let mut at = device_path(node)?;
            at.push(class.name().to_vec());
            Some(at)
        }
        None => Some(path(&[b"devices", b"virtual", class.name()])),
    }
}

/// `path` with `name` beneath it.
fn beneath(mut path: Vec<Vec<u8>>, name: &[u8]) -> Vec<Vec<u8>> {
    path.push(name.to_vec());
    path
}

/// The disk registered as `registration`, if it still is.
fn disk(registration: u64) -> Option<DiskInfo> {
    devfs::disks()
        .into_iter()
        .find(|disk| disk.registration == registration)
}

/// An interface, copied out of the net core.
#[derive(Debug, Clone)]
struct Interface {
    index: u32,
    name: Vec<u8>,
    hardware: [u8; 6],
    mtu: u32,
    flags: u32,
    loopback: bool,
    counters: ferrix_net::iface::Counters,
}

/// Every interface, copied out: nothing may sleep inside the net core's
/// lock, and rendering allocates.
fn interfaces() -> Vec<Interface> {
    // The reader's network namespace's (`docs/NETNS.md` section 4).
    net::acting().core().look(|stack| {
        stack
            .interfaces()
            .iter()
            .map(|interface| Interface {
                index: interface.index,
                name: interface.name.as_bytes().to_vec(),
                hardware: interface.hardware,
                mtu: interface.mtu,
                flags: interface.flags,
                loopback: interface.medium == ferrix_net::iface::Medium::Loopback,
                counters: interface.counters,
            })
            .collect()
    })
}

/// Interface `index`, if it is still there.
fn interface(index: u32) -> Option<Interface> {
    interfaces().into_iter().find(|each| each.index == index)
}

/// The static node at `slot` of devfs's table, and its class.
fn char_node(slot: usize) -> Option<(devfs::CharNode, Class)> {
    let node = devfs::char_nodes().nth(slot)?;
    let class = if node.major == 1 {
        Class::Mem
    } else {
        Class::Tty
    };
    Some((node, class))
}

/// The absolute path of `dir`, if what it shows is still there.
fn path_of(dir: Dir) -> Option<Vec<Vec<u8>>> {
    Some(match dir {
        Dir::Root => Vec::new(),
        Dir::Block => path(&[b"block"]),
        Dir::Bus => path(&[b"bus"]),
        Dir::BusOf(bus) => path(&[b"bus", bus_name(bus)]),
        Dir::BusDevices(bus) => path(&[b"bus", bus_name(bus), b"devices"]),
        Dir::BusDrivers(bus) => path(&[b"bus", bus_name(bus), b"drivers"]),
        Dir::Driver(bus, driver) => {
            let (_, name) = devmgr::drivers_on(bus)
                .ok()?
                .into_iter()
                .find(|(at, _)| *at == driver)?;
            path(&[b"bus", bus_name(bus), b"drivers", &name])
        }
        Dir::Class => path(&[b"class"]),
        Dir::ClassOf(class) => path(&[b"class", class.name()]),
        Dir::Dev => path(&[b"dev"]),
        Dir::DevChar => path(&[b"dev", b"char"]),
        Dir::DevBlock => path(&[b"dev", b"block"]),
        Dir::Devices => path(&[b"devices"]),
        Dir::Platform => path(&[b"devices", b"platform"]),
        Dir::System => path(&[b"devices", b"system"]),
        Dir::Cpus => path(&[b"devices", b"system", b"cpu"]),
        Dir::Cpu(cpu) => beneath(path_of(Dir::Cpus)?, &numbered("cpu", cpu)),
        Dir::Virtual => path(&[b"devices", b"virtual"]),
        Dir::VirtualOf(class) => path(&[b"devices", b"virtual", class.name()]),
        Dir::PciRoot(segment, bus) => {
            let mut root = Vec::new();
            name::pci_root(&mut root, segment, bus);
            path(&[b"devices", &root])
        }
        Dir::Device(index) => device_path(index)?,
        Dir::DeviceClass(index, class) => class_dir_path(Some(index), class)?,
        Dir::Card(index) => {
            let card = display::card(index)?;
            beneath(
                class_dir_path(Some(card.node), Class::Drm)?,
                &numbered("card", index),
            )
        }
        Dir::Connector(card, head) => {
            let at = path_of(Dir::Card(card))?;
            beneath(at, &connector_name(card, head)?)
        }
        Dir::Render(index) => {
            let renderer = render::renderer(index)?;
            beneath(
                class_dir_path(Some(renderer.node), Class::Drm)?,
                &numbered("renderD", index),
            )
        }
        Dir::Disk(registration) => {
            let disk = disk(registration)?;
            beneath(class_dir_path(disk.origin.node, Class::Block)?, &disk.name)
        }
        Dir::DiskQueue(registration) => beneath(path_of(Dir::Disk(registration))?, b"queue"),
        Dir::Net(index) => {
            let interface = interface(index)?;
            beneath(
                class_dir_path(net_ring::node_of(index), Class::Net)?,
                &interface.name,
            )
        }
        Dir::NetStats(index) => beneath(path_of(Dir::Net(index))?, b"statistics"),
        Dir::Input(index) => {
            let device = input::device(index)?;
            beneath(
                class_dir_path(Some(device.node), Class::Input)?,
                &numbered("input", index),
            )
        }
        Dir::InputId(index) => beneath(path_of(Dir::Input(index))?, b"id"),
        Dir::InputCaps(index) => beneath(path_of(Dir::Input(index))?, b"capabilities"),
        Dir::Event(index) => beneath(path_of(Dir::Input(index))?, &numbered("event", index)),
        Dir::Char(slot) => {
            let (node, class) = char_node(slot)?;
            path(&[b"devices", b"virtual", class.name(), node.name])
        }
        Dir::Fs => path(&[b"fs"]),
        Dir::FsCgroup => path(&[b"fs", b"cgroup"]),
    })
}

/// The name of head `head` of card `card`: `card0-Virtual-1`.
fn connector_name(card: u32, head: u32) -> Option<Vec<u8>> {
    let shown = display::card(card)?;
    let connector = display::drm::connectors(&shown)
        .into_iter()
        .nth(usize::try_from(head).ok()?)?;
    let mut out = Vec::new();
    drm_text::connector_name(&mut out, card, connector.kind, connector.kind_index);
    Some(out)
}

/// The indices of the cards, render nodes and input devices node `node`'s
/// drivers published, and its disks and interfaces.
fn has_class(node: usize, class: Class) -> bool {
    match class {
        Class::Drm => {
            display::card_indices()
                .into_iter()
                .any(|index| display::card(index).is_some_and(|card| card.node == node))
                || render::renderer_indices()
                    .into_iter()
                    .any(|index| render::renderer(index).is_some_and(|shown| shown.node == node))
        }
        Class::Block => devfs::disks()
            .iter()
            .any(|disk| disk.origin.node == Some(node)),
        Class::Net => interfaces()
            .iter()
            .any(|each| net_ring::node_of(each.index) == Some(node)),
        Class::Input => input::device_indices()
            .into_iter()
            .any(|index| input::device(index).is_some_and(|device| device.node == node)),
        Class::Mem | Class::Tty => false,
    }
}

// -- Listing -------------------------------------------------------------------

/// What `dir` holds now, or `ENOENT` for a directory whose object is gone.
fn entries(dir: Dir) -> Result<Vec<Entry>> {
    let mut list = Listing::default();
    match dir {
        Dir::Devices
        | Dir::Platform
        | Dir::PciRoot(..)
        | Dir::System
        | Dir::Cpus
        | Dir::Cpu(_)
        | Dir::Virtual
        | Dir::VirtualOf(_)
        | Dir::Device(_)
        | Dir::DeviceClass(..) => device_tree_entries(&mut list, dir)?,
        Dir::Card(_) | Dir::Connector(..) | Dir::Render(_) | Dir::Disk(_) | Dir::DiskQueue(_) => {
            drm_and_disk_entries(&mut list, dir)?;
        }
        Dir::Net(_)
        | Dir::NetStats(_)
        | Dir::Input(_)
        | Dir::InputId(_)
        | Dir::InputCaps(_)
        | Dir::Event(_)
        | Dir::Char(_) => other_class_entries(&mut list, dir)?,
        _ => index_entries(&mut list, dir)?,
    }
    Ok(list.0)
}

/// The directories that index the tree: `/sys` and everything under
/// `block`, `bus`, `class`, `dev` and `fs`.
fn index_entries(list: &mut Listing, dir: Dir) -> Result<()> {
    match dir {
        Dir::Root => {
            list.dir(b"block", Dir::Block);
            list.dir(b"bus", Dir::Bus);
            list.dir(b"class", Dir::Class);
            list.dir(b"dev", Dir::Dev);
            list.dir(b"devices", Dir::Devices);
            list.dir(b"fs", Dir::Fs);
        }
        Dir::Block => {
            for disk in devfs::disks() {
                list.link_to(&disk.name, Dir::Disk(disk.registration));
            }
        }
        Dir::Bus => {
            list.dir(b"pci", Dir::BusOf(Bus::Pci));
            list.dir(b"platform", Dir::BusOf(Bus::Platform));
        }
        Dir::BusOf(bus) => {
            list.dir(b"devices", Dir::BusDevices(bus));
            list.dir(b"drivers", Dir::BusDrivers(bus));
        }
        Dir::BusDevices(bus) => {
            for index in 0..device::devices().len() {
                if bus_of(index) == Some(bus)
                    && let Some(name) = device_name(index)
                {
                    list.link_to(&name, Dir::Device(index));
                }
            }
        }
        Dir::BusDrivers(bus) => {
            for (driver, name) in devmgr::drivers_on(bus).map_err(|_| Errno::ENOMEM)? {
                list.dir(&name, Dir::Driver(bus, driver));
            }
        }
        Dir::Driver(bus, driver) => {
            let _ = path_of(dir).ok_or(Errno::ENOENT)?;
            list.files(dir, &[Attr::Bind, Attr::Unbind]);
            for index in devmgr::bound_to(driver).map_err(|_| Errno::ENOMEM)? {
                if bus_of(index) == Some(bus)
                    && let Some(name) = device_name(index)
                {
                    list.link_to(&name, Dir::Device(index));
                }
            }
        }
        Dir::Class => {
            for class in Class::ALL {
                list.dir(class.name(), Dir::ClassOf(class));
            }
        }
        Dir::ClassOf(class) => class_links(list, class),
        Dir::Dev => {
            list.dir(b"block", Dir::DevBlock);
            list.dir(b"char", Dir::DevChar);
        }
        Dir::DevBlock => {
            for disk in devfs::disks() {
                let mut name = Vec::new();
                name::dev_number(&mut name, disk.major, disk.minor);
                list.link_to(&name, Dir::Disk(disk.registration));
            }
        }
        Dir::DevChar => dev_char_links(list),
        Dir::Fs => list.dir(b"cgroup", Dir::FsCgroup),
        Dir::FsCgroup => {}
        _ => {}
    }
    Ok(())
}

/// `/sys/devices`: the buses, the device nodes, the processors and the
/// virtual devices.
fn device_tree_entries(list: &mut Listing, dir: Dir) -> Result<()> {
    match dir {
        Dir::Devices => {
            list.dir(b"platform", Dir::Platform);
            list.dir(b"system", Dir::System);
            list.dir(b"virtual", Dir::Virtual);
            let mut roots: Vec<(u16, u8)> = (0..device::devices().len())
                .filter_map(|index| match parent_of(index) {
                    Some(Parent::Root(segment, bus)) => Some((segment, bus)),
                    _ => None,
                })
                .collect();
            roots.sort_unstable();
            roots.dedup();
            for (segment, bus) in roots {
                let mut name = Vec::new();
                name::pci_root(&mut name, segment, bus);
                list.dir(&name, Dir::PciRoot(segment, bus));
            }
        }
        Dir::Platform => children(list, Parent::Platform),
        Dir::PciRoot(segment, bus) => children(list, Parent::Root(segment, bus)),
        Dir::System => list.dir(b"cpu", Dir::Cpus),
        Dir::Cpus => {
            list.files(
                dir,
                &[
                    Attr::KernelMax,
                    Attr::Offline,
                    Attr::Online,
                    Attr::Possible,
                    Attr::Present,
                ],
            );
            for cpu in 0..cpu_count() {
                list.dir(&numbered("cpu", cpu), Dir::Cpu(cpu));
            }
        }
        Dir::Cpu(cpu) => {
            if cpu >= cpu_count() {
                return Err(Errno::ENOENT);
            }
            // The boot processor cannot be taken offline, and Linux gives it
            // no `online` to say so with.
            if cpu > 0 {
                list.file(dir, Attr::Online);
            }
        }
        Dir::Virtual => {
            for class in [Class::Block, Class::Mem, Class::Net, Class::Tty] {
                list.dir(class.name(), Dir::VirtualOf(class));
            }
        }
        Dir::VirtualOf(class) => virtual_entries(list, class),
        Dir::Device(index) => device_entries(list, index)?,
        Dir::DeviceClass(index, class) => device_class_entries(list, index, class),
        _ => {}
    }
    Ok(())
}

/// A card, a connector, a render node, a disk.
fn drm_and_disk_entries(list: &mut Listing, dir: Dir) -> Result<()> {
    match dir {
        Dir::Card(index) => {
            let card = display::card(index).ok_or(Errno::ENOENT)?;
            list.files(dir, &[Attr::Dev, Attr::Uevent]);
            list.link_to(b"subsystem", Dir::ClassOf(Class::Drm));
            list.link_to(b"device", Dir::Device(card.node));
            for head in 0..display::drm::connectors(&card).len() {
                let head = u32::try_from(head).unwrap_or(u32::MAX);
                if let Some(name) = connector_name(index, head) {
                    list.dir(&name, Dir::Connector(index, head));
                }
            }
        }
        Dir::Connector(card, _) => {
            let _ = path_of(dir).ok_or(Errno::ENOENT)?;
            list.files(dir, &[Attr::Edid, Attr::Enabled, Attr::Modes, Attr::Status]);
            list.link_to(b"subsystem", Dir::ClassOf(Class::Drm));
            list.link_to(b"device", Dir::Card(card));
        }
        Dir::Render(index) => {
            let renderer = render::renderer(index).ok_or(Errno::ENOENT)?;
            list.files(dir, &[Attr::Dev, Attr::Uevent]);
            list.link_to(b"subsystem", Dir::ClassOf(Class::Drm));
            list.link_to(b"device", Dir::Device(renderer.node));
        }
        Dir::Disk(registration) => {
            let disk = disk(registration).ok_or(Errno::ENOENT)?;
            list.files(
                dir,
                &[
                    Attr::Dev,
                    Attr::Range,
                    Attr::Removable,
                    Attr::Ro,
                    Attr::Size,
                    Attr::Uevent,
                ],
            );
            if serial_text(&disk.origin.serial).is_some() {
                list.file(dir, Attr::Serial);
            }
            list.dir(b"queue", Dir::DiskQueue(registration));
            list.link_to(b"subsystem", Dir::ClassOf(Class::Block));
            if let Some(node) = disk.origin.node {
                list.link_to(b"device", Dir::Device(node));
            }
        }
        Dir::DiskQueue(registration) => {
            let _ = disk(registration).ok_or(Errno::ENOENT)?;
            list.files(
                dir,
                &[
                    Attr::HwSectorSize,
                    Attr::LogicalBlockSize,
                    Attr::PhysicalBlockSize,
                ],
            );
        }
        _ => {}
    }
    Ok(())
}

/// An interface, an input device and its event node, a memory or terminal
/// device.
fn other_class_entries(list: &mut Listing, dir: Dir) -> Result<()> {
    match dir {
        Dir::Net(index) => {
            let _ = interface(index).ok_or(Errno::ENOENT)?;
            list.files(
                dir,
                &[
                    Attr::AddrLen,
                    Attr::Address,
                    Attr::Broadcast,
                    Attr::Carrier,
                    Attr::Flags,
                    Attr::Ifindex,
                    Attr::Iflink,
                    Attr::Mtu,
                    Attr::Operstate,
                    Attr::Type,
                    Attr::Uevent,
                ],
            );
            list.dir(b"statistics", Dir::NetStats(index));
            list.link_to(b"subsystem", Dir::ClassOf(Class::Net));
            if let Some(node) = net_ring::node_of(index) {
                list.link_to(b"device", Dir::Device(node));
            }
        }
        Dir::NetStats(index) => {
            let _ = interface(index).ok_or(Errno::ENOENT)?;
            for counter in Counter::ALL {
                list.file(dir, Attr::Stat(counter));
            }
        }
        Dir::Input(index) => {
            let device = input::device(index).ok_or(Errno::ENOENT)?;
            list.files(
                dir,
                &[
                    Attr::Name,
                    Attr::Phys,
                    Attr::Properties,
                    Attr::Uevent,
                    Attr::Uniq,
                ],
            );
            list.dir(b"capabilities", Dir::InputCaps(index));
            list.dir(b"id", Dir::InputId(index));
            list.dir(&numbered("event", index), Dir::Event(index));
            list.link_to(b"subsystem", Dir::ClassOf(Class::Input));
            list.link_to(b"device", Dir::Device(device.node));
        }
        Dir::InputId(index) => {
            let _ = input::device(index).ok_or(Errno::ENOENT)?;
            list.files(
                dir,
                &[Attr::Bustype, Attr::Product, Attr::Vendor, Attr::Version],
            );
        }
        Dir::InputCaps(index) => {
            let _ = input::device(index).ok_or(Errno::ENOENT)?;
            for cap in Cap::ALL {
                list.file(dir, Attr::Cap(cap));
            }
        }
        Dir::Event(index) => {
            let _ = input::device(index).ok_or(Errno::ENOENT)?;
            list.files(dir, &[Attr::Dev, Attr::Uevent]);
            list.link_to(b"subsystem", Dir::ClassOf(Class::Input));
            list.link_to(b"device", Dir::Input(index));
        }
        Dir::Char(slot) => {
            let (_, class) = char_node(slot).ok_or(Errno::ENOENT)?;
            list.files(dir, &[Attr::Dev, Attr::Uevent]);
            list.link_to(b"subsystem", Dir::ClassOf(class));
        }
        _ => {}
    }
    Ok(())
}

/// How many processors firmware described: `possible` and `present`, which
/// are the same here, since nothing hot-plugs one.
fn cpu_count() -> u32 {
    smp::topology().map_or(1, |topology| {
        u32::try_from(topology.count()).unwrap_or(u32::MAX)
    })
}

/// The processors that are running, ascending.
fn cpus_online() -> Vec<u32> {
    smp::topology().map_or_else(
        || vec![0],
        |topology| {
            topology
                .cpus()
                .iter()
                .filter(|cpu| cpu.is_online())
                .filter_map(|cpu| u32::try_from(cpu.logical).ok())
                .collect()
        },
    )
}

/// The device nodes whose parent is `parent`, each a directory.
fn children(list: &mut Listing, parent: Parent) {
    for index in 0..device::devices().len() {
        if parent_of(index) == Some(parent)
            && let Some(name) = device_name(index)
        {
            list.dir(&name, Dir::Device(index));
        }
    }
}

/// A device node's directory: its files, the links to its bus and driver,
/// the functions behind it if it is a bridge, and its class devices.
fn device_entries(list: &mut Listing, index: usize) -> Result<()> {
    let node = device::devices().get(index).ok_or(Errno::ENOENT)?;
    let dir = Dir::Device(index);
    let bus = bus_of(index).ok_or(Errno::ENOENT)?;
    if node.pci_function().is_some() {
        list.files(
            dir,
            &[
                Attr::Class,
                Attr::Device,
                Attr::Modalias,
                Attr::Revision,
                Attr::SubsystemDevice,
                Attr::SubsystemVendor,
                Attr::Uevent,
                Attr::Vendor,
            ],
        );
    } else {
        list.file(dir, Attr::Uevent);
    }
    list.link_to(b"subsystem", Dir::BusOf(bus));
    if let Some((driver, _)) = devmgr::driver_of(index) {
        list.link_to(b"driver", Dir::Driver(bus, driver));
    }
    children(list, Parent::Bridge(index));
    for class in [Class::Block, Class::Drm, Class::Input, Class::Net] {
        if has_class(index, class) {
            list.dir(class.name(), Dir::DeviceClass(index, class));
        }
    }
    Ok(())
}

/// `<device>/<class>`: the class devices of that class node `index` has.
fn device_class_entries(list: &mut Listing, index: usize, class: Class) {
    match class {
        Class::Drm => {
            for card in display::card_indices() {
                if display::card(card).is_some_and(|shown| shown.node == index) {
                    list.dir(&numbered("card", card), Dir::Card(card));
                }
            }
            for renderer in render::renderer_indices() {
                if render::renderer(renderer).is_some_and(|shown| shown.node == index) {
                    list.dir(&numbered("renderD", renderer), Dir::Render(renderer));
                }
            }
        }
        Class::Block => {
            for disk in devfs::disks() {
                if disk.origin.node == Some(index) {
                    list.dir(&disk.name, Dir::Disk(disk.registration));
                }
            }
        }
        Class::Net => {
            for each in interfaces() {
                if net_ring::node_of(each.index) == Some(index) {
                    list.dir(&each.name, Dir::Net(each.index));
                }
            }
        }
        Class::Input => {
            for device in input::device_indices() {
                if input::device(device).is_some_and(|shown| shown.node == index) {
                    list.dir(&numbered("input", device), Dir::Input(device));
                }
            }
        }
        Class::Mem | Class::Tty => {}
    }
}

/// `devices/virtual/<class>`: the devices of a class no node backs.
fn virtual_entries(list: &mut Listing, class: Class) {
    match class {
        Class::Mem | Class::Tty => {
            for slot in 0..devfs::char_nodes().count() {
                if let Some((node, of)) = char_node(slot)
                    && of == class
                {
                    list.dir(node.name, Dir::Char(slot));
                }
            }
        }
        Class::Net => {
            for each in interfaces() {
                if net_ring::node_of(each.index).is_none() {
                    list.dir(&each.name, Dir::Net(each.index));
                }
            }
        }
        Class::Block => {
            for disk in devfs::disks() {
                if disk.origin.node.is_none() {
                    list.dir(&disk.name, Dir::Disk(disk.registration));
                }
            }
        }
        Class::Drm | Class::Input => {}
    }
}

/// `/sys/class/<class>`: a link per device of the class.
fn class_links(list: &mut Listing, class: Class) {
    match class {
        Class::Block => {
            for disk in devfs::disks() {
                list.link_to(&disk.name, Dir::Disk(disk.registration));
            }
        }
        Class::Drm => {
            list.file(Dir::ClassOf(Class::Drm), Attr::Version);
            for card in display::card_indices() {
                list.link_to(&numbered("card", card), Dir::Card(card));
                let heads =
                    display::card(card).map_or(0, |shown| display::drm::connectors(&shown).len());
                for head in 0..heads {
                    let head = u32::try_from(head).unwrap_or(u32::MAX);
                    if let Some(name) = connector_name(card, head) {
                        list.link_to(&name, Dir::Connector(card, head));
                    }
                }
            }
            for renderer in render::renderer_indices() {
                list.link_to(&numbered("renderD", renderer), Dir::Render(renderer));
            }
        }
        Class::Input => {
            for device in input::device_indices() {
                list.link_to(&numbered("input", device), Dir::Input(device));
                list.link_to(&numbered("event", device), Dir::Event(device));
            }
        }
        Class::Net => {
            for each in interfaces() {
                list.link_to(&each.name, Dir::Net(each.index));
            }
        }
        Class::Mem | Class::Tty => {
            for slot in 0..devfs::char_nodes().count() {
                if let Some((node, of)) = char_node(slot)
                    && of == class
                {
                    list.link_to(node.name, Dir::Char(slot));
                }
            }
        }
    }
}

/// `/sys/dev/char`: every character device number the kernel answers to.
fn dev_char_links(list: &mut Listing) {
    let mut link = |major: u32, minor: u32, dir: Dir| {
        let mut name = Vec::new();
        name::dev_number(&mut name, major, minor);
        list.link_to(&name, dir);
    };
    for (slot, node) in devfs::char_nodes().enumerate() {
        link(node.major, node.minor, Dir::Char(slot));
    }
    for card in display::card_indices() {
        link(DRM_MAJOR, card, Dir::Card(card));
    }
    for renderer in render::renderer_indices() {
        link(DRM_MAJOR, renderer, Dir::Render(renderer));
    }
    for device in input::device_indices() {
        link(
            input::INPUT_MAJOR,
            input::EVDEV_MINOR_BASE.saturating_add(device),
            Dir::Event(device),
        );
    }
}

// -- Rendering -----------------------------------------------------------------

/// A disk's serial as text: up to the first NUL, and `None` for none.
fn serial_text(serial: &[u8; 20]) -> Option<&[u8]> {
    let end = serial.iter().position(|&byte| byte == 0).unwrap_or(20);
    serial.get(..end).filter(|text| !text.is_empty())
}

/// What file `attr` of `dir` says now.
fn render(dir: Dir, attr: Attr) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match dir {
        Dir::Device(index) => render_device(&mut out, index, attr)?,
        Dir::Cpus => render_cpus(&mut out, attr),
        Dir::Cpu(cpu) => attr::flag(&mut out, cpus_online().contains(&cpu)),
        Dir::ClassOf(Class::Drm) => out.extend_from_slice(drm_text::VERSION),
        Dir::Card(index) => {
            let _ = display::card(index).ok_or(Errno::ENOENT)?;
            node_file(
                &mut out,
                attr,
                DRM_MAJOR,
                index,
                &numbered("dri/card", index),
                Some(b"drm_minor"),
            );
        }
        Dir::Render(index) => {
            let _ = render::renderer(index).ok_or(Errno::ENOENT)?;
            node_file(
                &mut out,
                attr,
                DRM_MAJOR,
                index,
                &numbered("dri/renderD", index),
                Some(b"drm_minor"),
            );
        }
        Dir::Connector(card, head) => {
            let shown = display::card(card).ok_or(Errno::ENOENT)?;
            let head = usize::try_from(head).map_err(|_| Errno::ENOENT)?;
            let connector = display::drm::connectors(&shown)
                .into_iter()
                .nth(head)
                .ok_or(Errno::ENOENT)?;
            match attr {
                Attr::Edid => out.extend_from_slice(shown.edid(head).unwrap_or_default()),
                Attr::Status => drm_text::status(&mut out, connector.connected),
                Attr::Enabled => drm_text::enabled(&mut out, connector.connected),
                _ => drm_text::modes(&mut out, &connector.modes),
            }
        }
        Dir::Disk(registration) => render_disk(&mut out, registration, attr)?,
        Dir::DiskQueue(registration) => {
            let disk = disk(registration).ok_or(Errno::ENOENT)?;
            attr::decimal(&mut out, u64::from(disk.device.sector_size()));
        }
        Dir::Net(index) | Dir::NetStats(index) => render_net(&mut out, index, attr)?,
        Dir::Input(index) | Dir::InputId(index) | Dir::InputCaps(index) => {
            render_input(&mut out, index, attr)?;
        }
        Dir::Event(index) => {
            let _ = input::device(index).ok_or(Errno::ENOENT)?;
            node_file(
                &mut out,
                attr,
                input::INPUT_MAJOR,
                input::EVDEV_MINOR_BASE.saturating_add(index),
                &numbered("input/event", index),
                None,
            );
        }
        Dir::Char(slot) => {
            let (node, _) = char_node(slot).ok_or(Errno::ENOENT)?;
            match attr {
                Attr::Dev => attr::dev(&mut out, node.major, node.minor),
                _ => uevent::node(
                    &mut out,
                    &uevent::Node {
                        major: node.major,
                        minor: node.minor,
                        name: node.name,
                        mode: (node.permissions != 0o600).then_some(node.permissions),
                        kind: None,
                    },
                ),
            }
        }
        _ => return Err(Errno::ENOENT),
    }
    Ok(out)
}

/// `devices/system/cpu`'s files: the processor lists, and the highest
/// number a processor could have.
fn render_cpus(out: &mut Vec<u8>, attr: Attr) {
    let count = cpu_count();
    let online = cpus_online();
    match attr {
        Attr::Online => attr::cpu_list(out, &online),
        Attr::Offline => {
            let offline: Vec<u32> = (0..count).filter(|cpu| !online.contains(cpu)).collect();
            attr::cpu_list(out, &offline);
        }
        Attr::KernelMax => {
            attr::decimal(out, (ferrix_sched::MAX_CPUS as u64).saturating_sub(1));
        }
        _ => {
            let all: Vec<u32> = (0..count).collect();
            attr::cpu_list(out, &all);
        }
    }
}

/// A class device's `dev` or `uevent`.
fn node_file(
    out: &mut Vec<u8>,
    attr: Attr,
    major: u32,
    minor: u32,
    name: &[u8],
    kind: Option<&[u8]>,
) {
    if attr == Attr::Dev {
        attr::dev(out, major, minor);
    } else {
        uevent::node(
            out,
            &uevent::Node {
                major,
                minor,
                name,
                mode: None,
                kind,
            },
        );
    }
}

/// A device node's files.
fn render_device(out: &mut Vec<u8>, index: usize, attr: Attr) -> Result<()> {
    let node = device::devices().get(index).ok_or(Errno::ENOENT)?;
    let driver = devmgr::driver_of(index).map(|(_, name)| name);
    let Some(function) = node.pci_function() else {
        if let Some(driver) = driver {
            uevent::driver(out, &driver);
        }
        return Ok(());
    };
    let ids = pci_text::Identity {
        vendor: function.vendor,
        device: function.device,
        subsystem_vendor: function.subsystem_vendor,
        subsystem_device: function.subsystem,
        class: function.class,
    };
    match attr {
        Attr::Vendor => attr::hex16(out, ids.vendor),
        Attr::Device => attr::hex16(out, ids.device),
        Attr::SubsystemVendor => attr::hex16(out, ids.subsystem_vendor),
        Attr::SubsystemDevice => attr::hex16(out, ids.subsystem_device),
        Attr::Class => attr::class(out, ids.class),
        Attr::Revision => attr::hex8(out, function.revision),
        Attr::Modalias => pci_text::modalias(out, &ids),
        _ => {
            let slot = device_name(index).ok_or(Errno::ENOENT)?;
            pci_text::uevent(out, &ids, &slot, driver.as_deref());
        }
    }
    Ok(())
}

/// A disk's files.
fn render_disk(out: &mut Vec<u8>, registration: u64, attr: Attr) -> Result<()> {
    let disk = disk(registration).ok_or(Errno::ENOENT)?;
    let device = &disk.device;
    match attr {
        Attr::Dev => attr::dev(out, disk.major, disk.minor),
        Attr::Size => attr::decimal(
            out,
            attr::size_in_512_byte_sectors(device.sectors(), device.sector_size()),
        ),
        Attr::Ro => attr::flag(out, device.read_only()),
        Attr::Removable => attr::flag(out, false),
        // Minors a disk owns: itself and its partitions, as `DiskName`
        // numbers them.
        Attr::Range => attr::decimal(out, 16),
        Attr::Serial => attr::line(out, serial_text(&disk.origin.serial).unwrap_or_default()),
        _ => uevent::node(
            out,
            &uevent::Node {
                major: disk.major,
                minor: disk.minor,
                name: &disk.name,
                mode: None,
                kind: Some(b"disk"),
            },
        ),
    }
    Ok(())
}

/// An interface's files.
fn render_net(out: &mut Vec<u8>, index: u32, attr: Attr) -> Result<()> {
    let each = interface(index).ok_or(Errno::ENOENT)?;
    let counters = each.counters;
    match attr {
        Attr::Address => attr::hardware_address(out, &each.hardware),
        Attr::AddrLen => attr::decimal(out, 6),
        Attr::Broadcast => {
            let broadcast = if each.loopback { [0; 6] } else { [0xff; 6] };
            attr::hardware_address(out, &broadcast);
        }
        Attr::Carrier => {
            let carrier = net_text::carrier(each.flags).ok_or(Errno::EINVAL)?;
            attr::flag(out, carrier);
        }
        Attr::Flags => attr::alternate_hex(out, u64::from(each.flags & 0xffff)),
        Attr::Ifindex | Attr::Iflink => attr::decimal(out, u64::from(each.index)),
        Attr::Mtu => attr::decimal(out, u64::from(each.mtu)),
        Attr::Operstate => out.extend_from_slice(net_text::operstate(each.flags)),
        Attr::Type => {
            let kind = if each.loopback {
                net_text::ARPHRD_LOOPBACK
            } else {
                net_text::ARPHRD_ETHER
            };
            attr::decimal(out, u64::from(kind));
        }
        Attr::Stat(counter) => attr::decimal(
            out,
            match counter {
                Counter::RxPackets => counters.received,
                Counter::RxBytes => counters.received_bytes,
                Counter::RxErrors => counters.received_errors,
                Counter::RxDropped => counters.received_dropped,
                Counter::TxPackets => counters.sent,
                Counter::TxBytes => counters.sent_bytes,
                Counter::TxErrors => counters.sent_errors,
                Counter::TxDropped => counters.sent_dropped,
            },
        ),
        _ => uevent::interface(out, &each.name, each.index),
    }
    Ok(())
}

/// An input device's files: `input<N>`'s, and those of its `id` and
/// `capabilities` directories.
fn render_input(out: &mut Vec<u8>, index: u32, attr: Attr) -> Result<()> {
    let device = input::device(index).ok_or(Errno::ENOENT)?;
    // The session is copied out of its lock before anything is formatted.
    let (id, name, uniq, bits) = {
        let session = device.session.lock();
        (
            session.id(),
            session.name().as_bytes().to_vec(),
            session.serial().as_bytes().to_vec(),
            session.capabilities().bits,
        )
    };
    // The kernel's own `long`, which Linux prints the maps in words of.
    let word_bits = usize::BITS;
    match attr {
        Attr::Name => attr::line(out, &name),
        // No driver says where its device is plugged in.
        Attr::Phys => attr::line(out, b""),
        Attr::Uniq => attr::line(out, &uniq),
        Attr::Properties => input_text::bitmap(out, &bits.props, input_text::PROP_MAX, word_bits),
        Attr::Bustype => input_text::id(out, id.bustype),
        Attr::Vendor => input_text::id(out, id.vendor),
        Attr::Product => input_text::id(out, id.product),
        Attr::Version => input_text::id(out, id.version),
        Attr::Cap(cap) => {
            // No driver declares sound or force feedback, so both are empty.
            let (map, max): (&[u8], u32) = match cap {
                Cap::Ev => (&bits.types, input_text::EV_MAX),
                Cap::Key => (&bits.keys, input_text::KEY_MAX),
                Cap::Rel => (&bits.rels, input_text::REL_MAX),
                Cap::Abs => (&bits.abs, input_text::ABS_MAX),
                Cap::Msc => (&bits.msc, input_text::MSC_MAX),
                Cap::Led => (&bits.leds, input_text::LED_MAX),
                Cap::Snd => (&[], input_text::SND_MAX),
                Cap::Ff => (&[], input_text::FF_MAX),
                Cap::Sw => (&bits.sw, input_text::SW_MAX),
            };
            input_text::bitmap(out, map, max, word_bits);
        }
        _ => {
            let maps = [
                (
                    1,
                    Map {
                        key: "KEY",
                        bits: &bits.keys,
                        max: input_text::KEY_MAX,
                    },
                ),
                (
                    2,
                    Map {
                        key: "REL",
                        bits: &bits.rels,
                        max: input_text::REL_MAX,
                    },
                ),
                (
                    3,
                    Map {
                        key: "ABS",
                        bits: &bits.abs,
                        max: input_text::ABS_MAX,
                    },
                ),
                (
                    4,
                    Map {
                        key: "MSC",
                        bits: &bits.msc,
                        max: input_text::MSC_MAX,
                    },
                ),
                (
                    17,
                    Map {
                        key: "LED",
                        bits: &bits.leds,
                        max: input_text::LED_MAX,
                    },
                ),
                (
                    5,
                    Map {
                        key: "SW",
                        bits: &bits.sw,
                        max: input_text::SW_MAX,
                    },
                ),
            ];
            input_text::uevent(
                out,
                &input_text::Device {
                    id: [id.bustype, id.vendor, id.product, id.version],
                    name: &name,
                    uniq: &uniq,
                    properties: &bits.props,
                    types: &bits.types,
                    maps: &maps,
                },
                word_bits,
            );
        }
    }
    Ok(())
}

/// A write of `data` to file `attr` of `dir`: only a driver's `bind` and
/// `unbind` take one, and each is a request to `devmgr`.
///
/// What Linux checks before it asks a driver is checked here: the name must
/// be a device on the driver's bus (`ENODEV`), and an unbind must be of this
/// driver's device (`ENODEV`). Everything else is `devmgr`'s to answer.
fn write(dir: Dir, attr: Attr, data: &[u8]) -> Result<usize> {
    let Dir::Driver(bus, driver) = dir else {
        return Err(Errno::EACCES);
    };
    let named = name::written(data).ok_or(Errno::ENODEV)?;
    let device = device_named(bus, named).ok_or(Errno::ENODEV)?;
    match attr {
        Attr::Bind => devmgr::request(Request::Bind, device, driver)?,
        Attr::Unbind => {
            if devmgr::driver_of(device).map(|(at, _)| at) != Some(driver) {
                return Err(Errno::ENODEV);
            }
            devmgr::request(Request::Unbind, device, driver)?;
        }
        _ => return Err(Errno::EACCES),
    }
    Ok(data.len())
}

// -- The inode -----------------------------------------------------------------

/// A sysfs inode: a path, and what is there.
#[derive(Debug)]
struct Node {
    /// The instance's device and timestamps.
    shared: Arc<Shared>,
    /// Its components beneath the mount's root.
    path: Vec<Vec<u8>>,
    /// What it is.
    what: What,
}

impl Node {
    /// The path, as slices.
    fn components(&self) -> Vec<&[u8]> {
        self.path.iter().map(Vec::as_slice).collect()
    }

    /// The directory this is, or `ENOTDIR`.
    fn dir(&self) -> Result<Dir> {
        match self.what {
            What::Dir(dir) => Ok(dir),
            _ => Err(Errno::ENOTDIR),
        }
    }
}

impl Inode for Node {
    fn metadata(&self) -> Metadata {
        let made = self.shared.made;
        let kind = self.what.kind();
        let (permissions, size) = match &self.what {
            What::Dir(_) => (0o755, 0),
            // Linux says a page for every attribute, whatever it holds.
            What::File(_, attr) => (attr.permissions(), 4096),
            What::Link(_) => (
                0o777,
                self.read_link().map_or(0, |target| target.len() as u64),
            ),
        };
        Metadata {
            ino: order::inode(&self.components()),
            kind,
            permissions,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: 0,
            gid: 0,
            size,
            rdev: 0,
            blocks: 0,
            block_size: 4096,
            atime: made,
            mtime: made,
            ctime: made,
        }
    }

    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }

    fn caches_lookups(&self) -> bool {
        matches!(self.what, What::Dir(dir) if dir.fixed())
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn Inode>> {
        let dir = self.dir()?;
        let entry = entries(dir)?
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or(Errno::ENOENT)?;
        let mut path = self.path.clone();
        path.push(entry.name);
        Ok(Arc::new(Node {
            shared: Arc::clone(&self.shared),
            path,
            what: entry.what,
        }))
    }

    fn read_dir(&self, cursor: u64, emit: &mut dyn FnMut(DirEntry<'_>) -> bool) -> Result<()> {
        let dir = self.dir()?;
        let mut listed: Vec<(u64, Entry)> = entries(dir)?
            .into_iter()
            .map(|entry| (order::cursor(&entry.name), entry))
            .filter(|(at, _)| *at >= cursor)
            .collect();
        listed.sort_unstable_by(|(a, one), (b, other)| a.cmp(b).then(one.name.cmp(&other.name)));
        let mut components = self.components();
        for (at, entry) in &listed {
            components.push(&entry.name);
            let ino = order::inode(&components);
            let _ = components.pop();
            let accepted = emit(DirEntry {
                ino,
                kind: entry.what.kind(),
                name: &entry.name,
                next: at.saturating_add(1),
            });
            if !accepted {
                break;
            }
        }
        Ok(())
    }

    fn read_link(&self) -> Result<Vec<u8>> {
        let What::Link(target) = &self.what else {
            return Err(Errno::EINVAL);
        };
        let from: Vec<&[u8]> = self
            .path
            .iter()
            .take(self.path.len().saturating_sub(1))
            .map(Vec::as_slice)
            .collect();
        let to: Vec<&[u8]> = target.iter().map(Vec::as_slice).collect();
        let mut out = Vec::new();
        path::relative(&mut out, &from, &to);
        Ok(out)
    }

    fn open(&self) -> Result<Option<Arc<dyn Inode>>> {
        let What::File(dir, attr) = self.what else {
            return Ok(None);
        };
        let bytes = if attr.readable() {
            render(dir, attr)?
        } else {
            Vec::new()
        };
        let writer: Option<procfs::Writer> = attr
            .writable()
            .then(|| Box::new(move |data: &[u8]| write(dir, attr, data)) as procfs::Writer);
        procfs::snapshot(self.metadata(), bytes, writer, Errno::EACCES).map(Some)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        match self.what {
            What::File(..) => self.open()?.ok_or(Errno::EINVAL)?.read_at(offset, buf),
            What::Dir(_) => Err(Errno::EISDIR),
            What::Link(_) => Err(Errno::EINVAL),
        }
    }

    /// Accepted and ignored on a file that takes writes, for the `O_TRUNC` a
    /// shell's `>` opens with; refused on every other.
    fn set_len(&self, _len: u64) -> Result<()> {
        match self.what {
            What::File(_, attr) if attr.writable() => Ok(()),
            _ => Err(Errno::EACCES),
        }
    }

    /// Nothing is made in sysfs: kernfs has no `create`, so the VFS answers
    /// `EACCES` for a file, and its `mkdir` refuses with `EPERM`.
    fn create(&self, _name: &[u8], node: NewNode<'_>, _permissions: u32) -> Result<Arc<dyn Inode>> {
        let _ = self.dir()?;
        Err(match node {
            NewNode::Regular => Errno::EACCES,
            _ => Errno::EPERM,
        })
    }

    fn link(&self, _name: &[u8], _target: &Arc<dyn Inode>) -> Result<()> {
        let _ = self.dir()?;
        Err(Errno::EPERM)
    }

    fn unlink(&self, _name: &[u8]) -> Result<()> {
        let _ = self.dir()?;
        Err(Errno::EPERM)
    }

    fn rmdir(&self, _name: &[u8]) -> Result<()> {
        let _ = self.dir()?;
        Err(Errno::EPERM)
    }

    fn rename(
        &self,
        _old: &[u8],
        _new_parent: &Arc<dyn Inode>,
        _new: &[u8],
        _replace: bool,
    ) -> Result<()> {
        let _ = self.dir()?;
        Err(Errno::EPERM)
    }
}

/// An open file `openat` made, refused with `EACCES` if it is an attribute
/// opened for what it cannot do: `bind` or `unbind` for reading, anything
/// else for writing. kernfs refuses both at open, and so does this, since
/// [`Inode::open`] is not told the access mode.
pub(crate) fn refuse_open(file: Arc<OpenFile>) -> Result<Arc<OpenFile>> {
    let Ok(node) = Arc::clone(file.inode()).into_any().downcast::<Node>() else {
        return Ok(file);
    };
    if let What::File(_, attr) = node.what
        && ((file.writable() && !attr.writable()) || (file.readable() && !attr.readable()))
    {
        return Err(Errno::EACCES);
    }
    Ok(file)
}

/// Mount a sysfs on `/sys`, making the directory if the archive had none.
pub(crate) fn mount() -> Result<()> {
    let ns = fs::namespace();
    let ctx = ns.context();
    match ns.mkdir(&ctx, None, b"/sys", 0o555) {
        Ok(()) | Err(Errno::EEXIST) => {}
        Err(errno) => return Err(errno),
    }
    let at = ns.resolve(&ctx, None, b"/sys", true)?;
    ns.mount(Arc::new(Sysfs::new()), &at).map(drop)
}
