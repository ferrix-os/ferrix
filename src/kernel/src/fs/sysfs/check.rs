//! sysfs's boot check (`docs/SYSFS.md` §7): a sysfs mounted under `/tmp`,
//! walked whole through the VFS as `find /sys` would walk it, and what it says
//! held against what its owners know.
//!
//! The walk is what keeps the tree honest with itself: every name a listing
//! gives must look up, as the kind the listing said and with the number it
//! said; every file must open and read to its end; every link must lead,
//! relative to where it is, to a directory in the same mount. Listings are
//! read three names at a time, so every one also resumes from its hash
//! cursor, as `ls` on a big directory does.
//!
//! It runs last among the boot checks, after `devmgr` has started its
//! drivers, so the facts are a running machine's and each is its owner's:
//! enumeration's devices and identifiers, the processors firmware described,
//! the disks the `blk` drivers published and the node each is in, the net
//! core's interfaces, devfs's memory devices, and which driver `devmgr` says
//! drives which device. Writing `bind` and `unbind`, which stops and starts a
//! driver, is `cargo xtask test-sysfs`'s, from a shell.

use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_sysfs::attr;
use ferrix_vfs::{Context, DirEntry, Errno, FileType, Namespace, OpenFlags};

use super::Sysfs;
use crate::device::{self, Location};
use crate::fs::{self, devfs};

/// What [`run`] counted, for the boot line.
#[derive(Debug, Default)]
pub struct Report {
    /// Names looked up.
    pub names: u32,
    /// Directories listed.
    pub directories: u32,
    /// Links read and followed.
    pub links: u32,
    /// Device nodes found where their bus shows them.
    pub devices: u32,
    /// Devices whose `driver` link and driver directory say what `devmgr`
    /// said.
    pub bound: u32,
    /// Disks whose files matched the disk.
    pub disks: u32,
    /// Interfaces listed in `class/net`.
    pub interfaces: u32,
    /// Processors in `devices/system/cpu`.
    pub cpus: u32,
    /// Changes refused as kernfs refuses them.
    pub refusals: u32,
}

/// Where the check mounts its sysfs.
const CHECK_AT: &[u8] = b"/tmp/sysfs-check";

/// The most names the walk looks at before it calls the tree runaway: a
/// machine has a few thousand.
const MAX_NAMES: u32 = 50_000;

/// A check's failure, by name.
type Checked<T> = Result<T, &'static str>;

/// The check's way into its mount.
struct Harness {
    ns: &'static Namespace,
    ctx: Context,
    /// The mount's device number, which every link must stay on.
    device: u64,
    report: Report,
}

impl Harness {
    /// `tail` beneath the mount.
    fn path(tail: &[u8]) -> Vec<u8> {
        let mut path = Vec::from(CHECK_AT);
        path.extend_from_slice(tail);
        path
    }

    /// The file at `tail`, read to its end.
    fn read(&self, tail: &[u8]) -> ferrix_vfs::Result<Vec<u8>> {
        let flags = OpenFlags {
            read: true,
            ..OpenFlags::default()
        };
        let file = self
            .ns
            .open(&self.ctx, None, &Harness::path(tail), &flags, 0)?;
        let mut contents = Vec::new();
        let mut chunk = [0_u8; 256];
        loop {
            let count = file.read(&mut chunk)?;
            let Some(read) = chunk.get(..count).filter(|read| !read.is_empty()) else {
                return Ok(contents);
            };
            contents.extend_from_slice(read);
        }
    }

    /// Whether the file at `tail` reads exactly `expected`.
    fn reads(&self, tail: &[u8], expected: &[u8]) -> bool {
        self.read(tail).as_deref() == Ok(expected)
    }

