//! The global descriptor table and the task state segment.
//!
//! Long mode barely uses segmentation — every segment is flat and covers
//! everything — but it does not let you skip it either. Three things still
//! need the GDT:
//!
//! * the privilege level, which is a property of the code segment,
//! * `SYSCALL`/`SYSRET`, which do not take a selector but *compute* one from
//!   `IA32_STAR`, so the selectors have to sit in a particular order,
//! * the task state segment, which is the only way to say which stack the CPU
//!   should switch to when a fault arrives from user mode.
//!
//! The layout below is Linux's, entry for entry and selector for selector,
//! and the order is not a preference. `SYSRET` loads `CS` from
//! `STAR[63:48] + 16` and `SS` from `STAR[63:48] + 8`, so user data has to
//! precede user 64-bit code by exactly eight bytes, and `SYSCALL` loads the
//! kernel's pair the same way from `STAR[47:32]`. The numbers are Linux's too,
//! because a 32-bit program's are part of what it sees: a signal handler
//! reads `cs` out of its context, and Wine keeps both code selectors to
//! switch between the modes (`docs/I386.md` §3.1).

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::sync::atomic::Ordering;

use super::cpu;
use crate::smp::PerCpu;

pub(super) mod check;

/// Kernel code. `SYSCALL` loads this from `STAR[47:32]`. Entry 2, as on
/// Linux; entry 1 is Linux's 32-bit kernel code, which nothing here uses and
/// which is left empty.
pub(crate) const KERNEL_CODE: u16 = 0x10;
/// Kernel data. `SYSCALL` loads this as `SS` from `STAR[47:32] + 8`.
pub(crate) const KERNEL_DATA: u16 = 0x18;
/// 32-bit user code: what an i386 program runs in, in compatibility mode.
/// `SYSRET` with a 32-bit operand loads it from `STAR[63:48]`, which only the
/// `CSTAR` stub's refusal uses; everything else enters it by `IRETQ`
/// (`super::syscall` says why).
pub(crate) const USER_CODE32: u16 = 0x20;
/// User data, for both modes. `SYSRET` computes this as `STAR[63:48] + 8`.
pub(crate) const USER_DATA: u16 = 0x28;
/// User 64-bit code. `SYSRET` computes this as `STAR[63:48] + 16`.
pub(crate) const USER_CODE: u16 = 0x30;
/// The task state segment. Sixteen bytes, so it occupies two slots, 8 and 9.
const TSS_SELECTOR: u16 = 0x40;
/// Slots in the table: Linux's sixteen. 10 and 11 are its LDT's, left empty,
/// and 12 to 14 the three thread-local descriptors `set_thread_area` fills,
/// which belong to whichever thread runs here (`docs/I386.md` §3.5).
const GDT_SLOTS: usize = 16;
/// The first thread-local descriptor's slot: Linux's `GDT_ENTRY_TLS_MIN`.
pub(crate) const TLS_FIRST_SLOT: usize = 12;
/// How many thread-local descriptors a thread has.
pub(crate) const TLS_SLOTS: usize = 3;

const _: () = assert!(
    TLS_FIRST_SLOT + TLS_SLOTS < GDT_SLOTS
        && TLS_FIRST_SLOT as u32 == ferrix_linux_abi::user_desc::TLS_FIRST_ENTRY
        && TLS_SLOTS == ferrix_linux_abi::user_desc::TLS_ENTRIES,
    "the thread-local slots are Linux's, inside the table"
);

/// The base `SYSRET` computes user selectors from, without its RPL.
///
/// `STAR[63:48]` holds this OR 3, not this: `SYSRET` forces RPL 3 into the CS it
/// computes but loads SS exactly as written, and an RPL-0 SS in ring 3 is
/// refused by the next `iretq` back there. See `syscall::init`.
pub(crate) const SYSRET_BASE: u16 = USER_CODE32;

