//! The audit record: what the TSF decided, kept where only it writes
//! (finding F-21b, `docs/certification/AUDIT.md`).
//!
//! # What is kept
//!
//! One fixed 64-byte [`Record`] per event the kernel decided: a capability
//! refused, authority handed over, a process ended from outside, a device
//! quiesced, TSF data changed, and the boot's own configuration. Each is
//! recorded at the one place that decision is made, so recording it adds a
//! call and changes nothing about the decision. The layout is
//! `src/lib/proto/audit`'s, which a reader shares.
//!
//! # Where
//!
//! In two rings of static storage, so that nothing on a recording path
//! allocates (finding F-23) and a record can be taken before the heap is up:
//!
//! * the **high-value ring**, [`HIGH_RECORDS`] long, for every class but
//!   refusals: they are rare, and a flood of refusals can never evict one;
//! * the **refusal ring**, [`REFUSAL_RECORDS`] long, with fairness: past
//!   [`PER_BUDGET_PER_SECOND`] refusals charged to one budget in a second,
//!   that budget's further refusals are counted, not kept, and one
//!   *suppressed n* record says how many once its second has ended.
//!
//! # Whose budget
//!
//! A refusal is charged to its job's audit budget, [`Subject::budget`],
//! which each job carries from when it was made (`Job::audit_budget`): the
//! job at or above the refusing one that its members' own authority could
//! not have made. Its maker decides, since whether a directory is writable
//! by someone other than root is cgroupfs's to know: an anonymous job takes
//! its parent's budget, and so does a cgroup `mkdir` made in a directory
//! someone other than root may write. A
//! program that makes sub-jobs to refuse from -- anonymous ones, or named
//! ones in a cgroup delegated to it -- charges them all to the one budget,
//! so it writes at most [`PER_BUDGET_PER_SECOND`] refusals a second however
//! many jobs it makes, and cannot push another unit's refusals out faster
//! than that.
//!
//! # Numbering
//!
//! Each ring numbers its records from zero, per boot, without gaps, so a
//! reader who finds a number missing knows a record was lost, and is told
//! how many. A full ring overwrites its oldest record: refusing new ones
//! would let an attacker fill it first and then act unrecorded, and
//! stopping the TSF would make audit a lever for denial of service
//! (`AUDIT.md` §3).
//!
//! Each ring's lock is an [`IrqSpinLock`]: an IOMMU fault is recorded from
//! its interrupt handler, and an OOM kill can be decided with the
//! allocator's locks held, so a lock that left interrupts on could be taken
//! by a handler on the processor already holding it.
//!
//! **They are leaf locks.** Nothing is taken under them and nothing
//! allocates under them: a 64-byte copy and two counters. A limit's refusal
//! is recorded from inside the heap's charge path with other locks held, so
//! everything a record needs -- its subject among it -- is worked out
//! before a ring's lock is taken, and without a lock of its own on that
//! path ([`Subject::of`] is for callers that hold none).
//!
//! # The boot's own records
//!
//! The first [`BOOT_RECORDS`] system records -- the start-up record and the
//! boot's configuration -- are also pinned where no other record can reach
//! them ([`Which::Boot`]): the boot's own checks make processes, kill jobs
//! and set limits by the hundred, and could otherwise wrap the high-value
//! ring past them before any reader exists.
//!
//! # The boot's audit id
//!
//! A random 128-bit number drawn at start-up ([`start`]) and carried in the
//! start-up record and in every read, so that the records of two boots can
//! never be spliced into one sequence. The kernel's random generator is the
//! item's, above this module, so bring-up draws it and hands it in.

pub(crate) mod check;

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub(crate) use ferrix_audit::{
    BOOTED, CGROUP_KILLED, CGROUP_LIMIT, CONFIG, CONTROL, Class, Config, DELEGATED, DEVICE_LIMIT,
    DEVICE_LIMIT_SET, DEVMGR_STARTED, DMA_FAULT, DOMAIN, Event, INTERRUPT_FAULT, JOB_KILLED, LIMIT,
    LIMIT_SET, NO_UID, OOM_KILLED, Outcome, POWER, PROCESS_MADE, QUIESCED, READER_GIVEN, RIGHTS,
    ROOT_SWITCHED, Record, STARTER_GIVEN, SUPPRESSED, WIDEN, saturated, target,
};
use ferrix_sync::IrqSpinLock;

use crate::object::process::Process;
use crate::object::quota::Resource;

