//! Finding F-23's negative control: allocations fail, on the paths a program
//! drives, and the kernel carries on.
//!
//! The certified item allocates through `crate::fallible`, which reports a
//! failure as an error its caller turns into `NO_MEMORY` or `ENOMEM`. That is
//! a claim about every caller, and a gate that reads the source
//! (`tools/common/check/check-fallible-alloc.py`) can say only that the calls are the
//! fallible ones, not that a failure is handled well where it lands. This
//! runs the failure. Two parts:
//!
//! * **The reserve serves.** An `Arc` and a map insert cannot report failure
//!   once they have started; they run inside a reserved section, which fills
//!   this processor's reserve first. With the heap made to refuse every
//!   allocation inside a section, both must still complete, on the reserve
//!   alone -- and with the reserve refused its filling, both must fail before
//!   they start.
//! * **The native ABI survives.** One process drives a round of native calls
//!   that allocate -- make a VMO, a port, a channel and a job, duplicate a
//!   handle, register on a port, write a message carrying a handle, wait on
//!   the port, read the message, write the VMO, close everything -- over and
//!   over while every `n`th fallible allocation of this task is made to fail,
//!   for several `n`. Every call must succeed or answer `NO_MEMORY` (or the
//!   status that follows from an earlier call having failed); at least one
//!   must have been refused for memory, and at least one allowed through;
//!   the machine must still be here; and afterwards, with nothing failing, a
//!   round must succeed whole and the rounds must have leaked no frame.
//! * **Taking pages away needs no memory.** A decommit cannot refuse, so with
//!   every fallible allocation failing it must still give every page back:
//!   more pages than one chunk from the stack holds, from an object a
//!   process maps, so that phase two asks the space with no list to keep it
//!   in -- and no translation to a page it gave back may be left.
//! * **A close needs no memory.** A close whose object would be queued for
//!   disposal, with the queue refused its growth, drops the object where it
//!   is, and gives nothing up.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_vma::VmaFlags;

use ferrix_linux_abi::errno::Errno;
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::nr;
use ferrix_native_abi::rights::{Rights, SAME_RIGHTS};
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::status;

use crate::fallible;
use crate::mm;
use crate::object::check::{SCRATCH, Side, reg};
use crate::object::job::Job;
use crate::object::{self, Object};
use crate::user::space::Access;
use crate::user::vmo::Vmo;

/// Where each call's user memory is, in the side's scratch region.
const PAIR: u64 = SCRATCH + 0x400;
/// The message's bytes.
const PAYLOAD: u64 = SCRATCH + 0x500;
/// The handle a message carries, going out.
const SENT: u64 = SCRATCH + 0x600;
/// Where a read puts what arrives.
const INBOX: u64 = SCRATCH + 0x700;
/// Where a read puts the handles that arrive.
const RECEIVED: u64 = SCRATCH + 0x800;
/// Where a read reports what it delivered.
const ACTUAL: u64 = SCRATCH + 0x880;
/// A registration's key, and a VMO offset.
const WORDS: u64 = SCRATCH + 0x900;
/// A port wait's deadline: one nanosecond after boot, long past.
const DEADLINE: u64 = SCRATCH + 0x910;
/// Where a port wait puts its packet.
const PACKET: u64 = SCRATCH + 0x940;

/// Every how many fallible allocations one is failed, a pass each. Primes, so
/// that the failures land on a different call of the round each time.
const PERIODS: [u32; 6] = [2, 3, 5, 7, 11, 13];

/// Rounds per period.
const ROUNDS: usize = 12;

/// Bytes in the message a round writes.
const MESSAGE: u64 = 16;

/// Pages in the object the teardown part maps. Every other one is committed:
/// more than two of `Vmo`'s stack chunks, in more runs than phase two is
/// told about one by one.
const TORN_PAGES: u64 = 160;

/// What the check did, for the boot line.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Report {
    /// Native calls made with allocations failing.
    pub(crate) calls: u32,
    /// Of those, answered `NO_MEMORY`.
    pub(crate) refused: u32,
    /// Allocations the injection failed.
    pub(crate) injected: u64,
    /// Allocations served from a reserve while the heap refused.
    pub(crate) drawn: u64,
    /// Pages a decommit gave back with every allocation failing.
    pub(crate) torn_down: usize,
}