// `SYSRET` does not take a selector: it *computes* one, loading `CS` from
// `STAR[63:48] + 16` and `SS` from `STAR[63:48] + 8`. That makes the order of
// the three user entries part of the instruction's contract rather than a
// matter of taste, and getting it wrong returns to user mode with the wrong
// segment -- which faults immediately if you are lucky and does something far
// worse if you are not. Asserting it here means the layout cannot be shuffled
// without the build saying so.
const _: () = assert!(
    USER_DATA == SYSRET_BASE + 8,
    "SYSRET loads SS from STAR[63:48] + 8, so user data must sit there"
);
const _: () = assert!(
    USER_CODE == SYSRET_BASE + 16,
    "SYSRET loads CS from STAR[63:48] + 16, so user 64-bit code must sit there"
);
const _: () = assert!(
    KERNEL_DATA == KERNEL_CODE + 8,
    "SYSCALL loads SS from STAR[47:32] + 8, so kernel data must sit there"
);
// Linux's numbers, which a program can read: `__USER32_CS`, `__USER_DS` and
// `__USER_CS` with their RPL of 3.
const _: () = assert!(
    USER_CODE32 | 3 == 0x23 && USER_DATA | 3 == 0x2b && USER_CODE | 3 == 0x33,
    "the user selectors are Linux's numbers"
);

/// Descriptor bit: the segment is present.
const PRESENT: u64 = 1 << 47;
/// Descriptor bit: a code or data segment rather than a system one.
const USER_SEGMENT: u64 = 1 << 44;
/// Descriptor bit: executable, which is what makes a segment a code segment.
const EXECUTABLE: u64 = 1 << 43;
/// Descriptor bit: writable, for a data segment.
const WRITABLE: u64 = 1 << 41;
/// Descriptor bit: 64-bit code. Mutually exclusive with the 32-bit size bit.
const LONG_MODE: u64 = 1 << 53;
/// Descriptor field: the privilege level the segment runs at.
const fn dpl(level: u64) -> u64 {
    level << 45
}
/// Descriptor bit: set by the processor on the first load, and set here so
/// that it never writes the table.
const ACCESSED: u64 = 1 << 40;
/// Descriptor bit: a code segment may be read as well as executed.
const READABLE: u64 = 1 << 41;
/// Descriptor bit: 32-bit operands and addresses by default (`D`), or a
/// 32-bit stack pointer for a data segment (`B`). Mutually exclusive with
/// [`LONG_MODE`].
const DEFAULT_32: u64 = 1 << 54;
/// Descriptor bit: the limit counts pages rather than bytes.
const GRANULARITY_4K: u64 = 1 << 55;
/// A limit of `0xFFFFF` pages: the whole 4 GiB, which is what compatibility
/// mode checks every 32-bit access against. Its low sixteen bits sit at the
/// bottom of the descriptor, its high four at bits 48 to 51.
const FLAT_LIMIT: u64 = 0xFFFF | (0xF << 48);
/// Everything a flat 4 GiB user segment has but its type: present, a code or
/// data segment, ring 3, page-granular, and already accessed.
const FLAT_USER: u64 = USER_SEGMENT | PRESENT | dpl(3) | ACCESSED | GRANULARITY_4K | FLAT_LIMIT;
/// The 32-bit user code segment's descriptor: Linux's `0x00cffb000000ffff`.
const USER_CODE32_DESCRIPTOR: u64 = FLAT_USER | EXECUTABLE | READABLE | DEFAULT_32;
/// The user data segment's: Linux's `0x00cff3000000ffff`.
const USER_DATA_DESCRIPTOR: u64 = FLAT_USER | WRITABLE | DEFAULT_32;
/// Descriptor field: the privilege level, two bits.
const DPL_MASK: u64 = dpl(3);
/// Descriptor bit, for code: conforming, which runs at the caller's level.
const CONFORMING: u64 = 1 << 42;

const _: () = assert!(
    USER_CODE32_DESCRIPTOR == 0x00cf_fb00_0000_ffff
        && USER_DATA_DESCRIPTOR == 0x00cf_f300_0000_ffff,
    "the 32-bit user segments are Linux's"
);

