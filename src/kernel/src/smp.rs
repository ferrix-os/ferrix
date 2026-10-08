//! More than one CPU.
//!
//! Stage 4 of `docs/ROADMAP.md`. It begins by finding out how many processors
//! the machine has and which of them is running this code, before any of the
//! others is started — every later step needs the answer, and the tables it
//! comes from are reclaimed at the end of boot.
//!
//! # Logical and hardware numbers
//!
//! A processor has a *hardware* identifier, which is what the machine calls
//! it: an APIC ID on x86-64, the affinity fields of `MPIDR_EL1` on `AArch64`.
//! Neither is dense — a two-socket machine can number its cores 0..16 and
//! 32..48 — so the kernel also gives each one a *logical* number, its index
//! in [`Topology`], and the boot CPU is always logical CPU zero.
//!
//! # Per-CPU records
//!
//! Every processor has a [`PerCpu`] record, allocated once and never freed,
//! and a register pointing at it: `GS`'s base on x86-64, `TPIDR_EL1` on
//! `AArch64`. [`this_cpu`] reads that register, which is how code running on a
//! processor finds out which one it is without being told — an interrupt
//! handler in particular, which is told nothing at all.

pub(crate) mod check;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_bootinfo::{BootView, PAGE_SIZE};
use ferrix_sched::{CpuSet, MAX_CPUS};
use ferrix_sync::{IrqControl, Once};

use crate::sync::SpinLock;

use crate::arch;

/// The processors firmware described, as the architecture found them.
///
/// Built by `arch::describe_cpus` and checked by [`discover`], so that the
/// rules about what a sane list looks like are written once rather than once
/// per architecture.
#[derive(Debug)]
pub(crate) struct Described {
    /// What this architecture calls a hardware identifier, for the boot log.
    pub(crate) id_name: &'static str,
    /// The hardware identifier of the processor running this code.
    pub(crate) boot: u64,
    /// Every processor firmware says can be started, in table order, the boot
    /// processor included.
    pub(crate) ids: Vec<u64>,
}

/// One processor's own state.
///
/// `repr(C)` because of the first field, which is not optional: x86-64
/// reaches this record through `GS`, and the one thing `gs:0` can hand back
/// is a word stored at offset zero. Making that word the record's own address
/// turns a segment-relative load into an ordinary pointer.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct PerCpu {
    /// This record's own address.
    this: u64,
    /// The kernel stack a system call from user mode lands on.
    ///
    /// Read by x86-64's `SYSCALL` trampoline *from assembly*, at a fixed
    /// offset from `GS`, because `SYSCALL` does not switch stacks: the first
    /// thing the kernel does on entry is still standing on the user's stack,
    /// and the only thing it can reach without one is this record. Set by
    /// `arch::restore_user_state` whenever the scheduler switches to a task
    /// that runs user code, to the top of that task's own kernel stack.
    ///
    /// The two Arm architectures need neither this nor the field below --
    /// `SP_EL1` and the banked SVC stack pointer are the same idea in
    /// hardware -- but the record is shared, so they simply leave them zero.
    pub(crate) kernel_stack: AtomicU64,
    /// Where the trampoline parks the user stack pointer while it switches.
    ///
    /// One word of scratch, also reached from assembly. It cannot be a
    /// register: every register at that moment either holds a system call
    /// argument or is `rcx`/`r11`, which `SYSCALL` has already overwritten
    /// with the return address and flags.
    pub(crate) user_stack: u64,
    /// Logical number: this record's index in [`Topology`].
    pub(crate) logical: usize,
    /// What the machine calls this processor.
    pub(crate) hardware_id: u64,
    /// Set by the processor itself once it is running kernel code with its
    /// own record installed. Never cleared: nothing takes a CPU offline.
    online: AtomicBool,
    /// The generation of the last piece of work this processor finished.
    job_done: AtomicU64,
    /// Inter-processor interrupts this processor has taken.
    ipis: AtomicU64,
    /// The last TLB shootdown this processor has flushed for.
    tlb_seen: AtomicU64,
    /// The last grace period this processor has been outside a read-side
    /// section for.
    gp_seen: AtomicU64,
    /// The running context's preemption count and locks held, and where each
    /// was last raised: `sched::preempt`'s, which alone writes them.
    ///
    /// Here rather than in an array indexed by the processor's number because
    /// the count is what keeps a task on its processor, and finding a slot by
    /// number takes two steps a migration can come between. On x86-64 the
    /// raise and the lower are one instruction addressed through `GS`
    /// ([`PREEMPT_WORD_OFFSET`]); on Arm they are made with every exception
    /// that can take a lock masked. `arch::percpu` argues both.
    pub(crate) preempt: crate::sched::PreemptState,
    /// The task this processor runs, as `Arc::as_ptr` of its run queue's
    /// `current`, written beside it: what `sched::with_current` lends without
    /// the queue's lock. `sched::borrow` argues it.
    pub(crate) running: crate::sched::RunningSlot,
    /// x86-64: this processor's GDT, where its thread-local slots are, as
    /// `SGDT` reported it at the last load of `GDTR` or `TR` made with this
    /// record installed; zero for one this kernel did not build (Q4,
    /// `docs/OPAQUE-KERNEL.md` §9.11). Read at every switch instead of
    /// `SGDT`; written only by the x86-64 GDT code's `note_tables`. The Arm
    /// architectures leave it zero, as they do `kernel_stack`.
    pub(crate) gdt: AtomicU64,
    /// x86-64: where this processor's TSS keeps `RSP0`, as `STR` and `SGDT`
    /// found it at the same moment, or zero for no TSS. Kept as [`PerCpu::gdt`].
    pub(crate) privilege_stack: AtomicU64,
}

/// Where in a [`PerCpu`] record its preemption word is, for
/// `arch::this_cpu_add`, which raises and lowers it without first finding the
/// record. `repr(C)` above keeps it fixed.
pub(crate) const PREEMPT_WORD_OFFSET: usize =
    core::mem::offset_of!(PerCpu, preempt) + crate::sched::PreemptState::WORD_OFFSET;

/// Where in a [`PerCpu`] record the running task's pointer is, for
/// `arch::this_cpu_read`, which reads it without first finding the record.
pub(crate) const RUNNING_WORD_OFFSET: usize =
    core::mem::offset_of!(PerCpu, running) + crate::sched::RunningSlot::WORD_OFFSET;

impl PerCpu {
    /// Whether this processor has said it is running.
    pub(crate) fn is_online(&self) -> bool {
        self.online.load(Ordering::Acquire)
    }

    /// How many inter-processor interrupts this processor has taken.
    pub(crate) fn ipis_taken(&self) -> u64 {
        self.ipis.load(Ordering::Relaxed)
    }
}

/// The machine's processors.
#[derive(Debug)]
pub(crate) struct Topology {
    /// What this architecture calls a hardware identifier.
    id_name: &'static str,
    /// One record per processor, indexed by logical number. Index zero is the
    /// boot processor.
    cpus: &'static [PerCpu],
}

impl Topology {
    /// Check what the architecture found and build a record per processor.
    fn from_described(described: Described) -> Result<Topology, &'static str> {
        let Described { id_name, boot, ids } = described;

        if ids.is_empty() {
            return Err("firmware describes no processor that can be started");
        }

        // Two processors with one identifier cannot both be addressed, and
        // the second start request would go to the first one — which is
        // already running, and would be reset.
        for (index, id) in ids.iter().enumerate() {
            if ids.iter().skip(index + 1).any(|other| other == id) {
                return Err("firmware describes two processors with the same identifier");
            }
        }

        // The processor running this is already started. If it is not in the
        // list, the list is describing some other machine — or the identifier
        // was read in a form the table does not use, which is the more likely
        // bug and the one this is here to catch.
        if !ids.contains(&boot) {
            return Err("the processor running this code is not among those firmware describes");
        }

        // FATAL-ALLOC: boot only: stage 4 builds the processor table once, before the secondaries start.
        let mut ordered = Vec::with_capacity(ids.len());
        // FATAL-ALLOC: boot only: stage 4 builds the processor table once, before the secondaries start.
        ordered.push(boot);
        // FATAL-ALLOC: boot only: stage 4 builds the processor table once, before the secondaries start.
        ordered.extend(ids.iter().copied().filter(|id| *id != boot));

        let records: Vec<PerCpu> = ordered
            .into_iter()
            .enumerate()
            .map(|(logical, hardware_id)| PerCpu {
                this: 0,
                kernel_stack: AtomicU64::new(0),
                user_stack: 0,
                logical,
                hardware_id,
                online: AtomicBool::new(logical == 0),
                job_done: AtomicU64::new(0),
                ipis: AtomicU64::new(0),
                tlb_seen: AtomicU64::new(0),
                gp_seen: AtomicU64::new(0),
                preempt: crate::sched::PreemptState::new(),
                running: crate::sched::RunningSlot::new(),
                gdt: AtomicU64::new(0),
                privilege_stack: AtomicU64::new(0),
            })
            // FATAL-ALLOC: boot only: stage 4 builds the processor table once, before the secondaries start.
            .collect();

