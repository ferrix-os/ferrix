//! The kernel's start of `linuxd` under `personality=user`
//! (docs/microkernel/M5-LIFT.md §6): the region, the first image in it, the
//! partition's job, and the handles of MICROKERNEL.md §5, through a boot page.

use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_native_abi::lift::{
    BOOT_MAGIC, BootPage, COMMAND_LINE_AT, COMMAND_LINE_MOST, DEVICES_AT, DEVICES_MOST, DeviceFact,
};
use ferrix_native_abi::restricted::SHARED_BASE;
use ferrix_native_abi::rights::Rights;
use ferrix_vma::VmaFlags;

use crate::console::println;
use crate::object::Object;
use crate::object::bare::Bare;
use crate::object::job;
use crate::object::process::Host;
use crate::object::region::Region;
use crate::syscall::uaccess;
use crate::user::space::AddressSpace;
use crate::user::vmo::Vmo;

/// Where linuxd's image goes, in the region.
const IMAGE_BASE: u64 = SHARED_BASE + (1 << 40);
/// The boot page, read only to it.
const BOOT_PAGE: u64 = SHARED_BASE + (3 << 40);
/// linuxd's path in the initramfs (`xtask`, `native::LINUXD`).
const PATH: &[u8] = b"sbin/linuxd";

/// Start linuxd, or say why not. Its end is the kernel's end: `power::finish`.
pub fn run(view: &ferrix_bootinfo::BootView<'_>) {
    run_with(view, None);
}

/// [`run`], with `devmgr` when something started it first: what the boot
/// authority hands over (`docs/microkernel/BOOT-AUTHORITY.md`).
pub(crate) fn run_with(
    view: &ferrix_bootinfo::BootView<'_>,
    devmgr: Option<crate::discovery::devmgr::Detached>,
) {
    if !matches!(crate::mm::keep_initrd(view), Ok(Some(_))) {
        println!("  linuxd   did not start: no initramfs");
        return;
    }
    if let Err(why) = start(view.wall_clock(), view.cmdline(), devmgr) {
        println!("  linuxd   did not start: {why}");
    }
}

/// What the enumeration read, for linuxd's sysfs: no handle, no address.
fn write_devices(page: &Arc<Vmo>) -> Result<u32, &'static str> {
    let mut written = 0_u32;
    let mut dropped = 0_u32;
    for node in crate::device::devices() {
        let (crate::device::Location::Pci(address), Some(function)) =
            (node.location(), node.pci_function())
        else {
            continue;
        };
        if written as usize == DEVICES_MOST {
            dropped += 1;
            continue;
        }
        let fact = DeviceFact {
            location: u32::from(address.segment()) << 16
                | u32::from(address.bus()) << 8
                | u32::from(address.device()) << 3
                | u32::from(address.function()),
            vendor: function.vendor,
            device: function.device,
            class: function.class,
            subsystem_vendor: function.subsystem_vendor,
            subsystem: function.subsystem,
            revision: function.revision,
            secondary_bus: function.secondary_bus.unwrap_or(0xff),
        };
        let at = DEVICES_AT + written as usize * DeviceFact::BYTES;
        page.commit_and_write(0, at, &fact.to_bytes())
            .map_err(|_| "the boot page")?;
        written += 1;
    }
    if dropped != 0 {
        println!(
            "  linuxd   {dropped} devices past what its boot page holds: sysfs does not show them"
        );
    }
    Ok(written)
}