/// System descriptor type 9: an available 64-bit task state segment.
const TSS_AVAILABLE: u64 = 0b1001 << 40;

/// Interrupt stack table slot used for the double-fault handler.
///
/// A double fault usually means the kernel stack is unusable, and taking the
/// handler on that same stack turns it into a triple fault, which is a silent
/// reset.
pub(crate) const DOUBLE_FAULT_IST: u16 = 1;

/// Interrupt stack table slot for the non-maskable interrupt.
///
/// An NMI is not held off by `cli`, so it can arrive on any instruction the
/// kernel runs -- including the ones in the `SYSCALL` trampoline before the
/// stack switch and after the switch back, where `RSP` is the program's. An
/// exception that does not change privilege pushes its frame on whatever `RSP`
/// is, so without a stack of its own the kernel would write its frame, and run
/// its handler, on a stack a program chose.
pub(crate) const NMI_IST: u16 = 2;

/// Interrupt stack table slot for the debug exception, for the NMI's reason:
/// a hardware breakpoint fires wherever its address is, ring 0 on a user stack
/// included.
pub(crate) const DEBUG_IST: u16 = 3;

/// Interrupt stack table slot for the machine check, which the processor
/// raises when it pleases, for the NMI's reason again.
pub(crate) const MACHINE_CHECK_IST: u16 = 4;

/// How many interrupt stack table stacks each processor has, one per slot
/// above: slots 1 to this.
const IST_STACKS: usize = 4;

/// Bytes in each interrupt stack table stack.
///
/// The same as a vmap stack, which is what a secondary processor's come from:
/// four pages.
const IST_STACK_SIZE: usize = 16 * 1024;

/// Bytes left above each IST entry: one word counting the exceptions using the
/// stack, and one to keep the entry sixteen-byte aligned. `trap.rs`'s paranoid
/// entry reads the count at the entry itself, just above the frame the
/// processor pushes there.
const IST_OCCUPANCY_RESERVE: u64 = 16;

const _: () = assert!(
    IST_STACK_SIZE as u64 == crate::vmap::STACK_PAGES * ferrix_bootinfo::PAGE_SIZE,
    "the boot processor's interrupt stacks are the size a secondary's vmap stacks are"
);

/// Bytes in a 64-bit task state segment.
const TSS_SIZE: usize = 104;

/// Offset of `RSP0`, the stack the CPU switches to on entry to ring 0.
const TSS_PRIVILEGE_STACK: usize = 4;
/// Offset of `IST1`. The seven interrupt stack table slots follow it.
const TSS_INTERRUPT_STACK: usize = 36;
/// Offset of the I/O permission bitmap pointer.
const TSS_IOMAP_BASE: usize = 102;

/// The task state segment.
///
/// Long mode ignores almost all of the 32-bit TSS: what remains is the
/// privilege-level stack pointers, the interrupt stack table, and the I/O
/// permission bitmap offset.
///
/// **Bytes rather than fields, deliberately.** `RSP0` sits at offset 4, which
/// puts a `u64` at a four-byte-aligned offset, so a struct describing this
/// layout has to be `packed` — and taking a reference to a field of a packed
/// struct is undefined behaviour in Rust *even when the reference is never
/// read*. Named offsets into a byte array say exactly the same thing and stay
/// sound.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug)]
struct TaskStateSegment([u8; TSS_SIZE]);

impl TaskStateSegment {
    const fn new() -> TaskStateSegment {
        TaskStateSegment([0; TSS_SIZE])
    }

    /// Write a little-endian `u64` at `offset`.
    ///
    /// Silently does nothing if the offset is out of range, which cannot happen
    /// — every caller passes one of the constants above — but is the shape that
    /// avoids a panic in a kernel.
    fn write_u64(&mut self, offset: usize, value: u64) {
        if let Some(slot) = self.0.get_mut(offset..offset + 8) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
    }

