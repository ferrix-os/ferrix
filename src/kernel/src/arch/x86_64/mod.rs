//! The x86-64 end of the kernel.

mod apic;
mod clock;
pub(crate) mod console;
mod cpu;
pub(crate) use cpu::clean_for_walker;
pub(crate) use cpu::dma_barrier;
pub(crate) use cpu::{gs_xadd, read_gs_at};
mod gdt;
pub(crate) mod mmio;
mod msi;
mod paranoid;
mod signal;
mod smp;
pub(super) mod speculation;
mod switch;
mod syscall;
pub(crate) use syscall::check::{drive_native_words, drive_system_call};
mod trap;

pub(crate) use cpu::hardware_random;
pub(crate) use signal::{SIGNAL_RED_ZONE, UserContext, restore_signal_frame, setup_signal_frame};

use ferrix_bootinfo::{Arch, BootView};
use ferrix_linux_abi::nr::{self, Syscall};
use ferrix_linux_abi::types::{self, OpenFlagBits};

use crate::early::{EarlyError, EarlyMemory};
use crate::irq::Report;
use crate::sync::SpinLock;

/// Name for log lines.
pub(crate) const NAME: &str = "x86_64";

/// This machine, as the hand-off structure names it.
///
/// The kernel needs it for the same reason the loader does: to refuse an ELF
/// image built for a different architecture. `src/boot/common/uefi/src/arch/` has carried the
/// same constant since stage 1; this is the kernel's copy, and the two are
/// checked against each other by the image simply booting.
pub(crate) const ARCH: Arch = Arch::X86_64;

/// Which `struct stat` the stat calls fill in: x86-64's own, 144 bytes, from
/// `arch/x86/include/uapi/asm/stat.h`. x86-64 kept the layout it grew rather
/// than adopting the generic one, so this is not the AArch64 answer.
/// Whether this architecture's `SYSCALL`-style entry takes step 4's fast
/// path for `channel_write_read` when one is registered
/// (`docs/OPAQUE-KERNEL.md` §9.7): x86-64's alone.
pub(crate) const FAST_WRITE_READ: bool = true;

pub(crate) const STAT_LAYOUT: super::StatLayout = super::StatLayout::Legacy;

/// x86-64's `struct epoll_event`, 12 bytes: `EPOLL_PACKED` in
/// `include/uapi/linux/eventpoll.h`, so `data` follows `events` with no
/// padding.
pub(crate) const EPOLL_EVENT_BYTES: usize = 12;

/// The page table descriptor layout this machine uses.
pub(crate) type PageEncoding = ferrix_paging::x86_64::X86_64;

/// Bring up the early console.
///
/// Nothing to find and nothing to map: the 16550 is at a fixed port, behind
/// I/O space, which has no page tables of its own. The Arm counterparts have
/// to find a UART and map an `MMIO` window first, which is why this takes two
/// arguments it ignores.
pub(crate) fn init_console(
    _view: &BootView<'_>,
    _memory: &mut EarlyMemory,
) -> Result<(), EarlyError> {
    console::init();
    Ok(())
}

pub(crate) use smp::{CpuStarter, describe_cpus, hardware_id};

/// `IA32_GS_BASE`: the base address `GS`-relative accesses are made from.
///
/// The kernel's, for now. Once there is a user mode this is the register
/// `swapgs` exchanges with `IA32_KERNEL_GS_BASE` on every entry and exit, and
/// the per-CPU record moves to whichever of the two the kernel side holds.
const IA32_GS_BASE: u32 = 0xC000_0101;

/// Point this CPU's per-CPU register at `address`.
///
/// # Safety
///
/// (SHARED) `address` must be this processor's own `PerCpu` record, which must live for
/// the rest of the system's life: `cpu_local` hands it back as a reference.
pub(crate) unsafe fn set_cpu_local(address: u64) {
    // SAFETY: (SHARED) `IA32_GS_BASE` exists on every 64-bit x86 and accepts any
    // canonical address, which a kernel pointer is.
    unsafe { cpu::write_msr(IA32_GS_BASE, address) };

    // `SYSCALL` on this processor, now that `GS` names its record -- which
    // `syscall::init` parks for the first `swapgs`, and which the trampoline
    // reaches its stack through. Here because this runs once on every
    // processor, after its GDT: a program is a task that may be resumed on any
    // of them, and the first system call it made on one that skipped this
    // would take `#UD`. It used to run lazily before each program, when a
    // program never left the processor it started on.
    // SAFETY: (ENTRY) this processor's per-CPU record is installed in `GS` above. On
    // a secondary this runs before `init_secondary` loads its own GDT, which is
    // fine: the MSRs only record the selectors, and nothing uses them until a
    // program's first `SYSCALL`, long after that GDT is in place.
    unsafe { syscall::init() };
}

/// The address [`set_cpu_local`] installed on this CPU.
///
/// # Safety
///
/// (SHARED) [`set_cpu_local`] must have run on this CPU. Before it has, `GS` points
/// wherever firmware left it and the load below reads from there.
pub(crate) unsafe fn cpu_local() -> u64 {
    // SAFETY: (SHARED) the caller guarantees `GS` points at a per-CPU record, whose
    // first word is its own address.
    unsafe { cpu::read_gs_word() }
}

/// This CPU's per-CPU register, read from the register rather than through it.
///
/// Safe where [`cpu_local`] is not: before [`set_cpu_local`] has run the value
/// is whatever firmware left there, but reading it cannot fault. For a failure
/// report, which has to name the processor without trusting it.
pub(crate) fn cpu_local_register() -> u64 {
    // SAFETY: (SHARED) `IA32_GS_BASE` exists on every 64-bit x86.
    unsafe { cpu::read_msr(IA32_GS_BASE) }
}
pub(crate) use trap::{
    TrapFrame, advance_past_breakpoint, breakpoint, classify, fault_signal, report_trap,
};

/// Show that the exceptions nothing masks are survived where they land: an NMI
/// in the kernel, and hardware breakpoints in the `SYSCALL` trampoline's ring-0
/// stretches on the program's stack and `GS`. See `paranoid`. Then the
/// exceptions a program raises itself, each of which has to end it with the
/// signal Linux gives it, and `arch_prctl`'s refusals, the one call this
/// entry answers itself. See `trap::check`; and `syscall::check` for the
/// registers a fork child is handed.
///
/// # Errors
///
/// What did not come back, or did not end as it had to.
pub(crate) fn check_exception_entry() -> Result<(), &'static str> {
    paranoid::check::run()?;
    syscall::check::run()?;
    syscall::check::run_decode()?;
    gdt::check::run()?;
    trap::check::run()
}

/// Stage 9: what the switch gives a program back, read by programs in ring 3
/// -- the vector-state contract of the native calls that block, and the `FS`
/// base kept in the task (`docs/OPAQUE-KERNEL.md` §9.8, 3a and 3b). See
/// `switch::check`.
///
/// # Errors
///
/// The first case that read what it must not.
pub(crate) fn check_switch_state() -> Result<(), &'static str> {
    let report = switch::check::run()?;
    if report.xsave {
        crate::console::println!(
            "  vectors  {} wakes from a blocking native call reset the vector registers with \
             their own MXCSR and control word; {} Linux sleeps, preemptions and calls after one \
             kept them",
            report.reset,
            report.kept
        );
    } else {
        crate::console::println!(
            "  vectors  saved with FXSAVE: every switch keeps the vector registers whole, and \
             the reset's cases are not run"
        );
    }
    crate::console::println!(
        "  fsbase   {} switches between two FS bases, each its own; a base cleared by a null \
         selector came back as recorded, and leaked to no program",
        report.traded
    );
    Ok(())
}

/// Install the descriptor tables and the trap handlers.
///
/// Until this runs the kernel is executing on firmware's tables: a fault would
/// enter a handler that stopped existing at `exit_boot_services`, which is a
/// triple fault and a silent reset.
///
/// # Safety
///
/// (ENTRY) Must be called exactly once, on the boot CPU, before interrupts are enabled.
pub(crate) unsafe fn init_traps() {
    // SAFETY: (ENTRY) called once from `kmain`, before anything can fault deliberately.
    unsafe { gdt::init() };
    // SAFETY: (ENTRY) after `gdt::init`, whose kernel code selector every gate names.
    unsafe { trap::init() };
    // After the IDT, so that a fault this turns on is *reported* rather than
    // becoming a triple fault, and before user mode so that it covers every
    // program. Secondary processors inherit it: `smp::secondary_start` copies
    // the boot processor's `CR4`, snapshotted later in `CpuStarter::new`.
    let (smep, smap) = cpu::enable_user_access_protection();
    crate::console::println!(
        "  cpu      ring 0 kept out of user pages: SMEP {}, SMAP {}",
        if smep { "on" } else { "unavailable" },
        if smap { "on" } else { "unavailable" },
    );
    // KASLR's: a moved image is no secret while ring 3 can ask for the IDT's
    // address. Not in a kernel built `--mitigations off`, which does not move.
    let umip = if super::HARDENED {
        if cpu::enable_umip() {
            "on"
        } else {
            "unavailable, so SIDT gives the image's slide away (AoU-11)"
        }
    } else {
        "off, as built"
    };
    crate::console::println!("  cpu      descriptor table addresses kept from ring 3: UMIP {umip}");
    // Each secondary programs its own in `smp::secondary_start`.
    let pat = if cpu::program_pat() {
        let _ = PAT_PROGRAMMED.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
        "entry 1 write-combining"
    } else {
        "not programmed, so write-combining is refused and stage 9 stops the boot"
    };
    crate::console::println!("  cpu      page attribute table: {pat}");
    // Before `CpuStarter::new` snapshots `CR4`, so secondaries take
    // `OSXSAVE` with it; each loads `XCR0` in `smp::secondary_start`.
    // Whether AVX would let one program read another's vector registers
    // (Zenbleed, GDS), decided and covered before `XCR0` is written.
    let leak = speculation::vector_leak();
    let components = cpu::enable_extended_state(leak.allows_avx());
    let state = if components & cpu::XSTATE_AVX != 0 {
        "x87, SSE and AVX, saved with XSAVE"
    } else if components != 0 {
        "x87 and SSE, saved with XSAVE; no AVX"
    } else {
        "x87 and SSE, saved with FXSAVE; no XSAVE, so no AVX"
    };
    crate::console::println!(
        "  cpu      program register state: {state}; vector registers: {}",
        leak.describe()
    );
}

/// Decide which side-channel defences this machine gets, apply them on the
/// boot processor, and say so. Every secondary applies the same as it starts.
///
/// Before the second processor starts and before the first program.
pub(crate) fn init_speculation(_view: &BootView<'_>) {
    speculation::init();
}

/// Permit this processor to touch user pages until [`forbid_user_access`].
///
/// SMAP's `EFLAGS.AC` window. `cpu::permit_user_access` explains why almost
/// nothing needs it.
pub(crate) fn permit_user_access() {
    cpu::permit_user_access();
}

/// Refuse user pages to this processor again.
pub(crate) fn forbid_user_access() {
    cpu::forbid_user_access();
}

/// Invalidate the whole TLB — this processor's, global entries included.
pub(crate) fn flush_tlb() {
    cpu::flush_tlb_including_global();
}

/// Whether [`flush_tlb`] reaches every processor's TLB.
///
/// No: reloading `CR3` and `invlpg` are both local. Another processor's
/// stale translations are dropped by that processor, told to by an
/// interrupt, which is what a TLB shootdown is.
pub(crate) const TLB_FLUSH_IS_BROADCAST: bool = false;

/// Invalidate the translation of the page holding `address` — this
/// processor's only, as [`flush_tlb`] is.
pub(crate) fn flush_tlb_page(address: u64) {
    cpu::invalidate_page(address);
}

/// Interrupt the processor whose hardware identifier is `hardware_id`, and no
/// other: its local APIC ID.
pub(crate) fn send_ipi_to(hardware_id: u64) -> Result<(), &'static str> {
    let apic_id = u32::try_from(hardware_id)
        .map_err(|_| "an APIC ID above 255 needs x2APIC mode, which is not written yet")?;
    apic::send_ipi_to(apic_id)
}

/// The reverse map check's program: touches one shared page, or holds still,
/// as the kernel tells it through the next page.
///
/// Its role is its argument count less one, 0 for the parent and 1 for the
/// child, and its slot in the control page at `0x7000_1000` is sixteen bytes
/// per role: the command, the answer, and two words it saw. Commands are 0
/// (read the page at `0x7000_0000` over and over, exiting 7 if it holds the
/// poison `0xDEADBEEF`), 1 (hold still, answer 1), 2 (read the page, write
/// `0x5EED0000` plus the role, read it back, report both, answer 2), 3 (read
/// and report, answer 3) and 4 (exit 0); anything else exits 9. A command is
/// cleared before it is answered, so an answer the kernel sees is to the
/// command it gave.
///
/// ```text
///   mov r9d, [rsp] ; dec r9d                     ; role
///   mov r10d, 0x70000000 ; r11 = r10 + 0x1000 + role * 16
///   r12d = 0x5EED0000 + role ; r13d = 0xDEADBEEF
/// top: switch [r11]
///   0: cmp [r10], r13d ; jne top ; exit 7
///   1: [r11+4] = 1 ; jmp top
///   2: ecx = [r10] ; [r10] = r12d ; edx = [r10] ; [r11+8] = ecx ; [r11+12] = edx
///      [r11] = 0 ; [r11+4] = 2 ; jmp top
///   3: ecx = [r10] ; [r11+8] = ecx ; [r11] = 0 ; [r11+4] = 3 ; jmp top
///   4: exit 0
/// ```
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_RMAP_PROGRAM: &[u8] = &[
    0x44, 0x8b, 0x0c, 0x24, 0x41, 0xff, 0xc9, 0x41, 0xba, 0x00, 0x00, 0x00, 0x70, 0x44, 0x89, 0xc8,
    0xc1, 0xe0, 0x04, 0x4d, 0x8d, 0x9c, 0x02, 0x00, 0x10, 0x00, 0x00, 0x45, 0x8d, 0xa1, 0x00, 0x00,
    0xed, 0x5e, 0x41, 0xbd, 0xef, 0xbe, 0xad, 0xde, 0x41, 0x8b, 0x03, 0x85, 0xc0, 0x74, 0x1b, 0x83,
    0xf8, 0x01, 0x74, 0x25, 0x83, 0xf8, 0x02, 0x74, 0x2a, 0x83, 0xf8, 0x03, 0x74, 0x47, 0x83, 0xf8,
    0x04, 0x74, 0x5a, 0xbf, 0x09, 0x00, 0x00, 0x00, 0xeb, 0x55, 0x41, 0x8b, 0x0a, 0x44, 0x39, 0xe9,
    0x75, 0xd6, 0xbf, 0x07, 0x00, 0x00, 0x00, 0xeb, 0x46, 0x41, 0xc7, 0x43, 0x04, 0x01, 0x00, 0x00,
    0x00, 0xeb, 0xc5, 0x41, 0x8b, 0x0a, 0x45, 0x89, 0x22, 0x41, 0x8b, 0x12, 0x41, 0x89, 0x4b, 0x08,
    0x41, 0x89, 0x53, 0x0c, 0x41, 0xc7, 0x03, 0x00, 0x00, 0x00, 0x00, 0x41, 0xc7, 0x43, 0x04, 0x02,
    0x00, 0x00, 0x00, 0xeb, 0xa3, 0x41, 0x8b, 0x0a, 0x41, 0x89, 0x4b, 0x08, 0x41, 0xc7, 0x03, 0x00,
    0x00, 0x00, 0x00, 0x41, 0xc7, 0x43, 0x04, 0x03, 0x00, 0x00, 0x00, 0xeb, 0x8b, 0x31, 0xff, 0xb8,
    0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b,
];