        // Leaked, deliberately: a register on every processor is about to
        // hold an address into this, for as long as the machine runs.
        // FATAL-ALLOC: boot only: stage 4 builds the processor table once, before the secondaries start.
        let cpus: &'static mut [PerCpu] = Box::leak(records.into_boxed_slice());
        for cpu in cpus.iter_mut() {
            cpu.this = (&raw const *cpu) as u64;
        }
        Ok(Topology { id_name, cpus })
    }

    /// How many processors there are, the boot processor included.
    pub(crate) fn count(&self) -> usize {
        self.cpus.len()
    }

    /// Every processor's record, by logical number.
    pub(crate) const fn cpus(&self) -> &'static [PerCpu] {
        self.cpus
    }

    /// How many processors have said they are running.
    pub(crate) fn online(&self) -> usize {
        self.cpus.iter().filter(|cpu| cpu.is_online()).count()
    }

    /// The boot processor's hardware identifier.
    pub(crate) fn boot_id(&self) -> u64 {
        self.cpus.first().map_or(0, |cpu| cpu.hardware_id)
    }

    /// What this architecture calls a hardware identifier.
    pub(crate) const fn id_name(&self) -> &'static str {
        self.id_name
    }
}

/// The processors, once [`discover`] has run.
static TOPOLOGY: Once<Topology> = Once::new();

/// The processors, once [`discover`] has run.
pub(crate) fn topology() -> Option<&'static Topology> {
    TOPOLOGY.get()
}

/// Processor `cpu`'s record, by logical number, once [`discover`] has run:
/// for a reader of another processor's state, as a report is.
pub(crate) fn record(cpu: usize) -> Option<&'static PerCpu> {
    TOPOLOGY.get()?.cpus.get(cpu)
}

/// Set once the boot processor's record is installed.
///
/// Guards [`this_cpu`] against the one moment it would read garbage: before
/// the register has been written at all, when it holds whatever firmware left
/// there. A secondary processor installs its record before it runs any code
/// that could ask, so this only ever needs to cover the boot processor.
static LOCAL_READY: AtomicBool = AtomicBool::new(false);

/// Set by a panic and never cleared: a processor that sees it stops.
///
/// Looked at where every processor arrives whether or not it has work — the
/// inter-processor interrupt, and the wait that answers other processors'
/// shootdowns — and not in the scheduler, whose locks the panicking processor
/// may be holding.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// Find every processor firmware describes, and install the boot processor's
/// record.
///
/// Must run after `arch::init_interrupts`, because x86-64 learns which
/// processor it is by reading its own local APIC, which that call maps; and
/// before `mm::reclaim_boot_memory`, which gives the tables this reads back
/// to the frame allocator.
///
/// # Errors
///
/// If the tables cannot be read, describe a list no machine could have, or
/// the boot processor's record does not read back as its own.
pub(crate) fn discover(view: &BootView<'_>) -> Result<&'static Topology, &'static str> {
    let topology = Topology::from_described(arch::describe_cpus(view)?)?;
    let topology = TOPOLOGY.call_once(|| topology);

    let boot = topology
        .cpus
        .first()
        .ok_or("there is no record for the boot processor")?;
    // SAFETY: (SHARED) `boot` is the boot processor's own record, this is the boot
    // processor, and the record lives in a slice leaked for the life of the
    // system.
    unsafe { arch::set_cpu_local(boot.this) };
    LOCAL_READY.store(true, Ordering::Release);

    check_this_cpu(boot)?;
    Ok(topology)
}

/// How long a secondary has to report in before bring-up gives up on it.
///
/// Generous by orders of magnitude on hardware and by one under an emulator,
/// and short enough that a core which never answers costs the boot test a
/// second rather than its whole timeout.
const START_TIMEOUT_NANOS: u64 = 1_000_000_000;

/// Start every processor other than this one, one at a time.
///
/// One at a time for two reasons. The architecture's start sequence reads its
/// parameters from one shared block, which cannot be rewritten until the core
/// reading it is done; and a core that fails to come up should be named, not
/// lost in a crowd.
///
/// # Errors
///
/// If a processor cannot be started, or is started and does not report in.
pub(crate) fn start_secondaries(view: &BootView<'_>) -> Result<(), &'static str> {
    let topology = TOPOLOGY
        .get()
        .ok_or("the processors have not been discovered")?;

    // The side-channel defences, decided and applied on this processor before
    // another is started, because each applies what this one decided as it
    // starts -- and long before the first program. Here even on a machine with
    // no other processor, which the loop below simply skips.
    arch::init_speculation(view);

    // Before any secondary can unmask interrupts: an IPI arriving at a line
    // with nothing registered is counted as unclaimed and otherwise lost.
    crate::irq::register(arch::ipi_irq(), on_ipi)
        .map_err(|_| "the inter-processor interrupt's line is already taken")?;

    let mut starter = arch::CpuStarter::new(view)?;

    for cpu in topology.cpus.iter().skip(1) {
        // Never freed: nothing takes a processor offline, and this is the
        // stack it runs on for the rest of its life.
        let stack = crate::vmap::allocate_stack()
            .map_err(|_| "no kernel stack for a secondary processor")?;
        starter.start(cpu.hardware_id, stack.top, cpu.this)?;
        wait_until_online(cpu)?;
    }

    // Only now, and deliberately not on the failure paths above: a core that
    // has not reported in may still be about to read the start block, and
    // freeing it under that core would turn a missing processor into a
    // corrupted one.
    starter.finish()?;
    Ok(())
}

/// Wait for `cpu` to say it is running, for at most [`START_TIMEOUT_NANOS`].
fn wait_until_online(cpu: &PerCpu) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(START_TIMEOUT_NANOS);
    while !cpu.is_online() {
        if crate::timer::now_nanos() > deadline {
            return Err("a secondary processor was started and never reported in");
        }
        spin_loop();
    }
    Ok(())
}

/// The record at `address`, if it is one of the processors' records.
fn record_at(address: u64) -> Option<&'static PerCpu> {
    TOPOLOGY
        .get()
        .and_then(|topology| topology.cpus.iter().find(|cpu| cpu.this == address))
}

/// Make `record` this secondary processor's own, before anything on it can
/// ask [`this_cpu`].
///
/// Each architecture's start path calls this before it runs anything that
/// allocates. Allocation is the danger, not ordinary code: an arena
/// allocation that fails part way unmaps what it mapped, which runs a
/// shootdown, which reads [`this_cpu`] -- and [`LOCAL_READY`], the flag that
/// says the read is safe, is global and was set by the boot processor long
/// before. On a processor whose register still holds what reset left, that
/// read follows garbage.
///
/// `record` is checked against the processor list first, for the reason
/// [`secondary_main`] gives.
pub(crate) fn install_secondary_record(record: u64) {
    let Some(expected) = record_at(record) else {
        crate::panic::fatal!(
            crate::panic::catalog::SECONDARY_NO_RECORD,
            "a secondary processor arrived with no record of its own"
        );
    };
    // SAFETY: (SHARED) `expected` is a record in the leaked slice, and it is this
    // processor's own: `start_secondaries` passed its address to the start of
    // exactly this processor and no other.
    unsafe { arch::set_cpu_local(expected.this) };
}

