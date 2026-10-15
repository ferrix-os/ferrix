//! The paranoid entries' boot check: an NMI in the kernel, and hardware
//! breakpoints in the `SYSCALL` trampoline's ring-0 stretches and on the
//! `#DB` handler's own path, each survived where it lands.
//!
//! Verification, not the entry: a file of its own so that the manifest counts
//! it as the test it is (`tools/common/data/certification-item.json`,
//! `test_file_patterns`). The system call window's check runs a real program
//! -- [`ferrix_kernel_core::arch::x86_64::USER_TEST_PROGRAM`], built into an ELF and loaded by
//! the Linux personality's loader -- because only a program's own `syscall`
//! reaches the trampoline with its stack and `GS`. That is a fixture reached
//! from the load ring, which a verification file may do and the core's
//! product code may not (`docs/certification/FINDINGS.md`, F-33): the entry
//! it checks names nothing above the core.
//!
//! A child of the entry's module, so it reads the entry's counters without
//! the entry exporting them.

#![cfg(target_arch = "x86_64")]

use core::hint::spin_loop;
use core::sync::atomic::Ordering;

use ferrix_sched::{CpuSet, NICE_0_WEIGHT};
use ferrix_sync::IrqControl;

use ferrix_kernel_core::arch::x86_64::cpu;
use ferrix_kernel_core::arch::x86_64::paranoid::{BREAKPOINTS, FOREIGN_GS, HOOK_RUNS, NMIS, USER_NMIS, debug_hook};
use crate::console::println;
use crate::sync::SpinLock;

/// The instruction the nesting check breaks on first.
#[inline(never)]
fn breakpoint_target() -> u64 {
    core::hint::black_box(HOOK_RUNS.load(Ordering::Relaxed))
}

/// `DR7`'s local enable for `slot`. The slot's type and length fields left
/// zero mean an instruction breakpoint, one byte.
const fn dr7_local(slot: usize) -> u64 {
    1 << (slot * 2)
}

/// Kernel `#DB`s taken so far for breakpoint `slot`.
fn hits(slot: usize) -> u64 {
    BREAKPOINTS
        .get(slot)
        .map_or(0, |count| count.load(Ordering::Relaxed))
}

unsafe extern "C" {
    /// The trampoline's `sysretq`, after the `swapgs` back, still on the
    /// program's stack.
    static ferrix_syscall_sysret: [u8; 0];
}

/// How long the system call window check may take, program and all.
const WINDOW_PATIENCE_NANOS: u64 = 30_000_000_000;

/// How long an NMI sent to this processor has to arrive.
const NMI_PATIENCE_NANOS: u64 = 1_000_000_000;

/// What the pinned task of [`check_system_call_window`] found: the program's
/// status, or why it has none. `None` while it runs.
static WINDOW: SpinLock<Option<Result<i32, &'static str>>> = SpinLock::new(None);

/// Whether the window check's program is [`HIGH_GS_PROGRAM`]: read by the
/// pinned task, which takes no argument but its processor.
static WINDOW_HIGH_GS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The `GS` base the two programs below give themselves: the highest user
/// page. Its upper half is not zero, so the paranoid stub's sign test reads
/// a word a base below 4 GiB never gave it, and nothing is mapped there, so
/// a handler that ran on it as its per-CPU record would fault.
const HIGH_GS_BASE: u64 = 0x0000_7fff_ffff_f000;

/// What [`HIGH_GS_PROGRAM`] exits with.
const HIGH_GS_STATUS: i32 = 43;

/// A program that sets its `GS` base to [`HIGH_GS_BASE`] with
/// `arch_prctl(ARCH_SET_GS)`, makes two more calls and exits with
/// [`HIGH_GS_STATUS`], or with 9 if the base was refused: the window check's
/// program with a base a program chose.
///
/// ```text
///   movl $158, %eax ; movl $0x1001, %edi ; movabsq $0x00007ffffffff000, %rsi
///   syscall ; testq %rax, %rax ; jne 1f
///   movl $39, %eax ; syscall
///   movl $39, %eax ; syscall
///   movl $231, %eax ; movl $43, %edi ; syscall
///   1: movl $231, %eax ; movl $9, %edi ; syscall
/// ```
const HIGH_GS_PROGRAM: &[u8] = &[
    0xb8, 0x9e, 0x00, 0x00, 0x00, 0xbf, 0x01, 0x10, 0x00, 0x00, 0x48, 0xbe, 0x00, 0xf0, 0xff, 0xff,
    0xff, 0x7f, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75, 0x1a, 0xb8, 0x27, 0x00, 0x00, 0x00,
    0x0f, 0x05, 0xb8, 0x27, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x2b,
    0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x09, 0x00, 0x00, 0x00, 0x0f,
    0x05,
];

