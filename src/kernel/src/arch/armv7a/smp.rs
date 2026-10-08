//! Processors on ARMv7-A.
//!
//! The device tree is the processor list here, as the MADT is on the other
//! two: `/cpus` has a node per core, whose `reg` is the affinity fields of
//! that core's `MPIDR` and whose `enable-method` says how it is started. On
//! every machine this kernel runs on the method is PSCI, and `CPU_ON` is
//! AArch64's call in the 32-bit convention.
//!
//! # Starting one
//!
//! AArch64's problem, in coprocessor 15's spelling. `CPU_ON` starts a core in
//! SVC mode at a physical address, with its MMU and caches off and one
//! argument in `r0`, and everything the kernel runs on is in the upper half.
//! So the entry sequence below installs the loader's long-descriptor regime —
//! `MAIR0` and `MAIR1`, `TTBCR` with `EAE`, `TTBR0` and `TTBR1` — through an
//! identity map of itself, loading every parameter before the MMU goes on,
//! and branches to Rust at a virtual address. What differs from AArch64 is
//! `TTBR1`, which holds the kernel's root plus sixteen, for the reason
//! `src/boot/common/uefi/src/arch/armv7a.rs` gives.
//!
//! Which tree that identity map is in depends on where the machine keeps its
//! RAM, exactly as it does for the loader's own switch: a `TTBR0` tree of the
//! kernel's making where the kernel's text is below the split, and the
//! kernel's own tree where it is above it.
//!
//! One stack is all a secondary needs. The trap stubs store on the SVC stack
//! from whichever mode an exception arrives in, so no other mode has one —
//! which is also why the entry sequence checks that the core is in SVC mode
//! before anything else, and parks it if not: a core in any other mode would
//! take its first exception somewhere with no stack at all. A parked core
//! never reports in, and the boot fails naming that, rather than running on.
//!
//! # What QEMU cannot check
//!
//! A Cortex-A7 or A15 has to have `ACTLR.SMP` set before its caches and MMU
//! go on, or it is not coherent with the other cores. It is not set here.
//! From the non-secure world the write is permitted only if the secure world
//! allowed it, and an undefined instruction with the MMU off has nowhere to
//! go; PSCI firmware on real boards — TF-A, U-Boot's own — sets it before
//! entering the kernel, and QEMU does not model the bit at all. A board that
//! turns out not to is where this changes.
//!
//! So every core reads the bit once it is safely in Rust and reports it, and
//! the boot log says how many had it. That is worth a line of its own because
//! of what the alternative looks like: a core without it runs, takes
//! interrupts and passes every check that only reads its own memory, and
//! fails stage 4's shared counter — which reads as a bug in the lock, in the
//! barriers, or in the scheduler, anywhere but in a bit firmware did not set.
//! Reporting it costs one register read and turns that into a sentence.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ferrix_bootinfo::{BootView, PAGE_SIZE, flag_in};
use ferrix_fdt::PsciConduit;
use ferrix_paging::MapFlags;

use super::{cpu, gicv2};
use crate::smp::Described;

/// The affinity fields of `MPIDR`: `Aff0` to `Aff2`, bits 0..24.
///
/// For AArch64's reason: the rest of the register is not identity. Bit 31
/// reads as one on every core with the multiprocessing extensions and bit 30
/// says whether this is a uniprocessor, while the device tree stores the
/// affinity fields alone.
const MPIDR_AFFINITY: u64 = 0x00FF_FFFF;

/// PSCI `CPU_ON`, in the 32-bit calling convention.
const PSCI_CPU_ON: u32 = 0x8400_0003;

/// `ACTLR.SMP`: this core takes part in coherency. Cortex-A7 and A15 bit 6.
const ACTLR_SMP: u32 = 1 << 6;

/// How many cores have reached Rust and looked at their own `ACTLR`.
static COHERENCY_SEEN: AtomicU32 = AtomicU32::new(0);