/// Where every secondary processor arrives from its architecture's start
/// path: in the upper half, on its own stack, with its trap vectors and its
/// interrupt controller up, its per-CPU record installed, and every interrupt
/// masked.
///
/// `record` is the address [`start_secondaries`] handed the architecture for
/// this processor, and is checked rather than trusted: it crossed a start
/// sequence written in assembly to get here.
///
/// The architecture must already have installed it, with
/// [`install_secondary_record`], and this requires that it did.
pub(crate) fn secondary_main(record: u64) -> ! {
    let Some(expected) = record_at(record) else {
        crate::panic::fatal!(
            crate::panic::catalog::SECONDARY_NO_RECORD,
            "a secondary processor arrived with no record of its own"
        );
    };

    // Read from the register, not through it: a start path that skipped the
    // install has whatever reset left there, and following that is the bug
    // this is looking for.
    if arch::cpu_local_register() != expected.this {
        crate::panic::fatal!(
            crate::panic::catalog::SECONDARY_RECORD_MISMATCH,
            "secondary processor {}: arrived without its per-CPU record installed",
            expected.logical
        );
    }
    if let Err(problem) = check_this_cpu(expected) {
        crate::panic::fatal!(
            crate::panic::catalog::SECONDARY_RECORD_MISMATCH,
            "secondary processor {}: {problem}",
            expected.logical
        );
    }
    // Work handed out before this processor existed is not its to do.
    expected
        .job_done
        .store(JOB_GENERATION.load(Ordering::SeqCst), Ordering::Relaxed);

    // Online first, then look at the shootdown generation, then flush — in
    // that order, and sequentially consistent, against a shootdown's
    // "advance the generation, then look at who is online". Of any pair,
    // at least one sees the other: either the shootdown sees this processor
    // online and waits for it, or this processor sees the shootdown's
    // generation and the flush below covers it. A processor that came online
    // in between with a stale translation is the case neither would catch.
    expected.online.store(true, Ordering::SeqCst);
    let generation = TLB_GENERATION.load(Ordering::SeqCst);
    arch::flush_tlb();
    let _ = expected.tlb_seen.fetch_max(generation, Ordering::SeqCst);

    // The same handshake for grace periods, and a simpler one: a processor
    // that has only just come online is inside no read-side section, so it
    // can answer for every grace period requested so far.
    let _ = expected
        .gp_seen
        .fetch_max(GRACE_GENERATION.load(Ordering::SeqCst), Ordering::SeqCst);

    loop {
        // Masked while looking, so that the look and the wait are one step:
        // an interrupt sent after the look wakes the wait rather than being
        // taken, and forgotten, just before it.
        arch::disable_interrupts();

        // **Where this processor stops being stage 4's and becomes stage 5's.**
        // Until the scheduler exists this loop is all there is to do; once it
        // does, this processor's job is to run tasks, and `enter_idle` never
        // returns. Checked inside the masked region so that the handover
        // cannot happen between looking for work and waiting for it.
        //
        // Nothing after this point hands work out: `run_everywhere` belongs to
        // stage 4's checks, which have finished by the time `sched::init`
        // runs. What still reaches every processor — TLB shootdown, grace
        // periods — arrives as an interrupt, and an idle processor takes those
        // exactly as this loop did.
        if crate::sched::started() {
            crate::sched::enter_idle();
        }

        match next_job(expected) {
            Some((generation, work)) => {
                arch::enable_interrupts();
                work(expected);
                expected.job_done.store(generation, Ordering::Release);
            }
            None => arch::wait_for_work(),
        }
    }
}

/// Work to run on every processor, handed that processor's own record.
///
/// A function rather than a closure: the same code runs on every processor at
/// once, so anything it shares lives somewhere every processor can reach —
/// a static — rather than on the stack of the one that handed it out.
pub(crate) type Work = fn(&'static PerCpu);

/// The work most recently handed out.
static JOB: SpinLock<Option<Work>> = SpinLock::new(None);

/// How many pieces of work have been handed out. A processor whose `job_done`
/// is behind this has work to do.
static JOB_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Held by whoever is handing out work, so there is one piece at a time.
static HANDING_OUT: SpinLock<()> = SpinLock::new(());

/// How long a processor has to finish its share of a piece of work before the
/// caller gives up on it.
///
/// Long, because the work is the stage's tests and some of them are meant to
/// take a while; finite, because a processor that never finishes should be
/// named in the boot log rather than discovered by the test's timeout.
const WORK_TIMEOUT_NANOS: u64 = 30_000_000_000;

/// Run `work` on every online processor, this one included, and return once
/// all of them have finished.
///
/// # Errors
///
/// If the other processors cannot be interrupted, or one does not finish
/// within [`WORK_TIMEOUT_NANOS`].
pub(crate) fn run_everywhere(work: Work) -> Result<(), &'static str> {
    let _one_at_a_time = HANDING_OUT.lock();
    let topology = TOPOLOGY
        .get()
        .ok_or("the processors have not been discovered")?;
    let me = this_cpu().ok_or("the per-CPU register is not installed")?;

    *JOB.lock() = Some(work);
    let generation = JOB_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    arch::send_ipi_to_others()?;

    work(me);
    me.job_done.store(generation, Ordering::Release);

    let deadline = crate::timer::now_nanos().saturating_add(WORK_TIMEOUT_NANOS);
    for cpu in topology.cpus.iter().filter(|cpu| cpu.is_online()) {
        while cpu.job_done.load(Ordering::Acquire) < generation {
            if crate::timer::now_nanos() > deadline {
                return Err("a processor did not finish its share of the work");
            }
            // The work may be waiting on this processor's TLB.
            as_this_cpu(service_tlb);
            spin_loop();
        }
    }
    Ok(())
}

/// Run `answer` in the name of the processor this is running on, at the
/// moment it runs.
///
/// **Never with a record read earlier.** Kernel code is preemptible and a task
/// can be resumed on another processor, so a record read on entry to a long
/// wait names the processor the task *was* on. Flushing the TLB and recording
/// the flush in that record flushes one processor and vouches for another: the
/// shootdown then frees memory the processor left behind can still reach
/// through a stale translation. So the register is read, and the answer made,
/// with interrupts masked, which is what makes the two one step.
fn as_this_cpu(answer: fn(&'static PerCpu)) {
    let saved = <arch::Irq as IrqControl>::disable();
    if let Some(me) = this_cpu() {
        answer(me);
    }
    <arch::Irq as IrqControl>::restore(saved);
}

/// The work `me` has not done yet, if there is any, with its generation.
fn next_job(me: &PerCpu) -> Option<(u64, Work)> {
    let generation = JOB_GENERATION.load(Ordering::SeqCst);
    if generation <= me.job_done.load(Ordering::Relaxed) {
        return None;
    }
    // Written before the generation was advanced, and not rewritten until
    // every online processor has finished it — so this is the right work.
    let work = (*JOB.lock())?;
    Some((generation, work))
}

/// What an inter-processor interrupt does.
///
/// Every reason one is sent is answered here, whichever of them it was sent
/// for: a flush if a shootdown is outstanding, the answer to a grace period,
/// and — by returning — a wake-up for a processor waiting in
/// [`secondary_main`], which looks for work next. Answering all of them every
/// time is what lets one interrupt stand for several, which is what two sent
/// before the first is taken become.
fn on_ipi(_irq: u32) {
    halt_if_stopping();
    if let Some(me) = this_cpu() {
        let _ = me.ipis.fetch_add(1, Ordering::Relaxed);
        service_tlb(me);
        // Being here is the answer: an interrupt is never taken inside a
        // read-side section, so this processor is outside one, and was when
        // every grace period requested so far began waiting.
        answer_grace_periods(me);
    }
}

// ---------------------------------------------------------------------------
// TLB shootdown
// ---------------------------------------------------------------------------

/// Shootdowns requested so far. A processor whose `tlb_seen` is behind this
/// may still hold translations it has been told to drop.
static TLB_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Held by the processor running a shootdown, so there is one at a time.
static SHOOTING: SpinLock<()> = SpinLock::new(());

/// Shootdowns run, for the boot log.
static SHOOTDOWNS: AtomicU64 = AtomicU64::new(0);

/// How long a shootdown waits for a processor before counting its answer
/// late, at the least: see [`patience`] for the other half of the bound, and
/// [`Bounds`] for what a late answer costs.
const SHOOTDOWN_LATE_NANOS: u64 = 1_000_000_000;

/// How long any wait for other processors waits for one of them before
/// calling it stuck and stopping the machine, at the least: the other half is
/// [`patience`] of it.
///
/// Until 2026-10-07 the late bound was this bound, and FX-0001 stopped
/// self-hosting builds on a loaded host with nothing stuck: with one virtual
/// processor's host thread starved on nazuna, 257 waits passed one second
/// and every one was answered, after 1.0 to 3.6 s. Ten seconds is about three
/// times the worst of those, and, at about 51 s under `tcg` once [`patience`]
/// has had its say, still inside a boot test's timeout, so the boot that hit
/// a stuck processor still reports it.
const STUCK_NANOS: u64 = 10_000_000_000;

/// The two bounds a wait for other processors has.
///
/// **Late** is where a processor that has not answered is counted late: the
/// waiter keeps waiting, and the answer, when it comes, is reported once the
/// shootdown turn is given back ([`report_late`]). **Stuck** is where it gives
/// up on the machine (FX-0001, FX-0002). Each is a wall-clock floor and a
/// count of the waiter's own polls together, for the reason [`patience`]
/// gives. Between the two the cost is availability alone: the waiter's
/// processor runs nothing else, since the turn disables preemption, and other
/// shootdowns queue for the turn; nothing is freed and no permission relied on
/// before every processor has answered.
#[derive(Clone, Copy)]
pub(crate) struct Bounds {
    /// The late bound's wall-clock floor.
    late_nanos: u64,
    /// The late bound's count of polls.
    late_polls: u64,
    /// The stuck bound's wall-clock floor.
    stuck_nanos: u64,
    /// The stuck bound's count of polls.
    stuck_polls: u64,
    /// Set the moment the wait first counts an answer late: what a check
    /// holding a processor waits on. No product wait has one.
    mark: Option<&'static AtomicBool>,
}

impl Bounds {
    /// A product wait's bounds: late after `late_nanos` and [`patience`] of
    /// it, stuck after [`STUCK_NANOS`] and patience of that.
    const fn product(late_nanos: u64) -> Bounds {
        Bounds {
            late_nanos,
            late_polls: patience(late_nanos),
            stuck_nanos: STUCK_NANOS,
            stuck_polls: patience(STUCK_NANOS),
            mark: None,
        }
    }

    /// A check's bounds: late after `late_nanos` and `late_polls`, `mark` set
    /// when it is; stuck as a product wait is.
    pub(crate) const fn checked(
        late_nanos: u64,
        late_polls: u64,
        mark: &'static AtomicBool,
    ) -> Bounds {
        Bounds {
            late_nanos,
            late_polls,
            stuck_nanos: STUCK_NANOS,
            stuck_polls: patience(STUCK_NANOS),
            mark: Some(mark),
        }
    }
}

/// The processors one wait counted late, and the longest of their waits.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Late {
    /// The processors that answered past the late bound.
    pub(crate) cpus: CpuSet,
    /// How many they were.
    pub(crate) count: u64,
    /// Nanoseconds from the start of the wait to the last late answer.
    pub(crate) longest_nanos: u64,
    /// Polls the waiter had made by then.
    pub(crate) polls: u64,
}