/// Where [`SPIN_PROGRAM`] maps the word it waits on.
const SPIN_WORD: u64 = 0x5200_0000;

/// What [`SPIN_PROGRAM`] exits with.
const SPIN_STATUS: i32 = 44;

/// A program that sets its `GS` base to [`HIGH_GS_BASE`], maps a page at
/// [`SPIN_WORD`] and spins in ring 3 until the word there is not zero, then
/// exits with [`SPIN_STATUS`]; 9 if a call failed. What an NMI is sent into.
///
/// ```text
///   movl $158, %eax ; movl $0x1001, %edi ; movabsq $0x00007ffffffff000, %rsi
///   syscall ; testq %rax, %rax ; jne 1f
///   movl $0x52000000, %edi ; movl $4096, %esi ; movl $3, %edx ; movl $0x32, %r10d
///   movq $-1, %r8 ; xorl %r9d, %r9d ; movl $9, %eax
///   syscall ; cmpq $0x52000000, %rax ; jne 1f
///   2: pause ; cmpl $0, (%rax) ; je 2b
///   movl $231, %eax ; movl $44, %edi ; syscall
///   1: movl $231, %eax ; movl $9, %edi ; syscall
/// ```
const SPIN_PROGRAM: &[u8] = &[
    0xb8, 0x9e, 0x00, 0x00, 0x00, 0xbf, 0x01, 0x10, 0x00, 0x00, 0x48, 0xbe, 0x00, 0xf0, 0xff, 0xff,
    0xff, 0x7f, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75, 0x41, 0xbf, 0x00, 0x00, 0x00, 0x52,
    0xbe, 0x00, 0x10, 0x00, 0x00, 0xba, 0x03, 0x00, 0x00, 0x00, 0x41, 0xba, 0x32, 0x00, 0x00, 0x00,
    0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, 0x45, 0x31, 0xc9, 0xb8, 0x09, 0x00, 0x00, 0x00, 0x0f,
    0x05, 0x48, 0x3d, 0x00, 0x00, 0x00, 0x52, 0x75, 0x13, 0xf3, 0x90, 0x83, 0x38, 0x00, 0x74, 0xf9,
    0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x2c, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00,
    0x00, 0xbf, 0x09, 0x00, 0x00, 0x00, 0x0f, 0x05,
];

/// The boot checks for these entries, each of which halts the machine or
/// fails if the entry is not there.
///
/// # Errors
///
/// What failed, for the stage 3 report.
/// Verifies: `L.x86_64.80`
pub(crate) fn run() -> Result<(), &'static str> {
    let nmis = check_nmi()?;
    let (entries, returns, status) = check_system_call_window(false)?;
    let (high_entries, high_returns, _) = check_system_call_window(true)?;
    let user_nmis = check_nmi_in_a_program()?;
    let hook_runs = check_breakpoints_do_not_nest()?;
    println!(
        "  nmi      {nmis} NMI sent to this processor with interrupts masked was taken on its \
         own stack and returned from"
    );
    println!(
        "  debug    {entries} breakpoints on the system call entry and {returns} on its \
         sysretq, each on the program's stack, found the kernel's GS; the program exited \
         with {status}"
    );
    match user_nmis {
        Some(taken) => println!(
            "  debug    beside a program whose GS base is {HIGH_GS_BASE:#x}: {high_entries} \
             breakpoints on the system call entry and {high_returns} on its sysretq found the \
             kernel's GS, and {taken} NMIs taken in the program on another processor did"
        ),
        None => println!(
            "  debug    beside a program whose GS base is {HIGH_GS_BASE:#x}: {high_entries} \
             breakpoints on the system call entry and {high_returns} on its sysretq found the \
             kernel's GS; one processor, so no NMI was sent into the program"
        ),
    }
    println!(
        "  debug    a breakpoint on code the #DB handler runs did not fire inside it: \
         {hook_runs} handler runs, one stack"
    );
    Ok(())
}