/// The high-value ring's length: 32 KiB of records.
pub(crate) const HIGH_RECORDS: usize = 512;

/// The refusal ring's length: 256 KiB of records.
pub(crate) const REFUSAL_RECORDS: usize = 4096;

/// How many refusals charged to one budget a second are kept before the
/// rest are folded into a *suppressed n* record.
pub(crate) const PER_BUDGET_PER_SECOND: u32 = 64;

/// How many budgets' fairness windows are tracked at once. A budget that
/// finds every slot taken takes the one whose window began first, whose
/// count is written out before it goes, so eviction never loses a count.
const FAIR_SLOTS: usize = 32;

/// A fairness window's length, in nanoseconds.
const SECOND: u64 = 1_000_000_000;

/// How many of the first system records are pinned ([`Which::Boot`]).
pub(crate) const BOOT_RECORDS: usize = 9;

/// Who a record is about: the process and its job, which the TSF attests,
/// and beside them the uid the personality says, which it does not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Subject {
    /// The process, 0 for the kernel itself.
    pub(crate) pid: u32,
    /// The uid the personality gave, or [`NO_UID`]: personality-supplied
    /// data, never the TSF's identity of the subject (`AUDIT.md` §2).
    pub(crate) uid: u32,
    /// The process's job, 0 for the kernel itself.
    pub(crate) job: u64,
    /// The job its refusals are charged to ([`budget_of`]). Not recorded:
    /// it keys the refusal ring's fairness.
    pub(crate) budget: u64,
}

impl Subject {
    /// The kernel, deciding for itself.
    pub(crate) const KERNEL: Subject = Subject {
        pid: 0,
        uid: NO_UID,
        job: 0,
        budget: 0,
    };

    /// `process`, as the TSF attests it: its pid, its job and that job's
    /// budget. Reads the process's job under its membership lock, so it is
    /// for callers that hold no lock; a record made with locks held works
    /// its subject out without one.
    pub(crate) fn of(process: &Process) -> Subject {
        let job = process.job();
        Subject {
            pid: process.pid(),
            uid: NO_UID,
            job: job.id(),
            budget: job.audit_budget(),
        }
    }
}

/// What a record's decision was about: an object's kind and identity, a
/// device's location, a resource. Zero where nothing is named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Target {
    /// What kind of thing, numbered by the event.
    pub(crate) kind: u32,
    /// Which one.
    pub(crate) id: u64,
}

impl Target {
    /// Nothing in particular.
    pub(crate) const NONE: Target = Target { kind: 0, id: 0 };
}

/// The record of `event`, numbered later by the ring that keeps it.
fn record_of(
    time: u64,
    event: Event,
    outcome: Outcome,
    status: i16,
    subject: Subject,
    target: Target,
    detail: [u32; 3],
) -> Record {
    let mut record = Record {
        time,
        class: event.class as u16,
        code: event.code,
        outcome: outcome as u16,
        status,
        pid: subject.pid,
        uid: subject.uid,
        job: subject.job,
        target_kind: target.kind,
        detail,
        ..Record::EMPTY
    };
    record.set_target(target.id);
    record
}

/// Which of the two rings, or the pinned boot records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Which {
    /// Everything but refusals.
    High,
    /// Refusals, and the *suppressed n* records that count those not kept.
    Refusals,
    /// The first [`BOOT_RECORDS`] system records, numbered as the
    /// high-value ring numbered them, which never wraps: a record past the
    /// first nine is kept in the high-value ring alone.
    Boot,
}

/// What one read took out of a ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Read {
    /// Records copied to the front of the reader's buffer.
    pub(crate) copied: usize,
    /// The sequence number to read from next.
    pub(crate) next: u64,
    /// Records between the number asked for and the first one copied that
    /// the ring no longer held.
    pub(crate) lost: u64,
    /// The boot's audit id.
    pub(crate) id: u128,
}

/// A ring of `N` records, numbered from zero.
struct Ring<const N: usize> {
    /// The record numbered `n`, in slot `n % N` until `n + N` replaces it.
    records: [Record; N],
    /// How many records have ever been kept: the next one's number.
    next: u64,
}

impl<const N: usize> Ring<N> {
    /// Its length, as a count.
    const LENGTH: u64 = {
        assert!(N > 0, "an audit ring holds at least one record");
        N as u64
    };

    /// An empty ring.
    const fn new() -> Self {
        Ring {
            records: [Record::EMPTY; N],
            next: 0,
        }
    }

