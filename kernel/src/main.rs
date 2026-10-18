//! The Ferrix kernel.
//!
//! Entered from the UEFI loader with the MMU on, three mappings in place and
//! nothing else: no interrupt vectors, no allocator, no other CPU running. What
//! stage 1 does with that is prove the hand-off is sound and say so over the
//! serial port, which is the smallest thing that can honestly be called booting.
//!
//! See `docs/ROADMAP.md` for what comes next.

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod arch;
mod console;
mod early;
mod irq;
mod mm;
mod mmio;
mod timer;
mod trap;
mod vmap;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::panic::PanicInfo;

use ferrix_bootinfo::{BootInfo, BootView, KERNEL_VMAP_BASE, MemKind, PAGE_SIZE};
use ferrix_paging::MapFlags;

use console::println;
use early::EarlyMemory;

/// What the boot test waits for. Changing it means changing
/// `xtask/src/qemu.rs`, and the two are checked against each other there.
const SUCCESS_MARKER: &str = "FERRIX-BOOT-OK";

/// The kernel's entry point.
///
/// The loader calls this with the boot info pointer as its only argument; the
/// signature is [`ferrix_bootinfo::KernelEntry`], declared in the crate both
/// sides share so that a mismatch is a type error rather than a triple fault.
#[unsafe(no_mangle)]
#[unsafe(link_section = ".text.entry")]
extern "C" fn _start(boot_info: *const BootInfo) -> ! {
    // Nothing has been checked yet, including whether this pointer is a
    // `BootInfo` at all — so `validate` is the first thing that runs, and it
    // checks the magic before it follows anything.
    //
    // SAFETY: the loader passes a pointer to a `BootInfo` it built, inside the
    // direct map, immutable for the life of the system.
    let info = unsafe { &*boot_info };
    // SAFETY: the same structure, so its `regions` and `cmdline` point at the
    // arrays the loader wrote beside it, for as long as the system runs.
    let Ok(view) = (unsafe { info.validate() }) else {
        // No console yet, and no way to make one without a valid hand-off.
        arch::halt()
    };

    let mut memory = EarlyMemory::new(&view);
    if arch::init_console(&mut memory).is_err() {
        arch::halt()
    }
    // SAFETY: `init_console` configured the port and, on AArch64, mapped it.
    unsafe { console::mark_ready() };

    kmain(&view, &mut memory)
}

/// The kernel proper.
fn kmain(view: &BootView<'_>, memory: &mut EarlyMemory) -> ! {
    println!();
    println!("Ferrix {} on {}", env!("CARGO_PKG_VERSION"), arch::NAME);

    report(view);

    if let Err(problem) = self_check(view, memory) {
        println!("FERRIX-PANIC stage 1 self-check failed: {problem}");
        arch::halt()
    }
    println!("  stage 1  loader hand-off verified");

    // Before anything else, and before anything can fault: until this runs the
    // CPU is still pointing at firmware's handlers, which stopped existing at
    // `exit_boot_services`. A fault in that window is a jump into reclaimed
    // memory, which on x86-64 is a triple fault and a silent reset.
    //
    // SAFETY: called exactly once, on the boot CPU, with interrupts masked.
    unsafe { arch::init_traps() };
    println!("  traps    vectors installed");

    let stats = match mm::init(view) {
        Ok(stats) => stats,
        Err(problem) => {
            println!("FERRIX-PANIC could not bring up memory: {problem}");
            arch::halt()
        }
    };
    report_memory(&stats);

    if let Err(problem) = vmap::init() {
        println!("FERRIX-PANIC could not bring up the kernel address arena: {problem}");
        arch::halt()
    }

    if let Err(problem) = memory_check(&stats, view.raw().kernel_phys) {
        println!("FERRIX-PANIC stage 2 self-check failed: {problem}");
        arch::halt()
    }
    println!("  stage 2  frame allocator, heap and vmap arena verified");

    if let Err(problem) = trap_check() {
        println!("FERRIX-PANIC stage 3 self-check failed: {problem}");
        arch::halt()
    }

    // Everything above is synchronous: traps the kernel caused deliberately.
    // From here something arrives that the kernel did not ask for at the
    // moment it arrives, which is the whole difference between a program and
    // an operating system.
    //
    // SAFETY: called once, on the boot CPU, after `init_traps` filled the
    // vector table and while interrupts are still masked.
    let clocks = match unsafe { arch::init_interrupts(view) } {
        Ok(report) => report,
        Err(problem) => {
            println!("FERRIX-PANIC could not bring up interrupts: {problem}");
            arch::halt()
        }
    };
    report_clocks(&clocks);

    if let Err(problem) = timer::init() {
        println!("FERRIX-PANIC could not register the timer interrupt: {problem}");
        arch::halt()
    }
    arch::enable_interrupts();

    let measured = match timer_check() {
        Ok(hertz) => hertz,
        Err(problem) => {
            println!("FERRIX-PANIC stage 3 self-check failed: {problem}");
            arch::halt()
        }
    };
    println!(
        "  stage 3  {} breakpoints, {} page faults, {} ticks at {} Hz",
        trap::breakpoint_count(),
        trap::handled_fault_count(),
        timer::ticks(),
        measured,
    );

    // The rest of stage 2, deliberately last. Each of these needs something a
    // later part of boot brought up — the arena needs the heap, the sweep
    // needs every mapping the kernel is ever going to make, and reclaiming
    // ACPI memory needs the tables to have been read, which happened in
    // `init_interrupts` above.
    if let Err(problem) = finish_memory(view) {
        println!("FERRIX-PANIC stage 2 self-check failed: {problem}");
        arch::halt()
    }

    println!("{SUCCESS_MARKER} stages 1-3");
    arch::shutdown()
}

