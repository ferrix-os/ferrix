//! Stage 9's self-check of the address waits (K5, `docs/microkernel/K5.md`
//! §6): the native calls `address_wait`, `address_wake`, `address_requeue`
//! and `address_wait_any`, driven through `native::dispatch` with raw
//! registers as a program makes them, over processes the check builds.
//!
//! A waiter is a kernel task of the check's making that makes the call on a
//! process's behalf and posts its answer. Before a wake, the check waits to
//! see the waiter listed on the table, so that a wake which works cannot be
//! told from one that arrived first. Every wait the check expects to be woken
//! has a deadline far ahead, so a wake that is lost comes back as a timeout,
//! which the check names, rather than as a boot that never ends.
//!
//! The negative controls of §6 (C1 to C7) are single edits of
//! `user::address_wait` run through `gate.sh control --expect`; each breaks
//! one rule and must stop the boot on the sentence of the case that holds
//! that rule.

use alloc::sync::Arc;

use crate::sync::SpinLock;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::Rights;
use ferrix_native_abi::status;
use ferrix_native_abi::types::{
    ADDRESS_BY_VMO, ADDRESS_COMPARE, ADDRESS_SHARED, MAP_READ, MAP_WRITE,
};

use crate::object::check::{SCRATCH, Side, reg};
use crate::object::job::{Job, KILLED_STATUS};
use crate::object::quota::Resource;
use crate::object::{self, Object};
use crate::sched::{self, WaitQueue};
use crate::syscall::process::{self, Process};
use crate::syscall::{SyscallArgs, native};
use crate::user::address_wait::{self, Key};
use crate::user::vmo::Vmo;

/// What the check reports for its boot line.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Report {
    /// Waits woken by a wake, a requeue's wake or a wake through another
    /// process.
    pub(crate) woken: u32,
    /// Processes the waits and wakes were made from.
    pub(crate) processes: u32,
    /// Calls refused as the ABI says.
    pub(crate) refusals: u32,
    /// A wake's walk over a bucket, nanoseconds per entry it looked at.
    pub(crate) walk_ns: u64,
}

/// Private words in the scratch region, each in the first page.
const WORD: u64 = SCRATCH + 0x800;
/// A second word, for wait-any and requeue.
const WORD_B: u64 = SCRATCH + 0x804;
/// A third.
const WORD_C: u64 = SCRATCH + 0x808;
/// Where each waiter slot's deadline is staged.
const DEADLINES: u64 = SCRATCH + 0x900;
/// Where a wait-any vector is staged.
const VECTOR: u64 = SCRATCH + 0xA00;
/// Where a VMO offset for `vmo_map` is staged.
const OFFSET_AT: u64 = SCRATCH + 0xB00;
/// Where the shared VMO is mapped in the first process.
const MAPPED_A: u64 = 0x6000_0000;
/// And in the second, elsewhere, so that a key by address could not match.
const MAPPED_B: u64 = 0x6100_0000;
/// The shared word's offset in its VMO.
const SHARED_OFFSET: u64 = 0x40;

/// How long a wait the check expects to be woken may sleep: far longer than
/// any wake takes, short enough that a lost one fails the boot in seconds.
const PATIENCE_NANOS: u64 = 10_000_000_000;
/// How long the check gives a waiter to be listed.
const LISTED_NANOS: u64 = 5_000_000_000;
/// The deadline of the timed case.
const SHORT_NANOS: u64 = 5_000_000;
/// How late past its deadline the timed case may come back: under the
/// second a sleep with no deadline of its own takes to look again.
const LATE_NANOS: u64 = 900_000_000;

/// The most waiters at once.
const SLOTS: usize = 4;

/// One call a waiter makes.
struct Call {
    /// On whose behalf.
    process: Arc<Process>,
    /// The native number.
    number: usize,
    /// Its registers.
    args: [u64; 6],
}

/// The call each waiter slot is to make, taken by its task.
static CALLS: SpinLock<[Option<Call>; SLOTS]> = SpinLock::new([const { None }; SLOTS]);
/// Each slot's answer, once it has one.
static ANSWERS: SpinLock<[Option<Result<usize, Errno>>; SLOTS]> =
    SpinLock::new([const { None }; SLOTS]);
/// Woken when a slot answers.
static ANSWERED: WaitQueue = WaitQueue::new();

/// Make a native call on `process`'s behalf.
fn dispatch(process: &Process, number: usize, args: [u64; 6]) -> Result<usize, Errno> {
    let args = SyscallArgs {
        abi: crate::trap::Abi::Native,
        number,
        args,
        ip: 0,
    };
    native::dispatch(&args, Some(process))
}

/// A waiter's task: make slot `slot`'s call and post the answer.
fn waiter(slot: usize) {
    let call = CALLS.lock().get_mut(slot).and_then(Option::take);
    if let Some(call) = call {
        let answer = dispatch(&call.process, call.number, call.args);
        drop(call);
        if let Some(place) = ANSWERS.lock().get_mut(slot) {
            *place = Some(answer);
        }
        ANSWERED.wake_all();
    }
}

