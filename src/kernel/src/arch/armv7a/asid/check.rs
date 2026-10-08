//! The boot check of ARMv7-A's ASIDs (`docs/OPAQUE-KERNEL.md` §9.13): the
//! `asid` line.
//!
//! What a boot under QEMU can see of them, and one thing only hardware can:
//!
//! * every processor's `TTBCR` has `EAE` set and `A1` clear, so the ASID is
//!   `TTBR0`'s, and each has decided its flush plan; the boot processor's
//!   `CTR` and `ID_MMFR1` are printed, for the board to confirm what QEMU's
//!   model says (B7);
//! * an install writes the space's number, not 0, with the space's root and
//!   bit 0 clear, and `EPD0` clear; an uninstall leaves `TTBR0` zero and
//!   `EPD0` set (L.armv7a.13, L.armv7a.15);
//! * a space's leaves are all non-global (L.armv7a.17);
//! * a forced rollover adds a generation, flushes this processor once, and
//!   every other processor flushes at its next install (L.armv7a.14), each
//!   counted where the flush is made;
//! * the stale probe: a space reads its page under number n, a rollover
//!   follows, another space is given n and must read its own page at the
//!   same address. QEMU empties its TLB at every change of ASID, so under
//!   QEMU this passes whether or not the flush ran; on the board it fails if
//!   the rollover's flush is missing (A11).
//!
//! It runs in stage 6, on the processor that runs the user checks, with
//! interrupts masked around every install, since the scheduler would
//! otherwise switch the space out from under it. Every space it makes is
//! dropped before it returns.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_paging::asid::{generation_of, number_of};
use ferrix_sched::{CpuSet, NICE_0_WEIGHT};
use ferrix_sync::IrqControl;
use ferrix_vma::VmaFlags;

use super::super::cpu;
use crate::arch;
use crate::console::println;
use crate::mm;
use crate::user::space::{Access, AddressSpace};

/// Where the probe's page is mapped in both of its spaces.
const PROBE_AT: u64 = 0x3400_0000;

/// What the two spaces of the probe hold there.
const MARK_A: u64 = 0xA51D_A000_0000_000A;
const MARK_B: u64 = 0xA51D_B000_0000_000B;

/// Installs a forced rollover may take before the check gives up: two
/// generations' worth.
const ROLLOVER_BOUND: usize = 2 * 256;

/// Tries the stale probe makes to have a number reused.
const PROBE_TRIES: usize = 4;

/// Processors that answered the register look, and those whose `TTBCR` or
/// plan was wrong.
static LOOKED: AtomicUsize = AtomicUsize::new(0);
static WRONG: AtomicUsize = AtomicUsize::new(0);
/// The boot processor's `CTR` and `ID_MMFR1`, and whether any other
/// processor's differ.
static CTR: AtomicU32 = AtomicU32::new(0);
static MMFR1: AtomicU32 = AtomicU32::new(0);
static DIFFERENT: AtomicBool = AtomicBool::new(false);

/// `TTBCR.EAE` and `TTBCR.A1`.
const TTBCR_EAE: u32 = 1 << 31;
const TTBCR_A1: u32 = 1 << 22;

/// One processor's own registers.
fn look(me: &'static crate::smp::PerCpu) {
    let ttbcr = cpu::read_ttbcr();
    if ttbcr & TTBCR_EAE == 0
        || ttbcr & TTBCR_A1 != 0
        || super::plan(me.logical) & super::PLAN_DECIDED == 0
    {
        let _ = WRONG.fetch_add(1, Ordering::Relaxed);
    }
    let (ctr, mmfr1) = (cpu::read_ctr(), cpu::read_id_mmfr1());
    if me.logical == 0 {
        CTR.store(ctr, Ordering::Relaxed);
        MMFR1.store(mmfr1, Ordering::Relaxed);
    } else if ctr != CTR.load(Ordering::Relaxed) || mmfr1 != MMFR1.load(Ordering::Relaxed) {
        DIFFERENT.store(true, Ordering::Relaxed);
    }
    let _ = LOOKED.fetch_add(1, Ordering::Relaxed);
}

/// A space with one page at [`PROBE_AT`] holding `mark`.
fn space_with(mark: u64) -> Result<Arc<AddressSpace>, &'static str> {
    let space = AddressSpace::new().map_err(|_| "asid: could not make an address space")?;
    let _ = space
        .map_anonymous(PROBE_AT, PAGE_SIZE, VmaFlags::READ_WRITE)
        .map_err(|_| "asid: mapping failed")?;
    space
        .fault(PROBE_AT, Access::WRITE)
        .map_err(|_| "asid: a fault in a mapped region was not resolved")?;
    let phys = mm::translate_in(space.root_table(), PROBE_AT)
        .ok_or("asid: a faulted page does not translate")?;
    // SAFETY: (FRAME) the frame was just faulted in for this space, which nothing else
    // has seen, and the direct map covers every frame of RAM.
    unsafe { (mm::direct_map(phys) as *mut u64).write_volatile(mark) };
    Ok(space)
}