/// The half of stage 2 that cannot run until the rest of boot has.
///
/// Three things, in an order that is forced rather than chosen:
///
/// 1. the loader's identity map goes, which is what proves the kernel is
///    genuinely higher-half rather than accidentally depending on a low
///    address somewhere;
/// 2. the W^X sweep runs, which can only pass *after* step 1 — the identity
///    map has to be writable and executable, because the instruction after the
///    page table switch is fetched through it;
/// 3. the memory early boot has finished with goes back to the allocator.
fn finish_memory(view: &BootView<'_>) -> Result<(), &'static str> {
    // Before: the sweep must be able to *see* a violation, or its passing
    // afterwards means nothing. The loader's identity map is one, by
    // construction, so this is a test of the test.
    if mm::check_w_xor_x(view).is_ok() {
        return Err("the W^X sweep cannot see the loader's identity map");
    }

    // SAFETY: the kernel executes, and reaches its stack and the hand-off,
    // entirely through the upper half. Nothing has held a lower-half address
    // since `_start`.
    unsafe { arch::drop_identity_map(view) };

    // And nothing in the lower half resolves any more, which is the claim
    // "higher-half" actually makes. Address zero specifically: a null
    // dereference in kernel code must fault rather than find the first page of
    // physical memory, which under the identity map it would have.
    if mm::translate(0).is_some() {
        return Err("the identity map outlived the call that dropped it");
    }

    let wx = match mm::check_w_xor_x(view) {
        Ok(report) => report,
        Err(found) => {
            // Printed rather than counted: one offending mapping is enough,
            // and an address is what makes it findable. A count would say
            // there is a problem without saying where.
            println!(
                "  w^x      {:#x} is writable and executable, {} bytes of it",
                found.virt, found.len
            );
            return Err("a mapping is both writable and executable");
        }
    };
    if wx.executable == 0 {
        return Err("the sweep found no executable mapping at all, so it swept nothing");
    }
    println!(
        "  w^x      {} mappings swept, {} executable, none writable",
        wx.leaves, wx.executable
    );

    // SAFETY: called once, after the last use of `crate::acpi::Firmware` —
    // interrupt bring-up above is the only reader — and the loader's code has
    // not run since the jump into `_start`.
    let reclaimed = unsafe { mm::reclaim_boot_memory(view) };
    let usage = vmap::usage();
    println!(
        "  reclaim  {} MiB from the loader and ACPI, {} free; arena {} live, {} KiB",
        reclaimed.total() * 4 / 1024,
        mm::free_frames() * 4 / 1024,
        usage.allocations,
        usage.bytes / 1024,
    );
    if reclaimed.total() == 0 {
        return Err("nothing was reclaimed, so the memory map describes no early boot");
    }
    Ok(())
}

/// Stage 3's exit criterion: the kernel can take a trap and carry on.
///
/// Two things, and the second is the one that matters. A breakpoint proves the
/// whole entry path works — vector, register save, dispatch, restore, return —
/// because execution continues on the next instruction with every register
/// intact. A page fault proves the kernel can *resolve* a fault and let the
/// faulting instruction retry, which is exactly what demand paging is, and is
/// how every anonymous mapping will work from stage 6.
fn trap_check() -> Result<(), &'static str> {
    check_breakpoint()?;
    check_demand_paging()
}

/// A breakpoint must return to the instruction after it, twice.
fn check_breakpoint() -> Result<(), &'static str> {
    let before = trap::breakpoint_count();

    // A canary in a register the trap frame saves and restores. If the entry
    // path drops a register, this is what notices.
    let canary: u64 = 0x0123_4567_89AB_CDEF;
    let mut witness = canary;

    arch::breakpoint();
    witness = witness.rotate_left(1);
    arch::breakpoint();

    if trap::breakpoint_count() != before + 2 {
        return Err("a breakpoint did not reach the handler");
    }
    if witness != canary.rotate_left(1) {
        return Err("a register did not survive the trap");
    }
    Ok(())
}

