//! Starting the other processors on x86-64.
//!
//! # Why there is 16-bit assembly here
//!
//! An application processor comes out of reset in **16-bit real mode**, at
//! `CS:IP = (vector << 12):0` where the vector is the byte the startup
//! inter-processor interrupt carried. It has no page tables, no long mode, a
//! segment register for a code base and a byte of address space to be told
//! where to start. Rust has no 16-bit real-mode target and will not be getting
//! one, so the sequence that walks a processor from there to a 64-bit Rust
//! function is the only 16-bit code in this project — and once it has run,
//! nothing on this architecture is ever in 16-bit mode again.
//!
//! The sequence itself is the shortest one the architecture allows: set
//! `CR4.PAE`, load `CR3`, set `EFER.LME`, set `CR0.PG|PE` — which enters long
//! mode with a 16-bit code segment still cached — and immediately far jump
//! through a 64-bit descriptor. The alternative, going by way of 32-bit
//! protected mode, needs a second descriptor and a second far jump to arrive
//! in the same place.
//!
//! # Why the page it runs on was reserved at boot
//!
//! The startup vector is a byte, so the page has to be below 1 MiB, and by the
//! time processors are started the frame allocator has been handing out memory
//! for a while. [`crate::mm`] therefore sets one page aside before the
//! allocator is given anything — see `mm::startup_page`.
//!
//! # What the trampoline does not have to do
//!
//! Build page tables. The loader's identity map is still live at this point in
//! boot, deliberately: `kmain` starts the other processors *before*
//! `finish_memory` drops it. So the physical address the trampoline executes at
//! is already mapped in the very tables it installs, which is what makes the
//! instruction after `mov %cr0` fetchable.

use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_acpi::MadtEntry;
use ferrix_bootinfo::BootView;

use super::apic;
use super::cpu;
use super::gdt;
use super::trap;
use crate::smp::{Ipi, Target};

core::arch::global_asm!(
    r#"
.section .text
.globl ferrix_ap_start
.globl ferrix_ap_long
.globl ferrix_ap_gdt
.globl ferrix_ap_gdtr
.globl ferrix_ap_jump
.globl ferrix_ap_cr3
.globl ferrix_ap_stack
.globl ferrix_ap_rust
.globl ferrix_ap_end
.align 16
.code16
ferrix_ap_start:
    cli
    cld
    movw %cs, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw $(ferrix_ap_gdtr - ferrix_ap_start), %si
    lgdtl (%si)
    movl %cr4, %eax
    orl $0xA0, %eax
    movl %eax, %cr4
    movw $(ferrix_ap_cr3 - ferrix_ap_start), %si
    movl (%si), %eax
    movl %eax, %cr3
    movl $0xC0000080, %ecx
    rdmsr
    orl $0x900, %eax
    wrmsr
    movw $(ferrix_ap_jump - ferrix_ap_start), %si
    movl %cr0, %eax
    orl $0x80000001, %eax
    movl %eax, %cr0
    ljmpl *(%si)
.code64
ferrix_ap_long:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs
    movq ferrix_ap_stack(%rip), %rsp
    xorq %rbp, %rbp
    pushq $0
    movq ferrix_ap_rust(%rip), %rax
    jmpq *%rax
.align 8
ferrix_ap_gdt:
    .quad 0
    .quad 0x00AF9A000000FFFF
    .quad 0x00CF92000000FFFF
ferrix_ap_gdtr:
    .word 23
    .long 0
ferrix_ap_jump:
    .long 0
    .word 8
.align 8
ferrix_ap_cr3:
    .quad 0
ferrix_ap_stack:
    .quad 0
ferrix_ap_rust:
    .quad 0
ferrix_ap_end:
"#,
    options(att_syntax)
);

unsafe extern "C" {
    /// First byte of the trampoline, and the address a processor starts at.
    static ferrix_ap_start: u8;
    /// The 64-bit half, which the far jump has to be told the address of.
    static ferrix_ap_long: u8;
    /// The three descriptors the trampoline runs on: null, 64-bit code, data.
    static ferrix_ap_gdt: u8;
    /// The `lgdt` operand: a limit and a base that has to be patched.
    static ferrix_ap_gdtr: u8;
    /// The far jump's operand: a 32-bit offset and a selector.
    static ferrix_ap_jump: u8;
    /// The page table root the processor is to run on.
    static ferrix_ap_cr3: u8;
    /// The stack it is to run on.
    static ferrix_ap_stack: u8;
    /// The Rust function it is to enter.
    static ferrix_ap_rust: u8;
    /// One past the last byte.
    static ferrix_ap_end: u8;
}

