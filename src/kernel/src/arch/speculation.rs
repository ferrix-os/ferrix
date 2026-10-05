//! Side-channel defences: the part every architecture shares.
//!
//! `docs/certification/SPECULATION.md` is the argument; this is the
//! bookkeeping. Each architecture decides which defences its processors need
//! and offer (`<arch>/speculation.rs`) -- on the boot processor, or on
//! `AArch64` on each core for itself -- applies them there and on every
//! secondary as it starts, and reads back what it wrote. What is common is
//! kept here:
//!
//! * [`nospec_index`] and [`nospec_below`], the bounds checks a mispredicted
//!   branch cannot see past, built on each architecture's one-instruction
//!   clamp;
//! * the record of what each processor applied, which the boot check
//!   compares across processors;
//! * the decision a switch of address space rests on: whether the processor
//!   is about to run a different program's code than the one it last ran,
//!   which is when the branch predictors have to be emptied of it.
//!
//! # The switch
//!
//! [`HARDENED`] is false only in a kernel built with
//! `--cfg ferrix_mitigations_off` (`cargo xtask --mitigations off`), and then
//! every defence here and in the architectures is compiled out: the clamps are
//! the identity, nothing is written to the processor, no barrier is issued,
//! and the entry paths' extra instructions are assembled away. The reference
//! configuration is the other setting.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use ferrix_sched::MAX_CPUS;

use super::machine_speculation as machine;

/// Whether this kernel carries its side-channel defences: every build but one
/// made with `--mitigations off`.
pub(crate) const HARDENED: bool = !cfg!(ferrix_mitigations_off);

/// `Some(index)` when `index < len`, clamped so that no mispredicted path can
/// use it to reach past `len`; `None` otherwise.
///
/// For an index a program chose, before it indexes anything: a system call
/// number, a slot in a table. What makes it more than `index < len` is
/// that on the path a mispredicted comparison takes, the value it returns is
/// zero rather than whatever the program asked for (Spectre variant 1).
#[inline(always)]
pub(crate) fn nospec_index(index: usize, len: usize) -> Option<usize> {
    if index >= len {
        return None;
    }
    Some(machine::clamp_index(index, len))
}

/// [`nospec_index`] for a 64-bit value below `end`: a user address checked
/// against the top of the user half.
#[inline(always)]
pub(crate) fn nospec_below(value: u64, end: u64) -> Option<u64> {
    if value >= end {
        return None;
    }
    Some(machine::clamp_below(value, end))
}

/// A set of defences, as bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Defences(u32);

impl Defences {
    /// Nothing.
    pub(crate) const NONE: Defences = Defences(0);
    /// Program-chosen indices clamped after their bounds check: every
    /// architecture, always, in a hardened build.
    pub(crate) const CLAMPED_INDICES: Defences = Defences(1 << 0);
    /// x86-64: a fence after the conditional `swapgs` on interrupt entry.
    pub(crate) const SWAPGS_FENCE: Defences = Defences(1 << 1);
    /// x86-64: a program's registers cleared on entry, before any Rust runs.
    pub(crate) const ENTRY_REGISTERS_CLEARED: Defences = Defences(1 << 2);
    /// x86-64: Intel's enhanced IBRS, set once.
    pub(crate) const IBRS_ENHANCED: Defences = Defences(1 << 3);
    /// x86-64: AMD's automatic IBRS, set once in `EFER`.
    pub(crate) const IBRS_AUTOMATIC: Defences = Defences(1 << 4);
    /// x86-64: IBRS on a processor that says it may simply be left on.
    pub(crate) const IBRS_ALWAYS_ON: Defences = Defences(1 << 5);
    /// x86-64: single-thread indirect branch predictors.
    pub(crate) const STIBP: Defences = Defences(1 << 6);
    /// Speculative store bypass disabled: `SSBD` on x86-64, `PSTATE.SSBS`
    /// clear on `AArch64`, or firmware's workaround 2.
    pub(crate) const SSBD: Defences = Defences(1 << 7);
    /// The branch predictors emptied when the processor switches to another
    /// program's address space: `IBPB` on x86-64, firmware's workaround 1 on
    /// `AArch64`, `BPIALL` or `ICIALLU` on ARMv7-A.
    pub(crate) const SWITCH_BARRIER: Defences = Defences(1 << 8);
    /// x86-64: the return stack buffer refilled at the same switch.
    pub(crate) const RSB_FILL: Defences = Defences(1 << 9);
    /// x86-64: `VERW` clears the processor's buffers on every return to
    /// ring 3 (MDS).
    pub(crate) const BUFFERS_CLEARED: Defences = Defences(1 << 10);
    /// `AArch64`: the branch history overwritten on every entry from EL0.
    pub(crate) const BHB_LOOP: Defences = Defences(1 << 11);
    /// A defence was written and did not read back: the boot check fails.
    pub(crate) const READ_BACK_FAILED: Defences = Defences(1 << 30);

