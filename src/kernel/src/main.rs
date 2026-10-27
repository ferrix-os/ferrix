//! The Ferrix kernel.
//!
//! Entered from the UEFI loader with the MMU on, three mappings in place and
//! nothing else: no interrupt vectors, no allocator, no other CPU running. What
//! stage 1 does with that is prove the hand-off is sound and say so over the
//! serial port, which is the smallest thing that can honestly be called booting.
//!
//! See `docs/ROADMAP.md` for what comes next.

#![no_std]
#![no_main]
// A kernel without the Linux ABI is build-only and carries no claim
// (LINUX-ABI-SEPARATION.md §11.11): what only the Linux ABI and the checks
// that run a Linux program used is left unused in it, about 750 items, until
// those checks are re-homed onto native test programs (§7 row 13, ledger 3208
// (6) b), and the expectations of dead code those items carry go
// unfulfilled. The library crates only the Linux ABI reaches stay ordinary
// dependencies, unused here, and are not linked into the image (ledger 3220
// (14) a). Never in a build with `linux_abi=on`, which every profile sets.
#![cfg_attr(
    ferrix_linux_abi = "off",
    allow(
        dead_code,
        unused_imports,
        unused_variables,
        unfulfilled_lint_expectations,
        unused_crate_dependencies,
        reason = "linux_abi=off is build-only until the checks that run a Linux program are re-homed"
    )
)]
// A `runtime_settings = "fixed"` build names the removable crate
// `ferrix-kernel-settings` nowhere, so that rustc never loads it and none of
// it is linked (docs/KERNEL-CRATES.md §6; ledger 3558 (10), 3623 (R1), (R3)):
// its manifest lists it in every build, and only this attribute's build
// leaves it unused. A `live` build keeps the lint.
#![cfg_attr(
    ferrix_runtime_settings = "fixed",
    expect(
        unused_crate_dependencies,
        reason = "runtime_settings=fixed leaves ferrix-kernel-settings out of the link"
    )
)]

extern crate alloc;

use ferrix_kernel_load::*;
// Named, not only reached through the glob above, so that each crate's word
// is named by the binary (`kernel-interface.py`).
use ferrix_kernel_load::{CORE_FLAG_WORD, ITEM_FLAG_WORD, LOAD_FLAG_WORD};

// The four crates compiled with one set of build flags (ledger line 3282,
// (K4)): the build line is the item's `cfg`s, and the binary's, the core's
// and the load's must be the same, or the image would run code its line does
// not name.
const BINARY_FLAG_WORD: u64 = flag_word(&build_flag_word!());
const _: () = assert!(
    CORE_FLAG_WORD == BINARY_FLAG_WORD,
    "ferrix-kernel-core was compiled with other build flags than the binary"
);
const _: () = assert!(
    ITEM_FLAG_WORD == BINARY_FLAG_WORD,
    "ferrix-kernel-item was compiled with other build flags than the binary"
);
const _: () = assert!(
    LOAD_FLAG_WORD == BINARY_FLAG_WORD,
    "ferrix-kernel-load was compiled with other build flags than the binary"
);
#[cfg(ferrix_runtime_settings = "live")]
const _: () = assert!(
    ferrix_kernel_settings::SETTINGS_FLAG_WORD == BINARY_FLAG_WORD,
    "ferrix-kernel-settings was compiled with other build flags than the binary"
);

// The run-time setting entries' binding (docs/KERNEL-CRATES.md §6.3; ledger
// 3558 (9), 3623, 3934): the removable crate's `Live` in a `live` build, the
// load's `Absent` in a `fixed` one, recorded in certification-item.json's
// `composition_root.removable`. Each invocation defines the load's
// `__ferrix_setters_*` symbols as calls into its type, so a build binds
// exactly one or does not link. These, and the flag word's assert above, are
// the only places the tree names `ferrix_runtime_settings` outside the flag
// lists, and the only ones that name the crate.
#[cfg(all(ferrix_linux_abi = "on", ferrix_runtime_settings = "live"))]
ferrix_kernel_load::bind_setters!(ferrix_kernel_settings::Live);
#[cfg(all(ferrix_linux_abi = "on", ferrix_runtime_settings = "fixed"))]
ferrix_kernel_load::bind_setters!(setters::Absent);

#[cfg(ferrix_linux_abi = "on")]
pub mod stages_check;

// What belongs to one system on chip rather than to an architecture: the
// kernel's part in a board's devices, each found in the device tree at boot
// and a no-op on a machine without one, grouped by the vendor prefix of the
// chip's `compatible`. The gs201 is the Pixel 7's Tensor G2; its watchdogs
// are serviced from `arch::aarch64` alone.

use ferrix_bootinfo::{BootInfo, BootView, KASLR_FIXED_IMAGE, KASLR_MOVED, MemKind, PAGE_SIZE};
use ferrix_paging::MapError;

use console::{println, println_unlogged};
use discovery::finder::Finder;
use discovery::{board, devmgr, fdt, firmware, pci, tree};
use early::EarlyMemory;
#[cfg(ferrix_linux_abi = "on")]
use interfaces::{audio, block_ring, chardev, display, input, logctl, net_ring, render};
use panic::{catalog, fatal};

/// What the boot test waits for. Changing it means changing
/// `tools/common/xtask/src/qemu.rs`, and the two are checked against each other there.
const SUCCESS_MARKER: &str = "FERRIX-BOOT-OK";

/// What a boot that skipped its self-checks prints where the success marker
/// would be (`checks` says why it is another word). Checked against
/// `tools/common/xtask/src/qemu.rs` beside the success marker.
const UNCHECKED_MARKER: &str = "FERRIX-BOOT-UNCHECKED";

/// The kernel's entry point.
///
/// The loader calls this with the boot info pointer as its only argument; the
/// signature is [`ferrix_bootinfo::KernelEntry`], declared in the crate both
/// sides share so that a mismatch is a type error rather than a triple fault.
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
extern "C" fn _start(boot_info: *const BootInfo) -> ! {
    // Nothing has been checked yet, including whether this pointer is a
    // `BootInfo` at all — so `validate` is the first thing that runs, and it
    // checks the magic before it follows anything.
    //
    // SAFETY: (BOOT-DATA) the loader passes a pointer to a `BootInfo` it built, inside the
    // direct map, immutable for the life of the system.
    let info = unsafe { &*boot_info };
    // SAFETY: (BOOT-DATA) the same structure, so its `regions` and `cmdline` point at the
    // arrays the loader wrote beside it, for as long as the system runs.
    let Ok(view) = (unsafe { info.validate() }) else {
        // No console yet, and no way to make one without a valid hand-off.
        arch::halt()
    };

    let mut memory = EarlyMemory::new(&view);
    if arch::init_console(&view, &mut memory).is_err() {
        arch::halt()
    }
    // SAFETY: (DEVICE) `init_console` configured the port and, on AArch64, mapped it.
    unsafe { console::mark_ready() };

    kmain(&view, &mut memory)
}

/// The kernel proper.
/// What answers the core's system call entries, registered with it: the
/// way in likewise is the core's, and the dispatcher is the item's. What a
/// Linux call does is the personality's, composed with the dispatcher here,
/// at compile time, so that it costs no second indirect call. And the look
/// every call gets first, at all four entries, before the early answers the
/// entries keep and before the native range is split off: a program's own
/// filter is the personality's policy, so the core holds a pointer to it and
/// nothing more (`docs/SECCOMP.md` §3.3), with the one question the fast
/// path asks of it before running ahead of it: whether the filter would look
/// at a call at all (`docs/OPAQUE-KERNEL.md` §9.7, T2).
#[cfg(ferrix_linux_abi = "on")]
fn register_system_calls() {
    trap::set_syscall_entry(syscall::dispatch_with::<syscall::linux::Linux>);
    trap::set_quiet_syscall_filter(syscall::seccomp::check);
}

/// Without the Linux ABI (`linux_abi=off`, `docs/LINUX-ABI-SEPARATION.md`
/// §11.11): the native range is answered as always, every Linux number is
/// `ENOSYS`, and no program filter is registered, so the core's
/// [`trap::filter_quiet`] reads none and asks nothing.
#[cfg(ferrix_linux_abi = "off")]
fn register_system_calls() {
    trap::set_syscall_entry(syscall::dispatch_with::<NoLinux>);
}

// The Linux ABI's number and error types name `NoLinux`'s call below; with
// the Linux ABI on, the binary names the crate nowhere else, its users
// being the item's and the load's.
#[cfg(ferrix_linux_abi = "on")]
use ferrix_linux_abi as _;

/// The personality a kernel without the Linux ABI composes: it answers no
/// Linux call.
#[cfg(ferrix_linux_abi = "off")]
struct NoLinux;

#[cfg(ferrix_linux_abi = "off")]
impl syscall::Personality for NoLinux {
    fn answer(
        _call: ferrix_linux_abi::nr::Syscall,
        _args: &trap::SyscallArgs,
        _regs: Option<&arch::UserRegs>,
    ) -> trap::Outcome {
        trap::Outcome::Return(ferrix_linux_abi::errno::Errno::ENOSYS.as_return_value())
    }
}

/// The core's T2 hook, bound when the kernel is linked: the personality's
/// quiet predicate, called directly (`trap::filter_quiet`, which decides
/// whether to ask).
#[unsafe(no_mangle)]
fn ferrix_filter_quiet() -> bool {
    filter_quiet_of_personality()
}

/// The predicate [`ferrix_filter_quiet`] forwards to: the Linux personality's
/// seccomp filter's.
#[cfg(ferrix_linux_abi = "on")]
use syscall::seccomp::quiet as filter_quiet_of_personality;

/// Without the Linux ABI no filter is registered, so [`trap::filter_quiet`]
/// never asks this (`docs/LINUX-ABI-SEPARATION.md` §11.11).
#[cfg(ferrix_linux_abi = "off")]
const fn filter_quiet_of_personality() -> bool {
    false
}