/// A fault in the on-demand window must be resolved by mapping a page.
fn check_demand_paging() -> Result<(), &'static str> {
    let before = trap::handled_fault_count();
    let free_before = mm::free_frames();

    // Three pages, touched out of order, so a handler that mapped a fixed
    // address rather than the faulting one would fail here.
    let probes = [
        mm::DEMAND_WINDOW + 0x2000,
        mm::DEMAND_WINDOW,
        mm::DEMAND_WINDOW + 0x1000,
    ];

    for (index, probe) in probes.iter().enumerate() {
        if mm::translate(*probe).is_some() {
            return Err("the on-demand window was already mapped");
        }

        let value = 0xFEED_0000_u64 + index as u64;
        // SAFETY: nothing is mapped here, which is the point: the write takes a
        // page fault, the handler maps a zeroed page, and the CPU retries the
        // instruction. `volatile` so the compiler cannot decide the write is
        // dead and remove the fault along with it.
        unsafe { core::ptr::write_volatile(*probe as *mut u64, value) };
        // SAFETY: the page is mapped now, by the fault the write above took.
        let read_back = unsafe { core::ptr::read_volatile(*probe as *const u64) };

        if read_back != value {
            return Err("memory faulted in did not hold what was written to it");
        }
        if mm::translate(*probe).is_none() {
            return Err("the fault handler did not leave a mapping behind");
        }
    }

    let handled = trap::handled_fault_count() - before;
    if handled != probes.len() as u64 {
        return Err("the number of faults handled does not match the pages touched");
    }

    // Each fault consumes a frame for the page itself, and the *first* one into
    // a fresh region also consumes frames for the page tables above it -- three
    // of them here, since nothing was mapped in this window at all. So the
    // total is bounded rather than exact.
    let consumed = free_before.saturating_sub(mm::free_frames());
    if consumed < probes.len() as u64 {
        return Err("faulting in pages consumed fewer frames than pages");
    }
    if consumed > probes.len() as u64 + 3 {
        return Err("faulting in pages consumed more frames than pages plus a table per level");
    }

    // What *is* exact: a fault into a region whose tables already exist costs
    // one frame and no more. Checking it separately is what makes the bound
    // above a measurement rather than a shrug.
    let settled = mm::free_frames();
    let neighbour = mm::DEMAND_WINDOW + 0x3000;
    // SAFETY: unmapped, so this faults; the handler maps a zeroed page and the
    // instruction retries.
    unsafe { core::ptr::write_volatile(neighbour as *mut u64, 1) };
    if mm::free_frames() != settled - 1 {
        return Err("a fault into an already-tabled region cost more than one frame");
    }

    // The rest of the page must read as zero: a page handed out still holding
    // the last owner's data is an information leak, and from stage 6 the last
    // owner is another process.
    // SAFETY: mapped by the faults above.
    let tail = unsafe { core::ptr::read_volatile((mm::DEMAND_WINDOW + 0x800) as *const u64) };
    if tail != 0 {
        return Err("a faulted-in page was not zeroed");
    }
    Ok(())
}

/// A one-shot must fire exactly once.
///
/// **This is the check that catches the bug worth catching here.** AArch64's
/// timer interrupt is level triggered: the line stays asserted while the
/// comparator is in the past, so a handler that acknowledges the controller
/// without disarming the timer is re-entered immediately, forever. That does
/// not show up as a wrong number — it shows up as a machine that stops, with
/// the last thing in the log being whatever it printed before arming.
///
/// So: arm once, wait for the tick, then wait several further intervals and
/// require the count not to have moved.
fn check_one_shot(interval_nanos: u64) -> Result<(), &'static str> {
    let before = timer::ticks();
    timer::after(interval_nanos);

    let mut spins: u64 = 0;
    while timer::ticks() == before {
        arch::wait_for_interrupt();
        spins = spins.saturating_add(1);
        if spins > 10_000_000 {
            timer::stop();
            return Err("a one-shot timer never fired");
        }
    }

    let settled = timer::ticks();
    spin_nanos(interval_nanos.saturating_mul(10));
    if timer::ticks() != settled {
        timer::stop();
        return Err("a one-shot timer fired more than once");
    }
    Ok(())
}

/// Spin on the counter for `nanos`.
///
/// Deliberately not `wait_for_interrupt`: the point is to let real time pass
/// while *not* waiting for a timer, so that a timer which fires anyway is
/// noticed.
fn spin_nanos(nanos: u64) {
    let until = timer::now_nanos().saturating_add(nanos);
    while timer::now_nanos() < until {}
}

/// Print what interrupt and time bring-up found.
fn report_clocks(report: &irq::Report) {
    let (counter_mhz, counter_thousandths) = megahertz(report.counter_hz);
    let (timer_mhz, timer_thousandths) = megahertz(report.timer_hz);
    println!(
        "  clock    {} at {}.{:03} MHz",
        report.counter, counter_mhz, counter_thousandths
    );
    println!(
        "  irqs     {}, {} at {}.{:03} MHz",
        report.controller, report.timer, timer_mhz, timer_thousandths
    );
}