/// Start a waiter in `slot` making `number` with `args` for `side`.
fn start(slot: usize, side: &Side, number: usize, args: [u64; 6]) -> Result<(), &'static str> {
    if let Some(place) = ANSWERS.lock().get_mut(slot) {
        *place = None;
    }
    if let Some(place) = CALLS.lock().get_mut(slot) {
        *place = Some(Call {
            process: Arc::clone(&side.process),
            number,
            args,
        });
    }
    sched::spawn("addrwait waiter", waiter, slot, ferrix_sched::NICE_0_WEIGHT)
        .map(drop)
        .map_err(|_| "addrwait: could not start a waiter")
}

/// Slot `slot`'s answer, waiting up to `nanos` for it; `None` if it has none
/// by then.
fn answer(slot: usize, nanos: u64) -> Option<Result<usize, Errno>> {
    let deadline = crate::timer::now_nanos().saturating_add(nanos);
    let _ = ANSWERED.wait_until_deadline(
        || ANSWERS.lock().get(slot).is_some_and(Option::is_some),
        deadline,
    );
    ANSWERS.lock().get_mut(slot).and_then(Option::take)
}

/// A deadline `nanos` from now, staged for slot `slot` in `side`; the
/// pointer to it.
fn deadline_in(side: &Side, slot: usize, nanos: u64) -> Result<u64, &'static str> {
    let at = DEADLINES + 8 * slot as u64;
    let deadline = crate::timer::now_nanos().saturating_add(nanos);
    side.put(at, &deadline.to_ne_bytes())?;
    Ok(at)
}

/// Wait until `keys` have `count` waiters listed between them.
fn until_listed(keys: &[Key], count: usize) -> Result<(), &'static str> {
    let deadline = crate::timer::now_nanos().saturating_add(LISTED_NANOS);
    while address_wait::waiters_on(keys) < count {
        if crate::timer::now_nanos() >= deadline {
            return Err("addrwait: a waiter was never listed on the table");
        }
        sched::sleep_for(1_000_000);
    }
    Ok(())
}

/// The private key of `address` in `side`'s space.
fn private(side: &Side, address: u64) -> Key {
    Key::new(side.process.space(), address, false)
}

/// Write a word into `side`'s memory.
fn set(side: &Side, at: u64, value: u32) -> Result<(), &'static str> {
    side.put(at, &value.to_ne_bytes())
}

/// Count a refusal that is exactly `wanted`, or fail with `what`.
fn refused(
    result: Result<usize, Errno>,
    wanted: Errno,
    what: &'static str,
    report: &mut Report,
) -> Result<(), &'static str> {
    if result == Err(wanted) {
        report.refusals += 1;
        Ok(())
    } else {
        Err(what)
    }
}

/// Run every case and report.
pub(crate) fn run() -> Result<Report, &'static str> {
    let mut report = Report::default();
    let a = Side::new()?;
    let b = Side::new()?;
    let c = Side::new()?;
    report.processes = 3;
    a_wait_is_woken_and_a_changed_word_refused(&a, &mut report)?;
    a_shared_word_is_one_word_by_mapping_and_by_handle(&a, &b, &c, &mut report)?;
    wakes_take_the_oldest_and_match_bitsets(&a, &mut report)?;
    a_requeue_moves_waiters_across_buckets(&a, &mut report)?;
    a_wait_on_many_words_answers_which(&a, &mut report)?;
    a_deadline_ends_a_wait_and_never_early(&a, &mut report)?;
    an_ending_process_ends_its_wait(&mut report)?;
    a_wait_beyond_its_jobs_limit_is_refused(&mut report)?;
    the_calls_refuse_what_the_abi_refuses(&a, &mut report)?;
    report.walk_ns = a_walk_is_measured_and_its_room_given_back(&a)?;
    for side in [&a, &b, &c] {
        side.close_everything();
    }
    Ok(report)
}