/// Late answers reported so far, for the report's rate and the boot log.
static LATE_ANSWERS: AtomicU64 = AtomicU64::new(0);

/// The longest wait that ended in a late answer, in nanoseconds.
static LATE_LONGEST_NANOS: AtomicU64 = AtomicU64::new(0);

/// How many late answers get a line of their own; after these, a line each
/// time the count reaches a power of two.
const LATE_LINES: u64 = 4;

/// Count and report the late answers of one wait.
///
/// Called only after the shootdown turn is given back, never from inside a
/// wait, and so never with the turn held: a line takes the console's lock,
/// which a processor the waiter is waiting for may hold. Nothing calls this
/// from the console. Rate-limited: [`LATE_LINES`] lines with the processor
/// and the time, then one summary each time the count doubles.
fn report_late(late: Late, what: &str) {
    if late.count == 0 {
        return;
    }
    let total = LATE_ANSWERS.fetch_add(late.count, Ordering::Relaxed) + late.count;
    let longest = LATE_LONGEST_NANOS
        .fetch_max(late.longest_nanos, Ordering::Relaxed)
        .max(late.longest_nanos);
    let before = total - late.count;
    if before < LATE_LINES {
        crate::console::println!(
            "  smp      {} processor(s) {what} late, the last {} ms after it was asked \
             ({} polls): waited for, not stuck",
            late.count,
            late.longest_nanos / 1_000_000,
            late.polls
        );
    } else if total.ilog2() != before.ilog2() {
        crate::console::println!(
            "  smp      {total} late answers so far, the longest {} ms after it was asked",
            longest / 1_000_000
        );
    }
}

/// How many times a waiter asks, for each second of its wall-clock bound,
/// before a processor that has not answered is taken to be stuck.
///
/// See [`patience`]. What a shootdown's count came to on the development
/// host, a processor made to stop answering with interrupts masked
/// (2026-09-26): 1.8 s under KVM, 5.1 s under QEMU's `tcg`, 32 s under
/// `tcg` with the coverage plugin -- against the one second every one of them
/// used to get.
const POLLS_PER_SECOND: u64 = 1 << 24;

/// How many polls a waiter makes between readings of the clock.
///
/// A poll is a few loads and an interrupt mask round trip; a reading of the
/// clock can be three loads from the HPET, each a trip out of the guest under
/// a hypervisor and a device access under QEMU's global lock under `tcg` --
/// the lock the processor being waited for needs to take its interrupt. Read
/// every poll, the clock was nineteen microseconds of each one under KVM, and
/// the count would have measured the HPET rather than the guest.
const CLOCK_EVERY: u64 = 256;

/// The polls a wait whose wall-clock bound is `timeout` makes before it may
/// give up: [`POLLS_PER_SECOND`] for each whole second.
///
/// **Why a wait is bounded twice, and why the count is the bound that
/// matters.** Waiting longer never makes a shootdown or a grace period
/// unsafe -- nothing is freed and no permission is relied on until every
/// processor has answered -- so the bound's one job is to turn a processor
/// that will never answer into a report, and never to call a live one stuck.
/// A wall-clock bound cannot do the second. It measures the host, not the
/// guest: an emulated machine runs its code tens of times slower than the
/// processor it emulates, and slower again under an instrumenting plugin,
/// which also serialises every virtual processor on one lock per translated
/// block; a section with interrupts masked that takes microseconds on a
/// board takes a second there, and a virtual processor the host does not run
/// for a while answers nothing in the meantime. Under the coverage plugin a
/// second was not enough (`test-compositor`, 2026-09-26).
///
/// A count of the waiter's own polls is a clock in the guest's units. Each
/// poll is the same short run of instructions, so whatever slows the machine
/// slows the count with it, and while the waiter itself is not running -- a
/// task in [`synchronize`] preempted, or any waiter's virtual processor
/// descheduled by the host -- the count stands still where the wall clock
/// would run on. A processor that is stuck answers no number of polls, so it
/// is still found; the count only decides how long finding it takes. The
/// wall-clock half keeps a wait on hardware fast enough to make the count
/// in less than the old bound from ending sooner than it did before.
///
/// What it does not cover: a host that keeps running the waiter and stops
/// running the processor it waits for. The count then passes at full speed.
/// No guest can see that from inside without the host's help, and KVM's
/// steal time sees only part of it (a runnable virtual processor preempted,
/// not one woken from a halt and not yet run, nor one held in the emulator)
/// while it would make the kernel trust a page the host writes. That is what
/// the two bounds of [`Bounds`] are for: past the late one such a processor
/// is waited for, and only the stuck one, ten times later, calls it stuck.
const fn patience(timeout: u64) -> u64 {
    POLLS_PER_SECOND.saturating_mul(timeout / 1_000_000_000)
}

/// How long the shootdown generation may stand still while a processor waits
/// for its turn before the holder is taken to have stopped, at the least; the
/// waiter must also have asked [`patience`] of it times.
///
/// A holder gives up on the machine after [`STUCK_NANOS`] and its own polls'
/// worth of waiting, so anything past that is a holder not running at all.
/// Four times both, because a holder preempted on a host with more virtual
/// processors than real ones can lose whole seconds without being stuck.
const TURN_TIMEOUT_NANOS: u64 = 4 * STUCK_NANOS;

/// How often a processor waiting on all the others re-sends its interrupt.
///
/// Belt and braces. The handshake in [`secondary_main`] already covers the
/// processor that comes online in the middle of a wait, and an interrupt is
/// not otherwise lost — but a lost one would cost the whole timeout and the
/// machine, and a second one costs a flush.
const KICK_NANOS: u64 = 10_000_000;

/// Drop stale translations from every processor's TLB, and return once every
/// processor has.
///
/// What an unmap or a narrowed permission has to call before it frees the
/// memory or relies on the permission. Where the architecture's own
/// invalidation is broadcast that is all this does; elsewhere it interrupts
/// every other processor and waits for each to flush.
///
/// **Must not be called holding a lock another processor might be spinning
/// on with interrupts masked.** That processor could not take the interrupt,
/// and this would wait for it until the timeout. The waiting itself services
/// other processors' shootdowns, so two running at once do not deadlock on
/// each other.
///
/// A processor that answers past the late bound is waited for and reported
/// once the turn is given back ([`Bounds`]).
pub(crate) fn flush_tlb_everywhere() {
    report_late(
        flush_everywhere_within(Bounds::product(SHOOTDOWN_LATE_NANOS)),
        FLUSHED,
    );
}

/// What a processor answering [`flush_tlb_everywhere`] has done.
const FLUSHED: &str = "flushed its TLB for a shootdown";