/// How many of those had [`ACTLR_SMP`] set.
static COHERENCY_SET: AtomicU32 = AtomicU32::new(0);

/// Whether to read `ACTLR` at all, cleared by `noactlr` on the command line.
///
/// A escape hatch rather than a feature. Reading the register is architecturally
/// permitted from the non-secure world, but "permitted" here means every
/// Cortex-A7 the specification describes, and this kernel is about to meet
/// silicon it has never run on. If some part turns the read into an undefined
/// instruction, the symptom would be a panic in the middle of bring-up with
/// the console already working — diagnosable, but only if there is a way to
/// turn it off without rebuilding, which on a board means a word in U-Boot's
/// `bootargs`.
static READ_ACTLR: AtomicBool = AtomicBool::new(true);

/// Record this core's coherency bit, if the command line left the read on.
fn note_coherency() {
    if !READ_ACTLR.load(Ordering::Relaxed) {
        return;
    }
    let _ = COHERENCY_SEEN.fetch_add(1, Ordering::Relaxed);
    if cpu::read_actlr() & ACTLR_SMP != 0 {
        let _ = COHERENCY_SET.fetch_add(1, Ordering::Relaxed);
    }
}

/// Where the kernel half's level-1 table starts within the root: entry 2,
/// eight bytes each. `src/boot/common/uefi/src/arch/armv7a.rs` says why.
const TTBR1_OFFSET: u64 = 16;

/// Where `TTBR0`'s half of the address space ends, with `TTBCR.T1SZ = 1`.
///
/// The entry sequence has to be below this for a `TTBR0` tree to be able to
/// identity map it. On QEMU's `virt` RAM starts at 1 GiB and it is; on a
/// machine where it is not — the STM32MP157, whose DDR is at 3 GiB — the
/// mapping goes in the kernel's own tree instead, which is the only one that
/// translates those addresses.
const LOWER_HALF_END: u64 = 0x8000_0000;