/// Case 1 and 2: a private wait sleeps until a wake of its word, and a wait
/// on a word that does not hold its value is refused with nothing listed.
///
/// Verifies: H.OBJ.20, L.user.136, L.object.203, L.object.204
fn a_wait_is_woken_and_a_changed_word_refused(
    a: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    set(a, WORD, 7)?;
    // With a deadline already past, so that a wait which sleeps on a changed
    // word comes back to be named rather than sleeping for ever.
    let past = deadline_in(a, 1, 0)?;
    refused(
        a.call(
            nr::ADDRESS_WAIT,
            &[WORD, 8, u64::from(u32::MAX), 0, past, 0],
        ),
        status::SHOULD_WAIT,
        "addrwait: a wait on a word that had changed slept",
        report,
    )?;
    if address_wait::waiters_on(&[private(a, WORD)]) != 0 {
        return Err("addrwait: a refused wait was left on the table");
    }
    let deadline = deadline_in(a, 0, PATIENCE_NANOS)?;
    start(
        0,
        a,
        nr::ADDRESS_WAIT,
        [WORD, 7, u64::from(u32::MAX), 0, deadline, 0],
    )?;
    until_listed(&[private(a, WORD)], 1)?;
    let woken = a
        .call(nr::ADDRESS_WAKE, &[WORD, 1, u64::from(u32::MAX), 0, 0, 0])
        .map_err(|_| "addrwait: a wake was refused")?;
    match answer(0, PATIENCE_NANOS) {
        Some(Ok(0)) if woken == 1 => {}
        Some(Err(status::TIMED_OUT)) => {
            return Err("addrwait: a wake was lost: its waiter timed out");
        }
        _ => return Err("addrwait: a woken wait did not answer as woken"),
    }
    report.woken += 1;
    Ok(())
}

/// What case 3, 4 and 10 share: one VMO mapped into `a` and `b` at two
/// addresses, and two handles to it in `c`, one with `READ` and one without.
struct SharedWord {
    /// The VMO.
    object: Arc<Vmo>,
    /// The word's address in `a`.
    at_a: u64,
    /// And in `b`.
    at_b: u64,
    /// `c`'s handle with `READ`.
    reader: Handle,
    /// `c`'s handle with `MAP` only.
    blind: Handle,
    /// The key `a`'s shared wait gets.
    key: Key,
}

/// Map a fresh VMO into `a` and `b` and give `c` two handles to it.
fn shared_word(a: &Side, b: &Side, c: &Side) -> Result<SharedWord, &'static str> {
    let vmo = a.handle(
        nr::VMO_CREATE,
        &[2 * PAGE_SIZE],
        "addrwait: vmo_create failed",
    )?;
    let object = vmo_of(a, vmo)?;
    let in_b = give(b, &object, Rights::READ | Rights::WRITE | Rights::MAP)?;
    for (side, handle, at) in [(a, vmo, MAPPED_A), (b, in_b, MAPPED_B)] {
        side.put(OFFSET_AT, &0_u64.to_ne_bytes())?;
        let both = u64::from(MAP_READ | MAP_WRITE);
        let _ = side
            .call(nr::VMO_MAP, &[reg(handle), at, PAGE_SIZE, both, OFFSET_AT])
            .map_err(|_| "addrwait: vmo_map failed")?;
    }
    let at_a = MAPPED_A + SHARED_OFFSET;
    Ok(SharedWord {
        reader: give(c, &object, Rights::READ)?,
        blind: give(c, &object, Rights::MAP)?,
        object,
        at_a,
        at_b: MAPPED_B + SHARED_OFFSET,
        key: Key::new(a.process.space(), at_a, true),
    })
}

/// A wake of one on the shared word, by `handle` in `c`.
fn wake_by_handle(c: &Side, handle: Handle) -> Result<usize, Errno> {
    let all = u64::from(u32::MAX);
    c.call(
        nr::ADDRESS_WAKE,
        &[SHARED_OFFSET, 1, all, ADDRESS_BY_VMO, reg(handle), 0],
    )
}

/// Start `a`'s shared wait on the word in slot 0, and wait for it to be
/// listed under any of `keys`.
fn shared_waiter(a: &Side, word: &SharedWord, keys: &[Key]) -> Result<(), &'static str> {
    let deadline = deadline_in(a, 0, PATIENCE_NANOS)?;
    let all = u64::from(u32::MAX);
    start(
        0,
        a,
        nr::ADDRESS_WAIT,
        [word.at_a, 3, all, ADDRESS_SHARED, deadline, 0],
    )?;
    until_listed(keys, 1)
}

/// Case 3, 4 and 10: a shared word is one word in every process that maps
/// its VMO, at whatever address, and the same word named by a handle and an
/// offset with no mapping at all; a wait outlives the unmapping of its word;
/// a wait on the unmapped address faults; a word in a page of the VMO never
/// committed reads as zero and stays uncommitted.
///
/// Verifies: L.user.135, L.user.137, L.user.143, L.object.203, L.object.204
fn a_shared_word_is_one_word_by_mapping_and_by_handle(
    a: &Side,
    b: &Side,
    c: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let word = shared_word(a, b, c)?;
    set(a, word.at_a, 3)?;
    by_mapping_across_processes(a, b, &word, report)?;
    by_handle_without_a_mapping(a, c, &word, report)?;
    after_its_mapping_is_gone(a, c, &word, report)
}