/// [`flush_tlb_everywhere`] within `bounds`, returning the late answers it
/// waited for with the turn already given back, and reporting none of them.
pub(crate) fn flush_everywhere_within(bounds: Bounds) -> Late {
    shootdown_requested();
    arch::flush_tlb();
    if arch::TLB_FLUSH_IS_BROADCAST {
        return Late::default();
    }
    // Before discovery, or with nobody else running, there is nobody to tell.
    let Some(topology) = TOPOLOGY.get().filter(|_| this_cpu().is_some()) else {
        return Late::default();
    };
    if topology.online() <= 1 {
        return Late::default();
    }
    shootdown_waits_for_others();

    let _turn = take_turn();

    // The flush at the top was on whichever processor this was then. The one
    // it answers for has to be the one it is on now, which `service_tlb` sees
    // is behind the generation just taken, and flushes.
    let generation = TLB_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    as_this_cpu(service_tlb);
    let _ = arch::send_ipi_to_others();

    let late = wait_for(
        topology,
        bounds,
        FLUSHED,
        &crate::panic::catalog::SHOOTDOWN_TIMEOUT,
        service_tlb,
        PerCpu::is_online,
        |cpu| cpu.tlb_seen.load(Ordering::SeqCst) >= generation,
        || {
            let _ = arch::send_ipi_to_others();
        },
    );
    let _ = SHOOTDOWNS.fetch_add(1, Ordering::Relaxed);
    late
}

/// The first rule every shootdown is asked under, checked where it is asked:
/// no lock that disables preemption is held.
///
/// A shootdown waits for every other processor to answer an interrupt. A
/// processor spinning for a lock this one holds answers it too, because it
/// spins with interrupts on -- so holding a preemption-disabling lock here
/// does not deadlock. It does hold that lock, contended, with preemption
/// off, for an interrupt round trip, which is what the audit of 2026-09-13
/// found `brk`, the alarm clock and `channel_read` doing. Checked on every
/// architecture, the broadcast pair included, so a mistake on one is found
/// by every boot rather than by the x86-64 row alone. The reaper's by-hand
/// [`crate::sched::preempt_disable`] around freeing a stack is not a lock
/// and passes; `sched::locks_held` counts locks only.
fn shootdown_requested() {
    // Which processor and its counts in one read, for the reason
    // `sched::locks_here` gives; the site is the outermost lock's, not the
    // last raise, since with the count still up the last raise may be a lock
    // released since.
    let Some((cpu, held, site)) = crate::sched::locks_here() else {
        return;
    };
    debug_assert!(
        held == 0,
        "a shootdown was requested on processor {cpu} holding {held} lock(s) that disable \
         preemption, the outermost taken at {}:{}",
        site.map_or("?", |site| site.file()),
        site.map_or(0, core::panic::Location::line),
    );
}

/// The second rule, checked only where a shootdown is about to wait for
/// another processor: interrupts are on.
///
/// A processor that must answer this shootdown does so from an interrupt,
/// and a lock this one took with interrupts masked is one that processor may
/// be spinning on with interrupts masked in turn: it could never answer, and
/// this one would wait until the timeout stops the machine. A shootdown that
/// waits for nobody else is exempt on purpose: stage 6's checks fault a page
/// with interrupts masked, to keep a space installed on one processor while
/// they look at it, and a copy-on-write fault there retires the page it
/// displaced through a scoped shootdown whose set names only that
/// processor. Such a flush still takes the turn, but the wait for the turn
/// answers other processors' shootdowns as it spins, and once it holds the
/// turn it answers for itself and waits for no one. Only once tasks run:
/// the boot processor flushes with interrupts masked while it reclaims boot
/// memory, and nobody is waiting on it then.
fn shootdown_waits_for_others() {
    debug_assert!(
        !crate::sched::started() || arch::interrupts_enabled(),
        "a shootdown was requested with interrupts masked, which no other processor could answer"
    );
}

/// Wait for the shootdown turn, and hold it until the guard drops.
///
/// Waiting for the turn is where a processor spins longest, so it looks for
/// a panic's stop request as the wait for everyone does. And it gives up:
/// every holder takes a new generation as soon as it has the turn, so a
/// generation that moves is a holder that is alive, and one that stands
/// still for longer than any holder may wait is a holder that stopped
/// running with the turn in hand.
fn take_turn() -> impl Sized {
    let mut seen = TLB_GENERATION.load(Ordering::SeqCst);
    let mut since = crate::timer::now_nanos();
    let mut polls: u64 = 0;
    loop {
        if let Some(turn) = SHOOTING.try_lock() {
            break turn;
        }
        halt_if_stopping();
        as_this_cpu(service_tlb);
        polls = polls.saturating_add(1);
        if !polls.is_multiple_of(CLOCK_EVERY) {
            spin_loop();
            continue;
        }
        let generation = TLB_GENERATION.load(Ordering::SeqCst);
        let now = crate::timer::now_nanos();
        let stood = now.saturating_sub(since);
        if generation != seen {
            seen = generation;
            since = now;
            polls = 0;
        } else if stood > TURN_TIMEOUT_NANOS && polls > patience(TURN_TIMEOUT_NANOS) {
            crate::panic::fatal!(
                crate::panic::catalog::SHOOTDOWN_TURN_TIMEOUT,
                "no shootdown started for {} ms while this processor waited for its turn, \
                 asking {polls} times",
                stood / 1_000_000
            );
        }
        spin_loop();
    }
}