    /// Write a little-endian `u16` at `offset`.
    fn write_u16(&mut self, offset: usize, value: u16) {
        if let Some(slot) = self.0.get_mut(offset..offset + 2) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
    }

    /// Point the I/O permission bitmap past the end of the segment, which is
    /// how a TSS says that ring 3 may touch no port at all.
    fn deny_all_ports(&mut self) {
        self.write_u16(TSS_IOMAP_BASE, TSS_SIZE as u16);
    }

    /// Set one of the seven interrupt stack table entries, numbered from one
    /// as the gate's IST field numbers them.
    fn set_interrupt_stack(&mut self, slot: u16, stack_top: u64) {
        if slot == 0 || slot > 7 {
            return;
        }
        let offset = TSS_INTERRUPT_STACK + (slot as usize - 1) * 8;
        self.write_u64(offset, stack_top);
    }

    /// Set `RSP0`.
    fn set_privilege_stack(&mut self, stack_top: u64) {
        self.write_u64(TSS_PRIVILEGE_STACK, stack_top);
    }
}

/// One processor's descriptor tables.
///
/// **Per processor, and it has to be.** A TSS holds its processor's stack
/// pointers, and loading one marks its descriptor busy — so a second processor
/// loading the same descriptor takes a general protection fault. The GDT holds
/// that descriptor, so it is per processor too.
#[repr(C, align(16))]
struct Tables {
    /// Linux's sixteen eight-byte slots; the TSS descriptor takes two.
    gdt: [u64; GDT_SLOTS],
    tss: TaskStateSegment,
}

impl Tables {
    const fn new() -> Tables {
        Tables {
            gdt: [0; GDT_SLOTS],
            tss: TaskStateSegment::new(),
        }
    }
}

/// The boot processor's tables.
struct Global(UnsafeCell<Tables>);

// SAFETY: (SHARED) written once by `init` on the boot CPU before interrupts are enabled,
// and read by the CPU alone thereafter. Every other processor has its own, from
// `init_secondary`.
unsafe impl Sync for Global {}

/// The boot processor's tables: static, because they are loaded before there
/// is an allocator.
static BOOT_TABLES: Global = Global(UnsafeCell::new(Tables::new()));

/// One of the boot processor's interrupt stack table stacks, static for the
/// same reason.
///
/// Every other processor's come from the vmap arena, guard pages and all.
#[repr(C, align(16))]
struct BootStack(UnsafeCell<[u8; IST_STACK_SIZE]>);

// SAFETY: (SHARED) nothing in the kernel names these by address: the CPU switches to
// one on the exceptions its slot is given to, which is their whole purpose, and
// only the handler running on it touches it.
unsafe impl Sync for BootStack {}

/// The boot processor's interrupt stack table stacks, slot 1 first.
static BOOT_IST_STACKS: [BootStack; IST_STACKS] =
    [const { BootStack(UnsafeCell::new([0; IST_STACK_SIZE])) }; IST_STACKS];

/// The operand `lgdt` takes: a limit and a base.
#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Build the boot processor's GDT and TSS and load them.
///
/// # Safety
///
/// (ENTRY) Must be called exactly once, on the boot CPU, before any user mode entry and
/// before interrupts are enabled.
pub(crate) unsafe fn init() {
    // SAFETY: (SHARED) single-threaded early boot, and this is the only writer.
    let tables = unsafe { &mut *BOOT_TABLES.0.get() };

    // The x86 stack grows down and `push` decrements first, so the top is one
    // past the last byte. Sixteen-byte aligned because the ABI says so and
    // because a misaligned stack breaks `movaps` in any handler that touches
    // floating point.
    let tops = BOOT_IST_STACKS
        .each_ref()
        .map(|stack| (stack.0.get() as u64 + IST_STACK_SIZE as u64) & !0xF);
    // SAFETY: (ENTRY) the boot processor's own tables, loaded once, and stacks the CPU
    // alone uses.
    unsafe { load(tables, tops) };
}

