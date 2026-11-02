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

/// Enable the CPU features the page tables the loader built depend on.
///
/// Both of these must happen **before** `CR3` is loaded. Bit 63 of a page table
/// entry is a *reserved* bit until `EFER.NXE` is set, and a reserved bit that is
/// set does not mean "no execute" — it means every access through that entry
/// takes a page fault. And both after `exit_boot_services` ([`prepare_cpu`]).
fn enable_paging_features() {
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
}

/// Nothing to check on x86-64 while firmware still runs, and nothing to
/// change: [`enable_paging_features`] waits until it has gone.
///
/// Setting `EFER.NXE` and `CR4.PGE` here, under firmware's page tables, hung
/// a real machine (an Intel N150's AMI firmware) inside `exit_boot_services`:
/// firmware's exit handlers run with whatever the loader left in the control
/// registers, and with `CR4.PGE` set a reload of `CR3` no longer flushes the
/// global entries firmware may have. OVMF never noticed.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the other architectures' check can fail"
)]
pub(crate) const fn prepare_cpu() -> Result<(), &'static str> {
    Ok(())
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

/// `IA32_MCG_CAP`: the bank count in bits 7:0.
const MCG_CAP: u32 = 0x179;
/// `IA32_MC0_STATUS`; bank `i`'s registers are four apart from here:
/// status, address, misc.
const MC0_STATUS: u32 = 0x401;
/// Banks read at most, as the kernel reads at most.
const MAX_BANKS: u32 = 32;

/// The pages uncorrected records left in the machine-check banks name
/// (`docs/MACHINE-CHECK-RESTART.md` §2.7), into `out`; how many. Reads
/// only, never writes or clears a bank -- the kernel's bring-up reads,
/// prints and clears them -- and only where CPUID says the processor has
/// the machine-check architecture, on a vendor whose rules it knows, and
/// only banks below `MCG_CAP`'s count.
pub(crate) fn left_bank_pages(out: &mut [u32]) -> usize {
    use ferrix_bootinfo::pstore::{BankRules, left_page};
    // CPUID 1, EDX: MCE (bit 7) and MCA (bit 14).
    let features = core::arch::x86_64::__cpuid(1).edx;
    if features & (1 << 7) == 0 || features & (1 << 14) == 0 {
        return 0;
    }
    let vendor = core::arch::x86_64::__cpuid(0);
    let mut name = [0_u8; 12];
    for (chunk, word) in name
        .chunks_exact_mut(4)
        .zip([vendor.ebx, vendor.edx, vendor.ecx])
    {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    let rules = match &name {
        b"GenuineIntel" => BankRules::Intel,
        b"AuthenticAMD" | b"HygonGenuine" => BankRules::Amd,
        _ => return 0,
    };
    // SAFETY: CPUID says the processor has the machine-check architecture,
    // which defines `IA32_MCG_CAP`; reading it changes nothing.
    let banks = (unsafe { read_msr(MCG_CAP) } & 0xff).min(u64::from(MAX_BANKS)) as u32;
    let mut found = 0;
    for bank in 0..banks {
        let at = MC0_STATUS + 4 * bank;
        // SAFETY: bank `bank` is below `MCG_CAP`'s count, so its status
        // register exists; reading it changes nothing.
        let status = unsafe { read_msr(at) };
        // `MCi_ADDR` and `MCi_MISC` are read only where the status says
        // they hold something, as the kernel reads them: an absent one may
        // fault.
        let address = if status & (1 << 58) != 0 {
            // SAFETY: as above, and the status names it valid.
            unsafe { read_msr(at + 1) }
        } else {
            0
        };
        let misc = if status & (1 << 59) != 0 {
            // SAFETY: as above, and the status names it valid.
            unsafe { read_msr(at + 2) }
        } else {
            0
        };
        if let (Some(page), Some(slot)) =
            (left_page(rules, status, address, misc), out.get_mut(found))
        {
            *slot = page;
            found += 1;
        }
    }
    found
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
/// Boot services must already have been exited, and `handoff` must describe tables in which the loader's own code is
/// still identity mapped — the instruction after `mov cr3` is fetched through
/// the new tables.
pub(crate) unsafe fn enter_kernel(handoff: Handoff) -> ! {
    // One four-level tree covers both halves of the address space here, so the
    // loader should have handed us the same table twice.
    debug_assert_eq!(
        handoff.identity_table, handoff.root_table,
        "x86-64 has a single root table for both halves"
    );
    enable_paging_features();

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
            "mov cr3, rcx",
            // From here the loader is running out of its identity mapping in
            // the kernel's tables.
            "mov rsp, rdx",
            // A null frame pointer terminates any backtrace the kernel walks,
            // rather than letting it wander into the loader's dead frames.
            "xor rbp, rbp",
            "jmp rsi",
            wp = const CR0_WP,
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