/// Wait until `done` holds for every processor `waited` names.
///
/// Shared by the things that interrupt other processors and wait for each to
/// answer: every online processor for [`flush_tlb_everywhere`] and
/// [`synchronize`], the online members of a set for [`flush_tlb_pages`].
/// While it waits it runs `answer` for the processor it is on, which answers
/// other processors' shootdowns — two processors each waiting for the other
/// would otherwise wait forever — and answers for this processor itself if the
/// waiting task has moved to one the interrupt was not sent to. It calls
/// `kick` to re-send its interrupt every [`KICK_NANOS`].
///
/// Once the late bound of `bounds` has passed it counts the processor late,
/// sets the bounds' mark if they have one, and keeps waiting; what it returns
/// says which processors were late and how long the last of them took. Once
/// the stuck bound has passed it gives up on the machine: a processor that
/// never answers is one whose TLB or whose read-side section nothing can
/// vouch for any more, and carrying on would be carrying on regardless. Each
/// bound is a wall-clock floor *and* a count of polls, because the wall clock
/// alone measures the host rather than the guest; [`patience`] argues it. It
/// prints nothing: a late answer is the caller's to report, once the turn is
/// given back.
#[expect(
    clippy::too_many_arguments,
    reason = "three callers differ in exactly these; a struct would name each once more"
)]
fn wait_for<K>(
    topology: &Topology,
    bounds: Bounds,
    what: &str,
    entry: &'static crate::panic::catalog::Explanation,
    answer: fn(&'static PerCpu),
    waited: impl Fn(&PerCpu) -> bool,
    done: impl Fn(&PerCpu) -> bool,
    kick: impl Fn() -> K,
) -> Late {
    let started = crate::timer::now_nanos();
    let mut kicked = started;
    let mut polls: u64 = 0;
    let mut late = Late::default();
    for cpu in topology.cpus.iter().filter(|cpu| waited(cpu)) {
        let mut counted = false;
        while !done(cpu) {
            halt_if_stopping();
            as_this_cpu(answer);
            polls = polls.saturating_add(1);
            if !polls.is_multiple_of(CLOCK_EVERY) {
                spin_loop();
                continue;
            }
            let now = crate::timer::now_nanos();
            let waited = now.saturating_sub(started);
            if waited > bounds.stuck_nanos && polls > bounds.stuck_polls {
                crate::panic::fatal!(
                    *entry,
                    "processor {} never {what}: no answer in {} ms, asked {polls} times",
                    cpu.logical,
                    waited / 1_000_000
                );
            }
            if !counted && waited > bounds.late_nanos && polls > bounds.late_polls {
                counted = true;
                if let Some(mark) = bounds.mark {
                    mark.store(true, Ordering::SeqCst);
                }
            }
            if now.saturating_sub(kicked) > KICK_NANOS {
                let _ = kick();
                kicked = now;
            }
            spin_loop();
        }
        if counted {
            // NOALLOC: a `CpuSet` is a fixed bit set.
            let _ = late.cpus.insert(cpu.logical);
            late.count = late.count.saturating_add(1);
            late.longest_nanos = crate::timer::now_nanos().saturating_sub(started);
            late.polls = polls;
        }
    }
    late
}

/// Flush this processor's TLB if a shootdown is waiting for it to.
///
/// A generation [`flush_tlb_everywhere`] took, or one taken by anything but
/// [`flush_tlb_pages`], asks for the whole TLB. A generation `flush_tlb_pages`
/// took asks only the processors in its set, and only for its pages; a
/// processor outside the set answers it without flushing, because it held none
/// of the set's spaces' translations when the set was read.
///
/// Only a processor exactly one generation behind trusts a scoped request. One
/// further behind flushes everything: the requests it missed are gone, and
/// rather than rest on an argument about which of them could have concerned
/// it, it answers all of them the one way that is always enough.
///
/// `me` must be the record of the processor running this, read with
/// interrupts masked since: the interrupt handler, or [`as_this_cpu`].
fn service_tlb(me: &PerCpu) {
    let wanted = TLB_GENERATION.load(Ordering::SeqCst);
    let seen = me.tlb_seen.load(Ordering::SeqCst);
    if seen < wanted {
        let scoped = if seen.saturating_add(1) == wanted {
            scoped_request(wanted, me.logical)
        } else {
            None
        };
        match scoped {
            Some(pages) if pages.everything => arch::flush_tlb(),
            Some(pages) => {
                for &address in pages.addresses() {
                    arch::flush_tlb_page(address);
                }
            }
            None => arch::flush_tlb(),
        }
        // A maximum rather than a store: an interrupt taken between the load
        // above and here may have recorded a later generation already, and
        // a store would take it back.
        let _ = me.tlb_seen.fetch_max(wanted, Ordering::SeqCst);
    }
}

/// How many shootdowns have run.
pub(crate) fn shootdowns() -> u64 {
    SHOOTDOWNS.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Scoped shootdown: some pages, on the processors that may hold them
// ---------------------------------------------------------------------------

/// The most pages a scoped shootdown invalidates one at a time.
///
/// Past this it flushes the whole TLB instead: a flush costs a re-walk of
/// whatever is used next, and a few dozen single invalidations cost about the
/// same. Linux's `tlb_single_page_flush_ceiling` is 33 for the same reason.
pub(crate) const PAGE_FLUSH_CEILING: usize = 33;

/// Words of a [`CpuMask`]: one bit per processor a [`CpuSet`] can name.
const CPU_WORDS: usize = MAX_CPUS.div_ceil(64);

/// A set of processors that is joined and left without a lock.
///
/// What an address space keeps of the processors whose TLB may still hold its
/// translations. A [`CpuSet`] is a value; this is the shared, changing thing a
/// shootdown takes a [`CpuMask::snapshot`] of. Every operation is sequentially
/// consistent, which is what the join-before-install and read-after-takedown
/// argument in [`crate::user::space`] needs.
#[derive(Debug)]
pub(crate) struct CpuMask {
    /// One bit per logical processor.
    words: [AtomicU64; CPU_WORDS],
}

impl CpuMask {
    /// No processors.
    pub(crate) const fn new() -> CpuMask {
        CpuMask {
            words: [const { AtomicU64::new(0) }; CPU_WORDS],
        }
    }

    /// Add processor `cpu`.
    ///
    /// A processor past [`MAX_CPUS`] cannot be named, and the scheduler refuses
    /// to start on a machine that has one, so nothing runs a space there.
    pub(crate) fn join(&self, cpu: usize) {
        if let Some(word) = self.words.get(cpu / 64) {
            let _ = word.fetch_or(1 << (cpu % 64), Ordering::SeqCst);
        }
    }

    /// Remove processor `cpu`.
    pub(crate) fn leave(&self, cpu: usize) {
        if let Some(word) = self.words.get(cpu / 64) {
            let _ = word.fetch_and(!(1 << (cpu % 64)), Ordering::SeqCst);
        }
    }

    /// The processors in the set at the moment of reading.
    pub(crate) fn snapshot(&self) -> CpuSet {
        let mut set = CpuSet::empty();
        for (index, word) in self.words.iter().enumerate() {
            let bits = word.load(Ordering::SeqCst);
            for bit in (0..64).filter(|bit| bits & (1 << bit) != 0) {
                // NOALLOC: a `CpuSet` is a fixed bit set.
                let _ = set.insert(index * 64 + bit);
            }
        }
        set
    }
}

/// Add every processor in `from` to `into`.
pub(crate) fn add_cpus(into: &mut CpuSet, from: &CpuSet) {
    for cpu in from.iter() {
        // NOALLOC: a `CpuSet` is a fixed bit set.
        let _ = into.insert(cpu);
    }
}

/// The pages a shootdown asks to have invalidated: up to
/// [`PAGE_FLUSH_CEILING`] of them, or everything -- and the page tables the
/// unmaps behind it emptied, which [`flush_tlb_pages`] gives back once the
/// invalidation has reached everyone (finding F-36).
///
/// Neither `Clone` nor `Copy`: the tables have one owner, and a copy that was
/// flushed would leave the original holding tables already given back.
#[derive(Debug)]
pub(crate) struct TlbPages {
    /// The page addresses, the first `count` of them meaningful.
    pages: [u64; PAGE_FLUSH_CEILING],
    /// How many of `pages` are.
    count: usize,
    /// More were asked for than fit, or the caller could not say which: the
    /// whole TLB.
    everything: bool,
    /// Tables [`crate::mm::unmap_in`] unlinked, each translating at least one
    /// address this shootdown invalidates.
    tables: crate::mm::UnlinkedTables,
}

impl TlbPages {
    /// No pages.
    pub(crate) const fn new() -> TlbPages {
        TlbPages {
            pages: [0; PAGE_FLUSH_CEILING],
            count: 0,
            everything: false,
            tables: crate::mm::UnlinkedTables::new(),
        }
    }

    /// The tables held for release after this shootdown.
    pub(crate) fn tables(&mut self) -> &mut crate::mm::UnlinkedTables {
        &mut self.tables
    }

    /// Whether there is nothing to invalidate.
    pub(crate) fn is_empty(&self) -> bool {
        !self.everything && self.count == 0
    }

    /// Whether the whole TLB is asked for.
    pub(crate) fn is_everything(&self) -> bool {
        self.everything
    }

    /// The page addresses, unless the whole TLB is asked for.
    pub(crate) fn addresses(&self) -> &[u64] {
        self.pages.get(..self.count).unwrap_or(&[])
    }

    /// Ask for the whole TLB.
    pub(crate) fn everything(&mut self) {
        self.everything = true;
    }

    /// Ask for the page holding `address`.
    pub(crate) fn add(&mut self, address: u64) {
        if self.everything {
            return;
        }
        let page = address & !(PAGE_SIZE - 1);
        if self.addresses().contains(&page) {
            return;
        }
        match self.pages.get_mut(self.count) {
            Some(slot) => {
                *slot = page;
                self.count += 1;
            }
            None => self.everything = true,
        }
    }

    /// Ask for every page of `len` bytes from `start`.
    pub(crate) fn add_range(&mut self, start: u64, len: u64) {
        let pages = len.div_ceil(PAGE_SIZE);
        if pages > PAGE_FLUSH_CEILING as u64 {
            self.everything = true;
            return;
        }
        for index in 0..pages {
            self.add(start.saturating_add(index * PAGE_SIZE));
        }
    }

    /// Ask for everything `other` asks for as well, and take over the tables
    /// it holds, which this shootdown now covers.
    pub(crate) fn add_all(&mut self, other: &mut TlbPages) {
        if other.everything {
            self.everything = true;
        }
        for &address in other.addresses() {
            self.add(address);
        }
        self.tables.take_from(&mut other.tables);
    }
}

/// The generation the request below belongs to, or zero while one is being
/// written. Written by the holder of the turn, read by whoever answers.
static SCOPED_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The processors the current scoped request is for.
static SCOPED_CPUS: [AtomicU64; CPU_WORDS] = [const { AtomicU64::new(0) }; CPU_WORDS];

/// How many of [`SCOPED_PAGES`] the current request uses, or `u64::MAX` for
/// the whole TLB.
static SCOPED_COUNT: AtomicU64 = AtomicU64::new(0);

/// The page addresses of the current scoped request.
static SCOPED_PAGES: [AtomicU64; PAGE_FLUSH_CEILING] =
    [const { AtomicU64::new(0) }; PAGE_FLUSH_CEILING];

/// Scoped shootdowns that interrupted at least one processor.
static SCOPED_SHOOTDOWNS: AtomicU64 = AtomicU64::new(0);

/// Processors those shootdowns interrupted, in all.
static SCOPED_TARGETS: AtomicU64 = AtomicU64::new(0);

/// Scoped shootdowns that asked nobody: an empty set, one processor, or the
/// Arm pair's hardware broadcast.
static SCOPED_UNSENT: AtomicU64 = AtomicU64::new(0);

/// What the scoped request of generation `wanted` asks of processor `cpu`,
/// or `None` if that generation was not a scoped one, or its request was
/// being rewritten under the read -- both of which the caller answers with a
/// whole flush, which is always enough.
///
/// A seqlock without the counter: the writer zeroes [`SCOPED_GENERATION`],
/// writes the request, and only then stores the generation, so a request read
/// between two loads of an unchanged generation is that generation's whole.
fn scoped_request(wanted: u64, cpu: usize) -> Option<TlbPages> {
    if SCOPED_GENERATION.load(Ordering::SeqCst) != wanted {
        return None;
    }
    let member = SCOPED_CPUS
        .get(cpu / 64)
        .is_some_and(|word| word.load(Ordering::SeqCst) & (1 << (cpu % 64)) != 0);
    let mut pages = TlbPages::new();
    if member {
        let count = SCOPED_COUNT.load(Ordering::SeqCst);
        match usize::try_from(count) {
            Ok(count) if count <= PAGE_FLUSH_CEILING => {
                for slot in SCOPED_PAGES.iter().take(count) {
                    pages.add(slot.load(Ordering::SeqCst));
                }
            }
            _ => pages.everything(),
        }
    }
    (SCOPED_GENERATION.load(Ordering::SeqCst) == wanted).then_some(pages)
}

/// Invalidate `pages` on every processor in `cpus`, and return once each has.
///
/// The shootdown for a change to one address space's tables, scoped twice
/// over: to the pages that changed, up to [`PAGE_FLUSH_CEILING`] of them, and
/// to the processors whose TLB may still hold that space's translations. The
/// caller has taken the translations down and read `cpus` afterwards, under
/// the space's lock, and must not free what they reached until this returns
/// -- `crate::user::space` says why that order is the whole of what makes it
/// safe.
///
/// On x86-64 each processor in the set other than this one is interrupted by
/// name and answers with `invlpg` per page; the processor this is running on,
/// if it is in the set, answers for itself. The turn and the generations are
/// [`flush_tlb_everywhere`]'s, so the two never run at once and a processor
/// answering one never loses the other. An empty set, or nothing to
/// invalidate, interrupts nobody.
///
/// On `AArch64` and ARMv7-A invalidation is broadcast in hardware, and this
/// is a page-scoped broadcast (`TLBI VAAE1IS`, `TLBIMVAAIS`) to every core,
/// whatever the set, and by address for every ASID. On ARMv7-A, which gives
/// each space an ASID (`docs/OPAQUE-KERNEL.md` §9.13), a processor that left
/// a space's set may still hold its entries under its number, and the
/// broadcast is what reaches it (L.armv7a.16). An invalidation scoped by the
/// space's ASID would now be sound there too; it is a later step, measured on
/// the board first. On `AArch64` every user translation is tagged with ASID
/// zero, so there is no way to tell a core to drop one space's entries
/// without telling it to drop every space's.
///
/// **Must not be called holding a spin lock**, for the reason
/// [`flush_tlb_everywhere`] gives and one more: the wait spins, and a holder
/// waiting on another processor's answer is a holder every waiter for the lock
/// waits on too.
///
/// Once the invalidation has reached every processor it must, the page tables
/// `pages` holds go back to the allocator -- and not before, since until
/// then a processor may still walk through one it cached (finding F-36).
pub(crate) fn flush_tlb_pages(cpus: &CpuSet, pages: &mut TlbPages) {
    let late = invalidate_pages(cpus, pages);
    pages.tables.release();
    // Reported with the turn given back, never under it: see `report_late`.
    report_late(late, INVALIDATED);
}

/// [`flush_tlb_pages`]'s invalidation, returning once every processor in
/// `cpus` has answered, or at once when none can hold the translations, with
/// the late answers it waited for and the turn already given back.
fn invalidate_pages(cpus: &CpuSet, pages: &TlbPages) -> Late {
    shootdown_requested();
    if pages.is_empty() {
        return Late::default();
    }
    if arch::TLB_FLUSH_IS_BROADCAST {
        if pages.is_everything() {
            arch::flush_tlb();
        } else {
            for &address in pages.addresses() {
                arch::flush_tlb_page(address);
            }
        }
        let _ = SCOPED_UNSENT.fetch_add(1, Ordering::Relaxed);
        return Late::default();
    }
    if cpus.is_empty() {
        let _ = SCOPED_UNSENT.fetch_add(1, Ordering::Relaxed);
        return Late::default();
    }
    // Before discovery, or with nobody else running, the only TLB that can
    // hold anything is this one.
    let Some(topology) = TOPOLOGY
        .get()
        .filter(|topology| this_cpu().is_some() && topology.online() > 1)
    else {
        flush_here(pages);
        let _ = SCOPED_UNSENT.fetch_add(1, Ordering::Relaxed);
        return Late::default();
    };
    let me = this_cpu().map(|cpu| cpu.logical);
    if topology
        .cpus
        .iter()
        .any(|cpu| cpu.is_online() && cpus.contains(cpu.logical) && Some(cpu.logical) != me)
    {
        shootdown_waits_for_others();
    }

    let _turn = take_turn();

    // Written before the generation that points at it, for the reason
    // `scoped_request` gives. Nobody else writes it: that is what the turn is.
    SCOPED_GENERATION.store(0, Ordering::SeqCst);
    for (index, word) in SCOPED_CPUS.iter().enumerate() {
        let bits = (0..64)
            .filter(|bit| cpus.contains(index * 64 + bit))
            .fold(0_u64, |bits, bit| bits | 1 << bit);
        word.store(bits, Ordering::SeqCst);
    }
    // A list longer than the slots is the whole TLB, never a count past what
    // was written: a reader trusting that count would read slots nobody filled
    // for this generation, or skip pages it was asked for.
    let addresses = pages.addresses();
    let count = if pages.is_everything() || addresses.len() > SCOPED_PAGES.len() {
        u64::MAX
    } else {
        for (slot, &address) in SCOPED_PAGES.iter().zip(addresses) {
            slot.store(address, Ordering::SeqCst);
        }
        debug_assert!(
            addresses.len() <= SCOPED_PAGES.len(),
            "a scoped request counted more pages than it has slots"
        );
        addresses.len() as u64
    };
    SCOPED_COUNT.store(count, Ordering::SeqCst);
    let generation = TLB_GENERATION.load(Ordering::SeqCst) + 1;
    SCOPED_GENERATION.store(generation, Ordering::SeqCst);
    TLB_GENERATION.store(generation, Ordering::SeqCst);

    // For the processor this is on now, if it is in the set; `service_tlb`
    // reads the request as any processor would.
    as_this_cpu(service_tlb);

    let member = |cpu: &PerCpu| cpu.is_online() && cpus.contains(cpu.logical);
    let behind = |cpu: &PerCpu| cpu.tlb_seen.load(Ordering::SeqCst) < generation;
    let send = || {
        let mut sent = 0;
        for cpu in topology
            .cpus
            .iter()
            .filter(|cpu| member(cpu) && behind(cpu))
        {
            if arch::send_ipi_to(cpu.hardware_id).is_ok() {
                sent += 1;
            }
        }
        sent
    };
    let targets: u64 = send();

    let late = wait_for(
        topology,
        Bounds::product(SHOOTDOWN_LATE_NANOS),
        INVALIDATED,
        &crate::panic::catalog::SHOOTDOWN_TIMEOUT,
        service_tlb,
        member,
        |cpu| !behind(cpu),
        send,
    );
    if targets == 0 {
        let _ = SCOPED_UNSENT.fetch_add(1, Ordering::Relaxed);
    } else {
        let _ = SCOPED_SHOOTDOWNS.fetch_add(1, Ordering::Relaxed);
        let _ = SCOPED_TARGETS.fetch_add(targets, Ordering::Relaxed);
    }
    late
}

/// What a processor answering [`invalidate_pages`] has done.
const INVALIDATED: &str = "invalidated its TLB for a scoped shootdown";

/// Invalidate `pages` on this processor only.
fn flush_here(pages: &TlbPages) {
    if pages.is_everything() {
        arch::flush_tlb();
    } else {
        for &address in pages.addresses() {
            arch::flush_tlb_page(address);
        }
    }
}

/// What the scoped shootdowns have cost so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ScopedShootdowns {
    /// Those that interrupted at least one processor.
    pub(crate) sent: u64,
    /// The processors those interrupted, in all.
    pub(crate) processors: u64,
    /// Those that interrupted nobody.
    pub(crate) unsent: u64,
}

/// How many scoped shootdowns have run, and how many processors they cost.
pub(crate) fn scoped_shootdowns() -> ScopedShootdowns {
    ScopedShootdowns {
        sent: SCOPED_SHOOTDOWNS.load(Ordering::Relaxed),
        processors: SCOPED_TARGETS.load(Ordering::Relaxed),
        unsent: SCOPED_UNSENT.load(Ordering::Relaxed),
    }
}

// ---------------------------------------------------------------------------
// Grace periods
// ---------------------------------------------------------------------------

/// Grace periods requested so far. A processor whose `gp_seen` is behind this
/// may still be inside a read-side section the latest one has to wait for.
static GRACE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Grace periods completed, for the boot log.
static GRACE_PERIODS: AtomicU64 = AtomicU64::new(0);

/// How long a grace period waits for a processor before counting its answer
/// late; it is called stuck at [`STUCK_NANOS`], as a shootdown is.
///
/// Much longer than a shootdown's, because a shootdown waits for a flush and
/// this waits for other code to finish. But a read-side section that lasts
/// five seconds is a bug in its own right, or a host that stopped running the
/// processor, and either is worth a line in the log.
const GRACE_LATE_NANOS: u64 = 5_000_000_000;

/// Run `body` as a read-side section.
///
/// Inside one, a pointer loaded from shared data stays valid until the
/// section ends, even if a writer unpublishes the object meanwhile: the
/// writer frees it only after [`synchronize`], which waits for every section
/// that could have loaded it.
///
/// A section masks interrupts, and that is the whole mechanism.
/// [`synchronize`] interrupts every other processor and waits for each to
/// take the interrupt, which none can do inside a section — so once every one
/// has, every section that was running when it started has ended. It costs a
/// reader two instructions; what it asks of one is to be short and never to
/// wait for anything, since a processor inside a section answers nobody.
///
/// An interrupt handler is a read-side section already, for the same reason,
/// which is what will let a handler be unregistered safely: take it out of the
/// table, [`synchronize`], and nothing is still running it.
pub(crate) fn read_section<T>(body: impl FnOnce() -> T) -> T {
    let saved = <arch::Irq as IrqControl>::disable();
    let result = body();
    <arch::Irq as IrqControl>::restore(saved);
    result
}

/// Wait for a grace period: return once every read-side section that was
/// running anywhere when this was called has ended.
///
/// What a writer calls between unpublishing an object and freeing it — and
/// what the scheduler's domain-mode switch will call between retiring one set
/// of scheduling classes and installing another.
///
/// Must not be called from inside a [`read_section`], which would wait for
/// itself; nor holding a lock another processor may be spinning on with
/// interrupts masked, for the reason [`flush_tlb_everywhere`] gives.
///
/// A processor that answers past the late bound is waited for and reported
/// once every processor has answered ([`Bounds`]).
pub(crate) fn synchronize() {
    report_late(
        synchronize_within(Bounds::product(GRACE_LATE_NANOS)),
        LEFT_SECTION,
    );
}

/// What a processor answering [`synchronize`] has done.
const LEFT_SECTION: &str = "left a read-side section for a grace period";

/// [`synchronize`] within `bounds`, returning the late answers it waited for
/// and reporting none of them.
pub(crate) fn synchronize_within(bounds: Bounds) -> Late {
    let Some(topology) = TOPOLOGY.get().filter(|_| this_cpu().is_some()) else {
        return Late::default();
    };

    let generation = GRACE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    // The processor running this is outside any section: that is this
    // function's contract, and why it may answer for itself. For itself as it
    // is at the moment of answering, not as it was on entry -- a processor this
    // task has left may be running somebody else's section by now.
    as_this_cpu(answer_grace);

    let mut late = Late::default();
    if topology.online() > 1 {
        let _ = arch::send_ipi_to_others();
        late = wait_for(
            topology,
            bounds,
            LEFT_SECTION,
            &crate::panic::catalog::GRACE_PERIOD_TIMEOUT,
            answer_grace,
            PerCpu::is_online,
            |cpu| cpu.gp_seen.load(Ordering::SeqCst) >= generation,
            || {
                let _ = arch::send_ipi_to_others();
            },
        );
    }
    let _ = GRACE_PERIODS.fetch_add(1, Ordering::Relaxed);
    late
}

/// What a processor waiting in [`synchronize`] answers for itself: any
/// shootdown, and every grace period so far, because the task running on it is
/// outside a read-side section and interrupts are masked around the answer.
fn answer_grace(me: &'static PerCpu) {
    service_tlb(me);
    answer_grace_periods(me);
}

/// Answer every grace period requested so far for `me`, the processor
/// running this, with interrupts masked: what the interrupt handler and a
/// waiting processor both do.
///
/// The generation is read first, and only then what a program leaving its
/// speculation domain asked of this processor (`arch::serve_wanted_barrier`)
/// or published for it to compare (`arch::answer_leaving`). The leave stores
/// both before it advances the generation, all sequentially consistent, so an
/// answer that counts for its grace period has seen them, and its barrier is
/// issued before the leave returns (the certification finding F-60).
fn answer_grace_periods(me: &PerCpu) {
    let generation = GRACE_GENERATION.load(Ordering::SeqCst);
    arch::serve_wanted_barrier();
    arch::answer_leaving();
    let _ = me.gp_seen.fetch_max(generation, Ordering::SeqCst);
}

/// How many grace periods have completed.
pub(crate) fn grace_periods() -> u64 {
    GRACE_PERIODS.load(Ordering::Relaxed)
}

/// The record of the processor this is running on, or `None` before the
/// boot processor has installed its own.
/// How many processors the machine has, or one before they have been counted.
pub(crate) fn count() -> usize {
    TOPOLOGY.get().map_or(1, Topology::count)
}

pub(crate) fn this_cpu() -> Option<&'static PerCpu> {
    if !LOCAL_READY.load(Ordering::Acquire) {
        return None;
    }
    // SAFETY: (SHARED) the boot processor installed its record before the flag above
    // was set, and a secondary installs its own before running anything that
    // could reach here.
    let at = unsafe { arch::cpu_local() };
    // SAFETY: (SHARED) every processor's register holds the address of its own record
    // in the slice `Topology::from_described` leaked, which lives forever and
    // is only ever reached through shared references.
    Some(unsafe { &*(at as *const PerCpu) })
}