/// Point this processor's `RSP0` at `top`.
///
/// `RSP0` is the stack an interrupt or exception from ring 3 switches to, so
/// this has to follow whichever task is running: land a page fault on the
/// previous task's stack and two tasks share one, which corrupts quietly and
/// at a distance.
///
/// # Finding the TSS
///
/// By asking the processor, rather than by remembering what this module
/// built: `STR` gives this processor's task register and `SGDT` its GDT, so
/// the descriptor -- and the base address inside it -- is read back from the
/// hardware that is actually using them ([`ask_privilege_stack`]).
///
/// Asked when the tables are loaded, not at every switch (Q4,
/// `docs/OPAQUE-KERNEL.md` §9.11): `SGDT` and `STR` are microcoded, and a
/// switch made three of the one and one of the other. [`note_tables`] puts
/// the answer in `cpu`'s record after every load of `GDTR` or `TR` made with
/// a record installed, and when the record is installed, so the record holds
/// exactly what asking would answer: the processor's tables change only at
/// those loads. A record that holds nothing yet -- none installed, or a
/// processor still on the start-up trampoline's GDT -- is asked past.
///
/// # Safety
///
/// (ENTRY) A TSS must be loaded, which [`init`] or [`init_secondary`] has done by the
/// time any task runs, `top` must be the top of a stack this processor
/// alone uses, and `cpu` this processor's own record, if any.
pub(crate) unsafe fn set_privilege_stack(cpu: Option<&PerCpu>, top: u64) {
    let noted = cpu.map_or(0, |cpu| cpu.privilege_stack.load(Ordering::Relaxed));
    let rsp0 = if noted == 0 {
        // SAFETY: (ENTRY) the caller's guarantee: a TSS is loaded.
        let Some(asked) = (unsafe { ask_privilege_stack() }) else {
            return;
        };
        asked
    } else {
        noted as *mut u64
    };
    // SAFETY: (ENTRY) the TSS this processor has loaded, at the offset long mode puts
    // `RSP0`, as `ask_privilege_stack` found it now or at the last load of this
    // processor's tables. Unaligned because the 32-bit TSS layout put a `u32`
    // before it, which is why `TaskStateSegment` is a byte array in the first
    // place.
    unsafe { rsp0.write_unaligned(top) };
}

/// Where this processor's loaded TSS keeps `RSP0`, found through `STR` and
/// `SGDT`; `None` where the descriptor `TR` names is not inside the GDT.
///
/// # Safety
///
/// (ENTRY) A TSS must be loaded, or `TR` be null, whose slot 0 is inside any GDT
/// and yields an address nothing writes through: the caller writes only
/// once a TSS is loaded.
unsafe fn ask_privilege_stack() -> Option<*mut u64> {
    // SAFETY: (ENTRY) reads the task register; no memory is touched.
    let selector = unsafe { cpu::read_task_register() };
    // SAFETY: (ENTRY) writes ten bytes of GDTR into a local.
    let (gdt_base, gdt_limit) = unsafe { cpu::read_gdt() };

    let index = usize::from(selector & !0x7);
    // A 64-bit TSS descriptor is sixteen bytes, so both halves must be inside
    // the table. A limit that says otherwise means the GDT is not the one this
    // code built, and writing into it would be writing somewhere arbitrary.
    if selector == 0 || index + 16 > usize::from(gdt_limit) + 1 {
        return None;
    }

    let descriptor = (gdt_base as usize + index) as *const u64;
    // SAFETY: (ENTRY) `index` is inside the GDT the processor is using, which this
    // module built and which lives for the life of the processor.
    let low = unsafe { descriptor.read() };
    // SAFETY: (ENTRY) the second half of the same descriptor, whose sixteen bytes were
    // bounds-checked above.
    let upper = unsafe { descriptor.add(1) };
    // SAFETY: (ENTRY) as above; the pointer is inside the table.
    let high = unsafe { upper.read() };

    // The base is scattered across the descriptor in three pieces below 32
    // bits and one above, an arrangement inherited from the 286 and preserved
    // through two widenings.
    let base =
        ((low >> 16) & 0x00FF_FFFF) | (((low >> 56) & 0xFF) << 24) | ((high & 0xFFFF_FFFF) << 32);

    Some((base as usize + TSS_PRIVILEGE_STACK) as *mut u64)
}