/// A frequency split into megahertz and thousandths of one.
///
/// There is no floating point in this kernel and there is not going to be:
/// the state a kernel has to save and restore on every trap is large enough
/// without it. Two integers and a `{:03}` say the same thing.
const fn megahertz(hz: u64) -> (u64, u64) {
    (hz / 1_000_000, (hz % 1_000_000) / 1000)
}

/// The rest of stage 3's exit criterion: arm a timer, count a thousand ticks,
/// and require the rate to be the one that was asked for.
///
/// The measurement is what makes this a test rather than a demonstration. A
/// timer that fires is easy; a timer that fires at the frequency it was
/// programmed to is the thing every later stage depends on, because a
/// scheduler quantum, a `TCP` retransmit and a `futex` timeout are all this
/// number multiplied by something.
///
/// Ticks are counted with the *interrupt* and elapsed time is measured with
/// the *counter* — two independent pieces of hardware on x86-64. Counting
/// ticks and then converting them to seconds by the rate they were programmed
/// at would be arithmetic, not a measurement: it could not fail.
fn timer_check() -> Result<u64, &'static str> {
    /// Ticks to count.
    const TICKS: u64 = 1000;
    /// The interval to ask for, so a thousand ticks is about a second.
    const INTERVAL_NANOS: u64 = 1_000_000;
    /// How far the measured rate may sit from the requested one.
    ///
    /// Generous, and it has to be: the handler re-arms the timer, so every
    /// period carries one interrupt entry and exit, and under an emulator
    /// that is not a rounding error. What this is testing is that the clock
    /// and the timer agree about how long a second is -- not the interrupt
    /// latency, which is stage 14's subject and needs a different test.
    const TOLERANCE_PERCENT: u64 = 25;

    if timer::counter_hz() == 0 {
        return Err("the counter reports no frequency");
    }

    check_one_shot(INTERVAL_NANOS)?;

    let before = timer::ticks();
    let started = timer::now_nanos();
    timer::every(INTERVAL_NANOS);

    let mut spins: u64 = 0;
    while timer::ticks().wrapping_sub(before) < TICKS {
        arch::wait_for_interrupt();
        spins = spins.saturating_add(1);
        // `wait_for_interrupt` can return without one having arrived, so this
        // counts iterations rather than trusting it. Far more than a thousand
        // ticks could need, and far less than the boot test's timeout.
        if spins > 10_000_000 {
            timer::stop();
            return Err("the timer stopped arriving before a thousand ticks");
        }
    }

    let elapsed = timer::now_nanos().saturating_sub(started);
    timer::stop();

    if elapsed == 0 {
        return Err("a thousand ticks took no measurable time");
    }
    if irq::unclaimed() != 0 {
        return Err("an interrupt arrived that nothing had registered for");
    }
    if irq::delivered() < TICKS {
        return Err("fewer interrupts reached a handler than ticks were counted");
    }

    let measured = TICKS * 1_000_000_000 / elapsed;
    let requested = 1_000_000_000 / INTERVAL_NANOS;
    let lowest = requested * (100 - TOLERANCE_PERCENT) / 100;
    let highest = requested * (100 + TOLERANCE_PERCENT) / 100;
    if measured < lowest || measured > highest {
        return Err("the timer and the counter disagree about how long a second is");
    }
    Ok(measured)
}

/// Print what the allocators came up with.
fn report_memory(stats: &mm::Stats) {
    println!(
        "  frames   {} MiB managed, {} MiB free, {} entries at {:#x} ({} KiB)",
        stats.managed_frames * 4 / 1024,
        stats.free_frames * 4 / 1024,
        stats.managed_frames,
        stats.page_array_at,
        stats.page_array_bytes / 1024,
    );
}

/// Stage 2's exit criterion.
///
/// Every one of these is an invariant a later subsystem will assume without
/// checking, because by then there will be no way to check it: a scheduler that
/// gets a `Vec` back with the wrong contents has no idea the heap is at fault.
fn memory_check(stats: &mm::Stats, kernel_phys: u64) -> Result<(), &'static str> {
    if stats.managed_frames == 0 {
        return Err("the frame allocator was given nothing");
    }

    check_frames()?;

    // The heap must come back to where it started, and "where it started" is
    // not zero: the vmap arena below is a live `Vec` and a live `BTreeMap`,
    // and requiring zero afterwards would be requiring the arena not to exist.
    // So the balance is checked around the allocations that are supposed to be
    // transient, before anything permanent is built on top of them.
    let heap_before = mm::heap_allocated();
    let pages_before = mm::heap_pages();
    check_heap()?;
    if mm::heap_allocated() != heap_before {
        return Err("the heap did not give everything back");
    }

    // And gave the *pages* back too, not only the objects. `check_heap` grows
    // a `Vec` to 32 KiB and a `BTreeMap` to two thousand nodes, which is tens
    // of slab pages across several classes; a heap that kept them would pass
    // every other check here and grow monotonically for the life of the
    // system. What it is allowed to keep is one page per size class, which is
    // the rule `ferrix_heap` states.
    let kept = mm::heap_pages().saturating_sub(pages_before);
    if kept > ferrix_heap::CLASSES {
        return Err("the heap kept more slab pages than one per size class");
    }

    check_vmap(kernel_phys)?;
    check_stacks()?;
    Ok(())
}