/// Root of the loader's identity map, while it still exists.
///
/// Always `None` on x86-64: there is only one root table, and the identity map
/// is the lower half of it. A sweep that walks the kernel's tables has
/// therefore already seen it — which is what makes the W^X check on this
/// architecture find the identity map without being told where it is.
pub(crate) const fn identity_root(_view: &BootView<'_>) -> Option<u64> {
    None
}

/// True while anything the loader identity mapped still translates.
///
/// One tree translates both halves here, so a walk of it from software is the
/// hardware's answer. Two addresses: zero, because a null dereference in
/// kernel code must fault rather than find the first page of physical memory,
/// and the kernel's own physical address, which the identity map covered with
/// the rest of RAM.
pub(crate) fn identity_map_live(view: &BootView<'_>) -> bool {
    crate::mm::translate(0).is_some() || crate::mm::translate(view.raw().kernel_phys).is_some()
}

/// `CR0.WP`: ring 0 obeys a read-only page table entry only while it is set.
const CR0_WP: u64 = 1 << 16;

/// True if the kernel faults when it writes through a read-only mapping.
///
/// On x86-64 that is `CR0.WP`, which the loader sets and firmware is free to
/// have left clear. Without it every read-only kernel mapping is writable from
/// ring 0, and the W^X sweep, which reads entries, cannot tell.
pub(crate) fn kernel_write_protected() -> bool {
    cpu::read_cr0() & CR0_WP != 0
}

/// The first root-table slot belonging to the upper half.
///
/// A 48-bit address space has 512 top-level slots, and the upper half starts
/// at slot 256. Everything below it is the identity map the loader built and
/// nothing else: the direct map begins at slot 256, the `vmap` area at 510 and
/// the kernel image at 511.
const UPPER_HALF_SLOT: usize = 256;

/// Top-level slots in a four-level root table.
const ROOT_SLOTS: usize = 512;

/// Fold an x86-64 system call number onto the call it means.
///
/// x86-64 kept the table it grew rather than adopting the generic one every
/// architecture added after 2011 uses, so `read` is 0 here and 63 on AArch64.
/// This is the only place in the kernel that knows which of the three tables
/// applies; `crate::syscall` dispatches on the answer.
///
/// The number is a program's, and the table is a `match` the compiler makes a
/// jump table of, so it is bounded and clamped first: a processor that
/// mispredicts the table's own bounds check then jumps through slot zero
/// rather than through whatever lies past the table (Spectre variant 1).
pub(crate) fn decode_syscall(number: usize) -> Option<Syscall> {
    nr::from_x86_64(super::nospec_index(number, nr::X86_64_END)?)
}

/// The `AUDIT_ARCH_*` token a system call made through `abi` carries in
/// `seccomp_data.arch`: x86-64's for `SYSCALL`, i386's for `int $0x80`.
/// Decided by the entry, as the table is, and never by the image the program
/// runs (`docs/SECCOMP.md` §3.2, SR1).
pub(crate) const fn audit_arch(abi: crate::trap::Abi) -> u32 {
    match abi {
        // `EM_X86_64` | `__AUDIT_ARCH_64BIT` | `__AUDIT_ARCH_LE`.
        crate::trap::Abi::Native => 0xC000_003E,
        // `EM_386` | `__AUDIT_ARCH_LE`.
        crate::trap::Abi::Compat => 0x4000_0003,
    }
}

/// The value to put in the return register of a call that is not run, so that
/// the frame reads as it did when the program made the call: Linux's
/// `syscall_rollback`. `RAX` held the number, and the handler of a trapped
/// call finds it there (`docs/SECCOMP.md` §3.6).
pub(crate) const fn syscall_rollback_value(
    abi: crate::trap::Abi,
    number: usize,
    args: &[u64; 6],
) -> isize {
    let _ = (abi, args);
    number as isize
}

/// ELF's `e_machine` for i386.
const EM_386: u16 = 3;

/// The ABI a program image runs in, or `None` for one this machine cannot
/// run: an x86-64 image natively, and a 32-bit i386 one in compatibility mode
/// (`docs/I386.md` §3.4). The class is what says 32-bit: an `EM_386` image
/// claiming 64-bit words is nothing a processor runs.
pub(crate) fn image_abi(class: ferrix_elf::Class, machine: u16) -> Option<crate::trap::Abi> {
    match (class, machine) {
        (_, machine) if machine == ARCH.elf_machine() => Some(crate::trap::Abi::Native),
        (ferrix_elf::Class::Elf32, EM_386) => Some(crate::trap::Abi::Compat),
        _ => None,
    }
}

/// One past the highest address a 32-bit program's space may hold: Linux's
/// `IA32_PAGE_OFFSET`, the top page below 4 GiB left out, as the native
/// space leaves out its own top page.
pub(crate) const COMPAT_USER_END: u64 = 0xFFFF_E000;

/// Fold an i386 system call number onto the call it means: a call through
/// `int $0x80`, from a 32-bit program or a 64-bit one, as on Linux
/// (`docs/I386.md` §3.2). Clamped against its own table's end for the reason
/// [`decode_syscall`] is.
pub(crate) fn decode_compat_syscall(number: usize) -> Option<Syscall> {
    nr::from_i386(super::nospec_index(number, nr::I386_END)?)
}

/// The `open` flag bits that differ between architectures, as this one
/// numbers them: the generic header's, which x86-64 does not override.
pub(crate) const OPEN_FLAGS: OpenFlagBits = types::OPEN_FLAGS_GENERIC;

/// A whole program, in machine code: write a line to file descriptor 1 and
/// exit with a known status.
///
/// Forty-six bytes and a string, because the first thing to cross into ring 3
/// should be something that can be read in full. A compiled test program would
/// need a second crate, a second target and a build step, and when the
/// transition did not work the first question would be whether the program was
/// at fault. Nothing here can be: there is no libc, no relocation, no stack
/// use, and every instruction is listed.
///
/// ```text
///   mov  $1, %rax          ; __NR_write
///   mov  $1, %rdi          ; fd 1
///   lea  0x19(%rip), %rsi  ; the message, just past this code
///   mov  $18, %rdx         ; its length
///   syscall
///   mov  $231, %rax        ; __NR_exit_group
///   mov  $42, %rdi         ; a status nothing else would produce
///   syscall
/// ```
pub(crate) const USER_TEST_PROGRAM: &[u8] = &[
    0x48, 0xc7, 0xc0, 0x01, 0x00, 0x00, 0x00, // mov $1, %rax
    0x48, 0xc7, 0xc7, 0x01, 0x00, 0x00, 0x00, // mov $1, %rdi
    0x48, 0x8d, 0x35, 0x19, 0x00, 0x00, 0x00, // lea 0x19(%rip), %rsi
    0x48, 0xc7, 0xc2, 0x12, 0x00, 0x00, 0x00, // mov $18, %rdx
    0x0f, 0x05, // syscall
    0x48, 0xc7, 0xc0, 0xe7, 0x00, 0x00, 0x00, // mov $231, %rax
    0x48, 0xc7, 0xc7, 0x2a, 0x00, 0x00, 0x00, // mov $42, %rdi
    0x0f, 0x05, // syscall
    b'h', b'e', b'l', b'l', b'o', b' ', b'f', b'r', b'o', b'm', b' ', b'r', b'i', b'n', b'g', b' ',
    b'3', b'\n',
];

/// The status [`USER_TEST_PROGRAM`] exits with.
pub(crate) const USER_TEST_STATUS: i32 = 42;

/// A program that exits with whatever its first argument register held when it
/// entered user mode: the start argument a native process's bootstrap handle
/// arrives in.
///
/// ```text
///   movl $231, %eax   ; exit_group
///   syscall           ; with RDI, the start argument, as the status
/// ```
///
/// Two instructions, checked against the host's disassembler.
pub(crate) const USER_ARGUMENT_PROGRAM: &[u8] = &[0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05];

/// A program that writes through a null pointer, and exits with 98 if that did
/// not fault.
///
/// For the check that a program ended by its own fault is ended, heard by
/// whoever watches it, and freed: a kill forced from a trap, which has to run
/// with interrupts open.
///
/// ```text
///   xorl %eax, %eax
///   movq %rax, (%rax)                          ; SIGSEGV
///   movl $231, %eax ; movl $98, %edi ; syscall ; exit_group(98)
/// ```
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_FAULT_PROGRAM: &[u8] = &[
    0x31, 0xc0, // xorl %eax, %eax
    0x48, 0x89, 0x00, // movq %rax, (%rax)
    0xb8, 0xe7, 0x00, 0x00, 0x00, // movl $231, %eax
    0xbf, 0x62, 0x00, 0x00, 0x00, // movl $98, %edi
    0x0f, 0x05, // syscall
];

/// A program that forks, has its child exit with 23, waits for it, and exits
/// with the child's exit code plus one: 24 when `fork`, the child's copy of
/// its parent's registers and `wait4`'s status word are all right, 99 when
/// `wait4` reports the wrong child.
///
/// ```text
///   movl $57, %eax ; syscall                  ; fork
///   testq %rax, %rax ; jnz 1f
///   movl $231, %eax ; movl $23, %edi ; syscall ; the child exits 23
/// 1: subq $16, %rsp ; movq %rax, %rdi ; movq %rsp, %rsi
///   xorl %edx, %edx ; xorl %r10d, %r10d ; movl $61, %eax ; syscall ; wait4
///   cmpq %rdi, %rax ; jne 2f
///   movl (%rsp), %edi ; shrl $8, %edi ; addl $1, %edi
///   movl $231, %eax ; syscall                 ; exit with the child's code + 1
/// 2: movl $231, %eax ; movl $99, %edi ; syscall
/// ```
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_FORK_PROGRAM: &[u8] = &[
    0xb8, 0x39, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75, 0x0c, 0xb8, 0xe7, 0x00, 0x00,
    0x00, 0xbf, 0x17, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x83, 0xec, 0x10, 0x48, 0x89, 0xc7, 0x48,
    0x89, 0xe6, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0xb8, 0x3d, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x39,
    0xf8, 0x75, 0x10, 0x8b, 0x3c, 0x24, 0xc1, 0xef, 0x08, 0x83, 0xc7, 0x01, 0xb8, 0xe7, 0x00, 0x00,
    0x00, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x63, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f,
    0x0b,
];

/// A program that asks `clone` for namespaces and must be refused: exits with
/// 44 when both calls answer `EINVAL`, 98 when the first was allowed and 97
/// when the second was.
///
/// The two calls between them name every `CLONE_NEW*` flag `clone` can reach:
/// the first `CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWNET | CLONE_FS`,
/// refused because a user or a mount namespace cannot be asked with a shared fs
/// context (U5); the second `CLONE_NEWCGROUP | CLONE_NEWUTS | CLONE_NEWIPC |
/// CLONE_NEWNET | CLONE_SYSVSEM`, refused because a new IPC namespace excludes shared
/// semaphore undo. Every namespace exists now (`fs/smallns_check.rs`,
/// `fs/netns_check.rs`, the pid and time lines); what the program shows is that
/// `clone` judges the flags before it makes anything.
/// Each carries `SIGCHLD`, so nothing but the namespaces can be what is
/// refused. A kernel that ignored the flags would answer the first with a
/// child's pid, and both the parent and that child would exit 98.
///
/// ```text
///   movl $56, %eax ; movl $0x70020211, %edi ; zeroed rsi, rdx, r10, r8 ; syscall
///   movl $56, %eax ; movl $0x4e040011, %edi ; zeroed rsi, rdx, r10, r8 ; syscall
///   cmp/cmn against -22 ; exit_group(44), or 98 and 97 for a call allowed
/// ```
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_NAMESPACE_PROGRAM: &[u8] = &[
    0xb8, 0x38, 0x00, 0x00, 0x00, 0xbf, 0x11, 0x02, 0x02, 0x70, 0x31, 0xf6, 0x31, 0xd2, 0x45, 0x31,
    0xd2, 0x45, 0x31, 0xc0, 0x0f, 0x05, 0x48, 0x83, 0xf8, 0xea, 0x75, 0x23, 0xb8, 0x38, 0x00, 0x00,
    0x00, 0xbf, 0x11, 0x00, 0x04, 0x4e, 0x31, 0xf6, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0x45, 0x31, 0xc0,
    0x0f, 0x05, 0x48, 0x83, 0xf8, 0xea, 0x75, 0x0e, 0xbf, 0x2c, 0x00, 0x00, 0x00, 0xeb, 0x0c, 0xbf,
    0x62, 0x00, 0x00, 0x00, 0xeb, 0x05, 0xbf, 0x61, 0x00, 0x00, 0x00, 0xb8, 0xe7, 0x00, 0x00, 0x00,
    0x0f, 0x05, 0x0f, 0x0b,
];

