//! The architecture facade.
//!
//! Generic kernel code reaches the CPU only through this module — never by
//! naming `x86_64`, `aarch64` or `armv7a` — and `#[cfg(target_arch)]` appears
//! nowhere else in the tree. Both rules are enforced by
//! `tools/common/check/check-crate-layering.sh`, because a facade maintained by convention
//! is a facade for about six weeks.

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "arm")]
mod armv7a;
#[cfg(target_arch = "x86_64")]
mod x86_64;

// Side-channel defences: what every architecture shares, its boot check, and
// each architecture's own half, which the shared part reaches by one name.
// `docs/certification/SPECULATION.md`.
mod speculation;
mod speculation_check;
#[cfg(target_arch = "aarch64")]
use aarch64::speculation as machine_speculation;
#[cfg(target_arch = "arm")]
use armv7a::speculation as machine_speculation;
pub(crate) use speculation::{
    CheckHook, HARDENED, answer_leaving, arm_leave_hook, barrier_decisions_on, disarm_leave_hook,
    entering_space, forget_root, last_domain_on, leave_hook_armed_by, leaving_domain, left_space,
    nospec_below, nospec_index, refill_wanted_in_domain, refills_in_domain_on,
    report_exposure as report_speculation, serve_wanted_barrier, switch_barriers,
    switch_barriers_on,
};
pub(crate) use speculation_check::check as check_speculation;
#[cfg(target_arch = "x86_64")]
use x86_64::speculation as machine_speculation;

/// Which `struct stat` this architecture's stat calls fill in.
///
/// The choice is an architecture's, so it is stated here beside the other ABI
/// facts the facade carries — `OPEN_FLAGS`, `EPOLL_EVENT_BYTES` — and each
/// architecture names one in its `STAT_LAYOUT`. What the bytes *are* is not an
/// architecture's business: `crate::syscall::stat` owns the encoding and
/// carries this type's `impl`.
///
/// It reads oddly to define a Linux type in the architecture facade until you
/// try it the other way round, which is how it was: the facade named
/// `crate::syscall::stat::StatLayout`, and the trusted core therefore depended
/// on the Linux personality for a constant. Data here, behaviour there, and
/// the dependency points the way `tools/common/check/check-item-boundary.py` requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatLayout {
    /// x86-64's own, from `arch/x86/include/uapi/asm/stat.h`: 144 bytes. It
    /// predates the generic header and was kept rather than replaced.
    Legacy,
    /// The generic one, from `include/uapi/asm-generic/stat.h`: 128 bytes.
    Generic,
    /// ARMv7-A's `struct stat64`, from `arch/arm/include/uapi/asm/stat.h`:
    /// 104 bytes, filled by the `64` calls that are all it answers.
    Stat64,
}

// The core's own system call entry, run with a call the boot check chose
// (`syscall::seccomp_check`).
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{drive_native_words, drive_system_call};
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::{drive_native_words, drive_system_call};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{drive_native_words, drive_system_call};

// Drivers for the Arm peripherals both Arm architectures can have: the GICv2,
// the PL011, and the STM32MP1's USART, which is ARMv7-A's.
#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
mod arm_common;

