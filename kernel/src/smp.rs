//! More than one CPU.
//!
//! Stage 4 of `docs/ROADMAP.md`. Four things, and they arrive in this order
//! because each needs the one before it:
//!
//! 1. **Bring-up.** Every processor firmware describes is started, given a
//!    guard-paged stack of its own and a [`crate::percpu::PerCpu`] record, and
//!    made to say so.
//! 2. **Inter-processor interrupts.** The only way one CPU makes another do
//!    something now rather than eventually. They arrive through
//!    [`crate::irq`] like any other interrupt, because on both architectures
//!    that is exactly what they are.
//! 3. **TLB shootdown.** Unmapping a page on one CPU does nothing about the
//!    translation another CPU has cached for it. The unmapper has to ask, and
//!    then *wait*, because the page may be handed to somebody else the moment
//!    it does not.
//! 4. **A grace period.** [`synchronize`] returns once every CPU has been
//!    observed outside any read-side critical section, which is what makes it
//!    safe to free a structure other CPUs were reading without a lock. Stage 5
//!    needs it for the scheduler's class stack and stage 14 for the domain
//!    mode switch.
//!
//! # The rule about masked interrupts
//!
//! [`shootdown`] and [`synchronize`] both wait for other CPUs to answer an
//! interrupt. A CPU that calls either with interrupts masked on itself is one
//! that cannot answer somebody else's — and two CPUs doing that at once is a
//! deadlock neither can see. Both therefore check, and refuse.
//!
//! That is why `mm` still flushes only its own TLB from inside its locks: the
//! shootdown belongs to the caller that unmapped, not to the allocator holding
//! the lock. Stage 6, which is where user mappings start being torn down under
//! contention, is where that becomes a rule with teeth rather than a comment.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use alloc::boxed::Box;

use ferrix_bootinfo::BootView;

use crate::arch;
use crate::irq::{self, IrqError};
use crate::percpu::{self, MAX_CPUS, PerCpu};
use crate::vmap;

/// What one CPU can ask another to do.
///
/// Deliberately a closed set. An inter-processor interrupt carries no argument
/// on either architecture — x86-64 has a vector and AArch64 has a four-bit
/// software-generated interrupt number — so anything the receiver needs beyond
/// "one of these happened" has to be somewhere it can read, and a growing
/// enumeration of them would hide that.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ipi {
    /// Look again at the runqueue. Nothing uses it until stage 5; it is
    /// registered from the start so that the vector space is settled before a
    /// scheduler depends on it.
    Reschedule,
    /// Invalidate the TLB. See [`shootdown`].
    TlbFlush,
    /// Pass through a quiescent state. See [`synchronize`].
    Quiesce,
}

/// Who an inter-processor interrupt goes to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Target {
    /// Every CPU except the one sending.
    Others,
    /// One CPU, by whatever the interrupt controller calls it.
    One(u64),
}

/// Why bringing up another CPU failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SmpError {
    /// Firmware described no processors at all, which on a machine that is
    /// running means the tables are wrong rather than that there are none.
    NoProcessors,
    /// More processors than [`MAX_CPUS`].
    TooManyProcessors(usize),
    /// No kernel stack could be allocated for a processor.
    NoStack,
    /// The architecture could not start it, with its reason.
    NotStarted(&'static str),
    /// It was started and never reported in.
    NeverArrived(u64),
    /// An inter-processor interrupt could not be registered.
    Irq(IrqError),
}

impl core::fmt::Display for SmpError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SmpError::NoProcessors => f.write_str("firmware describes no processors"),
            SmpError::TooManyProcessors(found) => {
                write!(f, "firmware describes {found} processors, more than {MAX_CPUS}")
            }
            SmpError::NoStack => f.write_str("no kernel stack could be allocated for a processor"),
            SmpError::NotStarted(why) => write!(f, "a processor would not start: {why}"),
            SmpError::NeverArrived(id) => {
                write!(f, "processor {id:#x} was started but never reported in")
            }
            SmpError::Irq(error) => write!(f, "{error}"),
        }
    }
}

/// CPUs that have come online.
static ONLINE: AtomicUsize = AtomicUsize::new(0);

/// The record the CPU currently being started should install.
///
/// Bring-up is serialised — one processor is started and waited for before the
/// next is touched — so one slot is enough, and an application processor that
/// reads it is reading the record its starter wrote a moment earlier.
static ARRIVING: AtomicUsize = AtomicUsize::new(0);