/// Run both parts.
///
/// Verifies: L.object.7, H.MEM.11
pub(crate) fn run() -> Result<Report, &'static str> {
    let drawn = check_a_section_completes_on_the_reserve()?;
    let drawn = drawn.saturating_add(check_a_library_insert_runs_in_a_section()?);
    let mut report = check_the_native_calls_survive()?;
    report.drawn = drawn;
    report.torn_down = check_a_teardown_needs_no_memory()?;
    check_a_close_without_queue_room_drops_in_place()?;
    let frame = mm::allocate_frames(0).ok_or("no frame for the sweep's window")?;
    let swept = crate::user::alloc_check::run(frame);
    mm::deallocate_frames(frame, 0);
    let swept = swept?;
    crate::console::println!(
        "  sweep    {} memory operations run once per allocation each makes, that one failed: \
         {} failures met, {} absorbed, the rest answered as running out of memory; nothing kept",
        swept.scenarios,
        swept.failed,
        swept.absorbed,
    );
    if object::abandoned() != 0 {
        return Err("an object was given up rather than disposed of");
    }
    Ok(report)
}

/// A close whose object is queued for disposal, with no memory for the
/// queue to grow, drops the object where it is instead: the job the handle
/// named is gone when the close returns, the one allocation the close made
/// was the one refused, and nothing is given up.
///
/// The orphan queue is emptied by taking it whole, so a close finds it with
/// no room and grows it; a close that found room left by another
/// processor's disposal made no allocation to refuse, and is tried again.
///
/// Verifies: L.object.105
fn check_a_close_without_queue_room_drops_in_place() -> Result<(), &'static str> {
    let me = crate::sched::current_id().ok_or("the checking task is not running")?;
    for _ in 0..4 {
        let side = Side::new()?;
        let job = Job::new_root().map_err(|_| "no memory for a job to close")?;
        let gone = Arc::downgrade(&job);
        let handle = side
            .process
            .with_handles(|table| table.insert(Object::Job(job), Rights::JOB))
            .map_err(|_| "no room for a job to close")?;
        fallible::inject_once(me, 1, false);
        let closed = side.call(nr::HANDLE_CLOSE, &[reg(handle)]);
        let refused = fallible::stop_injecting();
        side.close_everything();
        if closed.is_err() {
            return Err("a close with no memory for the orphan queue failed");
        }
        if gone.upgrade().is_some() {
            return Err("a job whose queueing was refused outlived its close");
        }
        if object::abandoned() != 0 {
            return Err("an object whose queueing was refused was given up, not dropped");
        }
        if refused == 1 {
            return Ok(());
        }
    }
    Err("four closes of a job never grew the orphan queue")
}

/// An `Arc`, a large `Arc` and a run of map inserts complete with the heap
/// refusing every allocation inside their sections; and fail before they
/// start when their section cannot be entered.
///
/// Verifies: L.mm.35
fn check_a_section_completes_on_the_reserve() -> Result<u64, &'static str> {
    let (drawn_before, _) = mm::reserve_counts();
    mm::bypass_heap_in_sections(true);
    let small = fallible::try_arc([7_u64; 8]);
    let large = fallible::try_arc([9_u8; 3000]);
    let mut map = BTreeMap::new();
    let mut inserted = Ok(());
    for key in 0..200_u64 {
        if let Err(error) = fallible::insert(&mut map, key, key.wrapping_mul(3)) {
            inserted = Err(error);
            break;
        }
    }
    mm::bypass_heap_in_sections(false);
    let (drawn_after, _) = mm::reserve_counts();

    let small = small.map_err(|_| "an Arc was refused although its section was entered")?;
    let large = large.map_err(|_| "a large Arc was refused although its section was entered")?;
    inserted.map_err(|_| "a map insert was refused although its section was entered")?;
    if *small != [7; 8] || large.iter().any(|&byte| byte != 9) {
        return Err("an Arc built on the reserve does not hold its value");
    }
    if map.len() != 200 || !map.iter().all(|(key, value)| *value == key.wrapping_mul(3)) {
        return Err("a map built on the reserve does not hold what was inserted");
    }
    // Two `Arc`s, and a leaf and the nodes its splits made.
    let drawn = drawn_after.saturating_sub(drawn_before);
    if drawn < 4 {
        return Err("the reserve was not drawn on while the heap refused");
    }
    drop((small, large, map));

    // A section whose reserve cannot be filled refuses before anything runs.
    let task = crate::sched::current_id().ok_or("the allocation check runs outside a task")?;
    fallible::inject(task, 1);
    let refused_arc = fallible::try_arc(1_u64).is_err();
    let mut refused_map = BTreeMap::new();
    let refused_insert = fallible::insert(&mut refused_map, 1_u8, 1_u8).is_err();
    let failed = fallible::stop_injecting();
    if !refused_arc || !refused_insert || failed < 2 || !refused_map.is_empty() {
        return Err("a section whose reserve could not be filled went ahead");
    }
    Ok(drawn)
}