    /// Keep `record` as the next number, over the oldest when full, and
    /// answer the number it was given.
    fn put(&mut self, mut record: Record) -> u64 {
        let number = self.next;
        record.sequence = number;
        let slot = (number % Self::LENGTH) as usize;
        if let Some(kept) = self.records.get_mut(slot) {
            *kept = record;
        }
        self.next = number.wrapping_add(1);
        number
    }

    /// Copy records from number `from` on into `out`, as many as fit and
    /// are there; one overwritten before the reader came for it is counted
    /// as lost and skipped. Answers how many were copied, the number to
    /// read from next, and how many were lost.
    fn read(&self, from: u64, out: &mut [Record]) -> (usize, u64, u64) {
        let oldest = self.next.saturating_sub(Self::LENGTH);
        let start = from.max(oldest);
        let lost = start.saturating_sub(from);
        let mut at = start;
        let mut copied = 0;
        for slot in out.iter_mut() {
            if at >= self.next {
                break;
            }
            if let Some(record) = self.records.get((at % Self::LENGTH) as usize) {
                *slot = *record;
            }
            at += 1;
            copied += 1;
        }
        (copied, at, lost)
    }
}

/// The high-value ring and the boot records pinned beside it, under one
/// lock.
struct High<const N: usize> {
    ring: Ring<N>,
    boot: [Record; BOOT_RECORDS],
    pinned: usize,
}

impl<const N: usize> High<N> {
    /// Nothing kept.
    const fn new() -> Self {
        High {
            ring: Ring::new(),
            boot: [Record::EMPTY; BOOT_RECORDS],
            pinned: 0,
        }
    }

    /// Keep `record` in the ring, and a system record among the first
    /// [`BOOT_RECORDS`] pinned as well, with the number the ring gave it.
    fn put(&mut self, record: Record) -> u64 {
        let number = self.ring.put(record);
        if record.class == Class::System as u16
            && let Some(slot) = self.boot.get_mut(self.pinned)
        {
            *slot = Record {
                sequence: number,
                ..record
            };
            self.pinned += 1;
        }
        number
    }

    /// Copy the pinned records from number `from` on into `out`: those the
    /// ring numbered `from` or later, in order. Nothing is lost from them.
    fn read_boot(&self, from: u64, out: &mut [Record]) -> (usize, u64, u64) {
        let mut copied = 0;
        let mut next = from;
        let pinned = self.boot.iter().take(self.pinned);
        for (slot, record) in out
            .iter_mut()
            .zip(pinned.filter(|record| record.sequence >= from))
        {
            *slot = *record;
            next = record.sequence + 1;
            copied += 1;
        }
        (copied, next, 0)
    }
}

/// One budget's fairness window: how many refusals it has had kept in the
/// second that began at `began`, and how many it has had counted instead.
#[derive(Clone, Copy)]
struct Window {
    /// Whether the slot is in use.
    used: bool,
    /// The budget, a job's id.
    budget: u64,
    /// When the window began.
    began: u64,
    /// Refusals kept in it.
    kept: u32,
    /// Refusals past the limit, counted and not kept.
    suppressed: u32,
}

impl Window {
    /// A slot in no use.
    const FREE: Window = Window {
        used: false,
        budget: 0,
        began: 0,
        kept: 0,
        suppressed: 0,
    };
}

/// The refusal ring and the fairness windows it is kept by, under one lock.
struct Refusals<const N: usize> {
    ring: Ring<N>,
    windows: [Window; FAIR_SLOTS],
}

impl<const N: usize> Refusals<N> {
    /// None kept, no window open.
    const fn new() -> Self {
        Refusals {
            ring: Ring::new(),
            windows: [Window::FREE; FAIR_SLOTS],
        }
    }

    /// Keep `record`, a refusal charged to `budget`, if the budget's window
    /// has room, and answer its number; count it otherwise.
    fn keep(&mut self, now: u64, budget: u64, record: Record) -> Option<u64> {
        let at = self.window_of(now, budget);
        let window = self.windows.get_mut(at)?;
        if window.kept < PER_BUDGET_PER_SECOND {
            window.kept += 1;
            Some(self.ring.put(record))
        } else {
            window.suppressed = window.suppressed.saturating_add(1);
            None
        }
    }