/// The vmap arena hands out address space, maps it, and takes it back.
///
/// The frame count is what makes this a measurement: an arena that mapped the
/// pages and never unmapped them would pass every read-back check here and
/// leak a frame per page. Requiring the free count to return to exactly where
/// it started is the only assertion that notices.
fn check_vmap(kernel_phys: u64) -> Result<(), &'static str> {
    const PAGES: u64 = 8;

    let free_before = mm::free_frames();

    let first = vmap::allocate(PAGES, MapFlags::KERNEL_DATA).map_err(|_| "vmap refused a range")?;
    let second =
        vmap::allocate(PAGES, MapFlags::KERNEL_DATA).map_err(|_| "vmap refused a second range")?;

    if first.base == second.base {
        return Err("vmap handed out the same address twice");
    }
    if first.base < vmap::ARENA_BASE || second.end() > vmap::ARENA_END {
        return Err("vmap handed out an address outside its own arena");
    }

    // Two allocations may not touch: there is a guard page on each side of
    // each, so even adjacent ones are two pages apart.
    let (low, high) = if first.base < second.base {
        (first, second)
    } else {
        (second, first)
    };
    if high.base < low.end() + 2 * PAGE_SIZE {
        return Err("two vmap allocations are not separated by their guard pages");
    }

    check_vmap_contents(first)?;
    check_vmap_guards(first)?;
    check_vmap_protection(first)?;
    check_device_windows(kernel_phys)?;
    vmap::check_invariants()?;

    vmap::free(first.base).map_err(|_| "vmap refused to free its own allocation")?;
    vmap::free(second.base).map_err(|_| "vmap refused to free its own allocation")?;

    if vmap::free(first.base).is_ok() {
        return Err("vmap freed the same allocation twice");
    }
    if mm::translate(first.base).is_some() {
        return Err("freeing a vmap allocation left the mapping behind");
    }
    if mm::free_frames() != free_before {
        return Err("a vmap allocation leaked frames: the free count moved");
    }
    Ok(())
}

/// Every page of an allocation is mapped, distinct and zeroed.
fn check_vmap_contents(mapping: vmap::Mapping) -> Result<(), &'static str> {
    let pages = mapping.len / PAGE_SIZE;
    let mut previous = None;

    for page in 0..pages {
        let at = mapping.base + page * PAGE_SIZE;
        let phys = mm::translate(at).ok_or("a vmap page is not mapped")?;
        if Some(phys) == previous {
            return Err("two vmap pages resolve to the same frame");
        }
        previous = Some(phys);

        // SAFETY: `at` is inside an allocation this function was handed, so it
        // is mapped writable and nothing else refers to it.
        let existing = unsafe { core::ptr::read_volatile(at as *const u64) };
        if existing != 0 {
            return Err("a vmap page was not zeroed before it was handed out");
        }
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile(at as *mut u64, 0xA11C_0000 + page) };
    }

    for page in 0..pages {
        let at = mapping.base + page * PAGE_SIZE;
        // SAFETY: as above; written a moment ago.
        if unsafe { core::ptr::read_volatile(at as *const u64) } != 0xA11C_0000 + page {
            return Err("a vmap page did not hold what was written to it");
        }
    }
    Ok(())
}

/// Permissions can be changed on a live mapping, and the change is in the
/// tables rather than only in the caller's head.
///
/// Read back by walking the page tables, not by remembering what was asked
/// for: what matters is what the hardware will do, and the whole reason the
/// W^X sweep reads descriptors is that those are two different things.
fn check_vmap_protection(mapping: vmap::Mapping) -> Result<(), &'static str> {
    let before = mm::permissions_of(mapping.base).ok_or("a vmap page has no permissions at all")?;
    if !before.write {
        return Err("a fresh vmap allocation is not writable");
    }

    mm::protect_kernel(mapping.base, PAGE_SIZE, MapFlags::KERNEL_RODATA)
        .map_err(|_| "protecting a vmap page was refused")?;
    let after = mm::permissions_of(mapping.base).ok_or("protecting a page unmapped it")?;
    if after.write {
        return Err("protecting a page read-only left it writable");
    }
    if mm::translate(mapping.base).is_none() {
        return Err("protecting a page moved what it translates to");
    }

    // And back, so the caller's own read-back check below still holds.
    mm::protect_kernel(mapping.base, PAGE_SIZE, MapFlags::KERNEL_DATA)
        .map_err(|_| "restoring a vmap page's permissions was refused")?;
    Ok(())
}

