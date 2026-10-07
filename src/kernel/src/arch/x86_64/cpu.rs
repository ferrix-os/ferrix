//! Single x86-64 instructions with no spelling in Rust.
//!
//! Every function here is one instruction. They are on the assembly allow-list
//! as "CPU primitives" (`docs/ASSEMBLY.md`) because reading a control register
//! or touching the I/O address space is not something Rust has syntax for —
//! not because assembly is convenient.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Write a byte to an I/O port.
///
/// # Safety
///
/// (DEVICE) I/O ports are device registers: the caller must know what device is behind
/// `port` and that writing `value` to it is intended.
pub(crate) unsafe fn outb(port: u16, value: u8) {
    // SAFETY: (DEVICE) the caller guarantees the port and the value.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

/// Read a byte from an I/O port.
///
/// # Safety
///
/// (DEVICE) As [`outb`]: the caller must know what device is behind `port`. Reading a
/// device register can have side effects — on a 16550, reading the receive
/// buffer consumes a character.
pub(crate) unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: (DEVICE) the caller guarantees the port.
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Write a 32-bit word to an I/O port.
///
/// # Safety
///
/// (DEVICE) As [`outb`].
pub(crate) unsafe fn outl(port: u16, value: u32) {
    // SAFETY: (DEVICE) the caller guarantees the port and the value.
    unsafe {
        asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags));
    }
}

/// Halt until the next interrupt.
pub(crate) fn hlt() {
    // SAFETY: (SYSREG) `hlt` at ring 0 stops the CPU until an interrupt arrives and has
    // no other effect.
    unsafe {
        asm!("hlt", options(nomem, nostack, preserves_flags));
    }
}

/// Mask interrupts on this CPU.
pub(crate) fn disable_interrupts() {
    // SAFETY: (SYSREG) `cli` only clears the interrupt flag.
    unsafe {
        asm!("cli", options(nomem, nostack));
    }
}

/// `CR4.PGE` — the bit that makes the global bit in a page table entry mean
/// anything. The loader sets it; see `src/boot/common/uefi/src/arch/x86_64.rs`.
const CR4_PGE: u64 = 1 << 7;

/// `CR4.SMEP` — the processor refuses to *execute* a user page in ring 0.
const CR4_SMEP: u64 = 1 << 20;

/// `CR4.UMIP` — `SGDT`, `SIDT`, `SLDT`, `SMSW` and `STR` fault outside ring 0.
const CR4_UMIP: u64 = 1 << 11;

/// `CR4.SMAP` — the processor refuses to *read or write* a user page in ring 0
/// unless `EFLAGS.AC` is set.
const CR4_SMAP: u64 = 1 << 21;

/// `CR4.OSXSAVE` — `XSAVE`, `XRSTOR` and `XSETBV` may run, and `CPUID` tells
/// a program so (leaf 1, `ECX` bit 27), which is what it reads before it
/// asks `XGETBV` whether it may use AVX.
const CR4_OSXSAVE: u64 = 1 << 18;

/// The x87 and SSE state components, bits 0 and 1 of `XCR0`: the two
/// `FXSAVE` already saved, and the least `XCR0` may hold.
pub(crate) const XSTATE_X87_SSE: u64 = 0b011;

/// The AVX state component, bit 2: the upper halves of the sixteen `YMM`
/// registers.
pub(crate) const XSTATE_AVX: u64 = 0b100;

/// The x87 state component alone, bit 0.
pub(crate) const XSTATE_X87: u64 = 0b001;

/// `VZEROALL`, then `LDMXCSR` of `mxcsr`: every `YMM` register zero, which is
/// SSE's and AVX's initial state but for `MXCSR`, and `MXCSR` given.
///
/// # Safety
///
/// (SYSREG) AVX in `XCR0`, and `mxcsr` a value `STMXCSR` stored.
pub(crate) unsafe fn zero_vectors(mxcsr: u32) {
    // SAFETY: (SYSREG) the caller's guarantee; a load of this frame's word.
    unsafe {
        asm!(
            "vzeroall",
            "ldmxcsr [{0}]",
            in(reg) &raw const mxcsr,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
            out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
            options(nostack, preserves_flags),
        );
    }
}

/// Whether the x87 may hold anything but its initial state: `XINUSE` bit 0,
/// read with `XGETBV` 1 where `CPUID` 0xD.1 `EAX` bit 2 offers it, and
/// `true` where it does not, so the caller resets it either way.
pub(crate) fn x87_in_use() -> bool {
    if !XINUSE_READABLE.load(Ordering::Relaxed) {
        return true;
    }
    // SAFETY: (SYSREG) `XGETBV` with `ECX` 1, which `CPUID` said exists.
    let inuse = unsafe { core::arch::x86_64::_xgetbv(1) };
    inuse & XSTATE_X87 != 0
}

/// Whether `XGETBV` 1 may be asked, for the check.
pub(crate) fn xinuse_readable() -> bool {
    XINUSE_READABLE.load(Ordering::Relaxed)
}

/// Whether `XGETBV` 1 may be asked: decided by [`enable_extended_state`].
static XINUSE_READABLE: AtomicBool = AtomicBool::new(false);

/// Bytes of an `XSAVE` area holding x87, SSE and AVX in the standard form:
/// the 512-byte legacy area, the 64-byte header, and AVX's 256 bytes at the
/// architectural offset 576.
pub(crate) const XSAVE_AREA_BYTES: u64 = 832;

/// The state components `XCR0` enables on every processor: zero until
/// [`enable_extended_state`] has run on the boot processor, and zero for good
/// on one without `XSAVE`, which the switch then saves with `FXSAVE`.
static XSAVE_COMPONENTS: AtomicU64 = AtomicU64::new(0);

