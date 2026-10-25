//! The kernel's end of the ring: it produces submissions and consumes
//! completions.
//!
//! # Outstanding ids
//!
//! Every submission is remembered until its completion is consumed, in storage
//! the caller provides ([`Slot`]s), because a completion is only as good as the
//! kernel's ability to say which request it answers. An id that is not in the
//! table — never submitted, or already completed — is corruption, never a
//! second answer. The table is open-addressed on the id with linear probing and
//! backward-shift deletion, so lookups and removals cost a probe sequence
//! rather than a scan, and every probe loop is bounded by the storage length.
//!
//! The table also holds the kernel to the rule the crate documentation states:
//! never more than `entries` submissions outstanding. That bound is what keeps
//! the submission ring from overflowing without the kernel ever reading the
//! driver's `sub_head` — every unconsumed submission is outstanding — and what
//! guarantees the driver room in the completion ring.
//!
//! # Ending, and the data VMO
//!
//! A ring ends in one of three ways: the driver answers STOP with STOPPED, the
//! driver dies (its process ends or its control channel closes), or this side
//! detects corruption and the kernel treats the driver as dead. However it
//! ends, every outstanding request fails with EIO *at once* — [`KernelSide::end`]
//! hands them over — and the kernel does not wait for anything else.
//!
//! What it does wait for is the data VMO. Its pages are pinned for the device,
//! and on an untranslated domain, which is every domain today, a device that
//! was not reset can still write to those frames after they are unpinned. Not
//! even an orderly STOPPED proves the reset: it is the driver's word, and the
//! driver is not trusted. So devmgr resets the device before any pinned frame
//! is freed, and until it confirms, the pages stay held — leaked by design, and
//! the kernel glue logs it. [`KernelSide::data_vmo`] answers
//! [`DataVmo::HeldUntilReset`] from the moment a ring ends until the glue calls
//! [`KernelSide::confirm_reset`], and nothing else makes it
//! [`DataVmo::Releasable`]. If devmgr cannot reset the device, the glue never
//! calls it and the pages are never freed.

use core::fmt;

use crate::bell::{BELL_SUBMIT, Doorbell, Wait};
use crate::geometry::{Device, InvalidSubmission, check_submission};
use crate::layout::{
    DriverHeader, HeaderError, Op, RawCompletion, RingLayout, Status, Submission, header,
};
use crate::ring::{Consumer, Producer};
use crate::{Corruption, RingMemory};

/// Room for one outstanding submission, in storage the caller provides.
///
/// ```
/// # use ferrix_blkring::Slot;
/// let mut storage = [Slot::EMPTY; 64];
/// # let _ = &mut storage;
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Slot(Option<Submission>);

impl Slot {
    /// A slot holding nothing.
    pub const EMPTY: Slot = Slot(None);
}

/// Why [`KernelSide::attach`] refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AttachError {
    /// The ring header failed setup validation. Send REFUSED with
    /// [`crate::Refusal::Header`], and map nothing further.
    Header(HeaderError),
    /// The caller gave no storage for outstanding ids. A kernel bug, not the
    /// driver's.
    NoStorage,
}

impl fmt::Display for AttachError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttachError::Header(error) => write!(formatter, "ring header: {error}"),
            AttachError::NoStorage => formatter.write_str("no storage for outstanding ids"),
        }
    }
}

/// Why [`KernelSide::submit`] did not publish a submission.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubmitError {
    /// The ring is corrupt; see [`Corruption`].
    Corrupt(Corruption),
    /// STOP was sent or the ring has ended.
    NotRunning,
    /// `entries` submissions, or as many as the storage holds, are
    /// outstanding. Consume completions and try again.
    Full,
    /// The id is already outstanding.
    DuplicateId,
    /// The driver would refuse it, so it is not sent.
    Invalid(InvalidSubmission),
}

impl fmt::Display for SubmitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubmitError::Corrupt(corruption) => write!(formatter, "ring corrupt: {corruption}"),
            SubmitError::NotRunning => formatter.write_str("the ring is stopping or ended"),
            SubmitError::Full => {
                formatter.write_str("as many submissions as the ring holds are outstanding")
            }
            SubmitError::DuplicateId => formatter.write_str("the id is already outstanding"),
            SubmitError::Invalid(reason) => write!(formatter, "invalid submission: {reason}"),
        }
    }
}

