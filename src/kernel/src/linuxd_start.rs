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
use crate::first_rows::{FURTHER, Held, LINUXD};
use crate::object::Object;
use crate::object::bare::Bare;
use crate::object::job;
use crate::object::process::Host;
#[cfg(ferrix_kernel_memory = "retyped")]
use crate::object::quota;
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

/// A further Linux partition (`docs/microkernel/M8.md` §3): less than the first, which
/// alone holds the device cores, the file server's channel, the console and `devmgr`'s
/// channels. It has an initramfs, a debug log, and its memory.
#[derive(Debug, Clone)]
pub(crate) struct Further {
    image: [u8; 64],
    frames: u64,
    /// Its end of what `netd` opens its ring on.
    net_cores: Arc<crate::object::channel::Endpoint>,
}

impl Further {
    /// A partition running `image`, with `frames` of memory.
    pub(crate) fn of(
        image: &[u8],
        frames: u64,
        net_cores: Arc<crate::object::channel::Endpoint>,
    ) -> Option<Further> {
        let mut stored = [0_u8; 64];
        stored.get_mut(..image.len())?.copy_from_slice(image);
        Some(Further {
            image: stored,
            frames,
            net_cores,
        })
    }

    fn image(&self) -> &[u8] {
        let end = self.image.iter().position(|&byte| byte == 0).unwrap_or(64);
        self.image.get(..end).unwrap_or(&[])
    }
}

/// Start linuxd, or say why not. Its end is the kernel's end: `power::finish`.
pub(crate) fn run(view: &ferrix_bootinfo::BootView<'_>) {
    run_with(view, None, None, &[]);
}

/// [`run`], with `devmgr` when something started it first: what the boot
/// authority hands over (`docs/microkernel/BOOT-AUTHORITY.md`).
pub(crate) fn run_with(
    view: &ferrix_bootinfo::BootView<'_>,
    devmgr: Option<crate::discovery::devmgr::Detached>,
    files: Option<Arc<crate::object::channel::Endpoint>>,
    further: &[Further],
) {
    if !matches!(crate::mm::keep_initrd(view), Ok(Some(_))) {
        println!("  linuxd   did not start: no initramfs");
        return;
    }
    if let Err(why) = start(view.wall_clock(), view.cmdline(), devmgr, files, further) {
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

/// K16 §4: the partition's memory is a promise, its kernel objects and its arena alike.
/// What is left over is the pool's, the watermark's and the kernel's own; a further
/// partition's is its own row's, and is taken out first. Answers the frames promised.
#[cfg(ferrix_kernel_memory = "retyped")]
fn promise_for_partition(partition: &job::Job) -> Result<u64, &'static str> {
    // What can be promised at all, which the allocator knows and says: an eighth stays
    // for the pool and what the kernel asks at a job's expense, and further partition
    // rows have been promised by their own starts, above.
    let mut promised = crate::mm::free_frames();
    while let Err(quota::ReserveError::Short(short)) =
        partition.set_reservation(promised * PAGE_SIZE)
    {
        // `Short` names a shortfall of at least a frame, or this would not end: a zero
        // is refused here rather than spun on at boot.
        if short == 0 {
            return Err("a reservation short by nothing");
        }
        promised = promised.saturating_sub(short);
    }
    promised = promised / 8 * 7;
    partition
        .set_reservation(promised * PAGE_SIZE)
        .map_err(|why| {
            println!("  linuxd   no reservation of {promised} frames: {why:?}");
            "no reservation for the partition"
        })?;
    Ok(promised)
}

/// What linuxd's boot page says, its handles given as they are named.
fn boot_page(
    region: &Arc<Region>,
    partition: &Arc<job::Job>,
    page: &Arc<Vmo>,
    (archive, archive_len): (Arc<Vmo>, u64),
    cmdline: &str,
    (devmgr, files): (
        Option<crate::discovery::devmgr::Detached>,
        Option<Arc<crate::object::channel::Endpoint>>,
    ),
    (memory_frames, wall_clock): (u64, u64),
) -> Result<BootPage, &'static str> {
    let (devmgr_messages, cores, net_cores) = match devmgr {
        Some(given) => (
            give(
                region,
                Object::Channel(given.messages),
                const { LINUXD.of(Held::Channel) },
            )?,
            give(
                region,
                Object::Channel(given.cores),
                const { LINUXD.of(Held::Channel) },
            )?,
            give(
                region,
                Object::Channel(given.net_cores),
                const { LINUXD.of(Held::Channel) },
            )?,
        ),
        None => (0, 0, 0),
    };
    let files = match files {
        Some(end) => give(
            region,
            Object::Channel(end),
            const { LINUXD.of(Held::Channel) },
        )?,
        None => 0,
    };
    Ok(BootPage {
        devmgr: devmgr_messages,
        cores,
        files,
        net_cores: u64::from(net_cores),
        magic: BOOT_MAGIC,
        region: give(
            region,
            Object::Region(Arc::clone(region)),
            const { LINUXD.of(Held::Region) },
        )?,
        job: give(
            region,
            Object::Job(Arc::clone(partition)),
            const { LINUXD.of(Held::Job) },
        )?,
        initramfs: give(
            region,
            Object::Vmo(archive),
            const { LINUXD.of(Held::Archive) },
        )?,
        initramfs_bytes: archive_len,
        command_line_bytes: cmdline.len() as u32,
        processors: crate::smp::count() as u32,
        devices: u64::from(write_devices(page)?),
        debug_log: give(
            region,
            Object::DebugLog,
            const { LINUXD.of(Held::DebugLog) },
        )?,
        console: give(region, Object::Console, const { LINUXD.of(Held::Console) })?,
        memory_frames,
        wall_clock,
    })
}

