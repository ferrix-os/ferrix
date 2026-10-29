//! The architecture facade.
//!
//! Generic kernel code reaches the CPU only through this module — never by
//! naming `x86_64`, `aarch64` or `armv7a` — and `#[cfg(target_arch)]` appears
//! nowhere else in the tree. Both rules are enforced by
//! `tools/common/check/check-crate-layering.sh`, because a facade maintained by convention
//! is a facade for about six weeks.

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
#[cfg(target_arch = "arm")]
pub mod armv7a;
#[cfg(target_arch = "x86_64")]
pub mod x86_64;

// Side-channel defences: what every architecture shares, its boot check, and
// each architecture's own half, which the shared part reaches by one name.
// `docs/certification/SPECULATION.md`.
mod speculation;
mod speculation_check;
#[cfg(target_arch = "aarch64")]
use aarch64::speculation as machine_speculation;
#[cfg(target_arch = "arm")]
use armv7a::speculation as machine_speculation;
pub use speculation::nospec_index;
pub use speculation::{
    CheckHook, HARDENED, arm_forget_hook, arm_leave_hook, barrier_decisions_on, disarm_forget_hook,
    disarm_leave_hook, forget_root, last_domain_on, refill_wanted_in_domain, refills_in_domain_on,
    switch_barriers, switch_barriers_on,
};
pub(crate) use speculation::{
    answer_leaving, entering_space, leaving_domain, left_space, nospec_below, serve_wanted_barrier,
};
pub use speculation::{
    forget_hook_armed_by, leave_hook_armed_by, report_exposure as report_speculation,
};
pub use speculation_check::check as check_speculation;

/// `L.x86_64.211`'s check: every processor's `RDTSCP` named its own logical
/// number; `Ok(None)` where the architecture or the processor has none.
///
/// # Errors
///
/// As `x86_64::syscall::check::tsc_aux`.
// CHECKS-ONLY: for `check_speculation` in `stages_check.rs`.
pub fn check_tsc_aux() -> Result<Option<usize>, &'static str> {
    #[cfg(target_arch = "x86_64")]
    {
        x86_64::syscall::check::tsc_aux()
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        Ok(None)
    }
}
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
pub enum StatLayout {
    /// x86-64's own, from `arch/x86/include/uapi/asm/stat.h`: 144 bytes. It
    /// predates the generic header and was kept rather than replaced.
    Legacy,
    /// The generic one, from `include/uapi/asm-generic/stat.h`: 128 bytes.
    Generic,
    /// ARMv7-A's `struct stat64`, from `arch/arm/include/uapi/asm/stat.h`:
    /// 104 bytes, filled by the `64` calls that are all it answers.
    Stat64,
}

// Drivers for the Arm peripherals both Arm architectures can have: the GICv2,
// the PL011, and the STM32MP1's USART, which is ARMv7-A's.
#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
mod arm_common;