/// Case 3: a shared wait in `a` is found by a shared wake through `b`'s
/// mapping at another address, and not by a private one. Its private key is
/// watched beside its shared one, so that a shared word wrongly keyed by
/// address is still seen listed and the failure named is the wake's.
fn by_mapping_across_processes(
    a: &Side,
    b: &Side,
    word: &SharedWord,
    report: &mut Report,
) -> Result<(), &'static str> {
    shared_waiter(a, word, &[word.key.clone(), private(a, word.at_a)])?;
    let all = u64::from(u32::MAX);
    let private_wake = b
        .call(nr::ADDRESS_WAKE, &[word.at_b, 1, all, 0, 0, 0])
        .map_err(|_| "addrwait: a private wake was refused")?;
    if private_wake != 0 {
        return Err("addrwait: a private wake in another process found a shared waiter");
    }
    let woken = b
        .call(nr::ADDRESS_WAKE, &[word.at_b, 1, all, ADDRESS_SHARED, 0, 0])
        .map_err(|_| "addrwait: a shared wake was refused")?;
    if woken != 1 || answer(0, PATIENCE_NANOS) != Some(Ok(0)) {
        return Err("addrwait: a shared waiter was not found across processes");
    }
    report.woken += 1;
    Ok(())
}

/// Case 4: the same wait is found by a wake by handle from `c`, which has no
/// mapping, and not through a handle without `READ`; a wait by handle on a
/// page never committed reads zero and commits nothing.
fn by_handle_without_a_mapping(
    a: &Side,
    c: &Side,
    word: &SharedWord,
    report: &mut Report,
) -> Result<(), &'static str> {
    shared_waiter(a, word, core::slice::from_ref(&word.key))?;
    refused(
        wake_by_handle(c, word.blind),
        status::ACCESS_DENIED,
        "addrwait: a wake by a handle without READ was let through",
        report,
    )?;
    let woken =
        wake_by_handle(c, word.reader).map_err(|_| "addrwait: a wake by handle was refused")?;
    if woken != 1 || answer(0, PATIENCE_NANOS) != Some(Ok(0)) {
        return Err("addrwait: a wake by handle did not find the waiter by mapping");
    }
    report.woken += 1;

    let committed = word.object.committed();
    let past = deadline_in(c, 1, 0)?;
    let all = u64::from(u32::MAX);
    refused(
        c.call(
            nr::ADDRESS_WAIT,
            &[
                PAGE_SIZE + 8,
                0,
                all,
                ADDRESS_BY_VMO,
                past,
                reg(word.reader),
            ],
        ),
        status::TIMED_OUT,
        "addrwait: a wait on an uncommitted page did not read it as zero",
        report,
    )?;
    if word.object.committed() != committed {
        return Err("addrwait: a wait committed a page of its VMO");
    }
    Ok(())
}

/// Case 10: a waiter whose word is unmapped while it sleeps stays listed and
/// is woken by handle; a wait on the unmapped address faults.
fn after_its_mapping_is_gone(
    a: &Side,
    c: &Side,
    word: &SharedWord,
    report: &mut Report,
) -> Result<(), &'static str> {
    shared_waiter(a, word, core::slice::from_ref(&word.key))?;
    a.process
        .space()
        .unmap(MAPPED_A, PAGE_SIZE)
        .map_err(|_| "addrwait: the unmap failed")?;
    if address_wait::waiters_on(core::slice::from_ref(&word.key)) != 1 {
        return Err("addrwait: an unmap took a sleeping waiter off the table");
    }
    let woken = wake_by_handle(c, word.reader)
        .map_err(|_| "addrwait: a wake by handle after an unmap was refused")?;
    if woken != 1 || answer(0, PATIENCE_NANOS) != Some(Ok(0)) {
        return Err("addrwait: a waiter whose word was unmapped was not woken by handle");
    }
    report.woken += 1;
    refused(
        a.call(
            nr::ADDRESS_WAIT,
            &[word.at_a, 3, u64::from(u32::MAX), ADDRESS_SHARED, 0, 0],
        ),
        status::FAULT,
        "addrwait: a wait on an unmapped address did not fault",
        report,
    )
}

/// The VMO a handle in `side` names.
fn vmo_of(side: &Side, handle: Handle) -> Result<Arc<Vmo>, &'static str> {
    side.process.with_handles(|table| match table.get(handle) {
        Ok((Object::Vmo(vmo), _)) => Ok(Arc::clone(vmo)),
        _ => Err("addrwait: a handle that should name a VMO does not"),
    })
}

/// Put a handle to `vmo` with `rights` in `side`'s table, as a message would.
fn give(side: &Side, vmo: &Arc<Vmo>, rights: Rights) -> Result<Handle, &'static str> {
    side.process
        .with_handles(|table| table.insert(Object::Vmo(Arc::clone(vmo)), rights))
        .map_err(|refused| {
            object::dispose([refused]);
            "addrwait: a handle could not be given"
        })
}