#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{
    ARCH, COMPAT_USER_END, CpuStarter, EPOLL_EVENT_BYTES, FAST_WRITE_READ, Irq, NAME, OPEN_FLAGS,
    PageEncoding, STAT_LAYOUT, TLB_FLUSH_IS_BROADCAST, TrapFrame, USER_ARGUMENT_PROGRAM,
    USER_COW_PROGRAM, USER_EXEC_PROGRAM, USER_FAULT_PROGRAM, USER_FORK_PROGRAM,
    USER_MPROTECT_PROGRAM, USER_NAMESPACE_PROGRAM, USER_NATIVE_PROGRAM, USER_SHARED_PROGRAM,
    USER_SPIN_PROGRAM, USER_STEP_PROGRAM, USER_STEP_STATUS, USER_TEST_PROGRAM, USER_TEST_STATUS,
    UserRegs, UserState, advance_past_breakpoint, audit_arch, breakpoint, classify, console,
    console_receive_irq, counter_hz, counter_now, cpu_local, cpu_local_register,
    decode_compat_syscall, decode_syscall, describe_cpus, disable_interrupts, drain_console,
    drop_identity_map, enable_console_receive, enable_interrupts, enter_user, flush_tlb,
    forbid_user_access, frame_pointer, halt, hardware_id, hardware_random, identity_map_live,
    identity_root, image_abi, init_console, init_interrupts, init_traps, install_user_root,
    interrupts_enabled, ipi_irq, kernel_write_protected, mask_interrupt, msi_allocate,
    msi_doorbell, permit_user_access, prepare_stack, prepare_user_root, read_console_byte,
    report_trap, reset, reset_user_state, restore_user_state, resume_user, save_user_state,
    send_ipi_to_others, service_interrupts, set_cpu_local, set_thread_area, shutdown, switch_to,
    syscall_rollback_value, system_call, take_console_byte, thread_area, timer_arm, timer_disarm,
    timer_disarm_fired, timer_irq, uninstall_user_root, unmask_interrupt, user_hwcaps,
    user_platform, wait_for_interrupt, wait_for_work,
};
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::{
    ARCH, COMPAT_USER_END, CpuStarter, EPOLL_EVENT_BYTES, FAST_WRITE_READ, Irq, NAME, OPEN_FLAGS,
    PageEncoding, STAT_LAYOUT, TLB_FLUSH_IS_BROADCAST, TrapFrame, USER_ARGUMENT_PROGRAM,
    USER_COW_PROGRAM, USER_EXEC_PROGRAM, USER_FAULT_PROGRAM, USER_FORK_PROGRAM,
    USER_MPROTECT_PROGRAM, USER_NAMESPACE_PROGRAM, USER_NATIVE_PROGRAM, USER_SHARED_PROGRAM,
    USER_SPIN_PROGRAM, USER_STEP_PROGRAM, USER_STEP_STATUS, USER_TEST_PROGRAM, USER_TEST_STATUS,
    UserRegs, UserState, advance_past_breakpoint, audit_arch, breakpoint, classify, console,
    console_receive_irq, counter_hz, counter_now, cpu_local, cpu_local_register,
    decode_compat_syscall, decode_syscall, describe_cpus, disable_interrupts, drain_console,
    drop_identity_map, enable_console_receive, enable_interrupts, enter_user, flush_tlb,
    forbid_user_access, frame_pointer, halt, hardware_id, hardware_random, identity_map_live,
    identity_root, image_abi, init_console, init_interrupts, init_traps, install_user_root,
    interrupts_enabled, ipi_irq, kernel_write_protected, mask_interrupt, msi_allocate,
    msi_doorbell, permit_user_access, prepare_stack, prepare_user_root, read_console_byte,
    report_trap, reset, reset_user_state, restore_user_state, resume_user, save_user_state,
    send_ipi_to_others, service_interrupts, set_cpu_local, set_thread_area, shutdown, switch_to,
    syscall_rollback_value, system_call, take_console_byte, thread_area, timer_arm, timer_disarm,
    timer_disarm_fired, timer_irq, uninstall_user_root, unmask_interrupt, user_hwcaps,
    user_platform, wait_for_interrupt, wait_for_work,
};
// How many processors programmed the PAT with its write-combining entry:
// x86-64's, which the Arm architectures do without, their write-combining
// being a memory attribute every processor has.
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::write_combining_processors;

/// No PAT to program: write-combining is normal non-cacheable memory here.
#[cfg(not(target_arch = "x86_64"))]
pub(crate) const fn write_combining_processors() -> Option<usize> {
    None
}

