//! linuxd: the Linux personality as a user-space program
//! (`docs/microkernel/M5-LIFT.md`).
//!
//! The load's one source, `ferrix-kernel-load`'s root, built against a shim of
//! the kernel's core and item, so a Linux program's system call is a restricted
//! exit into this code and a call through the load, as it is a trap and a call
//! through the load in the hybrid kernel.

#![no_std]
#![no_main]

extern crate alloc;

mod arch;

use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_kernel_core::heap::Heap;
use ferrix_kernel_core::{console, mm, restricted, sched, sys};
use ferrix_kernel_item::init;
use ferrix_kernel_load::{fs, interfaces, syscall};
use ferrix_native::handle::OwnedHandle;
use ferrix_native::vmo::{self, Vmo};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::lift::{BOOT_MAGIC, BootPage, COMMAND_LINE_AT};
use ferrix_rt::Kernel;

#[cfg(ferrix_runtime_settings = "live")]
ferrix_kernel_load::bind_setters!(ferrix_kernel_settings::Live);
#[cfg(not(ferrix_runtime_settings = "live"))]
ferrix_kernel_load::bind_setters!(ferrix_kernel_load::setters::Absent);

#[global_allocator]
static HEAP: Heap = Heap::new();

/// What the kernel handed over: the boot page's address.
static BOOT: AtomicU64 = AtomicU64::new(0);

/// The first thread's first Rust frame: `rt`'s `_start` calls it with the first
/// argument register, the boot page.
/// linuxd's dispatch by name: see `restricted`'s declarations.
#[unsafe(no_mangle)]
fn ferrix_lift_dispatch(
    args: &ferrix_kernel_core::trap::SyscallArgs,
    regs: Option<&ferrix_kernel_core::arch::UserRegs>,
) -> ferrix_kernel_core::trap::Outcome {
    syscall::dispatch_with::<syscall::linux::Linux>(args, regs)
}

/// As above.
#[unsafe(no_mangle)]
fn ferrix_lift_attention() -> bool {
    syscall::deliver::needs_attention()
}

/// As above.
#[unsafe(no_mangle)]
fn ferrix_lift_quiet() -> bool {
    syscall::seccomp::quiet()
}

#[unsafe(no_mangle)]
extern "C" fn ferrix_rt_start(boot: usize) -> ! {
    BOOT.store(boot as u64, Ordering::Release);
    // SAFETY: the kernel mapped a boot page here, and it stays.
    let page = unsafe { &*(boot as *const BootPage) };
    if page.magic != BOOT_MAGIC {
        ferrix_rt::exit(2);
    }
    sys::set_debug_handle(page.debug_log);
    ferrix_rt::set_panic_writer(|line| {
        sys::debug_write(line.as_bytes());
        sys::debug_write(b"\n");
    });
    sys::debug_write(b"  linuxd   up\n");
    if HEAP.init().is_err() {
        sys::debug_write(b"  linuxd   no heap\n");
        ferrix_rt::exit(3);
    }
    if let Err(why) = arena() {
        console::write_bytes(why.as_bytes());
        ferrix_rt::exit(4);
    }
    let Ok(top) = sched::first_stack(served) else {
        ferrix_rt::exit(5);
    };
    // SAFETY: `top` is the top of a stack just allocated for this thread, whose
    // task names `served`. Nothing on the old stack is used again.
    unsafe { arch::onto_stack(top, on_its_stack) }
}

extern "C" fn on_its_stack() -> ! {
    sched::on_task_stacks();
    served(0);
    sched::exit_leaving()
}

/// The arena (`mm`): one VMO, and a handle that may map it executable.
fn arena() -> Result<(), &'static str> {
    const ARENA_BYTES: usize = 4 << 30;
    // With room above it, for the pages of VMOs drivers hand over (`bridge`).
    const ARENA_AT: usize = 0x7400_0000_0000;
    let writable = vmo::create(Kernel, ARENA_BYTES).map_err(|_| "  linuxd   no arena\n")?;
    let at = writable
        .map(
            Some(ARENA_AT),
            ARENA_BYTES,
            ferrix_native::pending::Protection::ReadWrite,
            0,
        )
        .map_err(|_| "  linuxd   no arena's mapping\n")?;
    let copy = writable
        .duplicate(Requested::Same)
        .map_err(|_| "  linuxd   no arena's second handle\n")?;
    let executable = copy
        .executable()
        .map_err(|_| "  linuxd   no arena's execute right\n")?;
    // Linuxd's alone, for good: neither can be copied or handed to anyone, a program's
    // private table above all (ledger 6012, R5).
    use ferrix_native_abi::rights::{Requested, Rights};
    let writable = Vmo::from_owned(
        writable
            .into_owned()
            .replace(Requested::Exactly(
                Rights::READ | Rights::WRITE | Rights::MAP,
            ))
            .map_err(|_| "  linuxd   the arena's rights\n")?,
    );
    let executable = Vmo::from_owned(
        executable
            .into_owned()
            .replace(Requested::Exactly(
                Rights::READ | Rights::MAP | Rights::EXECUTE,
            ))
            .map_err(|_| "  linuxd   the arena's execute rights\n")?,
    );
    use ferrix_native::handle::Object as _;
    sys::arena::set(writable.handle().0, executable.handle().0);
    #[expect(
        clippy::mem_forget,
        reason = "kept mapped for the program's life: its handle stays open"
    )]
    core::mem::forget(writable);
    #[expect(
        clippy::mem_forget,
        reason = "kept mapped for the program's life: its handle stays open"
    )]
    core::mem::forget(executable);
    // SAFETY: the boot page, as in `ferrix_rt_start`.
    let page = unsafe { &*(BOOT.load(Ordering::Acquire) as *const BootPage) };
    mm::init(at as u64, page.memory_frames);
    HEAP.frames_ready();
    Ok(())
}