/// Every processor the device tree says can be started, and which one this is.
///
/// Counted: the processor running this, whatever its node says, and every
/// other whose `enable-method` is PSCI. A core started some other way — a
/// spin table — is one this kernel cannot start, and counting it would have
/// the exit test wait for a processor that will never answer, which is the
/// rule ACPI's enabled flag stands for on the other two. `status = "disabled"`
/// is counted: for a processor it means stopped, not absent.
///
/// # Errors
///
/// If there is no device tree to read.
pub(crate) fn describe_cpus(view: &BootView<'_>) -> Result<Described, &'static str> {
    let tree = crate::discovery::fdt::open(view)?;
    let boot = hardware_id();
    // A tree that describes PSCI and says nothing about how a given processor
    // is started means PSCI. That is not a guess: it is what Linux does on
    // this architecture, where `enable-method` is optional, and the
    // STM32MP157 relies on it — its `/cpus` nodes carry no `enable-method` at
    // all, so requiring one would silently boot a dual-core board on one core.
    // A node naming some *other* method is still left out, because this kernel
    // cannot start one that way.
    // From the loader's command line, which xtask's `--kernel-option` writes,
    // or from `/chosen/bootargs`, where U-Boot puts `bootargs` on a board:
    // either says it, as AArch64 reads the one and `power::init` both.
    let args = tree.bootargs().unwrap_or("");
    let flag = |name: &str| view.flag(name) || flag_in(args, name);
    if flag("noactlr") {
        READ_ACTLR.store(false, Ordering::Relaxed);
    }
    // The boot core is in Rust already, so it can be counted here; the others
    // count themselves as they arrive.
    note_coherency();

    // `nosmp` keeps the boot processor and leaves the rest described but not
    // started. The reason it exists is the first boot on a new board: bring-up
    // failures that involve a second core — an entry sequence that faults, a
    // core that never reports in, caches that turn out not to be coherent —
    // all look like a machine that stops partway through stage 4, and none of
    // them can be told apart from a machine whose *first* core is wrong. One
    // core reaching the end of stage 3 says the loader, the hand-off, the page
    // tables, the console and the timer are all right, and narrows what is
    // left to the thing this flag turned off.
    if flag("nosmp") {
        return Ok(Described {
            id_name: "MPIDR",
            boot,
            // FATAL-ALLOC: boot only: stage 4 lists the processors firmware describes, once.
            ids: alloc::vec![boot],
        });
    }

    let psci = tree.psci_conduit().is_some();
    let ids: Vec<u64> = tree
        .cpus()
        .map(|cpu| (cpu.id & MPIDR_AFFINITY, cpu.enable_method()))
        .filter(|&(id, method)| {
            id == boot
                || match method {
                    Some(named) => named == "psci",
                    None => psci,
                }
        })
        .map(|(id, _)| id)
        // FATAL-ALLOC: boot only: stage 4 lists the processors firmware describes, once.
        .collect();

    Ok(Described {
        id_name: "MPIDR",
        boot,
        ids,
    })
}

/// This processor's hardware identifier: the affinity fields of its `MPIDR`.
pub(crate) fn hardware_id() -> u64 {
    u64::from(cpu::read_mpidr()) & MPIDR_AFFINITY
}

// The entry sequence. `r0` is the physical address of a `StartBlock`, and the
// MMU and caches are off, so every load here is of memory rather than of this
// core's cache — which is why the block is cleaned to the point of coherency
// before the core is started.
//
// All nine words are loaded in the first instruction, *before* the MMU goes
// on: after it, `r0` is a physical address with nothing mapped at it. The
// instruction cache and the branch predictor are invalidated before the MMU
// goes on, as the loader does, because whatever they hold predates this
// kernel.
core::arch::global_asm!(
    r#"
.arm
.section .text
.balign 32
.globl ferrix_secondary_entry
ferrix_secondary_entry:
    mrs   r6, cpsr
    and   r6, r6, #0x1f
    cmp   r6, #0x13
    bne   ferrix_secondary_parked
    ldm   r0, {{r1-r5, r8-r10, r12}}
    mov   r6, #0
    mcr   p15, 0, r6, c7, c5, 0
    mcr   p15, 0, r6, c7, c5, 6
    mcr   p15, 0, r1, c10, c2, 0
    mcr   p15, 0, r2, c10, c2, 1
    mcr   p15, 0, r3, c2, c0, 2
    mcrr  p15, 0, r4, r6, c2
    mcrr  p15, 1, r5, r6, c2
    isb
    mcr   p15, 0, r6, c8, c7, 0
    dsb
    isb
    mcr   p15, 0, r8, c1, c0, 0
    isb
    mov   sp, r9
    mov   r0, r12
    mov   r11, #0
    mov   lr, #0
    bx    r10
ferrix_secondary_parked:
    wfi
    b     ferrix_secondary_parked
.globl ferrix_secondary_entry_end
ferrix_secondary_entry_end:
"#
);

unsafe extern "C" {
    /// Where PSCI starts a secondary core, by its physical address.
    static ferrix_secondary_entry: [u8; 0];
    /// One past the entry sequence's last instruction.
    static ferrix_secondary_entry_end: [u8; 0];
}

/// What a starting core reads, with its MMU off, to get its MMU on.
///
/// Field order is the `ldm`'s: registers load in ascending order from
/// ascending addresses, so the order here is the order of `r1`–`r5`, `r8`–`r10`
/// and `r12` in the entry sequence.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct StartBlock {
    /// Memory attributes, copied from this core: `r1`.
    mair0: u32,
    /// The other half of them: `r2`.
    mair1: u32,
    /// Translation control, copied from this core with the lower half's walk
    /// enabled, since it is the identity map's: `r3`.
    ttbcr: u32,
    /// The identity map's root: `r4`.
    ttbr0: u32,
    /// The kernel's root plus [`TTBR1_OFFSET`]: `r5`.
    ttbr1: u32,
    /// System control, copied from this core — MMU and caches on, vectors at
    /// `VBAR`: `r8`.
    sctlr: u32,
    /// The new core's SVC stack: `r9`.
    stack_top: u32,
    /// Where to go once the MMU is on: [`secondary_start`], by virtual
    /// address: `r10`.
    entry: u32,
    /// Its argument, the new core's per-CPU record: `r12`.
    argument: u32,
    /// Padding to a multiple of eight bytes; not loaded.
    reserved: u32,
}