    /// Every name in the directory at `tail` but the dots, with its kind and
    /// number, read three at a time so that every listing resumes.
    fn list(&self, tail: &[u8]) -> Checked<Vec<(Vec<u8>, FileType, u64)>> {
        let flags = OpenFlags {
            read: true,
            directory: true,
            ..OpenFlags::default()
        };
        let file = self
            .ns
            .open(&self.ctx, None, &Harness::path(tail), &flags, 0)
            .map_err(|_| "a sysfs directory did not open")?;
        let mut names = Vec::new();
        loop {
            let mut taken = 0;
            file.read_dir(&mut |entry: DirEntry<'_>| {
                if taken == 3 {
                    return false;
                }
                taken += 1;
                if entry.name != b"." && entry.name != b".." {
                    names.push((entry.name.to_vec(), entry.kind, entry.ino));
                }
                true
            })
            .map_err(|_| "a sysfs directory did not list")?;
            if taken == 0 {
                break;
            }
            if names.len() > MAX_NAMES as usize {
                return Err("a sysfs directory lists without end");
            }
        }
        let mut sorted: Vec<&[u8]> = names.iter().map(|(name, _, _)| name.as_slice()).collect();
        sorted.sort_unstable();
        if sorted.windows(2).any(|pair| pair.first() == pair.get(1)) {
            return Err("a sysfs directory listed a name twice across a resumed listing");
        }
        Ok(names)
    }

    /// Require the error an operation answered, `got`, to be `errno`, and
    /// count it.
    fn refused(&mut self, got: Option<Errno>, errno: Errno, what: &'static str) -> Checked<()> {
        match got {
            Some(got) if got == errno => {
                self.report.refusals += 1;
                Ok(())
            }
            _ => Err(what),
        }
    }
}

/// Mount, walk, compare, refuse, unmount.
///
/// # Errors
///
/// The first thing that was not as it should be, by name.
pub fn run() -> Checked<Report> {
    let ns = fs::namespace();
    let ctx = ns.context();
    match ns.mkdir(&ctx, None, CHECK_AT, 0o755) {
        Ok(()) | Err(Errno::EEXIST) => {}
        Err(_) => return Err("could not make the sysfs check's mount point"),
    }
    let at = ns
        .resolve(&ctx, None, CHECK_AT, true)
        .map_err(|_| "the sysfs check's mount point did not resolve")?;
    let sysfs = Arc::new(Sysfs::new());
    let device = ferrix_vfs::FileSystem::device(sysfs.as_ref());
    let _mount = ns.mount(sysfs, &at).map_err(|_| "sysfs did not mount")?;
    let mut harness = Harness {
        ns,
        ctx,
        device,
        report: Report::default(),
    };
    let root = ns
        .resolve(&harness.ctx, None, CHECK_AT, true)
        .map_err(|_| "the sysfs mount did not resolve")?;
    if ns.statfs(&root).magic != super::SYSFS_MAGIC {
        return Err("statfs on sysfs does not say SYSFS_MAGIC");
    }

    walk(&mut harness)?;
    check_devices(&mut harness)?;
    check_cpus(&mut harness)?;
    check_disks(&mut harness)?;
    check_interfaces(&mut harness)?;
    check_char_devices(&harness)?;
    check_refusals(&mut harness)?;
    check_cgroup_mount_point(&harness)?;

    let root = ns
        .resolve(&harness.ctx, None, CHECK_AT, true)
        .map_err(|_| "the sysfs mount did not resolve")?;
    ns.unmount(&root).map_err(|_| "sysfs did not unmount")?;
    let _ = ns.rmdir(&harness.ctx, None, CHECK_AT);
    Ok(harness.report)
}

/// Every name in the tree, from the root down.
fn walk(harness: &mut Harness) -> Checked<()> {
    let mut pending: Vec<Vec<u8>> = alloc::vec![Vec::new()];
    while let Some(dir) = pending.pop() {
        harness.report.directories += 1;
        for (name, kind, ino) in harness.list(&dir)? {
            harness.report.names += 1;
            if harness.report.names > MAX_NAMES {
                return Err("the sysfs tree is larger than any machine's");
            }
            let mut tail = dir.clone();
            tail.push(b'/');
            tail.extend_from_slice(&name);
            let found = harness
                .ns
                .resolve(&harness.ctx, None, &Harness::path(&tail), false)
                .map_err(|_| "a name a sysfs directory listed does not look up")?;
            let stat = harness
                .ns
                .stat(&found)
                .map_err(|_| "a name a sysfs directory listed does not stat")?;
            if stat.metadata.kind != kind {
                return Err("a sysfs name is not the kind its directory listed it as");
            }
            if stat.metadata.ino != ino {
                return Err("a sysfs name has another number than its directory listed");
            }
            match kind {
                FileType::Directory => pending.push(tail),
                FileType::Symlink => follow(harness, &tail)?,
                _ if name == b"bind" || name == b"unbind" => {}
                _ => match harness.read(&tail) {
                    Ok(_) => {}
                    // An interface that is down has no carrier to report,
                    // and Linux refuses the read so.
                    Err(Errno::EINVAL) if name == b"carrier" => {}
                    Err(_) => return Err("a sysfs file did not open and read to its end"),
                },
            }
        }
    }
    Ok(())
}