/// A program that starts a child in a cgroup with `clone3`'s
/// `CLONE_INTO_CGROUP`, and has the child read its own `/proc/self/cgroup` as
/// the first thing it does: exits with 44 when the child found itself in
/// `/check-g` and both refusals were `EBADF`.
///
/// It opens `/tmp/cgroup-check/check-g`, a cgroupfs directory stage 13's check
/// made, and passes it in `struct clone_args` (`CLONE_ARGS_SIZE_VER2`, 88
/// bytes; `CLONE_INTO_CGROUP` is `0x200000000`, `SIGCHLD` 17). The child
/// exits 0 only if what it read is `0::/check-g\n`, byte for byte, and 1
/// otherwise; the parent exits 2 if the child did not exit 0. Then the same
/// call with descriptor 1000, which is not open, must answer `EBADF`, else 3;
/// and with a descriptor of `/tmp`, which is not a cgroup, `EBADF` again, as
/// Linux's `cgroup_get_from_file` answers, else 4. 5 is a failed open, 6 a
/// refused `clone3`.
///
/// ```text
///   openat(AT_FDCWD, "/tmp/cgroup-check/check-g", O_RDONLY) -> %r12
///   zero 96 bytes at %rsp ; flags = 0x200000000 ; exit_signal = 17 ; cgroup = %r12
///   clone3(%rsp, 88) ; child: openat("/proc/self/cgroup"), read 64 into 128(%rsp),
///     require 12 bytes equal to "0::/check-g\n" (repe cmpsb), exit_group(0 or 1)
///   parent: wait4(pid, 96(%rsp), 0, 0) ; status 0 or exit_group(2)
///   cgroup = 1000 ; clone3 must be -9, else 3 ; cgroup = openat("/tmp") ; -9, else 4
///   exit_group(44)
/// ```
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_INTO_CGROUP_PROGRAM: &[u8] = &[
    0xb8, 0x01, 0x01, 0x00, 0x00, 0xbf, 0x9c, 0xff, 0xff, 0xff, 0x48, 0x8d, 0x35, 0x55, 0x01, 0x00,
    0x00, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x88, 0x30, 0x01, 0x00,
    0x00, 0x49, 0x89, 0xc4, 0x48, 0x81, 0xec, 0x00, 0x01, 0x00, 0x00, 0x31, 0xc0, 0x48, 0x89, 0xe7,
    0xb9, 0x0c, 0x00, 0x00, 0x00, 0xf3, 0x48, 0xab, 0x48, 0xb8, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00,
    0x00, 0x00, 0x48, 0x89, 0x04, 0x24, 0x48, 0xc7, 0x44, 0x24, 0x20, 0x11, 0x00, 0x00, 0x00, 0x4c,
    0x89, 0x64, 0x24, 0x50, 0xb8, 0xb3, 0x01, 0x00, 0x00, 0x48, 0x89, 0xe7, 0xbe, 0x58, 0x00, 0x00,
    0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x88, 0xec, 0x00, 0x00, 0x00, 0x74, 0x7a, 0x48, 0x89,
    0xc7, 0x48, 0x8d, 0x74, 0x24, 0x60, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0xb8, 0x3d, 0x00, 0x00, 0x00,
    0x0f, 0x05, 0x83, 0x7c, 0x24, 0x60, 0x00, 0x0f, 0x85, 0xaf, 0x00, 0x00, 0x00, 0x48, 0xc7, 0x44,
    0x24, 0x50, 0xe8, 0x03, 0x00, 0x00, 0xb8, 0xb3, 0x01, 0x00, 0x00, 0x48, 0x89, 0xe7, 0xbe, 0x58,
    0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x83, 0xf8, 0xf7, 0x0f, 0x85, 0x94, 0x00, 0x00, 0x00, 0xb8,
    0x01, 0x01, 0x00, 0x00, 0xbf, 0x9c, 0xff, 0xff, 0xff, 0x48, 0x8d, 0x35, 0xc0, 0x00, 0x00, 0x00,
    0x31, 0xd2, 0x45, 0x31, 0xd2, 0x0f, 0x05, 0x48, 0x89, 0x44, 0x24, 0x50, 0xb8, 0xb3, 0x01, 0x00,
    0x00, 0x48, 0x89, 0xe7, 0xbe, 0x58, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x83, 0xf8, 0xf7, 0x75,
    0x69, 0xbf, 0x2c, 0x00, 0x00, 0x00, 0xeb, 0x75, 0xb8, 0x01, 0x01, 0x00, 0x00, 0xbf, 0x9c, 0xff,
    0xff, 0xff, 0x48, 0x8d, 0x35, 0x8c, 0x00, 0x00, 0x00, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0x0f, 0x05,
    0x89, 0xc7, 0x48, 0x8d, 0xb4, 0x24, 0x80, 0x00, 0x00, 0x00, 0xba, 0x40, 0x00, 0x00, 0x00, 0x31,
    0xc0, 0x0f, 0x05, 0x48, 0x83, 0xf8, 0x0c, 0x75, 0x1c, 0x48, 0x8d, 0x35, 0x77, 0x00, 0x00, 0x00,
    0x48, 0x8d, 0xbc, 0x24, 0x80, 0x00, 0x00, 0x00, 0xb9, 0x0c, 0x00, 0x00, 0x00, 0xf3, 0xa6, 0x75,
    0x04, 0x31, 0xff, 0xeb, 0x28, 0xbf, 0x01, 0x00, 0x00, 0x00, 0xeb, 0x21, 0xbf, 0x02, 0x00, 0x00,
    0x00, 0xeb, 0x1a, 0xbf, 0x03, 0x00, 0x00, 0x00, 0xeb, 0x13, 0xbf, 0x04, 0x00, 0x00, 0x00, 0xeb,
    0x0c, 0xbf, 0x05, 0x00, 0x00, 0x00, 0xeb, 0x05, 0xbf, 0x06, 0x00, 0x00, 0x00, 0xb8, 0xe7, 0x00,
    0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b, 0x2f, 0x74, 0x6d, 0x70, 0x2f, 0x63, 0x67, 0x72, 0x6f, 0x75,
    0x70, 0x2d, 0x63, 0x68, 0x65, 0x63, 0x6b, 0x2f, 0x63, 0x68, 0x65, 0x63, 0x6b, 0x2d, 0x67, 0x00,
    0x2f, 0x74, 0x6d, 0x70, 0x00, 0x2f, 0x70, 0x72, 0x6f, 0x63, 0x2f, 0x73, 0x65, 0x6c, 0x66, 0x2f,
    0x63, 0x67, 0x72, 0x6f, 0x75, 0x70, 0x00, 0x30, 0x3a, 0x3a, 0x2f, 0x63, 0x68, 0x65, 0x63, 0x6b,
    0x2d, 0x67, 0x0a,
];

/// A program that narrows its own mapping and writes through it: ended by
/// `SIGSEGV` when right, and exiting with 1 when the write went through.
///
/// The first write faults the page in and is retried, which leaves the
/// processor holding a writable translation for it. `mprotect` then makes the
/// page read-only, and the second write must fault. It can only fault if the
/// kernel invalidated that cached translation: the tables no longer allow the
/// write, but a processor that still holds the old entry never looks at them.
/// 97 is a refused `mmap`, 98 a refused `mprotect`.
///
/// ```text
///   mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) -> %rbx
///   movq $1, (%rbx)                        ; faulted in writable, then cached
///   mprotect(%rbx, 4096, PROT_READ)        ; must return 0
///   movq $2, (%rbx)                        ; must fault
///   exit_group(1)
/// ```
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_MPROTECT_PROGRAM: &[u8] = &[
    0x31, 0xff, 0xbe, 0x00, 0x10, 0x00, 0x00, 0xba, 0x03, 0x00, 0x00, 0x00, 0x41, 0xba, 0x22, 0x00,
    0x00, 0x00, 0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, 0x45, 0x31, 0xc9, 0xb8, 0x09, 0x00, 0x00,
    0x00, 0x0f, 0x05, 0x48, 0x3d, 0x00, 0xf0, 0xff, 0xff, 0x77, 0x31, 0x48, 0x89, 0xc3, 0x48, 0xc7,
    0x03, 0x01, 0x00, 0x00, 0x00, 0x48, 0x89, 0xdf, 0xbe, 0x00, 0x10, 0x00, 0x00, 0xba, 0x01, 0x00,
    0x00, 0x00, 0xb8, 0x0a, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75, 0x15, 0x48, 0xc7,
    0x03, 0x02, 0x00, 0x00, 0x00, 0xbf, 0x01, 0x00, 0x00, 0x00, 0xeb, 0x0c, 0xbf, 0x61, 0x00, 0x00,
    0x00, 0xeb, 0x05, 0xbf, 0x62, 0x00, 0x00, 0x00, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f,
    0x0b,
];

/// A program that forks with two private pages holding `A` and has each side
/// write one of them while the other still shares it: 61 when neither write
/// reaches the other side.
///
/// A pipe orders the two halves. The parent reads the second page, writes `C`
/// into it and only then writes a byte to the pipe; the child, blocked on the
/// pipe until then, must still read `A` there. The child then reads the first
/// page and writes `B` into it, which the parent -- after `wait4` -- must still
/// read as `A`. Each write lands on a page that was mapped read-only a moment
/// before, so each is the copy-on-write fault replacing a live translation.
///
/// The child's status is its own diagnosis: 1 a failed `read`, 2 the parent's
/// write showed through, 3 its own write did not stick. The parent exits with
/// 10 plus that, or with 4 when its page was not `A` after the fork, 5 when the
/// child's write showed through, 6 when its own write did not stick, 97 for a
/// refused `mmap`, 98 for any other refused call.
///
/// ```text
///   mmap(NULL, 8192, RW, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) -> %rbx
///   movq $'A', (%rbx) ; movq $'A', 4096(%rbx)          ; committed before the fork
///   pipe2(fds, 0) ; fork
/// parent:
///   4096(%rbx) == 'A' ; movq $'C', 4096(%rbx) ; == 'C'
///   write(fds[1], buf, 1) ; wait4(child, &status, 0, NULL)
///   status == 0 ; (%rbx) == 'A' ; 4096(%rbx) == 'C' ; exit_group(61)
/// child:
///   read(fds[0], buf, 1)
///   4096(%rbx) == 'A' ; (%rbx) == 'A' ; movq $'B', (%rbx) ; == 'B' ; exit_group(0)
/// ```
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_COW_PROGRAM: &[u8] = &[
    0x48, 0x83, 0xec, 0x20, 0x31, 0xff, 0xbe, 0x00, 0x20, 0x00, 0x00, 0xba, 0x03, 0x00, 0x00, 0x00,
    0x41, 0xba, 0x22, 0x00, 0x00, 0x00, 0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, 0x45, 0x31, 0xc9,
    0xb8, 0x09, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x3d, 0x00, 0xf0, 0xff, 0xff, 0x0f, 0x87, 0x2f,
    0x01, 0x00, 0x00, 0x48, 0x89, 0xc3, 0x48, 0xc7, 0x03, 0x41, 0x00, 0x00, 0x00, 0x48, 0xc7, 0x83,
    0x00, 0x10, 0x00, 0x00, 0x41, 0x00, 0x00, 0x00, 0x48, 0x89, 0xe7, 0x31, 0xf6, 0xb8, 0x25, 0x01,
    0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x85, 0x0c, 0x01, 0x00, 0x00, 0xb8, 0x39, 0x00,
    0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x88, 0xfc, 0x00, 0x00, 0x00, 0x0f, 0x84, 0x8d,
    0x00, 0x00, 0x00, 0x49, 0x89, 0xc4, 0x48, 0x83, 0xbb, 0x00, 0x10, 0x00, 0x00, 0x41, 0x0f, 0x85,
    0xc9, 0x00, 0x00, 0x00, 0x48, 0xc7, 0x83, 0x00, 0x10, 0x00, 0x00, 0x43, 0x00, 0x00, 0x00, 0x48,
    0x83, 0xbb, 0x00, 0x10, 0x00, 0x00, 0x43, 0x0f, 0x85, 0xbe, 0x00, 0x00, 0x00, 0x8b, 0x7c, 0x24,
    0x04, 0x48, 0x8d, 0x74, 0x24, 0x08, 0xba, 0x01, 0x00, 0x00, 0x00, 0xb8, 0x01, 0x00, 0x00, 0x00,
    0x0f, 0x05, 0x48, 0x83, 0xf8, 0x01, 0x0f, 0x85, 0xad, 0x00, 0x00, 0x00, 0x4c, 0x89, 0xe7, 0x48,
    0x8d, 0x74, 0x24, 0x08, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0xb8, 0x3d, 0x00, 0x00, 0x00, 0x0f, 0x05,
    0x4c, 0x39, 0xe0, 0x0f, 0x85, 0x90, 0x00, 0x00, 0x00, 0x8b, 0x7c, 0x24, 0x08, 0x85, 0xff, 0x75,
    0x17, 0x48, 0x83, 0x3b, 0x41, 0x75, 0x6d, 0x48, 0x83, 0xbb, 0x00, 0x10, 0x00, 0x00, 0x43, 0x75,
    0x6a, 0xbf, 0x3d, 0x00, 0x00, 0x00, 0xeb, 0x76, 0xc1, 0xef, 0x08, 0x83, 0xc7, 0x0a, 0xeb, 0x6e,
    0x8b, 0x3c, 0x24, 0x48, 0x8d, 0x74, 0x24, 0x08, 0xba, 0x01, 0x00, 0x00, 0x00, 0x31, 0xc0, 0x0f,
    0x05, 0x48, 0x83, 0xf8, 0x01, 0x75, 0x21, 0x48, 0x83, 0xbb, 0x00, 0x10, 0x00, 0x00, 0x41, 0x75,
    0x1e, 0x48, 0x83, 0x3b, 0x41, 0x75, 0x18, 0x48, 0xc7, 0x03, 0x42, 0x00, 0x00, 0x00, 0x48, 0x83,
    0x3b, 0x42, 0x75, 0x12, 0x31, 0xff, 0xeb, 0x36, 0xbf, 0x01, 0x00, 0x00, 0x00, 0xeb, 0x2f, 0xbf,
    0x02, 0x00, 0x00, 0x00, 0xeb, 0x28, 0xbf, 0x03, 0x00, 0x00, 0x00, 0xeb, 0x21, 0xbf, 0x04, 0x00,
    0x00, 0x00, 0xeb, 0x1a, 0xbf, 0x05, 0x00, 0x00, 0x00, 0xeb, 0x13, 0xbf, 0x06, 0x00, 0x00, 0x00,
    0xeb, 0x0c, 0xbf, 0x61, 0x00, 0x00, 0x00, 0xeb, 0x05, 0xbf, 0x62, 0x00, 0x00, 0x00, 0xb8, 0xe7,
    0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b,
];