/// A library crate's map insert -- `ferrix_fallible::try_map_insert`, which
/// the btrfs writer makes all its inserts through -- runs in the reserved
/// section the kernel installed at boot (`fallible::install_library_sections`):
/// with the heap refusing every allocation inside sections, a run of inserts
/// completes on the reserve alone; with the reserve refused its filling, an
/// insert fails before it starts and leaves the map as it was; and a map
/// whose nodes are larger than any size class is refused outright. A kernel
/// that never installed the section fails all three: the inserts would run
/// on the heap, outside any section, and could not fail at all.
fn check_a_library_insert_runs_in_a_section() -> Result<u64, &'static str> {
    let (drawn_before, _) = mm::reserve_counts();
    mm::bypass_heap_in_sections(true);
    let mut map = BTreeMap::new();
    let mut inserted = Ok(());
    for key in 0..200_u64 {
        if let Err(error) = ferrix_fallible::try_map_insert(&mut map, key, !key) {
            inserted = Err(error);
            break;
        }
    }
    mm::bypass_heap_in_sections(false);
    let (drawn_after, _) = mm::reserve_counts();
    inserted.map_err(|_| "a library map insert was refused although its section was entered")?;
    if map.len() != 200 || !map.iter().all(|(key, value)| *value == !key) {
        return Err("a library map built on the reserve does not hold what was inserted");
    }
    let drawn = drawn_after.saturating_sub(drawn_before);
    if drawn < 2 {
        return Err("a library map insert did not run in a reserved section");
    }

    let (_, refused_before) = mm::reserve_counts();
    // Preemption off, so the refusal and the insert are on one processor.
    crate::sched::preempt_disable();
    mm::refuse_reserve_fills(true);
    let refused = ferrix_fallible::try_map_insert(&mut map, 1000, 0);
    mm::refuse_reserve_fills(false);
    crate::sched::preempt_enable();
    let (_, refused_after) = mm::reserve_counts();
    if refused.is_ok() || refused_after <= refused_before || map.len() != 200 {
        return Err("a library map insert went ahead in a section that could not be entered");
    }

    let mut wide: BTreeMap<u64, [u8; 256]> = BTreeMap::new();
    if ferrix_fallible::try_map_insert(&mut wide, 1, [0; 256]).is_ok() || !wide.is_empty() {
        return Err("a library map whose nodes no size class holds was not refused");
    }
    Ok(drawn)
}

/// Every other page of a mapped object, committed through faults and given
/// back by a decommit with every fallible allocation failing: all of them
/// back, none still translated, no frame kept. How many pages went.
///
/// Verifies: L.user.19
fn check_a_teardown_needs_no_memory() -> Result<usize, &'static str> {
    let side = Side::new()?;
    let vmo = Vmo::new_anonymous(TORN_PAGES).map_err(|_| "no memory for the teardown's VMO")?;
    let space = side.process.space();
    let at = space
        .map_object(
            None,
            TORN_PAGES * PAGE_SIZE,
            Arc::clone(&vmo),
            0,
            VmaFlags::READ_WRITE,
        )
        .map_err(|_| "could not map the teardown's VMO")?;
    let touch = || -> Result<(), &'static str> {
        for index in (0..TORN_PAGES).step_by(2) {
            space
                .fault(at + index * PAGE_SIZE, Access::WRITE)
                .map_err(|_| "could not fault in a page of the teardown's VMO")?;
        }
        Ok(())
    };
    // Once before the window, for the tables the faults need, which stay.
    touch()?;
    let _ = vmo.decommit_range(0, TORN_PAGES);
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    touch()?;

    let task = crate::sched::current_id().ok_or("the allocation check runs outside a task")?;
    fallible::inject(task, 1);
    let given = vmo.decommit_range(0, TORN_PAGES);
    let failed = fallible::stop_injecting();

    let still_mapped = (0..TORN_PAGES).step_by(2).any(|index| {
        space
            .with_present_page(at + index * PAGE_SIZE, Access::READ, |_| ())
            .is_ok_and(|present| present.is_some())
    });
    if failed == 0 {
        return Err("the decommit met no failing allocation");
    }
    if given != (TORN_PAGES / 2) as usize || vmo.committed() != 0 {
        return Err("a decommit with no memory did not give every page back");
    }
    if still_mapped {
        return Err("a decommit with no memory left a page translated");
    }
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    if window.kept() != 0 {
        window.report("teardown with no memory");
        return Err("a decommit with no memory kept frames");
    }
    space
        .unmap(at, TORN_PAGES * PAGE_SIZE)
        .map_err(|_| "could not unmap the teardown's VMO")?;
    side.close_everything();
    Ok(given)
}