/// What an install of `space` left in the registers, and the value read
/// through [`PROBE_AT`] if `read`: (number written, root right, bit 0 clear,
/// `EPD0` clear, value).
fn install_and_look(space: &AddressSpace, read: bool) -> (u8, bool, bool, bool, u64) {
    let state = <arch::Irq as IrqControl>::disable();
    // SAFETY: (TRANSLATE) `space` is borrowed across the window, interrupts are masked,
    // and it is uninstalled below before anything else can want a user address.
    unsafe { space.install(None) };
    let ttbr0 = cpu::read_ttbr0();
    let epd0_clear = cpu::read_ttbcr() & cpu::TTBCR_EPD0 == 0;
    let value = if read {
        arch::permit_user_access();
        // SAFETY: (PROBE) `space` maps `PROBE_AT` and is installed on this processor.
        let seen = unsafe { (PROBE_AT as *const u64).read_volatile() };
        arch::forbid_user_access();
        seen
    } else {
        0
    };
    // SAFETY: (TRANSLATE) nothing after this wants a user address.
    unsafe { space.uninstall() };
    <arch::Irq as IrqControl>::restore(state);
    (
        ((ttbr0 >> 48) & 0xFF) as u8,
        ttbr0 & cpu::TTBR_ADDRESS == space.root_table(),
        ttbr0 & 1 == 0,
        epd0_clear,
        value,
    )
}

/// Install `spender` with a fresh number, again and again, until the
/// allocator rolls over or `stop` says so; the rollovers seen.
fn spend_until(spender: &AddressSpace, mut stop: impl FnMut() -> bool) -> u64 {
    let before = super::counts(0).rollovers;
    for _ in 0..ROLLOVER_BOUND {
        if stop() {
            break;
        }
        spender.tag.forget();
        let _ = install_and_look(spender, false);
    }
    super::counts(0).rollovers - before
}

/// Set by [`flush_on`]'s task when it has run in its space.
static RAN: AtomicBool = AtomicBool::new(false);

/// The task [`flush_on`] starts: installed in its space, which is what
/// makes its processor flush, it looks at its processor's registers.
fn ran(_: usize) {
    let saved = <arch::Irq as IrqControl>::disable();
    if let Some(me) = crate::smp::this_cpu() {
        look(me);
    }
    <arch::Irq as IrqControl>::restore(saved);
    RAN.store(true, Ordering::Release);
}

/// Make processor `cpu` install a space, by starting a task there in one.
fn flush_on(cpu: usize, space: &Arc<AddressSpace>) -> Result<(), &'static str> {
    RAN.store(false, Ordering::Release);
    let task = crate::sched::spawn_on_in(
        "asid-flush",
        ran,
        0,
        NICE_0_WEIGHT,
        cpu,
        CpuSet::of(cpu),
        Some(Arc::clone(space)),
    )
    .map_err(|_| "asid: could not start a task on another processor")?;
    let deadline = crate::timer::now_nanos().saturating_add(20_000_000_000);
    while !RAN.load(Ordering::Acquire) {
        if crate::timer::now_nanos() >= deadline {
            return Err("asid: a task on another processor never ran");
        }
        crate::sched::yield_now();
    }
    drop(task);
    Ok(())
}