fn start(
    wall_clock: u64,
    cmdline: &str,
    devmgr: Option<crate::discovery::devmgr::Detached>,
) -> Result<(), &'static str> {
    // An option past the room would just not apply, silently: not started, said why.
    if cmdline.len() > COMMAND_LINE_MOST {
        return Err("the command line is longer than the boot page holds");
    }
    let partition = job::root()
        .new_child()
        .map_err(|_| "no job for the partition")?;
    let region = Region::new(Arc::clone(&partition)).map_err(|_| "no memory for a region")?;
    let entry =
        crate::mm::with_initrd(|archive| load(region.space(), archive)).ok_or("no initramfs")??;
    let (archive, archive_len) = archive_vmo()?;
    let _ = crate::mm::give_back_initrd();
    let page = Vmo::new_anonymous(1).map_err(|_| "no memory for the boot page")?;
    let (devmgr_messages, cores) = match devmgr {
        Some(given) => (
            give(&region, Object::Channel(given.messages), Rights::CHANNEL)?,
            give(&region, Object::Channel(given.cores), Rights::CHANNEL)?,
        ),
        None => (0, 0),
    };
    let boot = BootPage {
        devmgr: devmgr_messages,
        cores,
        magic: BOOT_MAGIC,
        region: give(&region, Object::Region(Arc::clone(&region)), Rights::REGION)?,
        job: give(&region, Object::Job(Arc::clone(&partition)), Rights::JOB)?,
        initramfs: give(
            &region,
            Object::Vmo(archive),
            Rights(Rights::VMO.0 & !Rights::WRITE.0),
        )?,
        initramfs_bytes: archive_len,
        command_line_bytes: cmdline.len() as u32,
        processors: crate::smp::count() as u32,
        devices: write_devices(&page)?,
        debug_log: give(&region, Object::DebugLog, Rights::WRITE)?,
        console: give(&region, Object::Console, Rights::READ)?,
        memory_frames: crate::mm::free_frames() / 8 * 7,
        wall_clock,
        ..BootPage::EMPTY
    };
    page.commit_and_write(0, 0, &boot.to_bytes())
        .map_err(|_| "the boot page")?;
    let line = cmdline.as_bytes();
    let line = line.get(..boot.command_line_bytes as usize).unwrap_or(line);
    page.commit_and_write(0, COMMAND_LINE_AT, line)
        .map_err(|_| "the boot page")?;
    let _ = region
        .space()
        .map_object(
            Some(BOOT_PAGE),
            PAGE_SIZE,
            Arc::clone(&page),
            0,
            VmaFlags::READ,
        )
        .map_err(|_| "the boot page's mapping")?;

    let restricted = AddressSpace::new().map_err(|_| "no memory for a space")?;
    restricted
        .bind_region(region.space(), partition.quota_index())
        .map_err(|_| "bind")?;
    region.enter().map_err(|_| "the region ended")?;
    let host = Bare::new(restricted, partition, Some(Arc::clone(&region)))
        .map_err(|_| "no memory for the process")?;
    host.start(entry.entry, entry.stack, BOOT_PAGE)?;
    println!(
        "  linuxd   started at {:#x}, pid {}",
        entry.entry,
        host.core().pid()
    );
    let core = host.core();
    let _ = core.exited().wait_trusting(|| core.is_terminated());
    println!("  linuxd   ended with {:?}", core.exit_status());
    // What `reboot(2)` asked of the machine, which only the kernel can do. Otherwise
    // init ended, and the kernel's own rule for that follows (`power::finish`).
    use ferrix_bootinfo::pstore::Stop;
    use ferrix_native_abi::lift::{EXIT_HALT, EXIT_POWER_OFF, EXIT_RESTART};
    match core.exit_status() {
        Some(EXIT_RESTART) => {
            crate::audit::power(crate::audit::Subject::KERNEL, ferrix_audit::power::RESTART);
            let _ = crate::console::pstore::stop(Stop::Restart);
            crate::arch::reset();
        }
        Some(EXIT_POWER_OFF) => {
            crate::audit::power(crate::audit::Subject::KERNEL, ferrix_audit::power::OFF);
            let _ = crate::console::pstore::stop(Stop::PowerOff);
            crate::arch::power_off();
        }
        Some(EXIT_HALT) => {
            crate::audit::power(crate::audit::Subject::KERNEL, ferrix_audit::power::HALT);
            let _ = crate::console::pstore::stop(Stop::Halt);
            crate::arch::shutdown();
        }
        _ => {}
    }
    Ok(())
}

fn give(region: &Region, object: Object, rights: Rights) -> Result<u32, &'static str> {
    region
        // NOALLOC: `insert` reserves room fallibly and answers `Err` without it.
        .with_handles(|table| table.insert(object, rights))
        .map(|handle| handle.0)
        .map_err(|_| "the shared table")
}

/// The archive, as a read-only VMO: copied once.
fn archive_vmo() -> Result<(Arc<Vmo>, u64), &'static str> {
    crate::mm::with_initrd(|archive| {
        let pages = (archive.len() as u64).div_ceil(PAGE_SIZE);
        let vmo = Vmo::new_anonymous(pages).map_err(|_| "no memory for the initramfs")?;
        for (index, chunk) in archive.chunks(PAGE_SIZE as usize).enumerate() {
            vmo.commit_and_write(index as u64, 0, chunk)
                .map_err(|_| "the initramfs's copy")?;
        }
        Ok((vmo, archive.len() as u64))
    })
    .ok_or("no initramfs")?
}

/// linuxd's image out of `archive` and into `space`.
fn load(
    space: &Arc<AddressSpace>,
    archive: &[u8],
) -> Result<crate::syscall::native::EntryPoint, &'static str> {
    let image = ferrix_vfs::initramfs::file_named(archive, PATH).ok_or("no sbin/linuxd")?;
    crate::discovery::first_image::load_at(space, image, IMAGE_BASE).map_err(|why| {
        println!("  linuxd   image refused: {why:?}");
        "linuxd's image was refused"
    })
}