/// The handles one round has made and not yet closed.
#[derive(Debug, Default)]
struct Round {
    /// Every handle made, to close at the end.
    open: Vec<Handle>,
}

/// Rounds of native calls with allocations failing, then one without.
fn check_the_native_calls_survive() -> Result<Report, &'static str> {
    let side = Side::new()?;
    let root = Job::new_root().map_err(|_| "no memory for the check's job")?;
    let root = side
        .process
        .with_handles(|table| table.insert(Object::Job(root), Rights::JOB))
        .map_err(|_| "no room for the check's job")?;
    side.put(PAYLOAD, &[0xA5; MESSAGE as usize])?;
    side.put(WORDS, &0_u64.to_ne_bytes())?;
    side.put(DEADLINE, &1_u64.to_ne_bytes())?;

    let task = crate::sched::current_id().ok_or("the allocation check runs outside a task")?;
    // One round before the window, for the size classes and the table's first
    // slots, which stay.
    let mut tally = Report::default();
    let mut warm = Report::default();
    round(&side, root, &mut warm, false)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    for period in PERIODS {
        fallible::inject(task, period);
        let mut outcome = Ok(());
        for _ in 0..ROUNDS {
            outcome = round(&side, root, &mut tally, true);
            if outcome.is_err() {
                break;
            }
        }
        tally.injected += fallible::stop_injecting();
        outcome?;
    }
    // Nothing failing: every call of a round succeeds, so nothing the failures
    // left behind stands in the way.
    let mut clean = Report::default();
    round(&side, root, &mut clean, false)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let leaked = window.kept();
    if leaked != 0 {
        mm::print_frame_delta("allocation failure", leaked);
        window.report("allocation failure");
        return Err("rounds with allocations failing did not give every frame back");
    }
    side.close_everything();

    if tally.refused == 0 || tally.injected == 0 {
        return Err("no native call was refused for memory while allocations failed");
    }
    if tally.refused >= tally.calls {
        return Err("every native call was refused while allocations failed");
    }
    Ok(tally)
}

/// One round of calls. With `failing`, a call may answer `NO_MEMORY`, or what
/// follows from an earlier one having done so; without, every call must
/// succeed.
///
/// Verifies: L.object.30
fn round(side: &Side, root: Handle, tally: &mut Report, failing: bool) -> Result<(), &'static str> {
    let mut made = Round::default();
    let outcome = calls(side, root, tally, failing, &mut made);
    // A port's promises must have held: a packet with no room is one lost.
    let lost: u64 = made
        .open
        .iter()
        .filter_map(|&handle| {
            side.process.with_handles(|table| match table.get(handle) {
                Ok((Object::Port(port), _)) => Some(port.lost()),
                _ => None,
            })
        })
        .sum();
    for handle in made.open {
        let _ = side.call(nr::HANDLE_CLOSE, &[reg(handle)]);
    }
    if lost != 0 {
        return Err("a port found no room for a packet its registration had promised");
    }
    outcome
}

/// Makes the calls of a round, and says whether each outcome is allowed.
struct Caller<'a> {
    /// The process calling.
    side: &'a Side,
    /// What the calls came to.
    tally: &'a mut Report,
    /// Whether allocations are failing.
    failing: bool,
}