/// Let programs use AVX: set `CR4.OSXSAVE` and put x87, SSE and AVX in
/// `XCR0`, on the boot processor. Secondaries take the same `CR4` from it and
/// load the same `XCR0` in [`load_extended_state_on_this_cpu`].
///
/// A program learns whether it may use AVX from `CPUID`'s `OSXSAVE` bit and
/// from `XCR0`, not from the processor having it: with `OSXSAVE` clear it
/// must not, because nothing would save the upper halves of its `YMM`
/// registers when it is switched away. A runtime built for AVX2 without a
/// fallback -- Bun's, in Claude Code -- then finds no code path it may take
/// and cannot run. So the switch saves them with `XSAVE`
/// (`switch::save_user_state`), and a signal frame carries them
/// (`signal::write_fp_area`).
///
/// Only x87, SSE and AVX, even on a processor with more: each further
/// component -- AVX-512's three, AMX's tiles -- grows every thread's saved
/// state and every signal frame, and a program checks `XCR0` before using
/// them, so leaving them out costs a program that wants them its fast path
/// and nothing else.
///
/// AVX only where `allow_avx` says: `speculation::vector_leak` withholds it
/// on a processor where Zenbleed or Gather Data Sampling would let one
/// program read another's vector registers and nothing covers it.
///
/// Answers the components enabled, for the boot log: zero when the processor
/// has no `XSAVE`, as QEMU's `qemu64` model does not.
pub(crate) fn enable_extended_state(allow_avx: bool) -> u64 {
    use core::arch::x86_64::{__cpuid, __cpuid_count};

    if __cpuid(0).eax < 0xD {
        return 0;
    }
    let features = __cpuid(1).ecx;
    let has_xsave = features & (1 << 26) != 0;
    let has_avx = features & (1 << 28) != 0;
    if !has_xsave {
        return 0;
    }
    let supported = __cpuid_count(0xD, 0);
    let supported = u64::from(supported.eax) | (u64::from(supported.edx) << 32);
    if supported & XSTATE_X87_SSE != XSTATE_X87_SSE {
        return 0;
    }
    let mut components = XSTATE_X87_SSE;
    if has_avx && allow_avx && supported & XSTATE_AVX != 0 {
        components |= XSTATE_AVX;
    }
    // SAFETY: (SYSREG) CPUID reported `XSAVE`, which is all `OSXSAVE` needs.
    unsafe { write_cr4(read_cr4() | CR4_OSXSAVE) };
    // SAFETY: (SYSREG) `OSXSAVE` is set, and every bit is one leaf 0xD says
    // this processor supports, x87 among them, as `XSETBV` requires.
    unsafe { write_xcr0(components) };
    // The area the switch saves into holds x87, SSE and AVX where the
    // standard form puts them, and nothing past them. A processor that
    // disagrees about the size gets x87 and SSE alone, which fit any.
    if u64::from(__cpuid_count(0xD, 0).ebx) > XSAVE_AREA_BYTES
        || (components & XSTATE_AVX != 0 && __cpuid_count(0xD, 2).ebx != 576)
    {
        components = XSTATE_X87_SSE;
        // SAFETY: (SYSREG) as above, with fewer bits.
        unsafe { write_xcr0(components) };
    }
    XSAVE_COMPONENTS.store(components, Ordering::Relaxed);
    XINUSE_READABLE.store(__cpuid_count(0xD, 1).eax & (1 << 2) != 0, Ordering::Relaxed);
    components
}

/// Load the boot processor's `XCR0` on this one, which took its `CR4` --
/// `OSXSAVE` with it -- as it started.
pub(crate) fn load_extended_state_on_this_cpu() {
    let components = XSAVE_COMPONENTS.load(Ordering::Relaxed);
    if components != 0 && read_cr4() & CR4_OSXSAVE != 0 {
        // SAFETY: (SYSREG) `OSXSAVE` is set, and the boot processor, of the
        // same kind, accepted these bits.
        unsafe { write_xcr0(components) };
    }
}

/// The state components the switch saves with `XSAVE`: zero to save with
/// `FXSAVE`.
pub(crate) fn extended_state_components() -> u64 {
    XSAVE_COMPONENTS.load(Ordering::Relaxed)
}

/// The `PKRU` state component, bit 9 of `XCR0`. Never enabled here (`CR4.PKE`
/// stays off), and never part of a vector reset if it ever is: its initial
/// value grants every protection key, so a reset would widen a program's own
/// protection (`docs/OPAQUE-KERNEL.md` §9.8, 3a). Linux keeps it apart the
/// same way.
pub(crate) const XSTATE_PKRU: u64 = 1 << 9;

/// Every state component the switch's vector reset may initialise: the ones
/// `XCR0` enables, never [`XSTATE_PKRU`]. The reference configuration enables
/// x87, SSE and AVX only ([`enable_extended_state`]); a component past those
/// is reviewed before it is enabled, and AMX's would also have to leave out a
/// component armed in `IA32_XFD`.
pub(crate) fn reset_components() -> u64 {
    extended_state_components() & !XSTATE_PKRU
}

/// The live `MXCSR` and x87 control word, the two parts of the vector state a
/// blocking native call keeps (`switch::save_user_state`): `stmxcsr` and
/// `fnstcw`, each a store of a register Rust cannot name to memory.
///
/// # Safety
///
/// (SYSREG) `CR4.OSFXSR` set, which the switch's `FXSAVE64` and `XSAVE64`
/// already rely on on every processor that runs programs.
pub(crate) unsafe fn read_vector_controls() -> (u32, u16) {
    let mut mxcsr = 0_u32;
    let mut control = 0_u16;
    // SAFETY: (SYSREG) two stores to this frame's own words; neither changes
    // a register.
    unsafe {
        asm!(
            "stmxcsr [{0}]",
            "fnstcw [{1}]",
            in(reg) &raw mut mxcsr,
            in(reg) &raw mut control,
            options(nostack, preserves_flags),
        );
    }
    (mxcsr, control)
}

/// Load `control` into the x87 control word: `fldcw`, after a reset whose
/// `XRSTOR` left the initial `0x037F` there.
///
/// # Safety
///
/// (SYSREG) The registers must be the running task's, which the switch is about
/// to resume, and `control` one that task had.
pub(crate) unsafe fn load_x87_control(control: u16) {
    // SAFETY: (SYSREG) a load of the x87 control word from this frame's word;
    // the kernel never uses the x87, so only the resumed program sees it.
    unsafe {
        asm!("fldcw [{0}]", in(reg) &raw const control, options(nostack, preserves_flags));
    }
}

/// Write `XCR0`, the state components `XSAVE` manages and a program may use.
///
/// # Safety
///
/// (SYSREG) `CR4.OSXSAVE` must be set, bit 0 must be set, and every bit must be one
/// this processor supports, or `XSETBV` faults.
unsafe fn write_xcr0(components: u64) {
    // SAFETY: (SYSREG) the caller's guarantee. `XSETBV` as `core::arch` spells it.
    unsafe { core::arch::x86_64::_xsetbv(0, components) };
}