/// A consumed, checked completion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Completed {
    /// The submission it completes, as the kernel published it — so the glue
    /// knows which region to copy read data out of, and how much.
    pub submission: Submission,
    /// How it ended. An OK write that did fewer bytes than it carried is
    /// already [`Status::IoError`] here.
    pub status: Status,
    /// Bytes the driver says it did, at most the payload length.
    pub bytes_done: u64,
    /// What the driver says the device took over it, in counter ticks; zero
    /// when it did not measure. Nothing is checked or trusted of it: it is a
    /// measurement the seam's row reads (`docs/OPAQUE-KERNEL.md`, S0).
    pub device_ticks: u32,
}

/// Whether a disk whose driver last answered anything at `last_answer` has
/// gone silent by `now` -- every time in nanoseconds on one clock -- and is
/// to be taken offline: a request is still waiting for its answer
/// (`outstanding`), and the driver has completed nothing at all for at least
/// `patience`.
///
/// A driver that answers other requests is alive, if slow, and its disk
/// stays: only the one request that waited out its patience fails. One that
/// answers nothing while a request has waited a whole patience is a hung
/// drive behind it or a driver that cannot recover it, which Linux's SCSI
/// error handler takes offline once its retries and resets have failed; the
/// driver did those already (`docs/BLOCK-RING.md`, "A disk that stops
/// answering"). A clock that went back, or a `last_answer` from the future,
/// is never silence.
#[must_use]
pub const fn gone_silent(now: u64, last_answer: u64, outstanding: bool, patience: u64) -> bool {
    outstanding && now >= last_answer && now - last_answer >= patience
}

/// How the glue learned the ring is over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ending {
    /// The driver answered STOP with STOPPED.
    Stopped,
    /// The driver's process ended, its control channel closed, or the kernel
    /// gave up on it after [`Corruption`].
    DriverDied,
}

/// Why a ring ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EndReason {
    /// STOPPED.
    Stopped,
    /// The driver died, or was treated as dead.
    DriverDied,
    /// This side detected corruption.
    Corrupt(Corruption),
}

/// Where a ring is in its life.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Exchanging requests.
    Running,
    /// STOP was sent. No new submissions; completions are still consumed.
    Stopping,
    /// Over. Outstanding requests fail at once; the data VMO is held until
    /// devmgr confirms the device reset.
    Ended(EndReason),
    /// Over, and devmgr confirmed the reset: the data VMO may be released.
    ResetConfirmed(EndReason),
}

impl Phase {
    const fn corruption(self) -> Option<Corruption> {
        match self {
            Phase::Ended(EndReason::Corrupt(corruption))
            | Phase::ResetConfirmed(EndReason::Corrupt(corruption)) => Some(corruption),
            _ => None,
        }
    }

    const fn is_live(self) -> bool {
        matches!(self, Phase::Running | Phase::Stopping)
    }
}

/// What the glue may do with the data VMO's pinned pages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DataVmo {
    /// The ring is live and the device uses the pages.
    InUse,
    /// The ring has ended but the device is not known to be reset: the pages
    /// stay pinned and held, and must not be freed.
    HeldUntilReset,
    /// devmgr confirmed the reset: the pins may be released and the VMO freed.
    Releasable,
}

/// [`KernelSide::confirm_reset`] on a ring that has not ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RingInUse;

impl fmt::Display for RingInUse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the ring has not ended, so a reset cannot release its data VMO")
    }
}

/// The kernel's end of one ring.
pub struct KernelSide<'s, M> {
    memory: M,
    /// Copied out of the header once, at attach.
    layout: RingLayout,
    device: Device,
    submissions: Producer,
    completions: Consumer,
    table: Table<'s>,
    phase: Phase,
}

impl<M> fmt::Debug for KernelSide<'_, M> {
    /// Leaves out the memory: formatting shared memory would be a great many
    /// reads of values the peer can change.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KernelSide")
            .field("layout", &self.layout)
            .field("device", &self.device)
            .field("submissions", &self.submissions)
            .field("completions", &self.completions)
            .field("outstanding", &self.table.len)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

