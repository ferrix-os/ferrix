//! Processors on x86-64.
//!
//! Every processor has a local APIC, and the MADT lists them: type 0 for an
//! APIC identifier below 255, type 9 — the x2APIC entry — for the rest. The
//! identifier is what an inter-processor interrupt is addressed to, so it is
//! also how the kernel names a processor when it comes to start one.
//!
//! # Starting one
//!
//! INIT–SIPI–SIPI starts an application processor in 16-bit real mode at the
//! beginning of a page below one mebibyte, whose number the start-up IPI
//! carries. The trampoline below is the only 16-bit code in the project, and
//! it does as little as it can there: load the boot processor's control
//! registers, a four-entry GDT and a root table, and far-jump straight into a
//! 64-bit segment. Everything after that is Rust.
//!
//! Two frames have to be low, and both are borrowed for bring-up and given
//! back after it. The trampoline page, because real mode reaches nothing
//! higher. And a root table, because `mov cr3` outside long mode loads 32 bits
//! of it: a copy of the kernel's upper half, plus an identity map of the
//! trampoline page, so that the instruction after paging turns on is fetched
//! from the same place as the one before it.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use ferrix_acpi::MadtEntry;
use ferrix_bootinfo::{BootView, PAGE_SIZE};
use ferrix_paging::MapFlags;

use super::{ROOT_SLOTS, UPPER_HALF_SLOT, apic, cpu};
use crate::smp::Described;