const _: () = assert!(
    size_of::<StartBlock>() == 40,
    "the entry sequence loads nine words from the start block"
);

/// A physical or virtual address as the 32-bit register it has to fit.
///
/// Every address here is below 4 GiB by construction — the RAM of every
/// machine this kernel runs on, and the whole of its address space — so this
/// fails only if that stops being true, and says so rather than truncating.
fn register(value: u64, what: &'static str) -> Result<u32, &'static str> {
    u32::try_from(value).map_err(|_| what)
}

/// Where the identity mapping the entry sequence runs through lives.
#[derive(Clone, Copy, Debug)]
enum Identity {
    /// A `TTBR0` tree of this starter's own, which a started core leaves
    /// behind by disabling `TTBR0` as soon as it is running virtually.
    Own {
        /// Its root, by physical address.
        root: u64,
    },
    /// The kernel's own tree, because the entry sequence is at an address in
    /// the kernel's half and `TTBR0` translates none of those. The mapping is
    /// still transient — [`CpuStarter::finish`] takes it down — but while it
    /// exists every processor can see it.
    Kernel,
}

/// Map the entry sequence at its own address, in whichever tree can hold it.
fn install_identity(base: u64, len: u64) -> Result<Identity, &'static str> {
    // Not global: these entries are retired while the system runs, and a
    // global one would be free to survive in a TLB past the invalidation that
    // retires it.
    let flags = MapFlags {
        global: false,
        ..MapFlags::KERNEL_CODE
    };

    if base + len > LOWER_HALF_END {
        crate::mm::map_kernel(base, base, len, flags)
            .map_err(|_| "could not identity map the secondary entry sequence")?;
        return Ok(Identity::Kernel);
    }

    let root_frame =
        crate::mm::allocate_frames(0).ok_or("no frame for the secondary cores' identity map")?;
    crate::mm::zero_frame(root_frame);
    let root = root_frame * PAGE_SIZE;
    crate::mm::map_in(root, base, base, len, flags)
        .map_err(|_| "could not identity map the secondary entry sequence")?;
    Ok(Identity::Own { root })
}

/// Starts secondary cores, one at a time.
#[derive(Debug)]
pub(crate) struct CpuStarter {
    /// How to reach PSCI.
    conduit: PsciConduit,
    /// Where PSCI starts a core: the entry sequence's physical address.
    entry_phys: u32,
    /// First byte of the identity-mapped span.
    identity_base: u64,
    /// Bytes of it.
    identity_len: u64,
    /// Which tree holds it.
    identity: Identity,
    /// The frame the start block is written to, by physical address.
    block: u64,
    /// Everything in the start block that is the same for every core.
    template: StartBlock,
}

