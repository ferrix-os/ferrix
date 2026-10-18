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
//! The layout below is the one Linux uses, and the order is not a preference.
//! `SYSRET` loads `CS` from `STAR[63:48] + 16` and `SS` from `STAR[63:48] + 8`,
//! so user data has to precede user 64-bit code by exactly eight bytes.

use core::cell::UnsafeCell;

use super::cpu;
use crate::percpu::MAX_CPUS;

/// Kernel code. `SYSCALL` loads this from `STAR[47:32]`.
pub(crate) const KERNEL_CODE: u16 = 0x08;
/// Kernel data.
pub(crate) const KERNEL_DATA: u16 = 0x10;
/// Unused 32-bit user code. Present only to put user data at the right offset
/// for `SYSRET`, which is also why it cannot simply be deleted.
const USER_CODE32: u16 = 0x18;
/// User data. `SYSRET` computes this as `STAR[63:48] + 8`.
pub(crate) const USER_DATA: u16 = 0x20;
/// User 64-bit code. `SYSRET` computes this as `STAR[63:48] + 16`.
pub(crate) const USER_CODE: u16 = 0x28;
/// The task state segment. Sixteen bytes, so it occupies two slots.
const TSS_SELECTOR: u16 = 0x30;

/// The base `SYSRET` computes user selectors from.
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

/// System descriptor type 9: an available 64-bit task state segment.
const TSS_AVAILABLE: u64 = 0b1001 << 40;

/// Interrupt stack table slot used for the double-fault handler.
///
/// The one fault that has to be handled on a stack of its own: a double fault
/// usually means the kernel stack is unusable, and taking the handler on that
/// same stack turns it into a triple fault, which is a silent reset.
pub(crate) const DOUBLE_FAULT_IST: u16 = 1;

/// Bytes in each interrupt stack table stack.
const IST_STACK_SIZE: usize = 16 * 1024;

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

/// Everything one CPU needs, in one place so a CPU's tables are one object.
///
/// **One of these per CPU, not one per machine.** The GDT could in principle be
/// shared — every descriptor in it is the same on every core — but the TSS
/// cannot: it holds *this* core's `RSP0` and *this* core's interrupt stack
/// table, and two cores sharing one would take a double fault onto the same
/// stack. Since the TSS descriptor lives in the GDT, a per-CPU TSS means a
/// per-CPU GDT, and the two travel together.
#[repr(C, align(16))]
struct Tables {
    /// Seven eight-byte slots: the six selectors above plus the second half of
    /// the sixteen-byte TSS descriptor.
    gdt: [u64; 8],
    tss: TaskStateSegment,
    /// Stack for the double-fault handler.
    double_fault_stack: [u8; IST_STACK_SIZE],
}

impl Tables {
    const fn new() -> Tables {
        Tables {
            gdt: [0; 8],
            tss: TaskStateSegment::new(),
            double_fault_stack: [0; IST_STACK_SIZE],
        }
    }
}

/// The tables themselves, one set per CPU.
struct Global(UnsafeCell<[Tables; MAX_CPUS]>);

// SAFETY: each CPU writes only its own element, once, from `init`, before it
// enables interrupts and before any other CPU could look at it; and nothing
// reads another CPU's element at all. The `UnsafeCell` is what makes that
// disjointness expressible — a `static mut` array would be the same accesses
// with no way to say so.
unsafe impl Sync for Global {}

static TABLES: Global = Global(UnsafeCell::new([const { Tables::new() }; MAX_CPUS]));

/// The operand `lgdt` takes: a limit and a base.
#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Build this CPU's GDT and TSS and load them.
///
/// # Safety
///
/// Must be called exactly once per CPU, by that CPU, with a `cpu` number no
/// other CPU is using, before any user mode entry and before interrupts are
/// enabled.
pub(crate) unsafe fn init(cpu: usize) {
    // SAFETY: the caller guarantees this CPU owns `cpu`, and nothing reads
    // another CPU's element, so this reference aliases nothing.
    let Some(tables) = (unsafe { &mut *TABLES.0.get() }).get_mut(cpu) else {
        // Unreachable: `percpu::assign` hands out no number at or above
        // `MAX_CPUS`, which is what sizes the array. Halting rather than
        // returning because a CPU that carries on here is running on the
        // trampoline's GDT, which has no task state segment — the next fault
        // would be a triple fault with nothing on the wire to explain it.
        super::halt()
    };

    let stack_top = tables.double_fault_stack.as_ptr() as u64 + IST_STACK_SIZE as u64;
    // The x86 stack grows down and `push` decrements first, so the top is one
    // past the last byte. Sixteen-byte aligned because the ABI says so and
    // because a misaligned stack breaks `movaps` in any handler that touches
    // floating point.
    tables
        .tss
        .set_interrupt_stack(DOUBLE_FAULT_IST, stack_top & !0xF);
    tables.tss.deny_all_ports();

    let tss_address = (&raw const tables.tss) as u64;
    let (low, high) = tss_descriptor(tss_address);

    tables.gdt = [
        0,
        USER_SEGMENT | PRESENT | EXECUTABLE | LONG_MODE,
        USER_SEGMENT | PRESENT | WRITABLE,
        USER_SEGMENT | PRESENT | EXECUTABLE | dpl(3),
        USER_SEGMENT | PRESENT | WRITABLE | dpl(3),
        USER_SEGMENT | PRESENT | EXECUTABLE | LONG_MODE | dpl(3),
        low,
        high,
    ];

    let pointer = DescriptorTablePointer {
        limit: (size_of_val(&tables.gdt) - 1) as u16,
        base: (&raw const tables.gdt) as u64,
    };

    // SAFETY: `pointer` describes the table just built, whose selectors match
    // the constants the reload below and every gate use.
    unsafe { cpu::load_gdt(&raw const pointer as u64) };
    // SAFETY: the GDT is loaded and holds a flat kernel code and data segment
    // at these selectors.
    unsafe { cpu::reload_segments(KERNEL_CODE, KERNEL_DATA) };
    // SAFETY: slot 0x30 of the GDT just built is an available 64-bit TSS
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