/// Write the boot page and the command line after it, and map it read-only.
fn map_boot_page(
    region: &Region,
    page: &Arc<Vmo>,
    boot: &BootPage,
    cmdline: &str,
) -> Result<(), &'static str> {
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
            Arc::clone(page),
            0,
            VmaFlags::READ,
        )
        .map_err(|_| "the boot page's mapping")?;
    Ok(())
}

fn start(
    wall_clock: u64,
    cmdline: &str,
    devmgr: Option<crate::discovery::devmgr::Detached>,
    files: Option<Arc<crate::object::channel::Endpoint>>,
    further: &[Further],
) -> Result<(), &'static str> {
    // An option past the room would just not apply, silently: not started, said why.
    if cmdline.len() > COMMAND_LINE_MOST {
        return Err("the command line is longer than the boot page holds");
    }
    // Before any partition's: the kernel's own copy.
    let (archive, archive_len) = archive_vmo()?;
    // Each partition one speculation domain, so a switch between them is a barrier
    // (ledger 6226 N.10); with one partition this changes nothing it can see.
    let partition = job::root()
        .new_child_as(!further.is_empty(), false)
        .map_err(|_| "no job for the partition")?;
    for (index, row) in further.iter().enumerate() {
        if let Err(why) = start_further(cmdline, index + 1, row, &archive, archive_len) {
            println!("  linuxd {} did not start: {why}", index + 1);
        }
    }
    #[cfg(ferrix_kernel_memory = "retyped")]
    let promised = promise_for_partition(&partition)?;
    if !further.is_empty() {
        println!(
            "  linuxd   the first partition's speculation domain: {}",
            partition.domain()
        );
    }
    let region = Region::new(Arc::clone(&partition)).map_err(|_| "no memory for a region")?;
    let entry = crate::mm::with_initrd(|archive| load(region.space(), archive, PATH))
        .ok_or("no initramfs")??;
    let _ = crate::mm::give_back_initrd();
    let page = Vmo::new_anonymous(1).map_err(|_| "no memory for the boot page")?;
    #[cfg(ferrix_kernel_memory = "retyped")]
    let memory_frames = promised / 8 * 7;
    #[cfg(not(ferrix_kernel_memory = "retyped"))]
    let memory_frames = crate::mm::free_frames() / 8 * 7;
    let boot = boot_page(
        &region,
        &partition,
        &page,
        (archive, archive_len),
        cmdline,
        (devmgr, files),
        (memory_frames, wall_clock),
    )?;
    judge_table(&region, &LINUXD);
    map_boot_page(&region, &page, &boot, cmdline)?;

    let restricted = AddressSpace::new().map_err(|_| "no memory for a space")?;
    restricted
        .bind_region(region.space(), partition.quota_index())
        .map_err(|_| "bind")?;
    region.enter().map_err(|_| "the region ended")?;
    let partition_of_linuxd = Arc::clone(&partition);
    let host = Bare::new(restricted, partition, Some(Arc::clone(&region)))
        .map_err(|_| "no memory for the process")?;
    #[cfg(ferrix_kernel_memory = "retyped")]
    {
        crate::mm::seal::note_payer(&partition_of_linuxd);
        crate::boot_authority::seal();
    }
    host.start(entry.entry, entry.stack, BOOT_PAGE)?;
    println!(
        "  linuxd   started at {:#x}, pid {}",
        entry.entry,
        host.core().pid()
    );
    let core = host.core();
    let _ = core.exited().wait_trusting(|| core.is_terminated());
    crate::audit::first_process_ended(core);
    println!("  linuxd   ended with {:?}", core.exit_status());
    #[cfg(ferrix_kernel_memory = "retyped")]
    crate::mm::seal::report();
    // Its programs go with it (L.object.327): what is left of the `linux` job has
    // nobody to answer its system calls.
    let killed = partition_of_linuxd.kill(137).unwrap_or(0);
    println!("  linuxd   its job killed: {killed} processes left");
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