/// Offset of `symbol` within the trampoline.
///
/// The 16-bit half addresses its own data as an offset from the start of the
/// page, because a real-mode segment base *is* that page; the patching below
/// needs the same offsets from the other side.
fn offset_of(symbol: &u8) -> usize {
    (symbol as *const u8 as usize).saturating_sub((&raw const ferrix_ap_start) as usize)
}

/// Bytes the trampoline occupies.
fn trampoline_len() -> usize {
    offset_of(&ferrix_ap_end)
}

/// Where the trampoline was installed, physically.
static TRAMPOLINE: AtomicU64 = AtomicU64::new(0);

/// Tell `found` about every processor firmware describes as startable.
///
/// Includes this one. The caller knows which it is, and a list that left it out
/// would be a list whose length is not the processor count.
pub(crate) fn enumerate_cpus(view: &BootView<'_>, mut found: impl FnMut(u64)) {
    let Ok(firmware) = crate::acpi::Firmware::open(view) else {
        return;
    };
    let Ok(madt) = firmware.acpi().madt() else {
        return;
    };

    for entry in madt.entries() {
        match entry {
            MadtEntry::LocalApic(cpu) if cpu.is_enabled() => found(u64::from(cpu.apic_id)),
            MadtEntry::LocalX2Apic(cpu) if cpu.is_enabled() => found(u64::from(cpu.x2apic_id)),
            _ => {}
        }
    }
}

/// Copy the trampoline into the page reserved for it and patch it.
///
/// # Safety
///
/// Must be called while the loader's identity map is still live, and only from
/// the CPU doing the starting: the page holds one processor's stack pointer at
/// a time.
unsafe fn install_trampoline() -> Result<u64, &'static str> {
    let phys = crate::mm::startup_page().ok_or("no page below 1 MiB to start a processor on")?;
    let len = trampoline_len();
    if len > ferrix_bootinfo::PAGE_SIZE as usize {
        return Err("the startup trampoline does not fit in a page");
    }

    let at = crate::mm::direct_map(phys);
    // SAFETY: the trampoline is a run of bytes in this image's `.text`, and
    // `at` is a page the frame allocator was never given, reachable through the
    // direct map. They cannot overlap: one is in the kernel image and the other
    // is below 1 MiB.
    unsafe { core::ptr::copy_nonoverlapping(&raw const ferrix_ap_start, at as *mut u8, len) };

    // Everything the 16-bit half cannot compute for itself. A real-mode segment
    // gives it offsets within the page; what it needs are linear addresses, and
    // it has no way to turn one into the other before the MMU is on.
    let patch32 = |offset: usize, value: u32| {
        // SAFETY: `offset` is inside the copy made above, which is one page of
        // memory nothing else refers to.
        unsafe { core::ptr::write_unaligned((at as usize + offset) as *mut u32, value) };
    };
    let patch64 = |offset: usize, value: u64| {
        // SAFETY: as `patch32`.
        unsafe { core::ptr::write_unaligned((at as usize + offset) as *mut u64, value) };
    };

    // The GDT pointer's base, two bytes into the six-byte operand.
    patch32(
        offset_of(&ferrix_ap_gdtr) + 2,
        (phys + offset_of(&ferrix_ap_gdt) as u64) as u32,
    );
    // The far jump's 32-bit offset.
    patch32(
        offset_of(&ferrix_ap_jump),
        (phys + offset_of(&ferrix_ap_long) as u64) as u32,
    );
    patch64(offset_of(&ferrix_ap_cr3), crate::mm::root_table());
    patch64(offset_of(&ferrix_ap_rust), ap_entry as usize as u64);

    TRAMPOLINE.store(phys, Ordering::Release);
    Ok(phys)
}