impl CpuStarter {
    /// Build the identity map and the start block's template, from this
    /// core's own registers.
    pub(crate) fn new(view: &BootView<'_>) -> Result<CpuStarter, &'static str> {
        let conduit = super::psci_conduit().ok_or(
            "the device tree names no PSCI conduit, and PSCI is how this kernel starts a core",
        )?;

        let info = view.raw();
        let physical = |virt: u64| virt - info.kernel_virt + info.kernel_phys;
        let entry_phys = physical((&raw const ferrix_secondary_entry).addr() as u64);
        let end_phys = physical((&raw const ferrix_secondary_entry_end).addr() as u64);
        let identity_base = entry_phys - entry_phys % PAGE_SIZE;
        let identity_len = (end_phys - identity_base).next_multiple_of(PAGE_SIZE);
        let identity = install_identity(identity_base, identity_len)?;

        let block = crate::mm::allocate_frames(0)
            .ok_or("no frame for the secondary start block")?
            * PAGE_SIZE;

        Ok(CpuStarter {
            conduit,
            entry_phys: register(entry_phys, "the secondary entry is above 4 GiB")?,
            identity_base,
            identity_len,
            identity,
            block,
            template: StartBlock {
                mair0: cpu::read_mair0(),
                mair1: cpu::read_mair1(),
                // The lower half walks only where the identity map is a tree
                // of its own. Where it is the kernel's, `TTBR0` stays off and
                // the entry sequence is translated through `TTBR1` like
                // everything else the kernel runs.
                ttbcr: match identity {
                    Identity::Own { .. } => cpu::read_ttbcr() & !cpu::TTBCR_EPD0,
                    Identity::Kernel => cpu::read_ttbcr(),
                },
                ttbr0: match identity {
                    Identity::Own { root } => {
                        register(root, "the identity map's root is above 4 GiB")?
                    }
                    Identity::Kernel => 0,
                },
                ttbr1: register(
                    crate::mm::root_table() + TTBR1_OFFSET,
                    "the kernel's root table is above 4 GiB",
                )?,
                sctlr: cpu::read_sctlr(),
                stack_top: 0,
                entry: secondary_start as extern "C" fn(u32) -> ! as usize as u32,
                argument: 0,
                reserved: 0,
            },
        })
    }

    /// Start the core `hardware_id` on `stack_top`, handing it `argument`.
    ///
    /// Returns once PSCI has accepted the request, not once the core is
    /// running: the caller waits for that, and must before starting another,
    /// because every core reads the same start block.
    pub(crate) fn start(
        &mut self,
        hardware_id: u64,
        stack_top: u64,
        argument: u64,
    ) -> Result<(), &'static str> {
        let block = StartBlock {
            stack_top: register(stack_top, "a secondary stack is above 4 GiB")?,
            argument: register(argument, "a per-CPU record is above 4 GiB")?,
            ..self.template
        };
        let at = crate::mm::direct_map(self.block);
        // SAFETY: (FRAME) `self.block` is a frame this starter allocated and nothing
        // else refers to; the direct map makes it writable, and a frame is
        // aligned for anything.
        unsafe { (at as *mut StartBlock).write(block) };
        // The core that reads this has its caches off, so it sees memory —
        // not this core's cache, where the write above still is.
        cpu::clean_to_poc(at, size_of::<StartBlock>() as u64);

        let target = register(hardware_id, "an MPIDR wider than thirty-two bits")?;
        let context = register(self.block, "the start block is above 4 GiB")?;
        match psci(self.conduit, PSCI_CPU_ON, target, self.entry_phys, context) {
            0 => Ok(()),
            -2 => Err("PSCI refused the start request's parameters"),
            -4 => Err("PSCI says the core is already on"),
            -5 => Err("PSCI says the core is already being started"),
            -9 => Err("PSCI refused the entry point's address"),
            _ => Err("PSCI could not start the core"),
        }
    }

    /// Take down the identity map and the start block.
    ///
    /// Only once every core that was started has reported in: each one leaves
    /// the identity map before it does.
    pub(crate) fn finish(self) -> Result<(), &'static str> {
        const FAILED: &str = "could not take down the secondary cores' identity map";

        match self.identity {
            Identity::Own { root } => {
                crate::mm::unmap_unwalked(root, self.identity_base, self.identity_len)
                    .map_err(|_| FAILED)?;
                crate::mm::deallocate_frames(root / PAGE_SIZE, 0);
            }
            // Nothing is freed but the tables, which `unmap_kernel` gives back
            // itself: what this mapping pointed at is the kernel's own text.
            Identity::Kernel => {
                let _ = crate::mm::unmap_kernel(self.identity_base, self.identity_len, |_, _| {})
                    .map_err(|_| FAILED)?;
            }
        }
        crate::mm::deallocate_frames(self.block / PAGE_SIZE, 0);
        report_coherency();
        Ok(())
    }
}