/// A program that forks with a two-page `MAP_SHARED` anonymous mapping and a
/// one-page `MAP_PRIVATE` one: 62 when the child's writes to the shared pages
/// reach the parent and its write to the private page does not.
///
/// The first shared page and the private page hold `A` before the fork; the
/// second shared page is first touched by the child, so the page it commits
/// has to land in the object both sides name. The child writes `B` to all
/// three and exits 0; the parent waits and reads them back.
///
/// 2 is the committed shared page not seen, 3 the page the child committed not
/// seen, 4 the private write seen. A child that did not exit 0 is 10 plus its
/// code; 97 is a refused `mmap` and 98 any other refused call.
///
/// ```text
///   mmap(NULL, 8192, RW, MAP_SHARED | MAP_ANONYMOUS, -1, 0) -> %rbx
///   mmap(NULL, 4096, RW, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) -> %r13
///   movq $'A', (%rbx) ; movq $'A', (%r13) ; fork
/// parent:
///   wait4(child, &status, 0, NULL) ; status == 0
///   (%rbx) == 'B' ; 4096(%rbx) == 'B' ; (%r13) == 'A' ; exit_group(62)
/// child:
///   movq $'B', (%rbx) ; movq $'B', 4096(%rbx) ; movq $'B', (%r13) ; exit_group(0)
/// ```
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_SHARED_PROGRAM: &[u8] = &[
    0x48, 0x83, 0xec, 0x10, 0x31, 0xff, 0xbe, 0x00, 0x20, 0x00, 0x00, 0xba, 0x03, 0x00, 0x00, 0x00,
    0x41, 0xba, 0x21, 0x00, 0x00, 0x00, 0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, 0x45, 0x31, 0xc9,
    0xb8, 0x09, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x3d, 0x00, 0xf0, 0xff, 0xff, 0x0f, 0x87, 0xd0,
    0x00, 0x00, 0x00, 0x48, 0x89, 0xc3, 0x31, 0xff, 0xbe, 0x00, 0x10, 0x00, 0x00, 0xba, 0x03, 0x00,
    0x00, 0x00, 0x41, 0xba, 0x22, 0x00, 0x00, 0x00, 0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, 0x45,
    0x31, 0xc9, 0xb8, 0x09, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x3d, 0x00, 0xf0, 0xff, 0xff, 0x0f,
    0x87, 0x9e, 0x00, 0x00, 0x00, 0x49, 0x89, 0xc5, 0x48, 0xc7, 0x03, 0x41, 0x00, 0x00, 0x00, 0x49,
    0xc7, 0x45, 0x00, 0x41, 0x00, 0x00, 0x00, 0xb8, 0x39, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85,
    0xc0, 0x0f, 0x88, 0x83, 0x00, 0x00, 0x00, 0x74, 0x47, 0x49, 0x89, 0xc4, 0x48, 0x89, 0xc7, 0x48,
    0x89, 0xe6, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0xb8, 0x3d, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x4c, 0x39,
    0xe0, 0x75, 0x67, 0x8b, 0x3c, 0x24, 0x85, 0xff, 0x75, 0x1e, 0x48, 0x83, 0x3b, 0x42, 0x75, 0x3e,
    0x48, 0x83, 0xbb, 0x00, 0x10, 0x00, 0x00, 0x42, 0x75, 0x3b, 0x49, 0x83, 0x7d, 0x00, 0x41, 0x75,
    0x3b, 0xbf, 0x3e, 0x00, 0x00, 0x00, 0xeb, 0x47, 0xc1, 0xef, 0x08, 0x83, 0xc7, 0x0a, 0xeb, 0x3f,
    0x48, 0xc7, 0x03, 0x42, 0x00, 0x00, 0x00, 0x48, 0xc7, 0x83, 0x00, 0x10, 0x00, 0x00, 0x42, 0x00,
    0x00, 0x00, 0x49, 0xc7, 0x45, 0x00, 0x42, 0x00, 0x00, 0x00, 0x31, 0xff, 0xeb, 0x21, 0xbf, 0x02,
    0x00, 0x00, 0x00, 0xeb, 0x1a, 0xbf, 0x03, 0x00, 0x00, 0x00, 0xeb, 0x13, 0xbf, 0x04, 0x00, 0x00,
    0x00, 0xeb, 0x0c, 0xbf, 0x61, 0x00, 0x00, 0x00, 0xeb, 0x05, 0xbf, 0x62, 0x00, 0x00, 0x00, 0xb8,
    0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b,
];

/// A program that makes threads, and exits with 42 when `clone` with
/// `CLONE_VM` alone is `ENOSYS`; a thread made with `CLONE_THREAD` ran in its
/// process's memory, ended alone through `exit`, had its tid written to
/// `parent_tid` and cleared from `child_tid`, and that tid is not the pid; and
/// a second thread made without `CLONE_CHILD_CLEARTID` left its word alone, so
/// a 50 ms wait on it times out. 5 or 6 if a thread's `exit` ended the process;
/// 95, 96, 97, 98, 99 for the `ENOSYS`, a cleared word, the shared word,
/// `parent_tid` and the tid.
///
/// ```text
///   clone(CLONE_VM | SIGCHLD, 0, 0, 0, 0)          ; must be -ENOSYS
///   rbx = mmap(0, 64 KiB, RW, private anonymous)
///   child_tid = -1
///   clone(VM|FS|FILES|SIGHAND|THREAD|PARENT_SETTID|CHILD_CLEARTID,
///         rbx + 64 KiB, &parent_tid, &child_tid, tls = 0x5a5a)
///   thread: word = 42 ; exit(5)
///   while child_tid != 0: futex(&child_tid, FUTEX_WAIT, child_tid, NULL)
///   word == 42, parent_tid == tid, getpid() != tid
///   word2 = -1 ; clone(the same without CHILD_CLEARTID, ..., &word2, tls)
///   thread: exit(6)
///   futex(&word2, FUTEX_WAIT, -1, &{0, 50 ms})     ; must be -ETIMEDOUT
///   exit_group(42)
/// ```
///
/// The unused thread pointer is a distinct value, so a kernel that read
/// `child_tid` from its register would clear the wrong word and the first wait
/// would never end. Assembled by rustc's LLVM and read back out of the object
/// file.
pub(crate) const USER_THREAD_PROGRAM: &[u8] = &[
    0xbf, 0x11, 0x01, 0x00, 0x00, 0x31, 0xf6, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0x45, 0x31, 0xc0, 0xb8,
    0x38, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x83, 0xf8, 0xda, 0x0f, 0x85, 0x15, 0x01, 0x00, 0x00,
    0xb8, 0x09, 0x00, 0x00, 0x00, 0x31, 0xff, 0xbe, 0x00, 0x00, 0x01, 0x00, 0xba, 0x03, 0x00, 0x00,
    0x00, 0x41, 0xba, 0x22, 0x00, 0x00, 0x00, 0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, 0x45, 0x31,
    0xc9, 0x0f, 0x05, 0x48, 0x89, 0xc3, 0xc7, 0x43, 0x0c, 0xff, 0xff, 0xff, 0xff, 0xbf, 0x00, 0x0f,
    0x31, 0x00, 0x48, 0x8d, 0xb3, 0x00, 0x00, 0x01, 0x00, 0x48, 0x8d, 0x53, 0x08, 0x4c, 0x8d, 0x53,
    0x0c, 0x41, 0xb8, 0x5a, 0x5a, 0x00, 0x00, 0xb8, 0x38, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85,
    0xc0, 0x75, 0x12, 0xc7, 0x03, 0x2a, 0x00, 0x00, 0x00, 0xb8, 0x3c, 0x00, 0x00, 0x00, 0xbf, 0x05,
    0x00, 0x00, 0x00, 0x0f, 0x05, 0x49, 0x89, 0xc4, 0x8b, 0x53, 0x0c, 0x85, 0xd2, 0x74, 0x12, 0x48,
    0x8d, 0x7b, 0x0c, 0x31, 0xf6, 0x45, 0x31, 0xd2, 0xb8, 0xca, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xeb,
    0xe7, 0x83, 0x3b, 0x2a, 0x0f, 0x85, 0xa3, 0x00, 0x00, 0x00, 0x44, 0x39, 0x63, 0x08, 0x0f, 0x85,
    0xa5, 0x00, 0x00, 0x00, 0xb8, 0x27, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x4c, 0x39, 0xe0, 0x0f, 0x84,
    0xa1, 0x00, 0x00, 0x00, 0xc7, 0x43, 0x14, 0xff, 0xff, 0xff, 0xff, 0xbf, 0x00, 0x0f, 0x11, 0x00,
    0x48, 0x8d, 0xb3, 0x00, 0x00, 0x01, 0x00, 0x48, 0x8d, 0x53, 0x08, 0x4c, 0x8d, 0x53, 0x14, 0x41,
    0xb8, 0x5a, 0x5a, 0x00, 0x00, 0xb8, 0x38, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75,
    0x0c, 0xb8, 0x3c, 0x00, 0x00, 0x00, 0xbf, 0x06, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0xc7, 0x43,
    0x18, 0x00, 0x00, 0x00, 0x00, 0x48, 0xc7, 0x43, 0x20, 0x80, 0xf0, 0xfa, 0x02, 0x48, 0x8d, 0x7b,
    0x14, 0x31, 0xf6, 0xba, 0xff, 0xff, 0xff, 0xff, 0x4c, 0x8d, 0x53, 0x18, 0xb8, 0xca, 0x00, 0x00,
    0x00, 0x0f, 0x05, 0x48, 0x83, 0xf8, 0x92, 0x75, 0x18, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x2a,
    0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x5f, 0x00, 0x00, 0x00, 0x0f,
    0x05, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x60, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0xe7, 0x00,
    0x00, 0x00, 0xbf, 0x61, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x62,
    0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x63, 0x00, 0x00, 0x00, 0x0f,
    0x05, 0x0f, 0x0b,
];

/// A program whose last two threads call `exit` at the same instant: the main
/// thread makes one thread, which marks itself ready and spins on a go word;
/// the main thread spins until it sees ready, sets go, and both call `exit`
/// with no other call between, each on its own processor, the thread with 9
/// and the main thread with 7. Whichever leaves last, the process must end
/// with 7, the status its first thread left with and not the last thread's;
/// a kernel that decided "last thread" before counting itself gone would leave
/// it running with no thread. It races only with two processors.
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_EXITS_PROGRAM: &[u8] = &[
    0xb8, 0x09, 0x00, 0x00, 0x00, 0x31, 0xff, 0xbe, 0x00, 0x00, 0x01, 0x00, 0xba, 0x03, 0x00, 0x00,
    0x00, 0x41, 0xba, 0x22, 0x00, 0x00, 0x00, 0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, 0x45, 0x31,
    0xc9, 0x0f, 0x05, 0x48, 0x89, 0xc3, 0xc7, 0x03, 0x00, 0x00, 0x00, 0x00, 0xc7, 0x43, 0x04, 0x00,
    0x00, 0x00, 0x00, 0xbf, 0x00, 0x0f, 0x01, 0x00, 0x48, 0x8d, 0xb3, 0x00, 0x00, 0x01, 0x00, 0x31,
    0xd2, 0x45, 0x31, 0xd2, 0x45, 0x31, 0xc0, 0xb8, 0x38, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85,
    0xc0, 0x75, 0x1a, 0xc7, 0x43, 0x04, 0x01, 0x00, 0x00, 0x00, 0xf3, 0x90, 0x83, 0x3b, 0x00, 0x74,
    0xf9, 0xb8, 0x3c, 0x00, 0x00, 0x00, 0xbf, 0x09, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xf3, 0x90, 0x83,
    0x7b, 0x04, 0x00, 0x74, 0xf8, 0xc7, 0x03, 0x01, 0x00, 0x00, 0x00, 0xb8, 0x3c, 0x00, 0x00, 0x00,
    0xbf, 0x07, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b,
];

/// A program that blocks in `syslog`'s read, which the check keeps from ever
/// returning by parking the reader past the log: for the check that a program
/// killed there with no signal is still released. Exits 1 if the read returns.
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_SYSLOG_PROGRAM: &[u8] = &[
    0x48, 0x83, 0xec, 0x40, 0xb8, 0x67, 0x00, 0x00, 0x00, 0xbf, 0x02, 0x00, 0x00, 0x00, 0x48, 0x89,
    0xe6, 0xba, 0x40, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x01, 0x00,
    0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b,
];

/// A program whose second thread replaces the program while its first waits:
/// the main thread makes a thread with `CLONE_CHILD_CLEARTID` and waits in
/// `FUTEX_WAIT` on a word nobody wakes, for ever; the thread calls
/// `execve("/exec-target", ["/exec-target"], NULL)`. The kernel must end the
/// main thread and run the target, whose status the process ends with. 96 is
/// a refused `clone`, 97 an `execve` that returned.
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_DETHREAD_PROGRAM: &[u8] = &[
    0xb8, 0x09, 0x00, 0x00, 0x00, 0x31, 0xff, 0xbe, 0x00, 0x00, 0x01, 0x00, 0xba, 0x03, 0x00, 0x00,
    0x00, 0x41, 0xba, 0x22, 0x00, 0x00, 0x00, 0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, 0x45, 0x31,
    0xc9, 0x0f, 0x05, 0x48, 0x89, 0xc3, 0xc7, 0x03, 0x00, 0x00, 0x00, 0x00, 0xc7, 0x43, 0x04, 0xff,
    0xff, 0xff, 0xff, 0xbf, 0x00, 0x0f, 0x21, 0x00, 0x48, 0x8d, 0xb3, 0x00, 0x00, 0x01, 0x00, 0x31,
    0xd2, 0x4c, 0x8d, 0x53, 0x04, 0x45, 0x31, 0xc0, 0xb8, 0x38, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48,
    0x85, 0xc0, 0x78, 0x37, 0x75, 0x22, 0x48, 0x8d, 0x3d, 0x3c, 0x00, 0x00, 0x00, 0x6a, 0x00, 0x57,
    0x48, 0x89, 0xe6, 0x31, 0xd2, 0xb8, 0x3b, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00,
    0x00, 0xbf, 0x61, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0xca, 0x00, 0x00, 0x00, 0x48, 0x89, 0xdf,
    0x31, 0xf6, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0x0f, 0x05, 0xeb, 0xed, 0xb8, 0xe7, 0x00, 0x00, 0x00,
    0xbf, 0x60, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b, 0x2f, 0x65, 0x78, 0x65, 0x63, 0x2d, 0x74,
    0x61, 0x72, 0x67, 0x65, 0x74, 0x00,
];