// Each hook is the type `trap` declares it with: see the `const _`s there.
const _: trap::FilterQuiet = ferrix_filter_quiet;
const _: trap::FastWriteRead = ferrix_fast_write_read;

/// The core's fast-path hook, bound when the kernel is linked: the item's
/// `channel_write_read` fast path, called directly from x86-64's entry once
/// `trap::fast_path_on` says this boot takes it.
#[unsafe(no_mangle)]
fn ferrix_fast_write_read(a: &[u64; 6]) -> trap::Fast {
    syscall::native::fast_write_read(a)
}

fn kmain(view: &BootView<'_>, memory: &mut EarlyMemory) -> ! {
    verify_hand_off(view, memory);
    // Whether the rest of the stages check what they bring up. Read here,
    // after the one check that always runs and before the first that may not.
    checks::init(view);
    // The options the architecture's bring-up takes before its traps: on
    // x86-64 the preference every processor's HWP request asks for
    // (`ferrix.epp`), chosen before the first processor asks.
    arch::read_boot_options(view);

    // Before anything else, and before anything can fault: until this runs the
    // CPU is still pointing at firmware's handlers, which stopped existing at
    // `exit_boot_services`. A fault in that window is a jump into reclaimed
    // memory, which on x86-64 is a triple fault and a silent reset.
    //
    // SAFETY: (ENTRY) called exactly once, on the boot CPU, with interrupts masked.
    unsafe { arch::init_traps() };
    // The trap return is the core's; what happens on it is the Linux
    // personality's, and the core holds only a pointer to it
    // (`crate::trap::ReturnPath`). Registered here because the first user
    // program is a boot check below, not init.
    #[cfg(ferrix_linux_abi = "on")]
    syscall::deliver::install();
    register_system_calls();
    println!("  traps    vectors installed");
    console::screen::milestone(3);

    let stats = bring_up_memory(view);
    console::screen::milestone(4);
    // The log kept across a reset, as soon as the direct map's RAM is known
    // and while this is the only processor and interrupts are off.
    attach_pstore(view);

    if let Err(problem) = vmap::init(view.raw().kaslr.vmap_end) {
        fatal!(
            catalog::VMAP_ARENA_BRING_UP,
            "could not bring up the kernel address arena: {problem}"
        );
    }
    console::screen::milestone(5);

    // From here on each self-check runs only when `checks::run` says so; each
    // step that brings something up runs whatever it says. The checks that
    // are all check are gated here, in the order they run; the steps that do
    // both gate their own checks inside.
    if checks::run() {
        #[cfg(ferrix_linux_abi = "on")]
        stages_check::check_allocators_and_traps(&stats, view);
    }

    // Everything above is synchronous: traps the kernel caused deliberately.
    // From here something arrives that the kernel did not ask for at the
    // moment it arrives, which is the whole difference between a program and
    // an operating system.
    //
    // SAFETY: (DEVICE) called once, on the boot CPU, after `init_traps` filled the
    // vector table and while interrupts are still masked.
    let clocks = match unsafe { arch::init_interrupts(view) } {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::INTERRUPT_BRING_UP,
            "could not bring up interrupts: {problem}"
        ),
    };
    console::screen::milestone(6);
    report_clocks_and_power(view, &clocks);

    if let Err(problem) = timer::init() {
        fatal!(
            catalog::TIMER_REGISTRATION,
            "could not register the timer interrupt: {problem}"
        );
    }

    start_console_input(view);
    arch::enable_interrupts();

    check_timer_and_start_clocks(view);
    console::screen::milestone(7);

    // Stage 4. It has to be here: after interrupt bring-up, which maps the
    // local APIC x86-64 reads its own identifier from, and before
    // `finish_memory`, which reclaims the tables the processor list comes
    // from and sweeps a set of mappings that bringing processors up adds to.
    let cpus = bring_up_processors(view);
    console::screen::milestone(8);

    // Stage 5, after stage 4 because it needs every processor it is going to
    // schedule on, and before `finish_memory` because the task stacks it
    // takes and gives back are mappings the sweep below has to see settled.
    start_scheduler(cpus);
    console::screen::milestone(9);
    // `ferrix.msrlog`, x86-64 only and off without the word: a task on this
    // processor that reads the power-limit MSRs, once every processor is up
    // and there is a scheduler to sleep in (`L.x86_64.187`).
    arch::start_msr_log(view);

    // The console's transmit side, as a program's output goes out: from a
    // task, which the scheduler just made this, into the ring the port's
    // interrupt empties. Early, because every line after this one is sent
    // the way it checks.
    #[cfg(ferrix_linux_abi = "on")]
    stages_check::check_console_output();

    // Stage 6, so far the memory objects a process is built from and the
    // processor translating through one of them. Here
    // rather than after `finish_memory` because it allocates and frees frames
    // and requires the count to return to where it started, which is a
    // measurement the reclaim below would otherwise move under it.
    if checks::run() {
        #[cfg(ferrix_linux_abi = "on")]
        stages_check::check_user_memory();
    }

    // Stage 8's root filesystem. Before stage 7's checks, which open files,
    // and after stage 6's, because file contents are VMO pages and the
    // frames they take have to come back.
    #[cfg(ferrix_linux_abi = "on")]
    check_filesystems(view);

    if checks::run() {
        #[cfg(ferrix_linux_abi = "on")]
        stages_check::check_programs();
    }

    // Stage 10's enumeration: every PCI function the machine's ECAM windows
    // reach, with every BAR sized and every capability list walked. Before
    // `finish_memory`, which reclaims the ACPI tables the MCFG is read from
    // and sweeps the kernel's mappings, so the bus windows this maps have to
    // be given back first.
    let iommu = iommu::bring_up(view);
    println!(
        "  iommu    {} VT-d units and {} SMMUv3s translating, {} remapping only, {} left alone",
        iommu.vtd, iommu.smmu_v3, iommu.remapping_only, iommu.refused
    );
    if let Some(why) = iommu.why {
        println!("  iommu    a unit was left alone: {why}");
    }

    // Stage 10's device nodes: every PCI function above and every virtio,mmio
    // node in the device tree, with the rule a driver's memory and interrupts
    // rest on — nothing outside what the device has — required of each.
    // Straight after enumeration, which builds the PCI half.
    check_devices(view);
    iommu::report(view, device::devices());
    #[cfg(ferrix_linux_abi = "on")]
    iommu::domain_check::check_iommu();
    #[cfg(ferrix_linux_abi = "on")]
    iommu::domain_check::check_rmrr_identity(view.regions());

    // Stage 9's device objects, on the nodes just published: an I/O mapping of
    // a device's own aperture and nothing past it, reached from a forked
    // child, and an interrupt held from delivery to acknowledgement. After
    // `check_devices`, because before it there are no nodes to mint from.
    if checks::run() {
        #[cfg(ferrix_linux_abi = "on")]
        stages_check::check_device_objects();
    }
    #[cfg(ferrix_linux_abi = "on")]
    check_block_ring();

    // The rest of stage 2, deliberately last. Each of these needs something a
    // later part of boot brought up — the arena needs the heap, the sweep
    // needs every mapping the kernel is ever going to make, and reclaiming
    // ACPI memory needs the tables to have been read, which happened in
    // `init_interrupts` above.
    if let Err(problem) = finish_memory(view) {
        fatal!(
            catalog::STAGE2_FINISH_MEMORY,
            "stage 2 self-check failed: {problem}"
        );
    }
    say_booted();

    // After the marker, on purpose: see `init`. Returns at once when nothing
    // was named by `ferrix.init=` or built in, and the image has no
    // /sbin/init.
    init::run();
    power::finish()
}

/// Stage 3's timer check, when the checks run, and then the realtime clock
/// and the random generator, which are bring-up and started either way --
/// without the check, from a counter nobody has measured this boot.
#[cfg(ferrix_linux_abi = "on")]
fn check_timer_and_start_clocks(view: &BootView<'_>) {
    if !checks::run() {
        report_clock_and_random(view);
        return;
    }
    let measured = match stages_check::timer_check() {
        Ok(hertz) => hertz,
        Err(problem) => fatal!(
            catalog::STAGE3_TIMER,
            "stage 3 self-check failed: {problem}"
        ),
    };
    report_stage3(measured, view);
    // With the interrupt controller up, because part of it is the
    // controller's masking.
    if let Err(problem) = arch::check_machine() {
        fatal!(
            catalog::STAGE3_MACHINE,
            "stage 3 self-check failed: {problem}"
        );
    }
    stages_check::check_waits_before_the_scheduler();
}

/// Without the Linux ABI, stage 3's measured timer check and its waits are
/// not built (they are `stages_check`'s, which runs Linux programs:
/// LINUX-ABI-SEPARATION.md §11.11), so the clock and the random generator
/// start from a counter nobody measured, as with `ferrix.checks=skip`, and
/// the machine's own check runs when the checks do.
#[cfg(ferrix_linux_abi = "off")]
fn check_timer_and_start_clocks(view: &BootView<'_>) {
    if checks::run()
        && let Err(problem) = arch::check_machine()
    {
        fatal!(
            catalog::STAGE3_MACHINE,
            "stage 3 self-check failed: {problem}"
        );
    }
    report_clock_and_random(view);
}

/// Stop the machine if a hook a stage 9 check armed inside the item is still
/// armed: every one is disarmed by now, before the marker and init (F-60,
/// `docs/OPAQUE-KERNEL.md` §9.7's rules for such a hook).
fn require_hooks_disarmed() {
    if let Some(check) = arch::leave_hook_armed_by() {
        fatal!(
            catalog::CHECK_HOOK_LEFT_ARMED,
            "a check's hook was still armed after stage 9: {check}, in a speculation domain's leave"
        );
    }
    if let Some(check) = sched::work::hook_armed_by() {
        fatal!(
            catalog::CHECK_HOOK_LEFT_ARMED,
            "a check's hook was still armed after stage 9: {check}, in sched::work::notify's wake"
        );
    }
    if let Some(check) = user::vmo::reclaim_hook_armed_by() {
        fatal!(
            catalog::CHECK_HOOK_LEFT_ARMED,
            "a check's hook was still armed after stage 12: {check}, in user::vmo's reclaim of clean pages"
        );
    }
    if let Some(check) = arch::forget_hook_armed_by() {
        fatal!(
            catalog::CHECK_HOOK_LEFT_ARMED,
            "a check's hook was still armed after stage 9: {check}, in a forgotten root's clear"
        );
    }
}

