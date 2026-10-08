//! Single ARMv7-A instructions with no spelling in Rust.
//!
//! On the assembly allow-list as "CPU primitives" (`docs/ASSEMBLY.md`), as
//! AArch64's are. Coprocessor 15 is how ARMv7-A spells every system register,
//! and a coprocessor transfer is not something Rust has syntax for; nor are a
//! barrier, a TLB operation, a mode change or a hypervisor call.

use core::arch::asm;

use ferrix_fdt::PsciConduit;

/// `CPSR.A`, `CPSR.I` and `CPSR.F`: asynchronous aborts, IRQs and FIQs masked.
const CPSR_MASK_BITS: u32 = (1 << 8) | (1 << 7) | (1 << 6);

/// `CPSR.I`: IRQs masked.
pub(crate) const CPSR_IRQ_MASKED: u32 = 1 << 7;

/// `SCTLR.V`: exceptions go to the fixed high vectors at `0xFFFF_0000` rather
/// than to `VBAR`.
const SCTLR_HIGH_VECTORS: u32 = 1 << 13;
/// `SCTLR.TE`: exceptions are taken in Thumb state.
const SCTLR_THUMB_EXCEPTIONS: u32 = 1 << 30;

/// `TTBCR.EPD0`: translations through `TTBR0` fault instead of walking.
pub(crate) const TTBCR_EPD0: u32 = 1 << 7;

/// PSCI `SYSTEM_OFF`, in the 32-bit calling convention.
const PSCI_SYSTEM_OFF: u32 = 0x8400_0008;
/// PSCI `SYSTEM_RESET`, in the 32-bit calling convention.
const PSCI_SYSTEM_RESET: u32 = 0x8400_0009;

/// Wait for an interrupt.
pub(crate) fn wfi() {
    // SAFETY: (SYSREG) `wfi` is a hint. With interrupts masked it may still return at
    // any time, which is why every caller loops.
    unsafe {
        asm!("wfi", options(nomem, nostack, preserves_flags));
    }
}

/// Mask asynchronous aborts, IRQs and FIQs on this CPU.
pub(crate) fn disable_interrupts() {
    // SAFETY: (SYSREG) `cpsid` only sets mask bits in CPSR.
    unsafe {
        asm!("cpsid aif", options(nomem, nostack));
    }
}

/// Unmask IRQs on this CPU, leaving FIQs and asynchronous aborts masked.
///
/// Only IRQs, for AArch64's reasons: nothing is routed to FIQ, which is
/// conventionally a secure world's, and an asynchronous abort stays masked
/// until there is something that could act on one.
pub(crate) fn enable_interrupts() {
    // SAFETY: (SYSREG) `cpsie` only clears mask bits in CPSR. The vector table is
    // installed long before anything calls this.
    unsafe {
        asm!("cpsie i", options(nomem, nostack));
    }
}

/// The current program status.
pub(crate) fn read_cpsr() -> u32 {
    let cpsr: u32;
    // SAFETY: (SYSREG) reading CPSR has no side effects.
    unsafe {
        asm!("mrs {}, cpsr", out(reg) cpsr, options(nomem, nostack, preserves_flags));
    }
    cpsr
}

/// Put back the interrupt mask bits a [`read_cpsr`] saw.
///
/// Only the A, I and F bits change: the rest of the value written is the
/// current CPSR's own, so the mode and the endianness cannot be disturbed by a
/// caller that passed back something stale.
///
/// # Safety
///
/// (SYSREG) `saved` must have come from [`read_cpsr`] on this CPU. Unmasking what the
/// caller did not mask unmasks exceptions it may not be ready for.
pub(crate) unsafe fn restore_interrupt_mask(saved: u32) {
    // SAFETY: (SYSREG) the caller guarantees the bits came from this CPU's own CPSR.
    unsafe {
        asm!(
            "mrs {scratch}, cpsr",
            "bic {scratch}, {scratch}, #{mask}",
            "orr {scratch}, {scratch}, {saved}",
            "msr cpsr_xc, {scratch}",
            scratch = out(reg) _,
            saved = in(reg) saved & CPSR_MASK_BITS,
            mask = const CPSR_MASK_BITS,
            options(nomem, nostack),
        );
    }
}