// What the switch gives a program back, checked in user mode: x86-64's
// vector-state contract and its FS base kept in the task (docs/OPAQUE-KERNEL.md
// §9.8, 3a and 3b), and ARMv7-A's thread ID registers (§9.14).
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::check_switch_state;
// ARMv7-A's: its thread ID registers (§9.14, 3b and F-66).
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::check_switch_state;
// The switches that left DS and ES unloaded, 0 over 0, for the fast path's
// counts (OPAQUE-KERNEL.md §9.8, 3b, the DS/ES skip); none on Arm.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::selector_skips_total;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::selector_skips_total;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::selector_skips_total;

/// AArch64: no vector-state contract and no record of its own here yet.
#[cfg(target_arch = "aarch64")]
pub(crate) const fn check_switch_state() -> Result<(), &'static str> {
    Ok(())
}

// A signal return page, for an architecture without a vDSO its programs
// could read: ARMv7-A's (`syscall::sigpage`, F-48).
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::sigpage_code;

/// No signal return page: the vDSO has the trampoline here.
#[cfg(not(target_arch = "arm"))]
pub(crate) const fn sigpage_code() -> Option<&'static [u8]> {
    None
}

// What the architecture decodes and decides on its own from values the
// machine hands it -- trap syndromes, the console's description, the
// interrupt controller's masking -- checked at boot against built inputs
// (`aarch64/check.rs`, `armv7a/check.rs`). x86-64's is for that
// architecture's pass to write; until then it has nothing here to check.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::check_machine;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::check_machine;

/// What the architecture decodes on its own, checked: nothing yet on x86-64.
///
/// # Errors
///
/// None.
#[cfg(target_arch = "x86_64")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the Arm architectures' checks return what failed, and callers are shared"
)]
pub(crate) const fn check_machine() -> Result<(), &'static str> {
    Ok(())
}

/// What the interrupt controller's concurrent-change check found (F-50):
/// see `arm_common::gicv2::check`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DistributorCheck {
    /// The two neighbouring lines raced, when the check ran.
    pub(crate) lines: Option<(u32, u32)>,
    /// Rounds run.
    pub(crate) rounds: u32,
    /// Rounds in which a line lost its priority, target or configuration.
    pub(crate) lost: u32,
    /// Why the check did not run, when it did not.
    pub(crate) skipped: Option<&'static str>,
}

// Two cores changing neighbouring lines of a GICv2 distributor at once lose
// neither change: after device discovery, so that it can pick lines no
// device names.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::check_distributor;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::check_distributor;

/// The distributor check: x86-64 has no GICv2 to check.
///
/// # Errors
///
/// None.
#[cfg(target_arch = "x86_64")]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the Arm architectures' checks return what failed, and callers are shared"
)]
pub(crate) const fn check_distributor() -> Result<DistributorCheck, &'static str> {
    Ok(DistributorCheck {
        lines: None,
        rounds: 0,
        lost: 0,
        skipped: Some("no GICv2 distributor on this architecture"),
    })
}

