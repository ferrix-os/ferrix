//! Single `AArch64` instructions with no spelling in Rust.
//!
//! On the assembly allow-list as "CPU primitives" (`docs/ASSEMBLY.md`): a
//! barrier, a TLB maintenance operation and a hypervisor call are not things
//! Rust has syntax for.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

/// Wait for an interrupt.
pub(crate) fn wfi() {
    // SAFETY: (SYSREG) `wfi` is a hint. With interrupts masked it may still return at
    // any time, which is why every caller loops.
    unsafe {
        asm!("wfi", options(nomem, nostack, preserves_flags));
    }
}

/// Wait for an interrupt with `IRQ` masked, then unmask it.
///
/// `wfi` wakes on a *pending* interrupt whether or not it is masked, so one
/// that became pending after the caller masked is not lost: the wait returns
/// at once, and the unmask after it lets the interrupt be taken.
pub(crate) fn wait_then_enable_interrupts() {
    // SAFETY: (SYSREG) `wfi` is a hint and `daifclr` only clears the `IRQ` mask bit;
    // the vector table is installed long before this is used.
    unsafe {
        asm!("wfi", "msr daifclr, #0x2", options(nomem, nostack));
    }
}

/// Order every store before this against the next write to device memory.
///
/// For sending a software-generated interrupt: the interrupt is a write to the
/// distributor, and the core that takes it must see what this core wrote
/// before asking — a `dmb` orders memory against memory, and this is memory
/// against a device.
pub(crate) fn dsb_ishst() {
    // SAFETY: (SYSREG) a barrier has no effect beyond ordering.
    unsafe {
        asm!("dsb ishst", options(nostack, preserves_flags));
    }
}

/// Order this processor's accesses to memory a device reads or writes by
/// DMA: every access before it against every access after it, as the device
/// observes them, including a following write to the device's own registers.
///
/// `dmb osh`: the outer-shareable domain is where a DMA master sits, and a
/// device is not a processor of the inner one, so `dmb ish` -- what
/// `fence(SeqCst)` compiles to -- does not order what it sees. Both
/// directions, because a virtqueue needs both: descriptors before the index
/// that publishes them, and an index read before the entry it counts. Linux's
/// `dma_wmb` and `dma_rmb` are this barrier's two halves (`dmb oshst`,
/// `dmb oshld`), and its `__iowmb` puts one before a register write for the
/// same reason as the doorbell after a publish. F-44.
pub(crate) fn dma_barrier() {
    // SAFETY: (SYSREG) a barrier has no effect beyond ordering.
    unsafe {
        asm!("dmb osh", options(nostack, preserves_flags));
    }
}

/// Mask every interrupt on this CPU: debug, `SError`, `IRQ` and `FIQ`.
pub(crate) fn disable_interrupts() {
    // SAFETY: (SYSREG) `daifset` only sets mask bits in `PSTATE`.
    unsafe {
        asm!("msr daifset, #0xf", options(nomem, nostack));
    }
}

/// Publish page table writes and invalidate the whole TLB.
///
/// The first barrier is the one that matters and the one that is easy to leave
/// out: the table walker is a separate observer of memory, and a descriptor
/// still sitting in a store buffer is a descriptor it cannot see. Without
/// `dsb ishst` the mapping is invalidated *before* it exists, and the fault
/// that follows points at the access rather than at the omission.
pub(crate) fn flush_tlb() {
    // SAFETY: (TRANSLATE) barriers and TLB maintenance have no effect other than ordering
    // and invalidation.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags),
        );
    }
}

/// Publish page table writes and invalidate the page holding `address` on
/// every core, for every `ASID`.
///
/// `TLBI VAAE1IS` takes the page number, bits 55:12 of the address, in its
/// low 44 bits. Every user translation carries `ASID` zero and every kernel
/// one is global, and `VAAE1IS` drops both kinds for that page, so this is
/// [`flush_tlb`] narrowed to one page and nothing else. The barriers are that
/// function's, for its reasons.
pub(crate) fn flush_tlb_page(address: u64) {
    // SAFETY: (TRANSLATE) barriers and TLB maintenance have no effect other than ordering
    // and invalidation.
    unsafe {
        asm!(
            "dsb ishst",
            "tlbi vaae1is, {page}",
            "dsb ish",
            "isb",
            page = in(reg) (address >> 12) & ((1 << 44) - 1),
            options(nostack, preserves_flags),
        );
    }
}

/// PSCI `SYSTEM_OFF`, in the 32-bit calling convention.
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;