/// The last line of boot before init: the success marker when every check
/// ran, and the unchecked one, which no boot test accepts, when they were
/// skipped.
fn say_booted() {
    require_hooks_disarmed();
    panic::mark_booted();
    audit::record(
        audit::BOOTED,
        audit::Outcome::Done,
        0,
        audit::Subject::KERNEL,
        audit::Target::NONE,
        [u32::from(checks::run()), 0, 0],
    );
    // What a machine has left for its programs once the kernel is up, beside
    // stage 2's line of what it started with: the number a small machine's
    // memory is sized by (a `--memory` sweep reads it from each boot).
    println!(
        "  memory   {} KiB free of {} KiB managed at the end of boot, {} KiB of it kept from \
         programs",
        mm::free_frames() * 4,
        mm::managed_frames() * 4,
        mm::watermark() * 4,
    );
    // Without the Linux ABI the checks that run a Linux program are not
    // built (LINUX-ABI-SEPARATION.md §11.11), so such a kernel never says its
    // stages were checked.
    let checked = checks::run() && cfg!(ferrix_linux_abi = "on");
    if checked {
        // After every check that counts what was given back, since it loses
        // memory on purpose.
        #[cfg(ferrix_linux_abi = "on")]
        stages_check::check_a_give_up();
        // Last before the marker, so every driver the boot starts has run: a
        // DMA fault its unit recorded and nothing provoked fails the boot
        // here rather than sitting unread in the unit's record.
        // After the reclaim, which must have given an RMRR's frames to
        // nobody (`L.iommu.75`).
        #[cfg(ferrix_linux_abi = "on")]
        iommu::domain_check::check_rmrr_frames();
        let dma_faults_read = iommu::check_dma_faults();
        // And after it, since reading the faults is what records them.
        #[cfg(ferrix_linux_abi = "on")]
        stages_check::check_audit_booted(dma_faults_read);
        #[cfg(ferrix_linux_abi = "off")]
        let _ = dma_faults_read;
        println!("{SUCCESS_MARKER} stages 1-12");
    } else {
        // The faults nothing else reads in a boot without checks: one line,
        // never fatal (`L.iommu.72`).
        iommu::report_unchecked_faults();
        if cfg!(ferrix_linux_abi = "off") {
            println!(
                "{UNCHECKED_MARKER} stages 1-12 brought up, the self-checks that run a Linux \
                 program not built: linux_abi=off"
            );
        } else {
            println!(
                "{UNCHECKED_MARKER} stages 1-12 brought up, the self-checks of 2 to 12 skipped \
                 as ferrix.checks=skip asks"
            );
        }
    }
}

#[cfg(ferrix_linux_abi = "on")]
/// Stage 8: build the root from the initramfs, and require it to be what the
/// build wrote and to store what it is given.
///
/// Halts rather than returning, as every other stage's check does.
fn check_filesystems(view: &BootView<'_>) {
    let built = match fs::init(view) {
        Ok(built) => built,
        Err(problem) => fatal!(
            catalog::STAGE8_ROOT,
            "could not build the root filesystem: {problem}"
        ),
    };
    if !checks::run() {
        // What unpacking made is bring-up, and said either way; the check that
        // the marker came through intact is not.
        report_initrd(&built, "not verified");
        return;
    }
    let report = match fs::check::run(&built) {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE8_FILESYSTEM,
            "stage 8 self-check failed: {problem}"
        ),
    };

    report_initrd(
        &built,
        if report.initramfs_verified {
            "verified true"
        } else {
            "verified false"
        },
    );
    println!(
        "  tmpfs    {} pages written through a VMO and read back, {} filled from a page \
         source in runs, by reads and by faults, and cut, {} frames leaked",
        report.pages, report.filled, report.leaked,
    );

    let calls = match fs::check::run_calls() {
        Ok(calls) => calls,
        Err(problem) => fatal!(
            catalog::STAGE8_PIPES_AND_FILESYSTEM_CALLS,
            "stage 8 pipe and filesystem call self-check failed: {problem}"
        ),
    };
    println!(
        "  pipes    {} bytes through a pipe, a FIFO, sendfile, splice and copy_file_range; \
         statfs, truncate and fallocate answered; proc and devtmpfs mounted, read and unmounted, \
         sysfs mounted and unmounted; {} frames leaked",
        calls.bytes, calls.leaked,
    );

    let mapped = match fs::mmap_check::run() {
        Ok(mapped) => mapped,
        Err(problem) => fatal!(
            catalog::STAGE8_FILE_MAPPINGS,
            "stage 8 file mapping self-check failed: {problem}"
        ),
    };
    println!(
        "  mmap     {} bytes written through a shared file mapping and read back from the \
         file, and the other way; a loader-style fixed RX/RW mapping and RELRO protection \
         worked; refusals, msync and /proc maps answered; a truncation \
         took {} pages away from the mapping; {} pages copied into a private mapping and \
         kept from the file, a fork and a user-mode write included; {} frames leaked",
        mapped.bytes, mapped.cut, mapped.copied, mapped.leaked,
    );
    stages_check::check_memfd();
    stages_check::check_epoll();
    stages_check::check_eventfd();
    stages_check::check_timerfd();
    stages_check::check_program_files();
    stages_check::check_signals();
    stages_check::check_madvise();
    stages_check::check_cgroupfs();

    let pseudo = match fs::procfs::check::run() {
        Ok(pseudo) => pseudo,
        Err(problem) => fatal!(
            catalog::STAGE8_PSEUDO_FILESYSTEMS,
            "stage 8 /dev and /proc self-check failed: {problem}"
        ),
    };
    println!(
        "  devfs    {} nodes numbered as Linux numbers them; zero, null, full and urandom \
         do what they are for; a disk registered as {}:{} listed, stat'ed, read as a file, found \
         by number, {} sectors read, one in memory written as a file, gone from /dev and \
         /proc/partitions with its registration; {} frames leaked",
        pseudo.devices,
        pseudo.blocks.major,
        pseudo.blocks.minor,
        pseudo.blocks.sectors,
        pseudo.blocks.leaked,
    );
    println!(
        "  procfs   {} names listed and walked back to, {} maps lines parsed, {} of them named; \
         cwd and root read as getcwd; {} /proc/sys values read, a host name written there \
         reached uname; partitions empty with no block devices",
        pseudo.listed, pseudo.maps_lines, pseudo.named, pseudo.sysctl_values,
    );
    println!(
        "  procstat /proc/stat read twice {} ms apart: a cpu line for each of {} processors, \
         {} ticks advanced, no counter went backwards",
        pseudo.stat_apart_ms, pseudo.stat_cpus, pseudo.stat_ticks,
    );
}

#[cfg(ferrix_linux_abi = "on")]
/// What unpacking the initramfs made, and `verified`: whether the check that
/// it came through intact passed, or that it did not run.
fn report_initrd(built: &fs::Report, verified: &str) {
    match (built.initramfs_bytes, built.unpacked) {
        (Some(bytes), Some(made)) => println!(
            "  initrd   {} KiB unpacked: {} directories, {} files, {} hard links, \
             {} symbolic links, {} refused, {verified}",
            bytes.div_ceil(1024),
            made.directories,
            made.files,
            made.hard_links,
            made.symlinks,
            made.skipped,
        ),
        _ => println!("  initrd   none handed over; the root is an empty tmpfs"),
    }
}

#[cfg(ferrix_linux_abi = "on")]
/// Stage 10: the block ring's control plane, driven from a process that is
/// given a device, up to a published disk and back.
///
/// Halts rather than returning, as every other stage's check does. A machine
/// with no PCI function passes and says so: a ring names its disk by a PCI
/// location.
fn check_block_ring() {
    if checks::run() {
        stages_check::check_ring_control();
        stages_check::check_chardev();
        stages_check::check_display_copies();
        // Before devmgr, which may start a driver that claims the log.
        stages_check::check_log_control();
    }
    // `devmgr` starts the drivers, the driver check waits for their disks
    // (and starts them itself where `devmgr` did not) and mounts what comes
    // next, and the net core is started: bring-up, whatever `checks` says.
    // Each disk check below says it skipped on a machine without its disk.
    if devmgr::by_init() {
        // L12 (`docs/INIT.md` §7.3): pid 1 starts devmgr, so its drivers,
        // the disk checks of stages 10 to 12 that read through them, and
        // `/`'s switch to the root disk all come after init starts. That
        // configuration is outside the certified one, whose boots keep the
        // checks (`docs/certification/ITEM.md` §5).
        println!(
            "  devmgr   left to pid 1: the disk checks of stages 10 to 12 are the kernel path's, \
             and / switches once devmgr has reported"
        );
        if let Err(why) = devmgr::make_drivers_job() {
            println!("  devmgr   drivers.slice could not be made: {why}");
        }
        fs::root_disk::switch_after_devmgr();
    } else {
        let started_by_devmgr = check_devmgr();
        check_driver(started_by_devmgr);
    }
    // The net core follows the same chain rather than a line of its own in
    // `kmain`, for the reason the block ring's two do: it needs everything
    // they need -- a root filesystem for sockfs, and the scheduler for the
    // task that drives the stack -- and nothing else.
    check_net();
    // Last, so that what sysfs shows is a running machine's: the device
    // nodes, the disks and interfaces devmgr's drivers published, and which
    // driver devmgr says drives which device. Only a check: sysfs is mounted
    // by whoever wants it, not by this.
    //
    // Then the small services the stages above lean on, and the native
    // calls' refusals, in the shapes a passing boot never puts them in: after
    // the device nodes, which two of them claim and clock, and after
    // `devmgr`, which one of them asks for a driver. And the native calls
    // init needs, which run a program from a file, so after the filesystems.
    if checks::run() {
        stages_check::check_smbios();
        stages_check::check_sysfs();
        stages_check::check_services();
        stages_check::check_native_refusals();
        stages_check::check_init_calls();
        stages_check::check_init_inputs();
    }
}

