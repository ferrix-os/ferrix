//! The x86-64 end of the loader.

use core::arch::asm;

use ferrix_bootinfo::Arch;

use super::Handoff;

/// The architecture the kernel is told it booted on.
pub(crate) const ARCH: Arch = Arch::X86_64;

/// The `e_machine` a kernel for this architecture must carry.
pub(crate) const ELF_MACHINE: u16 = ferrix_elf::EM_X86_64;

/// The ELF class a kernel for this architecture must be.
pub(crate) const ELF_CLASS: ferrix_elf::Class = ferrix_elf::Class::Elf64;

/// Extended Feature Enable Register.
const IA32_EFER: u32 = 0xC000_0080;
/// `EFER.NXE` — makes bit 63 of a page table entry mean "no execute".
const EFER_NXE: u64 = 1 << 11;
/// `CR4.PGE` — makes the global bit in a page table entry mean anything.
const CR4_PGE: u64 = 1 << 7;
/// `CR0.WP` — makes ring 0 obey a read-only page table entry. Clear, the
/// kernel can write its own text and rodata, and the W^X sweep, which reads
/// entries rather than trying a write, would not notice.
const CR0_WP: u64 = 1 << 16;
/// `CR4.LA57` — five-level paging. Firmware that runs with it has page tables
/// of five levels, and the loader's four-level tree loaded into `CR3` under it
/// would be walked one level off: the first fetch after the switch faults,
/// with no handler, and the machine resets. It cannot be cleared in long mode.
const CR4_LA57: u64 = 1 << 12;
/// `CR4.CET` — control-flow enforcement. Firmware that turned on indirect
/// branch tracking or shadow stacks for itself would have the kernel's first
/// indirect jump, which lands on no `endbr64`, raise #CP with no handler; the
/// kernel does not use CET, so the jump clears it.
const CR4_CET: u64 = 1 << 23;

/// Enable the CPU features the page tables the loader built depend on.
///
/// Both of these must happen **before** `CR3` is loaded. Bit 63 of a page table
/// entry is a *reserved* bit until `EFER.NXE` is set, and a reserved bit that is
/// set does not mean "no execute" — it means every access through that entry
/// takes a page fault.
pub(crate) fn prepare_cpu() -> Result<(), &'static str> {
    if control_registers().is_some_and(|(_, cr4, _)| cr4 & CR4_LA57 != 0) {
        return Err(
            "firmware runs five-level paging (CR4.LA57), and the kernel's tables have four",
        );
    }
    // SAFETY: reading IA32_EFER is architecturally defined on every 64-bit x86
    // and has no side effects.
    let efer = unsafe { read_msr(IA32_EFER) };
    // SAFETY: setting NXE only changes the meaning of a bit we are about to
    // start using, and the tables that use it are not installed yet.
    unsafe { write_msr(IA32_EFER, efer | EFER_NXE) };

    let cr4: u64;
    // SAFETY: reading CR4 has no side effects.
    unsafe {
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
    }
    // SAFETY: enabling global pages changes nothing until an entry with the
    // global bit is installed, which happens after this returns.
    unsafe {
        asm!("mov cr4, {}", in(reg) cr4 | CR4_PGE, options(nostack, preserves_flags));
    }

    Ok(())
}

/// `CR0`, `CR4` and `EFER` as firmware and [`prepare_cpu`] left them, for the
/// line the loader prints when breadcrumbs are on: what a bare-metal boot that
/// stops after the jump most needs to be compared with QEMU's.
pub(crate) fn control_registers() -> Option<(u64, u64, u64)> {
    let (cr0, cr4): (u64, u64);
    // SAFETY: reading CR0 and CR4 has no side effects.
    unsafe {
        asm!(
            "mov {}, cr0",
            "mov {}, cr4",
            out(reg) cr0,
            out(reg) cr4,
            options(nomem, nostack, preserves_flags),
        );
    }
    // SAFETY: IA32_EFER exists on every 64-bit x86; reading it has no side
    // effects.
    let efer = unsafe { read_msr(IA32_EFER) };
    Some((cr0, cr4, efer))
}

/// Nothing to do: x86-64 caches are coherent with the page table walker, so a
/// table written through the cache is visible to it without being cleaned.
pub(crate) const fn clean_dcache(_start: u64, _len: u64) {}