/// Set once the machine is shutting down, so idle CPUs stop.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// CPUs online.
pub(crate) fn online() -> usize {
    ONLINE.load(Ordering::Acquire)
}

/// Bring up this CPU's own per-CPU state and the inter-processor interrupts.
///
/// Called on the boot CPU before anything else in this module. Separate from
/// [`start_others`] because the boot CPU needs a record before it can send an
/// interrupt to anybody, and because a machine with one processor still wants
/// per-CPU allocation.
///
/// # Errors
///
/// [`SmpError::Irq`] if something already holds one of the three interrupt
/// numbers the architecture reserves for inter-processor interrupts.
pub(crate) fn init(hardware_id: u64) -> Result<(), SmpError> {
    let cpu = Box::leak(Box::new(PerCpu::new(0, hardware_id)));
    percpu::install(cpu);
    let _ = ONLINE.fetch_add(1, Ordering::AcqRel);

    for (ipi, handler) in [
        (Ipi::Reschedule, on_reschedule as irq::Handler),
        (Ipi::TlbFlush, on_tlb_flush as irq::Handler),
        (Ipi::Quiesce, on_quiesce as irq::Handler),
    ] {
        irq::register(arch::ipi_irq(ipi), handler).map_err(SmpError::Irq)?;
        arch::unmask_ipi(ipi);
    }
    Ok(())
}

/// What bringing the other processors up found.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Report {
    /// Processors firmware described as startable, including this one.
    pub(crate) described: usize,
    /// Processors that came online, including this one.
    pub(crate) online: usize,
}

/// Start every other processor firmware describes, and wait for each.
///
/// Serialised deliberately. Starting them all at once would be faster and would
/// make a failure impossible to attribute: the startup page holds one CPU's
/// stack pointer and one CPU's record, and an application processor that reads
/// somebody else's is a fault in code that has no console yet.
///
/// # Errors
///
/// Any of [`SmpError`]. A processor that will not start stops the whole thing,
/// rather than being skipped: on a machine where one core is broken, a kernel
/// that quietly runs on the rest is a kernel whose performance nobody can
/// explain.
pub(crate) fn start_others(view: &BootView<'_>) -> Result<Report, SmpError> {
    let mut described = 0;
    let mut hardware_ids = [0u64; MAX_CPUS];
    arch::enumerate_cpus(view, |hardware_id| {
        if let Some(slot) = hardware_ids.get_mut(described) {
            *slot = hardware_id;
        }
        described += 1;
    });

    if described == 0 {
        return Err(SmpError::NoProcessors);
    }
    if described > MAX_CPUS {
        return Err(SmpError::TooManyProcessors(described));
    }

    let this = percpu::current().map_or(0, |cpu| cpu.hardware_id);
    let mut index = 1u32;

    for hardware_id in hardware_ids.iter().take(described).copied() {
        if hardware_id == this {
            continue;
        }
        start_one(index, hardware_id)?;
        index += 1;
    }

    Ok(Report {
        described,
        online: online(),
    })
}

/// How long to wait for a processor to report in.
///
/// Generous, and measured with the counter rather than counted in spins: a
/// spin count is a measure of how fast the waiting CPU is, which is not the
/// quantity anybody wants a timeout in.
const ARRIVAL_TIMEOUT_NANOS: u64 = 500_000_000;

/// Start one processor and wait for it.
fn start_one(index: u32, hardware_id: u64) -> Result<(), SmpError> {
    let stack = vmap::allocate_stack().map_err(|_| SmpError::NoStack)?;
    let cpu: &'static mut PerCpu = Box::leak(Box::new(PerCpu::new(index, hardware_id)));
    ARRIVING.store((&raw const *cpu) as usize, Ordering::Release);

    let before = online();
    // SAFETY: `stack` was allocated a moment ago and is handed to exactly one
    // processor, which is the only thing that will ever run on it; `ARRIVING`
    // holds the record that processor will install, published with a release
    // store it reads with an acquire load.
    unsafe { arch::start_cpu(hardware_id, stack.top) }.map_err(SmpError::NotStarted)?;

    let deadline = crate::timer::now_nanos().saturating_add(ARRIVAL_TIMEOUT_NANOS);
    while online() == before {
        if crate::timer::now_nanos() > deadline {
            return Err(SmpError::NeverArrived(hardware_id));
        }
        core::hint::spin_loop();
    }
    Ok(())
}