/// Arm's power-off is its `shutdown` (PSCI `SYSTEM_OFF`); only x86-64 tells a
/// halt from a power-off.
#[cfg(target_arch = "aarch64")]
pub use aarch64::shutdown as power_off;
#[cfg(target_arch = "aarch64")]
pub use aarch64::{
    ARCH, COMPAT_USER_END, EPOLL_EVENT_BYTES, Irq, NAME, OPEN_FLAGS, STAT_LAYOUT, TrapFrame,
    USER_ARGUMENT_PROGRAM, USER_COW_PROGRAM, USER_EXEC_PROGRAM, USER_FAULT_PROGRAM,
    USER_FORK_PROGRAM, USER_KERNEL_TIME_PROGRAM, USER_MPROTECT_PROGRAM, USER_NAMESPACE_PROGRAM,
    USER_NATIVE_PROGRAM, USER_SHARED_PROGRAM, USER_SPIN_PROGRAM, USER_TEST_PROGRAM,
    USER_TEST_STATUS, UserState, audit_arch, counter_hz, enter_user, image_abi, interrupts_enabled,
    read_console_byte, reset_user_state, resume_user, set_thread_area, syscall_rollback_value,
    take_console_byte, thread_area, user_base, user_hwcaps, user_platform,
};
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{
    CpuStarter, PageEncoding, advance_past_breakpoint, check_root_tags, classify,
    console_receive_irq, cpu_local, cpu_local_register, describe_cpus, drain_console,
    enable_console_receive, flush_tlb, forbid_user_access, frame_pointer, hardware_id,
    identity_root, install_user_root, ipi_irq, last_root_write, mask_interrupt, msi_doorbell,
    permit_user_access, prepare_stack, prepare_user_root, report_trap, restore_user_state,
    root_tag_now, save_user_state, send_ipi_to_others, service_interrupts, set_cpu_local,
    switch_to, system_call, timer_arm, timer_disarm, timer_disarm_fired, timer_irq,
    uninstall_user_root, unmask_interrupt,
};
#[cfg(target_arch = "aarch64")]
pub use aarch64::{
    FAST_WRITE_READ, UserRegs, counter_now, decode_compat_syscall, decode_syscall,
    disable_interrupts, enable_interrupts, hardware_random, msi_allocate, reset, shutdown,
};
#[cfg(target_arch = "aarch64")]
pub use aarch64::{
    TLB_FLUSH_IS_BROADCAST, USER_STEP_PROGRAM, USER_STEP_STATUS, breakpoint, console,
    drop_identity_map, halt, identity_map_live, init_console, init_interrupts, init_traps,
    kernel_write_protected, wait_for_interrupt, wait_for_work,
};
/// As AArch64's.
#[cfg(target_arch = "arm")]
pub use armv7a::shutdown as power_off;
#[cfg(target_arch = "arm")]
pub use armv7a::{
    ARCH, COMPAT_USER_END, EPOLL_EVENT_BYTES, Irq, NAME, OPEN_FLAGS, STAT_LAYOUT, TrapFrame,
    USER_ARGUMENT_PROGRAM, USER_COW_PROGRAM, USER_EXEC_PROGRAM, USER_FAULT_PROGRAM,
    USER_FORK_PROGRAM, USER_KERNEL_TIME_PROGRAM, USER_MPROTECT_PROGRAM, USER_NAMESPACE_PROGRAM,
    USER_NATIVE_PROGRAM, USER_SHARED_PROGRAM, USER_SPIN_PROGRAM, USER_TEST_PROGRAM,
    USER_TEST_STATUS, UserState, audit_arch, counter_hz, enter_user, image_abi, interrupts_enabled,
    read_console_byte, reset_user_state, resume_user, set_thread_area, syscall_rollback_value,
    take_console_byte, thread_area, user_base, user_hwcaps, user_platform,
};
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::{
    CpuStarter, PageEncoding, advance_past_breakpoint, check_root_tags, classify,
    console_receive_irq, cpu_local, cpu_local_register, describe_cpus, drain_console,
    enable_console_receive, flush_tlb, forbid_user_access, frame_pointer, hardware_id,
    identity_root, install_user_root, ipi_irq, last_root_write, mask_interrupt, msi_doorbell,
    permit_user_access, prepare_stack, prepare_user_root, report_trap, restore_user_state,
    root_tag_now, save_user_state, send_ipi_to_others, service_interrupts, set_cpu_local,
    switch_to, system_call, timer_arm, timer_disarm, timer_disarm_fired, timer_irq,
    uninstall_user_root, unmask_interrupt,
};
#[cfg(target_arch = "arm")]
pub use armv7a::{
    FAST_WRITE_READ, UserRegs, counter_now, decode_compat_syscall, decode_syscall,
    disable_interrupts, enable_interrupts, hardware_random, msi_allocate, reset, shutdown,
};
#[cfg(target_arch = "arm")]
pub use armv7a::{
    TLB_FLUSH_IS_BROADCAST, USER_STEP_PROGRAM, USER_STEP_STATUS, breakpoint, console,
    drop_identity_map, halt, identity_map_live, init_console, init_interrupts, init_traps,
    kernel_write_protected, wait_for_interrupt, wait_for_work,
};
// What `/proc/cpuinfo` reads of the processor: `CPUID` and the vector state
// programs may use on x86-64, each processor's `MIDR` on the Arm pair; the
// other architectures' answer is none.
#[cfg(target_arch = "aarch64")]
pub use aarch64::speculation::midr_of;
#[cfg(target_arch = "arm")]
pub use armv7a::speculation::midr_of;
#[cfg(target_arch = "x86_64")]
pub use x86_64::{cpuid, user_extended_state};

/// No `CPUID` off x86-64.
#[cfg(not(target_arch = "x86_64"))]
pub const fn cpuid(_leaf: u32, _subleaf: u32) -> Option<[u32; 4]> {
    None
}