impl Caller<'_> {
    /// Make one call: its value if it succeeded, `None` if it was refused in
    /// a way `failing` allows -- `NO_MEMORY`, or one of `also`, which follow
    /// from an earlier call having been refused -- and `what` otherwise.
    fn call(
        &mut self,
        number: usize,
        args: &[u64],
        also: &[Errno],
        what: &'static str,
    ) -> Result<Option<usize>, &'static str> {
        self.tally.calls += 1;
        match self.side.call(number, args) {
            Ok(value) => Ok(Some(value)),
            Err(status::NO_MEMORY) if self.failing => {
                self.tally.refused += 1;
                Ok(None)
            }
            Err(other) if self.failing && also.contains(&other) => Ok(None),
            Err(other) => {
                crate::console::println!("  no-mem   {what}: status {}", other.0);
                Err(what)
            }
        }
    }

    /// Make a call that answers a handle, and note the handle in `made`.
    fn handle(
        &mut self,
        number: usize,
        args: &[u64],
        what: &'static str,
        made: &mut Round,
    ) -> Result<Option<Handle>, &'static str> {
        let value = self.call(number, args, &[], what)?;
        let handle = value
            .and_then(|value| u32::try_from(value).ok())
            .map(Handle);
        if let Some(handle) = handle {
            made.open.push(handle);
        }
        Ok(handle)
    }
}

/// What a round made, where the call making it succeeded.
#[derive(Debug, Default, Clone, Copy)]
struct Objects {
    /// A VMO.
    vmo: Option<Handle>,
    /// A port.
    port: Option<Handle>,
    /// A channel's end the round writes into.
    writer: Option<Handle>,
    /// Its other end, which the round reads.
    reader: Option<Handle>,
}

/// The calls of [`round`], recording every handle made in `made`.
fn calls(
    side: &Side,
    root: Handle,
    tally: &mut Report,
    failing: bool,
    made: &mut Round,
) -> Result<(), &'static str> {
    let mut caller = Caller {
        side,
        tally,
        failing,
    };
    let objects = make(&mut caller, root, made)?;
    exchange(&mut caller, objects, made)
}

/// Make a VMO, a port, a job and a channel.
fn make(caller: &mut Caller<'_>, root: Handle, made: &mut Round) -> Result<Objects, &'static str> {
    let vmo = caller.handle(
        nr::VMO_CREATE,
        &[4096],
        "vmo_create failed with allocations failing",
        made,
    )?;
    let port = caller.handle(
        nr::PORT_CREATE,
        &[],
        "port_create failed with allocations failing",
        made,
    )?;
    let _job = caller.handle(
        nr::JOB_CREATE,
        &[reg(root)],
        "job_create failed with allocations failing",
        made,
    )?;
    let pair = caller.call(
        nr::CHANNEL_CREATE,
        &[PAIR],
        &[],
        "channel_create failed with allocations failing",
    )?;
    let (writer, reader) = if pair.is_some() {
        let ends = (
            Handle(caller.side.get_u32(PAIR)?),
            Handle(caller.side.get_u32(PAIR + 4)?),
        );
        made.open.push(ends.0);
        made.open.push(ends.1);
        (Some(ends.0), Some(ends.1))
    } else {
        (None, None)
    };
    Ok(Objects {
        vmo,
        port,
        writer,
        reader,
    })
}

/// Duplicate the VMO's handle, register on the port for the channel, write a
/// message carrying the duplicate, wait on the port, read the message, and
/// write the VMO: whichever of those the objects made allow.
fn exchange(
    caller: &mut Caller<'_>,
    objects: Objects,
    made: &mut Round,
) -> Result<(), &'static str> {
    let copy = match objects.vmo {
        Some(vmo) => caller.call(
            nr::HANDLE_DUPLICATE,
            &[reg(vmo), u64::from(SAME_RIGHTS)],
            &[],
            "handle_duplicate failed with allocations failing",
        )?,
        None => None,
    };
    let copy = copy.and_then(|value| u32::try_from(value).ok()).map(Handle);

    if let (Some(reader), Some(port)) = (objects.reader, objects.port) {
        let _ = caller.call(
            nr::OBJECT_WAIT_ASYNC,
            &[
                reg(reader),
                reg(port),
                u64::from(Signals::READABLE.0),
                WORDS,
            ],
            &[],
            "object_wait_async failed with allocations failing",
        )?;
    }

    let sent = send(caller, objects.writer, copy, made)?;

    if let Some(port) = objects.port {
        // Empty if the registration or the write failed.
        let _ = caller.call(
            nr::PORT_WAIT,
            &[reg(port), DEADLINE, PACKET],
            &[status::TIMED_OUT],
            "port_wait failed with allocations failing",
        )?;
    }

    if let Some(reader) = objects.reader {
        let read = caller.call(
            nr::CHANNEL_READ,
            &[reg(reader), INBOX, MESSAGE, RECEIVED, 4, ACTUAL],
            // Nothing to read if the write failed.
            &[status::SHOULD_WAIT],
            "channel_read failed with allocations failing",
        )?;
        if read.is_some() && sent && copy.is_some() {
            made.open.push(Handle(caller.side.get_u32(RECEIVED)?));
        }
    }

    if let Some(vmo) = objects.vmo {
        let _ = caller.call(
            nr::VMO_WRITE,
            &[reg(vmo), PAYLOAD, MESSAGE, WORDS],
            &[],
            "vmo_write failed with allocations failing",
        )?;
    }
    Ok(())
}