#[cfg(ferrix_linux_abi = "on")]
/// The net core, over the loopback: a socket call reaches the stack, the
/// stack builds a packet, and the packet comes back up the input path.
///
/// Here, after the block ring, because every socket is an inode on sockfs and
/// so it needs the root filesystem, and because the stack is driven by a task
/// and so it needs the scheduler. It needs no device at all: a machine with no
/// network adapter still has to be able to talk to itself, and that is the
/// whole of the path a packet takes with the wire left out.
///
/// Halts rather than returning, as every other stage's check does.
fn check_net() {
    if let Err(problem) = net::start() {
        fatal!(
            catalog::NET_CORE,
            "the net core's task could not be started: {problem}"
        );
    }
    if !checks::run() {
        return;
    }
    let report = match net::check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(catalog::NET_CORE, "net core self-check failed: {problem}"),
    };
    if let Some(why) = report.skipped {
        println!("  net      not checked: {why}");
        return;
    }
    println!(
        "  net      {} interface up, {} bytes carried over the loopback in both families, \
         {} connections made and accepted, {} calls refused as specified, \
         {} packets read by raw sockets",
        report.interfaces, report.bytes, report.connections, report.refusals, report.raw_packets,
    );
    stages_check::check_net_ring();
    stages_check::check_netlink();
}

#[cfg(ferrix_linux_abi = "on")]
/// Stage 10's exit, the half that is a driver: `/sbin/blk`, started from
/// here the way `devmgr` will start it, serves the test disk through the
/// block ring with the IOMMU on, and sectors read through the registry come
/// back as `xtask` wrote them. The driver is left running, and its disk in
/// `/dev`, for what mounts it next.
///
/// Halts rather than returning, as every other stage's check does. A machine
/// without a virtio-blk function passes and says so.
fn check_driver(started_by_devmgr: bool) {
    let report = match block_ring::driver_check::run(started_by_devmgr) {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE10_DRIVER,
            "stage 10 driver self-check failed: {problem}"
        ),
    };
    if let Some(why) = report.skipped {
        println!("  driver   not checked: {why}");
        return;
    }
    if checks::run() {
        println!(
            "  driver   blk serves {} ({} sectors) through the block ring; {} sectors read back \
             through the registry as xtask wrote them",
            report.names, report.sectors, report.read,
        );
    } else {
        println!(
            "  driver   blk serves {} ({} sectors) through the block ring; nothing read back",
            report.names, report.sectors,
        );
    }
    // Stage 11's and 12's exits are checks against the gates' fixture disks;
    // a boot that skips them still switches `/` to a root disk and mounts a
    // data disk, which `check_btrfs_write` does last either way.
    if checks::run() {
        stages_check::check_btrfs_disk();
    }
    check_btrfs_write();
}

#[cfg(ferrix_linux_abi = "on")]
/// Stage 10: `devmgr`, started from the initramfs with every device and
/// every driver image, and its REPORT (`docs/DEVMGR.md`). Answers whether it
/// started the block drivers, so the driver check reads through them rather
/// than starting its own.
///
/// Halts rather than returning, as every other stage's check does. An image
/// without `/sbin/devmgr` passes and says so.
fn check_devmgr() -> bool {
    let report = match devmgr::start() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE10_DEVMGR,
            "stage 10 devmgr self-check failed: {problem}"
        ),
    };
    console::screen::milestone(10);
    let Some(report) = report else {
        println!("  devmgr   not started: the image carries no /sbin/devmgr");
        return false;
    };
    println!(
        "  devmgr   {} devices, {} drivers, {} started, {} failed",
        report.devices, report.drivers, report.started, report.failed,
    );
    report.started > 0
}

#[cfg(ferrix_linux_abi = "on")]
/// Stage 12's exit, the guest's half: a blank btrfs volume on the third disk,
/// mounted writable, written, unmounted and mounted again, and read back —
/// so every byte compared comes off the disk rather than out of a cache.
///
/// Halts rather than returning, as every other stage's check does. A machine
/// without a third disk passes and says so.
fn check_btrfs_write() {
    use fs::btrfs_powerfail::{self as powerfail, Mode};
    let (result, verb) = match powerfail::mode() {
        Mode::Check => (None, ""),
        Mode::Churn => (Some(powerfail::churn(powerfail::seed())), "churned"),
        Mode::Replay => (Some(powerfail::replay()), "replayed"),
    };
    if let Some(result) = result {
        match result {
            Ok(report) if report.skipped.is_some() => {
                println!("  btrfs-pf not run: {}", report.skipped.unwrap_or_default());
            }
            Ok(report) => println!(
                "  btrfs-pf vdc {verb}: {} files, {} with a trailer whose promise held; {}",
                report.files,
                report.checked,
                if report.logged {
                    "the crash left a log, and the mount replayed it"
                } else {
                    "the crash left no log"
                },
            ),
            Err(problem) => fatal!(
                catalog::STAGE12_WRITE,
                "stage 12 power-fail check failed: {problem}"
            ),
        }
        return;
    }
    if !checks::run() {
        fs::root_disk::switch();
        fs::data_disk::mount();
        fs::home_disk::mount();
        return;
    }
    let report = match fs::btrfs_write_check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE12_WRITE,
            "stage 12 self-check failed: {problem}"
        ),
    };
    if let Some(why) = report.skipped {
        println!("  btrfs-rw not checked: {why}");
        // Stage 13's mount namespaces, which show the detach's write-out on
        // the stage 12 volume where there is one.
        stages_check::check_namespaces(false);
        return;
    }
    println!(
        "  btrfs-rw vdc written and remounted: {} files ({} bytes) and {} directories read back \
         as they were written",
        report.files, report.bytes, report.directories,
    );
    stages_check::check_namespaces(true);
    // After the check, which has vdc to itself: `/` onto the root disk, if an
    // interactive boot brought one, and then a data disk into that `/`.
    fs::root_disk::switch();
    fs::data_disk::mount();
    fs::home_disk::mount();
}

/// The item's discovery policy that stage 10's checks of the nodes hold
/// them to, the core's `device::publish` instantiated with it
/// (`device::check::Policy`, ledger 3294, 3934): the core may not name the
/// item.
struct Discovery;

impl device::check::Policy for Discovery {
    fn decoding_refusal(
        own: &[(u64, u64)],
        others: &[(u64, u64)],
        reserved: &device::Reserved,
    ) -> Option<&'static str> {
        pci::decoding_refusal(own, others, reserved)
    }
    fn may_lend() -> bool {
        firmware::may_lend()
    }
    fn refuses(base: u64, len: u64, kind: user::memory_type::MemoryType) -> bool {
        firmware::refuses(base, len, kind)
    }
    const KERNEL_MAPPING: user::memory_type::MemoryType = firmware::KERNEL_MAPPING;
}

/// Stage 10's discovery: run every finder in precedence order, reserving the
/// ranges they read, and publish what they found.
///
/// Halts rather than returning, as every other stage's check does: a failed
/// PCI walk under its own catalogue entry, anything else under the devices'.
fn publish_devices(view: &BootView<'_>) -> (device::Report, device::Reserved) {
    // The PCI walk first, then the device tree's nodes, then the boards': an
    // earlier finder's node keeps an aperture a later one also claims.
    let mut pci = pci::Enumeration::new(view);
    let mut tree = tree::VirtioMmio::new(view);
    let mut boards = board::Boards::new(view);
    // Last, so that the framebuffer it lends never takes an aperture from a
    // device: it adds a node only under `ferrix.efifb`.
    let mut framebuffer = firmware::FirmwareFramebuffer::new(view);
    // The power node, only under `ferrix.powerctl` on a processor the power
    // core's table names (`L.device.51`): no aperture, so its place is
    // immaterial.
    let mut power = discovery::powerctl::Power::new(view);
    // Before publishing, which yields the Intel integrated GPU unless the
    // command line lets it be driven (`L.device.44`).
    if view.flag(device::INTEL_GPU_OPTION) {
        device::allow_intel_gpu();
    }
    let reserved = device::Reserved::of(view, &[pci.reads(), tree.reads(), boards.reads()]);
    let mut finders: [&mut dyn Finder; 5] = [
        &mut pci,
        &mut tree,
        &mut boards,
        &mut framebuffer,
        &mut power,
    ];
    let report = match device::publish::<Discovery>(&mut finders, &reserved) {
        Ok(report) => report,
        // A failed walk halts the boot under its own entry, as it always has.
        Err(device::Stopped::Finder { name: "pci", why }) => {
            fatal!(catalog::STAGE10_PCI, "stage 10 self-check failed: {why}")
        }
        Err(device::Stopped::Finder { name, why }) => fatal!(
            catalog::STAGE10_DEVICES,
            "stage 10 self-check failed: the {name} finder: {why}"
        ),
        Err(device::Stopped::Again) => fatal!(
            catalog::STAGE10_DEVICES,
            "stage 10 self-check failed: device nodes were published twice"
        ),
        Err(device::Stopped::Node(problem)) => fatal!(
            catalog::STAGE10_DEVICES,
            "stage 10 self-check failed: {problem}"
        ),
    };
    (report, reserved)
}