/// Publish page table writes and invalidate the whole TLB, on every core.
///
/// The first barrier is the one that is easy to leave out, for the reason it
/// is on AArch64: the table walker is a separate observer of memory, and a
/// descriptor still in a store buffer is one it cannot see. The branch
/// predictor goes too, because the 32-bit architecture lets it hold virtual
/// addresses whose translation just changed.
pub(crate) fn flush_tlb() {
    // SAFETY: (TRANSLATE) barriers and maintenance operations have no effect other than
    // ordering and invalidation.
    unsafe {
        asm!(
            "dsb ishst",
            "mcr p15, 0, {zero}, c8, c3, 0",
            "mcr p15, 0, {zero}, c7, c1, 6",
            "dsb ish",
            "isb",
            zero = in(reg) 0u32,
            options(nostack, preserves_flags),
        );
    }
}

/// Publish page table writes and invalidate the page at `page` on every
/// core, for every `ASID`: `TLBIMVAAIS`, and `BPIALLIS` for the reason
/// [`flush_tlb`] gives.
///
/// `page` is the page's address; the low twelve bits are ignored.
pub(crate) fn flush_tlb_page(page: u32) {
    // SAFETY: (TRANSLATE) barriers and maintenance operations have no effect other than
    // ordering and invalidation.
    unsafe {
        asm!(
            "dsb ishst",
            "mcr p15, 0, {page}, c8, c3, 3",
            "mcr p15, 0, {zero}, c7, c1, 6",
            "dsb ish",
            "isb",
            page = in(reg) page & !0xFFF,
            zero = in(reg) 0u32,
            options(nostack, preserves_flags),
        );
    }
}

/// Ask the platform to power the machine off, through whichever instruction
/// the device tree says its PSCI firmware answers.
///
/// Returns if nothing answers, which is why the caller halts afterwards. As on
/// AArch64 it is made only after the success marker, because the wrong conduit
/// is an undefined-instruction exception rather than an error code.
pub(crate) fn psci_system_off(conduit: PsciConduit) {
    psci_system(conduit, PSCI_SYSTEM_OFF);
}

/// Ask the platform to reset the machine, through the same conduit as
/// [`psci_system_off`].
///
/// Returns if firmware does not implement `SYSTEM_RESET`, which is why the
/// caller powers off afterwards.
pub(crate) fn psci_system_reset(conduit: PsciConduit) {
    psci_system(conduit, PSCI_SYSTEM_RESET);
}

/// One of PSCI's whole-system calls, which take no arguments and either do not
/// return or return an error in `r0`.
fn psci_system(conduit: PsciConduit, function: u32) {
    match conduit {
        // SAFETY: (FIRMWARE) `hvc` with r0 = SYSTEM_OFF or SYSTEM_RESET either does what it
        // names or returns an error in r0; the callers carry on either way.
        PsciConduit::Hvc => unsafe {
            asm!(
                ".arch_extension virt",
                "hvc #0",
                inout("r0") function => _,
                lateout("r1") _,
                lateout("r2") _,
                lateout("r3") _,
                options(nostack),
            );
        },
        // SAFETY: (FIRMWARE) as above, through the secure monitor.
        PsciConduit::Smc => unsafe {
            asm!(
                ".arch_extension sec",
                "smc #0",
                inout("r0") function => _,
                lateout("r1") _,
                lateout("r2") _,
                lateout("r3") _,
                options(nostack),
            );
        },
    }
}