/// A link: it must read, and lead to a directory in the same mount.
fn follow(harness: &mut Harness, tail: &[u8]) -> Checked<()> {
    let target = harness
        .ns
        .read_link(&harness.ctx, None, &Harness::path(tail))
        .map_err(|_| "a sysfs link did not read")?;
    if target.first() == Some(&b'/') {
        return Err("a sysfs link is absolute, and would leave a mount elsewhere");
    }
    let led = harness
        .ns
        .resolve(&harness.ctx, None, &Harness::path(tail), true)
        .map_err(|_| "a sysfs link leads nowhere")?;
    let stat = harness
        .ns
        .stat(&led)
        .map_err(|_| "a sysfs link leads to nothing that stats")?;
    if stat.dev != harness.device {
        return Err("a sysfs link leads out of its mount");
    }
    if stat.metadata.kind != FileType::Directory {
        return Err("a sysfs link leads to something other than a directory");
    }
    harness.report.links += 1;
    Ok(())
}

/// Every device node is on its bus, and a PCI function says what
/// enumeration read.
fn check_devices(harness: &mut Harness) -> Checked<()> {
    for index in 0..device::devices().len() {
        let node = device::devices()
            .get(index)
            .ok_or("a device node went away")?;
        let name = super::device_name(index).ok_or("a device node has no name")?;
        let bus: &[u8] = match node.location() {
            Location::Pci(_) => b"pci",
            Location::VirtioMmio(_) | Location::Tree(_) => b"platform",
        };
        let mut at = Vec::from(&b"/bus/"[..]);
        at.extend_from_slice(bus);
        at.extend_from_slice(b"/devices/");
        at.extend_from_slice(&name);
        let _ = harness
            .ns
            .resolve(&harness.ctx, None, &Harness::path(&at), true)
            .map_err(|_| "a device node is not in its bus's devices")?;
        if let Some(function) = node.pci_function() {
            for (file, expected) in [
                (
                    &b"/vendor"[..],
                    text(|out| attr::hex16(out, function.vendor)),
                ),
                (b"/device", text(|out| attr::hex16(out, function.device))),
                (b"/class", text(|out| attr::class(out, function.class))),
            ] {
                let mut tail = at.clone();
                tail.extend_from_slice(file);
                if !harness.reads(&tail, &expected) {
                    return Err("a PCI function's vendor, device or class is not enumeration's");
                }
            }
        }
        harness.report.devices += 1;
        check_binding(harness, index, &at, bus, &name)?;
    }
    Ok(())
}

/// The inode number of what `tail` leads to.
fn number_of(harness: &Harness, tail: &[u8]) -> Option<u64> {
    let at = harness
        .ns
        .resolve(&harness.ctx, None, &Harness::path(tail), true)
        .ok()?;
    harness.ns.stat(&at).ok().map(|stat| stat.metadata.ino)
}