/// Stage 10: publish the device nodes, requiring each to hand out exactly the
/// apertures and vectors it has.
///
/// Halts rather than returning, as every other stage's check does.
fn check_devices(view: &BootView<'_>) {
    let (report, reserved) = publish_devices(view);
    println!(
        "  devices  {} nodes ({} from the device tree, {} with decoding off), {} apertures \
         ({} not whole pages, {} withheld, {} MSI-X ranges withheld), {} vectors ({} edge, \
         {} withheld), {} MSI-X tables ({} vectors minted), {} MSI capabilities ({} vectors \
         minted, {} deliveries), {} refusals as specified; {} published",
        report.nodes,
        report.tree,
        report.undecoded,
        report.apertures,
        report.partial_pages,
        report.withheld,
        report.msix_withheld,
        report.vectors,
        report.edge,
        report.vectors_withheld,
        report.msix_tables,
        report.msix_minted,
        report.msi_functions,
        report.msi_minted,
        report.msi_delivered,
        report.refusals,
        device::devices().len(),
    );
    let (published, order) = device::check::order(device::devices());
    println!("  nodes    {published} published, digest of their order {order:#018x}");
    let (map, others, digest) = device::check::reserved(&reserved);
    println!(
        "  reserved {map} memory-map ranges and {others} others no aperture may overlap, \
         digest of the others {digest:#018x}"
    );

    // After publishing, so that it can pick interrupt lines no device names.
    if checks::run() {
        match arch::check_distributor() {
            Ok(arch::DistributorCheck {
                skipped: Some(why), ..
            }) => println!("  gic      not checked: {why}"),
            Ok(check) => {
                let (first, second) = check.lines.unwrap_or_default();
                println!(
                    "  gic      lines {first} and {second} made edge-triggered and enabled by two \
                     cores at once, {} rounds, {} lost a priority, target or configuration",
                    check.rounds, check.lost,
                );
            }
            Err(problem) => fatal!(
                catalog::STAGE10_DISTRIBUTOR,
                "stage 10 self-check failed: {problem}"
            ),
        }
        #[cfg(ferrix_linux_abi = "on")]
        check_config_window();
    }
}

#[cfg(ferrix_linux_abi = "on")]
/// Stage 10's checks of a driver's configuration window and of
/// `device_aperture` (`docs/NVIDIA.md` §12.1, W1 to W7).
///
/// Halts rather than returning, as every other stage's check does.
fn check_config_window() {
    let report = match device::config_check::run() {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE10_CONFIG,
            "stage 10 self-check failed: {problem}"
        ),
    };
    println!(
        "  config   {} apertures reported whole by device_aperture, {} above 4 GiB and longer \
         than it; {} reads of {} functions answered as enumerated",
        report.apertures, report.above_4_gib, report.reads, report.functions,
    );
    println!(
        "  config   {} writes to kernel-owned bytes refused whole and unchanged, {} malformed \
         accesses refused, {} driver-writable fields written and read back, {} rounds of bus \
         mastering raced against a driver's writes",
        report.refused, report.malformed, report.written, report.rounds,
    );
    if report.yielded != 0 || report.lent != 0 {
        println!(
            "  config   {} function(s) holding the lent firmware framebuffer answered no read; \
             {} calls on its node and holders answered as ferrix.efifb requires",
            report.yielded, report.lent
        );
    }
    if let Some(why) = report.skipped {
        println!("  config   the vendor-capability writes not checked: {why}");
    }
    if let Some(why) = report.race_skipped {
        println!("  config   the race not checked: {why}");
    }
    let passed = report.breach_passed_over;
    match (report.breach, report.breach_at) {
        (Some(register), Some(at)) => println!(
            "  config   a {register} rewritten behind the kernel was found and its node refused, \
             then restored: {at}, after {passed} function(s) whose command register would not \
             take the rewrite"
        ),
        _ if passed != 0 => println!(
            "  config   the breach check not run: {passed} function(s), none whose command \
             register takes a rewrite of INTx Disable and Bus Master Enable"
        ),
        _ => println!("  config   the breach check not run: no PCI function"),
    }
}

/// Say how many processors came up, and then what the machine is exposed to:
/// every processor has decided and recorded its side-channel defences by now,
/// so that can be said for all of it -- which, on a machine of mixed cores,
/// the boot processor alone cannot. In that order, so that the kinds of core
/// the exposure lines count follow the count of processors.
fn report_processors(cpus: &smp::Topology) {
    println!(
        "  cpus     {} described by firmware, {} online, booted on {} {:#x}",
        cpus.count(),
        cpus.online(),
        cpus.id_name(),
        cpus.boot_id(),
    );
    arch::report_speculation();
}

#[cfg(ferrix_linux_abi = "on")]
/// The timer's skipped arm (`stages_check::check_a_skipped_arm`), on the
/// boot processor with its record and before any other processor runs.
fn check_the_skipped_arm() {
    if !checks::run() {
        return;
    }
    match stages_check::check_a_skipped_arm() {
        Ok(arm) => println!(
            "  arm      a 2 ms one-shot asked for under a 1 s one fired after {} us; a 1 s \
             one asked for over a 2 ms one left it to fire after {} us",
            arm.after_long, arm.before_long
        ),
        Err(problem) => fatal!(
            catalog::STAGE3_TIMER,
            "stage 3 self-check failed: {problem}"
        ),
    }
}