/// A kernel stack is guard-paged at both ends and usable in between.
///
/// Not run *on* — switching stacks is stage 5's context switch, and doing it
/// here would need the assembly that stage owns. What is checked is everything
/// that has to be true before a stack can be switched to: it is mapped, it is
/// writable to its last byte, the page below it is not, and freeing it gives
/// the frames back.
fn check_stacks() -> Result<(), &'static str> {
    let free_before = mm::free_frames();
    let stack = vmap::allocate_stack().map_err(|_| "no kernel stack could be allocated")?;

    if stack.len() != vmap::STACK_PAGES * PAGE_SIZE {
        return Err("a kernel stack is not the size it was asked for");
    }
    if !stack.top.is_multiple_of(16) {
        // Both architectures require a 16-byte aligned stack pointer at a
        // function call boundary, and neither faults on it — the symptom is a
        // misaligned spill somewhere deep in the callee.
        return Err("a kernel stack top is not sixteen-byte aligned");
    }

    // The last usable word, which is where the first push lands, and the first,
    // which is the byte an overflow reaches last before the guard.
    for at in [stack.top - 8, stack.base] {
        // SAFETY: inside the stack's own mapping, which is writable and which
        // nothing else refers to — no CPU is running on this stack.
        unsafe { core::ptr::write_volatile(at as *mut u64, 0x57AC_0000_0000_0000) };
        // SAFETY: as above.
        if unsafe { core::ptr::read_volatile(at as *const u64) } != 0x57AC_0000_0000_0000 {
            return Err("a kernel stack did not hold what was written to it");
        }
    }

    if mm::translate(stack.base - PAGE_SIZE).is_some() {
        return Err("a kernel stack has no guard page below it, so an overflow would be silent");
    }
    if mm::translate(stack.top).is_some() {
        return Err("a kernel stack has no guard page above it");
    }

    // SAFETY: nothing is running on it; it was allocated a few lines above and
    // never installed anywhere.
    unsafe { vmap::free_stack(stack) }.map_err(|_| "a kernel stack could not be freed")?;
    if mm::free_frames() != free_before {
        return Err("a kernel stack leaked frames");
    }
    Ok(())
}

/// A device window lands where it was asked to, offset and all, and can be
/// taken back.
///
/// The aperture used is the kernel's own image, which is real RAM rather than
/// registers — nothing is read or written through the window, only translated,
/// because reading RAM through an uncached device mapping while the same bytes
/// sit in a cache is exactly the aliasing the architecture does not define.
/// What is under test is the *address arithmetic*, which is where the bugs
/// are: an I/O APIC's registers start at an offset within their page, and a
/// window that rounded that away would work perfectly for the GIC and silently
/// address the wrong register here.
fn check_device_windows(kernel_phys: u64) -> Result<(), &'static str> {
    const OFFSET: u64 = 0x40;

    let free_before = mm::free_frames();
    let at = vmap::map_device(kernel_phys + OFFSET, 0x100)
        .map_err(|_| "a device window could not be mapped")?;

    if at % PAGE_SIZE != OFFSET {
        return Err("a device window did not preserve its offset within the page");
    }
    if mm::translate(at) != Some(kernel_phys + OFFSET) {
        return Err("a device window does not resolve to the registers it was asked for");
    }
    match mm::permissions_of(at) {
        Some(flags) if flags.device && !flags.execute => {}
        Some(_) => return Err("a device window is not mapped as device memory"),
        None => return Err("a device window is not mapped at all"),
    }

    vmap::unmap_device(at).map_err(|_| "a device window could not be unmapped")?;
    if mm::translate(at).is_some() {
        return Err("unmapping a device window left the mapping behind");
    }
    if mm::free_frames() != free_before {
        return Err("a device window gave the aperture's frames to the buddy allocator");
    }
    Ok(())
}

/// The guard pages either side of an allocation are not mapped.
///
/// Not read or written, only translated. A guard page whose absence is proved
/// by touching it proves it once and takes the machine down with it — the
/// whole point is that there is no handler for a fault there, and stage 3's
/// on-demand window is the only place a kernel fault is resolved rather than
/// reported.
fn check_vmap_guards(mapping: vmap::Mapping) -> Result<(), &'static str> {
    if mm::translate(mapping.base - PAGE_SIZE).is_some() {
        return Err("the guard page below a vmap allocation is mapped");
    }
    if mm::translate(mapping.end()).is_some() {
        return Err("the guard page above a vmap allocation is mapped");
    }
    Ok(())
}