/// Every processor firmware says can be started, and which one this is.
///
/// # Errors
///
/// If the ACPI tables or the MADT cannot be read.
pub(crate) fn describe_cpus(view: &BootView<'_>) -> Result<Described, &'static str> {
    let firmware = crate::discovery::acpi::Firmware::open(view)
        .map_err(|_| "the machine has no readable ACPI tables")?;
    let acpi = firmware.acpi();
    let madt = acpi.madt().map_err(|_| "the machine has no MADT")?;

    let mut ids = Vec::new();
    for entry in madt.entries() {
        // An entry whose enabled flag is clear is a processor firmware says
        // not to use: disabled, or an empty socket that could be hot plugged
        // later. Neither can be started, and counting one would have the exit
        // test wait for a processor that will never answer.
        match entry {
            MadtEntry::LocalApic(local) if local.is_enabled() => {
                // FATAL-ALLOC: boot only: stage 4 lists the processors firmware describes, once.
                ids.push(u64::from(local.apic_id));
            }
            MadtEntry::LocalX2Apic(local) if local.is_enabled() => {
                // FATAL-ALLOC: boot only: stage 4 lists the processors firmware describes, once.
                ids.push(u64::from(local.x2apic_id));
            }
            _ => {}
        }
    }

    Ok(Described {
        id_name: "APIC ID",
        boot: hardware_id(),
        ids,
    })
}

/// This processor's hardware identifier: its local APIC ID.
pub(crate) fn hardware_id() -> u64 {
    u64::from(apic::id())
}

/// Frames real mode can reach: one mebibyte of them.
const REAL_MODE_FRAMES: u64 = 0x100;

/// `IA32_EFER`.
const IA32_EFER: u32 = 0xC000_0080;
/// `EFER.LMA`: long mode is *active*. The processor sets it; handing a value
/// with it set to a processor not yet in long mode asks for nonsense.
const EFER_LMA: u64 = 1 << 10;
/// `CR4.PCIDE`. Setting it outside long mode is a general protection fault,
/// so the trampoline leaves it out and the processor adopts it from Rust.
const CR4_PCIDE: u64 = 1 << 17;

/// The trampoline GDT's code selector. The same number as the kernel's, so an
/// interrupt gate taken before the kernel's GDT is loaded still lands in a
/// 64-bit kernel code segment. Slot 1 is left empty to put it there, as the
/// kernel's own GDT leaves Linux's 32-bit kernel code slot empty.
const TRAMPOLINE_CODE: u16 = super::gdt::KERNEL_CODE;
/// 64-bit kernel code: present, ring 0, executable, long mode — and already
/// marked accessed.
///
/// **The accessed bit is not decoration.** Loading a selector whose descriptor
/// has it clear makes the processor *write* the descriptor to set it, and this
/// GDT lives in the trampoline page, which is mapped read-only because it is
/// executable. The write is a page fault with no IDT to take it: a triple
/// fault, and a machine that resets — which is how this was found, at
/// `mov %ax, %ds`. With the bit set there is nothing to write.
const TRAMPOLINE_CODE64: u64 = 0x00AF_9B00_0000_FFFF;
/// Flat, writable kernel data, marked accessed for the same reason.
const TRAMPOLINE_DATA: u64 = 0x00CF_9300_0000_FFFF;

/// How long INIT is given before the first start-up IPI: ten milliseconds,
/// the figure Intel's multiprocessor specification gives.
const INIT_DELAY_NANOS: u64 = 10_000_000;
/// And between the two start-up IPIs: two hundred microseconds.
const STARTUP_DELAY_NANOS: u64 = 200_000;

/// The boot processor's `CR4`, which a secondary adopts once it is in long
/// mode and the bits the trampoline could not set are settable.
static BOOT_CR4: AtomicU64 = AtomicU64::new(0);

// The trampoline. Copied to a low page and entered there, so everything it
// addresses is relative: to the page through `ds` in real mode, to `rip` in
// long mode. The header it reads is filled in by `CpuStarter`, and its layout
// is `Header`'s.
//
// Real mode to long mode in one step — `CR0.PE` and `CR0.PG` set together with
// `EFER.LME` already on — rather than by way of protected mode. The far jump
// is what enters 64-bit code: until it, the processor runs the 16-bit segment
// it woke up in, through the identity map.
//
// Its `lgdtl` loads the start-up GDT before this processor has a per-CPU
// record, so no `gdt::note_tables` is owed for it (Q4): `set_cpu_local` notes
// it as zero, and `init_secondary`'s load replaces it and notes the real
// tables.
core::arch::global_asm!(
    r#"
.pushsection .rodata.ferrix_trampoline, "a"
.balign 16
.globl ferrix_trampoline
ferrix_trampoline:
.code16
    cli
    cld
    mov %cs, %ax
    mov %ax, %ds
    movl ferrix_trampoline_header + 20 - ferrix_trampoline, %eax
    movl %eax, %cr4
    movl ferrix_trampoline_header + 16 - ferrix_trampoline, %eax
    movl %eax, %cr3
    movl $0xC0000080, %ecx
    movl ferrix_trampoline_header + 28 - ferrix_trampoline, %eax
    xorl %edx, %edx
    wrmsr
    lgdtl ferrix_trampoline_header - ferrix_trampoline
    movl ferrix_trampoline_header + 24 - ferrix_trampoline, %eax
    movl %eax, %cr0
    ljmpl *ferrix_trampoline_header + 8 - ferrix_trampoline
.code64
.globl ferrix_trampoline_long
ferrix_trampoline_long:
    mov ${data}, %eax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    movq ferrix_trampoline_header + 32(%rip), %rsp
    movq ferrix_trampoline_header + 48(%rip), %rdi
    movq ferrix_trampoline_header + 40(%rip), %rax
    xorl %ebp, %ebp
    pushq $0
    jmpq *%rax
.balign 8
.globl ferrix_trampoline_header
ferrix_trampoline_header:
    .fill 88, 1, 0
.globl ferrix_trampoline_end
ferrix_trampoline_end:
.popsection
"#,
    data = const super::gdt::KERNEL_DATA,
    options(att_syntax)
);

unsafe extern "C" {
    /// The trampoline's first byte: where a start-up IPI begins executing.
    static ferrix_trampoline: [u8; 0];
    /// Its first 64-bit instruction, where the far jump goes.
    static ferrix_trampoline_long: [u8; 0];
    /// The header `CpuStarter` fills in.
    static ferrix_trampoline_header: [u8; 0];
    /// One past its last byte.
    static ferrix_trampoline_end: [u8; 0];
}

/// What the trampoline reads. Offsets are the assembly's.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Header {
    /// `lgdt`'s operand: limit, then the base in two halves. The fourth word
    /// is padding.
    gdt_pointer: [u16; 4],
    /// The far jump's operand: a 32-bit offset in two halves, then the
    /// selector. The fourth word is padding.
    far_jump: [u16; 4],
    /// The trampoline's root table, which is below 1 MiB.
    cr3: u32,
    /// The boot processor's `CR4`, less what cannot be set outside long mode.
    cr4: u32,
    /// The boot processor's `CR0`, which turns on protection and paging.
    cr0: u32,
    /// The boot processor's `EFER`, less the bit only the processor sets.
    efer: u32,
    /// The new processor's kernel stack.
    stack_top: u64,
    /// Where to go in 64-bit mode: [`secondary_start`], by virtual address.
    entry: u64,
    /// Its argument: the new processor's per-CPU record.
    argument: u64,
    /// The trampoline's GDT: null, empty, 64-bit code, data -- the kernel's
    /// selectors for both.
    gdt: [u64; 4],
}

