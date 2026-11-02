//! The ARMv7-A end of the loader.
//!
//! AArch64's sequence, in coprocessor 15's spelling. Firmware hands over in
//! SVC mode, usually with the MMU on and short-descriptor tables of its own,
//! and the architecture no more permits switching `TTBCR.EAE` under a live
//! translation regime than AArch64 permits reprogramming `TCR_EL1`. So the
//! loader cleans what it wrote out of the caches, turns the MMU off, installs
//! a long-descriptor regime — `MAIR0` and `MAIR1`, `TTBCR` with `EAE` set,
//! and the two 64-bit table base registers — and turns it back on. Firmware's
//! identity map keeps the program counter meaning the same thing throughout,
//! exactly as it does there.
//!
//! One thing has no AArch64 counterpart. The kernel half is translated
//! through `TTBR1` with `TTBCR.T1SZ = 1`, whose level-1 table is two entries
//! indexed by address bit 30, while `src/lib/kernel/paging` indexes the same root by
//! bits 31:30 and so puts `0x8000_0000` in entry 2. `TTBR1` therefore holds
//! the root's address plus sixteen — what Linux calls `TTBR1_OFFSET` — and a
//! test in `src/lib/kernel/paging` pins the half of that arithmetic that lives there.

use core::arch::{asm, global_asm};
use core::slice;

use ferrix_bootinfo::Arch;
use ferrix_elf::Class;
use ferrix_paging::armv7a::{MAIR0, MAIR1};

use super::Handoff;

/// The architecture the kernel is told it booted on.
pub(crate) const ARCH: Arch = Arch::Armv7a;

/// The `e_machine` a kernel for this architecture must carry.
pub(crate) const ELF_MACHINE: u16 = ferrix_elf::EM_ARM;

/// The ELF class a kernel for this architecture must be.
pub(crate) const ELF_CLASS: Class = Class::Elf32;

/// `CPSR.M`, the processor mode.
const MODE_MASK: u32 = 0x1F;
/// Supervisor mode: PL1, where the kernel runs.
const MODE_SVC: u32 = 0x13;
/// Hypervisor mode: PL2.
const MODE_HYP: u32 = 0x1A;

/// `ID_MMFR0.VMSA` values from here up have the long-descriptor format.
const VMSA_WITH_LPAE: u32 = 5;

/// `TTBCR` for the kernel's regime, assembled so each field can be named.
///
/// `EAE` selects the long-descriptor format. `T0SZ = 0` and `T1SZ = 1` split
/// the address space at 2 GiB: `TTBR1` translates the top half, `TTBR0`
/// everything below it. Both walks are write-back cacheable and inner
/// shareable, as the descriptors they read are.
const TTBCR: u32 = {
    const EAE: u32 = 1 << 31;
    const T0SZ: u32 = 0;
    const T1SZ: u32 = 1;
    const WRITE_BACK: u32 = 0b01;
    const INNER_SHAREABLE: u32 = 0b11;
    EAE | T0SZ
        | (WRITE_BACK << 8)
        | (WRITE_BACK << 10)
        | (INNER_SHAREABLE << 12)
        | (T1SZ << 16)
        | (WRITE_BACK << 24)
        | (WRITE_BACK << 26)
        | (INNER_SHAREABLE << 28)
};

/// Where the kernel half's level-1 table starts within the root: entry 2.
const TTBR1_OFFSET: u64 = 16;

/// `SCTLR.M`, the MMU enable.
const SCTLR_MMU: u32 = 1 << 0;
/// `SCTLR.C`, the data cache enable.
const SCTLR_DCACHE: u32 = 1 << 2;
/// `SCTLR.I`, the instruction cache enable.
const SCTLR_ICACHE: u32 = 1 << 12;
/// `SCTLR.WXN`: every writable page execute-never. Cleared, because the
/// identity map is both, and the instruction after the switch is fetched
/// through it; the kernel's own W^X sweep is what enforces the rule after.
const SCTLR_WXN: u32 = 1 << 19;

/// Check the loader is somewhere it can install a translation regime.
///
/// Two assumptions to verify, as AArch64 verifies its exception level: that
/// firmware handed over in SVC mode, since `enter_kernel` writes PL1 registers
/// and a kernel entered in HYP would take its first exception somewhere it
/// did not expect; and that the CPU has the Large Physical Address Extension,
/// since every page table the loader builds is in its format. A Cortex-A7 or
/// A15 has it; a Cortex-A9 does not, and learning that from a message beats
/// learning it from a fault with the MMU off.
pub(crate) fn prepare_cpu() -> Result<(), &'static str> {
    match current_mode() {
        MODE_SVC => {}
        MODE_HYP => return Err("firmware handed off in HYP mode; the loader only supports SVC"),
        _ => return Err("firmware handed off in an unexpected processor mode"),
    }
    if memory_model() < VMSA_WITH_LPAE {
        return Err("this CPU has no Large Physical Address Extension, which the page tables need");
    }
    Ok(())
}