    /// The slot of `budget`'s current window: its own, opened afresh if its
    /// second has ended, or a free one, or the one begun first -- each
    /// closed first, so that what it counted is written out.
    fn window_of(&mut self, now: u64, budget: u64) -> usize {
        let own = self
            .windows
            .iter()
            .position(|window| window.used && window.budget == budget);
        let at = own.unwrap_or_else(|| {
            self.windows
                .iter()
                .position(|window| !window.used)
                .unwrap_or_else(|| self.first_begun())
        });
        let fresh = self.windows.get(at).is_none_or(|window| {
            !window.used || window.budget != budget || now.saturating_sub(window.began) >= SECOND
        });
        if fresh {
            self.close(now, at);
            if let Some(window) = self.windows.get_mut(at) {
                *window = Window {
                    used: true,
                    budget,
                    began: now,
                    kept: 0,
                    suppressed: 0,
                };
            }
        }
        at
    }

    /// The slot whose window began first.
    fn first_begun(&self) -> usize {
        let mut first = 0;
        for (at, window) in self.windows.iter().enumerate() {
            if self
                .windows
                .get(first)
                .is_some_and(|f| window.began < f.began)
            {
                first = at;
            }
        }
        first
    }

    /// Close the window in slot `at`: write its *suppressed n* record if it
    /// counted any, and free the slot.
    fn close(&mut self, now: u64, at: usize) {
        let Some(window) = self.windows.get_mut(at) else {
            return;
        };
        let closed = *window;
        *window = Window::FREE;
        if closed.used && closed.suppressed > 0 {
            let subject = Subject {
                pid: 0,
                uid: NO_UID,
                job: closed.budget,
                budget: closed.budget,
            };
            let _ = self.ring.put(record_of(
                now,
                SUPPRESSED,
                Outcome::Refused,
                0,
                subject,
                Target::NONE,
                [closed.suppressed, PER_BUDGET_PER_SECOND, 0],
            ));
        }
    }

    /// Close every window whose second has ended, so a reader sees its
    /// count without waiting for the budget's next refusal.
    fn close_ended(&mut self, now: u64) {
        for at in 0..FAIR_SLOTS {
            let ended = self
                .windows
                .get(at)
                .is_some_and(|window| window.used && now.saturating_sub(window.began) >= SECOND);
            if ended {
                self.close(now, at);
            }
        }
    }
}

/// An audit store: the two rings and the boot's id. The kernel's is
/// [`STORE`]; the boot check drives small ones of its own.
pub(crate) struct Store<const H: usize, const R: usize> {
    high: IrqSpinLock<High<H>, crate::arch::Irq>,
    refusals: IrqSpinLock<Refusals<R>, crate::arch::Irq>,
    /// The audit id's two halves, set once by [`Store::start`].
    id: [AtomicU64; 2],
    started: AtomicBool,
}

impl<const H: usize, const R: usize> Store<H, R> {
    /// The start-up record, before it is numbered and stamped: its lengths
    /// checked when the store's type is made, so the record can carry them.
    const START_FIELDS: () = assert!(
        H <= ferrix_audit::MAX_RING && R <= ferrix_audit::MAX_RING,
        "each ring's length fits the half word the start-up record gives it"
    );

    /// Empty, and not started.
    pub(crate) const fn new() -> Self {
        let () = Self::START_FIELDS;
        Store {
            high: IrqSpinLock::new(High::new()),
            refusals: IrqSpinLock::new(Refusals::new()),
            id: [AtomicU64::new(0), AtomicU64::new(0)],
            started: AtomicBool::new(false),
        }
    }

    /// Take `id` as the boot's audit id and write the start-up record, the
    /// first of the high-value ring when nothing was recorded before it: it
    /// carries the whole id and both ring lengths (`Record::start`). Only
    /// the first call counts.
    pub(crate) fn start(&self, now: u64, id: u128) -> bool {
        if self.started.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.id[0].store(id as u64, Ordering::Release);
        self.id[1].store((id >> 64) as u64, Ordering::Release);
        if let Some(mut record) = Record::start(id, H, R) {
            record.time = now;
            let _ = self.high.lock().put(record);
        }
        true
    }

    /// The boot's audit id, 0 before [`Store::start`].
    pub(crate) fn id(&self) -> u128 {
        u128::from(self.id[0].load(Ordering::Acquire))
            | (u128::from(self.id[1].load(Ordering::Acquire)) << 64)
    }