const _: () = assert!(
    size_of::<Header>() == 88,
    "the trampoline reserves eighty-eight bytes for its header"
);
const _: () = assert!(
    super::gdt::KERNEL_CODE == 0x10 && super::gdt::KERNEL_DATA == 0x18,
    "the trampoline's GDT puts code and data in slots 2 and 3"
);

/// Offset of the GDT within the header.
const HEADER_GDT: u64 = 56;

/// Starts secondary processors, one at a time.
#[derive(Debug)]
pub(crate) struct CpuStarter {
    /// The trampoline page, by physical address.
    code: u64,
    /// The trampoline's root table, by physical address.
    root: u64,
    /// Where the header is, from the start of the page.
    header_offset: u64,
    /// The header, to be written before each start.
    header: Header,
    /// Processors started.
    started: u64,
}

impl CpuStarter {
    /// Copy the trampoline to low memory and build the tree it runs on.
    ///
    /// Runs on the boot processor, whose control registers every secondary
    /// starts with.
    pub(crate) fn new(_view: &BootView<'_>) -> Result<CpuStarter, &'static str> {
        let base = (&raw const ferrix_trampoline) as u64;
        let long_offset = (&raw const ferrix_trampoline_long) as u64 - base;
        let header_offset = (&raw const ferrix_trampoline_header) as u64 - base;
        let len = (&raw const ferrix_trampoline_end) as u64 - base;
        if len > PAGE_SIZE {
            return Err("the trampoline does not fit in a page");
        }

        let code = crate::mm::allocate_frames_below(0, REAL_MODE_FRAMES)
            .ok_or("no free frame below one mebibyte for the trampoline")?
            * PAGE_SIZE;
        let root = crate::mm::allocate_frames_below(0, REAL_MODE_FRAMES)
            .ok_or("no free frame below one mebibyte for the trampoline's root table")?
            * PAGE_SIZE;

        // SAFETY: (FRAME) `code` is a frame just allocated and referred to by nothing
        // else, writable through the direct map; the source is the
        // trampoline's `len` bytes in this image, which fit in the page.
        unsafe {
            core::ptr::copy_nonoverlapping(
                (&raw const ferrix_trampoline).cast::<u8>(),
                crate::mm::direct_map(code) as *mut u8,
                len as usize,
            );
        };

        crate::mm::zero_frame(root / PAGE_SIZE);
        // Not global: this entry belongs to a tree that exists for a few
        // milliseconds, and a global one would outlive it in a TLB.
        let flags = MapFlags {
            global: false,
            ..MapFlags::KERNEL_CODE
        };
        crate::mm::map_in(root, code, code, PAGE_SIZE, flags)
            .map_err(|_| "could not identity map the trampoline")?;

        let boot_cr4 = cpu::read_cr4();
        BOOT_CR4.store(boot_cr4, Ordering::Relaxed);
        // SAFETY: (SYSREG) `IA32_EFER` exists on every processor that has long mode.
        let efer = unsafe { cpu::read_msr(IA32_EFER) };

        let gdt_base = code + header_offset + HEADER_GDT;
        let long = code + long_offset;
        let header = Header {
            gdt_pointer: [31, gdt_base as u16, (gdt_base >> 16) as u16, 0],
            far_jump: [long as u16, (long >> 16) as u16, TRAMPOLINE_CODE, 0],
            cr3: root as u32,
            cr4: (boot_cr4 & !CR4_PCIDE) as u32,
            cr0: cpu::read_cr0() as u32,
            efer: (efer & !EFER_LMA) as u32,
            stack_top: 0,
            entry: secondary_start as extern "C" fn(u64) -> ! as usize as u64,
            argument: 0,
            gdt: [0, 0, TRAMPOLINE_CODE64, TRAMPOLINE_DATA],
        };

        Ok(CpuStarter {
            code,
            root,
            header_offset,
            header,
            started: 0,
        })
    }

    /// Start the processor `hardware_id` on `stack_top`, handing it
    /// `argument`.
    ///
    /// Returns once the start-up IPIs are sent, not once the processor is
    /// running: the caller waits for that, and must before starting another,
    /// because every processor reads the same header.
    pub(crate) fn start(
        &mut self,
        hardware_id: u64,
        stack_top: u64,
        argument: u64,
    ) -> Result<(), &'static str> {
        let apic_id =
            u32::try_from(hardware_id).map_err(|_| "an APIC ID wider than thirty-two bits")?;
        let page = u8::try_from(self.code / PAGE_SIZE)
            .map_err(|_| "the trampoline is not below one mebibyte")?;

        // The kernel's upper half as it is now, rather than as it was when
        // this starter was built: a slot that appeared since would be missing
        // from the copy, under this processor's own stack for all anyone knows.
        crate::mm::share_kernel_slots(self.root, UPPER_HALF_SLOT..ROOT_SLOTS);

        self.header.stack_top = stack_top;
        self.header.argument = argument;
        let at = crate::mm::direct_map(self.code + self.header_offset) as *mut Header;
        // SAFETY: (FRAME) the header is inside the trampoline page this starter owns,
        // eight-byte aligned by the assembly, and nothing is reading it: the
        // last processor started has reported in and the next is not started.
        unsafe { at.write(self.header) };

        self.started += 1;
        apic::send_init(apic_id)?;
        spin_nanos(INIT_DELAY_NANOS);
        apic::send_startup(apic_id, page)?;
        spin_nanos(STARTUP_DELAY_NANOS);
        // Twice, as the specification says. A processor that missed the first
        // takes the second; one already running ignores it, because a
        // start-up IPI means nothing outside the wait-for-start-up state.
        apic::send_startup(apic_id, page)
    }