impl<'s, M: RingMemory> KernelSide<'s, M> {
    /// Take up a ring the driver set up, for a `device` whose HELLO passed
    /// [`crate::Hello::validate`], in a ring VMO of `ring_bytes` bytes.
    ///
    /// Reads the driver's header fields once and never again, zeroes the
    /// kernel's own fields, and clears `storage`, which bounds how many
    /// submissions may be outstanding along with `entries`.
    ///
    /// # Errors
    ///
    /// [`AttachError::Header`] for any setup check the header fails.
    pub fn attach(
        mut memory: M,
        ring_bytes: u64,
        device: Device,
        storage: &'s mut [Slot],
    ) -> Result<Self, AttachError> {
        if storage.is_empty() {
            return Err(AttachError::NoStorage);
        }
        let layout = DriverHeader::read_from(&memory)
            .validate(ring_bytes)
            .map_err(AttachError::Header)?;
        memory.write_u32(header::SUB_TAIL, 0);
        memory.write_u32(header::COMP_HEAD, 0);
        memory.write_u32(header::COMP_WANT_BELL, 0);
        let limit = usize::try_from(layout.entries()).unwrap_or(usize::MAX);
        Ok(Self {
            memory,
            layout,
            device,
            submissions: Producer::default(),
            completions: Consumer::default(),
            table: Table::new(storage, limit),
            phase: Phase::Running,
        })
    }

    /// The ring's layout, as read at attach.
    #[must_use]
    pub const fn layout(&self) -> RingLayout {
        self.layout
    }

    /// The device, as HELLO described it.
    #[must_use]
    pub const fn device(&self) -> &Device {
        &self.device
    }

    /// Submissions published or staged and not yet completed.
    #[must_use]
    pub const fn outstanding(&self) -> usize {
        self.table.len()
    }