/// Clean and invalidate the data cache over a physical range.
///
/// For the reason AArch64 does: the tables are written with the caches on and
/// read by a walker that starts with them off.
pub(crate) fn clean_dcache(start: u64, len: u64) {
    if len == 0 {
        return;
    }

    let cache_type: u32;
    // SAFETY: CTR is readable at PL1 and has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c0, c0, 1", out(reg) cache_type, options(nomem, nostack, preserves_flags));
    }
    // DminLine is log2 of the smallest data cache line in words.
    let line = 4u64 << ((cache_type >> 16) & 0xF);

    let mut at = start & !(line - 1);
    let end = start.saturating_add(len);
    while at < end {
        // SAFETY: `DCCIMVAC` cleans and invalidates one line by address, which
        // under firmware's identity map is this physical address. It has no
        // effect beyond the cache. Every address firmware allocated is below
        // 4 GiB, so the truncation is exact.
        unsafe {
            asm!("mcr p15, 0, {}, c7, c14, 1", in(reg) at as u32, options(nostack, preserves_flags));
        }
        at += line;
    }

    // SAFETY: barriers, ordering the maintenance above against what follows.
    unsafe {
        asm!("dsb", "isb", options(nostack, preserves_flags));
    }
}

/// The processor mode the loader is running in.
fn current_mode() -> u32 {
    let cpsr: u32;
    // SAFETY: reading CPSR has no side effects.
    unsafe {
        asm!("mrs {}, cpsr", out(reg) cpsr, options(nomem, nostack, preserves_flags));
    }
    cpsr & MODE_MASK
}

/// No random number instruction: ARMv7-A has none, so KASLR falls back from
/// firmware's `EFI_RNG_PROTOCOL` straight to [`counter`].
pub(crate) const fn cpu_random() -> Option<u64> {
    None
}

/// The virtual count of the generic timer, or zero on a core without one:
/// the loader's last resort for KASLR, and a poor one, since how long
/// firmware took to get here can be guessed.
pub(crate) fn counter() -> u64 {
    let features: u32;
    // SAFETY: ID_PFR1 is readable at PL1 and has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c0, c1, 1", out(reg) features, options(nomem, nostack, preserves_flags));
    }
    // ID_PFR1.GenTimer, bits 19:16: zero on a core without the generic timer,
    // where reading its count is an undefined instruction.
    if (features >> 16) & 0xF == 0 {
        return 0;
    }
    let (low, high): (u32, u32);
    // SAFETY: the core implements the generic timer, whose virtual count is
    // readable at PL1 and has no side effects.
    unsafe {
        asm!("mrrc p15, 1, {}, {}, c14", out(reg) low, out(reg) high, options(nomem, nostack, preserves_flags));
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// The virtual memory system this CPU implements, as `ID_MMFR0.VMSA`.
fn memory_model() -> u32 {
    let features: u32;
    // SAFETY: ID_MMFR0 is readable at PL1 and has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c0, c1, 4", out(reg) features, options(nomem, nostack, preserves_flags));
    }
    features & 0xF
}

// The switch itself, as a block of instructions the loader can run in place or
// copy to a page of its choosing and run there; see `switch_code`.
//
// Everything in it is register-only and position-independent: no branch leaves
// it except the last, no literal pool, no address loaded from memory. That is
// what lets a copy run anywhere, and `tools/common/xtask/src/pe.rs` refuses a loader whose
// block has a relocation inside it or does not fit one page.
//
// On entry: r0 the boot info, r1 and r2 MAIR0 and MAIR1, r3 TTBCR, r4 and r5
// the two table roots, r8 the stack top, r10 the kernel entry. r12 is scratch.
global_asm!(
    ".pushsection .text.ferrix_switch, \"ax\", %progbits",
    ".arm",
    ".balign 4",
    ".global ferrix_switch_start",
    ".global ferrix_switch_end",
    "ferrix_switch_start:",
    "cpsid aif",
    "dsb",
    "isb",
    // The MMU and both caches off. From here until the MMU is back on the
    // program counter is a physical address, which is fine only because
    // whatever page this runs from is identity mapped.
    "mrc p15, 0, r12, c1, c0, 0",
    "bic r12, r12, #{mmu}",
    "bic r12, r12, #{dcache}",
    "bic r12, r12, #{icache}",
    "mcr p15, 0, r12, c1, c0, 0",
    "isb",
    // The kernel's text was written as data. The data cache was cleaned by the
    // caller; the instruction cache and the branch predictor may still hold
    // what was there before, and are discarded.
    "mov r12, #0",
    "mcr p15, 0, r12, c7, c5, 0",
    "mcr p15, 0, r12, c7, c5, 6",
    // Our own translation regime: attributes, split, then the two roots as
    // 64-bit values whose upper words — and so ASIDs — are zero.
    "mcr p15, 0, r1, c10, c2, 0",
    "mcr p15, 0, r2, c10, c2, 1",
    "mcr p15, 0, r3, c2, c0, 2",
    "mcrr p15, 0, r4, r12, c2",
    "mcrr p15, 1, r5, r12, c2",
    "isb",
    "mcr p15, 0, r12, c8, c7, 0",
    "dsb",
    "isb",
    // And back on.
    "mrc p15, 0, r12, c1, c0, 0",
    "orr r12, r12, #{mmu}",
    "orr r12, r12, #{dcache}",
    "orr r12, r12, #{icache}",
    "bic r12, r12, #{wxn}",
    "mcr p15, 0, r12, c1, c0, 0",
    "isb",
    "mov sp, r8",
    // Null frame and link registers terminate any backtrace the kernel walks,
    // rather than letting it wander into the loader's frames.
    "mov r11, #0",
    "mov lr, #0",
    "bx r10",
    "ferrix_switch_end:",
    ".popsection",
    mmu = const SCTLR_MMU,
    dcache = const SCTLR_DCACHE,
    icache = const SCTLR_ICACHE,
    wxn = const SCTLR_WXN,
);