/// Turn on the hardware that keeps ring 0 out of user pages.
///
/// # Why this is safe to switch on without auditing every access
///
/// SMAP faults a kernel access to a user *linear* address, and this kernel
/// makes none. Every system call that takes a pointer goes through
/// `crate::syscall::uaccess`, which resolves the address through the target
/// [`AddressSpace`] and reaches the page through the **direct map** — a kernel
/// address — precisely so that it works on a space that is not installed on
/// this processor, which is what `execve` needs. So the paths that legitimately
/// touch a program's memory are already invisible to SMAP, and nothing needs
/// `stac`/`clac` around it.
///
/// That makes this cheap to turn on and worth having: what SMAP now catches is
/// the case `uaccess`'s own header calls out as the one nothing in the hardware
/// was stopping — a path that dereferences a user pointer directly, whether by
/// a future mistake or a wild pointer. Until now the bound check in `uaccess`
/// was the only barrier, which `docs/certification/VULNERABILITY-ANALYSIS.md`
/// records as V-01 and finding F-32.
///
/// SMEP is the same argument for instruction fetches, and there is no
/// legitimate case at all: the kernel never executes a user page.
///
/// # What happens if the premise is wrong
///
/// A page fault with the reserved-bit-clear, user-page, supervisor-mode
/// signature, reported by `report_trap` like any other. That is the intended
/// outcome — it is the bug being made visible — and it is why this is enabled
/// before user mode rather than quietly at the end of boot.
///
/// Returns which of the two the processor had, for the boot log.
pub(crate) fn enable_user_access_protection() -> (bool, bool) {
    use core::arch::x86_64::{__cpuid, __cpuid_count};

    // Leaf 7 is only read when leaf 0 says it exists.
    if __cpuid(0).eax < 7 {
        return (false, false);
    }
    let features = __cpuid_count(7, 0).ebx;

    let smep = features & (1 << 7) != 0;
    let smap = features & (1 << 20) != 0;

    let mut cr4 = read_cr4();
    if smep {
        cr4 |= CR4_SMEP;
    }
    if smap {
        cr4 |= CR4_SMAP;
    }
    // SAFETY: (PROTECT) each bit is set only when CPUID reported the feature, and both
    // are legal in long mode with the four-level table already in force.
    unsafe { write_cr4(cr4) };
    SMAP_ON.store(smap, Ordering::Relaxed);

    (smep, smap)
}

/// Keep the descriptor tables' addresses from ring 3, where the processor
/// offers UMIP, and say whether it did.
///
/// Without it any program can execute `SIDT` and read the IDT's address, and
/// the IDT is a static in the kernel image: that one instruction gives away
/// the image's slide, and KASLR with it (`docs/certification/SPECULATION.md`
/// §6). `SGDT` gives a GDT's address the same way. With `CR4.UMIP` set each
/// of those instructions faults outside ring 0, and the program gets
/// `SIGSEGV`, as it would on Linux.
///
/// Secondary processors inherit it with the rest of the boot processor's
/// `CR4`.
pub(crate) fn enable_umip() -> bool {
    use core::arch::x86_64::{__cpuid, __cpuid_count};

    // CPUID.(EAX=7,ECX=0):ECX.UMIP is bit 2, and leaf 7 is only read when
    // leaf 0 says it exists.
    if __cpuid(0).eax < 7 || __cpuid_count(7, 0).ecx & (1 << 2) == 0 {
        return false;
    }
    // SAFETY: (PROTECT) CPUID reported UMIP, which is legal to set in long mode and
    // changes only what ring 3 may execute.
    unsafe { write_cr4(read_cr4() | CR4_UMIP) };
    read_cr4() & CR4_UMIP != 0
}

/// Whether `stac` and `clac` may be executed at all.
///
/// They are SMAP's instructions: on a processor without the feature they are
/// `#UD`, so the flag is read before either is issued rather than assuming the
/// hardware that [`enable_user_access_protection`] found.
static SMAP_ON: AtomicBool = AtomicBool::new(false);

/// Permit this processor to touch user pages until [`forbid_user_access`].
///
/// Sets `EFLAGS.AC`, which is the exception SMAP is built around. Almost
/// nothing needs it: `crate::syscall::uaccess` reaches a program's memory
/// through the direct map and never through a user linear address, so the
/// ordinary path is already invisible to SMAP. What needs it is the deliberate
/// case — `crate::user::check` installs an address space and reads back
/// through the user address *on purpose*, to prove the processor walks an
/// installed space, and that is exactly the access SMAP exists to refuse.
///
/// Keep the window as short as the access. An `AC` left set is SMAP switched
/// off for this processor.
pub(crate) fn permit_user_access() {
    if SMAP_ON.load(Ordering::Relaxed) {
        // SAFETY: (PROTECT) `stac` sets one flag, and is only reached when CPUID
        // reported SMAP, without which it would be an undefined instruction.
        unsafe { asm!("stac", options(nomem, nostack)) };
    }
}

/// Refuse user pages to this processor again.
pub(crate) fn forbid_user_access() {
    if SMAP_ON.load(Ordering::Relaxed) {
        // SAFETY: (PROTECT) `clac` clears one flag, under the same guard as `stac`.
        unsafe { asm!("clac", options(nomem, nostack)) };
    }
}