    /// The most submissions that may be outstanding at once.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.table.limit
    }

    /// Where the ring is in its life.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// The corruption this side detected, if it has.
    #[must_use]
    pub const fn corruption(&self) -> Option<Corruption> {
        self.phase.corruption()
    }

    /// What the glue may do with the data VMO's pages. Only
    /// [`KernelSide::confirm_reset`] ever makes this [`DataVmo::Releasable`].
    #[must_use]
    pub const fn data_vmo(&self) -> DataVmo {
        match self.phase {
            Phase::Running | Phase::Stopping => DataVmo::InUse,
            Phase::Ended(_) => DataVmo::HeldUntilReset,
            Phase::ResetConfirmed(_) => DataVmo::Releasable,
        }
    }

    /// Whether this side last asked the driver to ring.
    #[must_use]
    pub const fn wants_bell(&self) -> bool {
        self.completions.wants_bell()
    }

    /// Write `submission` into the next slot. The driver does not see it until
    /// [`KernelSide::publish`], so a burst is written first and published once.
    ///
    /// The caller copies a write's payload into the data VMO first.
    ///
    /// # Errors
    ///
    /// [`SubmitError`] says why nothing was written.
    pub fn submit(&mut self, submission: Submission) -> Result<(), SubmitError> {
        if let Some(corruption) = self.phase.corruption() {
            return Err(SubmitError::Corrupt(corruption));
        }
        if self.phase != Phase::Running {
            return Err(SubmitError::NotRunning);
        }
        let raw = submission.raw();
        let checked = check_submission(&raw, &self.device).map_err(SubmitError::Invalid)?;
        self.table.insert(checked)?;
        raw.write_to(
            &mut self.memory,
            self.layout.submission_at(self.submissions.tail()),
        );
        self.submissions.stage();
        Ok(())
    }

    /// Publish everything submitted since the last publish, and return the
    /// doorbell to ring if the driver asked for one.
    ///
    /// Does no I/O beyond the ring: the caller queues [`Doorbell::packet`] on
    /// the driver's port, and a full port counts as rung
    /// ([`crate::bell::rung`]).
    pub fn publish(&mut self) -> Option<Doorbell> {
        if !self.phase.is_live() {
            return None;
        }
        self.submissions.publish(
            &mut self.memory,
            header::SUB_TAIL,
            header::SUB_WANT_BELL,
            BELL_SUBMIT,
        )
    }

    /// Consume one completion, if one is pending.
    ///
    /// Once the ring has ended this reads nothing: it answers the corruption
    /// that ended it, or `None`.
    ///
    /// # Errors
    ///
    /// [`Corruption`], which also ends the ring: fail everything outstanding
    /// with [`KernelSide::end`] and treat the driver as dead.
    pub fn poll(&mut self) -> Result<Option<Completed>, Corruption> {
        if !self.phase.is_live() {
            return self.phase.corruption().map_or(Ok(None), Err);
        }
        let pending =
            self.completions
                .pending(&self.memory, header::COMP_TAIL, self.layout.entries());
        if self.check(pending)? == 0 {
            return Ok(None);
        }
        let at = self.layout.completion_at(self.completions.head());
        let raw = RawCompletion::read_from(&self.memory, at);
        let judged = self.judge(&raw);
        let completed = self.check(judged)?;
        // The head is published before the id leaves the table, so the driver
        // never sees room in the completion ring that the kernel has not
        // already stopped counting as outstanding.
        self.completions
            .advance(&mut self.memory, header::COMP_HEAD);
        let _ = self.table.remove(raw.id);
        Ok(Some(completed))
    }

    /// Every check on a completion, against the copy of it.
    fn judge(&self, raw: &RawCompletion) -> Result<Completed, Corruption> {
        let submission = self.table.find(raw.id).ok_or(Corruption::UnknownId)?;
        let status = Status::from_raw(raw.status).ok_or(Corruption::UnknownStatus)?;
        let len = self.device.payload_len(submission.count);
        if raw.bytes_done > len {
            return Err(Corruption::BytesDoneTooLarge);
        }
        let short_write = submission.op != Op::Read && raw.bytes_done < len;
        let status = if status == Status::Ok && short_write {
            Status::IoError
        } else {
            status
        };
        Ok(Completed {
            submission,
            status,
            bytes_done: raw.bytes_done,
            device_ticks: raw.device_ticks,
        })
    }

    /// Before sleeping on the completion port: ask to be rung, then look once
    /// more. [`Wait::Pending`] means poll instead of sleeping.
    ///
    /// # Errors
    ///
    /// [`Corruption`] in the completion tail, which ends the ring.
    pub fn prepare_to_sleep(&mut self) -> Result<Wait, Corruption> {
        if !self.phase.is_live() {
            return self.phase.corruption().map_or(Ok(Wait::Sleep), Err);
        }
        let wait = self.completions.prepare_to_sleep(
            &mut self.memory,
            header::COMP_WANT_BELL,
            header::COMP_TAIL,
            self.layout.entries(),
        );
        self.check(wait)
    }

    /// On waking: stop asking to be rung. Poll until nothing is pending next.
    pub fn woke(&mut self) {
        if self.phase.is_live() {
            self.completions
                .woke(&mut self.memory, header::COMP_WANT_BELL);
        }
    }

    /// STOP was sent: take no new submissions, keep consuming completions.
    pub fn stop(&mut self) {
        if self.phase == Phase::Running {
            self.phase = Phase::Stopping;
        }
    }

    /// The ring is over. Returns every submission still outstanding, each
    /// once, to fail with EIO now; the side reads and writes the ring no more.
    ///
    /// After an orderly STOPPED, poll until nothing is pending first, so the
    /// completions the driver did write are not failed. A ring that already
    /// ended — by corruption, say — keeps its first reason.
    ///
    /// The data VMO is [`DataVmo::HeldUntilReset`] from here, whatever the
    /// ending: see the module documentation.
    pub fn end(&mut self, ending: Ending) -> Drain<'_, 's> {
        if self.phase.is_live() {
            self.phase = Phase::Ended(match ending {
                Ending::Stopped => EndReason::Stopped,
                Ending::DriverDied => EndReason::DriverDied,
            });
        }
        Drain {
            table: &mut self.table,
            at: 0,
        }
    }

    /// devmgr confirmed the device is reset: the data VMO's pins may be
    /// released. The only way to [`DataVmo::Releasable`].
    ///
    /// # Errors
    ///
    /// [`RingInUse`] if the ring has not ended; nothing changes.
    pub fn confirm_reset(&mut self) -> Result<(), RingInUse> {
        match self.phase {
            Phase::Ended(reason) | Phase::ResetConfirmed(reason) => {
                self.phase = Phase::ResetConfirmed(reason);
                Ok(())
            }
            Phase::Running | Phase::Stopping => Err(RingInUse),
        }
    }

    /// Record corruption, which ends the ring, and pass the result on.
    fn check<T>(&mut self, result: Result<T, Corruption>) -> Result<T, Corruption> {
        if let Err(corruption) = &result {
            self.phase = Phase::Ended(EndReason::Corrupt(*corruption));
        }
        result
    }
}