/// Say what the processors' coherency bits looked like, once they are all up.
///
/// Never a failure on its own, and deliberately careful about which of the
/// three answers is actually alarming. Stage 4 gives the real verdict a few
/// lines further down by making every core share one counter; this says, in
/// advance, what to suspect if that goes wrong.
///
/// # Why "clear everywhere" is not an alarm
///
/// Reading zero from every core has two causes that cannot be told apart by
/// reading: firmware left the bit alone, or nothing implements the register.
/// QEMU is the second — `cortex-a7` there does not model `ACTLR`, so every
/// boot test in this repository reports zero and then passes stage 4, because
/// emulated memory is coherent whatever the bit says. Printing "unsound" for
/// that would put a false alarm in the log of every green CI run, and a log
/// that cries wolf on every run is one nobody reads on the run that matters.
///
/// # Why "set on some but not all" is
///
/// That one is unambiguous. The register is implemented, firmware set it for
/// at least one core, and the cores it missed will run incoherently with the
/// ones it did not. There is no reading of that which is benign.
fn report_coherency() {
    let seen = COHERENCY_SEEN.load(Ordering::Relaxed);
    let set = COHERENCY_SET.load(Ordering::Relaxed);

    if seen == 0 {
        crate::println!("  coherency ACTLR not read; `noactlr` was on the command line");
    } else if seen == 1 {
        // Nothing to be coherent with.
        crate::println!("  coherency one processor, ACTLR.SMP {}", yes_no(set == 1));
    } else if set == seen {
        crate::println!("  coherency ACTLR.SMP set on all {seen} processors");
    } else if set == 0 {
        crate::println!(
            "  coherency ACTLR.SMP clear on all {seen} processors — firmware left it, or the \
             register is not implemented; stage 4 below is the arbiter"
        );
    } else {
        crate::println!(
            "  coherency ACTLR.SMP set on {set} of {seen} processors — the {} without it are \
             not coherent with the rest; expect stage 4 to lose counts",
            seen - set,
        );
    }
}

/// `set` or `clear`, for a bit being reported to a human.
const fn yes_no(set: bool) -> &'static str {
    if set { "set" } else { "clear" }
}

/// Make a PSCI call and return its status.
fn psci(conduit: PsciConduit, function: u32, a: u32, b: u32, c: u32) -> i32 {
    // SAFETY: (FIRMWARE) the only function this module calls is `CPU_ON`, whose entry
    // point is the sequence above and whose argument is a start block this
    // module wrote.
    let status = unsafe { cpu::psci_call(conduit, function, a, b, c) };
    status as i32
}

/// Where a secondary core arrives once the entry sequence has its MMU on.
///
/// In SVC mode, in the upper half, on its own stack, with every exception
/// masked and the identity map still installed.
extern "C" fn secondary_start(record: u32) -> ! {
    // SAFETY: (TRANSLATE) nothing from here on executes or reads through the lower half.
    // The identity map was for the instructions before the branch here, and
    // this also invalidates what the walk through it left in this core's TLB.
    unsafe { cpu::disable_ttbr0() };
    // SAFETY: (ENTRY) once on this core, before anything on it can fault, with every
    // exception masked. `trap::init` writes only this core's own `VBAR` and
    // `SCTLR` bits; the table it points them at is code every core shares.
    unsafe { super::trap::init() };
    gicv2::init_this_cpu();
    cpu::allow_user_counter();
    note_coherency();
    crate::smp::install_secondary_record(u64::from(record));
    // The boot core's side-channel defences, before this one can run a
    // program: after the record, which is where it says what it applied.
    super::speculation::apply_this_cpu();
    // Its own ASID flush plan, from its own registers, before it can run a
    // program.
    if let Some(me) = crate::smp::this_cpu() {
        super::asid::init_this_cpu(me.logical);
    }
    crate::smp::secondary_main(u64::from(record))
}