/// Ask the platform to power the machine off.
///
/// Returns if there is no PSCI implementation to answer, which is why the
/// caller halts afterwards rather than assuming this diverges.
///
/// The call is made *after* the boot test's success marker has been printed, on
/// purpose: `hvc` on a machine with no EL2 raises an exception, and until
/// stage 3 installs a vector table there is nothing to take it. Ordering it
/// last means the worst case is an untidy shutdown rather than a lost result.
pub(crate) fn psci_system_off() {
    psci_system(PSCI_SYSTEM_OFF);
}

/// PSCI `SYSTEM_RESET`, in the 32-bit calling convention.
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;

/// Ask the platform to reset the machine.
///
/// Returns if firmware does not implement `SYSTEM_RESET`, which is why the
/// caller powers off afterwards. Made late for the reason
/// [`psci_system_off`] gives.
///
/// Through `hvc` only, as `SYSTEM_OFF` is. A board whose firmware is reached
/// by `smc` takes an undefined-instruction exception here rather than an
/// error, and reset is now the path a board run ends on, so a conduit read
/// from the tables — as `smp` already does for `CPU_ON` — is owed before one.
pub(crate) fn psci_system_reset() {
    psci_system(PSCI_SYSTEM_RESET);
}

/// One of PSCI's whole-system calls, which take no arguments and either do not
/// return or return an error in `x0`.
fn psci_system(function: u64) {
    // SAFETY: (FIRMWARE) `hvc` with x0 = SYSTEM_OFF or SYSTEM_RESET either does what it
    // names or returns an error in x0. Both are fine; the callers carry on
    // either way.
    unsafe {
        asm!(
            "hvc #0",
            in("x0") function,
            lateout("x0") _,
            lateout("x1") _,
            lateout("x2") _,
            lateout("x3") _,
            options(nostack),
        );
    }
}

/// Install the exception vector table.
///
/// # Safety
///
/// (ENTRY) `table` must be the address of a sixteen-entry vector table aligned to 2048
/// bytes, every entry of which is real code. Until this is called the CPU is
/// still using firmware's table, which stopped existing at
/// `exit_boot_services` — so the window between the hand-off and this call is
/// one where any fault is unrecoverable.
pub(crate) unsafe fn write_vbar(table: u64) {
    // SAFETY: (ENTRY) the caller guarantees the table's contents and alignment. The
    // `isb` is what makes the write take effect before the next instruction is
    // fetched.
    unsafe {
        asm!(
            "msr vbar_el1, {}",
            "isb",
            in(reg) table,
            options(nostack, preserves_flags),
        );
    }
}

/// Unmask `IRQ` on this CPU, leaving `FIQ`, `SError` and debug masked.
///
/// Only `IRQ`: Ferrix routes nothing to `FIQ` — it is conventionally reserved
/// for a secure world the kernel does not own — and an `SError` is an
/// asynchronous abort that stays masked until there is something that could
/// act on one.
pub(crate) fn enable_interrupts() {
    // SAFETY: (SYSREG) `daifclr` only clears mask bits in `PSTATE`. The vector table is
    // installed long before anything calls this.
    unsafe {
        asm!("msr daifclr, #0x2", options(nomem, nostack));
    }
}

/// The interrupt mask bits of `PSTATE`.
///
/// Read for the same reason x86-64 reads `RFLAGS`: a lock that masks
/// interrupts has to put back the state it found, not unconditionally unmask.
pub(crate) fn read_daif() -> u64 {
    let daif: u64;
    // SAFETY: (SYSREG) reading `DAIF` has no side effects.
    unsafe {
        asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack, preserves_flags));
    }
    daif
}

/// Restore the interrupt mask bits `read_daif` returned.
///
/// # Safety
///
/// (SYSREG) `daif` must be a value a previous [`read_daif`] returned. Writing an
/// arbitrary value unmasks exceptions the caller may not be ready for — an
/// `SError` in particular, which Ferrix keeps masked until there is something
/// that could act on one.
pub(crate) unsafe fn write_daif(daif: u64) {
    // SAFETY: (SYSREG) the caller guarantees the value came from `read_daif`.
    unsafe {
        asm!("msr daif, {}", in(reg) daif, options(nomem, nostack));
    }
}

/// `TCR_EL1.EPD0` — translations through `TTBR0_EL1` fault instead of walking.
pub(crate) const TCR_EPD0: u64 = 1 << 7;