    /// Every defence, with the name a boot log gives it.
    const NAMES: [(Defences, &'static str); 12] = [
        (Defences::CLAMPED_INDICES, "clamped indices"),
        (Defences::SWAPGS_FENCE, "SWAPGS fence"),
        (Defences::ENTRY_REGISTERS_CLEARED, "entry registers cleared"),
        (Defences::IBRS_ENHANCED, "eIBRS"),
        (Defences::IBRS_AUTOMATIC, "AutoIBRS"),
        (Defences::IBRS_ALWAYS_ON, "IBRS always-on"),
        (Defences::STIBP, "STIBP"),
        (Defences::SSBD, "SSBD"),
        (Defences::SWITCH_BARRIER, "predictor barrier on switch"),
        (Defences::RSB_FILL, "RSB fill on switch"),
        (Defences::BUFFERS_CLEARED, "VERW on exit"),
        (Defences::BHB_LOOP, "BHB loop on entry"),
    ];

    /// Whether every defence in `other` is in this set.
    pub(crate) const fn contains(self, other: Defences) -> bool {
        self.0 & other.0 == other.0
    }

    /// This set with `other` added.
    #[must_use]
    pub(crate) const fn with(self, other: Defences) -> Defences {
        Defences(self.0 | other.0)
    }

    /// This set, written as the boot log writes it: the names, comma
    /// separated, or `none`.
    pub(crate) fn names(self) -> impl core::fmt::Display {
        Names(self)
    }

    /// The bits, for a record kept in an atomic.
    pub(crate) const fn bits(self) -> u32 {
        self.0
    }

    /// The set [`Defences::bits`] made.
    pub(crate) const fn from_bits(bits: u32) -> Defences {
        Defences(bits)
    }
}

/// [`Defences::names`].
struct Names(Defences);

impl core::fmt::Display for Names {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut first = true;
        for (defence, name) in Defences::NAMES {
            if self.0.contains(defence) {
                if !first {
                    f.write_str(", ")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        if first {
            f.write_str("none")?;
        }
        Ok(())
    }
}

/// Set in a processor's record once it has written one: a record of zero is
/// a processor that never got here.
const RECORDED: u32 = 1 << 31;

/// What each processor applied, by logical number, [`RECORDED`] included.
static APPLIED: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];

/// The user root each processor last ran a program in, by logical number:
/// zero before the first.
static LAST_ROOT: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// How many switch barriers each processor has issued, by logical number.
static SWITCH_BARRIERS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// The running processor's logical number, or zero for the boot processor
/// before it has a per-CPU record -- which it is, being processor zero.
pub(crate) fn this_cpu() -> usize {
    crate::smp::this_cpu().map_or(0, |cpu| cpu.logical)
}

/// Record what the running processor applied.
pub(crate) fn record_this_cpu(applied: Defences) {
    if let Some(slot) = APPLIED.get(this_cpu()) {
        slot.store(applied.bits() | RECORDED, Ordering::Release);
    }
}

/// What processor `logical` recorded, or `None` if it recorded nothing.
pub(crate) fn applied_by(logical: usize) -> Option<Defences> {
    let bits = APPLIED.get(logical)?.load(Ordering::Acquire);
    (bits & RECORDED != 0).then_some(Defences::from_bits(bits & !RECORDED))
}

/// How many switch barriers have been issued so far, on every processor
/// together.
pub(crate) fn switch_barriers() -> u64 {
    SWITCH_BARRIERS
        .iter()
        .map(|issued| issued.load(Ordering::Relaxed))
        .sum()
}

/// How many switch barriers processor `logical` has issued so far.
pub(crate) fn switch_barriers_on(logical: usize) -> u64 {
    SWITCH_BARRIERS
        .get(logical)
        .map_or(0, |issued| issued.load(Ordering::Relaxed))
}

/// Say what the machine is exposed to, once every processor has started and
/// recorded what it applied. On `AArch64`, where each core decides for itself,
/// a line per kind of core; elsewhere the boot processor decided for all of
/// them and said so as it did, and this adds nothing.
pub(crate) fn report_exposure() {
    machine::report_once_started();
}

/// Called by an architecture's `install_user_root` after it has written
/// `root`: issue the switch barrier if the processor last ran a program in a
/// different address space.
///
/// Keyed on the root table's address, per processor, so that a thread of the
/// same process coming back after the idle loop or a kernel thread costs
/// nothing -- which is most switches. The one way two programs can share a
/// root address is a root freed and reused, and [`forget_root`] closes that:
/// a new space's root is wiped from every processor's record before it is
/// ever installed.
pub(crate) fn entered_space(root: u64) {
    if !HARDENED {
        return;
    }
    let cpu = this_cpu();
    let Some(last) = LAST_ROOT.get(cpu) else {
        return;
    };
    // Taken, not read: a root installed without `entering_space` before it,
    // as a check may, is in no domain rather than the last one named here.
    //
    // Loads and stores, not read-modify-writes (OPAQUE-KERNEL.md §9.8, 2f):
    // this runs with interrupts masked (`AddressSpace::install`'s contract),
    // and only this processor writes its `ENTERING_DOMAIN`. `LAST_DOMAIN` and
    // `LAST_ROOT` are also written by `forget_root` from another processor,
    // which clears a root no space uses yet and the domain with it: a store
    // here that overwrites that clear stores this processor's own root and
    // domain, never the forgotten root, which is all `forget_root` asks.
    // And `LAST_DOMAIN`'s remote reader, `leaving_domain`'s scan, is since
    // F-60's fix only a shortcut: every processor checks its own at the
    // grace period's answer (`answer_leaving`), after any switch it was
    // making. So no ordering rests on these accesses beyond their own
    // processor's program order. The table in §9.8 lists every remote
    // reader of a word the switch writes.
    let incoming = ENTERING_DOMAIN.get(cpu).map_or(0, |domain| {
        let incoming = domain.load(Ordering::Relaxed);
        domain.store(0, Ordering::Relaxed);
        incoming
    });
    let outgoing = LAST_DOMAIN.get(cpu).map_or(0, |domain| {
        let outgoing = domain.load(Ordering::Relaxed);
        domain.store(incoming, Ordering::Relaxed);
        outgoing
    });
    let previous = last.load(Ordering::Relaxed);
    last.store(root, Ordering::Relaxed);
    if previous == root {
        return;
    }
    // Inside one speculation domain the predictor invalidation is left out,
    // and the rest of the barrier, x86-64's return-stack refill, stays
    // (`docs/OPAQUE-KERNEL.md` §9.3a, A2). Not counted as a barrier: the
    // count is of invalidations, which the domain check reads; the refill is
    // counted apart.
    if same_domain(outgoing, incoming) {
        if machine::switch_barrier_in_domain(cpu)
            && let Some(refilled) = REFILLS_IN_DOMAIN.get(cpu)
        {
            count_here(refilled);
        }
        return;
    }
    issue_barrier(cpu);
}

/// Issue the switch barrier on processor `cpu`, which is this one, and count
/// it: the decision, and the invalidation if the processor has one.
fn issue_barrier(cpu: usize) {
    if let Some(decided) = BARRIER_DECISIONS.get(cpu) {
        count_here(decided);
    }
    if machine::switch_barrier(cpu)
        && let Some(issued) = SWITCH_BARRIERS.get(cpu)
    {
        count_here(issued);
    }
}

/// Add one to `counter`, a word of this processor's that only it writes,
/// with interrupts masked (every caller: the switch, the grace period's
/// answer, the barrier interrupt): a load and a store, not a locked add
/// (2f). Another processor reads it whole, as a report or the domain check
/// does after the switches it counts.
fn count_here(counter: &AtomicU64) {
    counter.store(
        counter.load(Ordering::Relaxed).wrapping_add(1),
        Ordering::Relaxed,
    );
}

/// How many times each processor refilled its return stack at a switch
/// inside a speculation domain, where the invalidation is left out: what the
/// check reads to see that the rest of the barrier still ran there.
static REFILLS_IN_DOMAIN: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// How many in-domain refills processor `logical` has made: see
/// [`REFILLS_IN_DOMAIN`].
pub(crate) fn refills_in_domain_on(logical: usize) -> u64 {
    REFILLS_IN_DOMAIN
        .get(logical)
        .map_or(0, |refilled| refilled.load(Ordering::Relaxed))
}

/// Whether a switch inside a speculation domain still refills the return
/// stack on this architecture, for the check: x86-64's does.
pub(crate) const REFILL_IN_DOMAIN: bool = machine::REFILL_IN_DOMAIN;

/// Processors asked to issue the barrier at once, by [`leaving_domain`].
static BARRIER_WANTED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// How many leaves of a domain may be under way at once before another
/// waits for one of them to finish.
const LEAVING_SLOTS: usize = 8;

/// The domains being left now, each by a [`leaving_domain`] that has not yet
/// returned: zero in a slot no leave holds. What each processor compares its
/// own [`LAST_DOMAIN`] with as it answers a grace period
/// ([`answer_leaving`]), the certification finding F-60.
///
/// A set rather than one word, so that two leaves of different domains at
/// once each keep theirs.
static LEAVING: [AtomicU64; LEAVING_SLOTS] = [const { AtomicU64::new(0) }; LEAVING_SLOTS];

/// A program has left speculation domain `domain` (`docs/OPAQUE-KERNEL.md`
/// §9.3a, A1, and the consultant's F1): every processor whose last space was
/// in it issues the barrier before this returns, this one at once and the
/// others as they answer the grace period this waits for.
///
/// Waiting for the next switch is not enough. A program that rises in
/// privilege -- a set-id `execve` in place, a change of credentials -- goes
/// on running in the same space, on a processor whose predictors its
/// domain's other members trained a moment ago, and its other threads go on
/// running on theirs. The space is already out of every domain when this
/// runs, so a processor that installs it from now on issues the barrier at
/// that switch; the ones that had it, or a member's, are found here.
///
/// They are found two ways. The scan below asks each processor whose
/// [`LAST_DOMAIN`] is `domain` now ([`serve_wanted_barrier`]). But nothing
/// orders the space's store of `OUT` before the scan's loads, so a processor
/// that read the space's domain before that store may record `domain` after
/// the scan has passed it -- the store-buffer pattern, open on x86-64 and
/// ARMv7-A (the certification finding F-60). So the domain is also published
/// in [`LEAVING`] before the scan and before the grace period's generation
/// is advanced, both sequentially consistent, and every processor answering
/// the grace period reads the generation first, then [`LEAVING`], and issues
/// the barrier itself if its own [`LAST_DOMAIN`] is there
/// ([`answer_leaving`]). It answers with interrupts masked, so after any
/// switch it was making, and having read a generation the publish came
/// before, it cannot miss the domain; and once it has seen the publish, it
/// reads the space's domain as `OUT` from then on.
///
/// Waits for the grace period, so the caller may block: it has checked so
/// (FX-0907).
pub(crate) fn leaving_domain(domain: u64) {
    if !HARDENED || domain == 0 {
        return;
    }
    let slot = publish_leaving(domain);
    let online = crate::smp::count().max(1);
    for (wanted, last) in BARRIER_WANTED.iter().zip(LAST_DOMAIN.iter()).take(online) {
        if last.load(Ordering::SeqCst) == domain {
            wanted.store(true, Ordering::SeqCst);
        }
    }
    // Between the scan and the grace period: where stage 9 makes another
    // processor record the domain the scan has just passed (case 11).
    // Copied out, so that the lock is not held while it runs.
    let hook = *LEAVE_HOOK.lock();
    if let Some(hook) = hook {
        (hook.run)();
    }
    let saved = <super::Irq as ferrix_sync::IrqControl>::disable();
    serve_wanted_barrier();
    <super::Irq as ferrix_sync::IrqControl>::restore(saved);
    crate::smp::synchronize();
    if let Some(published) = LEAVING.get(slot) {
        published.store(0, Ordering::SeqCst);
    }
}

/// Put `domain` in a free slot of [`LEAVING`] and answer the slot. When every
/// slot is held, each by a leave that gives it back after one grace period,
/// yield until one is: the caller may block.
fn publish_leaving(domain: u64) -> usize {
    loop {
        for (slot, leaving) in LEAVING.iter().enumerate() {
            if leaving
                .compare_exchange(0, domain, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok()
            {
                return slot;
            }
        }
        crate::sched::yield_now();
    }
}

/// The local half of [`leaving_domain`] (F-60): if the domain this
/// processor last ran is one being left, issue the barrier and forget it.
///
/// Called as this processor answers a grace period, with interrupts masked,
/// and after it has read the generation it answers, so that every domain
/// published before that generation was advanced is seen here.
pub(crate) fn answer_leaving() {
    if !HARDENED {
        return;
    }
    let cpu = this_cpu();
    let Some(last) = LAST_DOMAIN.get(cpu) else {
        return;
    };
    let mine = last.load(Ordering::SeqCst);
    if mine != 0
        && LEAVING
            .iter()
            .any(|leaving| leaving.load(Ordering::SeqCst) == mine)
    {
        last.store(0, Ordering::SeqCst);
        issue_barrier(cpu);
    }
}

/// Issue the barrier [`leaving_domain`] asked of this processor, if it did,
/// and forget the domain it last ran. With interrupts masked: from the
/// interrupt handler, or from `leaving_domain` itself.
pub(crate) fn serve_wanted_barrier() {
    let cpu = this_cpu();
    if BARRIER_WANTED
        .get(cpu)
        .is_some_and(|wanted| wanted.swap(false, Ordering::SeqCst))
    {
        if let Some(last) = LAST_DOMAIN.get(cpu) {
            last.store(0, Ordering::SeqCst);
        }
        issue_barrier(cpu);
    }
}

/// Code a check runs inside the item, at a point no program can aim at:
/// what `docs/OPAQUE-KERNEL.md` §9.7's rules for such a hook ask. Only stage
/// 9 arms one, it disarms it before init starts, and a boot check after
/// stage 9 stops the machine (FX-0908) naming the check that armed one still
/// set.
#[derive(Debug)]
pub(crate) struct CheckHook {
    /// The check that armed it, for the boot check's message.
    pub(crate) armed_by: &'static str,
    /// What it does where it is called.
    pub(crate) run: fn(),
}

/// The hook [`leaving_domain`] calls between its scan and its grace period,
/// if a check has armed one. Read under a lock rather than by one load: a
/// leave is rare, and waits for a grace period anyway.
static LEAVE_HOOK: crate::sync::SpinLock<Option<&'static CheckHook>> =
    crate::sync::SpinLock::new(None);

/// Arm `hook` in [`leaving_domain`]: stage 9's check only.
pub(crate) fn arm_leave_hook(hook: &'static CheckHook) {
    *LEAVE_HOOK.lock() = Some(hook);
}

/// Disarm whatever hook [`leaving_domain`] holds.
pub(crate) fn disarm_leave_hook() {
    *LEAVE_HOOK.lock() = None;
}

/// The check that armed the hook [`leaving_domain`] still holds, if any: for
/// the boot check after stage 9.
pub(crate) fn leave_hook_armed_by() -> Option<&'static str> {
    LEAVE_HOOK.lock().map(|hook| hook.armed_by)
}

/// The domain processor `logical` last ran, for stage 9's check: zero for
/// none.
pub(crate) fn last_domain_on(logical: usize) -> u64 {
    LAST_DOMAIN
        .get(logical)
        .map_or(0, |domain| domain.load(Ordering::SeqCst))
}

/// Whether a switch from a space of domain `outgoing` to one of `incoming`
/// stays inside one speculation domain: both the same, and not zero, which
/// is no domain.
fn same_domain(outgoing: u64, incoming: u64) -> bool {
    outgoing != 0 && outgoing == incoming
}

/// How many times each processor decided a switch needed the predictor
/// barrier, whether or not it had one to issue: what the speculation domain
/// check counts, since a processor the reference configuration runs without
/// a barrier -- QEMU's Cortex-A72, with no `ARCH_WORKAROUND_1` -- issues
/// none, and [`SWITCH_BARRIERS`] would then say nothing about the rule.
static BARRIER_DECISIONS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// How many switches processor `logical` has decided needed the predictor
/// barrier: see [`BARRIER_DECISIONS`].
pub(crate) fn barrier_decisions_on(logical: usize) -> u64 {
    BARRIER_DECISIONS
        .get(logical)
        .map_or(0, |decided| decided.load(Ordering::Relaxed))
}

/// The speculation domain of the space each processor last ran, read as
/// that space left it (`left_space`) and set by each install
/// (`entered_space`): zero for none.
static LAST_DOMAIN: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// The speculation domain of the space each processor is installing, from
/// `entering_space` to `entered_space`.
static ENTERING_DOMAIN: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// The space this processor ran is leaving it, in speculation domain
/// `domain` as it stands now: to a kernel thread, or to the space about to be
/// installed. Read now and not when it came, so that a program that left its
/// domain while it ran is out of it at the switch that ends its turn.
pub(crate) fn left_space(domain: u64) {
    if let Some(last) = LAST_DOMAIN.get(this_cpu()) {
        last.store(domain, Ordering::Relaxed);
    }
}

/// The space about to be installed on this processor is in speculation
/// domain `domain`, for the `entered_space` its install makes.
pub(crate) fn entering_space(domain: u64) {
    if let Some(entering) = ENTERING_DOMAIN.get(this_cpu()) {
        entering.store(domain, Ordering::Relaxed);
    }
}

/// Called by an architecture's `prepare_user_root` for a root that is about to
/// belong to a new address space: no processor may treat it as the space it
/// last ran.
pub(crate) fn forget_root(root: u64) {
    if !HARDENED {
        return;
    }
    for (last, domain) in LAST_ROOT
        .iter()
        .zip(LAST_DOMAIN.iter())
        .take(crate::smp::count())
    {
        // And its domain with it, so that a reused root never inherits one.
        if last
            .compare_exchange(root, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            domain.store(0, Ordering::Relaxed);
        }
    }
}