    /// Record `event` at `now`.
    #[expect(
        clippy::too_many_arguments,
        reason = "the record's parts and its time, which the check sets and `record` reads"
    )]
    pub(crate) fn record_at(
        &self,
        now: u64,
        event: Event,
        outcome: Outcome,
        status: i16,
        subject: Subject,
        target: Target,
        detail: [u32; 3],
    ) -> Option<u64> {
        let record = record_of(now, event, outcome, status, subject, target, detail);
        if event.class == Class::Refused {
            self.refusals.lock().keep(now, subject.budget, record)
        } else {
            Some(self.high.lock().put(record))
        }
    }

    /// Copy records of ring `which`, from number `from` on, into `out`.
    /// Windows of the refusal ring whose second has ended are closed first,
    /// so their counts are there to read.
    pub(crate) fn read_at(&self, now: u64, which: Which, from: u64, out: &mut [Record]) -> Read {
        let (copied, next, lost) = match which {
            Which::High => self.high.lock().ring.read(from, out),
            Which::Boot => self.high.lock().read_boot(from, out),
            Which::Refusals => {
                let mut refusals = self.refusals.lock();
                refusals.close_ended(now);
                refusals.ring.read(from, out)
            }
        };
        Read {
            copied,
            next,
            lost,
            id: self.id(),
        }
    }
}

/// The kernel's audit store: 288 KiB of static storage on every
/// architecture.
pub(crate) static STORE: Store<HIGH_RECORDS, REFUSAL_RECORDS> = Store::new();

/// Start the kernel's audit function with the boot's audit `id`, drawn by
/// bring-up from the random generator. False if it had already started.
pub(crate) fn start(id: u128) -> bool {
    STORE.start(crate::timer::now_nanos(), id)
}

/// Record `event` in the kernel's store, now.
pub(crate) fn record(
    event: Event,
    outcome: Outcome,
    status: i16,
    subject: Subject,
    target: Target,
    detail: [u32; 3],
) {
    let _ = record_numbered(event, outcome, status, subject, target, detail);
}

/// [`record`], answering the number its ring gave the record, or `None` for
/// a refusal the fairness counted instead of keeping.
pub(crate) fn record_numbered(
    event: Event,
    outcome: Outcome,
    status: i16,
    subject: Subject,
    target: Target,
    detail: [u32; 3],
) -> Option<u64> {
    STORE.record_at(
        crate::timer::now_nanos(),
        event,
        outcome,
        status,
        subject,
        target,
        detail,
    )
}

/// Record one item of the boot's configuration.
pub(crate) fn config(key: Config, value: u32, more: u32) {
    record(
        CONFIG,
        Outcome::Done,
        0,
        Subject::KERNEL,
        Target::NONE,
        [key as u32, value, more],
    );
}

/// A resource's number in a record (`ferrix_audit::resource`).
pub(crate) fn resource_number(resource: Resource) -> u64 {
    match resource {
        Resource::Memory => ferrix_audit::resource::MEMORY,
        Resource::Objects => ferrix_audit::resource::OBJECTS,
        Resource::Tasks => ferrix_audit::resource::TASKS,
        Resource::Kernel => ferrix_audit::resource::KERNEL,
    }
}

/// Record a charge to `job`, whose budget is `budget`, that the limit at
/// quota slot `at` refused: from `object::quota::charge`, which can run
/// inside the heap, so the subject is the job as its slot names it, and no
/// process is looked up.
pub(crate) fn limit_refused(
    job: u64,
    budget: u64,
    resource: Resource,
    amount: u64,
    limit: u64,
    at: u32,
) {
    let subject = Subject {
        pid: 0,
        uid: NO_UID,
        job,
        budget,
    };
    let target = Target {
        kind: target::RESOURCE,
        id: resource_number(resource),
    };
    record(
        LIMIT,
        Outcome::Refused,
        0,
        subject,
        target,
        [saturated(amount), saturated(limit), at],
    );
}

/// Record `subject` setting `job`'s limit on resource number `resource` to
/// `limit`, as `event` says: through its handle ([`LIMIT_SET`]) or its
/// cgroup's file ([`CGROUP_LIMIT`]).
pub(crate) fn limit_set(event: Event, subject: Subject, job: u64, resource: u64, limit: u64) {
    let target = Target {
        kind: target::RESOURCE,
        id: resource,
    };
    record(
        event,
        Outcome::Done,
        0,
        subject,
        target,
        [job as u32, (job >> 32) as u32, saturated(limit)],
    );
}