/// No `XCR0` off x86-64.
#[cfg(not(target_arch = "x86_64"))]
pub const fn user_extended_state() -> u64 {
    0
}

/// No `MIDR` on x86-64.
#[cfg(target_arch = "x86_64")]
pub const fn midr_of(_logical: usize) -> Option<u32> {
    None
}

// How many processors programmed the PAT with its write-combining entry:
// x86-64's, which the Arm architectures do without, their write-combining
// being a memory attribute every processor has.
#[cfg(target_arch = "x86_64")]
pub use x86_64::write_combining_processors;

/// No PAT to program: write-combining is normal non-cacheable memory here.
#[cfg(not(target_arch = "x86_64"))]
pub const fn write_combining_processors() -> Option<usize> {
    None
}

// `ferrix.msrlog`, x86-64's read-only MSR log (`L.x86_64.187`); none on Arm.
#[cfg(target_arch = "x86_64")]
pub use x86_64::start_msr_log;

/// No MSRs: the Arm kernels have no such log.
#[cfg(not(target_arch = "x86_64"))]
pub const fn start_msr_log(_view: &ferrix_bootinfo::BootView<'_>) {}

// The power core, x86-64's VR current limit through typed calls on a claim
// (`docs/POWERCTL.md`, `L.x86_64.194`-`208`, `L.device.51`-`53`); none on
// Arm, whose kernels publish no power node and answer the calls WRONG_TYPE.
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::restore_power;
#[cfg(target_arch = "x86_64")]
pub use x86_64::{power_claim, power_probe, power_read, power_set, power_unserved};

/// No power node on Arm.
#[cfg(not(target_arch = "x86_64"))]
pub const fn power_probe(_view: &ferrix_bootinfo::BootView<'_>) -> bool {
    false
}

/// No power core on Arm.
#[cfg(not(target_arch = "x86_64"))]
pub fn power_claim()
-> Result<alloc::sync::Arc<crate::object::channel::Endpoint>, ferrix_linux_abi::errno::Errno> {
    Err(ferrix_native_abi::status::WRONG_TYPE)
}

/// No power core on Arm.
#[cfg(not(target_arch = "x86_64"))]
pub fn power_set(
    _claim: &alloc::sync::Arc<crate::object::channel::Endpoint>,
    _eighths: u64,
) -> Result<usize, ferrix_linux_abi::errno::Errno> {
    Err(ferrix_native_abi::status::WRONG_TYPE)
}

/// No power core on Arm.
#[cfg(not(target_arch = "x86_64"))]
pub fn power_read(
    _claim: &alloc::sync::Arc<crate::object::channel::Endpoint>,
) -> Result<[u64; ferrix_native_abi::types::POWER_READ_WORDS], ferrix_linux_abi::errno::Errno> {
    Err(ferrix_native_abi::status::WRONG_TYPE)
}

/// No power claim on Arm.
#[cfg(not(target_arch = "x86_64"))]
pub const fn power_unserved() -> bool {
    true
}

/// Nothing raised on Arm, so nothing to restore.
#[cfg(not(target_arch = "x86_64"))]
pub(crate) const fn restore_power(_what: &'static str) {}

// The boot options an architecture's bring-up reads before its traps are
// installed, and stage 3's check of what it chose: x86-64's `ferrix.epp`
// (`L.x86_64.182`); none on Arm.
#[cfg(target_arch = "x86_64")]
pub use x86_64::{check_boot_options, read_boot_options};

/// Nothing to read: the Arm kernels take no boot option before their traps.
#[cfg(not(target_arch = "x86_64"))]
pub const fn read_boot_options(_view: &ferrix_bootinfo::BootView<'_>) {}

/// Nothing to check: [`read_boot_options`] chose nothing here.
#[cfg(not(target_arch = "x86_64"))]
pub const fn check_boot_options(_view: &ferrix_bootinfo::BootView<'_>) -> Result<(), &'static str> {
    Ok(())
}

// The switches that left DS and ES unloaded, 0 over 0, for the fast path's
// counts (OPAQUE-KERNEL.md §9.8, 3b, the DS/ES skip); none on Arm.
#[cfg(target_arch = "aarch64")]
pub use aarch64::selector_skips_total;
#[cfg(target_arch = "arm")]
pub use armv7a::selector_skips_total;
#[cfg(target_arch = "x86_64")]
pub use x86_64::selector_skips_total;

// A signal return page, for an architecture without a vDSO its programs
// could read: ARMv7-A's (`syscall::sigpage`, F-48).
#[cfg(target_arch = "arm")]
pub use armv7a::sigpage_code;

/// No signal return page: the vDSO has the trampoline here.
#[cfg(not(target_arch = "arm"))]
pub const fn sigpage_code() -> Option<&'static [u8]> {
    None
}

// What the architecture decodes and decides on its own from values the
// machine hands it -- trap syndromes, the console's description, the
// interrupt controller's masking -- checked at boot against built inputs
// (`aarch64/check.rs`, `armv7a/check.rs`). x86-64's is for that
// architecture's pass to write; until then it checks what it decides of the
// clock (`x86_64/clock/check.rs`).
#[cfg(target_arch = "aarch64")]
pub use aarch64::check_machine;
#[cfg(target_arch = "arm")]
pub use armv7a::check_machine;
#[cfg(target_arch = "x86_64")]
pub use x86_64::check_machine;

/// What the interrupt controller's concurrent-change check found (F-50):
/// see `arm_common::gicv2::check`.
#[derive(Clone, Copy, Debug, Default)]
pub struct DistributorCheck {
    /// The two neighbouring lines raced, when the check ran.
    pub lines: Option<(u32, u32)>,
    /// Rounds run.
    pub rounds: u32,
    /// Rounds in which a line lost its priority, target or configuration.
    pub lost: u32,
    /// Why the check did not run, when it did not.
    pub skipped: Option<&'static str>,
}

// Two cores changing neighbouring lines of a GICv2 distributor at once lose
// neither change: after device discovery, so that it can pick lines no
// device names.
#[cfg(target_arch = "aarch64")]
pub use aarch64::check_distributor;
#[cfg(target_arch = "arm")]
pub use armv7a::check_distributor;

/// The distributor check: x86-64 has no GICv2 to check.
///
/// # Errors
///
/// None.
#[cfg(target_arch = "x86_64")]
pub const fn check_distributor() -> Result<DistributorCheck, &'static str> {
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
pub use aarch64::init_watchdogs;
#[cfg(target_arch = "aarch64")]
pub use aarch64::start_watchdogs;
#[cfg(target_arch = "arm")]
pub use armv7a::init_watchdogs;
#[cfg(target_arch = "arm")]
pub use armv7a::start_watchdogs;
#[cfg(target_arch = "x86_64")]
pub use x86_64::init_watchdogs;
#[cfg(target_arch = "x86_64")]
pub use x86_64::start_watchdogs;
// The program that takes its bootstrap handle after an `execve`, for the
// check of init's native calls (`docs/INIT.md` §6, K3); and the one that
// answers `getuid`, for the check that a native process runs as its creator.
#[cfg(target_arch = "aarch64")]
pub use aarch64::{USER_BOOTSTRAP_PROGRAM, USER_GETUID_PROGRAM};
#[cfg(target_arch = "arm")]
pub use armv7a::{USER_BOOTSTRAP_PROGRAM, USER_GETUID_PROGRAM};
#[cfg(target_arch = "x86_64")]
pub use x86_64::{USER_BOOTSTRAP_PROGRAM, USER_GETUID_PROGRAM};
// The programs stage 13's `CLONE_INTO_CGROUP` and scoped OOM kill checks run.
#[cfg(target_arch = "aarch64")]
pub use aarch64::{USER_INTO_CGROUP_PROGRAM, USER_OOM_PROGRAM};
#[cfg(target_arch = "arm")]
pub use armv7a::{USER_INTO_CGROUP_PROGRAM, USER_OOM_PROGRAM};
#[cfg(target_arch = "x86_64")]
pub use x86_64::{USER_INTO_CGROUP_PROGRAM, USER_OOM_PROGRAM};
// Signal delivery: the register context the way back to user mode loads, the
// architecture's signal frame, and the signal a user-mode fault becomes.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::fault_signal;
#[cfg(target_arch = "aarch64")]
pub use aarch64::{
    SIGNAL_RED_ZONE, USER_DETHREAD_PROGRAM, USER_EXITS_PROGRAM, USER_HANDOFF_PROGRAM,
    USER_SIGNAL_PROGRAM, USER_STOPPED_PROGRAM, USER_SYSLOG_PROGRAM, USER_THREAD_PROGRAM,
    USER_XSTATE_PROGRAM, UserContext, restore_signal_frame, setup_signal_frame,
};
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::fault_signal;
#[cfg(target_arch = "arm")]
pub use armv7a::{
    SIGNAL_RED_ZONE, USER_DETHREAD_PROGRAM, USER_EXITS_PROGRAM, USER_HANDOFF_PROGRAM,
    USER_SIGNAL_PROGRAM, USER_STOPPED_PROGRAM, USER_SYSLOG_PROGRAM, USER_THREAD_PROGRAM,
    USER_XSTATE_PROGRAM, UserContext, restore_signal_frame, setup_signal_frame,
};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::fault_signal;
#[cfg(target_arch = "x86_64")]
pub use x86_64::{
    ARCH, COMPAT_USER_END, EPOLL_EVENT_BYTES, Irq, NAME, OPEN_FLAGS, STAT_LAYOUT, TrapFrame,
    USER_ARGUMENT_PROGRAM, USER_COW_PROGRAM, USER_EXEC_PROGRAM, USER_FAULT_PROGRAM,
    USER_FORK_PROGRAM, USER_KERNEL_TIME_PROGRAM, USER_MPROTECT_PROGRAM, USER_NAMESPACE_PROGRAM,
    USER_NATIVE_PROGRAM, USER_SHARED_PROGRAM, USER_SPIN_PROGRAM, USER_TEST_PROGRAM,
    USER_TEST_STATUS, UserState, audit_arch, counter_hz, enter_user, image_abi, interrupts_enabled,
    read_console_byte, reset_user_state, resume_user, set_thread_area, syscall_rollback_value,
    take_console_byte, thread_area, user_base, user_hwcaps, user_platform,
};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{
    CpuStarter, PageEncoding, advance_past_breakpoint, check_root_tags, classify,
    console_receive_irq, cpu_local, cpu_local_register, describe_cpus, drain_console,
    enable_console_receive, flush_tlb, forbid_user_access, frame_pointer, hardware_id,
    identity_root, install_user_root, ipi_irq, last_root_write, mask_interrupt, msi_doorbell,
    permit_user_access, prepare_stack, prepare_user_root, report_trap, restore_user_state,
    root_tag_now, save_user_state, send_ipi_to_others, service_interrupts, set_cpu_local,
    switch_to, system_call, timer_arm, timer_disarm, timer_disarm_fired, timer_irq,
    uninstall_user_root, unmask_interrupt,
};
#[cfg(target_arch = "x86_64")]
pub use x86_64::{
    FAST_WRITE_READ, UserRegs, counter_now, decode_compat_syscall, decode_syscall,
    disable_interrupts, enable_interrupts, hardware_random, msi_allocate, power_off, reset,
    shutdown,
};
#[cfg(target_arch = "x86_64")]
pub use x86_64::{
    SIGNAL_RED_ZONE, USER_DETHREAD_PROGRAM, USER_EXITS_PROGRAM, USER_HANDOFF_PROGRAM,
    USER_SIGNAL_PROGRAM, USER_STOPPED_PROGRAM, USER_SYSLOG_PROGRAM, USER_THREAD_PROGRAM,
    USER_XSTATE_PROGRAM, UserContext, restore_signal_frame, setup_signal_frame,
};
#[cfg(target_arch = "x86_64")]
pub use x86_64::{
    TLB_FLUSH_IS_BROADCAST, USER_STEP_PROGRAM, USER_STEP_STATUS, breakpoint, console,
    drop_identity_map, halt, identity_map_live, init_console, init_interrupts, init_traps,
    kernel_write_protected, wait_for_interrupt, wait_for_work,
};
// The scoped TLB shootdown: one page invalidated, one processor interrupted,
// and the program that checks it from user mode.
#[cfg(target_arch = "aarch64")]
pub use aarch64::USER_RMAP_PROGRAM;
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{flush_tlb_page, send_ipi_to};
#[cfg(target_arch = "arm")]
pub use armv7a::USER_RMAP_PROGRAM;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::{flush_tlb_page, send_ipi_to};
#[cfg(target_arch = "x86_64")]
pub use x86_64::USER_RMAP_PROGRAM;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{flush_tlb_page, send_ipi_to};

// Ordering memory a device reaches by DMA, and a register write after it (F-44).
#[cfg(target_arch = "aarch64")]
pub use aarch64::dma_barrier;
#[cfg(target_arch = "arm")]
pub use armv7a::dma_barrier;
#[cfg(target_arch = "x86_64")]
pub use x86_64::dma_barrier;

// A word of this processor's own record, read and changed atomically against
// interrupts and migration: the preemption count (`sched::preempt`).
mod percpu;
pub use percpu::this_cpu_add;
pub(crate) use percpu::this_cpu_read;

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

// The vDSO's code, whether it can read the counter itself, and the program
// the boot check holds it to: none of the three on an architecture whose
// vDSO nobody has written.
#[cfg(target_arch = "aarch64")]
pub use aarch64::{USER_VDSO_PROGRAM, vdso_can_read_counter, vdso_spec};
#[cfg(target_arch = "arm")]
pub use armv7a::{USER_VDSO_PROGRAM, vdso_can_read_counter, vdso_spec};
#[cfg(target_arch = "x86_64")]
pub use x86_64::{USER_VDSO_PROGRAM, vdso_can_read_counter, vdso_spec};

// Whether a program can read the counter the kernel's clock counts, for a
// driver's own timings: a separate question from the vDSO's, which only
// x86-64 has written.
#[cfg(target_arch = "aarch64")]
pub use aarch64::ring3_reads_counter;
#[cfg(target_arch = "arm")]
pub use armv7a::ring3_reads_counter;
#[cfg(target_arch = "x86_64")]
pub use x86_64::ring3_reads_counter;

// The counter the kernel-time stamps read at every entry from user mode and
// every return (`sched::vtime`), and whether it is the clock's and cheap.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::{cheap_counter, cheap_counter_is_clock};
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::{cheap_counter, cheap_counter_is_clock};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{cheap_counter, cheap_counter_is_clock};

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

// That every processor reports a program's x87 error as the exception,
// checked once all are online: x86-64's `CR0.NE`, with `MP` set and `EM` and
// `TS` clear (L.x86_64.178). The Arm architectures have no legacy route
// for a floating-point error to choose against.
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::check_x87_controls;

/// That every processor reports a program's x87 error as the exception,
/// checked: nothing to set here.
///
/// # Errors
///
/// None.
#[cfg(not(target_arch = "x86_64"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "x86-64's check returns what failed, and the caller is shared"
)]
pub(crate) const fn check_x87_controls() -> Result<(), &'static str> {
    Ok(())
}