/// Run the check, and print the `asid` line.
///
/// # Errors
///
/// The first thing that did not hold.
///
/// Verifies: L.armv7a.13
/// Verifies: L.armv7a.14
/// Verifies: L.armv7a.15
/// Verifies: L.armv7a.16
/// Verifies: L.armv7a.17
/// Verifies: L.armv7a.20
pub(crate) fn run() -> Result<(), &'static str> {
    for count in [&LOOKED, &WRONG] {
        count.store(0, Ordering::Relaxed);
    }
    // The other processors look at theirs from a task each, after the
    // rollover below: `run_everywhere`'s work is taken only before the
    // scheduler starts.
    let saved = <arch::Irq as IrqControl>::disable();
    let me = crate::smp::this_cpu().ok_or("asid: no per-CPU record");
    if let Ok(me) = me {
        look(me);
    }
    <arch::Irq as IrqControl>::restore(saved);
    if me?.logical != 0 {
        return Err("asid: the check runs on the boot processor only");
    }
    let processors = crate::smp::count();
    // L.armv7a.16: a processor that switched away keeps a space's entries
    // under its number, and only a broadcast shootdown reaches it.
    if !arch::TLB_FLUSH_IS_BROADCAST {
        return Err(
            "asid: shootdowns are not a broadcast, so a processor that left a space's set keeps its entries",
        );
    }

    let a = space_with(MARK_A)?;
    let b = space_with(MARK_B)?;
    let spender = AddressSpace::new().map_err(|_| "asid: could not make an address space")?;

    // An install and an uninstall, read back from the registers.
    let (number, root_right, bit0_clear, epd0_clear, seen) = install_and_look(&a, true);
    if number == 0 || u64::from(number) != a.tag.get() & 0xFF {
        return Err("asid: an install did not write the space's ASID");
    }
    if !root_right || !bit0_clear || !epd0_clear {
        return Err("asid: an install did not write the space's root, or left EPD0 set");
    }
    if seen != MARK_A {
        return Err("asid: a space read something else than its own page");
    }
    if cpu::read_ttbr0() != 0 || cpu::read_ttbcr() & cpu::TTBCR_EPD0 == 0 {
        return Err("asid: an uninstall left TTBR0 non-zero or EPD0 clear");
    }

    // Every leaf of a user space is non-global.
    let (leaves, global) = global_leaves(a.root_table());
    if leaves == 0 || global != 0 {
        return Err("asid: a user space has a global leaf, or none at all");
    }

    // A forced rollover: this processor flushes once, at the install that
    // rolled over; every other processor at its next install.
    let others: alloc::vec::Vec<(usize, u64)> = (1..processors)
        .map(|cpu| (cpu, super::counts(cpu).flushes))
        .collect();
    let before = super::counts(0);
    let _ = spend_until(&spender, || super::counts(0).rollovers > before.rollovers);
    let after = super::counts(0);
    if after.rollovers <= before.rollovers || after.generation <= before.generation {
        return Err("asid: spending a generation's numbers did not roll the allocator over");
    }
    if after.flushes < before.flushes + (after.rollovers - before.rollovers) {
        return Err("asid: a rollover left this processor unflushed");
    }
    for &(cpu, flushes) in &others {
        flush_on(cpu, &b)?;
        if super::counts(cpu).flushes <= flushes {
            return Err("asid: another processor ran a new generation without its flush");
        }
    }
    if LOOKED.load(Ordering::Relaxed) != processors {
        return Err("asid: a processor did not report its TTBCR");
    }
    if WRONG.load(Ordering::Relaxed) != 0 {
        return Err("asid: a processor's TTBCR has EAE clear or A1 set, or no flush plan");
    }

    // The stale probe: `a` runs number n, a rollover, then `b` is given n.
    // A number another processor still holds reserved cannot come back, so
    // a few tries; on one processor the first always reuses it.
    let mut reused = None;
    for _ in 0..PROBE_TRIES {
        a.tag.forget();
        let (n, _, _, _, seen) = install_and_look(&a, true);
        if seen != MARK_A {
            return Err("asid: a space read something else than its own page");
        }
        let generation = generation_of(a.tag.get());
        let _ = spend_until(&spender, || super::counts(0).generation > generation);
        let _ = spend_until(&spender, || super::counts(0).next_free == Some(n));
        b.tag.forget();
        let (given, _, _, _, seen) = install_and_look(&b, true);
        if seen != MARK_B {
            return Err("asid: a space given a reused number read the page of the space before it");
        }
        if given == n && number_of(b.tag.get()) == n {
            reused = Some(n);
            break;
        }
    }
    let Some(n) = reused else {
        return Err("asid: the probe never had a number reused by another space");
    };

    let rollovers = super::counts(0).rollovers;
    drop((a, b, spender));
    println!(
        "  asid     ASID from TTBR0 on {processors} of {processors} processors (CTR {:#010x}, \
         ID_MMFR1 {:#010x}{}); install wrote number {number} and uninstall ASID 0 with EPD0; \
         {leaves} user leaves, none global; {rollovers} rollovers, each processor flushed for \
         the new generation; number {n} reused by another space, which read its own page",
        CTR.load(Ordering::Relaxed),
        MMFR1.load(Ordering::Relaxed),
        if DIFFERENT.load(Ordering::Relaxed) {
            ", others differ"
        } else {
            ""
        },
    );
    Ok(())
}

/// The leaves under `root`, and how many of them are global: every ASID
/// argument rests on a user leaf being non-global and, once the loader's
/// alias has gone, a kernel leaf global (L.armv7a.17).
fn global_leaves(root: u64) -> (u64, u64) {
    let mut global = 0;
    let outcome = mm::check::sweep(root, |leaf| {
        if leaf.flags.global {
            global += 1;
        }
        true
    });
    (outcome.leaves, global)
}

/// Once the loader's alias has gone, the kernel's own tree holds no
/// non-global leaf, so `TTBR0`'s tables are the only ones whose entries an
/// ASID tags: what lets one `TTBR0` write change root and ASID together
/// (DDI 0406C.d B3.10.4; `docs/OPAQUE-KERNEL.md` §9.13, item 5's DK1
/// exception). The `global` line, after the W^X sweep.
///
/// # Errors
///
/// A non-global kernel leaf, or no leaf at all.
///
/// Verifies: L.armv7a.17
pub(crate) fn kernel_tree_is_global() -> Result<(), &'static str> {
    let (leaves, global) = global_leaves(mm::root_table());
    if leaves == 0 || global != leaves {
        crate::console::println_unlogged!(
            "  global   {} of the kernel's {leaves} leaves are not global",
            leaves - global
        );
        return Err("the kernel's tree holds a non-global leaf after the loader's alias went");
    }
    println!("  global   {leaves} kernel leaves, every one global");
    Ok(())
}