/// An NMI taken in the kernel is survived.
///
/// Sent to this processor through its own local APIC with interrupts masked,
/// which an NMI ignores. Without the paranoid entry vector 2 is fatal.
/// Verifies: `L.x86_64.24`
fn check_nmi() -> Result<u64, &'static str> {
    let before = NMIS.load(Ordering::Relaxed);
    let foreign = FOREIGN_GS.load(Ordering::Relaxed);

    let saved = <ferrix_kernel_core::arch::x86_64::Irq as IrqControl>::disable();
    let sent = ferrix_kernel_core::arch::x86_64::apic::send_nmi_to_self();
    let deadline = crate::timer::now_nanos().saturating_add(NMI_PATIENCE_NANOS);
    while sent.is_ok()
        && NMIS.load(Ordering::Relaxed) == before
        && crate::timer::now_nanos() < deadline
    {
        spin_loop();
    }
    <ferrix_kernel_core::arch::x86_64::Irq as IrqControl>::restore(saved);

    sent?;
    let taken = NMIS.load(Ordering::Relaxed).wrapping_sub(before);
    if taken == 0 {
        return Err("an NMI sent to this processor never arrived");
    }
    if FOREIGN_GS.load(Ordering::Relaxed) != foreign {
        return Err("an NMI handler ran with a GS that names no processor's record");
    }
    Ok(taken)
}

/// A `#DB` in the `SYSCALL` trampoline's ring-0 stretches on the program's
/// stack is survived, with the kernel's `GS`.
///
/// Instruction breakpoints on the stub's first instruction -- `swapgs`, with
/// the program's `GS` and stack -- and on its `sysretq`, after the `swapgs`
/// back, while [`ferrix_kernel_core::arch::x86_64::USER_TEST_PROGRAM`] writes a line and exits. Debug
/// registers are per processor, so the program and the task that arms them
/// are pinned to one. Returns the hits at each, and the program's status.
/// Verifies: `L.x86_64.25`, `L.x86_64.52`, `L.x86_64.87`
fn check_system_call_window(high_gs: bool) -> Result<(u64, u64, i32), &'static str> {
    WINDOW_HIGH_GS.store(high_gs, Ordering::Release);
    let cpu = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let entries = hits(0);
    let returns = hits(1);
    let foreign = FOREIGN_GS.load(Ordering::Relaxed);
    *WINDOW.lock() = None;

    let task = crate::sched::spawn_on(
        "syscall window",
        window_task,
        cpu,
        NICE_0_WEIGHT,
        cpu,
        CpuSet::of(cpu),
    )?;
    let deadline = crate::timer::now_nanos().saturating_add(WINDOW_PATIENCE_NANOS);
    let outcome = loop {
        if let Some(outcome) = *WINDOW.lock() {
            break outcome;
        }
        if crate::timer::now_nanos() > deadline {
            return Err("the system call window check never finished");
        }
        crate::sched::sleep_for(1_000_000);
    };
    while !task.is_dead() && crate::timer::now_nanos() < deadline {
        crate::sched::sleep_for(1_000_000);
    }
    drop(task);
    let _ = crate::sched::reap();

    let status = outcome?;
    let entries = hits(0).wrapping_sub(entries);
    let returns = hits(1).wrapping_sub(returns);
    if FOREIGN_GS.load(Ordering::Relaxed) != foreign {
        return Err("a #DB in the system call window ran with the program's GS");
    }
    // Two calls, `write` and `exit_group`, and only the first returns.
    if entries < 2 || returns < 1 {
        return Err("a breakpoint in the system call window did not fire");
    }
    let expected = if high_gs {
        HIGH_GS_STATUS
    } else {
        ferrix_kernel_core::arch::x86_64::USER_TEST_STATUS
    };
    if status != expected {
        return Err("the program with breakpoints in its system calls exited wrongly");
    }
    Ok((entries, returns, status))
}

/// The pinned half of [`check_system_call_window`], on processor `cpu`.
fn window_task(cpu: usize) {
    let outcome = run_with_window_breakpoints(cpu);
    *WINDOW.lock() = Some(outcome);
}