// That every processor takes a machine check as the exception, checked once
// all are online: x86-64's `CR4.MCE` (L.x86_64.175, F-73). On the Arm
// architectures an external abort or SError is taken through the vectors
// with nothing to enable.
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::check_machine_check_enabled;

/// That every processor takes a machine check, checked: nothing to enable
/// here.
///
/// # Errors
///
/// None.
#[cfg(not(target_arch = "x86_64"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "x86-64's check returns what failed, and the caller is shared"
)]
pub(crate) const fn check_machine_check_enabled() -> Result<(), &'static str> {
    Ok(())
}

// Cache maintenance for a device that does not snoop the caches: the
// DK board's display controller reads a framebuffer straight from memory, so
// whatever a program drew has to be written back from the caches to the point
// of coherency before the controller is told to read it.
#[cfg(target_arch = "aarch64")]
pub use aarch64::clean_for_device;
#[cfg(target_arch = "arm")]
pub use armv7a::clean_for_device;
#[cfg(target_arch = "x86_64")]
pub use x86_64::clean_for_device;

// Cache maintenance before a reset may discard the caches: the region the
// kernel log is kept in across a reset (`console::pstore`), written back to
// memory by the panic path. Every architecture cleans by address over the
// region alone: `dc cvac`, `DCCMVAC`, `clflush`.
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::write_back_for_reset;
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::write_back_for_reset;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::write_back_for_reset;

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
pub use aarch64::flush_for_device;
#[cfg(target_arch = "arm")]
pub use armv7a::flush_for_device;
#[cfg(target_arch = "x86_64")]
pub use x86_64::flush_for_device;