/// Hold a region's whole table to its row, before anything runs in it; a miss stops the
/// boot, not this one partition (L.object.321).
fn judge_table(region: &Region, row: &crate::first_rows::Rows) {
    let judged = region.with_handles(|table| {
        row.judge(table.handles().filter_map(|(handle, rights)| {
            table.get(handle).ok().map(|(object, _)| (object, rights))
        }))
    });
    if let Err(why) = judged {
        crate::panic::fatal!(
            crate::panic::catalog::FIRST_PROCESSES,
            "first processes: {why}"
        );
    }
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

/// A further partition: started now, and watched by a task of its own, since its end
/// is not the kernel's.
#[expect(
    clippy::too_many_lines,
    reason = "one partition's start, its boot page's rows in one place"
)]
fn start_further(
    cmdline: &str,
    index: usize,
    row: &Further,
    archive: &Arc<Vmo>,
    archive_len: u64,
) -> Result<(), &'static str> {
    use core::fmt::Write as _;
    let partition = job::root()
        .new_child_as(true, false)
        .map_err(|_| "no job for it")?;
    // K16 §4: this partition's memory is a promise, as the first's is.
    #[cfg(ferrix_kernel_memory = "retyped")]
    partition
        .set_reservation(row.frames * PAGE_SIZE)
        .map_err(|why| {
            println!("  linuxd {index} no reservation: {why:?}");
            "no reservation for it"
        })?;
    println!(
        "  linuxd {index} speculation domain: {}",
        partition.domain()
    );
    let region = Region::new(Arc::clone(&partition)).map_err(|_| "no memory for a region")?;
    let entry = crate::mm::with_initrd(|file| load(region.space(), file, row.image()))
        .ok_or("no initramfs")??;
    let page = Vmo::new_anonymous(1).map_err(|_| "no memory for the boot page")?;
    let mut line = alloc::string::String::new();
    let _ = write!(line, "{cmdline} ferrix.partition={index}");
    let boot = BootPage {
        magic: BOOT_MAGIC,
        region: give(
            &region,
            Object::Region(Arc::clone(&region)),
            const { FURTHER.of(Held::Region) },
        )?,
        job: give(
            &region,
            Object::Job(Arc::clone(&partition)),
            const { FURTHER.of(Held::Job) },
        )?,
        initramfs: give(
            &region,
            Object::Vmo(Arc::clone(archive)),
            const { FURTHER.of(Held::Archive) },
        )?,
        initramfs_bytes: archive_len,
        command_line_bytes: line.len().min(COMMAND_LINE_MOST) as u32,
        processors: crate::smp::count() as u32,
        debug_log: give(
            &region,
            Object::DebugLog,
            const { FURTHER.of(Held::DebugLog) },
        )?,
        memory_frames: row.frames,
        net_cores: u64::from(give(
            &region,
            Object::Channel(Arc::clone(&row.net_cores)),
            const { FURTHER.of(Held::Channel) },
        )?),
        ..BootPage::EMPTY
    };
    judge_table(&region, &FURTHER);
    page.commit_and_write(0, 0, &boot.to_bytes())
        .map_err(|_| "the boot page")?;
    let bytes = line.as_bytes();
    let bytes = bytes
        .get(..boot.command_line_bytes as usize)
        .unwrap_or(bytes);
    page.commit_and_write(0, COMMAND_LINE_AT, bytes)
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
    let host = Bare::new(
        restricted,
        Arc::clone(&partition),
        Some(Arc::clone(&region)),
    )
    .map_err(|_| "no memory for the process")?;
    host.start(entry.entry, entry.stack, BOOT_PAGE)?;
    println!(
        "  linuxd {index} started at {:#x}, pid {}",
        entry.entry,
        host.core().pid()
    );
    /// Each further partition's, until its watching task takes one.
    type Watched = (Arc<Bare>, Arc<job::Job>, usize);
    static WATCHED: crate::sync::SpinLock<alloc::vec::Vec<Watched>> =
        crate::sync::SpinLock::new(alloc::vec::Vec::new());
    crate::fallible::try_push(&mut WATCHED.lock(), (host, partition, index))
        .map_err(|_| "no memory to watch it")?;
    fn watch(_argument: usize) {
        let Some((host, partition, index)) = WATCHED.lock().pop() else {
            return;
        };
        let core = host.core();
        let _ = core.exited().wait_trusting(|| core.is_terminated());
        crate::audit::first_process_ended(core);
        let killed = partition.kill(137).unwrap_or(0);
        println!(
            "  linuxd {index} ended with {:?}: its job killed, {killed} processes left",
            core.exit_status()
        );
    }
    crate::sched::spawn("partition watch", watch, 0, ferrix_sched::NICE_0_WEIGHT)
        .map(|_| ())
        .map_err(|_| "no task to watch it")
}

/// linuxd's image out of `archive` and into `space`.
fn load(
    space: &Arc<AddressSpace>,
    archive: &[u8],
    path: &[u8],
) -> Result<crate::syscall::native::EntryPoint, &'static str> {
    let image = ferrix_cpio::file_named(archive, path).ok_or("no such image")?;
    crate::discovery::first_image::load_at(space, image, IMAGE_BASE).map_err(|why| {
        println!("  linuxd   image refused: {why:?}");
        "linuxd's image was refused"
    })
}