/// A program of three threads on a page at `0x6000_0000`: the main thread
/// counts at 0 and a second thread at 4, each for ever, and a third waits in
/// `FUTEX_WAIT` with no timeout on the word at 8, recording at 12 what the wait
/// returned -- 1 until it does -- and exiting if it ever does. 96 is a refused
/// `mmap` or `clone`.
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_STOPPED_PROGRAM: &[u8] = &[
    0xb8, 0x09, 0x00, 0x00, 0x00, 0xbf, 0x00, 0x00, 0x00, 0x60, 0xbe, 0x00, 0x10, 0x00, 0x00, 0xba,
    0x03, 0x00, 0x00, 0x00, 0x41, 0xba, 0x32, 0x00, 0x00, 0x00, 0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff,
    0xff, 0x45, 0x31, 0xc9, 0x0f, 0x05, 0x48, 0x3d, 0x00, 0x00, 0x00, 0x60, 0x0f, 0x85, 0xa2, 0x00,
    0x00, 0x00, 0x48, 0x89, 0xc3, 0xc7, 0x43, 0x0c, 0x01, 0x00, 0x00, 0x00, 0xb8, 0x09, 0x00, 0x00,
    0x00, 0x31, 0xff, 0xbe, 0x00, 0x00, 0x02, 0x00, 0xba, 0x03, 0x00, 0x00, 0x00, 0x41, 0xba, 0x22,
    0x00, 0x00, 0x00, 0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, 0x45, 0x31, 0xc9, 0x0f, 0x05, 0x48,
    0x85, 0xc0, 0x78, 0x70, 0x49, 0x89, 0xc4, 0xbf, 0x00, 0x0f, 0x01, 0x00, 0x49, 0x8d, 0xb4, 0x24,
    0x00, 0x00, 0x01, 0x00, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0x45, 0x31, 0xc0, 0xb8, 0x38, 0x00, 0x00,
    0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x78, 0x4c, 0x74, 0x27, 0xbf, 0x00, 0x0f, 0x01, 0x00, 0x49,
    0x8d, 0xb4, 0x24, 0x00, 0x00, 0x02, 0x00, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0x45, 0x31, 0xc0, 0xb8,
    0x38, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x78, 0x29, 0x74, 0x09, 0xff, 0x03, 0xeb,
    0xfc, 0xff, 0x43, 0x04, 0xeb, 0xfb, 0xb8, 0xca, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x7b, 0x08, 0x31,
    0xf6, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0x0f, 0x05, 0x89, 0x43, 0x0c, 0xb8, 0x3c, 0x00, 0x00, 0x00,
    0x31, 0xff, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x60, 0x00, 0x00, 0x00, 0x0f, 0x05,
    0x0f, 0x0b,
];

/// A program of two threads with two handlers, on a page at `0x6000_0000`:
/// `SIGUSR1`'s handler writes `gettid()` at 0; `SIGUSR2`'s, whose mask blocks
/// `SIGUSR1`, waits in 10 ms timed futex waits -- 300 at most -- for that word
/// to be set. The main thread records its id at 12 and waits in `FUTEX_WAIT`
/// on the word at 8 for ever; the second records its id at 4 and spins. 96 is
/// a refused `mmap`, `rt_sigaction` or `clone`.
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_HANDOFF_PROGRAM: &[u8] = &[
    0xb8, 0x09, 0x00, 0x00, 0x00, 0xbf, 0x00, 0x00, 0x00, 0x60, 0xbe, 0x00, 0x10, 0x00, 0x00, 0xba,
    0x03, 0x00, 0x00, 0x00, 0x41, 0xba, 0x32, 0x00, 0x00, 0x00, 0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff,
    0xff, 0x45, 0x31, 0xc9, 0x0f, 0x05, 0x48, 0x3d, 0x00, 0x00, 0x00, 0x60, 0x0f, 0x85, 0x5d, 0x01,
    0x00, 0x00, 0x48, 0x89, 0xc3, 0xb8, 0x09, 0x00, 0x00, 0x00, 0x31, 0xff, 0xbe, 0x00, 0x00, 0x01,
    0x00, 0xba, 0x03, 0x00, 0x00, 0x00, 0x41, 0xba, 0x22, 0x00, 0x00, 0x00, 0x49, 0xc7, 0xc0, 0xff,
    0xff, 0xff, 0xff, 0x45, 0x31, 0xc9, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x88, 0x2e, 0x01, 0x00,
    0x00, 0x49, 0x89, 0xc4, 0x48, 0xc7, 0x43, 0x18, 0x00, 0x00, 0x00, 0x00, 0x48, 0xc7, 0x43, 0x20,
    0x80, 0x96, 0x98, 0x00, 0x48, 0x8d, 0x05, 0xd4, 0x00, 0x00, 0x00, 0x48, 0x89, 0x43, 0x40, 0x48,
    0xc7, 0x43, 0x48, 0x00, 0x00, 0x00, 0x04, 0x48, 0x8d, 0x05, 0xf8, 0x00, 0x00, 0x00, 0x48, 0x89,
    0x43, 0x50, 0x48, 0xc7, 0x43, 0x58, 0x00, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x05, 0xbd, 0x00, 0x00,
    0x00, 0x48, 0x89, 0x43, 0x60, 0x48, 0xc7, 0x43, 0x68, 0x00, 0x00, 0x00, 0x04, 0x48, 0x8d, 0x05,
    0xd2, 0x00, 0x00, 0x00, 0x48, 0x89, 0x43, 0x70, 0x48, 0xc7, 0x43, 0x78, 0x00, 0x02, 0x00, 0x00,
    0xb8, 0x0d, 0x00, 0x00, 0x00, 0xbf, 0x0a, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x73, 0x40, 0x31, 0xd2,
    0x41, 0xba, 0x08, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x85, 0xae, 0x00, 0x00,
    0x00, 0xb8, 0x0d, 0x00, 0x00, 0x00, 0xbf, 0x0c, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x73, 0x60, 0x31,
    0xd2, 0x41, 0xba, 0x08, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x85, 0x8d, 0x00,
    0x00, 0x00, 0xb8, 0xba, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x89, 0x43, 0x0c, 0xbf, 0x00, 0x0f, 0x01,
    0x00, 0x49, 0x8d, 0xb4, 0x24, 0x00, 0x00, 0x01, 0x00, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0x45, 0x31,
    0xc0, 0xb8, 0x38, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x78, 0x62, 0x74, 0x14, 0xb8,
    0xca, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x7b, 0x08, 0x31, 0xf6, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0x0f,
    0x05, 0xeb, 0xec, 0xb8, 0xba, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x89, 0x43, 0x04, 0xeb, 0xfe, 0xb8,
    0xba, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb9, 0x00, 0x00, 0x00, 0x60, 0x89, 0x01, 0xc3, 0x41, 0xbd,
    0x2c, 0x01, 0x00, 0x00, 0xb9, 0x00, 0x00, 0x00, 0x60, 0x83, 0x39, 0x00, 0x75, 0x17, 0xb8, 0xca,
    0x00, 0x00, 0x00, 0x48, 0x89, 0xcf, 0x31, 0xf6, 0x31, 0xd2, 0x4c, 0x8d, 0x51, 0x18, 0x0f, 0x05,
    0x41, 0xff, 0xcd, 0x75, 0xdf, 0xc3, 0xb8, 0x0f, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b, 0xb8,
    0xe7, 0x00, 0x00, 0x00, 0xbf, 0x60, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b,
];

/// A program that signals itself and exits with what its handler did: 77 when
/// the handler ran on a frame of its own, saw the right `siginfo` and mask, and
/// `rt_sigreturn` put back the registers the frame held -- including the one
/// the handler changed through the `ucontext`. 7 if the handler never ran or
/// the change did not come back; 98 if the handler saw something wrong; 99 if
/// a call or the mask after the return was wrong.
///
/// ```text
///   rt_sigaction(SIGUSR1, {handler, SA_SIGINFO | SA_RESTORER, restorer, 0}, NULL, 8)
///   movl $7, %ebx
///   tgkill(getpid(), getpid(), SIGUSR1)         ; must return 0
///   rt_sigprocmask(SIG_BLOCK, NULL, &old, 8)    ; old must be empty
///   exit_group(%ebx)
/// handler:                                      ; rdi = 10, rsi = siginfo, rdx = ucontext
///   cmpl $10, %edi ; cmpl $10, (%rsi)
///   rt_sigprocmask(SIG_BLOCK, NULL, &mask, 8)   ; mask must be SIGUSR1's bit
///   addq $70, 128(%rdx)                         ; uc_mcontext.rbx += 70
///   ret                                         ; into the restorer
/// restorer: movl $15, %eax ; syscall            ; rt_sigreturn
/// ```
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_SIGNAL_PROGRAM: &[u8] = &[
    0x48, 0x83, 0xec, 0x20, 0x48, 0x8d, 0x05, 0x8f, 0x00, 0x00, 0x00, 0x48, 0x89, 0x04, 0x24, 0x48,
    0xc7, 0x44, 0x24, 0x08, 0x04, 0x00, 0x00, 0x04, 0x48, 0x8d, 0x05, 0xbf, 0x00, 0x00, 0x00, 0x48,
    0x89, 0x44, 0x24, 0x10, 0x48, 0xc7, 0x44, 0x24, 0x18, 0x00, 0x00, 0x00, 0x00, 0xb8, 0x0d, 0x00,
    0x00, 0x00, 0xbf, 0x0a, 0x00, 0x00, 0x00, 0x48, 0x89, 0xe6, 0x31, 0xd2, 0x41, 0xba, 0x08, 0x00,
    0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75, 0x45, 0xbb, 0x07, 0x00, 0x00, 0x00, 0xb8, 0x27,
    0x00, 0x00, 0x00, 0x0f, 0x05, 0x89, 0xc7, 0x89, 0xc6, 0xba, 0x0a, 0x00, 0x00, 0x00, 0xb8, 0xea,
    0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75, 0x24, 0x31, 0xff, 0x31, 0xf6, 0x48, 0x89,
    0xe2, 0x41, 0xba, 0x08, 0x00, 0x00, 0x00, 0xb8, 0x0e, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x83,
    0x3c, 0x24, 0x00, 0x75, 0x09, 0x89, 0xdf, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xbf, 0x63,
    0x00, 0x00, 0x00, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x83, 0xff, 0x0a, 0x75, 0x33, 0x83,
    0x3e, 0x0a, 0x75, 0x2e, 0x49, 0x89, 0xd4, 0x48, 0x83, 0xec, 0x08, 0x31, 0xff, 0x31, 0xf6, 0x48,
    0x89, 0xe2, 0x41, 0xba, 0x08, 0x00, 0x00, 0x00, 0xb8, 0x0e, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x58,
    0x48, 0x3d, 0x00, 0x02, 0x00, 0x00, 0x75, 0x0a, 0x49, 0x83, 0x84, 0x24, 0x80, 0x00, 0x00, 0x00,
    0x46, 0xc3, 0xbf, 0x62, 0x00, 0x00, 0x00, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0x0f,
    0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b,
];

/// A program whose `SIGUSR1` handler poisons the `XSAVE` header of the frame
/// it runs on and returns: 79 when the frame had one and the program ran on
/// after `rt_sigreturn`, 78 when it had `FXSAVE` alone, 99 when a call failed.
///
/// ```text
///   rt_sigaction(SIGUSR1, {handler, SA_SIGINFO | SA_RESTORER, restorer, 0}, NULL, 8)
///   movl $78, %ebx
///   tgkill(getpid(), getpid(), SIGUSR1)         ; must return 0
///   exit_group(%ebx)
/// handler:                                      ; rdx = ucontext
///   movq 224(%rdx), %rcx                        ; uc_mcontext.fpstate
///   cmpl $FP_XSTATE_MAGIC1, 464(%rcx) ; jne 1f
///   movq $-1, 512(%rcx) ; 520 ; 528 ; 568       ; XSTATE_BV, XCOMP_BV, reserved
///   addq $1, 128(%rdx)                          ; uc_mcontext.rbx += 1
/// 1: ret                                        ; into the restorer
/// restorer: movl $15, %eax ; syscall            ; rt_sigreturn
/// ```
///
/// Assembled by GNU as and read back with `objcopy -O binary`.
pub(crate) const USER_XSTATE_PROGRAM: &[u8] = &[
    0x48, 0x83, 0xec, 0x20, 0x48, 0x8d, 0x05, 0x6c, 0x00, 0x00, 0x00, 0x48, 0x89, 0x04, 0x24, 0x48,
    0xc7, 0x44, 0x24, 0x08, 0x04, 0x00, 0x00, 0x04, 0x48, 0x8d, 0x05, 0xa5, 0x00, 0x00, 0x00, 0x48,
    0x89, 0x44, 0x24, 0x10, 0x48, 0xc7, 0x44, 0x24, 0x18, 0x00, 0x00, 0x00, 0x00, 0xb8, 0x0d, 0x00,
    0x00, 0x00, 0xbf, 0x0a, 0x00, 0x00, 0x00, 0x48, 0x89, 0xe6, 0x31, 0xd2, 0x41, 0xba, 0x08, 0x00,
    0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x85, 0x80, 0x00, 0x00, 0x00, 0xbb, 0x4e, 0x00,
    0x00, 0x00, 0xb8, 0x27, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x89, 0xc7, 0x89, 0xc6, 0xba, 0x0a, 0x00,
    0x00, 0x00, 0xb8, 0xea, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75, 0x5f, 0x89, 0xdf,
    0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x8b, 0x8a, 0xe0, 0x00, 0x00, 0x00, 0x48, 0x85,
    0xc9, 0x74, 0x40, 0x81, 0xb9, 0xd0, 0x01, 0x00, 0x00, 0x53, 0x58, 0x50, 0x46, 0x75, 0x34, 0x48,
    0xc7, 0x81, 0x00, 0x02, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0x48, 0xc7, 0x81, 0x08, 0x02, 0x00,
    0x00, 0xff, 0xff, 0xff, 0xff, 0x48, 0xc7, 0x81, 0x10, 0x02, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff,
    0x48, 0xc7, 0x81, 0x38, 0x02, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0x48, 0x83, 0x82, 0x80, 0x00,
    0x00, 0x00, 0x01, 0xc3, 0xb8, 0x0f, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b, 0xbf, 0x63, 0x00,
    0x00, 0x00, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b,
];

/// A program that `execve`s `/exec-target` and, if that returns, exits with
/// the error number: the target's own status when it exists, 2 (`ENOENT`) when
/// it does not.
///
/// ```text
///   leaq path(%rip), %rdi ; pushq $0 ; pushq %rdi ; movq %rsp, %rsi
///   xorl %edx, %edx ; movl $59, %eax ; syscall ; execve(path, [path], NULL)
///   negl %eax ; movl %eax, %edi ; movl $231, %eax ; syscall ; exit with errno
/// path: "/exec-target\0"
/// ```
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_EXEC_PROGRAM: &[u8] = &[
    0x48, 0x8d, 0x3d, 0x1c, 0x00, 0x00, 0x00, 0x6a, 0x00, 0x57, 0x48, 0x89, 0xe6, 0x31, 0xd2, 0xb8,
    0x3b, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xf7, 0xd8, 0x89, 0xc7, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f,
    0x05, 0x0f, 0x0b, 0x2f, 0x65, 0x78, 0x65, 0x63, 0x2d, 0x74, 0x61, 0x72, 0x67, 0x65, 0x74, 0x00,
];