// How a framebuffer is mapped: write-combining where the architecture has it.
// On x86-64 that is decided by the boot processor's PAT as it read back
// before the map (`x86_64::framebuffer`); the Arm kernels map their constant.
#[cfg(target_arch = "aarch64")]
use aarch64::FRAMEBUFFER_FLAGS;
#[cfg(target_arch = "arm")]
use armv7a::FRAMEBUFFER_FLAGS;
#[cfg(target_arch = "x86_64")]
pub use x86_64::{check_hwp_processors, user_combining};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::{flush_cache_lines, framebuffer_flags};
#[cfg(target_arch = "x86_64")]
pub use x86_64::{flush_window, program_memory_types};

/// No write-combining user mapping of memory yet on the Arm architectures:
/// each one's attribute (normal non-cacheable) is still to be argued (K6,
/// ledger 2352 K6 (1)).
#[cfg(not(target_arch = "x86_64"))]
pub const fn user_combining() -> bool {
    false
}

/// Nothing to flush: no VMO is made write-combining here ([`user_combining`]).
#[cfg(not(target_arch = "x86_64"))]
pub(crate) const fn flush_cache_lines(_virt: u64, _len: u64) {}

/// Nothing to count: the Arm kernels ask no hardware P-states of anything
/// (`L.x86_64.177` is x86-64's).
#[cfg(not(target_arch = "x86_64"))]
pub const fn check_hwp_processors() -> Result<(), &'static str> {
    Ok(())
}