#[expect(
    clippy::too_many_lines,
    reason = "linuxd's bring-up is one order of steps, as the kernel's `kmain`"
)]
fn served(_argument: usize) {
    // SAFETY: the kernel mapped a boot page here, and it stays.
    let page = unsafe { &*(BOOT.load(Ordering::Acquire) as *const BootPage) };
    let initramfs = Vmo::from_owned(OwnedHandle::from_raw(Kernel, Handle(page.initramfs)));
    let whole = (page.initramfs_bytes as usize).next_multiple_of(4096);
    let Ok(at) = initramfs.map(None, whole, ferrix_native::pending::Protection::Read, 0) else {
        console::write_bytes(b"  linuxd   the initramfs could not be mapped\n");
        return;
    };
    mm::set_initrd(at as u64, page.initramfs_bytes);
    #[expect(
        clippy::mem_forget,
        reason = "kept mapped for the program's life: its handle stays open"
    )]
    core::mem::forget(initramfs);
    sys::set_partition(page.region, page.job);
    sys::set_console_handle(page.console);
    ferrix_kernel_core::smp::set_processors(page.processors as usize);
    match ferrix_kernel_core::lift::core::calls_check::run(
        Handle(page.job),
        Handle(sys::arena::writable_handle()),
        Handle(sys::arena::executable_handle()),
    ) {
        Ok(held) => console::write(format_args!("  lift-calls {held} refusals held\n")),
        Err(why) => console::write(format_args!("  lift-calls FAILED: {why}\n")),
    }
    sched::account::start_keeper();
    // SAFETY: mapped just above, read only, and it stays.
    ferrix_kernel_item::discovery::devmgr::read_manifest(unsafe {
        core::slice::from_raw_parts(at as *const u8, page.initramfs_bytes as usize)
    });
    ferrix_kernel_core::device::set_devices((0..page.devices as usize).map(|index| {
        let start = core::ptr::from_ref(page).cast::<u8>().wrapping_add(
            ferrix_native_abi::lift::DEVICES_AT
                + index * ferrix_native_abi::lift::DeviceFact::BYTES,
        );
        // SAFETY: inside the boot page, which the kernel mapped here and stays.
        let bytes = unsafe { &*start.cast::<[u8; ferrix_native_abi::lift::DeviceFact::BYTES]>() };
        ferrix_native_abi::lift::DeviceFact::from_bytes(bytes)
    }));
    restricted::set_dispatch(syscall::dispatch_with::<syscall::linux::Linux>);
    restricted::set_quiet(syscall::seccomp::quiet);
    ferrix_kernel_core::trap::set_quiet_syscall_filter(syscall::seccomp::check);
    syscall::deliver::install();
    if syscall::launch::install()
        .and_then(|()| fs::install())
        .is_err()
    {
        console::write_bytes(b"  linuxd   a registration failed\n");
        return;
    }
    // SAFETY: a hand-off record of integers, read as zeros: nothing to report.
    let info =
        unsafe { core::mem::MaybeUninit::<ferrix_bootinfo::BootInfo>::zeroed().assume_init() };
    let start = core::ptr::from_ref(page)
        .cast::<u8>()
        .wrapping_add(COMMAND_LINE_AT);
    // SAFETY: the kernel wrote this many bytes of the boot page, from a `str`.
    let command_line =
        unsafe { core::slice::from_raw_parts(start, page.command_line_bytes as usize) };
    // SAFETY: from a `str`, as above.
    let command_line = unsafe { core::str::from_utf8_unchecked(command_line) };
    let view = ferrix_bootinfo::BootView::from_parts(&info, &[], command_line);
    // HTTPS needs the time of day, and so does anything that checks a certificate.
    if page.wall_clock != 0 {
        syscall::time::set_realtime(page.wall_clock);
    }
    init::read_option(&view);
    fs::btrfs_powerfail::init(&view);
    fs::root_disk::init(&view);
    fs::procfs::remember_command_line(command_line);
    match fs::init(&view) {
        Ok(_) => console::write_bytes(b"  linuxd   root built\n"),
        Err(why) => {
            console::write(format_args!(
                "  linuxd   the root could not be built: {why:?}\n"
            ));
            return;
        }
    }
    // The disks, once devmgr's drivers have come up. The cores are its by the
    // channel the kernel gave both (M5-LIFT.md §11).
    let cored = fs::start_block_side(page.files)
        .and_then(|()| interfaces::net_ring::install())
        .and_then(|()| interfaces::display::install())
        .and_then(|()| interfaces::render::install())
        .and_then(|()| interfaces::input::install())
        .and_then(|()| interfaces::audio::install())
        .and_then(|()| interfaces::chardev::install())
        .and_then(|()| interfaces::logctl::install());
    if cored.is_err() {
        console::write_bytes(b"  linuxd   a core's registration failed\n");
        return;
    }
    // What drives the stack's clock: ARP retries, TCP's timers. Nothing else would.
    if ferrix_kernel_load::net::start().is_err() {
        console::write_bytes(b"  linuxd   no task for the network's clock\n");
    }
    // A further partition has this and not the rest: its interface is `netd`'s to give.
    ferrix_kernel_item::cores::start_for_netd(page.net_cores as u32);
    if page.cores != 0 {
        ferrix_kernel_item::cores::start(page.cores, page.devmgr);
        let patience = ferrix_kernel_core::timer::now_nanos().saturating_add(20_000_000_000);
        if !ferrix_kernel_item::discovery::devmgr::wait_reported(patience) {
            console::write_bytes(b"  linuxd   devmgr did not report\n");
        }
        fs::settle_block_side();
        // test-powerfail's two boots: what `check_btrfs_write` does in a hybrid.
        {
            use fs::btrfs_powerfail::{self as powerfail, Mode};
            let (result, verb) = match powerfail::mode() {
                Mode::Check => (None, ""),
                Mode::Churn => (Some(powerfail::churn(powerfail::seed())), "churned"),
                Mode::Replay => (Some(powerfail::replay()), "replayed"),
                Mode::Keep => (Some(powerfail::keep()), "kept"),
            };
            match result {
                Some(Ok(report)) if report.skipped.is_some() => {
                    console::write(format_args!(
                        "  btrfs-pf not run: {}\n",
                        report.skipped.unwrap_or_default()
                    ));
                }
                Some(Ok(report)) => console::write(format_args!(
                    "  btrfs-pf vdc {verb}: {} files, {} with a trailer whose promise held; {}\n",
                    report.files,
                    report.checked,
                    if report.logged {
                        "the crash left a log, and the mount replayed it"
                    } else {
                        "the crash left no log"
                    },
                )),
                Some(Err(problem)) => {
                    console::write(format_args!(
                        "  btrfs-pf power-fail check failed: {problem}\n"
                    ));
                    ferrix_rt::exit(1);
                }
                None => {}
            }
        }
        match fs::btrfs_write_check::mount_fixture() {
            Ok(true) => console::write_bytes(b"  btrfs    the fixture is mounted at /mnt\n"),
            Ok(false) => {}
            Err(why) => console::write(format_args!("  btrfs    {why}\n")),
        }
        match fs::btrfs_write_check::run() {
            Ok(report) => match report.skipped {
                Some(why) => console::write(format_args!("  btrfs-rw not checked: {why}\n")),
                None => console::write(format_args!(
                    "  btrfs-rw vdc written and remounted: {} files ({} bytes) and {} directories read back as they were written\n",
                    report.files, report.bytes, report.directories,
                )),
            },
            Err(why) => console::write(format_args!("  btrfs-rw {why}\n")),
        }
        fs::root_disk::switch();
        fs::data_disk::mount();
        fs::home_disk::mount();
    }
    console::write_bytes(b"  linuxd   booted\n");
    // On a thread of its own: `reboot(2)` from anywhere ends the partition under it.
    if sched::spawn(
        "init",
        |_| {
            init::run();
            console::write_bytes(b"  linuxd   init returned\n");
            sys::end_partition();
        },
        0,
        0,
    )
    .is_err()
    {
        console::write_bytes(b"  linuxd   no thread for init\n");
        return;
    }
    let status = sys::wait_for_the_end();
    ferrix_kernel_item::power::sync_disks();
    ferrix_rt::exit(status);
}
