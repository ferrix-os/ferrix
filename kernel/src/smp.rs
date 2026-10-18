//! Every other CPU in the machine.
//!
//! The two architectures start a processor in completely different ways — one
//! by sending it three interrupts and a page of sixteen-bit code, the other by
//! asking firmware politely — so bring-up lives in `arch`. What is here is
//! everything above it: the CPU numbering, the roll call of who came up, the
//! request an already-running CPU can make of the others, and the parking loop
//! an application processor spends its life in until stage 5 gives it something
//! to do.
//!
//! # What a remote request is, and what it is not
//!
//! [`Request`] is an enum, not a function pointer. A function pointer would be
//! more general and would need a `transmute` through an atomic to get it into
//! an interrupt handler, which is exactly the kind of generality that is free
//! to write and expensive to be wrong about. The set of things one CPU asks
//! another to do is small, known, and grows by a variant in a diff somebody
//! reads.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ferrix_bootinfo::BootView;

use crate::percpu::{MAX_CPUS, Padded, PerCpu};
use crate::{arch, timer};

/// What one CPU can ask the others to do.
///
/// Deliberately tiny. Every variant is something that has to happen *on* the
/// other CPU because it is about that CPU's own state — its TLB, its view of
/// memory — rather than something that merely could be done there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub(crate) enum Request {
    /// Nothing. The interrupt itself is the message: it drags the target out
    /// of `wfi`/`hlt`, which is what a reschedule IPI will be in stage 5.
    Poke = 0,
    /// Invalidate the whole TLB on the receiving CPU.
    ///
    /// The whole thing rather than a range: see [`crate::mm::shootdown`].
    FlushTlb = 1,
    /// Read the word at [`PROBE_ADDRESS`] into this CPU's probe slot.
    ///
    /// Exists for the TLB shootdown test and nothing else, which is why it
    /// says so here rather than pretending to be general. Making one CPU read
    /// an address through its own translations is the only way to observe that
    /// CPU's TLB from another one.
    Probe = 2,
}

impl Request {
    /// The request a number stands for, or [`Request::Poke`] for one that
    /// stands for nothing — an interrupt with no request attached is still a
    /// poke, and treating it as one is what the sender meant.
    const fn from_u64(value: u64) -> Request {
        match value {
            1 => Request::FlushTlb,
            2 => Request::Probe,
            _ => Request::Poke,
        }
    }
}

/// The request currently being broadcast.
static REQUEST: AtomicU64 = AtomicU64::new(0);

/// Responses to it.
///
/// Counted rather than a per-CPU flag because the sender only ever waits for
/// all of them, and because a single cache line bouncing between eight cores
/// for the length of one shootdown is cheaper than eight lines each bouncing
/// once.
static RESPONSES: AtomicUsize = AtomicUsize::new(0);

/// Requests this CPU has serviced, for the boot log and the test.
static SERVICED: PerCpu<AtomicU64> = PerCpu::new([const { Padded(AtomicU64::new(0)) }; MAX_CPUS]);

/// Where [`Request::Probe`] reads from.
static PROBE_ADDRESS: AtomicU64 = AtomicU64::new(0);

/// What each CPU last read from it.
static PROBED: PerCpu<AtomicU64> = PerCpu::new([const { Padded(AtomicU64::new(0)) }; MAX_CPUS]);

/// CPUs that have reached [`ap_main`] and are taking interrupts.
static ONLINE: AtomicUsize = AtomicUsize::new(1);

/// Set once the boot CPU has finished enumerating processors, so that
/// [`this_cpu`] stops asking the hardware before there is an answer.
static NUMBERED: AtomicUsize = AtomicUsize::new(0);

/// This CPU's number.
///
/// Zero until the CPUs have been enumerated, which is correct rather than a
/// fallback: before enumeration the boot CPU is the only one running, and it
/// is number zero.
pub(crate) fn this_cpu() -> usize {
    if NUMBERED.load(Ordering::Acquire) == 0 {
        return 0;
    }
    crate::percpu::index_of(arch::cpu_identifier()).unwrap_or(0)
}

/// CPUs that are up and taking interrupts.
pub(crate) fn online() -> usize {
    ONLINE.load(Ordering::Acquire)
}

/// Processors firmware described, whether or not they started.
pub(crate) fn described() -> usize {
    crate::percpu::assigned()
}

/// What [`start_others`] found and did.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Roll {
    /// Processors the firmware tables describe.
    pub(crate) described: usize,
    /// Processors that reported for duty.
    pub(crate) online: usize,
    /// Processors that were asked to start and never answered.
    pub(crate) lost: usize,
}