/// Case 5: a wake of two rouses the two oldest of three waiters, and a wake
/// whose bitset shares no bit with a waiter's rouses none.
///
/// Verifies: L.user.138, L.object.204
fn wakes_take_the_oldest_and_match_bitsets(
    a: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    set(a, WORD, 5)?;
    let key = private(a, WORD);
    for slot in 0..3 {
        let deadline = deadline_in(a, slot, PATIENCE_NANOS)?;
        let bitset = if slot == 2 { 0b01 } else { u64::from(u32::MAX) };
        start(slot, a, nr::ADDRESS_WAIT, [WORD, 5, bitset, 0, deadline, 0])?;
        until_listed(core::slice::from_ref(&key), slot + 1)?;
    }
    let none = a
        .call(nr::ADDRESS_WAKE, &[WORD, 0, u64::from(u32::MAX), 0, 0, 0])
        .map_err(|_| "addrwait: a wake of none was refused")?;
    if none != 0 || address_wait::waiters_on(core::slice::from_ref(&key)) != 3 {
        return Err("addrwait: a wake of zero roused a waiter");
    }
    let woken = a
        .call(nr::ADDRESS_WAKE, &[WORD, 2, u64::from(u32::MAX), 0, 0, 0])
        .map_err(|_| "addrwait: a wake of two was refused")?;
    if woken != 2
        || answer(0, PATIENCE_NANOS) != Some(Ok(0))
        || answer(1, PATIENCE_NANOS) != Some(Ok(0))
    {
        return Err("addrwait: a wake of two did not rouse the two oldest waiters");
    }
    report.woken += 2;
    let missed = a
        .call(nr::ADDRESS_WAKE, &[WORD, 1, 0b10, 0, 0, 0])
        .map_err(|_| "addrwait: a wake with a bitset was refused")?;
    if missed != 0 {
        return Err("addrwait: a wake roused a waiter whose bitset it shares no bit with");
    }
    let matched = a
        .call(nr::ADDRESS_WAKE, &[WORD, 1, 0b11, 0, 0, 0])
        .map_err(|_| "addrwait: a wake with a bitset was refused")?;
    if matched != 1 || answer(2, PATIENCE_NANOS) != Some(Ok(0)) {
        return Err("addrwait: a wake did not rouse a waiter whose bitset it shares a bit with");
    }
    report.woken += 1;
    Ok(())
}

/// Case 6: a requeue moves waiters from one word onto another in another
/// bucket, where a wake finds them; a compared requeue on a changed word is
/// refused and moves nothing.
///
/// Verifies: L.user.139, L.object.205
fn a_requeue_moves_waiters_across_buckets(
    a: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let source = WORD;
    let source_bucket = address_wait::bucket_of(&private(a, source));
    let target = (1..256_u64)
        .map(|n| WORD + 4 * n)
        .find(|&at| address_wait::bucket_of(&private(a, at)) != source_bucket)
        .ok_or("addrwait: no second word in another bucket")?;
    set(a, source, 9)?;
    let (from, to) = (private(a, source), private(a, target));
    for slot in 0..2 {
        let deadline = deadline_in(a, slot, PATIENCE_NANOS)?;
        start(
            slot,
            a,
            nr::ADDRESS_WAIT,
            [source, 9, u64::from(u32::MAX), 0, deadline, 0],
        )?;
        until_listed(core::slice::from_ref(&from), slot + 1)?;
    }
    refused(
        a.call(
            nr::ADDRESS_REQUEUE,
            &[source, target, 0, 2, ADDRESS_COMPARE, 10],
        ),
        status::SHOULD_WAIT,
        "addrwait: a compared requeue of a changed word moved waiters",
        report,
    )?;
    let moved = a
        .call(
            nr::ADDRESS_REQUEUE,
            &[source, target, 0, 2, ADDRESS_COMPARE, 9],
        )
        .map_err(|_| "addrwait: a requeue was refused")?;
    if moved != 2
        || address_wait::waiters_on(core::slice::from_ref(&from)) != 0
        || address_wait::waiters_on(core::slice::from_ref(&to)) != 2
    {
        return Err("addrwait: a requeue did not move its waiters onto the other word");
    }
    // Back the other way, waking the oldest and moving the other: a
    // requeue's wake rouses as a wake does.
    set(a, target, 9)?;
    let taken = a
        .call(nr::ADDRESS_REQUEUE, &[target, source, 1, 1, 0, 0])
        .map_err(|_| "addrwait: a requeue that wakes was refused")?;
    if taken != 2 || answer(0, PATIENCE_NANOS) != Some(Ok(0)) {
        return Err("addrwait: a requeue's wake did not rouse the oldest waiter");
    }
    let woken = a
        .call(nr::ADDRESS_WAKE, &[source, 1, u64::from(u32::MAX), 0, 0, 0])
        .map_err(|_| "addrwait: a wake after a requeue was refused")?;
    if woken != 1 || answer(1, PATIENCE_NANOS) != Some(Ok(0)) {
        return Err("addrwait: requeued waiters were not woken on their new word");
    }
    report.woken += 2;
    Ok(())
}