/// The outstanding submissions of an ended ring, each yielded once.
///
/// Dropping it early leaves the rest for the next [`KernelSide::end`].
#[derive(Debug)]
pub struct Drain<'a, 's> {
    table: &'a mut Table<'s>,
    at: usize,
}

impl Iterator for Drain<'_, '_> {
    type Item = Submission;

    fn next(&mut self) -> Option<Submission> {
        while let Some(slot) = self.table.slots.get_mut(self.at) {
            self.at = self.at.saturating_add(1);
            if let Some(submission) = slot.0.take() {
                self.table.len = self.table.len.saturating_sub(1);
                return Some(submission);
            }
        }
        None
    }
}

/// Outstanding submissions by id: open addressing over the caller's slots.
#[derive(Debug)]
pub(crate) struct Table<'s> {
    slots: &'s mut [Slot],
    len: usize,
    limit: usize,
}

impl<'s> Table<'s> {
    /// A table over `slots`, cleared, holding at most `limit` entries — and
    /// never more than there are slots.
    pub(crate) fn new(slots: &'s mut [Slot], limit: usize) -> Self {
        for slot in slots.iter_mut() {
            *slot = Slot::EMPTY;
        }
        let limit = limit.min(slots.len());
        Self {
            slots,
            len: 0,
            limit,
        }
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    fn home(&self, id: u64) -> usize {
        match u64::try_from(self.slots.len()) {
            // The remainder is below the slot count, which is a `usize`.
            Ok(count) if count > 0 => (id % count) as usize,
            _ => 0,
        }
    }

    fn after(&self, at: usize) -> usize {
        let next = at.wrapping_add(1);
        if next >= self.slots.len() { 0 } else { next }
    }

    fn position(&self, id: u64) -> Option<usize> {
        let mut at = self.home(id);
        for _ in 0..self.slots.len() {
            match self.slots.get(at)?.0 {
                None => return None,
                Some(submission) if submission.id == id => return Some(at),
                Some(_) => at = self.after(at),
            }
        }
        None
    }

    /// The outstanding submission `id` names.
    pub(crate) fn find(&self, id: u64) -> Option<Submission> {
        self.slots.get(self.position(id)?)?.0
    }

    /// Remember `submission` as outstanding.
    pub(crate) fn insert(&mut self, submission: Submission) -> Result<(), SubmitError> {
        if self.len >= self.limit {
            return Err(SubmitError::Full);
        }
        let mut at = self.home(submission.id);
        for _ in 0..self.slots.len() {
            let Some(slot) = self.slots.get_mut(at) else {
                break;
            };
            match slot.0 {
                None => {
                    slot.0 = Some(submission);
                    self.len = self.len.saturating_add(1);
                    return Ok(());
                }
                Some(held) if held.id == submission.id => return Err(SubmitError::DuplicateId),
                Some(_) => at = self.after(at),
            }
        }
        // Unreachable while `limit` is below the slot count, which `new` keeps.
        Err(SubmitError::Full)
    }

    /// Forget the submission `id` names, and return it.
    ///
    /// Backward-shift deletion: every entry after the hole that could live in
    /// it moves into it, so no lookup ever stops early at a hole it should
    /// have probed past.
    pub(crate) fn remove(&mut self, id: u64) -> Option<Submission> {
        let mut hole = self.position(id)?;
        let removed = self.slots.get_mut(hole)?.0.take();
        self.len = self.len.saturating_sub(1);
        let count = self.slots.len();
        let mut at = self.after(hole);
        for _ in 0..count {
            let Some(entry) = self.slots.get(at).and_then(|slot| slot.0) else {
                break;
            };
            if distance(self.home(entry.id), at, count) >= distance(hole, at, count) {
                if let Some(slot) = self.slots.get_mut(hole) {
                    slot.0 = Some(entry);
                }
                if let Some(slot) = self.slots.get_mut(at) {
                    slot.0 = None;
                }
                hole = at;
            }
            at = self.after(at);
        }
        removed
    }
}

/// Steps forward from `from` to `to` on a ring of `count` slots, both below
/// `count`.
const fn distance(from: usize, to: usize, count: usize) -> usize {
    if to >= from {
        to - from
    } else {
        count.saturating_sub(from).saturating_add(to)
    }
}