/// The Arm kernels' framebuffer flags, normal non-cacheable: their constant.
#[cfg(not(target_arch = "x86_64"))]
pub(crate) const fn framebuffer_flags() -> ferrix_paging::MapFlags {
    FRAMEBUFFER_FLAGS
}

/// No page attribute table to program before the framebuffer is mapped.
#[cfg(not(target_arch = "x86_64"))]
pub const fn program_memory_types() {}

/// Nothing to flush before the first store: the Arm kernels keep the
/// framebuffer's mapping as it was.
#[cfg(not(target_arch = "x86_64"))]
pub const fn flush_window(_virt: u64, _len: u64) {}

// Whether `ferrix.efifb` may lend the framebuffer to a ring-3 driver: only
// where a user mapping of an aperture can take the kernel's own type for it.
#[cfg(target_arch = "aarch64")]
pub use aarch64::LENDS_FRAMEBUFFER;
#[cfg(target_arch = "arm")]
pub use armv7a::LENDS_FRAMEBUFFER;
#[cfg(target_arch = "x86_64")]
pub use x86_64::LENDS_FRAMEBUFFER;

// The lent firmware framebuffer's write-combining, x86-64's alone
// (`docs/DISPLAY.md` §8): a PAT entry and the MTRRs' type read back, and the
// fence that empties the write-combining buffers. The Arm kernels lend no
// framebuffer and keep their panic screen's mapping as it was.
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::drain_write_combining;
#[cfg(target_arch = "x86_64")]
pub use x86_64::mtrr_type;
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::pat;