/// Record `subject` setting limit number `which` of the device at `device`
/// in the kernel's list from `old` to `new` (`device_set_limit`), as
/// `answered` says: [`DEVICE_LIMIT_SET`] when it was set, [`DEVICE_LIMIT`]
/// with the status when it was refused.
pub(crate) fn device_limit(
    subject: Subject,
    device: usize,
    which: u64,
    [old, new]: [u64; 2],
    answered: &Result<usize, ferrix_linux_abi::errno::Errno>,
) {
    let (event, outcome, status) = match answered {
        Ok(_) => (DEVICE_LIMIT_SET, Outcome::Done, 0),
        Err(refused) => (
            DEVICE_LIMIT,
            Outcome::Refused,
            i16::try_from(refused.0).unwrap_or(i16::MAX),
        ),
    };
    let target = Target {
        kind: target::DEVICE,
        id: device as u64,
    };
    record(
        event,
        outcome,
        status,
        subject,
        target,
        [saturated(which), saturated(old), saturated(new)],
    );
}

/// Record `subject` ending what `target` names, as `event` says.
pub(crate) fn ended(event: Event, subject: Subject, target: Target, detail: [u32; 3]) {
    record(event, Outcome::Done, 0, subject, target, detail);
}

/// Record the scoped OOM kill of `pid`, for the job `limited` whose memory
/// limit asked, with `resident` pages: the kernel decided it, so the kernel
/// is the subject, and the victim and the job are what it names.
pub(crate) fn oom_killed(pid: u32, limited: u64, resident: u64) {
    ended(
        OOM_KILLED,
        Subject::KERNEL,
        Target {
            kind: target::PROCESS,
            id: u64::from(pid),
        },
        [limited as u32, (limited >> 32) as u32, saturated(resident)],
    );
}

/// Record `subject` asking for power action `action` (`ferrix_audit::power`),
/// just before it is taken, and say its number on the console: nothing can
/// read the record once the machine is off, so the console line is what
/// shows it was made, beside init's last read, which stops just short of it
/// (`docs/certification/AUDIT.md` §4).
///
/// And every high-value record the reader has not read -- made between its
/// last read and this -- goes to the console too, one line each, where the
/// console's log keeps it: nothing made after the reader's last read is lost
/// without a trace. A reader that read just before asking for the power
/// action leaves none, or the few a device's driver made meanwhile.
pub(crate) fn power(subject: Subject, action: u32) {
    let number = record_numbered(
        POWER,
        Outcome::Done,
        0,
        subject,
        Target::NONE,
        [action, 0, 0],
    )
    .unwrap_or(0);
    let read = READ_THROUGH.load(Ordering::Acquire);
    crate::console::println!(
        "  audit    power action {action} recorded as record {number}; the reader read the \
         high-value ring through {read}"
    );
    let end = STORE.high.lock().ring.next;
    let mut unread = [Record::EMPTY; 1];
    let mut at = read;
    while at < end {
        let (copied, next, lost) = STORE.high.lock().ring.read(at, &mut unread);
        if lost > 0 {
            // Overwritten before anyone read it -- a boot with no reader
            // keeps only the last of them: one line for the run, not none.
            crate::console::println!("  audit    lost #{}..#{}", at, at + lost - 1);
        }
        let [record] = unread;
        if copied == 0 {
            break;
        }
        crate::console::println!(
            "  audit    unread #{} {} pid {} job {} target {}:{} detail {} {} {}",
            record.sequence,
            record.event_name(),
            record.pid,
            record.job,
            record.target_kind,
            record.target(),
            record.detail[0],
            record.detail[1],
            record.detail[2],
        );
        at = next;
    }
}

/// How far the reader has read the high-value ring, for the native boot
/// check: a read whose copy out failed must not move it.
pub(crate) fn read_through_now() -> u64 {
    READ_THROUGH.load(Ordering::Acquire)
}

/// How far the reader -- the holder of the audit handle, through
/// `audit_read` -- has read the high-value ring: the number it reads from
/// next. The power action says it, and prints what lies past it.
static READ_THROUGH: AtomicU64 = AtomicU64::new(0);

/// Note that the reader has read the high-value ring up to `next`.
pub(crate) fn read_through(next: u64) {
    let _ = READ_THROUGH.fetch_max(next, Ordering::AcqRel);
}

/// The boot's audit id, 0 before the store starts.
pub(crate) fn id() -> u128 {
    STORE.id()
}

/// Read the kernel's store: see [`Store::read_at`].
pub(crate) fn read(which: Which, from: u64, out: &mut [Record]) -> Read {
    STORE.read_at(crate::timer::now_nanos(), which, from, out)
}