/// Put in `cpu`'s record what asking this processor for its tables answers
/// now: its GDT ([`live_table`]) and where its TSS keeps `RSP0`
/// ([`ask_privilege_stack`]), zero for either it does not have (Q4). Called
/// by [`load`] after it loads `GDTR` and `TR`, for a record installed by
/// then, and by `set_cpu_local` as it installs one; nothing else loads
/// either register once a record is installed, so the record answers as
/// asking would from then on.
///
/// # Safety
///
/// (ENTRY) `cpu` must be this processor's own record, and interrupts masked.
pub(crate) unsafe fn note_tables(cpu: &PerCpu) {
    let table = live_table().map_or(0, |table| table as u64);
    // SAFETY: (ENTRY) the task register is null or names the TSS a load put
    // there; only the address is computed, nothing is written.
    let rsp0 = unsafe { ask_privilege_stack() }.map_or(0, |rsp0| rsp0 as u64);
    cpu.gdt.store(table, Ordering::Relaxed);
    cpu.privilege_stack.store(rsp0, Ordering::Relaxed);
}

/// Whether this processor's task register names a TSS: false on a
/// secondary until `init_secondary` loads one.
pub(crate) fn task_register_loaded() -> bool {
    // SAFETY: (ENTRY) reads the task register; no memory is touched.
    unsafe { cpu::read_task_register() } != 0
}

/// This processor's GDT, as the processor reports it, if it is one of the
/// tables this module built: long enough to hold every slot.
fn live_table() -> Option<*mut u64> {
    // SAFETY: (ENTRY) writes ten bytes of GDTR into a local.
    let (base, limit) = unsafe { cpu::read_gdt() };
    (usize::from(limit) + 1 >= GDT_SLOTS * 8).then_some(base as *mut u64)
}

/// This processor's GDT as its record noted it at the last load
/// ([`note_tables`]), or as the processor reports it where the record has
/// none.
fn noted_table() -> Option<*mut u64> {
    let noted = crate::smp::this_cpu().map_or(0, |cpu| cpu.gdt.load(Ordering::Relaxed));
    if noted == 0 {
        live_table()
    } else {
        Some(noted as *mut u64)
    }
}

/// The three thread-local descriptors this processor holds: the running
/// thread's, since the switch to it loaded them and `set_thread_area` writes
/// them here.
///
/// # Safety
///
/// (CONTEXT) Interrupts must be masked, or the thread could move to another processor
/// between the question and the answer.
pub(crate) unsafe fn read_tls() -> [u64; TLS_SLOTS] {
    let mut tls = [0; TLS_SLOTS];
    if let Some(table) = noted_table() {
        for (slot, value) in tls.iter_mut().enumerate() {
            // SAFETY: (CONTEXT) slots 12 to 14 are inside the table, whose limit
            // `live_table` measured, now or at its load (`noted_table`).
            let at = unsafe { table.add(TLS_FIRST_SLOT + slot) };
            // SAFETY: (CONTEXT) a slot of this processor's table, which the processor
            // reads and nothing else writes.
            *value = unsafe { at.read() };
        }
    }
    tls
}