/// Nothing to drain: no framebuffer is mapped write-combining here.
#[cfg(not(target_arch = "x86_64"))]
pub(crate) const fn drain_write_combining() {}

/// No page attribute table here.
#[cfg(not(target_arch = "x86_64"))]
pub(crate) const fn pat() -> Option<u64> {
    None
}

/// No MTRRs here.
#[cfg(not(target_arch = "x86_64"))]
pub const fn mtrr_type(_phys: u64, _len: u64) -> Option<u8> {
    None
}

// Full-entropy bytes firmware hands out, where it has a TRNG to ask.
#[cfg(target_arch = "aarch64")]
pub use aarch64::firmware_entropy;
#[cfg(target_arch = "arm")]
pub use armv7a::firmware_entropy;
#[cfg(target_arch = "x86_64")]
pub use x86_64::firmware_entropy;

// Instructions the kernel wrote into memory, made the ones every processor
// fetches there: for a page about to be mapped executable in user mode. An
// Arm core's instruction cache does not see what its data side wrote, nor
// what another core's did, until it is told to.
#[cfg(target_arch = "aarch64")]
pub use aarch64::sync_instructions;
#[cfg(target_arch = "arm")]
pub use armv7a::sync_instructions;
#[cfg(target_arch = "x86_64")]
pub use x86_64::sync_instructions;

// Where a task that is not running keeps its entry frame and the start of its
// call chain, for `sched::inspect` (`docs/DEBUG.md` §1.1).
pub(crate) mod inspect;
#[cfg(target_arch = "aarch64")]
pub(crate) use aarch64::inspect::{SWITCH_FRAME, entry_frame};
#[cfg(target_arch = "arm")]
pub(crate) use armv7a::inspect::{SWITCH_FRAME, entry_frame};
#[cfg(target_arch = "x86_64")]
pub(crate) use x86_64::inspect::{SWITCH_FRAME, entry_frame};