    /// Give back the trampoline page and its tree.
    ///
    /// Only once every processor started has reported in: each one has left
    /// the trampoline's tree and GDT before it does.
    pub(crate) fn finish(self) -> Result<(), &'static str> {
        // Every processor started has looked at its local APIC's mode, the
        // boot processor's included (check R8).
        if let Some(line) = apic::check::require_every_processor(self.started + 1)? {
            crate::println!("  apic     {line}");
        }
        crate::mm::unmap_unwalked(self.root, self.code, PAGE_SIZE)
            .map_err(|_| "could not take down the trampoline's identity map")?;
        crate::mm::deallocate_frames(self.root / PAGE_SIZE, 0);
        crate::mm::deallocate_frames(self.code / PAGE_SIZE, 0);
        Ok(())
    }
}

/// Spin on the counter for `nanos`.
fn spin_nanos(nanos: u64) {
    let until = crate::timer::now_nanos().saturating_add(nanos);
    while crate::timer::now_nanos() < until {
        core::hint::spin_loop();
    }
}

/// Where a secondary processor arrives from the trampoline: in long mode, in
/// the upper half, on its own stack, with interrupts masked — and still on the
/// trampoline's tree and GDT, both of which are about to be given back.
extern "C" fn secondary_start(record: u64) -> ! {
    // Before anything that may use the local APIC's MMIO window -- an IPI a
    // failed allocation's shootdown sends included: in x2APIC mode it does
    // not exist, and INIT does not take a processor out of it (G6).
    apic::check::put_first_in_x2apic();
    if let Err(problem) = apic::leave_x2apic() {
        let initial = core::arch::x86_64::__cpuid(1).ebx >> 24;
        crate::panic::fatal!(
            crate::panic::catalog::X2APIC_LOCKED,
            "processor with APIC ID {initial}: {problem}"
        );
    }
    // SAFETY: (TRANSLATE) the kernel's root maps the upper half exactly as the
    // trampoline's copy of it does — this code, this stack, every record —
    // and nothing from here on touches the lower half, the only part in which
    // the two differ.
    unsafe { cpu::write_cr3(crate::mm::root_table()) };

    let boot_cr4 = BOOT_CR4.load(Ordering::Relaxed);
    if cpu::read_cr4() != boot_cr4 {
        // SAFETY: (SYSREG) the boot processor's own `CR4`, on a processor of the same
        // kind that is now in the same mode, with a `CR3` whose low bits are
        // clear as `PCIDE` requires.
        unsafe { cpu::write_cr4(boot_cr4) };
    }
    // `XCR0` is each processor's own; `CR4` just gave this one `OSXSAVE`.
    cpu::load_extended_state_on_this_cpu();
    // So is the PAT: entry 1 write-combining, as on the boot processor.
    if cpu::program_pat() {
        let _ = super::PAT_PROGRAMMED.fetch_add(1, Ordering::AcqRel);
    }

    // The IDT before the GDT: every gate names selector 0x10, which is 64-bit
    // kernel code in the trampoline's GDT as much as in the kernel's, so a
    // fault in the allocations `init_secondary` makes is reported rather than
    // turned into a triple fault.
    //
    // SAFETY: (ENTRY) the boot processor filled the table in `init_traps`.
    unsafe { super::trap::load_on_this_cpu() };

    // The per-CPU record before anything that allocates. `init_secondary`
    // takes a stack from the vmap arena, and an arena allocation that fails
    // part way unmaps what it mapped, which runs a shootdown, which reads
    // `this_cpu`. The flag that says that read is safe is global, set long ago
    // by the boot processor, so without this it would follow `GS` as reset left
    // it: a load from address zero, which the identity map still covers.
    crate::smp::install_secondary_record(record);
    // The boot processor's side-channel defences, before this one can run a
    // program: after the record, which is where it says what it applied.
    super::speculation::apply_this_cpu();

    // SAFETY: (ENTRY) once, on this processor, with interrupts masked.
    if let Err(problem) = unsafe { super::gdt::init_secondary() } {
        crate::panic::fatal!(
            crate::panic::catalog::SECONDARY_GDT,
            "a secondary processor could not build its GDT: {problem}"
        );
    }
    // Its last note was `init_secondary`'s: from here its record answers as
    // asking it would (Q4, the consultant's Q4-C1).
    match crate::smp::this_cpu() {
        Some(cpu) => super::gdt::check::require_tables_noted(cpu),
        None => crate::panic::fatal!(
            crate::panic::catalog::SECONDARY_NO_RECORD,
            "a secondary processor had no record after building its GDT"
        ),
    }
    apic::init_this_cpu();
    crate::smp::secondary_main(record)
}