/// Where an application processor lands once its architecture has given it
/// working trap vectors and a stack.
///
/// Never returns: until stage 5 there is no scheduler to return to, so a
/// processor with nothing to do spins in [`idle`] waiting to be given some.
pub(crate) fn ap_start() -> ! {
    let address = ARRIVING.swap(0, Ordering::AcqRel);
    if address != 0 {
        // SAFETY: the starting CPU published the address of a `Box::leak`ed
        // `PerCpu` here and is waiting for this processor to come online
        // before it publishes another, so this is the only reader and the
        // reference is unique until `install` shares it.
        let cpu = unsafe { &mut *(address as *mut PerCpu) };
        percpu::install(cpu);
    }

    // Per-CPU interrupt controller state: on x86-64 this CPU's local APIC, on
    // AArch64 this CPU's GIC interface. Neither was touched by the processor
    // that started this one, because neither is reachable from it.
    //
    // SAFETY: called once, on this processor, after its trap vectors are
    // installed and while interrupts are still masked.
    if unsafe { arch::init_cpu_interrupts() }.is_ok() {
        arch::enable_interrupts();
    }

    let _ = ONLINE.fetch_add(1, Ordering::AcqRel);
    idle()
}

/// What a processor with nothing to do does.
///
/// Spins rather than waiting for an interrupt, because [`broadcast`] hands out
/// work by storing to memory rather than by sending one. That is a stage-4
/// shape and not a lasting one: stage 5's scheduler wakes an idle CPU with a
/// [`Ipi::Reschedule`], and this becomes a `wfi`.
fn idle() -> ! {
    let mut seen = WORK_GENERATION.load(Ordering::Acquire);
    loop {
        if STOPPING.load(Ordering::Acquire) {
            arch::halt()
        }

        let generation = WORK_GENERATION.load(Ordering::Acquire);
        if generation != seen {
            seen = generation;
            run_pending_work();
        }
        core::hint::spin_loop();
    }
}