/// Case 7: a wait on three words answers the index of the one woken, and
/// leaves no entry on any of them.
///
/// Verifies: L.user.140, L.object.206
fn a_wait_on_many_words_answers_which(a: &Side, report: &mut Report) -> Result<(), &'static str> {
    let words = [WORD, WORD_B, WORD_C];
    let mut vector = [0_u8; 48];
    for (index, (&at, element)) in words.iter().zip(vector.chunks_exact_mut(16)).enumerate() {
        set(a, at, 20 + index as u32)?;
        let (address, rest) = element.split_at_mut(8);
        address.copy_from_slice(&at.to_ne_bytes());
        if let Some(expected) = rest.get_mut(..4) {
            expected.copy_from_slice(&(20 + index as u32).to_ne_bytes());
        }
    }
    a.put(VECTOR, &vector)?;
    let keys = words.map(|at| private(a, at));
    let deadline = deadline_in(a, 0, PATIENCE_NANOS)?;
    start(0, a, nr::ADDRESS_WAIT_ANY, [VECTOR, 3, 0, deadline, 0, 0])?;
    until_listed(&keys, 3)?;
    let woken = a
        .call(nr::ADDRESS_WAKE, &[WORD_B, 1, u64::from(u32::MAX), 0, 0, 0])
        .map_err(|_| "addrwait: a wake of a wait-any word was refused")?;
    if woken != 1 || answer(0, PATIENCE_NANOS) != Some(Ok(1)) {
        return Err("addrwait: a wait on many words did not answer the word woken");
    }
    if address_wait::waiters_on(&keys) != 0 {
        return Err("addrwait: a wait on many words left an entry on the table");
    }
    report.woken += 1;
    Ok(())
}

/// Case 8: a wait whose deadline passes answers `TIMED_OUT`, not before its
/// deadline; one whose deadline has passed already answers at once.
///
/// Verifies: L.user.141
fn a_deadline_ends_a_wait_and_never_early(
    a: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    set(a, WORD, 1)?;
    let past = deadline_in(a, 1, 0)?;
    refused(
        a.call(
            nr::ADDRESS_WAIT,
            &[WORD, 1, u64::from(u32::MAX), 0, past, 0],
        ),
        status::TIMED_OUT,
        "addrwait: a wait whose deadline had passed did not answer TIMED_OUT",
        report,
    )?;
    let at = deadline_in(a, 1, SHORT_NANOS)?;
    let started = crate::timer::now_nanos();
    let answer = a.call(nr::ADDRESS_WAIT, &[WORD, 1, u64::from(u32::MAX), 0, at, 0]);
    let took = crate::timer::now_nanos().saturating_sub(started);
    if answer != Err(status::TIMED_OUT) {
        return Err("addrwait: a wait outlived its deadline or did not answer TIMED_OUT");
    }
    if took < SHORT_NANOS {
        return Err("addrwait: a wait answered TIMED_OUT before its deadline");
    }
    if took > SHORT_NANOS + LATE_NANOS {
        return Err("addrwait: a wait outlived its deadline");
    }
    report.refusals += 1;
    Ok(())
}

/// Case 9: a waiter whose process ends leaves its wait with `EINTR`, and
/// nothing of it stays on the table.
///
/// Verifies: L.user.142
fn an_ending_process_ends_its_wait(report: &mut Report) -> Result<(), &'static str> {
    let d = Side::new()?;
    report.processes += 1;
    set(&d, WORD, 4)?;
    let deadline = deadline_in(&d, 3, PATIENCE_NANOS)?;
    start(
        3,
        &d,
        nr::ADDRESS_WAIT,
        [WORD, 4, u64::from(u32::MAX), 0, deadline, 0],
    )?;
    let key = private(&d, WORD);
    until_listed(core::slice::from_ref(&key), 1)?;
    process::kill(&d.process, KILLED_STATUS);
    if answer(3, PATIENCE_NANOS) != Some(Err(Errno::EINTR)) {
        return Err("addrwait: a waiter whose process ended did not leave with EINTR");
    }
    if address_wait::waiters_on(core::slice::from_ref(&key)) != 0 {
        return Err("addrwait: an ended waiter was left on the table");
    }
    d.close_everything();
    Ok(())
}