/// Number every processor firmware describes, then start all but this one.
///
/// # Errors
///
/// Only if the machine cannot be enumerated at all. A processor that is
/// described and fails to start is counted in [`Roll::lost`] rather than
/// failing the boot: a kernel that refuses to run on seven cores because the
/// eighth is wedged is worse than one that says so and carries on.
///
/// # Safety
///
/// Must be called once, on the boot CPU, after traps, memory, the interrupt
/// controller and the timer are up — an application processor reaches Rust code
/// that assumes all four within a few hundred instructions of starting.
pub(crate) unsafe fn start_others(view: &BootView<'_>) -> Result<Roll, &'static str> {
    // SAFETY: the caller's contract. This enumerates processors and assigns
    // each a number; nothing is started yet.
    let described = unsafe { arch::enumerate_cpus(view)? };
    NUMBERED.store(1, Ordering::Release);

    register_ipi()?;

    let mut lost = 0;
    for cpu in 1..described {
        let before = online();
        // SAFETY: the caller's contract, and `cpu` is a number `enumerate_cpus`
        // assigned a processor it found in the firmware tables.
        if unsafe { arch::start_cpu(cpu) }.is_err() || !wait_for(before + 1) {
            lost += 1;
        }
    }

    Ok(Roll {
        described,
        online: online(),
        lost,
    })
}

/// How long to wait for a processor to answer, in nanoseconds.
///
/// Generous: QEMU under load on a busy build machine is much slower than the
/// hardware this models, and the cost of being wrong in the tight direction is
/// a boot that reports a working CPU as lost.
const START_TIMEOUT_NANOS: u64 = 500_000_000;

/// Spin until `count` CPUs are online, or the timeout expires.
fn wait_for(count: usize) -> bool {
    let deadline = timer::now_nanos() + START_TIMEOUT_NANOS;
    while timer::now_nanos() < deadline {
        if online() >= count {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Attach the inter-processor interrupt to its handler.
fn register_ipi() -> Result<(), &'static str> {
    crate::irq::register(arch::ipi_irq(), handle_ipi)
        .map_err(|_| "the inter-processor interrupt already has a handler")
}

/// Run the broadcast request. Called from the interrupt path.
fn handle_ipi(_irq: u32) {
    let request = Request::from_u64(REQUEST.load(Ordering::Acquire));
    perform(request);
    let _ = SERVICED.mine().fetch_add(1, Ordering::Relaxed);
    let _ = RESPONSES.fetch_add(1, Ordering::Release);
}

/// Do what a request asks, on the CPU that received it.
fn perform(request: Request) {
    match request {
        Request::Poke => {}
        Request::FlushTlb => arch::flush_tlb(),
        Request::Probe => {
            let address = PROBE_ADDRESS.load(Ordering::Acquire);
            // SAFETY: the address is one the boot CPU mapped and published
            // before sending the request, and `Ordering::Acquire` above is what
            // makes that mapping visible here. Nothing unmaps it while a probe
            // is outstanding: `broadcast` does not return until every CPU has
            // answered, and only the caller of `broadcast` unmaps.
            let value = unsafe { core::ptr::read_volatile(address as *const u64) };
            PROBED.mine().store(value, Ordering::Release);
        }
    }
}

/// Ask every other online CPU to do something, and wait until they have.
///
/// Returns how many answered. Fewer than [`online`] minus one means a CPU did
/// not respond inside [`BROADCAST_TIMEOUT_NANOS`], which the caller has to
/// treat as a failure rather than as slowness — a TLB shootdown that gave up
/// is a stale translation somebody is about to use.
pub(crate) fn broadcast(request: Request) -> usize {
    let others = online().saturating_sub(1);
    if others == 0 {
        return 0;
    }

    // One broadcast at a time. The request and the response count are both
    // single globals, and two overlapping broadcasts would have the second
    // sender count the first's answers.
    let _guard = BROADCAST.lock();

    RESPONSES.store(0, Ordering::Release);
    REQUEST.store(request as u64, Ordering::Release);
    arch::send_ipi_to_others();

    let deadline = timer::now_nanos() + BROADCAST_TIMEOUT_NANOS;
    loop {
        let answered = RESPONSES.load(Ordering::Acquire);
        if answered >= others || timer::now_nanos() >= deadline {
            return answered;
        }
        core::hint::spin_loop();
    }
}

/// How long to wait for every CPU to answer a broadcast.
const BROADCAST_TIMEOUT_NANOS: u64 = 100_000_000;

/// Serialises broadcasts. See [`broadcast`].
static BROADCAST: ferrix_sync::IrqSpinLock<(), arch::Irq> = ferrix_sync::IrqSpinLock::new(());

/// Point [`Request::Probe`] at an address.
pub(crate) fn set_probe_address(address: u64) {
    PROBE_ADDRESS.store(address, Ordering::Release);
}

/// What a CPU last read through its own translations.
pub(crate) fn probed(cpu: usize) -> Option<u64> {
    PROBED.get(cpu).map(|slot| slot.load(Ordering::Acquire))
}

/// Requests a CPU has serviced.
pub(crate) fn serviced(cpu: usize) -> Option<u64> {
    SERVICED.get(cpu).map(|slot| slot.load(Ordering::Relaxed))
}

/// Requests every CPU has serviced between them.
pub(crate) fn serviced_total() -> u64 {
    SERVICED.iter().map(|slot| slot.load(Ordering::Relaxed)).sum()
}

// ---------------------------------------------------------------------------
// The application processor's own life
// ---------------------------------------------------------------------------

/// Where an application processor ends up, and stays until stage 5.
///
/// Called from `arch` once the CPU has its own descriptor tables, its own
/// interrupt controller state, and a stack. It never returns: stage 5 replaces
/// the loop at the bottom with the scheduler's idle task, which is the same
/// loop with somewhere to go when work appears.
pub(crate) fn ap_main() -> ! {
    arch::enable_interrupts();
    let _ = ONLINE.fetch_add(1, Ordering::AcqRel);

    loop {
        crate::rcu::quiesce();
        if let Some(work) = crate::work::take() {
            work.run();
            continue;
        }
        // Not a spin: `wfi` and `hlt` both return when an interrupt arrives,
        // and an IPI is an interrupt. A CPU with nothing to do costs nothing
        // until somebody wants it.
        arch::wait_for_interrupt();
    }
}