/// Hammer the frame allocator and require the books to balance.
///
/// **Deliberately allocates nothing on the heap.** `Vec` would be far more
/// convenient here and would also make the check meaningless: growing one takes
/// slab pages out of this very allocator, and `ferrix_heap` documents that slab
/// pages are never returned. The first version of this used a `Vec` and
/// reported a leak that was the heap working as designed.
fn frames_hammer() -> Result<(), &'static str> {
    /// Blocks held at once. Sixteen bytes each on a 64 KiB boot stack.
    const BATCH: usize = 256;
    /// How many times to fill and drain the batch.
    const ROUNDS: usize = 16;

    let before = mm::free_frames();

    for round in 0..ROUNDS {
        let mut taken = [(0u64, 0u8); BATCH];
        let mut held = 0usize;

        for (step, slot) in taken.iter_mut().enumerate() {
            // Orders 0..4, in a pattern that shifts each round so blocks do not
            // always pair up the same way.
            let order = ((step + round) % 5) as u8;
            let Some(frame) = mm::allocate_frames(order) else {
                break;
            };
            *slot = (frame, order);
            held += 1;
        }
        if held == 0 {
            return Err("the frame allocator handed out nothing");
        }

        // Free every other block first, then the rest. Buddies come apart and
        // then back together, which is the path that actually exercises
        // coalescing -- freeing in allocation order barely does.
        for (frame, order) in taken.iter().take(held).skip(1).step_by(2) {
            mm::deallocate_frames(*frame, *order);
        }
        for (frame, order) in taken.iter().take(held).step_by(2) {
            mm::deallocate_frames(*frame, *order);
        }
    }

    let after = mm::free_frames();
    if after != before {
        return Err("frames leaked: the free count did not return to where it started");
    }
    Ok(())
}

/// Check the frame allocator hands out distinct, aligned blocks.
fn check_frames() -> Result<(), &'static str> {
    let first = mm::allocate_frames(0).ok_or("no frame available")?;
    let second = mm::allocate_frames(0).ok_or("only one frame available")?;
    if first == second {
        return Err("the same frame was handed out twice");
    }

    let block = mm::allocate_frames(4).ok_or("no sixteen-frame block available")?;
    if !block.is_multiple_of(16) {
        return Err("a sixteen-frame block is not sixteen-frame aligned");
    }

    mm::deallocate_frames(block, 4);
    mm::deallocate_frames(second, 0);
    mm::deallocate_frames(first, 0);

    frames_hammer()
}

/// Check that `alloc` works, which is the whole point of the stage.
fn check_heap() -> Result<(), &'static str> {
    // A `Box`, which is the smallest possible proof that `GlobalAlloc` is wired
    // up at all.
    let boxed = Box::new(0x5EED_1234_ABCD_0001u64);
    if *boxed != 0x5EED_1234_ABCD_0001 {
        return Err("a Box did not hold what was put in it");
    }
    drop(boxed);

    // A `Vec` that grows through several reallocations, so the heap has to
    // move data between size classes and then between whole pages.
    let mut values: Vec<u64> = Vec::new();
    for value in 0..4096u64 {
        values.push(value.wrapping_mul(2_654_435_761));
    }
    for (index, value) in values.iter().enumerate() {
        if *value != (index as u64).wrapping_mul(2_654_435_761) {
            return Err("a Vec did not survive its own reallocations");
        }
    }
    drop(values);

    // A `BTreeMap`, which allocates nodes of an awkward size and frees them in
    // an order nothing controls.
    let mut map: BTreeMap<u64, u64> = BTreeMap::new();
    for key in 0..2048u64 {
        let _ = map.insert(key.wrapping_mul(2_654_435_761) % 100_003, key);
    }
    let entries = map.len();
    if entries == 0 {
        return Err("a BTreeMap held nothing");
    }
    for (key, value) in &map {
        if value.wrapping_mul(2_654_435_761) % 100_003 != *key {
            return Err("a BTreeMap returned a value under the wrong key");
        }
    }
    drop(map);

    if mm::heap_pages() == 0 {
        return Err("the heap never took a page, so nothing was really allocated");
    }
    Ok(())
}

/// Print what the loader handed over.
fn report(view: &BootView<'_>) {
    let info = view.raw();
    let mebibytes = |bytes: u64| bytes / (1024 * 1024);

    println!(
        "  memory   {} MiB total, {} MiB usable, {} regions",
        mebibytes(view.total_ram()),
        mebibytes(view.usable_ram()),
        view.regions().len()
    );
    println!(
        "  kernel   {:#x} -> {:#x}, {} KiB",
        info.kernel_phys,
        info.kernel_virt,
        info.kernel_len / 1024
    );
    println!(
        "  physmap  {:#x} covering {} MiB",
        info.physmap_base,
        mebibytes(info.physmap_len)
    );
    println!("  tables   root {:#x}", info.root_table_phys);

    if info.framebuffer.is_present() {
        println!(
            "  display  {}x{}, stride {}",
            info.framebuffer.width, info.framebuffer.height, info.framebuffer.stride
        );
    }
    if info.rsdp != 0 {
        println!("  acpi     rsdp at {:#x}", info.rsdp);
    }
    if info.dtb != 0 {
        println!("  fdt      at {:#x}", info.dtb);
    }
}