// The watchdogs a board's firmware leaves running: found at boot, fed once
// the scheduler can run a task, fired to reset. Only the Pixel 7's today.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{init_watchdogs, start_watchdogs};
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::{init_watchdogs, start_watchdogs};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{init_watchdogs, start_watchdogs};
// The program that takes its bootstrap handle after an `execve`, for the
// check of init's native calls (`docs/INIT.md` §6, K3); and the one that
// answers `getuid`, for the check that a native process runs as its creator.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{USER_BOOTSTRAP_PROGRAM, USER_GETUID_PROGRAM};
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::{USER_BOOTSTRAP_PROGRAM, USER_GETUID_PROGRAM};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{USER_BOOTSTRAP_PROGRAM, USER_GETUID_PROGRAM};
// The programs stage 13's `CLONE_INTO_CGROUP` and scoped OOM kill checks run.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{USER_INTO_CGROUP_PROGRAM, USER_OOM_PROGRAM};
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::{USER_INTO_CGROUP_PROGRAM, USER_OOM_PROGRAM};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{USER_INTO_CGROUP_PROGRAM, USER_OOM_PROGRAM};
// Signal delivery: the register context the way back to user mode loads, the
// architecture's signal frame, and the signal a user-mode fault becomes.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{
    SIGNAL_RED_ZONE, USER_DETHREAD_PROGRAM, USER_EXITS_PROGRAM, USER_HANDOFF_PROGRAM,
    USER_SIGNAL_PROGRAM, USER_STOPPED_PROGRAM, USER_SYSLOG_PROGRAM, USER_THREAD_PROGRAM,
    USER_XSTATE_PROGRAM, UserContext, fault_signal, restore_signal_frame, setup_signal_frame,
};
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::{
    SIGNAL_RED_ZONE, USER_DETHREAD_PROGRAM, USER_EXITS_PROGRAM, USER_HANDOFF_PROGRAM,
    USER_SIGNAL_PROGRAM, USER_STOPPED_PROGRAM, USER_SYSLOG_PROGRAM, USER_THREAD_PROGRAM,
    USER_XSTATE_PROGRAM, UserContext, fault_signal, restore_signal_frame, setup_signal_frame,
};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{
    ARCH, COMPAT_USER_END, CpuStarter, EPOLL_EVENT_BYTES, FAST_WRITE_READ, Irq, NAME, OPEN_FLAGS,
    PageEncoding, STAT_LAYOUT, TLB_FLUSH_IS_BROADCAST, TrapFrame, USER_ARGUMENT_PROGRAM,
    USER_COW_PROGRAM, USER_EXEC_PROGRAM, USER_FAULT_PROGRAM, USER_FORK_PROGRAM,
    USER_MPROTECT_PROGRAM, USER_NAMESPACE_PROGRAM, USER_NATIVE_PROGRAM, USER_SHARED_PROGRAM,
    USER_SPIN_PROGRAM, USER_STEP_PROGRAM, USER_STEP_STATUS, USER_TEST_PROGRAM, USER_TEST_STATUS,
    UserRegs, UserState, advance_past_breakpoint, audit_arch, breakpoint, classify, console,
    console_receive_irq, counter_hz, counter_now, cpu_local, cpu_local_register,
    decode_compat_syscall, decode_syscall, describe_cpus, disable_interrupts, drain_console,
    drop_identity_map, enable_console_receive, enable_interrupts, enter_user, flush_tlb,
    forbid_user_access, frame_pointer, halt, hardware_id, hardware_random, identity_map_live,
    identity_root, image_abi, init_console, init_interrupts, init_traps, install_user_root,
    interrupts_enabled, ipi_irq, kernel_write_protected, mask_interrupt, msi_allocate,
    msi_doorbell, permit_user_access, prepare_stack, prepare_user_root, read_console_byte,
    report_trap, reset, reset_user_state, restore_user_state, resume_user, save_user_state,
    send_ipi_to_others, service_interrupts, set_cpu_local, set_thread_area, shutdown, switch_to,
    syscall_rollback_value, system_call, take_console_byte, thread_area, timer_arm, timer_disarm,
    timer_disarm_fired, timer_irq, uninstall_user_root, unmask_interrupt, user_hwcaps,
    user_platform, wait_for_interrupt, wait_for_work,
};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{
    SIGNAL_RED_ZONE, USER_DETHREAD_PROGRAM, USER_EXITS_PROGRAM, USER_HANDOFF_PROGRAM,
    USER_SIGNAL_PROGRAM, USER_STOPPED_PROGRAM, USER_SYSLOG_PROGRAM, USER_THREAD_PROGRAM,
    USER_XSTATE_PROGRAM, UserContext, fault_signal, restore_signal_frame, setup_signal_frame,
};
// The scoped TLB shootdown: one page invalidated, one processor interrupted,
// and the program that checks it from user mode.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{USER_RMAP_PROGRAM, flush_tlb_page, send_ipi_to};
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::{USER_RMAP_PROGRAM, flush_tlb_page, send_ipi_to};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{USER_RMAP_PROGRAM, flush_tlb_page, send_ipi_to};