/// Arm the two breakpoints on this processor, run the program on it, disarm.
/// Verifies: `L.x86_64.53`
fn run_with_window_breakpoints(cpu: usize) -> Result<i32, &'static str> {
    use crate::syscall::exec::{Executable, load_executable};

    let file = crate::syscall::image::build_with(
        ferrix_elf::Class::Elf64,
        ferrix_kernel_core::arch::x86_64::ARCH.elf_machine(),
        crate::syscall::image::Shape::Good,
        if WINDOW_HIGH_GS.load(Ordering::Acquire) {
            HIGH_GS_PROGRAM
        } else {
            ferrix_kernel_core::arch::x86_64::USER_TEST_PROGRAM
        },
    );
    let program = Executable {
        image: crate::syscall::load::Source::Bytes(&file),
        exe: b"/window",
        exec_fn: b"/window",
        set_ids: crate::fs::SetIds::NONE,
        interpreter: None,
    };
    let process = load_executable(
        program,
        &[b"/window"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "the system call window program could not be loaded")?;

    // `LSTAR`'s target, whose first instruction is `swapgs` on the program's
    // stack. Taken from `syscall`, whose declaration is the symbol's only one:
    // a second, of another type, is renamed `.1` by the release profile's link
    // and left undefined.
    let entry = ferrix_kernel_core::arch::x86_64::syscall::stub_address();
    let sysret = (&raw const ferrix_syscall_sysret) as u64;
    // SAFETY: (PROBE) an instruction of the trampoline, not in the paranoid entry nor
    // on an IST stack, which is what a `#DB` there needs; unarmed until DR7.
    unsafe { cpu::write_breakpoint_address(0, entry) };
    // SAFETY: (PROBE) as above, for the trampoline's `sysretq`.
    unsafe { cpu::write_breakpoint_address(1, sysret) };
    // SAFETY: (PROBE) arms the two slots just written, and nothing else.
    unsafe { cpu::write_dr7(dr7_local(0) | dr7_local(1)) };

    let status = crate::syscall::process::start_on(&process, Some(cpu))
        .ok()
        .and_then(|_task| process.wait_for_exit(u64::MAX));

    // SAFETY: (PROBE) disarming changes nothing but that they stop firing. This task
    // is pinned, so this is the processor they were armed on.
    unsafe { cpu::write_dr7(0) };

    // The GS base the program ran with, parked on the kernel's side. If it were
    // a per-CPU record, both halves of `swapgs` would hold the same address,
    // and a handler that skipped the swap would find the kernel's GS anyway:
    // the breakpoints above would prove nothing.
    // SAFETY: (CONTEXT) a kernel task, with the kernel's GS, outside the trampoline.
    let program_gs = unsafe { ferrix_kernel_core::arch::x86_64::syscall::program_gs_base() };
    if is_cpu_record(program_gs) {
        return Err("a program ran with a per-CPU record as its GS base");
    }
    status.ok_or("the system call window program could not be started")
}

/// An NMI taken while a program whose `GS` base is [`HIGH_GS_BASE`] runs in
/// ring 3 is survived, with the kernel's `GS`: the entry reads a base whose
/// upper half is not zero, takes it for the program's by its sign, and
/// swaps. [`SPIN_PROGRAM`] is pinned to another processor and NMIs are sent
/// there until three have been taken from ring 3; then its word is written,
/// and it must exit with its status. Answers the NMIs taken in it, or `None`
/// on a machine with one processor.
///
/// Verifies: `L.x86_64.24`
fn check_nmi_in_a_program() -> Result<Option<u64>, &'static str> {
    use crate::syscall::exec::{Executable, load_executable};

    let here = crate::smp::this_cpu()
        .ok_or("the per-CPU register is not installed")?
        .logical;
    let Some(topology) = crate::smp::topology() else {
        return Ok(None);
    };
    let Some(other) = topology.cpus().iter().rev().find(|cpu| cpu.logical != here) else {
        return Ok(None);
    };
    let file = crate::syscall::image::build_with(
        ferrix_elf::Class::Elf64,
        ferrix_kernel_core::arch::x86_64::ARCH.elf_machine(),
        crate::syscall::image::Shape::Good,
        SPIN_PROGRAM,
    );
    let program = Executable {
        image: crate::syscall::load::Source::Bytes(&file),
        exe: b"/nmi-spin",
        exec_fn: b"/nmi-spin",
        set_ids: crate::fs::SetIds::NONE,
        interpreter: None,
    };
    let process = load_executable(
        program,
        &[b"/nmi-spin"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "the NMI check's program could not be loaded")?;
    let before = USER_NMIS.load(Ordering::Relaxed);
    let foreign = FOREIGN_GS.load(Ordering::Relaxed);
    let task = crate::syscall::process::start_on(&process, Some(other.logical))
        .map_err(|_| "the NMI check's program could not be started")?;
    let deadline = crate::timer::now_nanos().saturating_add(WINDOW_PATIENCE_NANOS);
    let mut sent = Ok(());
    while sent.is_ok()
        && USER_NMIS.load(Ordering::Relaxed).wrapping_sub(before) < 3
        && crate::timer::now_nanos() < deadline
    {
        crate::sched::sleep_for(1_000_000);
        sent = ferrix_kernel_core::arch::x86_64::apic::send_nmi_to(other.hardware_id as u32);
    }
    let taken = USER_NMIS.load(Ordering::Relaxed).wrapping_sub(before);
    // Let it go, whatever happened: its word, once its page is mapped.
    let mut released = false;
    while !released && crate::timer::now_nanos() < deadline {
        released = crate::syscall::uaccess::put_u32(process.space(), SPIN_WORD, 1).is_ok();
        if !released {
            crate::sched::sleep_for(1_000_000);
        }
    }
    let status = process.wait_for_exit(deadline);
    drop(task);
    sent?;
    if FOREIGN_GS.load(Ordering::Relaxed) != foreign {
        return Err("an NMI taken in a program with a high GS base ran with that base as GS");
    }
    if taken < 3 {
        return Err("NMIs sent to a processor running a program never arrived in ring 3");
    }
    if status != Some(SPIN_STATUS) {
        return Err("the program NMIs were sent into did not exit with its status");
    }
    Ok(Some(taken))
}