/// A program that sets its trap flag and makes a system call: `exit_group`,
/// with a status nothing else produces.
///
/// For the check that `SYSCALL` masks the trap flag. If it did not, the
/// processor would single-step the first instruction of the trampoline, in
/// ring 0 and still on the program's stack, and the `#DB` would stop the
/// kernel before the call was served. `popfq` setting the flag does not trap
/// after itself, only after the instruction that follows it, which is why the
/// call comes directly after it.
///
/// **The call is one that does not return.** `SYSRET` gives the program its
/// flags back, trap flag included, and the processor then traps in ring 3 at
/// the return address before running anything there. That trap raises
/// `SIGTRAP`, which by default ends the program -- a status of its own, and not
/// the one this check wants to read. `exit_group`'s status says only that the
/// call made with the flag set was served.
///
/// ```text
///   movl $231, %eax ; movl $231, %edi
///   pushfq ; orq $0x100, (%rsp) ; popfq   ; the trap flag, from here on
///   syscall                               ; exit_group(231), single-stepped
///   ud2
/// ```
///
/// Assembled by hand and checked with `objdump -D -b binary -mi386:x86-64`.
pub(crate) const USER_STEP_PROGRAM: &[u8] = &[
    0xb8, 0xe7, 0x00, 0x00, 0x00, // movl $231, %eax
    0xbf, 0xe7, 0x00, 0x00, 0x00, // movl $231, %edi
    0x9c, // pushfq
    0x48, 0x81, 0x0c, 0x24, 0x00, 0x01, 0x00, 0x00, // orq $0x100, (%rsp)
    0x9d, // popfq
    0x0f, 0x05, // syscall
    0x0f, 0x0b, // ud2
];

/// The status [`USER_STEP_PROGRAM`] exits with.
pub(crate) const USER_STEP_STATUS: i32 = 231;

/// A program that spins, then writes a tagged line and exits with a status it
/// reads out of its own image.
///
/// For the check that two programs run at once: it spins long enough to be
/// preempted in ring 3, which a program run with interrupts masked never is.
/// The last ten bytes are the layout every architecture's copy shares, so the
/// check patches them without knowing the instruction set -- the tag character
/// and the newline, then the loop count and the exit status as little-endian
/// words, both loaded RIP-relative.
///
/// Its stack pointer is recorded before the loop and compared after, and a
/// program whose stack pointer changed while it was preempted exits with 99
/// instead. The kernel must give every program back its own.
///
/// ```text
///   movq  %rsp, %r8
///   movl  count(%rip), %ecx
/// 1: decq %rcx
///   jnz   1b
///   cmpq  %rsp, %r8 ; jne 2f
///   movl $1, %eax ; movl $1, %edi ; leaq msg(%rip), %rsi ; movl $16, %edx ; syscall
///   movl $231, %eax ; movl status(%rip), %edi ; syscall
/// 2: movl $231, %eax ; movl $99, %edi ; syscall
///   ud2
/// msg: "spinning task ?\n"   count: .long   status: .long
/// ```
///
/// Assembled by rustc's LLVM and read back out of the object file.
pub(crate) const USER_SPIN_PROGRAM: &[u8] = &[
    0x49, 0x89, 0xe0, 0x8b, 0x0d, 0x4d, 0x00, 0x00, 0x00, 0x48, 0xff, 0xc9, 0x75, 0xfb, 0x49, 0x39,
    0xe0, 0x75, 0x25, 0xb8, 0x01, 0x00, 0x00, 0x00, 0xbf, 0x01, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x35,
    0x22, 0x00, 0x00, 0x00, 0xba, 0x10, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00, 0x00,
    0x8b, 0x3d, 0x24, 0x00, 0x00, 0x00, 0x0f, 0x05, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0xbf, 0x63, 0x00,
    0x00, 0x00, 0x0f, 0x05, 0x0f, 0x0b, 0x73, 0x70, 0x69, 0x6e, 0x6e, 0x69, 0x6e, 0x67, 0x20, 0x74,
    0x61, 0x73, 0x6b, 0x20, 0x3f, 0x0a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// The stage 9 exit test's program: one side of a conversation over a channel,
/// spoken entirely in the native ABI.
///
/// Two copies run as two processes, told apart by a role byte the check
/// patches in. The sender (`s`) creates a VMO, writes a secret into it, sends
/// `ping` with the VMO's handle over its bootstrap channel, waits for the
/// reply, and exits 0 only if the reply is the secret. The receiver (any other
/// role) waits, reads the message and the handle, reads the secret back out of
/// the VMO through the handle it was given, and sends that as the reply.
///
/// A failure exits with the number of the step that failed, so the status is
/// the diagnosis: the sender's calls are 1 to 5 and its comparison 6; the
/// receiver's calls are 11 to 14, and a message without exactly one handle is
/// 20. The number is kept in a register the kernel preserves across a call and
/// no call takes as an argument.
///
/// The layout every architecture's copy shares, so the check patches it
/// without knowing the instruction set: a branch padded to four bytes, the
/// secret at 4, `ping` at 23, and eight bytes at 28 -- the role, a newline, two
/// zero bytes, and the bootstrap handle as a little-endian word. The data comes
/// first rather than last because ARM state's `adr` reaches only what an 8-bit
/// rotated immediate can express, and data after the code was out of that
/// reach.
///
/// ```text
///   b start ; secret: "carried by a handle" ; ping: "ping" ; role: '?' '\n' 0 0 ; handle: .long
/// start:
///   load the handle and the role; reserve 256 bytes of stack for buffers
///   sender:   vmo_create(64) ; vmo_write(vmo, secret, 19, &0)
///             channel_write(h, ping, 4, &vmo, 1)
///             object_wait_one(h, READABLE | PEER_CLOSED, null, null)
///             channel_read(h, buf, 64, handles, 4, &actual)
///             exit(actual.bytes == 19 && buf == secret ? 0 : 1)
///   receiver: object_wait_one(h, READABLE, null, null)
///             channel_read(h, buf, 64, handles, 4, &actual)
///             vmo_read(handles[0], reply, 19, &0) ; channel_write(h, reply, 19, null, 0)
///             exit(0)
/// ```
///
/// Assembled by rustc's LLVM and read back out of the object file, as
/// [`USER_SPIN_PROGRAM`] was.
pub(crate) const USER_NATIVE_PROGRAM: &[u8] = &[
    0xeb, 0x22, 0x90, 0x90, 0x63, 0x61, 0x72, 0x72, 0x69, 0x65, 0x64, 0x20, 0x62, 0x79, 0x20, 0x61,
    0x20, 0x68, 0x61, 0x6e, 0x64, 0x6c, 0x65, 0x70, 0x69, 0x6e, 0x67, 0x00, 0x3f, 0x0a, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x44, 0x8b, 0x25, 0xf5, 0xff, 0xff, 0xff, 0x44, 0x0f, 0xb6, 0x2d, 0xe9,
    0xff, 0xff, 0xff, 0x48, 0x81, 0xec, 0x00, 0x01, 0x00, 0x00, 0x48, 0xc7, 0x84, 0x24, 0x98, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x41, 0x80, 0xfd, 0x73, 0x0f, 0x85, 0x1e, 0x01, 0x00, 0x00,
    0x41, 0xbf, 0x01, 0x00, 0x00, 0x00, 0xb8, 0x20, 0x10, 0x00, 0x00, 0xbf, 0x40, 0x00, 0x00, 0x00,
    0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x8e, 0xbd, 0x01, 0x00, 0x00, 0x41, 0x89, 0xc6, 0x41, 0xbf,
    0x02, 0x00, 0x00, 0x00, 0xb8, 0x22, 0x10, 0x00, 0x00, 0x44, 0x89, 0xf7, 0x48, 0x8d, 0x35, 0x81,
    0xff, 0xff, 0xff, 0xba, 0x13, 0x00, 0x00, 0x00, 0x4c, 0x8d, 0x94, 0x24, 0x98, 0x00, 0x00, 0x00,
    0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x85, 0x8d, 0x01, 0x00, 0x00, 0x44, 0x89, 0xb4, 0x24, 0xa0,
    0x00, 0x00, 0x00, 0x41, 0xbf, 0x03, 0x00, 0x00, 0x00, 0xb8, 0x11, 0x10, 0x00, 0x00, 0x44, 0x89,
    0xe7, 0x48, 0x8d, 0x35, 0x5f, 0xff, 0xff, 0xff, 0xba, 0x04, 0x00, 0x00, 0x00, 0x4c, 0x8d, 0x94,
    0x24, 0xa0, 0x00, 0x00, 0x00, 0x41, 0xb8, 0x01, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0,
    0x0f, 0x85, 0x52, 0x01, 0x00, 0x00, 0x41, 0xbf, 0x04, 0x00, 0x00, 0x00, 0xb8, 0x08, 0x10, 0x00,
    0x00, 0x44, 0x89, 0xe7, 0xbe, 0x05, 0x00, 0x00, 0x00, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0x0f, 0x05,
    0x48, 0x85, 0xc0, 0x0f, 0x85, 0x2f, 0x01, 0x00, 0x00, 0x41, 0xbf, 0x05, 0x00, 0x00, 0x00, 0xb8,
    0x12, 0x10, 0x00, 0x00, 0x44, 0x89, 0xe7, 0x48, 0x89, 0xe6, 0xba, 0x40, 0x00, 0x00, 0x00, 0x4c,
    0x8d, 0x94, 0x24, 0x80, 0x00, 0x00, 0x00, 0x41, 0xb8, 0x04, 0x00, 0x00, 0x00, 0x4c, 0x8d, 0x8c,
    0x24, 0x90, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x85, 0xf8, 0x00, 0x00, 0x00,
    0x41, 0xbf, 0x06, 0x00, 0x00, 0x00, 0x83, 0xbc, 0x24, 0x90, 0x00, 0x00, 0x00, 0x13, 0x0f, 0x85,
    0xe4, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x35, 0xb9, 0xfe, 0xff, 0xff, 0x48, 0x89, 0xe7, 0xb9, 0x13,
    0x00, 0x00, 0x00, 0x8a, 0x06, 0x3a, 0x07, 0x0f, 0x85, 0xcb, 0x00, 0x00, 0x00, 0x48, 0xff, 0xc6,
    0x48, 0xff, 0xc7, 0xff, 0xc9, 0x75, 0xec, 0x31, 0xff, 0xe9, 0xbd, 0x00, 0x00, 0x00, 0x41, 0xbf,
    0x0b, 0x00, 0x00, 0x00, 0xb8, 0x08, 0x10, 0x00, 0x00, 0x44, 0x89, 0xe7, 0xbe, 0x01, 0x00, 0x00,
    0x00, 0x31, 0xd2, 0x45, 0x31, 0xd2, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x0f, 0x85, 0x97, 0x00, 0x00,
    0x00, 0x41, 0xbf, 0x0c, 0x00, 0x00, 0x00, 0xb8, 0x12, 0x10, 0x00, 0x00, 0x44, 0x89, 0xe7, 0x48,
    0x89, 0xe6, 0xba, 0x40, 0x00, 0x00, 0x00, 0x4c, 0x8d, 0x94, 0x24, 0x80, 0x00, 0x00, 0x00, 0x41,
    0xb8, 0x04, 0x00, 0x00, 0x00, 0x4c, 0x8d, 0x8c, 0x24, 0x90, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48,
    0x85, 0xc0, 0x75, 0x64, 0x41, 0xbf, 0x14, 0x00, 0x00, 0x00, 0x83, 0xbc, 0x24, 0x94, 0x00, 0x00,
    0x00, 0x01, 0x75, 0x54, 0x41, 0xbf, 0x0d, 0x00, 0x00, 0x00, 0xb8, 0x21, 0x10, 0x00, 0x00, 0x8b,
    0xbc, 0x24, 0x80, 0x00, 0x00, 0x00, 0x48, 0x8d, 0x74, 0x24, 0x40, 0xba, 0x13, 0x00, 0x00, 0x00,
    0x4c, 0x8d, 0x94, 0x24, 0x98, 0x00, 0x00, 0x00, 0x0f, 0x05, 0x48, 0x85, 0xc0, 0x75, 0x29, 0x41,
    0xbf, 0x0e, 0x00, 0x00, 0x00, 0xb8, 0x11, 0x10, 0x00, 0x00, 0x44, 0x89, 0xe7, 0x48, 0x8d, 0x74,
    0x24, 0x40, 0xba, 0x13, 0x00, 0x00, 0x00, 0x45, 0x31, 0xd2, 0x45, 0x31, 0xc0, 0x0f, 0x05, 0x48,
    0x85, 0xc0, 0x75, 0x04, 0x31, 0xff, 0xeb, 0x03, 0x44, 0x89, 0xff, 0xb8, 0xe7, 0x00, 0x00, 0x00,
    0x0f, 0x05, 0xeb, 0xfe,
];

/// One byte from the console, if one has arrived: from the receive ring once
/// the port's interrupt is installed, straight from the port before then.
pub(crate) fn read_console_byte() -> Option<u8> {
    crate::console::input::read_byte(console::read_byte)
}

/// One byte straight from the console port, for its receive interrupt.
pub(crate) fn take_console_byte() -> Option<u8> {
    console::read_byte()
}

/// The I/O APIC input the console port was given, between
/// [`console_receive_irq`] choosing it and [`enable_console_receive`] opening it.
static CONSOLE_INPUT: SpinLock<Option<(apic::IoApicInput, u64)>> = SpinLock::new(None);

/// The interrupt the console port receives on.
///
/// COM1 raises ISA IRQ 4, which reaches an I/O APIC input wherever the MADT
/// says. That input is given a device vector and routed to it masked, so
/// nothing arrives before the generic layer has a handler; `None` — input stays
/// polled — when the machine has no readable MADT, no I/O APIC covering the
/// line, or no vector left.
///
/// Also registers with `iommu` what interrupt remapping's bring-up needs of
/// this architecture at stage 10: the vectors handed out, and this line,
/// which it converts to remappable format (`iommu::remapping`).
pub(crate) fn console_receive_irq(view: &BootView<'_>) -> Option<u32> {
    let routed = route_console(view);
    crate::iommu::remapping::note_hooks(crate::iommu::remapping::Hooks {
        vector: msi::take_vector,
        taken: msi::taken,
        nmis: paranoid::nmis,
        line: routed.map(|(io_apic, _)| crate::iommu::LiveLine {
            io_apic,
            routed: console_vector,
            mask: mask_console_line,
            unmask: unmask_console_line,
            retire: retire_console_line,
            restore: restore_console_line,
            convert: convert_console_line,
            loop_back: loop_back_console,
        }),
    });
    routed.map(|(_, irq)| irq)
}

/// Route the console's line masked, as [`console_receive_irq`] describes:
/// its I/O APIC's MADT identifier and the interrupt's number.
fn route_console(view: &BootView<'_>) -> Option<(u8, u32)> {
    let firmware = crate::discovery::acpi::Firmware::open(view).ok()?;
    let madt = firmware.acpi().madt().ok()?;
    let input = apic::IoApicInput::for_isa(&madt, console::ISA_IRQ).ok()?;
    let vector = msi::allocate_vector()?;
    input.route(vector, true);
    *CONSOLE_INPUT.lock() = Some((input, vector));
    Some((input.io_apic_id(), (vector - trap::IRQ_BASE) as u32))
}

/// The vector the console's line is routed to now.
fn console_vector() -> Option<u8> {
    CONSOLE_INPUT
        .lock()
        .and_then(|(_, vector)| u8::try_from(vector).ok())
}

/// Mask the console's line for its conversion, under its lock, and say what
/// it is routed to.
fn mask_console_line() -> Option<crate::iommu::remapping::Masked> {
    let (input, _) = (*CONSOLE_INPUT.lock())?;
    let held = CONSOLE_INPUT.lock();
    input.mask();
    let (vector, apic_id) = input.routed();
    drop(held);
    Some(crate::iommu::remapping::Masked {
        vector,
        apic_id,
        level: input.level(),
    })
}

/// Unmask the console's line as it is, a conversion having been given up.
fn unmask_console_line() {
    let held = CONSOLE_INPUT.lock();
    if let Some((input, _)) = *held {
        input.write(input.redirection() & !(1 << 16));
    }
}

/// Loop `byte` back through the console's port to its own receiver, with
/// the port's transmit side quiet.
fn loop_back_console(byte: u8) {
    crate::console::output::with_port_quiet(|| console::loop_back(byte));
}

/// The interrupt number `vector` arrives as.
fn vector_number(vector: u8) -> u32 {
    (u64::from(vector) - trap::IRQ_BASE) as u32
}

/// Move the console line's handler from `old` to `new` through the generic
/// interrupt layer, with the line masked, leaving `old` a handler that counts
/// what still arrives on it.
fn retire_console_line(old: u8, new: u8) -> Result<(), &'static str> {
    crate::irq::move_handler(
        vector_number(old),
        vector_number(new),
        crate::iommu::remapping::note_retired_delivery,
    )
    .map_err(|_| "its handler could not be moved to its new vector")
}