/// A device's driver is the one `devmgr` said, three ways: its `driver`
/// link leads to the driver's directory, that directory has a link named
/// after the device, and its `uevent` says `DRIVER=`. A device `devmgr`
/// said nothing of has no `driver` link.
fn check_binding(
    harness: &mut Harness,
    index: usize,
    at: &[u8],
    bus: &[u8],
    name: &[u8],
) -> Checked<()> {
    let mut link = at.to_vec();
    link.extend_from_slice(b"/driver");
    let Some((_, driver)) = crate::discovery::devmgr::driver_of(index) else {
        if number_of(harness, &link).is_some() {
            return Err("a device devmgr bound nothing to has a driver link");
        }
        return Ok(());
    };
    let mut directory = Vec::from(&b"/bus/"[..]);
    directory.extend_from_slice(bus);
    directory.extend_from_slice(b"/drivers/");
    directory.extend_from_slice(&driver);
    if number_of(harness, &link).is_none()
        || number_of(harness, &link) != number_of(harness, &directory)
    {
        return Err("a device's driver link does not lead to the driver devmgr said");
    }
    let mut listed = directory.clone();
    listed.push(b'/');
    listed.extend_from_slice(name);
    if number_of(harness, &listed) != number_of(harness, at) {
        return Err("a driver's directory does not lead to a device devmgr said it drives");
    }
    let mut said = Vec::from(&b"DRIVER="[..]);
    said.extend_from_slice(&driver);
    said.push(b'\n');
    let mut uevent = at.to_vec();
    uevent.extend_from_slice(b"/uevent");
    let text = harness
        .read(&uevent)
        .map_err(|_| "a bound device's uevent did not read")?;
    if !text.starts_with(&said) {
        return Err("a bound device's uevent does not begin DRIVER= and its driver");
    }
    harness.report.bound += 1;
    Ok(())
}

/// A file's text, from a formatter.
fn text(fill: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = Vec::new();
    fill(&mut out);
    out
}

/// The processor lists are firmware's and the scheduler's.
fn check_cpus(harness: &mut Harness) -> Checked<()> {
    let count = super::cpu_count();
    let all: Vec<u32> = (0..count).collect();
    let possible = text(|out| attr::cpu_list(out, &all));
    let online = text(|out| attr::cpu_list(out, &super::cpus_online()));
    if !harness.reads(b"/devices/system/cpu/possible", &possible)
        || !harness.reads(b"/devices/system/cpu/present", &possible)
    {
        return Err("devices/system/cpu/possible or present is not every processor");
    }
    if !harness.reads(b"/devices/system/cpu/online", &online) {
        return Err("devices/system/cpu/online is not the processors that are running");
    }
    for cpu in 0..count {
        let tail = format!("/devices/system/cpu/cpu{cpu}");
        let _ = harness
            .ns
            .resolve(&harness.ctx, None, &Harness::path(tail.as_bytes()), true)
            .map_err(|_| "a processor has no cpu<N> directory")?;
        harness.report.cpus += 1;
    }
    Ok(())
}

/// Every disk devfs has is in `/sys/block`, with its size, `ro` and `dev`,
/// and `/sys/dev/block` names it.
fn check_disks(harness: &mut Harness) -> Checked<()> {
    for disk in devfs::disks() {
        let mut at = Vec::from(&b"/block/"[..]);
        at.extend_from_slice(&disk.name);
        let file = |name: &[u8]| {
            let mut tail = at.clone();
            tail.extend_from_slice(name);
            tail
        };
        let size = attr::size_in_512_byte_sectors(disk.device.sectors(), disk.device.sector_size());
        let dev = text(|out| attr::dev(out, disk.major, disk.minor));
        if !harness.reads(&file(b"/size"), &text(|out| attr::decimal(out, size)))
            || !harness.reads(
                &file(b"/ro"),
                &text(|out| attr::flag(out, disk.device.read_only())),
            )
            || !harness.reads(&file(b"/dev"), &dev)
        {
            return Err("a disk's size, ro or dev in /sys/block is not the disk's");
        }
        let number = format!("/dev/block/{}:{}/dev", disk.major, disk.minor);
        if !harness.reads(number.as_bytes(), &dev) {
            return Err("/sys/dev/block does not lead to a disk by its number");
        }
        // A disk a driver serves is inside that driver's device.
        if let Some(node) = disk.origin.node {
            let name = super::device_name(node).ok_or("a disk's device node has no name")?;
            let mut device = Vec::from(&b"/bus/pci/devices/"[..]);
            device.extend_from_slice(&name);
            if number_of(harness, &file(b"/device")).is_none()
                || number_of(harness, &file(b"/device")) != number_of(harness, &device)
            {
                return Err("a disk's device link does not lead to the node its driver serves");
            }
        }
        harness.report.disks += 1;
    }
    Ok(())
}