/// Memory attribute indirection: what each attribute index in a descriptor
/// means.
pub(crate) fn read_mair() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading `MAIR_EL1` has no side effects.
    unsafe {
        asm!("mrs {}, mair_el1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Translation control: sizes, granules and walk attributes of both halves.
pub(crate) fn read_tcr() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading `TCR_EL1` has no side effects.
    unsafe {
        asm!("mrs {}, tcr_el1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// The lower half's translation table base: the root address in bits 47 to 1,
/// the `ASID` above it.
pub(crate) fn read_ttbr0() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading `TTBR0_EL1` has no side effects.
    unsafe {
        asm!("mrs {}, ttbr0_el1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// The table address bits of a `TTBR0_EL1` value.
pub(crate) const TTBR_ADDRESS: u64 = 0x0000_FFFF_FFFF_FFFE;

/// System control: the MMU, the caches, alignment checking.
pub(crate) fn read_sctlr() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading `SCTLR_EL1` has no side effects.
    unsafe {
        asm!("mrs {}, sctlr_el1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

// `MSR PAN, #imm` is emitted as a word rather than written as `msr pan, #1`,
// for two reasons found the hard way. The mnemonic needs the ARMv8.1 `pan`
// extension this target does not enable; and `.arch_extension pan` inside an
// `asm!` changes assembler state for the rest of the translation unit, which
// broke section emission and failed the link on anonymous constants. A `const`
// operand does the same, so the word is written out literally.
//
// `MSR (immediate)` is `0xd500401f | op1 << 16 | CRm << 8 | op2 << 5`, and PAN
// is `op1 = 0`, `op2 = 4`, with `CRm` carrying the immediate. So `#1` is
// `0xd500419f` and `#0` is `0xd500409f` -- the same two words Linux emits for
// `SET_PSTATE_PAN`.

/// `SCTLR_EL1.SPAN`: set, an exception entry leaves `PSTATE.PAN` alone.
const SCTLR_SPAN: u64 = 1 << 23;
/// `SCTLR_EL1.UCT`: user mode may read `CTR_EL0`.
const SCTLR_UCT: u64 = 1 << 15;
/// `SCTLR_EL1.UCI`: user mode may run `DC CVAU`, `DC CIVAC`, `DC CVAC` and
/// `IC IVAU`.
const SCTLR_UCI: u64 = 1 << 26;

/// Whether this processor has `PAN`, so that the two words above are legal.
static PAN_ON: AtomicBool = AtomicBool::new(false);

/// Turn on Privileged Access Never: EL1 may not touch a page EL0 can.
///
/// AArch64's answer to SMAP, and the same argument applies — `syscall::uaccess`
/// reaches a program's memory through the direct map, never through a user
/// linear address, so the ordinary path is invisible to PAN and needs no
/// window. See `arch::x86_64::cpu::enable_user_access_protection`, which this
/// mirrors, and finding F-32.
///
/// Two registers, because PAN is both a state bit and a policy:
///
/// * `PSTATE.PAN` is set now, which is what forbids the access.
/// * `SCTLR_EL1.SPAN` is *cleared*, which is what keeps it forbidden. SPAN
///   means "**S**et **PAN** on exception entry is disabled"; leaving it set
///   would clear `PSTATE.PAN` on every trap from user mode, so the protection
///   would be off for exactly the code that handles a program's system calls.
///   Clearing SPAN is the whole point and is easy to miss.
///
/// The same write lets user mode maintain its own caches, as Linux does:
/// `SCTLR_EL1.UCT` lets it read `CTR_EL0`, the cache line sizes, and `UCI`
/// lets it run `DC CVAU` and `IC IVAU` on its own pages. A JIT needs both --
/// V8 writes code, cleans it to the point of unification and invalidates the
/// instruction cache over it, reading the line size first -- and without
/// them each traps as an undefined instruction, so Chromium on AArch64 would
/// die at its first compiled function. The operations only ever act on an
/// address the program can reach, and change no mapping. A secondary copies
/// the boot processor's `SCTLR_EL1` (`smp`), so every processor has them.
///
/// Returns whether the processor had PAN.
pub(crate) fn enable_user_access_protection() -> bool {
    let mmfr1: u64;
    // SAFETY: (SYSREG) reading `ID_AA64MMFR1_EL1` has no side effects.
    unsafe {
        asm!("mrs {}, id_aa64mmfr1_el1", out(reg) mmfr1, options(nomem, nostack, preserves_flags));
    }
    // PAN is bits 23:20; zero means the feature is absent.
    let pan = (mmfr1 >> 20) & 0xF != 0;

    let user_caches = read_sctlr() | SCTLR_UCT | SCTLR_UCI;
    let sctlr = if pan {
        user_caches & !SCTLR_SPAN
    } else {
        user_caches
    };
    // SAFETY: (PROTECT) clearing `SPAN` only changes whether an exception entry sets
    // `PSTATE.PAN`, and `UCT` and `UCI` only whether user mode may read the
    // cache type and maintain its own lines; none alters a mapping or a
    // cache or MMU setting of the kernel's.
    unsafe {
        asm!("msr sctlr_el1, {}", "isb", in(reg) sctlr, options(nostack, preserves_flags));
    }
    if !pan {
        return false;
    }
    // SAFETY: (PROTECT) `PAN` was just reported present by `ID_AA64MMFR1_EL1`, and
    // setting it only forbids EL1 access to EL0-accessible pages -- which
    // nothing on the ordinary path does.
    unsafe {
        asm!(".inst 0xd500419f", options(nomem, nostack, preserves_flags));
    }

    PAN_ON.store(true, Ordering::Relaxed);
    true
}

/// Permit this processor to touch user pages until [`forbid_user_access`].
///
/// Clears `PSTATE.PAN`. As on x86-64, almost nothing needs it: only
/// `crate::user::check`, which reaches a user address on purpose to prove the
/// processor walks an installed space.
pub(crate) fn permit_user_access() {
    if PAN_ON.load(Ordering::Relaxed) {
        // SAFETY: (PROTECT) `msr pan, #0` clears one state bit, and is only reached when
        // the feature was found, without which it would be undefined.
        unsafe {
            asm!(".inst 0xd500409f", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Refuse user pages to this processor again.
pub(crate) fn forbid_user_access() {
    if PAN_ON.load(Ordering::Relaxed) {
        // SAFETY: (PROTECT) `msr pan, #1` sets one state bit, under the same guard.
        unsafe {
            asm!(".inst 0xd500419f", options(nomem, nostack, preserves_flags));
        }
    }
}

/// The smallest data cache line in bytes, from `CTR_EL0`'s `DminLine`, which
/// is log2 of it in words.
fn data_line() -> u64 {
    let cache_type: u64;
    // SAFETY: (SYSREG) reading `CTR_EL0` has no side effects.
    unsafe {
        asm!("mrs {}, ctr_el0", out(reg) cache_type, options(nomem, nostack, preserves_flags));
    }
    4u64 << ((cache_type >> 16) & 0xF)
}

/// Write the data cache lines covering `start..start + len` back to the
/// point of coherency.
///
/// For memory a core with its caches off is about to read: it reads RAM, and
/// a line still dirty in this core's cache is a line it does not see.
pub(crate) fn clean_to_poc(start: u64, len: u64) {
    let line = data_line();
    let mut at = start - start % line;
    let end = start.saturating_add(len);
    while at < end {
        // SAFETY: (SYSREG) `dc cvac` writes one line back by virtual address and
        // changes no data; the caller's range is mapped.
        unsafe {
            asm!("dc cvac, {}", in(reg) at, options(nostack, preserves_flags));
        }
        at += line;
    }
    // SAFETY: (SYSREG) a barrier, completing the maintenance above before anything
    // after it — in particular the call that starts the core that reads it.
    unsafe {
        asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// Make the instructions in `start..start + len`, written through the data
/// side, the ones every core fetches from that memory.
///
/// For a page about to be mapped executable in user mode. Without it a core
/// can run what its instruction cache kept of whatever that frame held
/// before, or miss what is still only in a data cache: on the Pixel 7's eight
/// cores that was a program ending on an illegal instruction, and checks
/// failing one run in one place and the next run in another. QEMU models
/// neither cache, so it never showed there.
///
/// The data lines go to the point of coherency, by [`clean_to_poc`], which is
/// past the point of unification where fetches meet them and ends with the
/// barrier that completes it; then every instruction cache in the inner
/// shareable domain is emptied.
/// `IC IALLUIS` rather than `IC IVAU` by address: the Pixel's Cortex-A55s have
/// VIPT instruction caches, which a user address can index differently from
/// the direct map's. Neither step is skipped on `CTR_EL0`'s `IDC` or `DIC`,
/// because on a machine of three core types the one mapping the page is not
/// the one that runs it, and a page's worth of maintenance is cheap beside a
/// fault.
pub(crate) fn sync_instructions(start: u64, len: u64) {
    clean_to_poc(start, len);
    // SAFETY: (SYSREG) barriers and cache maintenance change no data. The second `dsb`
    // completes the invalidate on every core before the page can be mapped.
    unsafe {
        asm!(
            "ic ialluis",
            "dsb ish",
            "isb",
            options(nostack, preserves_flags)
        );
    }
}

/// Write the data cache lines covering `start..start + len` back to the
/// point of coherency and drop them.
///
/// For memory about to be shared with a device that does not snoop, through
/// a mapping that bypasses the caches: a dirty line left behind could be
/// evicted later, over whatever the device wrote since.
pub(crate) fn clean_invalidate_to_poc(start: u64, len: u64) {
    let line = data_line();
    let mut at = start - start % line;
    let end = start.saturating_add(len);
    while at < end {
        // SAFETY: (SYSREG) `dc civac` writes one line back and invalidates it by
        // virtual address; its data reaches memory first. The caller's range
        // is mapped.
        unsafe {
            asm!("dc civac, {}", in(reg) at, options(nostack, preserves_flags));
        }
        at += line;
    }
    // SAFETY: (SYSREG) a barrier, completing the maintenance before the memory is
    // handed over.
    unsafe {
        asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// A PSCI call through the hypervisor conduit.
///
/// # Safety
///
/// (FIRMWARE) `function` must be a PSCI function taking `a`, `b` and `c`, and what it
/// does must be what the caller intends: `CPU_ON` starts a core executing at
/// an address the caller chose.
pub(crate) unsafe fn hvc_call(function: u64, a: u64, b: u64, c: u64) -> u64 {
    // SAFETY: (FIRMWARE) the caller's guarantee, passed on.
    unsafe { hvc_call_x0_x3(function, a, b, c)[0] }
}

/// A PSCI call through the secure monitor conduit.
///
/// # Safety
///
/// (FIRMWARE) As [`hvc_call`].
pub(crate) unsafe fn smc_call(function: u64, a: u64, b: u64, c: u64) -> u64 {
    // SAFETY: (FIRMWARE) the caller's guarantee, passed on.
    unsafe { smc_call_x0_x3(function, a, b, c)[0] }
}

/// An SMCCC call through the hypervisor conduit, with all four result
/// registers: SMCCC 1.1 returns up to `x0`-`x3`, and `TRNG_RND64` puts its
/// bits in `x1`-`x3`.
///
/// # Safety
///
/// (FIRMWARE) As [`hvc_call`].
pub(crate) unsafe fn hvc_call_x0_x3(function: u64, a: u64, b: u64, c: u64) -> [u64; 4] {
    let mut result = [function, a, b, c];
    // SAFETY: (FIRMWARE) the caller guarantees the function and its arguments. The SMC
    // calling convention lets the callee corrupt every register the C ABI
    // does, which is what the clobber says.
    unsafe {
        asm!(
            "hvc #0",
            inlateout("x0") result[0],
            inlateout("x1") result[1],
            inlateout("x2") result[2],
            inlateout("x3") result[3],
            clobber_abi("C"),
            options(nostack),
        );
    }
    result
}

/// [`hvc_call_x0_x3`] through the secure monitor conduit.
///
/// # Safety
///
/// (FIRMWARE) As [`hvc_call`].
pub(crate) unsafe fn smc_call_x0_x3(function: u64, a: u64, b: u64, c: u64) -> [u64; 4] {
    let mut result = [function, a, b, c];
    // SAFETY: (FIRMWARE) as `hvc_call_x0_x3`.
    unsafe {
        asm!(
            "smc #0",
            inlateout("x0") result[0],
            inlateout("x1") result[1],
            inlateout("x2") result[2],
            inlateout("x3") result[3],
            clobber_abi("C"),
            options(nostack),
        );
    }
    result
}

/// Stop the CPU translating the lower half of the address space at all.
///
/// This is how `AArch64` drops the loader's identity map. Unlike x86-64, where
/// the identity map is a set of entries in the same table as everything else
/// and is dropped by clearing them, here it is a whole second translation
/// regime with its own base register — so the way to drop it is to switch the
/// regime off.
///
/// `TTBR0_EL1` is zeroed as well as disabled. With `EPD0` set the register is
/// not consulted, so this changes no behaviour; it is here so that a later
/// change that clears `EPD0` — stage 6, giving the lower half to a user
/// process — cannot accidentally resurrect the loader's tables.
///
/// # Safety
///
/// (TRANSLATE) Nothing may still be executing or reading through the lower half. The
/// kernel runs entirely in the upper half from its first instruction, so this
/// holds once the boot stack and the hand-off are being reached through the
/// direct map, which they are.
pub(crate) unsafe fn disable_ttbr0() {
    // SAFETY: (TRANSLATE) the caller guarantees nothing needs the lower half. The `isb`
    // makes both writes take effect before the next instruction is fetched;
    // without it the CPU is permitted to walk the old tables for a while yet.
    unsafe {
        asm!(
            "mrs {scratch}, tcr_el1",
            "orr {scratch}, {scratch}, {epd0}",
            "msr tcr_el1, {scratch}",
            "msr ttbr0_el1, xzr",
            "isb",
            scratch = out(reg) _,
            epd0 = in(reg) TCR_EPD0,
            options(nostack, preserves_flags),
        );
    }
}

/// Translate the lower half through the tables at `root`, with `ASID` zero.
///
/// The inverse of [`disable_ttbr0`], and the two writes are in the order that
/// order matters in: the root first and `EPD0` second. Between them the
/// processor is walking the new tables through a regime that is still
/// disabled, which faults; the other order leaves a window in which the regime
/// is live and the register still holds whatever was there before — which for
/// the first user process is the zero [`disable_ttbr0`] wrote, and for every
/// switch after it is *another process's tables*.
///
/// The `ASID` field of `TTBR0_EL1` is left zero, because stage 6 does not
/// allocate address space identifiers: every address space is `ASID` zero and
/// the switch invalidates all of them. See [`flush_user_tlb`].
///
/// # Safety
///
/// (TRANSLATE) `root` must be the physical address of a live translation table for the
/// lower half, and it must stay live until another root replaces it here.
pub(crate) unsafe fn write_ttbr0(root: u64) {
    // SAFETY: (TRANSLATE) the caller guarantees the tables. The `isb` makes both writes
    // take effect before the next instruction is fetched.
    unsafe {
        asm!(
            "msr ttbr0_el1, {root}",
            "mrs {scratch}, tcr_el1",
            "bic {scratch}, {scratch}, {epd0}",
            "msr tcr_el1, {scratch}",
            "isb",
            root = in(reg) root,
            scratch = out(reg) _,
            epd0 = in(reg) TCR_EPD0,
            options(nostack, preserves_flags),
        );
    }
}

/// Drop this processor's cached user translations, and keep the kernel's.
///
/// `TLBI ASIDE1` invalidates the entries matching an `ASID` and by definition
/// not the global ones, so the kernel's — its text, the direct map, the device
/// windows, all mapped global — survive. That is the whole reason this is not
/// [`flush_tlb`]: that one is `vmalle1is`, which throws away the global
/// entries too *and* broadcasts, and an address space switch neither needs nor
/// can afford either.
///
/// `nsh` rather than `ish` on the barrier: the invalidation is this
/// processor's business. Another processor running another thread of the same
/// process must keep its translations, and one that is about to run this
/// address space will invalidate as it installs the root.
pub(crate) fn flush_user_tlb() {
    // SAFETY: (TRANSLATE) invalidating translations can only cost a re-walk. The `dsb`
    // waits for the invalidation and the `isb` keeps the next instruction from
    // being fetched through an entry it removed.
    unsafe {
        asm!(
            "tlbi aside1, {asid}",
            "dsb nsh",
            "isb",
            asid = in(reg) 0_u64,
            options(nostack, preserves_flags),
        );
    }
}

/// This processor's multiprocessor affinity register.
///
/// Read-only and fixed at reset: it is how the processor is named to PSCI and
/// to the interrupt controller, and nothing the kernel does can change it.
pub(crate) fn read_mpidr() -> u64 {
    let mpidr: u64;
    // SAFETY: (SYSREG) reading `MPIDR_EL1` has no side effects.
    unsafe {
        asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack, preserves_flags));
    }
    mpidr
}

/// `ID_AA64PFR0_EL1`: whether the core has floating point and Advanced SIMD.
pub(crate) fn read_id_aa64pfr0() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading an identification register has no side effects.
    unsafe {
        asm!("mrs {}, id_aa64pfr0_el1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// `ID_AA64ISAR0_EL1`: the first instruction set attribute register.
pub(crate) fn read_id_aa64isar0() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading an identification register has no side effects.
    unsafe {
        asm!("mrs {}, id_aa64isar0_el1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// `ID_AA64ISAR1_EL1`: the second instruction set attribute register.
pub(crate) fn read_id_aa64isar1() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading an identification register has no side effects.
    unsafe {
        asm!("mrs {}, id_aa64isar1_el1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// `CPACR_EL1.FPEN`, both bits: floating point and SIMD do not trap at EL0 or
/// EL1.
const CPACR_FPEN_NO_TRAP: u64 = 0b11 << 20;

/// Let EL0 use floating point and SIMD, on this core.
///
/// musl's `memcpy` and every AArch64 Linux program's string handling use SIMD
/// registers, so without this a program's first copy traps. Firmware usually
/// leaves `FPEN` open, which is why busybox ran before this existed; the
/// architecture resets it to an unknown value, and a kernel that depends on
/// firmware for a register it could set itself fails on the first board whose
/// firmware does not.
///
/// Nothing is saved or restored: the kernel is built soft-float and one
/// program runs at a time, as ARMv7-A's version of this explains.
pub(crate) fn enable_user_fpu() {
    // SAFETY: (SYSREG) `FPEN` only controls trapping; the kernel does not rely on
    // floating point trapping, and the `isb` makes the change take effect
    // before the next instruction.
    unsafe {
        asm!(
            "mrs {scratch}, cpacr_el1",
            "orr {scratch}, {scratch}, #{fpen}",
            "msr cpacr_el1, {scratch}",
            "isb",
            scratch = out(reg) _,
            fpen = const CPACR_FPEN_NO_TRAP,
            options(nostack, preserves_flags),
        );
    }
}

/// Set `TPIDR_EL1`, the software thread ID register the kernel keeps its
/// per-CPU record in.
///
/// A scratch register with no architectural meaning, which is the point: the
/// hardware never reads it, and EL0 cannot see it.
pub(crate) fn write_tpidr_el1(value: u64) {
    // SAFETY: (SYSREG) writing a scratch register has no effect beyond the register.
    unsafe {
        asm!("msr tpidr_el1, {}", in(reg) value, options(nomem, nostack, preserves_flags));
    }
}

/// Read `TPIDR_EL1`.
pub(crate) fn read_tpidr_el1() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading a scratch register has no side effects.
    unsafe {
        asm!("mrs {}, tpidr_el1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// This function's frame pointer.
///
/// With `force-frame-pointers` on (see `.cargo/config.toml`) this register
/// holds the address of this function's frame record: the caller's frame
/// pointer, and the address it will return to. Walking that chain is how a
/// panic reports who called what.
#[inline(always)]
pub(crate) fn frame_pointer() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading a register has no side effects.
    unsafe {
        asm!("mov {}, x29", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// The frequency of the architected counter, in hertz.
///
/// Firmware programs this register at reset and it is read-only thereafter, so
/// a zero here means firmware did not do its job rather than that the counter
/// is stopped.
pub(crate) fn read_cntfrq() -> u64 {
    let frequency: u64;
    // SAFETY: (SYSREG) reading `CNTFRQ_EL0` has no side effects.
    unsafe {
        asm!("mrs {}, cntfrq_el0", out(reg) frequency, options(nomem, nostack, preserves_flags));
    }
    frequency
}

/// The virtual counter.
///
/// The `isb` is not optional: the counter read is otherwise free to be
/// satisfied out of order with respect to the instructions around it, and a
/// timing loop built on that measures something other than what it thinks.
pub(crate) fn read_cntvct() -> u64 {
    let count: u64;
    // SAFETY: (SYSREG) a barrier and a system register read, neither of which changes
    // any state.
    unsafe {
        asm!(
            "isb",
            "mrs {}, cntvct_el0",
            out(reg) count,
            options(nostack, preserves_flags),
        );
    }
    count
}

/// Let user mode read the virtual counter, and `CNTFRQ_EL0` with it:
/// `CNTKCTL_EL1.EL0VCTEN`, which Linux sets on every processor. Programs read
/// the counter directly for their clocks -- Chromium's `TimeTicks` is `mrs
/// x0, CNTVCT_EL0` -- and without it that read is an undefined instruction.
/// The physical counter and both timers' registers are closed to it
/// (`EL0PCTEN`, `EL0VTEN`, `EL0PTEN` clear), as Linux does; the event stream's
/// bits are left as found. A processor's own register: every processor runs this.
pub(crate) fn allow_user_counter() {
    let control: u64;
    // SAFETY: (SYSREG) reading `CNTKCTL_EL1` has no side effects.
    unsafe {
        asm!("mrs {}, cntkctl_el1", out(reg) control, options(nomem, nostack, preserves_flags));
    }
    // SAFETY: (SYSREG) these bits decide only what user mode may read or program of the
    // timer; no mapping, interrupt or timer of the kernel's changes.
    unsafe {
        asm!("msr cntkctl_el1, {}", "isb", in(reg) (control & !CNTKCTL_EL0_CLOSED) | CNTKCTL_EL0VCTEN, options(nostack, preserves_flags));
    }
}

/// `CNTKCTL_EL1.EL0VCTEN`: user mode may read `CNTVCT_EL0` and `CNTFRQ_EL0`.
const CNTKCTL_EL0VCTEN: u64 = 1 << 1;

/// What user mode is never given: `EL0PCTEN` (the physical counter),
/// `EL0VTEN` and `EL0PTEN` (the virtual and physical timers' registers).
const CNTKCTL_EL0_CLOSED: u64 = 1 | (1 << 8) | (1 << 9);

/// Set the instant the virtual timer fires at.
pub(crate) fn write_cntv_cval(instant: u64) {
    // SAFETY: (SYSREG) `CNTV_CVAL_EL0` is a comparator. Writing it changes when the
    // timer's output asserts and nothing else.
    unsafe {
        asm!("msr cntv_cval_el0, {}", in(reg) instant, options(nomem, nostack, preserves_flags));
    }
}

/// Enable or mask the virtual timer.
pub(crate) fn write_cntv_ctl(control: u64) {
    // SAFETY: (SYSREG) `CNTV_CTL_EL0` holds the timer's enable and mask bits. The `isb`
    // makes the write take effect before the next instruction, so a caller
    // that disarms and then returns cannot take one more interrupt.
    unsafe {
        asm!(
            "msr cntv_ctl_el0, {}",
            "isb",
            in(reg) control,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Turn on this core's GICv3 CPU interface, which is system registers rather
/// than memory, and return `ICC_SRE_EL1` as it reads afterwards.
///
/// In order: select the system register interface (`SRE`), let every priority
/// through, take the default `ICC_CTLR_EL1` -- one write to `ICC_EOIR1_EL1`
/// both drops the priority and deactivates -- and enable group 1, the group a
/// non-secure kernel's interrupts are in. A returned value with bit 0 clear
/// means `SRE` did not stick: EL2 or EL3 has not allowed EL1 the system
/// registers, and none of the others did anything.
pub(crate) fn enable_gicv3_cpu_interface() -> u64 {
    let sre: u64;
    // SAFETY: (SYSREG) each write configures this core's own view of the interrupt
    // controller, and interrupts stay masked in `DAIF` throughout, so nothing
    // is delivered halfway through.
    unsafe {
        asm!(
            "mrs {sre}, icc_sre_el1",
            "orr {sre}, {sre}, #1",
            "msr icc_sre_el1, {sre}",
            "isb",
            "msr icc_pmr_el1, {all}",
            "msr icc_ctlr_el1, xzr",
            "msr icc_igrpen1_el1, {one}",
            "mrs {sre}, icc_sre_el1",
            sre = out(reg) sre,
            all = in(reg) 0xFF_u64,
            one = in(reg) 1_u64,
            options(nostack, preserves_flags),
        );
    }
    sre
}

/// Acknowledge the highest priority pending group 1 interrupt:
/// `ICC_IAR1_EL1`, whose low 24 bits are its identifier.
pub(crate) fn read_icc_iar1() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) the read claims an interrupt, which is what the caller asked
    // for, and changes nothing else.
    unsafe {
        asm!("mrs {}, icc_iar1_el1", out(reg) value, options(nostack, preserves_flags));
    }
    value
}

/// Retire an interrupt [`read_icc_iar1`] returned, with exactly its value.
pub(crate) fn write_icc_eoir1(value: u64) {
    // SAFETY: (SYSREG) ends the interrupt the value names on this core, nothing else.
    unsafe {
        asm!("msr icc_eoir1_el1, {}", in(reg) value, options(nostack, preserves_flags));
    }
}

/// Send a group 1 software-generated interrupt: `ICC_SGI1R_EL1`.
///
/// The `isb` makes the write take effect now rather than whenever the core
/// next synchronises, which for an inter-processor interrupt is the whole
/// point.
pub(crate) fn write_icc_sgi1r(value: u64) {
    // SAFETY: (SYSREG) raises an interrupt on the cores the value names.
    unsafe {
        asm!("msr icc_sgi1r_el1, {}", "isb", in(reg) value, options(nostack, preserves_flags));
    }
}

/// A 64-bit random number from the CPU's `RNDR` register, tried ten times;
/// `None` on a core without it, which `ID_AA64ISAR0_EL1` says, or one that
/// kept reporting failure.
pub(crate) fn hardware_random() -> Option<u64> {
    let isar0: u64;
    // SAFETY: (SYSREG) reading an identification register at EL1 changes nothing.
    unsafe {
        core::arch::asm!(
            "mrs {}, id_aa64isar0_el1",
            out(reg) isar0,
            options(nomem, nostack, preserves_flags),
        );
    }
    if (isar0 >> 60) & 0xf == 0 {
        return None;
    }
    for _ in 0..10 {
        let value: u64;
        let good: u64;
        // SAFETY: (SYSREG) the identification register says `RNDR` exists. Reading it
        // sets the condition flags, clear Z meaning the value is good.
        unsafe {
            core::arch::asm!(
                "mrs {value}, s3_3_c2_c4_0",
                "cset {good}, ne",
                value = out(reg) value,
                good = out(reg) good,
                options(nomem, nostack),
            );
        }
        if good == 1 {
            return Some(value);
        }
    }
    None
}