/// Give the console line's handler back to `old`, a conversion given up.
fn restore_console_line(old: u8, new: u8) {
    crate::irq::return_handler(vector_number(new), vector_number(old));
}

/// Finish the console line's conversion to interrupt remapping, its handler
/// already on `new`: its entry written in remappable format naming entry
/// `handle`, with `new` as its vector and its trigger the entry's,
/// unmasked; then the port serviced once, as its handler would, since an
/// edge raised while the line was masked was lost.
fn convert_console_line(new: u8, handle: u16) -> Result<(), &'static str> {
    {
        let mut held = CONSOLE_INPUT.lock();
        let Some((input, _)) = *held else {
            return Err("the console's line is not routed");
        };
        input.write(ferrix_paging::vtd::remap::redirection_entry(
            handle,
            new,
            input.level(),
            input.active_low(),
            false,
        ));
        *held = Some((input, u64::from(new)));
    }
    crate::irq::dispatch(vector_number(new));
    Ok(())
}

/// Enable the console port's receive interrupt, at the I/O APIC and in the
/// port. Routed to the calling processor, like the Arm ports' GIC line: a
/// device interrupt goes to the core that enables it.
pub(crate) fn enable_console_receive(_irq: u32) {
    let Some((input, vector)) = *CONSOLE_INPUT.lock() else {
        return;
    };
    input.route(vector, false);
    console::enable_receive_interrupt();
}

/// `AT_HWCAP` and `AT_HWCAP2` for a program started on this machine.
///
/// Linux reports `CPUID` leaf 1's `EDX` as `AT_HWCAP` on x86-64. `AT_HWCAP2`
/// carries only `FSGSBASE` and ring-3 `MWAIT`, which are bits the kernel sets
/// when it has enabled them for user mode, and this one enables neither.
pub(crate) fn user_hwcaps() -> (u64, u64) {
    (u64::from(core::arch::x86_64::__cpuid(1).edx), 0)
}

/// `AT_PLATFORM` for a program started on this machine.
///
/// `arch/x86/include/asm/elf.h` defines `ELF_PLATFORM` as `utsname()->machine`,
/// which is `"x86_64"` on this architecture and never anything else -- there is
/// no second string the way ARMv7-A has one per core generation. A 32-bit
/// program is told `COMPAT_ELF_PLATFORM`, `"i686"`, which is what its linker
/// builds a platform library path from (`docs/I386.md` §3.4).
pub(crate) const fn user_platform(abi: crate::trap::Abi) -> Option<&'static [u8]> {
    match abi {
        crate::trap::Abi::Native => Some(b"x86_64"),
        crate::trap::Abi::Compat => Some(b"i686"),
    }
}

/// Enter ring 3 for the first time, at `entry` on `stack`, with `argument` in
/// the first argument register, in the mode `abi` names. Does not return.
///
/// # Safety
///
/// (CONTEXT) Must be called by a user task, on its own kernel stack, with its address
/// space installed; `entry` and `stack` must be addresses within it.
pub(crate) unsafe fn enter_user(entry: u64, stack: u64, argument: u64, abi: crate::trap::Abi) -> ! {
    // SAFETY: (CONTEXT) the caller's guarantee, passed straight through.
    unsafe { syscall::enter_user(entry, stack, argument, abi) }
}

/// Service a system call that arrived through the trap vector: `int $0x80`,
/// an i386 system call, from a 32-bit program or a 64-bit one
/// (`docs/I386.md` §3.2, §3.3).
///
/// The i386 convention: the number in `EAX`, the arguments in `EBX`, `ECX`,
/// `EDX`, `ESI`, `EDI` and `EBP`, the result in `EAX`. Each is read as the 32
/// bits a 32-bit program can have put there, zero-extended, as ARMv7-A's
/// entry reads its registers; the upper halves are whatever the processor
/// kept from 64-bit mode, which the program did not choose to pass.
///
/// The dispatcher is handed the trap frame as the caller's registers, which a
/// `fork` or `clone` child resumes from by `IRETQ` in the caller's mode.
/// `sigreturn` and `rt_sigreturn` are answered here rather than dispatched:
/// they replace the whole frame, and have no result to write into it.
///
/// # Errors
///
/// A trap-vector system call that did not come from ring 3.
pub(crate) fn system_call(frame: &mut TrapFrame) -> Result<(), &'static str> {
    use crate::trap::{Abi, Outcome, SyscallArgs};

    if !frame.came_from_user() {
        return Err("a system call through the trap vector from ring 0");
    }
    let word = |register: u64| register & 0xFFFF_FFFF;
    let args = SyscallArgs {
        abi: Abi::Compat,
        number: word(frame.rax) as usize,
        args: [
            word(frame.rbx),
            word(frame.rcx),
            word(frame.rdx),
            word(frame.rsi),
            word(frame.rdi),
            word(frame.rbp),
        ],
        // `int $0x80` pushed the address of the next instruction.
        ip: frame.rip,
    };

    // The registered filter looks at the call first, before `sigreturn` and
    // `rt_sigreturn` are answered below (`docs/SECCOMP.md` §3.3), with this
    // entry's own architecture token: `AUDIT_ARCH_I386`, whatever the image
    // the program runs.
    let outcome = match crate::trap::filter_system_call(&args) {
        Some(outcome) => outcome,
        None => {
            // `sigreturn` for a handler without `SA_SIGINFO`, `rt_sigreturn`
            // for one with: an i386 program has both frames (`signal::compat`).
            let returning = match decode_compat_syscall(args.number) {
                Some(Syscall::RtSigreturn) => Some(true),
                Some(Syscall::Sigreturn) => Some(false),
                _ => None,
            };
            if let Some(rt) = returning {
                let mut context = UserContext::from_trap(frame);
                enable_interrupts();
                if let Some(path) = crate::trap::return_path() {
                    (path.sigreturn)(&mut context, rt);
                }
                disable_interrupts();
                context.store_trap(frame);
                return Ok(());
            }

            // Open while the call is served, as `SYSCALL`'s path opens them: the
            // gate closed them on entry.
            let regs = UserRegs::Trap(*frame);
            enable_interrupts();
            let outcome = crate::trap::system_call(&args, Some(&regs));
            disable_interrupts();
            outcome
        }
    };

    match outcome {
        // Sign-extended, as Linux stores a compat call's result: a 32-bit
        // program reads `EAX`, and a 64-bit one issuing `int $0x80` reads
        // `-errno` in all of `RAX`.
        // `int $0x80` reaches no native call, so has no words to give back.
        Outcome::Return(value) | Outcome::ReturnWords { value, .. } => {
            frame.rax = value as i64 as u64;
        }
        // `execve`: the registers belong to a program that no longer exists,
        // so they are replaced rather than returned into, in the new image's
        // mode.
        Outcome::Enter { entry, stack, abi } => {
            if abi == Abi::Compat {
                switch::enter_compat_segments();
            }
            UserContext::entering(entry, stack, abi).store_trap(frame);
        }
    }
    Ok(())
}

/// Make a freshly allocated user root usable.
///
/// x86-64 keeps both halves of the address space in one root, so a user root
/// that did not name the kernel's tables would fault on the first instruction
/// of the trap handler it entered — including the page fault handler, which is
/// a triple fault and a silent reset. The kernel's top-level slots are shared
/// rather than copied, so a later kernel mapping appears in every address
/// space without any of them being walked.
pub(crate) fn prepare_user_root(root: u64) {
    crate::mm::share_kernel_slots(root, UPPER_HALF_SLOT..ROOT_SLOTS);
    crate::arch::speculation::forget_root(root);
}

/// Translate this processor's user half through the tables at `root`.
///
/// One write to `CR3`, and nothing else. The flush is the architecture's: a
/// write to `CR3` drops every cached translation that is not marked global,
/// which is precisely the set that belongs to the address space being left.
/// The kernel's own translations are global — `CR4.PGE` is set by the loader
/// and the kernel keeps it — so kernel text, the direct map and the device
/// windows survive the switch and are not re-walked.
///
/// It is therefore emphatically *not* [`flush_tlb`], which is the global
/// flush: using it here would throw away exactly the entries that must be
/// kept, on every switch, for nothing.
///
/// # Safety
///
/// (TRANSLATE) `root` must be a root table [`prepare_user_root`] has made, so that it
/// still names the kernel's upper half — the instruction after this one is a
/// kernel instruction, and the trap taken if it did not translate would be a
/// triple fault. The tables it roots must also stay alive until another root
/// replaces this one on this processor.
pub(crate) unsafe fn install_user_root(root: u64) {
    // SAFETY: (TRANSLATE) the caller guarantees the root carries the kernel's half, which
    // is what maps the code and stack this returns onto.
    unsafe { cpu::write_cr3(root) };
    // `IBPB` and a return stack refill, if this is another program's space
    // than the one this processor last ran. See `speculation`.
    crate::arch::speculation::entered_space(root);
}

/// Go back to translating nothing but the kernel's own tables.
///
/// What a processor picking up a kernel thread does, so that no user address
/// translates while one runs. The alternative — leaving the outgoing process's
/// root installed, because a kernel thread has no user addresses to get wrong
/// — is Linux's lazy TLB, and it is an optimisation that has to keep the
/// address space alive underneath a thread that does not reference it. Stage 6
/// takes the plain version.
///
/// # Safety
///
/// (TRANSLATE) Nothing may still need a user address on this processor.
pub(crate) unsafe fn uninstall_user_root() {
    // SAFETY: (TRANSLATE) the kernel's own root maps everything the kernel runs on, and
    // the caller guarantees no user address is wanted.
    unsafe { cpu::write_cr3(crate::mm::root_table()) };
}

/// Drop the loader's identity map by clearing the lower half of the root
/// table.
///
/// The frames those tables occupied are *not* given back. They are part of the
/// loader's page table pool, which the memory map reports as
/// `MemKind::PageTables` and which the kernel is still running on — the upper
/// half's tables came out of the same pool. Reclaiming the lower half's share
/// would mean tracking which frame of the pool belongs to which half, and the
/// pool is a few dozen frames.
///
/// # Safety
///
/// (TRANSLATE) Nothing may still be executing or reading through the lower half. The
/// kernel runs entirely in the upper half from its first instruction.
pub(crate) unsafe fn drop_identity_map(_view: &BootView<'_>) {
    crate::mm::clear_root_slots(0..UPPER_HALF_SLOT);
}

/// Where a backtrace starts: this function's frame pointer.
#[inline(always)]
pub(crate) fn frame_pointer() -> u64 {
    cpu::frame_pointer()
}

/// Stop the machine, and QEMU with it, once the console has sent its last line.
pub(crate) fn shutdown() -> ! {
    crate::console::drain();
    cpu::debug_exit();
    halt()
}

/// The FADT's reset register, when firmware describes one in I/O space: its
/// port, zero for none, and the value that resets the machine. Recorded by
/// [`init_interrupts`], because by the time anything resets, the tables' memory
/// has been reclaimed.
static ACPI_RESET_PORT: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(0);
static ACPI_RESET_VALUE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// The 8042 keyboard controller's command and status port.
const KBC_PORT: u16 = 0x64;
/// Status bit 1: the controller's input buffer is still full.
const KBC_INPUT_FULL: u8 = 1 << 1;
/// The command that pulses the reset line.
const KBC_PULSE_RESET: u8 = 0xFE;

/// Reset the machine, once the console has sent its last line.
///
/// Three ways, in the order Linux tries them on a PC: the FADT's reset
/// register, the keyboard controller's reset pulse, and a triple fault. Each
/// of the first two gets time to act, and one that returns is a machine that
/// did not reset, so the next is tried. A triple fault cannot return.
pub(crate) fn reset() -> ! {
    crate::console::drain();
    cpu::disable_interrupts();

    let port = ACPI_RESET_PORT.load(core::sync::atomic::Ordering::Relaxed);
    if port != 0 {
        let value = ACPI_RESET_VALUE.load(core::sync::atomic::Ordering::Relaxed);
        // SAFETY: (FIRMWARE) firmware named this port and value as the way to reset the
        // machine, and resetting it is the point.
        unsafe { cpu::outb(port, value) };
        settle();
    }

    for _ in 0..10 {
        for _ in 0..1000 {
            // SAFETY: (FIRMWARE) reading the 8042's status port has no side effect.
            if unsafe { cpu::inb(KBC_PORT) } & KBC_INPUT_FULL == 0 {
                break;
            }
            settle_briefly();
        }
        // SAFETY: (FIRMWARE) the reset pulse is the command's only effect, and on a
        // machine with no controller the write is discarded.
        unsafe { cpu::outb(KBC_PORT, KBC_PULSE_RESET) };
        settle();
    }

    // SAFETY: (FIRMWARE) the last way to reset the machine, taken on purpose.
    unsafe { cpu::triple_fault() }
}