/// Stop every other CPU.
///
/// Called before the machine shuts down, so that a processor is not still
/// spinning in [`idle`] over a page the shutdown path has taken back.
pub(crate) fn stop_others() {
    STOPPING.store(true, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Work
// ---------------------------------------------------------------------------

/// The function every CPU is to run, as an address.
static WORK: AtomicUsize = AtomicUsize::new(0);

/// Bumped once per round of work, which is how an idle CPU notices there is
/// some.
static WORK_GENERATION: AtomicU64 = AtomicU64::new(0);

/// CPUs that have finished the current round.
static WORK_DONE: AtomicUsize = AtomicUsize::new(0);

/// Run `work` on every online CPU, including this one, and wait for all of
/// them.
///
/// **Not a scheduler, and not pretending to be one.** It is the smallest thing
/// that can put the same code on every core at the same time, which is what
/// stage 4's exit criterion needs and what a contended-counter test is for.
/// Stage 5 replaces it with `Task` and a runqueue, and this goes.
pub(crate) fn broadcast(work: fn()) {
    let others = online().saturating_sub(1);

    WORK_DONE.store(0, Ordering::Release);
    WORK.store(work as usize, Ordering::Release);
    let _ = WORK_GENERATION.fetch_add(1, Ordering::AcqRel);

    work();

    while WORK_DONE.load(Ordering::Acquire) < others {
        core::hint::spin_loop();
    }
}

/// Run whatever [`broadcast`] published, and report it done.
fn run_pending_work() {
    let address = WORK.load(Ordering::Acquire);
    if address != 0 {
        // SAFETY: `broadcast` stores a `fn()` cast to `usize` and bumps the
        // generation afterwards with a release; this CPU observed that bump
        // with an acquire, so the address it reads is the one that was
        // published and is a live `fn()` in this image.
        let work: fn() = unsafe { core::mem::transmute::<usize, fn()>(address) };
        work();
    }
    let _ = WORK_DONE.fetch_add(1, Ordering::AcqRel);
}

// ---------------------------------------------------------------------------
// TLB shootdown
// ---------------------------------------------------------------------------

/// Invalidate every other CPU's TLB, and wait until they have.
///
/// The wait is the entire point. A CPU that has unmapped a page and given the
/// frame back has to know that nothing can still reach it through a cached
/// translation, and "asked politely" is not that: the frame may be somebody
/// else's data by the time the interrupt is taken.
///
/// # Errors
///
/// [`SmpError::NotStarted`] if called with interrupts masked, which is the one
/// way this deadlocks — see the module documentation.
pub(crate) fn shootdown() -> Result<(), SmpError> {
    arch::flush_tlb();

    if online() <= 1 {
        return Ok(());
    }
    if !arch::interrupts_enabled() {
        return Err(SmpError::NotStarted(
            "a TLB shootdown was asked for with interrupts masked",
        ));
    }

    let this = percpu::current().map(|cpu| cpu.index);
    let mut wanted = [0u64; MAX_CPUS];

    percpu::for_each(|cpu| {
        if Some(cpu.index) == this {
            return;
        }
        let want = cpu.flush_requested.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some(slot) = wanted.get_mut(cpu.index as usize) {
            *slot = want;
        }
    });

    arch::send_ipi(Target::Others, Ipi::TlbFlush);

    percpu::for_each(|cpu| {
        if Some(cpu.index) == this {
            return;
        }
        let want = wanted.get(cpu.index as usize).copied().unwrap_or(0);
        while cpu.flush_completed.load(Ordering::Acquire) < want {
            core::hint::spin_loop();
        }
    });
    Ok(())
}

/// What a [`Ipi::TlbFlush`] does on the receiving CPU.
///
/// The order is forced: read what was asked for, *then* flush, *then* say what
/// was done. Recording completion before the flush would let the initiator free
/// a frame this CPU can still reach.
fn on_tlb_flush(_irq: u32) {
    let Some(cpu) = percpu::current() else { return };
    let _ = cpu.ipis.fetch_add(1, Ordering::Relaxed);

    let requested = cpu.flush_requested.load(Ordering::Acquire);
    arch::flush_tlb();
    cpu.flush_completed.store(requested, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Grace periods
// ---------------------------------------------------------------------------

/// The grace period being waited for.
static EPOCH: AtomicU64 = AtomicU64::new(0);

/// Wait until every CPU has been seen outside a read-side critical section.
///
/// The read side is not a lock and costs nothing: a CPU reading a structure
/// this protects simply does so with interrupts masked, which on both
/// architectures is what an interrupt gate already does. So "seen outside one"
/// is the same statement as "took an interrupt and returned from it", and this
/// asks every CPU to do exactly that.
///
/// That makes it the expedited form — one round of interrupts rather than
/// waiting for CPUs to pass through quiescent states of their own accord. The
/// unexpedited form is cheaper and needs a scheduler to hook, which is stage 5.
///
/// # Errors
///
/// As [`shootdown`]: with interrupts masked on this CPU, this cannot complete.
pub(crate) fn synchronize() -> Result<(), SmpError> {
    let epoch = EPOCH.fetch_add(1, Ordering::AcqRel) + 1;

    // This CPU is outside a read-side section by definition: it is here.
    if let Some(cpu) = percpu::current() {
        cpu.quiescent.store(epoch, Ordering::Release);
    }
    if online() <= 1 {
        return Ok(());
    }
    if !arch::interrupts_enabled() {
        return Err(SmpError::NotStarted(
            "a grace period was asked for with interrupts masked",
        ));
    }

    arch::send_ipi(Target::Others, Ipi::Quiesce);

    percpu::for_each(|cpu| {
        while cpu.quiescent.load(Ordering::Acquire) < epoch {
            core::hint::spin_loop();
        }
    });
    Ok(())
}

/// What a [`Ipi::Quiesce`] does on the receiving CPU.
fn on_quiesce(_irq: u32) {
    let Some(cpu) = percpu::current() else { return };
    let _ = cpu.ipis.fetch_add(1, Ordering::Relaxed);
    cpu.quiescent
        .store(EPOCH.load(Ordering::Acquire), Ordering::Release);
}

/// What a [`Ipi::Reschedule`] does, which until stage 5 is to be counted.
///
/// Registered rather than left unhandled on purpose: an interrupt number with
/// no handler is counted as unclaimed, and the boot test requires that count to
/// be zero. A vector reserved for something that does not exist yet should not
/// be indistinguishable from a bug.
fn on_reschedule(_irq: u32) {
    if let Some(cpu) = percpu::current() {
        let _ = cpu.ipis.fetch_add(1, Ordering::Relaxed);
    }
}

/// Inter-processor interrupts every CPU has taken.
pub(crate) fn ipis() -> u64 {
    let mut total = 0;
    percpu::for_each(|cpu| total += cpu.ipis.load(Ordering::Relaxed));
    total
}