/// Ask every other processor to stop, for a panic, and return how many were
/// asked.
///
/// None before the secondaries are started: until then there is nobody to
/// ask, and on x86-64 the local APIC the interrupt would be sent through may
/// not be mapped yet. A processor running with interrupts masked outside the
/// shootdown wait does not stop until it next looks; this is the best a
/// panic can do without a non-maskable interrupt.
pub(crate) fn stop_others() -> usize {
    STOPPING.store(true, Ordering::Release);
    let Some(topology) = TOPOLOGY.get() else {
        return 0;
    };
    let others = topology.online().saturating_sub(1);
    if others > 0 {
        let _ = arch::send_ipi_to_others();
    }
    others
}

/// Stop this processor if a panic has asked every processor to.
fn halt_if_stopping() {
    if STOPPING.load(Ordering::Acquire) {
        arch::halt()
    }
}

/// Which processor this is, for a failure report.
///
/// [`this_cpu`] follows the per-CPU register, which on a processor that has
/// not installed its record yet points wherever firmware left it — exactly the
/// processor most likely to be reporting a failure. This compares the
/// register's value against the address of every record instead, and believes
/// it only if it is one of them. The error says what can be said instead.
pub(crate) fn this_cpu_for_report() -> Result<&'static PerCpu, &'static str> {
    let Some(topology) = TOPOLOGY.get() else {
        return Err("the boot processor, before any other was started");
    };
    let register = arch::cpu_local_register();
    topology
        .cpus
        .iter()
        .find(|cpu| cpu.this == register)
        .ok_or("a processor that has not installed its per-CPU record")
}