/// Stage 4: find every processor, start them, and require them to work
/// together.
///
/// Panics rather than returning an error, like the rest of `kmain`: each step
/// here has its own message, because "stage 4 failed" says
/// nothing about which of a dozen processors, or which of the checks, did.
fn bring_up_processors(view: &BootView<'_>) -> &'static smp::Topology {
    // Counting first, starting nothing.
    let cpus = match smp::discover(view) {
        Ok(topology) => topology,
        Err(problem) => fatal!(
            catalog::PROCESSOR_DISCOVERY,
            "could not enumerate the processors: {problem}"
        ),
    };

    // The timer's skipped arm, which needs this processor's record and no
    // other processor arming a timer: here, between the two.
    #[cfg(ferrix_linux_abi = "on")]
    check_the_skipped_arm();

    // Then the rest of them. Each is started, waited for, and required to
    // find its own record through its own register before the next one is
    // started.
    if let Err(problem) = smp::start_secondaries(view) {
        fatal!(
            catalog::SECONDARY_START,
            "could not start the secondary processors: {problem}"
        );
    }
    if cpus.online() != cpus.count() {
        fatal!(
            catalog::PROCESSORS_MISSING,
            "not every processor firmware described came online"
        );
    }
    report_processors(cpus);
    if !checks::run() {
        return cpus;
    }

    let smp = match smp::check::run(cpus) {
        Ok(report) => report,
        Err(problem) => fatal!(catalog::STAGE4_SMP, "stage 4 self-check failed: {problem}"),
    };
    println!(
        "  smp      {} rounds of work on every processor, {} IPIs taken",
        smp.rounds, smp.ipis,
    );
    if arch::TLB_FLUSH_IS_BROADCAST {
        println!(
            "  tlb      {} remaps seen by every processor, invalidated by broadcast",
            smp.remaps,
        );
    } else {
        println!(
            "  tlb      {} remaps seen by every processor, {} shootdowns",
            smp.remaps, smp.shootdowns,
        );
    }
    println!(
        "  grace    {} grace periods against {} reads, none of them stale",
        smp.grace_periods, smp.reads,
    );
    println!(
        "  counter  {} of {}, {} of {} shares overlapping in round {}, {} updates lost without the lock",
        smp.counter,
        smp.expected,
        smp.overlapping,
        cpus.online(),
        smp.counter_round,
        smp.lost,
    );
    println!(
        "  stage 4  {} processors online, a contended counter came to {} of {}",
        cpus.online(),
        smp.counter,
        smp.expected,
    );
    cpus
}

fn start_scheduler(cpus: &'static smp::Topology) {
    if let Err(problem) = sched::init(cpus) {
        fatal!(
            catalog::SCHEDULER_BRING_UP,
            "could not start the scheduler: {problem}"
        );
    }
    // First thing once a task can run: on the Pixel 7 the watchdogs have
    // been counting since the loader, and the checks below take seconds.
    arch::start_watchdogs();
    if !checks::run() {
        return;
    }

    let report = match sched::run_checks(cpus) {
        Ok(report) => report,
        Err(problem) => fatal!(
            catalog::STAGE5_SCHEDULER,
            "stage 5 self-check failed: {problem}"
        ),
    };

    println!(
        "  tasks    {} threads run to completion on {} processors ({:#b}), {} switches, {} steals,          {} shootdowns to free their stacks",
        report.threads,
        report.processors,
        report.processor_mask,
        report.switches,
        report.steals,
        report.shootdowns,
    );
    println!(
        "  sleep    one task slept {} us and came back",
        report.slept / 1000,
    );
    // Both numbers, because the bound moves: it is a slice plus the worst
    // overrun the scheduler actually served, and a bound that moves is only
    // honest if it is printed beside what it bounded.
    println!(
        "  fair     {} spinners on every processor, worst lag {} us within a bound of {} us",
        report.spinners,
        report.worst_lag / 1000,
        report.bound / 1000,
    );
    println!(
        "  place    {} spawns sent elsewhere by the placer, landing on {} processors, affinity held",
        report.placed_elsewhere, report.placed_on,
    );
    println!(
        "  load     busy processor {} of {}, idle {} of {}",
        report.load_high,
        ferrix_sched_class::LOAD_SCALE,
        report.load_low,
        ferrix_sched_class::LOAD_SCALE,
    );
    println!(
        "  balance  {} tasks moved between processors that never went idle",
        report.balanced,
    );
    println!(
        "  slice    {} us before crowding, {} us with sixteen more runnable",
        report.slice_one / 1000,
        report.slice_many / 1000,
    );
    println!(
        "  cost     ms per check: one={} sleep={} many={} fair={} place={} affin={} load={} bal={} slice={}",
        report.spent_ms[0],
        report.spent_ms[1],
        report.spent_ms[2],
        report.spent_ms[3],
        report.spent_ms[4],
        report.spent_ms[5],
        report.spent_ms[6],
        report.spent_ms[7],
        report.spent_ms[8],
    );
    println!(
        "  stage 5  {} threads scheduled fairly across {} processors",
        report.threads, report.processors,
    );
    shootdowns_under_the_scheduler(cpus);
}

/// Stage 4's shootdown checks that need the scheduler: a waiter that moves,
/// and a processor that answers late, each kept so by a task.
fn shootdowns_under_the_scheduler(cpus: &smp::Topology) {
    // Stage 4's shootdown again, now that a task waiting in one can be
    // preempted and resumed on another processor, which before the scheduler
    // nothing could.
    match smp::check::migrating_shootdown(cpus) {
        Ok(Some((left, moved_to))) => println!(
            "  migrate  a task waiting for a shootdown moved from processor {left} to \
             {moved_to} and answered for {moved_to}"
        ),
        Ok(None) => println!(
            "  migrate  no processor waits for another's shootdown on {}",
            arch::NAME
        ),
        Err(problem) => fatal!(
            catalog::STAGE4_SMP,
            "stage 4 self-check failed under the scheduler: {problem}"
        ),
    }

    // A processor kept from answering past a wait's late bound, as a host
    // that stops running it keeps it, is waited for rather than called stuck.
    match smp::check::late_answer(cpus) {
        Ok(Some(seen)) => match seen.shootdown_us {
            Some(us) => println!(
                "  late     processor {} kept from answering was waited for and counted late: \
                 a grace period after {} us, a shootdown after {us} us",
                seen.held, seen.grace_us
            ),
            None => println!(
                "  late     processor {} kept from answering was waited for and counted late: \
                 a grace period after {} us, no shootdown waits for it",
                seen.held, seen.grace_us
            ),
        },
        Ok(None) => println!("  late     one processor online, nothing waits for another"),
        Err(problem) => fatal!(
            catalog::STAGE4_SMP,
            "stage 4 self-check failed under the scheduler: {problem}"
        ),
    }
}

/// The half of stage 2 that cannot run until the rest of boot has.
///
/// Three things, in an order that is forced rather than chosen:
///
/// 1. the loader's identity map goes, which is what proves the kernel is
///    genuinely higher-half rather than accidentally depending on a low
///    address somewhere;
/// 2. the W^X sweep runs, which can only pass *after* step 1 — the identity
///    map has to be writable and executable, because the instruction after the
///    page table switch is fetched through it;
/// 3. the memory early boot has finished with goes back to the allocator.
fn finish_memory(view: &BootView<'_>) -> Result<(), &'static str> {
    // Before: the sweep must be able to *see* a violation, or its passing
    // afterwards means nothing. The loader's identity map is one, by
    // construction, so this is a test of the test. The sweeps and the
    // identity map's absence are checks; dropping the map and the reclaim are
    // not, and run whatever `checks` says.
    let checking = checks::run();
    if checking && mm::check::check_w_xor_x(view).is_ok() {
        return Err("the W^X sweep cannot see the loader's identity map");
    }

    // SAFETY: (TRANSLATE) the kernel executes, and reaches its stack and the hand-off,
    // entirely through the upper half. Nothing has held a lower-half address
    // since `_start`.
    unsafe { arch::drop_identity_map(view) };

    // And nothing the identity map translated translates any more, which is
    // the claim "higher-half" actually makes. Asked of the architecture, not
    // of a walk of the kernel's tables: on the Arm pair the identity map is
    // the `TTBR0` regime, which that walk never reaches, so it would find
    // nothing whether the map had gone or not.
    if checking {
        if arch::identity_map_live(view) {
            return Err("the identity map outlived the call that dropped it");
        }
        mm::check::sweep_w_xor_x(view)?;
    }

    // SAFETY: (KMEM) called once, after the last use of `crate::discovery::acpi::Firmware` —
    // interrupt bring-up above is the only reader — and the loader's code has
    // not run since the jump into `_start`.
    let reclaimed = unsafe { mm::reclaim_boot_memory(view) };
    let usage = vmap::usage();
    println!(
        "  reclaim  {} MiB from the loader and ACPI, {} free; arena {} live, {} KiB",
        reclaimed.total() * 4 / 1024,
        mm::free_frames() * 4 / 1024,
        usage.allocations,
        usage.bytes / 1024,
    );
    if reclaimed.total() == 0 {
        return Err("nothing was reclaimed, so the memory map describes no early boot");
    }
    // Whatever `checks` says: the region a reset keeps must never be handed
    // out, and after the reclaim is when it could have been.
    mm::check::check_pstore_never_free()
}

#[cfg(ferrix_linux_abi = "on")]
/// Report the clocks interrupt bring-up measured, then read what the end of
/// boot is to do: `power::init` says so on the console, and says it after the
/// clocks, as it always has.
/// Say what stage 3 proved, then start the clock and the random generator,
/// which both read the counter it has just proved.
fn report_stage3(measured: u64, view: &BootView<'_>) {
    println!(
        "  stage 3  {} breakpoints, {} page faults, {} ticks at {} Hz",
        trap::breakpoint_count(),
        trap::handled_fault_count(),
        timer::ticks(),
        measured,
    );
    report_clock_and_random(view);
}

/// Start the realtime clock and seed the random generator from what firmware
/// handed over, and say what each had to go on. After stage 3, because both
/// read the counter the timer check has just proved.
fn report_clock_and_random(view: &BootView<'_>) {
    let info = view.raw();
    #[cfg(ferrix_linux_abi = "on")]
    match syscall::time::set_boot_time(info) {
        Some(seconds) => {
            println!("  clock    {seconds} seconds since the epoch, from firmware's clock");
        }
        None => println!("  clock    firmware has no clock: CLOCK_REALTIME starts at the epoch"),
    }
    // The wall clock is the Linux ABI's (`syscall::time`); the native ABI
    // has none yet (LINUX-ABI-SEPARATION.md §7 row 6).
    #[cfg(ferrix_linux_abi = "off")]
    println!(
        "  clock    not kept: the wall clock is the Linux ABI's, which this kernel is built without"
    );
    let mut trng = [0_u8; 48];
    let trng_bytes = arch::firmware_entropy(view, &mut trng);
    let seeding = random::init(info, trng.get(..trng_bytes).unwrap_or(&[]));
    let (firmware, trng_bytes) = (seeding.firmware_bytes, seeding.trng_bytes);
    if seeding.seeded {
        println!(
            "  random   seeded with {} bits: {firmware} bytes from firmware, {trng_bytes} from its TRNG, {} words from the CPU, timer jitter",
            seeding.credited, seeding.cpu_words
        );
    } else {
        println!(
            "  random   NOT SEEDED: {} of {} bits, from {firmware} bytes from firmware, {trng_bytes} from its TRNG and {} words from the CPU, and timer jitter; keys made on this boot are guessable",
            seeding.credited,
            ferrix_crng::SEEDED_BITS,
            seeding.cpu_words
        );
    }
    if let Err(problem) = random::check() {
        fatal!(
            catalog::RANDOM_GENERATOR,
            "random generator check failed: {problem}"
        );
    }
    start_audit(view);
}

/// Start the audit record (finding F-21b, `docs/certification/AUDIT.md`)
/// with the boot's audit id, drawn from the random generator now that it has
/// passed its check, and record the boot's configuration: every option has
/// been read by now. The store is checked here when the checks run. A boot
/// that skips them records that it did, which only a reader outside that
/// boot can check (§6).
fn start_audit(view: &BootView<'_>) {
    let mut id = [0_u8; 16];
    random::fill(&mut id);
    let _ = audit::start(u128::from_le_bytes(id));
    let kaslr = view.raw().kaslr;
    let (build, profile_and_level) = build_flags::audit_words();
    let expected = audit::check::Expected {
        checks: u32::from(checks::run()),
        devmgr: u32::from(devmgr::by_init()),
        mitigations: u32::from(arch::HARDENED),
        kaslr: kaslr.state,
        pstore: u32::from(console::pstore::attached().is_some()),
        efifb: u32::from(firmware::asked(view)),
        build,
        vtd: u32::from(!iommu::vtd_off_asked(view)),
        amdvi: u32::from(!iommu::amdvi_off_asked(view)),
        unisolated: u32::from(devmgr::unisolated_allowed()),
    };
    audit::config(audit::Config::Checks, expected.checks, 0);
    audit::config(audit::Config::Devmgr, expected.devmgr, 0);
    audit::config(audit::Config::Mitigations, expected.mitigations, 0);
    audit::config(
        audit::Config::Kaslr,
        expected.kaslr,
        u32::from(kaslr.is_random()),
    );
    audit::config(audit::Config::Pstore, expected.pstore, 0);
    audit::config(audit::Config::Efifb, expected.efifb, 0);
    audit::config(audit::Config::Build, build, profile_and_level);
    audit::config(audit::Config::Vtd, expected.vtd, 0);
    audit::config(audit::Config::AmdVi, expected.amdvi, 0);
    audit::config(audit::Config::Unisolated, expected.unisolated, 0);
    println!(
        "  audit    started: {} high-value and {} refusal records, {} configuration items \
         recorded",
        audit::HIGH_RECORDS,
        audit::REFUSAL_RECORDS,
        audit::CONFIG_ITEMS
    );
    if !checks::run() {
        return;
    }
    match audit::check::run(&expected) {
        Ok(report) => println!(
            "  audit    {} records kept of a wrapped ring and {} reported lost, resumed after a \
             partial read; {} jobs charged to their maker's budget; {} of a unit's refusals \
             from three of its jobs kept and {} counted as suppressed, another unit's kept, \
             grants untouched; {} configuration items read back; a record costs {} ns kept, \
             {} ns counted past its budget",
            report.kept,
            report.lost,
            report.budgets,
            report.flood_kept,
            report.suppressed,
            report.configs,
            report.grant_nanos,
            report.refusal_nanos,
        ),
        Err(problem) => fatal!(catalog::AUDIT_STORE, "audit self-check failed: {problem}"),
    }
}

#[cfg(ferrix_linux_abi = "on")]
/// Register the load ring's side of every interface the certified item
/// reaches it through, and check that each was registered.
///
/// The item may not name what is above it (`docs/certification/ITEM.md`): power
/// commits whatever filesystems registered a flush, init starts pid 1 with
/// whatever launcher was registered, `devmgr` reads with whatever reader was,
/// and device enumeration asks whatever board bindings were. This file is the
/// crate root and declares every module, so it is where the load ring is
/// told to register, explicitly and in order, rather than by a link-time
/// table whose order nobody can read.
///
/// The check is not gated by `ferrix.checks`: it costs nothing, and a boot
/// that lost a registration would power off without committing its disks,
/// which is not something to find out by losing them.
fn register_load(view: &BootView<'_>) {
    let registered = syscall::launch::install()
        .and_then(|()| fs::install())
        .and_then(|()| platform::st::stm32mp1::install(view))
        .and_then(|()| platform::google::gs201::usb::install())
        .and_then(|()| block_ring::install())
        .and_then(|()| net_ring::install())
        .and_then(|()| display::install())
        .and_then(|()| render::install())
        .and_then(|()| input::install())
        .and_then(|()| audio::install())
        .and_then(|()| chardev::install())
        .and_then(|()| logctl::install());
    if let Err(hooks::Full) = registered {
        fatal!(
            catalog::LOAD_REGISTRATION,
            "a registration list is full: the load ring registers more than the item expects"
        );
    }
    let flushes = power::flushes();
    let boards = board::board_bindings();
    let missing = if flushes == 0 {
        Some("no filesystem registered a flush, so a power-off would commit nothing")
    } else if !init::has_launcher() {
        Some("init has no launcher registered")
    } else if !devmgr::has_reader() {
        Some("devmgr has no reader registered")
    } else if boards == 0 {
        Some("no board support registered a device binding")
    } else if !power::has_boot_mode() {
        Some("no board support registered where reboot's word goes")
    } else if syscall::native::processes().is_none() {
        Some("nothing registered to make and start a native process")
    } else {
        None
    };
    if let Some(missing) = missing {
        fatal!(
            catalog::LOAD_REGISTRATION,
            "a registration is missing: {missing}"
        );
    }
    // The native calls the item leaves to the load ring: a `match` held every
    // one of them to an answer at compile time until they were registered
    // instead, so every boot holds them to one here.
    if let Some(call) = syscall::native::unserved() {
        fatal!(
            catalog::LOAD_REGISTRATION,
            "a registration is missing: nothing answers the native call {call:?}"
        );
    }
    println!(
        "  hooks    {flushes} commits before power-off, {boards} board bindings, init's launcher, devmgr's reader and the boot mode registered"
    );
    println!(
        "  hooks    {} native calls answered above the item, {} subsystems a quiesce waits out, native processes made by the personality",
        syscall::native::served_count(),
        syscall::native::server_count()
    );
}

/// [`register_load`] in a kernel without the Linux ABI: only board support
/// is above the item to register, and only what it registers is required.
/// No filesystem flush, init launcher, `devmgr` reader or native process
/// maker exists to be registered, and the native calls the load would have
/// served stay unserved (LINUX-ABI-SEPARATION.md §7 rows 2 and 5, §11.11).
#[cfg(ferrix_linux_abi = "off")]
fn register_load(view: &BootView<'_>) {
    let registered = platform::st::stm32mp1::install(view)
        .and_then(|()| platform::google::gs201::usb::install());
    if let Err(hooks::Full) = registered {
        fatal!(
            catalog::LOAD_REGISTRATION,
            "a registration list is full: the load ring registers more than the item expects"
        );
    }
    let boards = board::board_bindings();
    let missing = if boards == 0 {
        Some("no board support registered a device binding")
    } else if !power::has_boot_mode() {
        Some("no board support registered where reboot's word goes")
    } else {
        None
    };
    if let Some(missing) = missing {
        fatal!(
            catalog::LOAD_REGISTRATION,
            "a registration is missing: {missing}"
        );
    }
    println!(
        "  hooks    {boards} board bindings and the boot mode registered; no Linux ABI: no filesystem, init launcher, devmgr reader or native process maker"
    );
}

fn report_clocks_and_power(view: &BootView<'_>, clocks: &irq::Report) {
    report_clocks(clocks);
    // What the item reaches above it through, registered by what is above it,
    // here in bring-up order: before device enumeration asks board support for
    // its peripherals, before `devmgr` reads its drivers, before anything is
    // mounted that a power-off must commit, and long before init.
    register_load(view);
    power::init(view);
    #[cfg(ferrix_linux_abi = "on")]
    fs::btrfs_powerfail::init(view);
    #[cfg(ferrix_linux_abi = "on")]
    fs::root_disk::init(view);
    init::read_option(view);
    devmgr::read_option(view);
    #[cfg(ferrix_linux_abi = "on")]
    fs::procfs::remember_command_line(view.cmdline());
}

/// Print what interrupt and time bring-up found.
fn report_clocks(report: &irq::Report) {
    let (counter_mhz, counter_thousandths) = megahertz(report.counter_hz);
    let (timer_mhz, timer_thousandths) = megahertz(report.timer_hz);
    println!(
        "  clock    {} at {}.{:03} MHz",
        report.counter, counter_mhz, counter_thousandths
    );
    println!(
        "  irqs     {}, {} at {}.{:03} MHz",
        report.controller, report.timer, timer_mhz, timer_thousandths
    );
}

/// A frequency split into megahertz and thousandths of one.
///
/// There is no floating point in this kernel and there is not going to be:
/// the state a kernel has to save and restore on every trap is large enough
/// without it. Two integers and a `{:03}` say the same thing.
const fn megahertz(hz: u64) -> (u64, u64) {
    (hz / 1_000_000, (hz % 1_000_000) / 1000)
}

/// The console's input ring, checked while nothing can fill it, and then the
/// port's receive interrupt, installed with the timer's and before interrupts
/// are first enabled: a byte the port already holds lands in the ring the moment
/// they are.
fn start_console_input(view: &BootView<'_>) {
    let ring = match console::input::check() {
        Ok(checked) => checked,
        Err(problem) => fatal!(
            catalog::CONSOLE_INPUT,
            "console input self-check failed: {problem}"
        ),
    };
    let receive = match console::input::init(view) {
        Ok(receive) => receive,
        Err(problem) => fatal!(
            catalog::CONSOLE_INPUT,
            "could not install the console's receive interrupt: {problem}"
        ),
    };
    match receive {
        Some(irq) => println!(
            "  input    {} bytes back in order from the ring, {} past it counted; the port receives by interrupt {irq}",
            ring.held, ring.dropped
        ),
        None => println!(
            "  input    {} bytes back in order from the ring, {} past it counted; the port is polled",
            ring.held, ring.dropped
        ),
    }
}

/// Print what the allocators came up with.
/// Bring up memory management and report it, then start the boot console if
/// the command line asks for one. That is the first moment the framebuffer's
/// mapping can be checked: the kernel's root table is known, and a fault has a
/// handler to go to.
fn bring_up_memory(view: &BootView<'_>) -> mm::Stats {
    let stats = match mm::init(view) {
        Ok(stats) => stats,
        Err(problem) => fatal!(
            catalog::MEMORY_BRING_UP,
            "could not bring up memory: {problem}"
        ),
    };
    // The boot processor's allocation reserve, filled now so that what it
    // holds is part of the heap every later check starts from.
    if mm::fill_reserve().is_err() {
        fatal!(
            catalog::BOOT_OUT_OF_MEMORY,
            "no memory for the boot processor's allocation reserve"
        );
    }
    fallible::install_library_sections();
    report_memory(&stats);
    stats
}

/// Grow the kernel log as `log_buf_len=` asks (`console::log::size_from`),
/// and say so; a size that is no size, or no memory for it, is said and the
/// log kept as it was, as Linux keeps its static buffer.
fn size_log(view: &BootView<'_>) {
    match console::log::size_from(view) {
        Ok(Some(bytes)) => println!(
            "  log      {} KiB, as log_buf_len asked, with what was logged before",
            bytes / 1024
        ),
        Ok(None) => {}
        Err(problem) => {
            println!("  log      log_buf_len refused: {problem}; the log stays as it was");
        }
    }
}

/// Grow the log as `log_buf_len=` asks ([`size_log`]); then mirror it into
/// the region the loader gave, if it passes its checks
/// (`console::pstore`), and when the checks run, hold the placement rule to
/// regions made up from the map.
fn attach_pstore(view: &BootView<'_>) {
    // The log first, as large as `log_buf_len=` asks, while nothing else
    // writes.
    size_log(view);
    console::pstore::attach(view);
    if !checks::run() {
        return;
    }
    match console::log_check::check_pstore_placement(view) {
        Ok(refused) => println!("  pstore   placement refused {refused} made-up regions"),
        Err(problem) => fatal!(
            catalog::CONSOLE_LOG,
            "console log self-check failed: {problem}"
        ),
    }
}

/// Unlogged: the page array's address is in the direct map, which KASLR moves
/// (`console::write_unlogged`).
fn report_memory(stats: &mm::Stats) {
    println_unlogged!(
        "  frames   {} MiB managed, {} MiB free, {} entries at {:#x} ({} KiB)",
        stats.managed_frames * 4 / 1024,
        stats.free_frames * 4 / 1024,
        stats.managed_frames,
        stats.page_array_at,
        stats.page_array_bytes / 1024,
    );
}

/// Print what the loader handed over.
fn report(view: &BootView<'_>) {
    let info = view.raw();
    let mebibytes = |bytes: u64| bytes / (1024 * 1024);

    println!(
        "  memory   {} MiB total, {} MiB usable, {} regions",
        mebibytes(view.total_ram()),
        mebibytes(view.usable_ram()),
        view.regions().len()
    );
    // Where the loader put the image and the direct map: to the port, not the
    // kernel log, which a program may read (`console::write_unlogged`).
    println_unlogged!(
        "  kernel   {:#x} -> {:#x}, {} KiB",
        info.kernel_phys,
        info.kernel_virt,
        info.kernel_len / 1024
    );
    println_unlogged!(
        "  physmap  {:#x} covering {} MiB from {:#x}",
        info.physmap_base,
        mebibytes(info.physmap_len),
        info.physmap_phys
    );
    report_layout(view);
    let unreachable = view.max_ram_address().saturating_sub(view.physmap_limit());
    if unreachable != 0 {
        println!(
            "  memory   {} MiB above the direct map, which this kernel cannot use",
            mebibytes(unreachable)
        );
    }
    println!("  tables   root {:#x}", info.root_table_phys);

    if info.framebuffer.is_present() {
        println!(
            "  display  {}x{}, stride {}{}",
            info.framebuffer.width,
            info.framebuffer.height,
            info.framebuffer.stride,
            if info.framebuffer.is_reclaimable() {
                ", in memory the allocator owns: panics go to serial only"
            } else {
                ""
            }
        );
    }
    if info.rsdp != 0 {
        println!("  acpi     rsdp at {:#x}", info.rsdp);
    }
    if let Some((at, len)) = view.device_tree() {
        // The model is the machine's own name for itself, and reading it is
        // the check that the copy the loader made still parses.
        let model = fdt::open(view)
            .ok()
            .and_then(|tree| tree.model())
            .unwrap_or("unreadable");
        println!("  fdt      {model}, {len} bytes at {at:#x}");
    }
    report_smbios(view);
}

/// Accept or refuse the loader's SMBIOS copy, and say which: the version, the
/// size, and the vendor and product only -- never a serial, a UUID or a byte
/// of the table (`docs/DMI.md` §4).
fn report_smbios(view: &BootView<'_>) {
    use ferrix_smbios::Field;
    match discovery::smbios::open(view) {
        Ok(tables) => {
            let version = tables.point().version;
            let identity = tables.identity();
            println!(
                "  smbios   {}.{}, {} structures in {} bytes: {} {}",
                version >> 16,
                (version >> 8) & 0xFF,
                tables.structures(),
                tables.table().len(),
                discovery::smbios::Shown(identity.get(Field::SysVendor)),
                discovery::smbios::Shown(identity.get(Field::ProductName)),
            );
        }
        Err(discovery::smbios::Refusal::None) => println!("  smbios   none handed over"),
        Err(refused) => println!("  smbios   refused: {}", refused.text()),
    }
}

/// Check the things the rest of the kernel is about to assume.
///
/// This is stage 1's exit criterion. Every one of these is something that would
/// otherwise be discovered much later, by a subsystem that had no way to know
/// the ground under it was wrong.
fn self_check(view: &BootView<'_>, memory: &mut EarlyMemory) -> Result<(), &'static str> {
    let regions = view.regions();
    if regions.is_empty() {
        return Err("the memory map is empty");
    }

    // The frame allocator will walk this map assuming it is ordered and that no
    // two regions claim the same frame.
    let mut previous_end = 0u64;
    for region in regions {
        if region.base < previous_end {
            return Err("the memory map is unsorted or overlapping");
        }
        previous_end = region.end();
    }

    if view.usable_ram() == 0 {
        return Err("the memory map reports no usable RAM");
    }

    #[cfg(ferrix_linux_abi = "on")]
    stages_check::check_loader_allocations(view)?;

    // Every read-only mapping the loader made, the kernel's text and rodata
    // among them, protects nothing from the kernel itself unless this holds.
    if !arch::kernel_write_protected() {
        return Err("the kernel can write through a read-only mapping: CR0.WP is clear");
    }

    #[cfg(ferrix_linux_abi = "on")]
    stages_check::check_layout(view)?;
    #[cfg(ferrix_linux_abi = "on")]
    stages_check::check_direct_map(view, memory)?;
    check_early_mapper(view, memory)?;
    #[cfg(ferrix_linux_abi = "on")]
    stages_check::check_no_early_window_wraps(memory)?;
    Ok(())
}

/// Say where the layout came from, without the addresses, which `report`
/// already printed and the loader's own line gives with the slide.
fn report_layout(view: &BootView<'_>) {
    let kaslr = view.raw().kaslr;
    if kaslr.state != KASLR_MOVED {
        println!("  kaslr    {}", kaslr.state_text());
    } else if kaslr.is_random() {
        println!(
            "  kaslr    image, direct map and arena moved, {}, {} and {} bits from {}",
            kaslr.image_bits,
            kaslr.physmap_bits,
            kaslr.vmap_bits,
            kaslr.source_text()
        );
    } else {
        println!(
            "  kaslr    moved, but from {}: NOT randomised against a local attacker",
            kaslr.source_text()
        );
    }
}

/// How many bytes of a framebuffer hold visible lines: `stride` pixels of four
/// bytes on each of `height` lines, and never more than firmware said it has.
fn framebuffer_bytes(framebuffer: &ferrix_bootinfo::Framebuffer) -> u64 {
    let pixels = u64::from(framebuffer.stride).saturating_mul(u64::from(framebuffer.height));
    pixels.saturating_mul(4).min(framebuffer.size)
}

/// Prove the kernel can read and extend the page tables the loader left.
///
/// Two things, both of which everything after stage 1 depends on. First that
/// walking the loader's tables from software agrees with what the hardware is
/// doing — the kernel's own image is the one mapping whose answer is known in
/// advance. Second that a *new* mapping can be installed and takes effect,
/// which is the whole of `EarlyMemory`'s job and, on AArch64, the only reason
/// there is a console at all.
fn check_early_mapper(view: &BootView<'_>, memory: &mut EarlyMemory) -> Result<(), &'static str> {
    let info = view.raw();

    match memory.translate(info.kernel_virt) {
        Some(phys) if phys == info.kernel_phys => {}
        Some(_) => return Err("walking the page tables disagrees with the loader"),
        None => return Err("the kernel image is not mapped in its own page tables"),
    }

    // No device window over the image itself: its text has no writable
    // mapping anywhere, and a device window is writable. Refused before the
    // mapper runs, so the on-demand window, stage 3's and unused until then,
    // stays empty.
    let text = info.kernel_phys;
    match memory.map_device(vmap::DEMAND_WINDOW, text, PAGE_SIZE) {
        Err(early::EarlyError::KernelImage(at)) if at == text => {}
        Err(_) => {
            return Err(
                "an early device window over the kernel's text failed for the wrong reason",
            );
        }
        Ok(()) => return Err("an early device window over the kernel's text was mapped"),
    }
    if memory.translate(vmap::DEMAND_WINDOW).is_some() {
        return Err("a refused early device window left a mapping behind");
    }

    // A device window the kernel has a real use for, when firmware left one:
    // the visible part of the framebuffer, which is where a panic is drawn.
    // The PL011 console took the same path a moment ago.
    // Mapped at the top of `kmain` (`map_boot_framebuffer`), so that the
    // boot console is on the screen before anything else runs; checked here
    // with the rest of the early mapper.
    let framebuffer = info.framebuffer;
    if framebuffer.is_present()
        && !framebuffer.is_reclaimable()
        && memory.translate(vmap::FRAMEBUFFER_WINDOW) != Some(framebuffer.phys)
    {
        return Err("the framebuffer mapping does not resolve to the framebuffer");
    }

    Ok(())
}

/// Stage 1: the boot console, the banner, the hand-off report and its
/// self-check, with the first three milestone stripes. Out of `kmain` so
/// that function keeps the length the complexity gate holds it to.
fn verify_hand_off(view: &BootView<'_>, memory: &mut EarlyMemory) {
    // Before the first line, so the boot console draws every one of them.
    // The milestone stripes (`console::screen::milestone`) are what a photo
    // of a screen with no legible text still tells: rows 0 to 10, in order.
    let framebuffer = map_boot_framebuffer(view, memory);
    console::screen::milestone(0);
    println!();
    println!("Ferrix {} on {}", env!("CARGO_PKG_VERSION"), arch::NAME);
    // What this kernel was built as, from its own `cfg`s, before anything
    // else it says (`docs/BUILD-FLAGS.md`, rule 6).
    build_flags::report();

    report(view);
    console::screen::milestone(1);

    if let Err(problem) = framebuffer.and_then(|()| self_check(view, memory)) {
        fatal!(
            catalog::STAGE1_HANDOFF,
            "stage 1 self-check failed: {problem}"
        );
    }
    println!("  stage 1  loader hand-off verified");
    // After the check: a panic before it halts, whatever the command line.
    panic::reset::read_option(view);
    console::screen::milestone(2);
}

/// Map the framebuffer firmware left, record it for the panic screen, and
/// start the boot console on it under `ferrix.fbcon`: the first thing `kmain`
/// does, so a machine with no serial port shows how far boot got from its
/// very first line on.
///
/// A framebuffer in memory the allocator will hand out again is not drawn
/// on: the panic screen would write into frames that belong to someone else.
/// The panic still goes to serial. `docs/DISPLAY.md` §2.4. A failure is
/// returned for stage 1's self-check to report, after the console is up.
fn map_boot_framebuffer(view: &BootView<'_>, memory: &mut EarlyMemory) -> Result<(), &'static str> {
    // The boot processor's PAT before the window: on x86-64 the window is
    // write-combining from its first map only under the kernel's table.
    arch::program_memory_types();
    let framebuffer = view.raw().framebuffer;
    #[cfg(ferrix_linux_abi = "on")]
    display::note_boot_framebuffer(framebuffer.is_present(), framebuffer.is_reclaimable());
    if !framebuffer.is_present() || framebuffer.is_reclaimable() {
        return Ok(());
    }
    let at = vmap::FRAMEBUFFER_WINDOW;
    let len = framebuffer_bytes(&framebuffer).min(vmap::FRAMEBUFFER_WINDOW_SIZE);
    if memory.map_framebuffer(at, framebuffer.phys, len).is_err() {
        return Err("could not map the framebuffer");
    }
    if memory.translate(at) != Some(framebuffer.phys) {
        return Err("the framebuffer mapping does not resolve to the framebuffer");
    }
    // No line firmware's mapping left in a cache beside the new type, before
    // the first store (x86-64; nothing on Arm).
    arch::flush_window(at, len);
    panic::screen::install(&framebuffer, at, len);
    console::screen::start(view);
    Ok(())
}