/// A random word from the processor's `RDRAND`, where `CPUID` says it has
/// one: the loader's second choice for KASLR, after firmware's
/// `EFI_RNG_PROTOCOL`.
///
/// Tried ten times, which is what Intel's guidance allows for the transient
/// underflow a busy generator reports.
pub(crate) fn cpu_random() -> Option<u64> {
    // CPUID.01H:ECX.RDRAND is bit 30.
    if core::arch::x86_64::__cpuid(1).ecx & (1 << 30) == 0 {
        return None;
    }
    (0..10).find_map(|_| {
        // SAFETY: CPUID says this processor implements RDRAND.
        unsafe { rdrand() }
    })
}

/// One `RDRAND`, or `None` if the generator had nothing to give.
#[target_feature(enable = "rdrand")]
fn rdrand() -> Option<u64> {
    let mut word = 0;
    (core::arch::x86_64::_rdrand64_step(&mut word) == 1).then_some(word)
}

/// The time-stamp counter: the loader's last resort for KASLR, and a poor
/// one, since how long firmware took to get here can be guessed.
pub(crate) fn counter() -> u64 {
    // SAFETY: RDTSC exists on every 64-bit x86, and reading the counter has
    // no side effects.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Read a model-specific register.
///
/// # Safety
///
/// `msr` must be a register this CPU implements; reading one it does not raises
/// a general protection fault.
unsafe fn read_msr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: the caller guarantees the register exists.
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// Write a model-specific register.
///
/// # Safety
///
/// `msr` must exist, and `value` must be one it accepts: an invalid value
/// raises a general protection fault, and a valid but wrong one changes how the
/// CPU interprets memory.
unsafe fn write_msr(msr: u32, value: u64) {
    // SAFETY: the caller guarantees the register and the value.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// The switch's instructions, for copying to a trampoline page: none here,
/// because a 64-bit layout never plans one.
pub(crate) const fn switch_code() -> Option<&'static [u8]> {
    None
}

/// Install the loader's page tables and jump to the kernel.
///
/// # Safety
///
/// Boot services must already have been exited, [`prepare_cpu`] must have been
/// called, and `handoff` must describe tables in which the loader's own code is
/// still identity mapped — the instruction after `mov cr3` is fetched through
/// the new tables.
pub(crate) unsafe fn enter_kernel(handoff: Handoff) -> ! {
    // One four-level tree covers both halves of the address space here, so the
    // loader should have handed us the same table twice.
    debug_assert_eq!(
        handoff.identity_table, handoff.root_table,
        "x86-64 has a single root table for both halves"
    );

    // Every operand is bound to a named register, for AArch64's reason:
    // `options(noreturn)` forbids outputs, so the only way to have a scratch
    // register the compiler agrees is free is to claim one as an input.
    //
    // SAFETY: the caller's contract is exactly the set of conditions that make
    // this sequence sound. `cli` first, because firmware's interrupt handlers
    // stopped existing at `exit_boot_services` and its timer has not.
    unsafe {
        asm!(
            "cli",
            // Write protection before the tables that depend on it, and here
            // rather than in `prepare_cpu`: firmware is gone, so nothing that
            // expected to write through its own read-only entries runs again.
            // Nothing between here and the jump writes memory at all.
            "mov rax, cr0",
            "or rax, {wp}",
            "mov cr0, rax",
            // Control-flow enforcement off, if firmware left it on: the jump
            // below lands on no `endbr64`, and the kernel keeps no shadow
            // stack. A no-op on every machine whose firmware did not.
            "mov rax, cr4",
            "btr rax, {cet_bit}",
            "mov cr4, rax",
            "mov cr3, rcx",
            // From here the loader is running out of its identity mapping in
            // the kernel's tables.
            "mov rsp, rdx",
            // A null frame pointer terminates any backtrace the kernel walks,
            // rather than letting it wander into the loader's dead frames.
            "xor rbp, rbp",
            "jmp rsi",
            wp = const CR0_WP,
            cet_bit = const CR4_CET.trailing_zeros(),
            in("rcx") handoff.root_table,
            in("rdx") handoff.stack_top,
            in("rsi") handoff.entry,
            // The System V argument register: the kernel entry takes the boot
            // info pointer as its only argument.
            in("rdi") handoff.boot_info,
            // Claimed so the scratch above cannot collide with anything live.
            in("rax") 0u64,
            options(noreturn),
        );
    }
}