/// Write the round's message through `writer`, carrying `copy` if there is
/// one. Returns whether it went; a copy that did not go stays with the
/// writer, and is noted in `made` to be closed.
fn send(
    caller: &mut Caller<'_>,
    writer: Option<Handle>,
    copy: Option<Handle>,
    made: &mut Round,
) -> Result<bool, &'static str> {
    let mut sent = false;
    if let Some(writer) = writer {
        if let Some(copy) = copy {
            caller.side.put(SENT, &copy.0.to_ne_bytes())?;
        }
        let written = caller.call(
            nr::CHANNEL_WRITE,
            &[
                reg(writer),
                PAYLOAD,
                MESSAGE,
                SENT,
                u64::from(copy.is_some()),
            ],
            &[],
            "channel_write failed with allocations failing",
        )?;
        sent = written.is_some();
    }
    if !sent && let Some(copy) = copy {
        made.open.push(copy);
    }
    Ok(sent)
}

/// `defer` gives an object up, counted, only past [`object::IN_PLACE_DEPTH`]
/// drops in place, and the kernel runs on: a chain of channel ends, each
/// queued unread in the one before's inbox, closed from its outermost end
/// with every allocation failing, so that each end the close defers is
/// dropped in place inside the drop of the one before, until the one past
/// the depth is given up. How many were given up: one.
///
/// The one check that loses memory on purpose -- the end given up, and its
/// channel, are never freed -- so it runs last of all, after every check
/// that counts what the heap or the frame allocator gave back. A checked
/// boot keeps that one object for its whole life, and [`object::abandoned`]
/// reads 1 after the marker. Nothing after the marker reads it, and the
/// leak checks that run later count what their own work gave back, from
/// before it to after, which the object is outside.
///
/// Verifies: L.object.8
pub(crate) fn give_up() -> Result<u64, &'static str> {
    // `dispose` closes an end at once and defers only what its messages
    // carry, so down the chain every other end is deferred: the outermost
    // closed, then a deferred one dropped in place and the one it carries
    // closed at once, `IN_PLACE_DEPTH` times, and the last deferred one
    // given up, with nothing past it.
    const ENDS: usize = 2 * object::IN_PLACE_DEPTH + 2;
    let me = crate::sched::current_id().ok_or("the checking task is not running")?;
    if object::abandoned() != 0 {
        return Err("an object was given up before the give-up check");
    }
    let side = Side::new()?;
    let mut ends = Vec::new();
    for _ in 0..ENDS {
        let _ = side
            .call(nr::CHANNEL_CREATE, &[PAIR])
            .map_err(|_| "channel_create failed for the give-up chain")?;
        let writer = Handle(side.get_u32(PAIR)?);
        let reader = Handle(side.get_u32(PAIR + 4)?);
        ends.push((writer, reader));
    }
    // Each reader after the first goes into the inbox of the one before.
    for pair in ends.windows(2) {
        let [(writer, _), (_, next)] = pair else {
            continue;
        };
        side.put(SENT, &next.0.to_ne_bytes())?;
        let _ = side
            .call(nr::CHANNEL_WRITE, &[reg(*writer), PAYLOAD, 0, SENT, 1])
            .map_err(|_| "a write into the give-up chain failed")?;
    }
    for (writer, _) in &ends {
        let _ = side.call(nr::HANDLE_CLOSE, &[reg(*writer)]);
    }
    let (_, outermost) = *ends.first().ok_or("the give-up chain is empty")?;
    fallible::inject(me, 1);
    let closed = side.call(nr::HANDLE_CLOSE, &[reg(outermost)]);
    let _ = fallible::stop_injecting();
    side.close_everything();
    if closed.is_err() {
        return Err("a close with every allocation failing failed");
    }
    let given_up = object::abandoned();
    if given_up != 1 {
        return Err("a chain one past the in-place depth did not give up exactly one object");
    }
    Ok(given_up)
}