/// Put `tls` in this processor's thread-local slots.
///
/// A descriptor already loaded into a segment register is not changed by
/// this -- the processor keeps a copy -- so a caller that changed a slot a
/// register names reloads that register after.
///
/// # Safety
///
/// (CONTEXT) Interrupts must be masked, and every nonzero descriptor must be one
/// `ferrix_linux_abi::user_desc` built: ring 3 data, which no kernel selector
/// names.
pub(crate) unsafe fn write_tls(tls: &[u64; TLS_SLOTS]) {
    if let Some(table) = noted_table() {
        for (slot, value) in tls.iter().enumerate() {
            // SAFETY: (CONTEXT) as in `read_tls`.
            let at = unsafe { table.add(TLS_FIRST_SLOT + slot) };
            // SAFETY: (CONTEXT) only this processor uses its table. Ordinary memory, as
            // `set_privilege_stack`'s write to the TSS is: the processor reads
            // it at the next selector load, which is an `asm!` block that may
            // read memory, so the write is done by then.
            unsafe { at.write(*value) };
        }
    }
}

/// `selector` if it can be loaded into a data segment register with `tls`
/// in the thread-local slots, with RPL 3, and the null selector if it cannot.
///
/// What a program loaded was valid when it loaded it, but a thread-local
/// descriptor may have been emptied since, and a load of a selector naming
/// an empty slot is `#GP` in ring 0. Linux recovers from that fault; here the
/// selector is checked first (certification review, T.ESCALATE path 7). It
/// has to name one of the only slots a program may load -- 32-bit user code,
/// user data, or a thread-local slot -- never the LDT (bit 2), and the
/// descriptor there has to be one ring 3 may hold ([`ring_3_segment`]).
/// RPL is forced to 3: a program may load its own segments with a lower one,
/// which means the same segment and no more.
pub(crate) fn loadable(selector: u16, tls: &[u64; TLS_SLOTS]) -> u16 {
    const TABLE_INDICATOR: u16 = 1 << 2;
    if selector & TABLE_INDICATOR != 0 {
        return 0;
    }
    let slot = usize::from(selector >> 3);
    let descriptor = match slot {
        _ if slot == usize::from(USER_CODE32 >> 3) => USER_CODE32_DESCRIPTOR,
        _ if slot == usize::from(USER_DATA >> 3) => USER_DATA_DESCRIPTOR,
        _ => match slot
            .checked_sub(TLS_FIRST_SLOT)
            .and_then(|index| tls.get(index))
        {
            Some(&descriptor) => descriptor,
            None => return 0,
        },
    };
    if ring_3_segment(descriptor) {
        selector | 3
    } else {
        0
    }
}

/// Whether ring 3 may hold `descriptor` in a data segment register: present,
/// a code or data segment rather than a system one or a gate, DPL 3, not a
/// 64-bit segment, and data, or code that is readable and not conforming.
pub(crate) const fn ring_3_segment(descriptor: u64) -> bool {
    let usable = descriptor & PRESENT != 0
        && descriptor & USER_SEGMENT != 0
        && descriptor & DPL_MASK == DPL_MASK
        && descriptor & LONG_MODE == 0;
    let kind = descriptor & EXECUTABLE == 0
        || (descriptor & READABLE != 0 && descriptor & CONFORMING == 0);
    usable && kind
}

/// Build and load this secondary processor's own GDT and TSS./// Build and load this secondary processor's own GDT and TSS.
///
/// # Safety
///
/// (ENTRY) Must be called once, on the secondary processor itself, before it enables
/// interrupts.
pub(crate) unsafe fn init_secondary() -> Result<(), &'static str> {
    // One per slot, and never freed: the processor uses them for the rest of
    // its life. A failure part way leaks the ones already taken, which is what
    // the fatal report this becomes costs anyway.
    let mut tops = [0; IST_STACKS];
    for top in &mut tops {
        *top = crate::vmap::allocate_stack()
            .map_err(|_| "no interrupt stack table stack for a secondary processor")?
            .top;
    }
    // Leaked: the processor uses these for the rest of its life.
    let tables: &'static mut Tables = Box::leak(
        crate::fallible::try_box(Tables::new()).map_err(|_| "no memory for a processor's GDT")?,
    );
    // SAFETY: (ENTRY) fresh tables that nothing else refers to, and fresh stacks.
    unsafe { load(tables, tops) };
    Ok(())
}