/// Invalidate the whole `TLB`, **including global entries**.
///
/// # Why this is not just a `CR3` reload
///
/// Reloading `CR3` is the obvious way to flush a `TLB` and it is the wrong one
/// here, because it leaves global entries exactly where they were — the
/// architecture says so, and that is what the global bit is *for*: a
/// translation that survives an address space switch.
///
/// Nearly every mapping this kernel makes is global. Kernel text, the direct
/// map and every device window carry `MapFlags::global`, because they are the
/// same in every address space and marking them so is what stops a process
/// switch from throwing away the kernel's own translations. So a flush that
/// spared global entries spared essentially everything, and `flush_tlb` was a
/// no-op wearing a descriptive name.
///
/// **This was not theoretical.** Under emulation it never showed, because a
/// `TLB` that does not really exist cannot hold a stale entry; under hardware
/// virtualisation it produced three different failures with one cause. The
/// `vmap` arena reuses address space, so a device window unmapped in stage 2
/// handed its address to the `HPET` in stage 3 — which then read the kernel
/// image through the old translation and reported a period no `HPET` can
/// have. The local `APIC` read its calibration the same way. And stage 4's
/// shootdown check, which exists precisely to catch a processor holding a
/// translation it was told to drop, caught this one.
///
/// Clearing and restoring `CR4.PGE` is the architecturally defined way to
/// invalidate global entries: a write to `CR4` that changes `PGE` flushes the
/// whole `TLB`, global entries included. `invlpg` per page would also do it
/// and is what a grown kernel uses for a small range; this is the blunt
/// instrument, and the right one while the kernel still counts its mappings in
/// the hundreds.
pub(crate) fn flush_tlb_including_global() {
    // **Masked throughout, and not for atomicity of the flush.** The hazard is
    // re-entry: an interrupt taken between the two writes would run with `PGE`
    // clear, and a handler that flushed the `TLB` itself would read that
    // `CR4`, clear an already-clear bit — changing nothing, so flushing
    // nothing — and restore a `CR4` with `PGE` still clear. Global pages would
    // then be off for the rest of the boot, silently.
    let flags = read_rflags();
    disable_interrupts();

    let cr4 = read_cr4();
    // SAFETY: (TRANSLATE) `cr4` was just read, so this is that value with one bit cleared.
    // Clearing `PGE` alters no mapping; its only effect is to invalidate the
    // whole `TLB`, global entries included, which is what this exists for. The
    // bit is restored immediately below, with interrupts masked in between.
    unsafe { write_cr4(cr4 & !CR4_PGE) };
    // SAFETY: (TRANSLATE) the value read a moment ago, put back unchanged, before anything
    // could observe `PGE` clear.
    unsafe { write_cr4(cr4) };

    // And `CR3`, for the non-global entries: toggling `PGE` covers them too,
    // but this is the operation the rest of the kernel means by "flush" and
    // leaving it out would make the correctness of this function depend on a
    // footnote rather than on two instructions that plainly do it.
    let cr3: u64;
    // SAFETY: (TRANSLATE) reading CR3 has no side effects.
    unsafe {
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
    }
    // SAFETY: (TRANSLATE) writing back the value just read changes no mapping; its only
    // effect is to flush the TLB, which is the point.
    unsafe {
        asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags));
    }

    if flags & RFLAGS_INTERRUPT != 0 {
        enable_interrupts();
    }
}

/// Drop this processor's cached translation of the page holding `address`.
///
/// `invlpg`, which drops the entry whether or not it is global, and only on
/// this processor.
pub(crate) fn invalidate_page(address: u64) {
    // SAFETY: (TRANSLATE) `invlpg` names an address and reads nothing at it; its only
    // effect is that the next use of the page re-walks the tables.
    unsafe {
        asm!("invlpg [{}]", in(reg) address, options(nostack, preserves_flags));
    }
}

/// `RFLAGS.IF` — interrupts are unmasked.
pub(crate) const RFLAGS_INTERRUPT: u64 = 1 << 9;

/// The flags register.
///
/// Read for one bit: `IF`, which says whether interrupts are unmasked. A lock
/// that masks interrupts has to restore the state it found rather than
/// unconditionally unmasking, or taking one inside another silently enables
/// interrupts halfway out of the outer critical section.
pub(crate) fn read_rflags() -> u64 {
    let flags: u64;
    // SAFETY: (SYSREG) `pushfq` and `pop` read the flags register through the stack and
    // leave it as they found it. `nostack` is deliberately *not* claimed: this
    // is the one primitive here that uses the stack.
    unsafe {
        asm!("pushfq", "pop {}", out(reg) flags, options(preserves_flags));
    }
    flags
}