/// Whether `address` is one of the processors' per-CPU records.
fn is_cpu_record(address: u64) -> bool {
    crate::smp::topology().is_some_and(|topology| {
        topology
            .cpus()
            .iter()
            .any(|cpu| core::ptr::from_ref(cpu) as u64 == address)
    })
}

/// A breakpoint on code the `#DB` handler itself runs does not fire inside it.
///
/// Breakpoints on [`breakpoint_target`] and on [`debug_hook`], which every
/// kernel `#DB` handler calls. The target's `#DB` runs the hook with no
/// breakpoint firing, because the entry cleared `DR7`; calling the hook here
/// shows its breakpoint was armed. Without the clear, the hook's `#DB` would
/// land on the top of the stack the target's handler is still using, which the
/// occupancy count reports as FX-9006. Returns the hook's runs: three, two in
/// handlers and one here.
/// Verifies: `L.x86_64.26`
fn check_breakpoints_do_not_nest() -> Result<u64, &'static str> {
    let targets = hits(0);
    let hooks = hits(2);
    let runs = HOOK_RUNS.load(Ordering::Relaxed);

    // Masked, so that arming, both breakpoints and disarming happen on one
    // processor.
    let saved = <ferrix_kernel_core::arch::x86_64::Irq as IrqControl>::disable();
    // SAFETY: (PROBE) a kernel function outside the paranoid entry and the IST stacks;
    // unarmed until DR7, and disarmed below before interrupts open again.
    unsafe { cpu::write_breakpoint_address(0, breakpoint_target as fn() -> u64 as usize as u64) };
    // SAFETY: (PROBE) as above, for the hook every kernel `#DB` handler runs.
    unsafe { cpu::write_breakpoint_address(2, debug_hook as fn() as usize as u64) };
    // SAFETY: (PROBE) arms the two slots just written, and nothing else.
    unsafe { cpu::write_dr7(dr7_local(0) | dr7_local(2)) };
    let _ = core::hint::black_box(breakpoint_target());
    debug_hook();
    // SAFETY: (PROBE) disarming.
    unsafe { cpu::write_dr7(0) };
    <ferrix_kernel_core::arch::x86_64::Irq as IrqControl>::restore(saved);

    if hits(0).wrapping_sub(targets) != 1 {
        return Err("an instruction breakpoint in the kernel did not fire exactly once");
    }
    if hits(2).wrapping_sub(hooks) != 1 {
        return Err("a breakpoint on the #DB handler's path fired other than once");
    }
    let runs = HOOK_RUNS.load(Ordering::Relaxed).wrapping_sub(runs);
    if runs != 3 {
        return Err("the #DB handler did not run once per breakpoint");
    }
    Ok(runs)
}