/// Case 11: a wait is charged to its process's job: refused `NO_MEMORY`, with
/// nothing listed, in a job with no memory to spare, while a sibling job's
/// wait goes on; and the charge is given back when the wait ends.
///
/// Verifies: L.user.144
fn a_wait_beyond_its_jobs_limit_is_refused(report: &mut Report) -> Result<(), &'static str> {
    let tree = Job::new_root().map_err(|_| "addrwait: no memory for a job")?;
    let full = tree
        .new_child()
        .map_err(|_| "addrwait: a job refused a child")?;
    let sibling = tree
        .new_child()
        .map_err(|_| "addrwait: a job refused a child")?;
    let starved = member_of(&full)?;
    let fed = member_of(&sibling)?;
    report.processes += 2;
    let used = |job: &Job| job.usage(Resource::Kernel).map_or(0, |usage| usage.used);
    let _ = full.set_limit(Resource::Memory, 1);
    set(&starved, WORD, 2)?;
    set(&fed, WORD, 2)?;
    let past = deadline_in(&starved, 1, 0)?;
    refused(
        starved.call(
            nr::ADDRESS_WAIT,
            &[WORD, 2, u64::from(u32::MAX), 0, past, 0],
        ),
        status::NO_MEMORY,
        "addrwait: a wait beyond its job's memory limit was listed",
        report,
    )?;
    let before = used(&sibling);
    let past = deadline_in(&fed, 1, 0)?;
    refused(
        fed.call(
            nr::ADDRESS_WAIT,
            &[WORD, 2, u64::from(u32::MAX), 0, past, 0],
        ),
        status::TIMED_OUT,
        "addrwait: a job at its limit held back its sibling's wait",
        report,
    )?;
    if used(&sibling) != before {
        return Err("addrwait: a wait's charge was not given back when it ended");
    }
    // The charge is taken while the waiter sleeps.
    let deadline = deadline_in(&fed, 0, PATIENCE_NANOS)?;
    start(
        0,
        &fed,
        nr::ADDRESS_WAIT,
        [WORD, 2, u64::from(u32::MAX), 0, deadline, 0],
    )?;
    until_listed(&[private(&fed, WORD)], 1)?;
    let held = used(&sibling);
    let _ = fed.call(nr::ADDRESS_WAKE, &[WORD, 1, u64::from(u32::MAX), 0, 0, 0]);
    if answer(0, PATIENCE_NANOS) != Some(Ok(0)) || held <= before {
        return Err("addrwait: a sleeping wait was not charged to its job");
    }
    report.woken += 1;
    for side in [&starved, &fed] {
        process::kill(&side.process, KILLED_STATUS);
        side.close_everything();
    }
    Ok(())
}

/// A check process moved into `job`.
fn member_of(job: &Arc<Job>) -> Result<Side, &'static str> {
    let side = Side::new()?;
    side.process
        .move_new_to(job)
        .map_err(|_| "addrwait: a job refused a process")?;
    Ok(side)
}

/// A call the ABI refuses: its number, registers, status and the sentence
/// the check fails with if it is not refused so.
type Refusal = (usize, [u64; 6], Errno, &'static str);

/// What `address_wait` refuses, `vmo` a VMO handle of one page and
/// `channel` a channel handle.
fn wait_refusals(vmo: Handle, channel: Handle) -> [Refusal; 8] {
    let all = u64::from(u32::MAX);
    [
        (
            nr::ADDRESS_WAIT,
            [WORD, 0, all, 1 << 3, 0, 0],
            status::INVALID_ARGS,
            "addrwait: a wait with K11's flag was accepted",
        ),
        (
            nr::ADDRESS_WAIT,
            [WORD, 0, 0, 0, 0, 0],
            status::INVALID_ARGS,
            "addrwait: a wait with a zero bitset was accepted",
        ),
        (
            nr::ADDRESS_WAIT,
            [WORD + 2, 0, all, 0, 0, 0],
            status::INVALID_ARGS,
            "addrwait: a misaligned wait was accepted",
        ),
        (
            nr::ADDRESS_WAIT,
            [WORD, 1 << 32, all, 0, 0, 0],
            status::INVALID_ARGS,
            "addrwait: a wait's 33-bit value was cut",
        ),
        (
            nr::ADDRESS_WAIT,
            [PAGE_SIZE, 0, all, ADDRESS_BY_VMO, 0, reg(vmo)],
            status::INVALID_ARGS,
            "addrwait: a wait past its VMO's end was accepted",
        ),
        (
            nr::ADDRESS_WAIT,
            [0, 0, all, ADDRESS_BY_VMO, 0, reg(channel)],
            status::WRONG_TYPE,
            "addrwait: a wait by a handle to a channel was accepted",
        ),
        (
            nr::ADDRESS_WAIT,
            [0, 0, all, ADDRESS_BY_VMO, 0, 0x7FFF_0001],
            status::BAD_HANDLE,
            "addrwait: a wait by a dead handle was accepted",
        ),
        (
            nr::ADDRESS_WAIT,
            [0x10, 0, all, 0, 0, 0],
            status::FAULT,
            "addrwait: a wait on an unmapped word did not fault",
        ),
    ]
}

/// What the other three calls refuse.
fn other_refusals() -> [Refusal; 7] {
    let all = u64::from(u32::MAX);
    [
        (
            nr::ADDRESS_WAKE,
            [WORD, 1, 0, 0, 0, 0],
            status::INVALID_ARGS,
            "addrwait: a wake with a zero bitset was accepted",
        ),
        (
            nr::ADDRESS_WAKE,
            [WORD, 1, all, ADDRESS_COMPARE, 0, 0],
            status::INVALID_ARGS,
            "addrwait: a wake with requeue's flag was accepted",
        ),
        (
            nr::ADDRESS_REQUEUE,
            [WORD, WORD_B, 0, 1, ADDRESS_BY_VMO, 0],
            status::INVALID_ARGS,
            "addrwait: a requeue by handle was accepted",
        ),
        (
            nr::ADDRESS_REQUEUE,
            [WORD, WORD_B + 1, 0, 1, 0, 0],
            status::INVALID_ARGS,
            "addrwait: a misaligned requeue was accepted",
        ),
        (
            nr::ADDRESS_WAIT_ANY,
            [VECTOR, 0, 0, 0, 0, 0],
            status::INVALID_ARGS,
            "addrwait: a wait on no words was accepted",
        ),
        (
            nr::ADDRESS_WAIT_ANY,
            [VECTOR, 129, 0, 0, 0, 0],
            status::INVALID_ARGS,
            "addrwait: a wait on 129 words was accepted",
        ),
        (
            nr::ADDRESS_WAIT_ANY,
            [VECTOR, 1, ADDRESS_SHARED, 0, 0, 0],
            status::INVALID_ARGS,
            "addrwait: a wait-any with call flags was accepted",
        ),
    ]
}

/// Case 12: what the calls refuse.
///
/// Verifies: L.object.203, L.object.204, L.object.205, L.object.206
fn the_calls_refuse_what_the_abi_refuses(
    a: &Side,
    report: &mut Report,
) -> Result<(), &'static str> {
    let vmo = a.handle(nr::VMO_CREATE, &[PAGE_SIZE], "addrwait: vmo_create failed")?;
    let (channel, _) = {
        let _ = a
            .call(nr::CHANNEL_CREATE, &[SCRATCH + 0xC00])
            .map_err(|_| "addrwait: channel_create failed")?;
        (Handle(a.get_u32(SCRATCH + 0xC00)?), ())
    };
    for (number, args, wanted, what) in wait_refusals(vmo, channel)
        .into_iter()
        .chain(other_refusals())
    {
        refused(a.call(number, &args), wanted, what, report)?;
    }
    // A vector word with a flag other than SHARED.
    let mut element = [0_u8; 16];
    element[..8].copy_from_slice(&WORD.to_ne_bytes());
    element[12..16].copy_from_slice(&(ADDRESS_BY_VMO as u32).to_ne_bytes());
    a.put(VECTOR, &element)?;
    refused(
        a.call(nr::ADDRESS_WAIT_ANY, &[VECTOR, 1, 0, 0]),
        status::INVALID_ARGS,
        "addrwait: a wait-any word by handle was accepted",
        report,
    )?;
    Ok(())
}