/// Require that this processor's register leads back to `expected`, and that
/// the record names the processor that is actually reading it.
///
/// Two different mistakes, and each has a symptom that appears somewhere
/// else: a register pointing at another CPU's record makes two processors
/// share per-CPU state, and a record naming the wrong hardware sends every
/// interrupt meant for this processor to another one.
fn check_this_cpu(expected: &PerCpu) -> Result<(), &'static str> {
    let me = this_cpu().ok_or("the per-CPU register is not installed")?;
    if !core::ptr::eq(me, expected) {
        return Err("this processor's per-CPU register points at another record");
    }
    if me.this != (&raw const *me) as u64 {
        return Err("a per-CPU record does not hold its own address");
    }
    let indexed = TOPOLOGY
        .get()
        .and_then(|topology| topology.cpus.get(me.logical));
    if !indexed.is_some_and(|record| core::ptr::eq(record, me)) {
        return Err("a per-CPU record's logical number does not lead back to it");
    }
    if me.hardware_id != arch::hardware_id() {
        return Err("this processor's per-CPU record names a different processor");
    }
    Ok(())
}

/// Interrupt processor `cpu`, and no other where the architecture can
/// address one: what the scheduler's kick sends, having set its flag.
///
/// Every other processor is interrupted where it cannot -- `send_ipi_to`
/// refuses, or broadcasts behind the name as the Arm architectures do -- or
/// before the processors are known, which is what the kick always did.
/// Declared last in this file so that the lines above keep the numbers the
/// coverage arguments cite them by.
pub(crate) fn interrupt_one(cpu: usize) {
    let record = TOPOLOGY
        .get()
        .and_then(|topology| topology.cpus.get(cpu))
        .filter(|record| record.is_online());
    let sent = record.map(|record| arch::send_ipi_to(record.hardware_id));
    if !matches!(sent, Some(Ok(()))) {
        let _ = arch::send_ipi_to_others();
    }
}