/// Fill `tables` for this processor and make it use them.
///
/// # Safety
///
/// (ENTRY) `tables` must belong to this processor alone and live as long as it runs,
/// and each of `ist_tops`, the stack for slot 1 first, must be the top of a
/// stack nothing else uses.
unsafe fn load(tables: &'static mut Tables, ist_tops: [u64; IST_STACKS]) {
    for (slot, top) in (1..).zip(ist_tops) {
        // Sixteen bytes below the top, not the top: the word there counts the
        // exceptions using the stack, which the paranoid entry reads just
        // above the frame the processor pushes at the IST entry, and nothing
        // else writes. It must start at zero, whatever the stack held.
        let entry = top - IST_OCCUPANCY_RESERVE;
        // SAFETY: (ENTRY) `entry` is inside the stack `top` ends, which the caller
        // guarantees nothing else is using, and eight-byte aligned.
        unsafe { (entry as *mut u64).write(0) };
        tables.tss.set_interrupt_stack(slot, entry);
    }
    tables.tss.deny_all_ports();

    let tss_address = (&raw const tables.tss) as u64;
    let (low, high) = tss_descriptor(tss_address);

    // The two user segments compatibility mode runs in are flat 4 GiB ones
    // with 32-bit defaults, because there, unlike in 64-bit mode, the
    // processor checks every access against the limit and takes the operand
    // size from `D`. Linux's `0x00cffb000000ffff` and `0x00cff3000000ffff`.
    tables.gdt = [
        0,
        0,
        USER_SEGMENT | PRESENT | EXECUTABLE | LONG_MODE,
        USER_SEGMENT | PRESENT | WRITABLE,
        USER_CODE32_DESCRIPTOR,
        USER_DATA_DESCRIPTOR,
        USER_SEGMENT | PRESENT | EXECUTABLE | LONG_MODE | dpl(3),
        0,
        low,
        high,
        0,
        0,
        0,
        0,
        0,
        0,
    ];

    let pointer = DescriptorTablePointer {
        limit: (size_of_val(&tables.gdt) - 1) as u16,
        base: (&raw const tables.gdt) as u64,
    };

    // SAFETY: (ENTRY) `pointer` describes the table just built, whose selectors match
    // the constants the reload below and every gate use.
    unsafe { cpu::load_gdt(&raw const pointer as u64) };
    // SAFETY: (ENTRY) the GDT is loaded and holds a flat kernel code and data segment
    // at these selectors.
    unsafe { cpu::reload_segments(KERNEL_CODE, KERNEL_DATA) };
    // SAFETY: (ENTRY) slot 0x40 of the GDT just built is an available 64-bit TSS
    // descriptor for `tables.tss`.
    unsafe { cpu::load_tss(TSS_SELECTOR) };

    // Give `RSP0` the stack the kernel is running on right now. Nothing enters
    // user mode yet, so nothing reads it yet -- but a TSS whose `RSP0` is zero
    // is one where the first trap from ring 3 pushes onto address zero, and it
    // costs nothing to make that impossible from the start. The scheduler
    // replaces it per task from stage 5.
    tables
        .tss
        .set_privilege_stack(cpu::read_stack_pointer() & !0xF);

    // A secondary's record is installed before its tables are loaded: what
    // it noted then was the start-up trampoline's, so it notes these (Q4).
    // The boot processor has no record yet; `set_cpu_local` notes its tables.
    if let Some(record) = crate::smp::this_cpu() {
        // SAFETY: (ENTRY) this processor's own record; bring-up runs with
        // interrupts masked.
        unsafe { note_tables(record) };
    }
}

/// Split a TSS base address into the two halves of a system descriptor.
fn tss_descriptor(base: u64) -> (u64, u64) {
    let limit = (TSS_SIZE - 1) as u64;

    let low = limit
        | ((base & 0x00FF_FFFF) << 16)
        | TSS_AVAILABLE
        | PRESENT
        | (((base >> 24) & 0xFF) << 56);
    let high = base >> 32;
    (low, high)
}