// Ordering memory a device reaches by DMA, and a register write after it (F-44).
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::dma_barrier;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::dma_barrier;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::dma_barrier;

// A word of this processor's own record, read and changed atomically against
// interrupts and migration: the preemption count (`sched::preempt`).
mod percpu;
pub(crate) use percpu::{this_cpu_add, this_cpu_read};

// Device register accesses, one instruction each that a hypervisor can
// emulate; `crate::mmio`'s windows are the only caller.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::mmio;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::mmio;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::mmio;

// Deciding and applying the side-channel defences, on the boot processor.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::init_speculation;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::init_speculation;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::init_speculation;

// The boot check for exceptions that arrive wherever the processor is.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::check_exception_entry;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::check_exception_entry;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::check_exception_entry;

// The vDSO's code, whether it can read the counter itself, and the program
// the boot check holds it to: none of the three on an architecture whose
// vDSO nobody has written.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{USER_VDSO_PROGRAM, vdso_can_read_counter, vdso_spec};
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::{USER_VDSO_PROGRAM, vdso_can_read_counter, vdso_spec};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{USER_VDSO_PROGRAM, vdso_can_read_counter, vdso_spec};

// Whether a program can read the counter the kernel's clock counts, for a
// driver's own timings: a separate question from the vDSO's, which only
// x86-64 has written.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::ring3_reads_counter;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::ring3_reads_counter;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::ring3_reads_counter;

// What user mode may read of the timer, checked on every processor once all
// are online: ARMv7-A's `CNTKCTL` (L.armv7a.4). AArch64's check is a
// docs/BACKLOG.md row; x86-64's counter is the TSC, which nothing closes.
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::check_user_counter;

/// What user mode may read of the timer, checked: nothing yet here.
///
/// # Errors
///
/// None.
#[cfg(not(target_arch = "arm"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "ARMv7-A's check returns what failed, and the caller is shared"
)]
pub(crate) const fn check_user_counter() -> Result<(), &'static str> {
    Ok(())
}

// Cache maintenance for a device that does not snoop the caches: the
// DK board's display controller reads a framebuffer straight from memory, so
// whatever a program drew has to be written back from the caches to the point
// of coherency before the controller is told to read it.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::clean_for_device;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::clean_for_device;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::clean_for_device;

// Cache maintenance for an IOMMU whose table walk does not snoop the caches
// (VT-d's ECAP.C clear): the entries a processor wrote through its cached
// direct map are written back to memory, and waited for, before the unit is
// told to read them (finding F-58). Unlike `clean_for_device`, x86-64 does
// it too, by `clflush`: a PC's devices snoop, its IOMMU's walk may not.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::clean_for_walker;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::clean_for_walker;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::clean_for_walker;

// The same, and the lines dropped too: for memory a program will share with
// such a device through a mapping past the caches (`vmo_pin`'s
// `PIN_COHERENT`), where a line left in the cache could be written back over
// what the device wrote.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::flush_for_device;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::flush_for_device;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::flush_for_device;

// How a framebuffer is mapped: write-combining where the architecture has it.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::FRAMEBUFFER_FLAGS;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::FRAMEBUFFER_FLAGS;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::FRAMEBUFFER_FLAGS;

// Full-entropy bytes firmware hands out, where it has a TRNG to ask.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::firmware_entropy;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::firmware_entropy;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::firmware_entropy;

// Instructions the kernel wrote into memory, made the ones every processor
// fetches there: for a page about to be mapped executable in user mode. An
// Arm core's instruction cache does not see what its data side wrote, nor
// what another core's did, until it is told to.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::sync_instructions;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::sync_instructions;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::sync_instructions;