/// The loopback interface is interface 1, and every interface is listed.
fn check_interfaces(harness: &mut Harness) -> Checked<()> {
    if !harness.reads(b"/class/net/lo/ifindex", b"1\n")
        || !harness.reads(b"/class/net/lo/operstate", b"unknown\n")
    {
        return Err("lo is not interface 1, or not operstate unknown, in class/net");
    }
    let listed = harness.list(b"/class/net")?;
    let known = super::interfaces();
    if listed.len() != known.len() {
        return Err("class/net does not list every interface the net core has");
    }
    harness.report.interfaces = u32::try_from(listed.len()).unwrap_or(u32::MAX);
    Ok(())
}

/// A memory device by its number, as `/sys/dev/char` names it.
fn check_char_devices(harness: &Harness) -> Checked<()> {
    let target = harness
        .ns
        .read_link(&harness.ctx, None, &Harness::path(b"/dev/char/1:3"))
        .map_err(|_| "/sys/dev/char/1:3 is not a link")?;
    if target != b"../../devices/virtual/mem/null" {
        return Err("/sys/dev/char/1:3 does not lead to devices/virtual/mem/null");
    }
    if !harness.reads(
        b"/class/mem/null/uevent",
        b"MAJOR=1\nMINOR=3\nDEVNAME=null\nDEVMODE=0666\n",
    ) {
        return Err("null's uevent is not what Linux's says");
    }
    Ok(())
}

/// What kernfs refuses: a write to a value, and anything made or removed.
fn check_refusals(harness: &mut Harness) -> Checked<()> {
    let flags = OpenFlags {
        write: true,
        ..OpenFlags::default()
    };
    let written = harness
        .ns
        .open(
            &harness.ctx,
            None,
            &Harness::path(b"/devices/system/cpu/online"),
            &flags,
            0,
        )
        .and_then(|file| file.write(b"0-1\n"));
    harness.refused(
        written.err(),
        Errno::EACCES,
        "a write to a sysfs value was not EACCES",
    )?;
    let made = harness
        .ns
        .mkdir(&harness.ctx, None, &Harness::path(b"/made"), 0o755);
    harness.refused(made.err(), Errno::EPERM, "mkdir in sysfs was not EPERM")?;
    let removed = harness.ns.unlink(
        &harness.ctx,
        None,
        &Harness::path(b"/devices/system/cpu/online"),
    );
    harness.refused(removed.err(), Errno::EPERM, "unlink in sysfs was not EPERM")?;
    let created = OpenFlags {
        write: true,
        create: true,
        ..OpenFlags::default()
    };
    let opened = harness.ns.open(
        &harness.ctx,
        None,
        &Harness::path(b"/created"),
        &created,
        0o644,
    );
    harness.refused(
        opened.err(),
        Errno::EACCES,
        "a file created in sysfs was not EACCES",
    )
}

/// cgroup2 mounts on `fs/cgroup`, which needs its dentry remembered, and
/// comes off again.
fn check_cgroup_mount_point(harness: &Harness) -> Checked<()> {
    let at = harness
        .ns
        .resolve(&harness.ctx, None, &Harness::path(b"/fs/cgroup"), true)
        .map_err(|_| "sysfs has no fs/cgroup")?;
    let _mount = harness
        .ns
        .mount(Arc::new(fs::cgroupfs::Cgroupfs::new()), &at)
        .map_err(|_| "cgroup2 did not mount on sysfs's fs/cgroup")?;
    let _ = harness
        .ns
        .resolve(
            &harness.ctx,
            None,
            &Harness::path(b"/fs/cgroup/cgroup.procs"),
            true,
        )
        .map_err(|_| "cgroup2 on sysfs's fs/cgroup shows no cgroup.procs")?;
    let root = harness
        .ns
        .resolve(&harness.ctx, None, &Harness::path(b"/fs/cgroup"), true)
        .map_err(|_| "cgroup2's mount on fs/cgroup did not resolve")?;
    harness
        .ns
        .unmount(&root)
        .map_err(|_| "cgroup2 did not unmount from sysfs's fs/cgroup")
}