/// About a millisecond, by writing to the POST code port: the delay a PC's I/O
/// bus has always been timed by, and with interrupts masked there is no clock
/// to wait on.
fn settle_briefly() {
    for _ in 0..1000 {
        // SAFETY: (DEVICE) port 0x80 is the POST diagnostic port; writes to it are
        // discarded or shown on a debug card, and nothing reads it.
        unsafe { cpu::outb(0x80, 0) };
    }
}

/// About fifty milliseconds, for a reset that was asked for to take hold.
fn settle() {
    for _ in 0..50 {
        settle_briefly();
    }
}

/// Remember the FADT's reset register for [`reset`].
///
/// Only a register in I/O space is kept. That is where a PC's firmware puts it;
/// one in memory space would need a mapping made by a machine on its way down,
/// and the keyboard controller and the triple fault still follow.
fn record_reset_register(acpi: &ferrix_acpi::Acpi<'_, crate::discovery::acpi::DirectMap>) {
    let Some((register, value)) = acpi.fadt().ok().and_then(|fadt| fadt.reset()) else {
        return;
    };
    if register.address_space_id != ferrix_acpi::GenericAddress::SYSTEM_IO {
        return;
    }
    let Ok(port) = u16::try_from(register.address) else {
        return;
    };
    ACPI_RESET_VALUE.store(value, core::sync::atomic::Ordering::Relaxed);
    ACPI_RESET_PORT.store(port, core::sync::atomic::Ordering::Relaxed);
}

/// Wait until the console port has sent everything written to it.
pub(crate) fn drain_console() {
    console::drain();
}

/// Stop this CPU permanently.
pub(crate) fn halt() -> ! {
    cpu::disable_interrupts();
    loop {
        cpu::hlt();
    }
}

/// Bring up the local APIC, the counter and the timer.
///
/// Order matters twice over. The counter has to exist before the local APIC
/// timer can be calibrated against it, and the interrupt descriptor table has
/// to exist before the local APIC is enabled — an interrupt delivered to a
/// vector with no gate is a fault the CPU cannot report.
///
/// # Safety
///
/// (DEVICE) Must be called exactly once, on the boot CPU, after [`init_traps`] and
/// while interrupts are still masked.
pub(crate) unsafe fn init_interrupts(view: &BootView<'_>) -> Result<Report, &'static str> {
    let firmware = crate::discovery::acpi::Firmware::open(view)
        .map_err(|_| "the machine has no readable ACPI tables")?;
    let acpi = firmware.acpi();
    record_reset_register(&acpi);

    let counter = clock::init(&acpi)?;
    // SAFETY: (DEVICE) called once from `kmain`, on the boot CPU, after `init_traps`
    // filled the IDT and with interrupts masked.
    unsafe { apic::init(&acpi)? };

    Ok(Report {
        counter,
        counter_hz: clock::counter_hz(),
        controller: "APIC",
        timer: "local APIC timer",
        timer_hz: apic::timer_hz(),
    })
}

/// Unmask interrupts on this CPU.
pub(crate) fn enable_interrupts() {
    cpu::enable_interrupts();
}

/// Mask interrupts on this CPU.
pub(crate) fn disable_interrupts() {
    cpu::disable_interrupts();
}

/// With interrupts masked, unmask them and wait for one, atomically: an
/// interrupt that arrived since they were masked wakes the wait instead of
/// being taken just before it. Returns with interrupts unmasked.
pub(crate) fn wait_for_work() {
    cpu::enable_interrupts_and_halt();
}

/// Whether this processor is taking interrupts right now.
pub(crate) fn interrupts_enabled() -> bool {
    // Bit 9 of RFLAGS is IF.
    cpu::read_rflags() & (1 << 9) != 0
}

pub(crate) use apic::{ipi_irq, send_ipi_to_others};
pub(crate) use msi::msi_allocate;

/// The page a device's MSI writes land in, which an IOMMU that translates them
/// must map into every domain. None here: VT-d leaves the local APIC's message
/// range untranslated.
pub(crate) fn msi_doorbell() -> Option<u64> {
    None
}

/// How `ferrix_sync`'s interrupt-masking lock masks interrupts here.
#[derive(Debug)]
pub(crate) struct Irq;

// SAFETY: (SHARED) `disable` masks interrupts on this CPU with `cli` and reports
// whether they were unmasked beforehand; `restore` unmasks only if they were,
// so nesting two critical sections leaves the inner one unable to unmask
// halfway out of the outer one. Neither touches any other state.
unsafe impl ferrix_sync::IrqControl for Irq {
    fn disable() -> usize {
        let was_enabled = cpu::read_rflags() & cpu::RFLAGS_INTERRUPT != 0;
        cpu::disable_interrupts();
        usize::from(was_enabled)
    }

    fn restore(state: usize) {
        if state != 0 {
            cpu::enable_interrupts();
        }
    }
}

/// Wait until an interrupt arrives.
pub(crate) fn wait_for_interrupt() {
    cpu::hlt();
}

/// The free-running counter.
pub(crate) fn counter_now() -> u64 {
    clock::counter_now()
}

/// How fast it counts.
pub(crate) fn counter_hz() -> u64 {
    clock::counter_hz()
}

/// Whether a program can read the counter [`counter_now`] reads: when it is
/// the TSC, which `rdtsc` reads in ring 3, and not the HPET. What a driver's
/// own timings, in its counter's ticks, need to mean something here.
pub(crate) fn ring3_reads_counter() -> bool {
    clock::counter_is_tsc()
}

/// Fire the timer interrupt once, `nanos` from now.
pub(crate) fn timer_arm(nanos: u64) {
    apic::arm(nanos);
}

/// Stop the timer.
pub(crate) fn timer_disarm() {
    apic::disarm();
}

/// Stop a one-shot that has just fired: nothing to do. The local APIC's
/// one-shot has counted down to zero and stays there, and every register
/// write would be an exit under a hypervisor.
pub(crate) fn timer_disarm_fired() {}

/// The interrupt number the timer arrives on.
pub(crate) fn timer_irq() -> u32 {
    apic::timer_irq()
}

/// Stop interrupt `number` being delivered until [`unmask_interrupt`] lets
/// it through again.
///
/// # Errors
///
/// Always, for now. A device's interrupts reach x86-64 as MSI-X, and an MSI-X
/// vector is masked in the device's own table entry, which the interrupt
/// controller does not reach; stage 10's `Vector::mask` will route there.
pub(crate) fn mask_interrupt(number: u32) -> Result<(), &'static str> {
    let _ = number;
    Err("x86-64 has no controller line to mask: its device interrupts are MSI-X")
}

/// Let interrupt `number` be delivered again after [`mask_interrupt`].
///
/// # Errors
///
/// Always, for now, for the reason [`mask_interrupt`] gives.
pub(crate) fn unmask_interrupt(number: u32) -> Result<(), &'static str> {
    let _ = number;
    Err("x86-64 has no controller line to mask: its device interrupts are MSI-X")
}

/// Dispatch the interrupt that arrived and retire it at the controller.
///
/// x86-64 puts the vector in the frame, so there is nothing to claim: the
/// work here is deciding what *not* to acknowledge. The spurious vector is
/// raised by the local APIC when an interrupt is withdrawn between being
/// signalled and being taken, and it is the one vector that must never be
/// given an end-of-interrupt — doing so retires a different interrupt that
/// was genuinely in service.
pub(crate) fn service_interrupts(frame: &mut TrapFrame, handle: fn(u32)) {
    if frame.vector == apic::SPURIOUS_VECTOR {
        return;
    }
    // Interrupt remapping's check vector: counted and acknowledged, never
    // dispatched (ruling 3).
    if frame.vector == u64::from(crate::iommu::CHECK_VECTOR) {
        crate::iommu::remapping::note_check_vector();
        apic::end_of_interrupt();
        return;
    }
    handle((frame.vector - trap::IRQ_BASE) as u32);
    apic::end_of_interrupt();
}

/// The context switch, and the stack layout a new task starts on.
pub(crate) use switch::{
    UserState, prepare_stack, reset_user_state, restore_user_state, save_user_state,
    set_thread_area, switch_to, thread_area,
};
pub(crate) use syscall::{UserRegs, resume_user};

/// Nothing to clean: every device a PC's kernel drives snoops the caches, so
/// what a processor wrote is what the device reads.
pub(crate) fn clean_for_device(start: u64, len: u64) {
    let _ = (start, len);
}

/// Nothing to flush, for the same reason as [`clean_for_device`].
pub(crate) fn flush_for_device(start: u64, len: u64) {
    let _ = (start, len);
}

/// Nothing to do: an x86 processor's instruction fetch is coherent with every
/// processor's stores, so code written is code fetched.
pub(crate) fn sync_instructions(start: u64, len: u64) {
    let _ = (start, len);
}

/// Processors whose PAT [`cpu::program_pat`] programmed and read back.
pub(crate) static PAT_PROGRAMMED: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// How many processors programmed their PAT with its write-combining entry.
pub(crate) fn write_combining_processors() -> Option<usize> {
    Some(PAT_PROGRAMMED.load(core::sync::atomic::Ordering::Acquire))
}

/// How the kernel maps a framebuffer: as a device. Write-combining needs the
/// PAT, which is not programmed, and these tables ignore `uncached`, which
/// would leave a framebuffer write-back.
pub(crate) const FRAMEBUFFER_FLAGS: ferrix_paging::MapFlags =
    ferrix_paging::MapFlags::KERNEL_DEVICE;

/// Entropy from firmware's TRNG: none, since a PC has no SMCCC. The CPU's
/// `RDSEED` is [`hardware_random`].
pub(crate) fn firmware_entropy(_view: &BootView<'_>, _out: &mut [u8]) -> usize {
    0
}

/// No board this architecture boots leaves a watchdog running.
pub(crate) const fn init_watchdogs(_tree: &ferrix_fdt::Fdt<'_>) {}

/// Nothing to feed: see [`init_watchdogs`].
pub(crate) const fn start_watchdogs() {}

// The vDSO's code and the program that checks it. Declared last, so that the
// lines above keep the numbers the coverage arguments cite them by.
mod vdso;
pub(crate) use vdso::{USER_VDSO_PROGRAM, vdso_can_read_counter, vdso_spec};

// Stage 13's scoped OOM kill check's program, last for the same reason.
/// A program that maps 8 MiB of anonymous memory and writes a byte to each
/// page of it: stage 13's scoped OOM kill check runs it in a cgroup whose
/// `memory.max` is far smaller, where it must be ended by `SIGKILL` part of
/// the way through. It exits with 0 if every page was written, and with 1
/// if the `mmap` was refused.
///
/// ```text
///   mmap(NULL, 0x800000, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) -> %rax
///   loop: movb $1, (%rax) ; add $4096, %rax ; until %rax is 0x800000 past the start
///   exit_group(0), or exit_group(1) for a refused mmap
/// ```
///
/// Assembled with binutils' `as` and read back out of the object file.
pub(crate) const USER_OOM_PROGRAM: &[u8] = &[
    0x31, 0xff, 0xbe, 0x00, 0x00, 0x80, 0x00, 0xba, 0x03, 0x00, 0x00, 0x00, 0x41, 0xba, 0x22, 0x00,
    0x00, 0x00, 0x49, 0xc7, 0xc0, 0xff, 0xff, 0xff, 0xff, 0x45, 0x31, 0xc9, 0xb8, 0x09, 0x00, 0x00,
    0x00, 0x0f, 0x05, 0x48, 0x3d, 0x00, 0xf0, 0xff, 0xff, 0x77, 0x19, 0x48, 0x8d, 0x88, 0x00, 0x00,
    0x80, 0x00, 0xc6, 0x00, 0x01, 0x48, 0x05, 0x00, 0x10, 0x00, 0x00, 0x48, 0x39, 0xc8, 0x72, 0xf2,
    0x31, 0xff, 0xeb, 0x05, 0xbf, 0x01, 0x00, 0x00, 0x00, 0xb8, 0xe7, 0x00, 0x00, 0x00, 0x0f, 0x05,
    0x0f, 0x0b,
];

// And init's check's program, last for the same reason.
/// A program that takes its bootstrap handle with `process_bootstrap`, closes
/// it with `handle_close`, and exits with what the close returned: 0 when it
/// was given a handle, `-EBADF & 0xFF` (247) when `process_bootstrap`
/// answered zero, which names nothing (`docs/INIT.md` §6, K3). A Linux
/// program making native calls by number, as a musl program's `syscall()`
/// makes them.
///
/// ```text
///   movl $0x1033, %eax ; syscall    ; process_bootstrap()
///   movq %rax, %rdi                 ; its answer, a handle or zero
///   movl $0x1000, %eax ; syscall    ; handle_close(it)
///   movl %eax, %edi
///   movl $231, %eax ; syscall       ; exit_group(what the close said)
/// ```
///
/// Assembled by the host's GNU assembler and read back with its
/// disassembler.
pub(crate) const USER_BOOTSTRAP_PROGRAM: &[u8] = &[
    0xb8, 0x33, 0x10, 0x00, 0x00, // movl $0x1033, %eax
    0x0f, 0x05, // syscall
    0x48, 0x89, 0xc7, // movq %rax, %rdi
    0xb8, 0x00, 0x10, 0x00, 0x00, // movl $0x1000, %eax
    0x0f, 0x05, // syscall
    0x89, 0xc7, // movl %eax, %edi
    0xb8, 0xe7, 0x00, 0x00, 0x00, // movl $231, %eax
    0x0f, 0x05, // syscall
];

/// A program that asks `getuid` who it runs as and exits with the answer, so
/// its exit status is the low byte of its real user id: 0 for root, 232 for
/// uid 1000. Made with `process_create` by a uid-1000 process in the check of
/// `docs/AUTH.md` §7's P0 (`fs/cgroupfs/creator_check.rs`), where a child
/// that became root would exit 0.
///
/// ```text
///   movl $102, %eax ; syscall      ; getuid()
///   movl %eax, %edi
///   movl $231, %eax ; syscall      ; exit_group(it)
/// ```
///
/// Assembled by the host's GNU assembler and read back with its
/// disassembler.
pub(crate) const USER_GETUID_PROGRAM: &[u8] = &[
    0xb8, 0x66, 0x00, 0x00, 0x00, // movl $102, %eax
    0x0f, 0x05, // syscall
    0x89, 0xc7, // movl %eax, %edi
    0xb8, 0xe7, 0x00, 0x00, 0x00, // movl $231, %eax
    0x0f, 0x05, // syscall
];