/// Write a model-specific register.
///
/// # Safety
///
/// (SYSREG) `msr` must be a register this CPU implements and `value` one it accepts. An
/// invalid one is a general protection fault; a valid but wrong one changes how
/// the CPU behaves.
pub(crate) unsafe fn write_msr(msr: u32, value: u64) {
    // SAFETY: (SYSREG) the caller guarantees the register and the value.
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

/// `IA32_PAT`: the page attribute table.
const IA32_PAT: u32 = 0x277;

/// Program this processor's page attribute table with
/// [`ferrix_paging::X86_PAT`], whose entry 1 is write-combining, and say
/// whether it reads back so.
///
/// Before anything maps write-combining: entry 1 is what a descriptor with
/// only its write-through bit selects, which no mapping used before, so the
/// change of type reaches no translation a processor holds. The flush that
/// follows is for the rule (SDM Vol. 3A §11.12.4), not for one that does.
/// `false` on a processor whose `CPUID` says it has no PAT, which no x86-64
/// processor is.
///
/// Called once per processor, at its bring-up: Ferrix has no processor
/// hotplug, no S3 resume and no UEFI runtime calls, any of which can leave a
/// processor with another table. Whichever is added must call this again
/// before that processor runs anything that reaches a write-combining
/// mapping, and `object::io_mapping::combining_ready` refuses
/// write-combining while the count it keeps is short.
pub(crate) fn program_pat() -> bool {
    use core::arch::x86_64::__cpuid;

    // CPUID.(EAX=1):EDX.PAT is bit 16.
    if __cpuid(1).edx & (1 << 16) == 0 {
        return false;
    }
    // SAFETY: (SYSREG) CPUID reported the PAT, and every entry of the value is
    // a memory type the SDM defines (0x00, 0x01, 0x04, 0x06, 0x07).
    unsafe { write_msr(IA32_PAT, ferrix_paging::X86_PAT) };
    flush_tlb_including_global();
    // SAFETY: (SYSREG) the register exists, as CPUID said.
    unsafe { read_msr(IA32_PAT) == ferrix_paging::X86_PAT }
}

/// Read a model-specific register.
///
/// # Safety
///
/// (SYSREG) `msr` must be a register this CPU implements; reading one it does not is a
/// general protection fault.
pub(crate) unsafe fn read_msr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: (SYSREG) the caller guarantees the register exists.
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

/// Control register 0: protection, paging, write protection, caching.
pub(crate) fn read_cr0() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading CR0 has no side effects.
    unsafe {
        asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Control register 4: the paging and instruction-set extensions in force.
pub(crate) fn read_cr4() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading CR4 has no side effects.
    unsafe {
        asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Write control register 4.
///
/// # Safety
///
/// (SYSREG) Every bit set must be a feature this processor has and a mode it can be in
/// now: `PCIDE` outside long mode, or `LA57` under a four-level table, is a
/// fault at best.
pub(crate) unsafe fn write_cr4(value: u64) {
    // SAFETY: (SYSREG) the caller guarantees the value.
    unsafe {
        asm!("mov cr4, {}", in(reg) value, options(nostack, preserves_flags));
    }
}

/// Switch to the page tables rooted at `root`.
///
/// # Safety
///
/// (TRANSLATE) `root` must map everything this processor touches next — the code it is
/// executing, its stack — at the addresses it is using them at.
pub(crate) unsafe fn write_cr3(root: u64) {
    // SAFETY: (TRANSLATE) the caller guarantees the new tables map what runs next.
    unsafe {
        asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
    }
}

/// The word at offset zero from `GS`'s base.
///
/// # Safety
///
/// (SHARED) `GS`'s base must point at readable memory. Until the kernel writes it, it
/// points wherever firmware left it.
pub(crate) unsafe fn read_gs_word() -> u64 {
    let word: u64;
    // SAFETY: (SHARED) the caller guarantees the base is readable.
    unsafe {
        asm!(
            "mov {}, qword ptr gs:[0]",
            out(reg) word,
            options(readonly, nostack, preserves_flags),
        );
    }
    word
}

/// The word `offset` bytes into `GS`'s base, in one instruction.
///
/// One instruction is the point: the processor finds the record and reads it
/// as one, so an interrupt, and with it a migration, comes before the read or
/// after it, never between finding this processor's record and reading it.
///
/// # Safety
///
/// (SHARED) `GS`'s base must be this processor's per-CPU record and `offset`
/// the offset of an aligned `u64` inside it.
#[inline(always)]
pub(crate) unsafe fn read_gs_at(offset: usize) -> u64 {
    let word: u64;
    // SAFETY: (SHARED) the caller guarantees the address is an aligned word of
    // this processor's record, which lives for the life of the system.
    unsafe {
        asm!(
            "mov {word}, qword ptr gs:[{offset}]",
            offset = in(reg) offset, word = out(reg) word,
            options(readonly, nostack, preserves_flags),
        );
    }
    word
}

/// Add `delta` to the word `offset` bytes into `GS`'s base and return what it
/// held: one `xadd`, without a `lock` prefix.
///
/// One instruction, for [`read_gs_at`]'s reason: an interrupt is taken before
/// it or after it, and no migration can come between finding the record and
/// changing it. No `lock`, because only this processor writes the word: the
/// prefix orders an update against other processors' writes, and there are
/// none. Another processor reading it sees it before or after, since an
/// aligned eight-byte access is single-copy atomic.
///
/// # Safety
///
/// (SHARED) `GS`'s base must be this processor's per-CPU record and `offset`
/// the offset of an aligned `u64` inside it that no other processor writes.
#[inline(always)]
pub(crate) unsafe fn gs_xadd(offset: usize, delta: u64) -> u64 {
    let mut value = delta;
    // SAFETY: (SHARED) the caller guarantees the address is an aligned word of
    // this processor's record that only this processor writes.
    unsafe {
        asm!(
            "xadd qword ptr gs:[{offset}], {value}",
            offset = in(reg) offset, value = inout(reg) value,
            options(nostack),
        );
    }
    value
}

/// This function's frame pointer.
///
/// With `force-frame-pointers` on (see `.cargo/config.toml`) this register
/// holds the address of this function's frame record: the caller's frame
/// pointer, and the address it will return to. Walking that chain is how a
/// panic reports who called what.
///
/// Always inlined: read out of line, it would name this function's own frame,
/// which has been left by the time anything walks it — and which the walk
/// then reuses for its own frames.
#[inline(always)]
pub(crate) fn frame_pointer() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading a register has no side effects.
    unsafe {
        asm!("mov {}, rbp", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// QEMU's `isa-debug-exit` device.
///
/// Writing to it ends the emulator with `(value << 1) | 1`, which is how the
/// boot test distinguishes a kernel that finished from one that was killed on a
/// timeout. On real hardware the port is unused and the write does nothing,
/// which is why [`super::shutdown`] halts afterwards rather than assuming.
const DEBUG_EXIT_PORT: u16 = 0xF4;

/// The value that makes QEMU exit 33.
const DEBUG_EXIT_SUCCESS: u32 = 0x10;

/// Ask QEMU to exit successfully. Does nothing on real hardware.
pub(crate) fn debug_exit() {
    // SAFETY: (FIRMWARE) on QEMU this port is the debug-exit device and this is exactly
    // what it is for; on hardware, port 0xF4 is unassigned and a write to an
    // unassigned port is discarded.
    unsafe { outl(DEBUG_EXIT_PORT, DEBUG_EXIT_SUCCESS) };
}

// ---------------------------------------------------------------------------
// Descriptor tables
// ---------------------------------------------------------------------------

/// Load the global descriptor table.
///
/// # Safety
///
/// (ENTRY) `pointer` must be the address of a `limit`/`base` pair describing a valid
/// GDT that stays alive for as long as it is loaded. The CPU keeps using it for
/// every privilege transition, so a GDT on a stack that goes away is a fault
/// with no obvious cause.
pub(crate) unsafe fn load_gdt(pointer: u64) {
    // SAFETY: (ENTRY) the caller guarantees the operand describes a live, valid table.
    unsafe {
        asm!("lgdt [{}]", in(reg) pointer, options(readonly, nostack, preserves_flags));
    }
}

/// Load the interrupt descriptor table.
///
/// # Safety
///
/// (ENTRY) As [`load_gdt`], for an IDT whose every present gate points at real code.
pub(crate) unsafe fn load_idt(pointer: u64) {
    // SAFETY: (ENTRY) the caller guarantees the operand describes a live, valid table.
    unsafe {
        asm!("lidt [{}]", in(reg) pointer, options(readonly, nostack, preserves_flags));
    }
}

/// Reset the processor the hard way: load an interrupt descriptor table with no
/// entries and raise an exception. Delivering it needs a gate there is not, the
/// double fault that follows needs one too, and a third fault resets.
///
/// # Safety
///
/// (FIRMWARE) Destroys the machine's state on purpose. Only the last step of a reset may
/// call it.
pub(crate) unsafe fn triple_fault() -> ! {
    // An `lidt` operand: a limit of zero and a base of zero, so no gate exists.
    let empty = [0_u8; 10];
    // SAFETY: (FIRMWARE) an empty table is exactly what this function is for, and the
    // caller has given up on the machine.
    unsafe { load_idt(empty.as_ptr() as u64) };
    // SAFETY: (FIRMWARE) with no gate for it, this breakpoint becomes a double fault and
    // then a triple fault, which resets the processor.
    unsafe { asm!("int3", options(noreturn)) }
}

/// Load the task register.
///
/// # Safety
///
/// (ENTRY) `selector` must name an available 64-bit TSS descriptor in the current GDT.
/// Loading one that is already busy, or that is not a TSS at all, is a general
/// protection fault.
pub(crate) unsafe fn load_tss(selector: u16) {
    // SAFETY: (ENTRY) the caller guarantees the selector names an available TSS.
    unsafe {
        asm!("ltr {0:x}", in(reg) selector, options(nostack, preserves_flags));
    }
}

/// Reload every segment register from the new GDT.
///
/// Necessary because `lgdt` does not touch the hidden descriptor caches: after
/// it, the CPU is still running on the *old* segment descriptors, and the first
/// interrupt to reload `CS` from the new table would fault.
///
/// # Safety
///
/// (ENTRY) `code` and `data` must name a 64-bit code segment and a writable data
/// segment in the currently loaded GDT.
pub(crate) unsafe fn reload_segments(code: u16, data: u16) {
    // SAFETY: (ENTRY) the caller guarantees both selectors are valid in the live GDT.
    // The far return is the only way to reload CS: there is no `mov cs`.
    unsafe {
        asm!(
            "push {code}",
            "lea {scratch}, [rip + 55f]",
            "push {scratch}",
            "retfq",
            "55:",
            "mov ds, {data:e}",
            "mov es, {data:e}",
            "mov ss, {data:e}",
            code = in(reg) u64::from(code),
            data = in(reg) u32::from(data),
            scratch = lateout(reg) _,
            options(preserves_flags),
        );
    }
}

/// The four data segment selectors this processor holds: `DS`, `ES`, `FS`
/// and `GS`, in that order.
///
/// In the kernel these are still the program's: nothing on the way in from
/// ring 3 loads them, and long mode's kernel never uses them. They matter only
/// to a 32-bit program, which addresses through all four (`docs/I386.md`
/// §3.5), and a switch between tasks keeps each task's own.
pub(crate) fn read_data_selectors() -> [u16; 4] {
    let (ds, es, fs, gs): (u16, u16, u16, u16);
    // SAFETY: (SYSREG) reading a segment register has no side effect.
    unsafe {
        asm!(
            "mov {0:x}, ds",
            "mov {1:x}, es",
            "mov {2:x}, fs",
            "mov {3:x}, gs",
            out(reg) ds,
            out(reg) es,
            out(reg) fs,
            out(reg) gs,
            options(nomem, nostack, preserves_flags),
        );
    }
    [ds, es, fs, gs]
}

/// Load `DS` alone: selector and, from the GDT, the hidden part.
///
/// # Safety
///
/// (CONTEXT) As [`load_data_selectors`], for `selector`.
pub(crate) unsafe fn load_ds(selector: u16) {
    // SAFETY: (CONTEXT) the caller guarantees the selector loads.
    unsafe {
        asm!("mov ds, {0:e}", in(reg) u32::from(selector), options(nostack, preserves_flags));
    }
}

/// Load `ES` alone: selector and, from the GDT, the hidden part.
///
/// # Safety
///
/// (CONTEXT) As [`load_data_selectors`], for `selector`.
pub(crate) unsafe fn load_es(selector: u16) {
    // SAFETY: (CONTEXT) the caller guarantees the selector loads.
    unsafe {
        asm!("mov es, {0:e}", in(reg) u32::from(selector), options(nostack, preserves_flags));
    }
}

/// Load `FS` alone: selector and, from the GDT, the hidden base, which a
/// null selector may clear (see [`load_data_selectors`]).
///
/// # Safety
///
/// (CONTEXT) As [`load_data_selectors`], for `selector`.
pub(crate) unsafe fn load_fs(selector: u16) {
    // SAFETY: (CONTEXT) the caller guarantees the selector loads.
    unsafe {
        asm!("mov fs, {0:e}", in(reg) u32::from(selector), options(nostack, preserves_flags));
    }
}

/// Load the null selector into `FS` and `GS`: once per processor at bring-up,
/// before its per-CPU `GS_BASE` is written, so that from then on a `FS` or
/// `GS` reading 0 was loaded with 0 (`docs/OPAQUE-KERNEL.md` §9.8 3c, A5).
/// `GS` goes through [`load_user_gs`]'s `swapgs` pair, so the null load acts
/// on `KERNEL_GS_BASE`, which the first switch to a program writes, and
/// `GS_BASE` comes back as it was, to be written by the caller after.
///
/// # Safety
///
/// (CONTEXT) Only before this processor's `GS_BASE` holds its per-CPU record,
/// with nothing yet relying on either base.
pub(crate) unsafe fn load_null_fs_gs() {
    // SAFETY: (CONTEXT) the null selector always loads in long mode.
    unsafe { load_fs(0) };
    // SAFETY: (CONTEXT) as for `FS`; the caller guarantees nothing relies on
    // either `GS` base yet.
    unsafe { load_user_gs(0) };
}

/// Load `DS`, `ES` and `FS` for a program: selector and, from the GDT, the
/// hidden base and limit.
///
/// Loading `FS` replaces `FS_BASE` with the descriptor's base, or, for a null
/// selector, may clear it; a caller that wants a 64-bit program's thread
/// pointer back writes the MSR after this.
///
/// # Safety
///
/// (CONTEXT) Each selector must be null or name a present data segment, or a readable
/// code segment, in this processor's GDT whose DPL admits it. Anything else is
/// `#GP` in ring 0.
pub(crate) unsafe fn load_data_selectors(ds: u16, es: u16, fs: u16) {
    // SAFETY: (CONTEXT) the caller guarantees each selector loads; one load
    // a register, which the switch's `DS`/`ES` skip takes one at a time.
    unsafe { load_ds(ds) };
    // SAFETY: (CONTEXT) as for `DS`.
    unsafe { load_es(es) };
    // SAFETY: (CONTEXT) as for `DS`.
    unsafe { load_fs(fs) };
}

/// Load `GS` for a program without disturbing the kernel's own `GS` base.
///
/// A `GS` load replaces `GS_BASE`, which in the kernel is the per-CPU record.
/// So the load happens between two `swapgs`, and it is the program's base --
/// parked in `KERNEL_GS_BASE` while the kernel runs -- that the descriptor
/// replaces: Linux's `load_gs_index`. Interrupts are masked for the three
/// instructions, since a handler entered between the two `swapgs` would find
/// the program's base where the kernel's belongs; an NMI there is the
/// paranoid entry's, which asks the MSR rather than trusting the order.
/// Both `swapgs` always run, in the kernel with its own `GS`, so there is no
/// skipped swap to mispredict and F-31's `lfence` after an entry's `swapgs`
/// does not apply here.
///
/// # Safety
///
/// (CONTEXT) As [`load_data_selectors`], for `selector`.
pub(crate) unsafe fn load_user_gs(selector: u16) {
    let open = read_rflags() & RFLAGS_IF != 0;
    disable_interrupts();
    // SAFETY: (CONTEXT) the caller guarantees the selector loads; interrupts are masked
    // between the two `swapgs`, which leave `GS_BASE` the kernel's again.
    unsafe {
        asm!(
            "swapgs",
            "mov gs, {0:e}",
            "swapgs",
            in(reg) u32::from(selector),
            options(nostack, preserves_flags),
        );
    }
    if open {
        enable_interrupts();
    }
}

/// `RFLAGS.IF`: interrupts are unmasked.
const RFLAGS_IF: u64 = 1 << 9;

/// The linear address a page fault was taken on./// The linear address a page fault was taken on.
pub(crate) fn read_cr2() -> u64 {
    let address: u64;
    // SAFETY: (SYSREG) reading CR2 has no side effects. It is only meaningful inside a
    // page fault handler, before another fault overwrites it.
    unsafe {
        asm!("mov {}, cr2", out(reg) address, options(nomem, nostack, preserves_flags));
    }
    address
}

/// The current stack pointer.
///
/// Used to give the task state segment a valid `RSP0` at boot, so that the
/// first trap arriving from ring 3 has somewhere to land even before the
/// scheduler starts replacing it per task.
pub(crate) fn read_stack_pointer() -> u64 {
    let stack: u64;
    // SAFETY: (SYSREG) reading `rsp` has no side effects.
    unsafe {
        asm!("mov {}, rsp", out(reg) stack, options(nomem, nostack, preserves_flags));
    }
    stack
}

/// Unmask interrupts on this CPU.
///
/// The counterpart to [`disable_interrupts`], and the moment the kernel stops
/// being the only thing that decides when it runs.
pub(crate) fn enable_interrupts() {
    // SAFETY: (SYSREG) `sti` only sets the interrupt flag. Every vector has a gate by
    // the time anything calls this — `init_traps` runs long before.
    unsafe {
        asm!("sti", options(nomem, nostack));
    }
}

/// Unmask interrupts and halt until one arrives, with no gap between the two.
///
/// `sti` takes effect only after the instruction that follows it, so an
/// interrupt that became pending while they were masked is delivered to the
/// `hlt` — waking it — rather than slipping in between and leaving the
/// processor asleep with the reason it should be awake already handled.
pub(crate) fn enable_interrupts_and_halt() {
    // SAFETY: (SYSREG) `sti` and `hlt` change only the interrupt flag and whether the
    // processor is running; every vector has a gate by the time this is used.
    unsafe {
        asm!("sti", "hlt", options(nomem, nostack));
    }
}

/// The time-stamp counter.
///
/// Counts core clock cycles on every CPU since the Pentium, and at a constant
/// rate independent of frequency scaling on anything since Nehalem. Nothing
/// reports that rate, which is why [`super::clock`] measures it.
pub(crate) fn rdtsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: (SYSREG) `rdtsc` reads a counter into edx:eax and has no other effect.
    unsafe {
        asm!(
            "rdtsc",
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}

// ---------------------------------------------------------------------------
// Debug registers
// ---------------------------------------------------------------------------

/// Point hardware breakpoint `slot`, 0 to 3, at `address`. Arming it is
/// [`write_dr7`]'s. A slot above 3 is ignored: there are four.
///
/// # Safety
///
/// (SYSREG) Changes which accesses raise `#DB` once `DR7` enables the slot. The kernel
/// must be ready to take one there: not on the entry stubs, and not on an
/// interrupt stack, which a nested `#DB` would land on.
pub(crate) unsafe fn write_breakpoint_address(slot: usize, address: u64) {
    // Each: writing a debug address register at ring 0 changes nothing until
    // DR7 enables it, and the caller guarantees the address is one to trap on.
    match slot {
        // SAFETY: (SYSREG) as above, for DR0.
        0 => unsafe {
            asm!("mov dr0, {}", in(reg) address, options(nomem, nostack, preserves_flags));
        },
        // SAFETY: (SYSREG) as above, for DR1.
        1 => unsafe {
            asm!("mov dr1, {}", in(reg) address, options(nomem, nostack, preserves_flags));
        },
        // SAFETY: (SYSREG) as above, for DR2.
        2 => unsafe {
            asm!("mov dr2, {}", in(reg) address, options(nomem, nostack, preserves_flags));
        },
        // SAFETY: (SYSREG) as above, for DR3.
        3 => unsafe {
            asm!("mov dr3, {}", in(reg) address, options(nomem, nostack, preserves_flags));
        },
        _ => {}
    }
}

/// Debug control: which of the four breakpoints are armed, and on what.
///
/// # Safety
///
/// (SYSREG) As [`write_breakpoint_address`], for every slot `value` enables.
pub(crate) unsafe fn write_dr7(value: u64) {
    // SAFETY: (SYSREG) the caller guarantees each enabled slot's address.
    unsafe {
        asm!("mov dr7, {}", in(reg) value, options(nomem, nostack, preserves_flags));
    }
}

/// Debug status: which condition raised the last `#DB`. Sticky: the processor
/// sets bits and never clears them, so a handler clears it with [`write_dr6`].
pub(crate) fn read_dr6() -> u64 {
    let value: u64;
    // SAFETY: (SYSREG) reading a debug register at ring 0 has no side effects.
    unsafe {
        asm!("mov {}, dr6", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Write the debug status register.
///
/// # Safety
///
/// (SYSREG) `value` must keep DR6's reserved bits as the processor defines them:
/// bits 63:32 clear, and the fixed-one bits set.
pub(crate) unsafe fn write_dr6(value: u64) {
    // SAFETY: (SYSREG) the caller guarantees the reserved bits.
    unsafe {
        asm!("mov dr6, {}", in(reg) value, options(nomem, nostack, preserves_flags));
    }
}

/// Read this processor's task register: the selector of its loaded TSS.
///
/// # Safety
///
/// (ENTRY) None beyond being x86-64; `STR` touches no memory.
pub(crate) unsafe fn read_task_register() -> u16 {
    let selector: u16;
    // SAFETY: (ENTRY) reads a register into a local.
    unsafe {
        core::arch::asm!(
            "str {0:x}",
            out(reg) selector,
            options(nomem, nostack, preserves_flags),
        );
    }
    selector
}

/// Read this processor's GDT base and limit.
///
/// # Safety
///
/// (ENTRY) None beyond being x86-64; `SGDT` writes ten bytes to the local below.
pub(crate) unsafe fn read_gdt() -> (u64, u16) {
    let mut pointer = [0_u8; 10];
    // SAFETY: (ENTRY) `SGDT` writes exactly ten bytes, which is the size of the array.
    unsafe {
        core::arch::asm!(
            "sgdt [{0}]",
            in(reg) pointer.as_mut_ptr(),
            options(nostack, preserves_flags),
        );
    }
    let limit = u16::from_le_bytes([pointer[0], pointer[1]]);
    let base = u64::from_le_bytes([
        pointer[2], pointer[3], pointer[4], pointer[5], pointer[6], pointer[7], pointer[8],
        pointer[9],
    ]);
    (base, limit)
}

/// A 64-bit random number from the CPU: `RDSEED`, or `RDRAND` on a CPU that
/// has only that, each tried ten times as Intel's guidance says; `None` on a
/// CPU with neither, or one that kept reporting failure.
pub(crate) fn hardware_random() -> Option<u64> {
    use core::arch::x86_64::{__cpuid, __cpuid_count};
    let rdseed = __cpuid(0).eax >= 7 && __cpuid_count(7, 0).ebx & (1 << 18) != 0;
    let rdrand = __cpuid(1).ecx & (1 << 30) != 0;
    if !rdseed && !rdrand {
        return None;
    }
    for _ in 0..10 {
        let value: u64;
        let carry: u8;
        if rdseed {
            // SAFETY: (SYSREG) CPUID says `RDSEED` exists; it sets one register and the
            // carry flag, which says whether the value is good.
            unsafe {
                core::arch::asm!(
                    "rdseed {value}",
                    "setc {carry}",
                    value = out(reg) value,
                    carry = out(reg_byte) carry,
                    options(nomem, nostack),
                );
            }
        } else {
            // SAFETY: (SYSREG) as above, for `RDRAND`.
            unsafe {
                core::arch::asm!(
                    "rdrand {value}",
                    "setc {carry}",
                    value = out(reg) value,
                    carry = out(reg_byte) carry,
                    options(nomem, nostack),
                );
            }
        }
        if carry == 1 {
            return Some(value);
        }
    }
    None
}

/// Order this processor's accesses to memory a device reads or writes by
/// DMA against each other, as the device observes them. F-44.
///
/// The compiler's reordering is all there is to stop. x86-64 keeps stores in
/// order with stores and loads with loads (TSO), device DMA snoops the caches,
/// and a virtqueue needs exactly those two: descriptors before the index that
/// publishes them, an index read before the entry it counts. A write to a
/// register through an uncached mapping is ordered after earlier stores too.
/// Linux's `dma_wmb` and `dma_rmb` are the same compiler barrier here.
///
/// **It does not order an earlier store before a later load**, which TSO lets
/// pass. The item's queue needs no such order: it rings the doorbell after
/// every publish and never reads `used.flags` or `avail_event` after one.
/// Notification suppression or `VIRTIO_F_EVENT_IDX` in the item would need a
/// full fence there, as Linux's `virtio_mb` is.
pub(crate) fn dma_barrier() {
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
}

/// Write the cache lines covering `start..start + len` back to memory, and
/// wait until they are there.
///
/// For an IOMMU whose table walk does not snoop the caches -- VT-d's
/// `ECAP.C` clear -- which reads its root, context and second-level entries
/// from memory, past every processor's cache (finding F-58). `CLFLUSH` writes
/// one line back and drops it, by virtual address, and is ordered with every
/// earlier store to that line. `MFENCE` then waits for every flush before it,
/// so the register write that publishes the entries to the unit comes after
/// they are in memory, as the SDM's description of `CLFLUSH` and Linux's
/// `clflush_cache_range` both have it.
///
/// The line size is `CPUID` leaf 1's `CLFLUSH` size, in eight-byte units,
/// and 64 bytes if it reports none, read once ([`clflush_line`]): `CPUID` is
/// a VM exit under KVM, and this runs on every map and unmap a unit that
/// does not snoop makes.
pub(crate) fn clean_for_walker(start: u64, len: u64) {
    let line = clflush_line();
    let mut at = start - start % line;
    let end = start.saturating_add(len);
    while at < end {
        // SAFETY: (SYSREG) `clflush` writes one line back to memory and drops it
        // from the caches; it changes no data, and the caller's range is mapped.
        unsafe { asm!("clflush [{}]", in(reg) at, options(nostack, preserves_flags)) };
        at += line;
    }
    // SAFETY: (SYSREG) a barrier, completing every flush above before any later
    // store -- in particular the register write that publishes the entries.
    unsafe { asm!("mfence", options(nostack, preserves_flags)) };
}

/// The `CLFLUSH` line size, once `CPUID` has been asked; zero until then.
static CLFLUSH_LINE: AtomicU64 = AtomicU64::new(0);

/// The bytes one `CLFLUSH` covers, asked of `CPUID` the first time only. Two
/// processors asking at once both store the same answer.
fn clflush_line() -> u64 {
    let known = CLFLUSH_LINE.load(Ordering::Relaxed);
    if known != 0 {
        return known;
    }
    let reported = u64::from((core::arch::x86_64::__cpuid(1).ebx >> 8) & 0xFF) * 8;
    let line = if reported == 0 { 64 } else { reported };
    CLFLUSH_LINE.store(line, Ordering::Relaxed);
    line
}