/// Check the things the rest of the kernel is about to assume.
///
/// This is stage 1's exit criterion. Every one of these is something that would
/// otherwise be discovered much later, by a subsystem that had no way to know
/// the ground under it was wrong.
fn self_check(view: &BootView<'_>, memory: &mut EarlyMemory) -> Result<(), &'static str> {
    let regions = view.regions();
    if regions.is_empty() {
        return Err("the memory map is empty");
    }

    // The frame allocator will walk this map assuming it is ordered and that no
    // two regions claim the same frame.
    let mut previous_end = 0u64;
    for region in regions {
        if region.base < previous_end {
            return Err("the memory map is unsorted or overlapping");
        }
        previous_end = region.end();
    }

    if view.usable_ram() == 0 {
        return Err("the memory map reports no usable RAM");
    }

    // The loader must have described its own allocations, or the kernel will
    // hand the frames holding its own page tables to the first caller that asks
    // for memory.
    let mut kinds = (false, false, false);
    for region in regions {
        match region.kind {
            MemKind::Kernel => kinds.0 = true,
            MemKind::PageTables => kinds.1 = true,
            MemKind::BootInfo => kinds.2 = true,
            _ => {}
        }
    }
    if kinds != (true, true, true) {
        return Err("the memory map does not describe the loader's own allocations");
    }

    check_direct_map(view, memory)?;
    check_early_mapper(view, memory)
}

/// Prove the direct map really does alias physical memory.
///
/// Everything from stage 2 onwards reads physical memory through it — page
/// tables, page-cache pages, `DMA` buffers — so if the loader mapped it at the
/// wrong offset, the first symptom would be a page table full of plausible
/// nonsense.
///
/// The test is to read the kernel's own first bytes twice: once through the
/// image mapping, once through the direct map at the physical address the
/// loader reported. They are the same bytes, so they must agree.
fn check_direct_map(view: &BootView<'_>, memory: &EarlyMemory) -> Result<(), &'static str> {
    let info = view.raw();

    for offset in [0u64, 1, 2, 3, 64, 4095] {
        // SAFETY: `kernel_virt` is where the loader mapped the kernel image and
        // `offset` is inside its first page, which is `.text` and always
        // present.
        let through_image =
            unsafe { core::ptr::read_volatile((info.kernel_virt + offset) as *const u8) };
        let through_physmap = memory.read_physical_byte(info.kernel_phys + offset);

        if through_image != through_physmap {
            return Err("the direct map does not alias the kernel image");
        }
    }
    Ok(())
}

/// Prove the kernel can read and extend the page tables the loader left.
///
/// Two things, both of which everything after stage 1 depends on. First that
/// walking the loader's tables from software agrees with what the hardware is
/// doing — the kernel's own image is the one mapping whose answer is known in
/// advance. Second that a *new* mapping can be installed and takes effect,
/// which is the whole of `EarlyMemory`'s job and, on AArch64, the only reason
/// there is a console at all.
fn check_early_mapper(view: &BootView<'_>, memory: &mut EarlyMemory) -> Result<(), &'static str> {
    let info = view.raw();

    match memory.translate(info.kernel_virt) {
        Some(phys) if phys == info.kernel_phys => {}
        Some(_) => return Err("walking the page tables disagrees with the loader"),
        None => return Err("the kernel image is not mapped in its own page tables"),
    }

    // A device window the kernel has a real use for, when firmware left one.
    // On QEMU's AArch64 `virt` there is no display, so this does not run there
    // — the PL011 console took the same path a moment ago.
    if info.framebuffer.is_present() {
        let at = KERNEL_VMAP_BASE + FRAMEBUFFER_WINDOW;
        if memory
            .map_device(at, info.framebuffer.phys, PAGE_SIZE)
            .is_err()
        {
            return Err("could not map the framebuffer");
        }
        if memory.translate(at) != Some(info.framebuffer.phys) {
            return Err("the framebuffer mapping does not resolve to the framebuffer");
        }
    }

    Ok(())
}

/// Offset within the kernel's dynamic mapping area for the framebuffer window,
/// clear of the `AArch64` console at offset zero.
const FRAMEBUFFER_WINDOW: u64 = 0x1000_0000;

/// Where a kernel panic ends up.
///
/// There is no supervisor above us and no unwinder, so this says what happened
/// and stops the machine. The marker is what turns a panic into a failed boot
/// test rather than a two-minute timeout with no explanation.
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    println!();
    println!("FERRIX-PANIC {info}");
    arch::halt()
}