/// Set `TPIDRURO`, the thread ID register a USR-mode program can read and
/// cannot write.
///
/// It is where a program's thread pointer lives. musl built for ARMv7-A reads
/// it with `mrc p15, 0, rX, c13, c0, 3` on every access to a thread-local
/// variable, and asks for it to be set through the ARM-private `set_tls` call
/// -- because being read-only to USR mode is the point, the kernel is the only
/// thing that can put the value there.
pub(crate) fn write_tpidruro(value: u32) {
    // SAFETY: (CONTEXT) a software register the kernel keeps nothing of its own in, so
    // writing it changes no state the kernel depends on.
    unsafe {
        asm!(
            "mcr p15, 0, {}, c13, c0, 3",
            in(reg) value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// `CPACR` bits 20 to 23: full access to coprocessors 10 and 11, which are the
/// FPU's.
const CPACR_CP10_CP11_FULL: u32 = 0xF << 20;

/// Let USR mode use the FPU, on this core.
///
/// A hard-float program -- which is what an ARMv7-A Linux distribution builds
/// -- saves VFP registers in its first function prologue, and without this
/// that instruction is undefined. Two switches, because the architecture has
/// two: `CPACR` grants access to the coprocessors, and `FPEXC.EN` turns the
/// FPU on.
///
/// `FPEXC` is written from the one assembly block that tells the assembler
/// there is an FPU, which the soft-float kernel target does not declare. It is
/// only written if `CPACR` kept the access bits: on a core without an FPU they
/// read back as zero, and touching `FPEXC` there would be an undefined
/// instruction in the kernel rather than in the program.
///
/// Saving and loading a program's registers is the scheduler's, on every
/// switch between tasks that run user code; see `switch::UserState`.
pub(crate) fn enable_user_fpu() {
    let mut granted: u32 = 0;
    // SAFETY: (SYSREG) `CPACR` only gates coprocessor access; the kernel does not rely
    // on the FPU being denied. The `isb` makes the grant visible to the read
    // that follows.
    unsafe {
        asm!(
            "mrc p15, 0, {scratch}, c1, c0, 2",
            "orr {scratch}, {scratch}, #{bits}",
            "mcr p15, 0, {scratch}, c1, c0, 2",
            "isb",
            "mrc p15, 0, {scratch}, c1, c0, 2",
            scratch = inout(reg) granted,
            bits = const CPACR_CP10_CP11_FULL,
            options(nostack, preserves_flags),
        );
    }
    if granted & CPACR_CP10_CP11_FULL != CPACR_CP10_CP11_FULL {
        return;
    }
    // SAFETY: (SYSREG) access to coprocessor 10 was just granted and read back, so the
    // FPU exists and `FPEXC` is accessible.
    unsafe { super::switch::fpu_enable() };

    // How many double registers a program's state has, which is what a switch
    // between programs has to save. `MVFR0`'s low four bits say: one for
    // sixteen, two for thirty-two.
    // SAFETY: (SYSREG) as above; a read of an identification register.
    let features = unsafe { super::switch::fpu_features() };
    // SAFETY: (SYSREG) as above.
    let more = unsafe { super::switch::fpu_features1() };
    USER_MVFR0.store(features, core::sync::atomic::Ordering::Relaxed);
    USER_MVFR1.store(more, core::sync::atomic::Ordering::Relaxed);
    let doubles = match features & 0xF {
        1 => 16,
        2 => 32,
        _ => 0,
    };
    USER_FPU_DOUBLES.store(doubles, core::sync::atomic::Ordering::Relaxed);
}

/// Double registers in a program's floating-point state: 0 with no FPU, else 16
/// or 32. Set by [`enable_user_fpu`], and the same on every core of a machine.
static USER_FPU_DOUBLES: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// How many double registers a program's floating-point state has.
pub(crate) fn user_fpu_doubles() -> u8 {
    USER_FPU_DOUBLES.load(core::sync::atomic::Ordering::Relaxed)
}

/// `MVFR0` as [`enable_user_fpu`] read it; zero with no FPU.
static USER_MVFR0: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// `MVFR1` as [`enable_user_fpu`] read it; zero with no FPU.
static USER_MVFR1: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// `MVFR0` and `MVFR1`, the FPU's feature registers, or zeroes with no FPU.
///
/// Kept from [`enable_user_fpu`] rather than read again, because reading them
/// needs the coprocessor access only that function has checked for.
pub(crate) fn user_fpu_features() -> (u32, u32) {
    (
        USER_MVFR0.load(core::sync::atomic::Ordering::Relaxed),
        USER_MVFR1.load(core::sync::atomic::Ordering::Relaxed),
    )
}

/// Read `ID_ISAR0`, which says whether the core divides in hardware.
pub(crate) fn read_id_isar0() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) reading an identification register has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c0, c2, 0", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Read `ID_MMFR0`, which says whether the core has LPAE.
pub(crate) fn read_id_mmfr0() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) reading an identification register has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c0, c1, 4", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Read `TPIDRURO`, a program's thread pointer.
pub(crate) fn read_tpidruro() -> u32 {
    let value: u32;
    // SAFETY: (CONTEXT) reading a software register has no side effects.
    unsafe {
        asm!(
            "mrc p15, 0, {}, c13, c0, 3",
            out(reg) value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// Install the exception vector table.
///
/// Also clears `SCTLR.V` and `SCTLR.TE`, which firmware may have left set and
/// either of which makes `VBAR` meaningless: one sends exceptions to a fixed
/// address in the kernel image's region where nothing is mapped, the other
/// enters the vectors in the wrong instruction set.
///
/// # Safety
///
/// (ENTRY) `table` must be the address of an eight-entry ARM-state vector table,
/// 32-byte aligned, every entry of which is real code.
pub(crate) unsafe fn install_vectors(table: u32) {
    // SAFETY: (ENTRY) the caller guarantees the table. The `isb` makes both writes
    // take effect before the next instruction is fetched.
    unsafe {
        asm!(
            "mrc p15, 0, {scratch}, c1, c0, 0",
            "bic {scratch}, {scratch}, #{high}",
            "bic {scratch}, {scratch}, #{thumb}",
            "mcr p15, 0, {scratch}, c1, c0, 0",
            "mcr p15, 0, {table}, c12, c0, 0",
            "isb",
            scratch = out(reg) _,
            table = in(reg) table,
            high = const SCTLR_HIGH_VECTORS,
            thumb = const SCTLR_THUMB_EXCEPTIONS,
            options(nostack, preserves_flags),
        );
    }
}

/// Stop the CPU translating the lower half of the address space at all.
///
/// How ARMv7-A drops the loader's identity map, as AArch64 does: the map is a
/// second regime with its own base register, so it is switched off rather
/// than dismantled. `TTBR0` is zeroed as well, so a later change that clears
/// `EPD0` cannot resurrect the loader's tables, and the TLB is invalidated,
/// because entries the identity map left there would otherwise keep
/// translating until something evicted them.
///
/// # Safety
///
/// (TRANSLATE) Nothing may still be executing or reading through the lower half.
pub(crate) unsafe fn disable_ttbr0() {
    // SAFETY: (TRANSLATE) the caller guarantees nothing needs the lower half.
    unsafe {
        asm!(
            "mrc p15, 0, {scratch}, c2, c0, 2",
            "orr {scratch}, {scratch}, #{epd0}",
            "mcr p15, 0, {scratch}, c2, c0, 2",
            "mcrr p15, 0, {zero}, {zero}, c2",
            "isb",
            "mcr p15, 0, {zero}, c8, c7, 0",
            "dsb",
            "isb",
            scratch = out(reg) _,
            zero = in(reg) 0u32,
            epd0 = const TTBCR_EPD0,
            options(nostack, preserves_flags),
        );
    }
}

/// Translate the lower half through the tables at `root`, with `ASID` zero.
///
/// The inverse of [`disable_ttbr0`], and the two writes are in the order that
/// order matters in: `TTBR0` first, `TTBCR.EPD0` second. `disable_ttbr0`
/// zeroed the register precisely so that clearing `EPD0` on its own could not
/// resurrect the loader's tables — this is the change it was guarding against,
/// and the guard holds only if the root is in place before the regime is
/// switched back on.
///
/// `TTBR0` is a 64-bit register in the long-descriptor format and takes a
/// `mcrr` pair. Its `ASID` field is bits 55 to 48 of the high word, left zero
/// because stage 6 allocates no address space identifiers; see
/// [`flush_user_tlb`].
///
/// # Safety
///
/// (TRANSLATE) `root` must be the physical address of a live translation table for the
/// lower half, and it must stay live until another root replaces it here.
pub(crate) unsafe fn write_ttbr0(root: u64) {
    let low = root as u32;
    let high = (root >> 32) as u32;
    // SAFETY: (TRANSLATE) the caller guarantees the tables. The `isb` makes both writes
    // take effect before the next instruction is fetched.
    unsafe {
        asm!(
            "mcrr p15, 0, {low}, {high}, c2",
            "mrc p15, 0, {scratch}, c2, c0, 2",
            "bic {scratch}, {scratch}, #{epd0}",
            "mcr p15, 0, {scratch}, c2, c0, 2",
            "isb",
            low = in(reg) low,
            high = in(reg) high,
            scratch = out(reg) _,
            epd0 = const TTBCR_EPD0,
            options(nostack, preserves_flags),
        );
    }
}

/// Drop this processor's cached user translations, and keep the kernel's.
///
/// `TLBIASID` invalidates the entries matching an `ASID`, which by definition
/// are the ones not marked global — so the kernel's, all mapped global through
/// `TTBR1`, survive. That is why this is not [`flush_tlb`]: that one is
/// `TLBIALLIS`, which throws the global entries away too *and* broadcasts to
/// every core, and an address space switch neither needs nor can afford
/// either.
///
/// `nsh` on the barrier for AArch64's reason: the invalidation is this
/// processor's business, and a processor about to run this address space
/// invalidates as it installs the root.
pub(crate) fn flush_user_tlb() {
    // SAFETY: (TRANSLATE) invalidating translations can only cost a re-walk. The `dsb`
    // waits for it and the `isb` keeps the next instruction from being fetched
    // through an entry it removed.
    unsafe {
        asm!(
            "mcr p15, 0, {asid}, c8, c7, 2",
            "dsb nsh",
            "isb",
            asid = in(reg) 0_u32,
            options(nostack, preserves_flags),
        );
    }
}

/// The frequency of the architected counter, in hertz.
pub(crate) fn read_cntfrq() -> u32 {
    let frequency: u32;
    // SAFETY: (SYSREG) reading `CNTFRQ` has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c14, c0, 0", out(reg) frequency, options(nomem, nostack, preserves_flags));
    }
    frequency
}

/// The virtual counter.
///
/// The `isb` is not optional, for AArch64's reason: without it the read is
/// free to be satisfied out of order with the instructions around it.
pub(crate) fn read_cntvct() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: (SYSREG) a barrier and a 64-bit system register read.
    unsafe {
        asm!(
            "isb",
            "mrrc p15, 1, {low}, {high}, c14",
            low = out(reg) low,
            high = out(reg) high,
            options(nostack, preserves_flags),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// Set the instant the virtual timer fires at.
pub(crate) fn write_cntv_cval(instant: u64) {
    // SAFETY: (SYSREG) `CNTV_CVAL` is a comparator; writing it changes when the
    // timer's output asserts and nothing else.
    unsafe {
        asm!(
            "mcrr p15, 3, {low}, {high}, c14",
            low = in(reg) instant as u32,
            high = in(reg) (instant >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// This core's multiprocessor affinity register.
pub(crate) fn read_mpidr() -> u32 {
    let mpidr: u32;
    // SAFETY: (SYSREG) reading `MPIDR` has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c0, c0, 5", out(reg) mpidr, options(nomem, nostack, preserves_flags));
    }
    mpidr
}

/// This core's auxiliary control register.
///
/// Read only, and deliberately: on a Cortex-A7 the bit worth knowing about is
/// `ACTLR.SMP`, which has to be set before the caches and MMU come on or the
/// core is not coherent with the others, and the secure world decides whether
/// the non-secure world may write it at all. Reading is permitted regardless,
/// so this can report what firmware did without depending on being allowed to
/// change it. What the bits mean is implementation defined, which is why the
/// caller names the one it wants rather than this returning anything richer
/// than the register.
pub(crate) fn read_actlr() -> u32 {
    let actlr: u32;
    // SAFETY: (SYSREG) reading `ACTLR` has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c1, c0, 1", out(reg) actlr, options(nomem, nostack, preserves_flags));
    }
    actlr
}

/// Set `TPIDRPRW`, the thread ID register the kernel keeps its per-CPU record
/// in.
///
/// The PL1-only one of the three, which is the point: the hardware never reads
/// it, and user mode can neither read nor write it — unlike `TPIDRURW`, which
/// it can write, and `TPIDRURO`, which it can read.
pub(crate) fn write_tpidrprw(value: u32) {
    // SAFETY: (SYSREG) writing a scratch register has no effect beyond the register.
    unsafe {
        asm!("mcr p15, 0, {}, c13, c0, 4", in(reg) value, options(nomem, nostack, preserves_flags));
    }
}

/// Read `TPIDRPRW`.
pub(crate) fn read_tpidrprw() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) reading a scratch register has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c13, c0, 4", out(reg) value, options(nomem, nostack, preserves_flags));
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
pub(crate) fn frame_pointer() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) reading a register has no side effects.
    unsafe {
        asm!("mov {}, r11", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// `MAIR0`: the first four memory attribute encodings.
pub(crate) fn read_mair0() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) reading `MAIR0` has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c10, c2, 0", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// `MAIR1`: the other four.
pub(crate) fn read_mair1() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) reading `MAIR1` has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c10, c2, 1", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// `TTBCR`: the long-descriptor format, and where the address space splits
/// between `TTBR0` and `TTBR1`.
pub(crate) fn read_ttbcr() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) reading `TTBCR` has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c2, c0, 2", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// `TTBR0` in the long-descriptor format: the root address in bits 39 to 0,
/// the `ASID` in bits 55 to 48. A 64-bit register, read as an `mrrc` pair.
pub(crate) fn read_ttbr0() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: (SYSREG) reading `TTBR0` has no side effects.
    unsafe {
        asm!(
            "mrrc p15, 0, {low}, {high}, c2",
            low = out(reg) low,
            high = out(reg) high,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

/// The table address bits of a long-descriptor `TTBR0` value.
pub(crate) const TTBR_ADDRESS: u64 = 0x0000_00FF_FFFF_FFFF;

/// `SCTLR`: the MMU, the caches, where the vectors are.
pub(crate) fn read_sctlr() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) reading `SCTLR` has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c1, c0, 0", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// The smallest data cache line in bytes, from `CTR`'s `DminLine`, which is
/// log2 of it in words.
fn data_line() -> u64 {
    let cache_type: u32;
    // SAFETY: (SYSREG) reading `CTR` has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c0, c0, 1", out(reg) cache_type, options(nomem, nostack, preserves_flags));
    }
    4u64 << ((cache_type >> 16) & 0xF)
}

/// Write the data cache lines covering `start..start + len` back to the point
/// of coherency.
///
/// For memory a core with its caches off is about to read: it reads RAM, and
/// a line still dirty in this core's cache is a line it does not see.
pub(crate) fn clean_to_poc(start: u64, len: u64) {
    let line = data_line();
    let mut at = start - start % line;
    let end = start.saturating_add(len);
    while at < end {
        // SAFETY: (SYSREG) `DCCMVAC` writes one line back by virtual address and changes
        // no data; the caller's range is mapped, and below 4 GiB like every
        // address on this architecture.
        unsafe {
            asm!("mcr p15, 0, {}, c7, c10, 1", in(reg) at as u32, options(nostack, preserves_flags));
        }
        at += line;
    }
    // SAFETY: (SYSREG) a barrier, completing the maintenance above before anything
    // after it — in particular the call that starts the core that reads it.
    unsafe {
        asm!("dsb", options(nostack, preserves_flags));
    }
}

/// Make the instructions in `start..start + len`, written through the data
/// side, the ones every core fetches from that memory: AArch64's
/// `sync_instructions`, for the same reasons, with this architecture's
/// operations. The lines are cleaned to the point of coherency, which is
/// past the point of unification where fetches meet them, by
/// [`clean_to_poc`], whose last barrier completes it. Then `ICIALLUIS`
/// empties every instruction cache in the inner shareable domain and
/// `BPIALLIS` every branch predictor, whose targets can point into what was
/// there before.
pub(crate) fn sync_instructions(start: u64, len: u64) {
    clean_to_poc(start, len);
    // SAFETY: (SYSREG) barriers and cache and predictor maintenance change no data;
    // the register the two invalidates take is ignored.
    unsafe {
        asm!(
            "mcr p15, 0, {zero}, c7, c1, 0",
            "mcr p15, 0, {zero}, c7, c1, 6",
            "dsb",
            "isb",
            zero = in(reg) 0_u32,
            options(nostack, preserves_flags)
        );
    }
}

/// Write the data cache lines covering `start..start + len` back to the point
/// of coherency and drop them.
///
/// For memory about to be shared with a device that does not snoop, through
/// a mapping that bypasses the caches: a dirty line left behind could be
/// evicted later, over whatever the device wrote since, and a clean one
/// would be read in place of it by the next cached access.
pub(crate) fn clean_invalidate_to_poc(start: u64, len: u64) {
    let line = data_line();
    let mut at = start - start % line;
    let end = start.saturating_add(len);
    while at < end {
        // SAFETY: (SYSREG) `DCCIMVAC` writes one line back and invalidates it by
        // virtual address; the data it holds reaches memory first, so nothing
        // is lost. The caller's range is mapped, and below 4 GiB.
        unsafe {
            asm!("mcr p15, 0, {}, c7, c14, 1", in(reg) at as u32, options(nostack, preserves_flags));
        }
        at += line;
    }
    // SAFETY: (SYSREG) a barrier, completing the maintenance before the memory is
    // handed over.
    unsafe {
        asm!("dsb", options(nostack, preserves_flags));
    }
}

/// Order every store before this against the next write to device memory.
///
/// For sending a software-generated interrupt, as on AArch64: the core that
/// takes it must see what this one wrote before asking, and the interrupt is
/// a device write, which an ordinary memory barrier does not order.
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

/// Wait for an interrupt with IRQs masked, then unmask them.
///
/// `wfi` wakes on a *pending* interrupt whether or not it is masked, so one
/// that became pending after the caller masked is not lost: the wait returns
/// at once, and the unmask after it lets the interrupt be taken.
pub(crate) fn wait_then_enable_interrupts() {
    // SAFETY: (SYSREG) `wfi` is a hint and `cpsie i` only clears the IRQ mask bit; the
    // vector table is installed long before this is used.
    unsafe {
        asm!("wfi", "cpsie i", options(nomem, nostack));
    }
}

/// A PSCI call in the 32-bit convention, through whichever instruction the
/// device tree says the firmware answers.
///
/// # Safety
///
/// (FIRMWARE) `function` must be a PSCI function taking `a`, `b` and `c`, and what it
/// does must be what the caller intends: `CPU_ON` starts a core executing at
/// an address the caller chose.
pub(crate) unsafe fn psci_call(conduit: PsciConduit, function: u32, a: u32, b: u32, c: u32) -> u32 {
    let result: u32;
    match conduit {
        // SAFETY: (FIRMWARE) the caller guarantees the function and its arguments. The
        // calling convention lets the callee corrupt `r0`–`r3` and `r12`,
        // which is what the outputs say.
        PsciConduit::Hvc => unsafe {
            asm!(
                ".arch_extension virt",
                "hvc #0",
                inlateout("r0") function => result,
                inlateout("r1") a => _,
                inlateout("r2") b => _,
                inlateout("r3") c => _,
                lateout("r12") _,
                options(nostack),
            );
        },
        // SAFETY: (FIRMWARE) as above, through the secure monitor.
        PsciConduit::Smc => unsafe {
            asm!(
                ".arch_extension sec",
                "smc #0",
                inlateout("r0") function => result,
                inlateout("r1") a => _,
                inlateout("r2") b => _,
                inlateout("r3") c => _,
                lateout("r12") _,
                options(nostack),
            );
        },
    }
    result
}

/// Enable or mask the virtual timer.
pub(crate) fn write_cntv_ctl(control: u32) {
    // SAFETY: (SYSREG) `CNTV_CTL` holds the timer's enable and mask bits. The `isb`
    // makes the write take effect before the next instruction, so a caller
    // that disarms and then returns cannot take one more interrupt.
    unsafe {
        asm!(
            "mcr p15, 0, {}, c14, c3, 1",
            "isb",
            in(reg) control,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// `CNTKCTL`: what of the generic timer user mode may touch.
pub(crate) fn read_cntkctl() -> u32 {
    let control: u32;
    // SAFETY: (SYSREG) reading `CNTKCTL` has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c14, c1, 0", out(reg) control, options(nomem, nostack, preserves_flags));
    }
    control
}

/// Let user mode read the virtual counter, and close the rest of the timer
/// to it: `CNTKCTL.PL0VCTEN` set; `PL0PCTEN`, `PL0VTEN` and `PL0PTEN` clear,
/// whatever firmware left there, as Linux's `arch_counter_set_user_access`
/// does on ARM and arm64 alike. A program then reads `CNTVCT` with `mrrc`
/// for its clock without a system call; the physical counter, and both
/// timers' compare and control registers, stay the kernel's. `EVNTEN`,
/// `EVNTDIR` and `EVNTI` are left as found: they are the event stream, which
/// wakes a `wfe` and gives no program a register, and nothing in Ferrix turns
/// it on. A processor's own register: every processor runs this before it
/// can run a program -- the boot processor from `timer::init`, each
/// secondary in `smp::secondary_start` -- and it is the only writer of
/// `CNTKCTL` in the kernel.
pub(crate) fn allow_user_counter() {
    let control = (read_cntkctl() & !CNTKCTL_CLOSED) | CNTKCTL_PL0VCTEN;
    // SAFETY: (SYSREG) `CNTKCTL` decides only what user mode may read or program of
    // the generic timer; no mapping, interrupt or timer of the kernel's
    // changes. The `isb` makes the write take effect before a program runs.
    unsafe {
        asm!("mcr p15, 0, {}, c14, c1, 0", "isb", in(reg) control, options(nostack, preserves_flags));
    }
}

/// `CNTKCTL.PL0VCTEN`: user mode may read `CNTVCT` and `CNTFRQ`.
pub(crate) const CNTKCTL_PL0VCTEN: u32 = 1 << 1;

/// What user mode is never given: `PL0PCTEN` (the physical counter),
/// `PL0VTEN` and `PL0PTEN` (the virtual and physical timers' registers).
pub(crate) const CNTKCTL_CLOSED: u32 = 1 | (1 << 8) | (1 << 9);

/// MEASUREMENT ONLY (`bench_pmu`, never lands): `ID_DFR0.PerfMon`, bits 27
/// to 24 -- 1 `PMUv1`, 2 `PMUv2`, 3 `PMUv3`; 0 and 0xF no PMU the architecture
/// defines.
pub(crate) fn pmu_version() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) reading an identification register has no side effects.
    unsafe {
        asm!("mrc p15, 0, {}, c0, c1, 2", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    (value >> 24) & 0xF
}

/// MEASUREMENT ONLY (`bench_pmu`): `PMCR`, for the line that says what a
/// program finds. Only where [`pmu_version`] names a PMU.
pub(crate) fn read_pmcr() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) reading `PMCR` has no side effects; the caller checked
    // that the core has a PMU, so the encoding is defined.
    unsafe {
        asm!("mrc p15, 0, {}, c9, c12, 0", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// MEASUREMENT ONLY (`bench_pmu`): set `PMUSERENR.EN`, so user mode may
/// program and read this processor's PMU. Only where [`pmu_version`] names
/// one.
pub(crate) fn allow_user_pmu() {
    // SAFETY: (SYSREG) `PMUSERENR` decides only what user mode may reach of
    // the PMU, which the kernel does not use; no mapping, interrupt or timer
    // of the kernel's changes. The `isb` makes the write take effect before
    // a program runs.
    unsafe {
        asm!("mcr p15, 0, {}, c9, c14, 0", "isb", in(reg) PMUSERENR_EN, options(nostack, preserves_flags));
    }
}

/// MEASUREMENT ONLY (`bench_pmu`): `PMUSERENR`, read back after
/// [`allow_user_pmu`]. Only where [`pmu_version`] names a PMU.
pub(crate) fn read_pmuserenr() -> u32 {
    let value: u32;
    // SAFETY: (SYSREG) reading `PMUSERENR` has no side effects; the caller
    // checked that the core has a PMU, so the encoding is defined.
    unsafe {
        asm!("mrc p15, 0, {}, c9, c14, 0", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// `PMUSERENR.EN`: user mode may reach the PMU.
const PMUSERENR_EN: u32 = 1;