/// Start the processor the interrupt controller calls `hardware_id`.
///
/// The protocol is fixed by the architecture and is not negotiable: an `INIT`
/// inter-processor interrupt to reset the target, then two startup interrupts
/// carrying the page to begin at. Two because the first is permitted to be lost
/// on some parts, and a second one arriving at a processor that already
/// started is ignored.
///
/// # Safety
///
/// `stack_top` must be the top of a stack nothing else will run on, and the
/// caller must have published the [`crate::percpu::PerCpu`] record this
/// processor is to install.
pub(crate) unsafe fn start_cpu(hardware_id: u64, stack_top: u64) -> Result<(), &'static str> {
    let phys = match TRAMPOLINE.load(Ordering::Acquire) {
        0 => {
            // SAFETY: called from `smp::start_others`, which runs before the
            // identity map is dropped and is the only caller.
            unsafe { install_trampoline()? }
        }
        installed => installed,
    };

    let at = crate::mm::direct_map(phys) as usize;
    // SAFETY: inside the trampoline's own page; the processor it is for has
    // not been started yet, so nothing is reading it.
    unsafe {
        core::ptr::write_unaligned(
            (at + offset_of(&ferrix_ap_stack)) as *mut u64,
            stack_top,
        );
    }

    let vector = u8::try_from(phys / 4096).map_err(|_| "the startup page is above 1 MiB")?;

    apic::send_init(hardware_id)?;
    spin_nanos(10_000_000);
    apic::send_startup(hardware_id, vector)?;
    spin_nanos(200_000);
    apic::send_startup(hardware_id, vector)?;
    Ok(())
}

/// Wait for `nanos` on the counter.
///
/// The counter rather than a loop count: the startup protocol's delays are in
/// microseconds of real time, and a spin count measures how fast the CPU doing
/// the waiting is.
fn spin_nanos(nanos: u64) {
    let until = crate::timer::now_nanos().saturating_add(nanos);
    while crate::timer::now_nanos() < until {
        core::hint::spin_loop();
    }
}

/// Where a freshly started processor arrives from the trampoline.
///
/// Entered with a stack, the kernel's page tables and nothing else: the
/// descriptor tables it is running on belong to the trampoline and are about to
/// be reused by the next processor started, so the first thing it does is get
/// off them.
extern "C" fn ap_entry() -> ! {
    // Before the allocation `init_secondary` makes: allocating asks which CPU
    // this is, and that question is a `%gs`-relative load.
    cpu::park_gs_base();
    // SAFETY: this is the first Rust to run on this processor, exactly once,
    // with interrupts masked by the trampoline's `cli`.
    unsafe { gdt::init_secondary() };
    // SAFETY: after `gdt::init_secondary`, whose kernel code selector every
    // gate in the table names.
    unsafe { trap::load() };

    crate::smp::ap_start()
}

/// Bring up this processor's local APIC.
///
/// # Safety
///
/// Must be called once per processor, after its interrupt descriptor table is
/// loaded and while interrupts are masked.
pub(crate) unsafe fn init_cpu_interrupts() -> Result<(), &'static str> {
    // SAFETY: the caller's contract.
    unsafe { apic::init_this_cpu() }
}

/// The interrupt number an inter-processor interrupt arrives as.
pub(crate) const fn ipi_irq(ipi: Ipi) -> u32 {
    (apic::ipi_vector(ipi) - trap::IRQ_BASE) as u32
}

/// Nothing: x86-64 has no per-interrupt mask for an inter-processor interrupt.
///
/// The local APIC delivers whatever vector the sender named, and the task
/// priority register — which `apic::init_this_cpu` drops to zero — is the only
/// thing that could refuse it.
pub(crate) const fn unmask_ipi(_ipi: Ipi) {}

/// Send `ipi` to `target`.
pub(crate) fn send_ipi(target: Target, ipi: Ipi) {
    apic::send_ipi(target, ipi);
}

/// Where this CPU's per-CPU record is, or zero before one is installed.
///
/// Read through `%gs`, which is the whole reason the record keeps a pointer to
/// itself at offset zero: the segment base is not in a general register, so
/// there is no way to ask the CPU for it in one instruction — but there is a
/// way to load from it.
///
/// The base is never left as zero: [`super::init_traps`] points it at a
/// quadword of zeroes before anything can read it, so this reads a mapped
/// address and reports "no record" rather than faulting.
pub(crate) fn current_percpu() -> usize {
    cpu::read_gs_base_slot()
}

/// Point `%gs` at this CPU's record.
///
/// # Safety
///
/// `address` must be a `PerCpu` that lives for the rest of the system, whose
/// first field is its own address.
pub(crate) unsafe fn install_percpu(address: usize) {
    // SAFETY: the caller guarantees the record; `IA32_GS_BASE` is a segment
    // base and changing it affects only `%gs`-relative accesses, which are made
    // nowhere but `current_percpu`.
    unsafe { cpu::write_gs_base(address as u64) };
}