unsafe extern "C" {
    /// The first instruction of the switch.
    static ferrix_switch_start: u8;
    /// One past its last.
    static ferrix_switch_end: u8;
}

/// The switch's instructions, for copying to a trampoline page.
pub(crate) fn switch_code() -> Option<&'static [u8]> {
    let start = &raw const ferrix_switch_start;
    let end = &raw const ferrix_switch_end;
    // SAFETY: both symbols bound one block of this image's own text, which is
    // mapped and never written for the loader's whole life.
    Some(unsafe { slice::from_raw_parts(start, end.addr() - start.addr()) })
}

/// Install the loader's translation regime and jump to the kernel.
///
/// # Safety
///
/// Boot services must already have been exited; the loader must be running in
/// SVC mode; the page the switch runs from — the loader's own image, or the
/// trampoline in `handoff.switch` — must be identity mapped in the tables
/// installed, because the middle of the switch executes with the MMU off; and
/// everything `handoff` points at, the trampoline included, must already have
/// been cleaned out of the data cache with [`clean_dcache`].
pub(crate) unsafe fn enter_kernel(handoff: Handoff) -> ! {
    // Every address here is below 4 GiB by construction — the kernel's layout
    // is `LAYOUT_32`, and firmware allocated the tables — so a register holds
    // each exactly.
    let identity = handoff.identity_table as u32;
    let kernel_half = (handoff.root_table + TTBR1_OFFSET) as u32;
    let switch = handoff
        .switch
        .map_or((&raw const ferrix_switch_start).addr() as u32, |page| {
            page as u32
        });

    // As on AArch64, every operand is bound to a named register, because
    // `options(noreturn)` forbids outputs and so forbids asking for scratch.
    // `r6`, `r7`, `r9` and `r11` are the registers the compiler reserves on
    // this architecture, so none of them is an operand.
    //
    // Before the branch, the instruction cache and the branch predictor are
    // discarded: a trampoline page was just written as data, and the block's
    // own invalidation runs only after its first instructions have already
    // been fetched — through whatever the cache held for that page. This is
    // the sequence the Arm architecture requires after writing instructions,
    // and on the in-place path it is merely redundant. `lr` carries the value
    // the two writes ignore; nothing returns through it.
    //
    // SAFETY: the caller's contract is exactly the set of conditions that make
    // the switch sound. Interrupts and aborts are masked before the branch,
    // because firmware's handlers stopped existing at `exit_boot_services`.
    unsafe {
        asm!(
            "cpsid aif",
            "mov lr, #0",
            "mcr p15, 0, lr, c7, c5, 0",
            "mcr p15, 0, lr, c7, c5, 6",
            "dsb",
            "isb",
            "bx r12",
            // The AAPCS argument register: the kernel entry takes the boot
            // info pointer as its only argument.
            in("r0") handoff.boot_info as u32,
            in("r1") MAIR0,
            in("r2") MAIR1,
            in("r3") TTBCR,
            in("r4") identity,
            in("r5") kernel_half,
            in("r8") handoff.stack_top as u32,
            in("r10") handoff.entry as u32,
            in("r12") switch,
            options(noreturn),
        );
    }
}

/// No machine-check banks on ARMv7-A: no page is kept out for one
/// (`docs/MACHINE-CHECK-RESTART.md` §4).
pub(crate) const fn left_bank_pages(_out: &mut [u32]) -> usize {
    0
}