/// Case 13: a wake that looks through a bucket of 1,024 entries, none its
/// own, is timed; once they leave, the bucket gives its room back.
///
/// Verifies: L.user.138, L.user.144
#[cfg(ferrix_checks = "on")]
fn a_walk_is_measured_and_its_room_given_back(a: &Side) -> Result<u64, &'static str> {
    const ENTRIES: usize = 1024;
    let bucket = address_wait::bucket_of(&private(a, WORD));
    let mut stale = alloc::vec::Vec::new();
    crate::fallible::try_reserve(&mut stale, ENTRIES)
        .map_err(|_| "addrwait: no memory for the walk")?;
    let mut at = WORD;
    while stale.len() < ENTRIES {
        at = at.wrapping_add(4);
        let key = private(a, at);
        if address_wait::bucket_of(&key) != bucket || at == WORD {
            continue;
        }
        let entry = address_wait::sabotage::list_stale_entry(key)
            .map_err(|()| "addrwait: a dormant entry could not be listed")?;
        // NOALLOC: reserved above.
        let _ = crate::fallible::push_within(&mut stale, entry);
    }
    let started = crate::timer::now_nanos();
    let woken = address_wait::wake(&private(a, WORD), 1, address_wait::MATCH_ANY);
    let took = crate::timer::now_nanos().saturating_sub(started);
    if woken != 0 {
        return Err("addrwait: a wake over dormant entries roused one");
    }
    let (len, _) = address_wait::bucket_room(bucket);
    if len < ENTRIES {
        return Err("addrwait: a dormant entry left its bucket");
    }
    for entry in stale.drain(..) {
        let _ = entry.remove();
    }
    let (len, room) = address_wait::bucket_room(bucket);
    if room > address_wait::MIN_ROOM.max(len.saturating_mul(4)) {
        return Err("addrwait: a bucket kept its room after its entries left");
    }
    Ok(took / ENTRIES as u64)
}

/// Case 13 needs the sabotage hooks, which a kernel built `checks=off` has
/// not; such a kernel runs no check at all.
#[cfg(not(ferrix_checks = "on"))]
fn a_walk_is_measured_and_its_room_given_back(_: &Side) -> Result<u64, &'static str> {
    Ok(0)
}
