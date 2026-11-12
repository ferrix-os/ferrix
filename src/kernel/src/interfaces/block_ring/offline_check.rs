//! An offline disk sends nothing more and never changes an answer
//! (`docs/BLOCK-RING.md` §6.3, "A disk that stops answering"; the
//! certification consultant's review, ledger 4290 (2)).
//!
//! `test-disk-offline` shows the machine answering once a stalled disk is
//! offline, but it cannot show these two rules: btrfs waits for each write,
//! so nothing is ever queued behind the one on the ring when the disk goes,
//! and nothing answers a request twice. So this plays the ring's task and
//! the driver itself, on a disk of its own that no ring serves: it queues
//! more writes than the disk's depth lets onto its ring, takes the first two
//! off the queue as a driver would hold them, and lets the disk go offline
//! with a patience of 0. Then it requires:
//!
//! * every request answered EIO, and a new one refused;
//! * the drain to empty the queue -- the commands held and the commands
//!   still queued all answered -- with nothing left to dispatch, and the
//!   disk not dispatchable (stopped dispatch: a command answered as failed
//!   is never sent later);
//! * a completion of a held command that comes between the disk going
//!   offline and the drain -- the queue still knowing it -- and late ones
//!   after the drain, as a driver that recovered would send, to leave every
//!   answer EIO (an answer is never replaced);
//! * a successor taking the disk up to find it online with nothing to
//!   dispatch.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_blkring::{Device, DeviceFlags, DiskName};
use ferrix_block::{Limits, Request, Token};
use ferrix_vfs::Errno;

use super::{MAX_PARTS, Payload, PayloadReserve, RingDisk, answer, ticks};
use crate::object::port::Port;

/// One sector of 512 bytes per command, a depth of two on the ring, and
/// four writes: two held on the "ring", two left queued.
const BLOCK: u32 = 512;
const DEPTH: u32 = 2;
const WRITES: u64 = 4;

/// Run the check.
///
/// # Errors
///
/// Which rule did not hold.
pub(super) fn run() -> Result<(), &'static str> {
    let device = Device::new(BLOCK, 64, 1, DeviceFlags::default(), u64::from(BLOCK) * 4)
        .map_err(|_| "offline-check: no device")?;
    let limits =
        Limits::new(BLOCK, 64, 1, MAX_PARTS, DEPTH).map_err(|_| "offline-check: no limits")?;
    let name = DiskName::for_index(25).ok_or("offline-check: no name")?;
    let port = Port::new().map_err(|_| "offline-check: no port")?;
    let reserve = PayloadReserve::new(BLOCK as usize).ok_or("offline-check: no reserve")?;
    let disk = Arc::new(RingDisk::new(
        device,
        name,
        limits,
        Arc::clone(&port),
        reserve,
    ));
    let mut ids = Vec::new();
    for at in 0..WRITES {
        // Sectors apart, so the queue merges none of them.
        let request = Request::write(0, at * 2, 1).sync();
        let id = disk
            .submit(request, Some(Payload::Heap(vec![0xa5; BLOCK as usize])), 0)
            .map_err(|_| "offline-check: a write was not queued")?;
        ids.push(id);
    }
    // The ring's task puts two on the ring, and the "driver" holds them.
    let held: Vec<u64> = {
        let mut state = disk.state.lock();
        let mut held = Vec::new();
        while let Some(command) = state.queue.dispatch(ticks()) {
            held.push(command.token.raw());
        }
        held
    };
    if held.len() != DEPTH as usize {
        return Err("offline-check: the queue did not put the disk's depth on the ring");
    }
    disk.offline_if_silent(0);
    if !disk.state.lock().offline {
        return Err("offline-check: a disk silent past its patience was not taken offline");
    }
    if disk
        .submit(
            Request::write(0, 60, 1).sync(),
            Some(Payload::Heap(vec![0; BLOCK as usize])),
            0,
        )
        .is_ok()
    {
        return Err("offline-check: an offline disk took a new request");
    }
    {
        let mut state = disk.state.lock();
        // The driver completes the first command it held, successfully, in
        // the moment between the disk going offline and the task's drain:
        // the queue still knows it, and its caller was answered EIO already.
        let late = held.first().copied().ok_or("offline-check: nothing held")?;
        let _ = answer(&mut state, Token::from_raw(late), Ok(()), None);
        if !state.drain_offline(held.iter().copied().skip(1)) {
            return Err("offline-check: the drain did not see the disk offline");
        }
        if state.queue.outstanding() != 0 || state.queue.dispatch(ticks()).is_some() {
            return Err(
                "offline-check: an offline disk's queue kept a command that could be sent later",
            );
        }
        if state.dispatchable() {
            return Err("offline-check: an offline disk was dispatchable");
        }
        // And the rest of what it held, after the drain.
        for &token in held.iter().skip(1) {
            let _ = answer(&mut state, Token::from_raw(token), Ok(()), None);
        }
    }
    for &id in &ids {
        if disk.collect(id, &mut []) != Err(Errno::EIO) {
            return Err(
                "offline-check: a request answered EIO when the disk went offline was answered \
                 again",
            );
        }
    }
    // A new driver takes the disk up.
    disk.take_up(port);
    let mut state = disk.state.lock();
    if state.offline {
        return Err("offline-check: a disk taken up by a new driver stayed offline");
    }
    if state.queue.dispatch(ticks()).is_some() || state.queue.outstanding() != 0 {
        return Err("offline-check: the disk's successor found a command to send");
    }
    Ok(())
}

// The write reserve's check (L.device.86-87), beside this one: the same
// kind of disk, of its own, that no ring serves.
pub(super) mod reserve {
    //! A write's payload when the heap refuses it (L.device.86) and a request
    //! the requests map has no room for (L.device.87): the block ring's write
    //! reserve (`docs/BTRFS-PRESSURE.md`, "Against Linux's btrfs", W4; the
    //! certification consultant's design review, ledger 8275).
    //!
    //! `RingDisk::write` copied every request's bytes into the heap with an
    //! allocation that could not fail, 128 KiB at a time on NVMe, a candidate
    //! for the N150's FX-0008 "memory allocation of 131072 bytes failed". So
    //! this plays the writer, the ring's task and the driver, as
    //! `offline_check` does, on a disk of its own that no ring serves, with the
    //! heap's refusals made by the allocation check's injection on this task
    //! alone, and requires:
    //!
    //! * (i) a payload the heap refused to come from the reserve, with the
    //!   caller's bytes, and its request queued;
    //! * (ii) with both elements lent to requests the "driver" holds, a third
    //!   payload to answer ENOMEM once its wait is over, and not long after;
    //! * (iii) once a held request is answered and collected, its element back
    //!   and a payload the heap refuses served from it again;
    //! * (iv) every element back -- lent and free together always the
    //!   reserve's size -- after each way a request leaves: collected, and
    //!   abandoned and then answered;
    //! * (v) a request whose entry the requests map has no room for refused
    //!   with ENOMEM, nothing queued, its element back.
    //! * (vi) with both elements lent and a writer of its own task waiting,
    //!   the only completion an abandoned write's: the element it gives back
    //!   reaches the writer well before the writer's deadline (ledger 8328
    //!   WR.1: the answer's wake, which an all-abandoned completion once
    //!   left empty).

    use alloc::sync::Arc;

    use ferrix_blkring::{Device, DeviceFlags, DiskName};
    use ferrix_block::{Limits, Request, Token};
    use ferrix_vfs::Errno;

    use super::super::{
        MAX_PARTS, Payload, PayloadReserve, RESERVE_ELEMENTS, RingDisk, answer, request_bytes,
        ticks,
    };
    use crate::object::port::Port;
    use crate::sched::WaitQueue;
    use crate::sync::SpinLock;
    use crate::timer;

    /// The disk's block size, and one request's bytes: the reserve's element.
    const BLOCK: u32 = 512;
    const CHUNK: usize = 4 * BLOCK as usize;

    /// How long (ii)'s writer waits for an element: short, for the boot's sake;
    /// the production wait is `RESERVE_WAIT_NANOS`, through the same code.
    const WAIT_NANOS: u64 = 20_000_000;

    /// How long past its wait (ii)'s answer may come: a wait that did not end
    /// at its deadline fails here (ledger 8275 W4.9, K2).
    const LATE_NANOS: u64 = 500_000_000;

    /// `payload_within` for `chunk`, with this task's next allocation -- the
    /// heap copy -- refused.
    fn refused_payload(disk: &RingDisk, chunk: &[u8], wait: u64) -> Result<Payload, Errno> {
        let task = crate::sched::current_id().ok_or(Errno::EIO)?;
        crate::fallible::inject_once(task, 1, false);
        let payload = disk.payload_within(chunk, wait);
        let _ = crate::fallible::stop_injecting();
        payload
    }

    /// The bytes the reserve's element `index` holds, its first `len`.
    fn element_holds(disk: &RingDisk, index: usize, bytes: &[u8]) -> bool {
        disk.reserve
            .elements
            .get(index)
            .is_some_and(|element| element.lock().get(..bytes.len()) == Some(bytes))
    }

    /// Take every queued command off the queue, as a driver holding them would
    /// have them on its ring: their tokens.
    fn hold(disk: &RingDisk) -> alloc::vec::Vec<u64> {
        let mut state = disk.state.lock();
        let mut held = alloc::vec::Vec::new();
        while let Some(command) = state.queue.dispatch(ticks()) {
            if held.try_reserve(1).is_err() {
                break;
            }
            held.push(command.token.raw());
        }
        held
    }

    /// Answer the held command `token` as done, and wake what the answer
    /// says to, as the ring's task does (`Serving::complete`: only a
    /// non-empty wake wakes anyone).
    fn complete(disk: &RingDisk, token: u64) {
        let wake = {
            let mut state = disk.state.lock();
            answer(&mut state, Token::from_raw(token), Ok(()), None)
        };
        if wake != 0 {
            disk.wake(wake);
        }
    }

    /// (vi)'s writer: the disk it waits on, and what it was answered with
    /// how long it waited.
    static WAITER_DISK: SpinLock<Option<Arc<RingDisk>>> = SpinLock::new(None);
    static WAITER_ANSWER: SpinLock<Option<(Result<Payload, Errno>, u64)>> = SpinLock::new(None);
    static WAITER_DONE: WaitQueue = WaitQueue::new();

    /// The two requests' bytes the cases write. Statics, not locals: two
    /// arrays of a request each made `run`'s frame 4504 bytes, over a page,
    /// and ARMv7-A has no stack probes (F-100, `check-frames`).
    static FIRST: [u8; CHUNK] = [0x5a; CHUNK];
    static SECOND: [u8; CHUNK] = [0xa5; CHUNK];

    /// How long (vi)'s writer may wait for an element: the production wait,
    /// so that a wake that never comes is seen as an answer a second late.
    const WAITER_WAIT_NANOS: u64 = 1_000_000_000;

    /// (vi)'s writer, a task of its own: with the heap refusing its copy,
    /// it waits for an element.
    fn waiting_writer(_: usize) {
        let disk = WAITER_DISK.lock().clone();
        let answered = match (disk, crate::sched::current_id()) {
            (Some(disk), Some(me)) => {
                let began = timer::now_nanos();
                crate::fallible::inject_once(me, 1, false);
                let payload = disk.payload_within(&[0x3c_u8; CHUNK], WAITER_WAIT_NANOS);
                let _ = crate::fallible::stop_injecting();
                (payload, timer::now_nanos().saturating_sub(began))
            }
            _ => (Err(Errno::EIO), 0),
        };
        *WAITER_ANSWER.lock() = Some(answered);
        WAITER_DONE.wake_all();
    }

    /// (vi): with both elements lent and a writer of its own task waiting,
    /// the only completion an abandoned write's gives its element back, its
    /// answer wakes the reserve's waiters, and the writer is served well
    /// before its deadline.
    fn abandoned_completion_serves_a_waiter(
        disk: &Arc<RingDisk>,
        second: &[u8; CHUNK],
    ) -> Result<(), &'static str> {
        let fourth = refused_payload(disk, second, WAIT_NANOS)
            .map_err(|_| "reserve-check: the free element was not lent for (vi)")?;
        let four = disk
            .submit(Request::write(0, 48, 4).sync(), Some(fourth), 0)
            .map_err(|_| "reserve-check: (vi)'s write was not queued")?;
        let token = *hold(disk)
            .first()
            .ok_or("reserve-check: (vi)'s write was not held")?;
        if disk.collect(four, &mut []) != Err(Errno::EIO) {
            return Err("reserve-check: (vi)'s write collected before its answer did not give up");
        }
        *WAITER_ANSWER.lock() = None;
        *WAITER_DISK.lock() = Some(Arc::clone(disk));
        let _ = crate::sched::spawn(
            "reserve-waiter",
            waiting_writer,
            0,
            ferrix_sched::NICE_0_WEIGHT,
        )?;
        // Listed on the reserve's queue before the answer, not guessed from
        // how long it was given.
        let listed_by = timer::now_nanos().saturating_add(WAITER_WAIT_NANOS / 2);
        while disk.reserve.returned.listed() == 0 && WAITER_ANSWER.lock().is_none() {
            if timer::now_nanos() >= listed_by {
                return Err("reserve-check: (vi)'s writer never waited for an element");
            }
            crate::sched::sleep_for(1_000_000);
        }
        if WAITER_ANSWER.lock().is_some() {
            return Err("reserve-check: a writer with both elements lent did not wait");
        }
        // The answer's own wake is what serves it: counted, since a bounded
        // wait also looks again of its own accord every few milliseconds.
        let wakes = disk.reserve.returned.wakes();
        complete(disk, token);
        if disk.reserve.returned.wakes() == wakes {
            return Err(
                "reserve-check: an abandoned write's answer did not wake the reserve's waiters",
            );
        }
        let deadline = timer::now_nanos().saturating_add(WAITER_WAIT_NANOS.saturating_mul(2));
        let _ = WAITER_DONE.wait_until_deadline(|| WAITER_ANSWER.lock().is_some(), deadline);
        *WAITER_DISK.lock() = None;
        let (served, waited) = WAITER_ANSWER
            .lock()
            .take()
            .ok_or("reserve-check: (vi)'s writer never came back")?;
        let Ok(Payload::Reserved { index, .. }) = served else {
            return Err(
                "reserve-check: an element an abandoned write gave back did not reach a waiting \
                 writer",
            );
        };
        if waited >= WAITER_WAIT_NANOS / 2 {
            return Err(
                "reserve-check: an element an abandoned write gave back did not reach a waiting \
                 writer",
            );
        }
        // The check gives the writer's element back itself: it submits nothing.
        disk.state.lock().reserve_free |= 1 << index;
        Ok(())
    }

    /// Run the check.
    ///
    /// # Errors
    ///
    /// Which rule did not hold.
    ///
    /// Verifies: L.device.86, L.device.87
    pub(in super::super) fn run() -> Result<(), &'static str> {
        let device = Device::new(BLOCK, 64, 4, DeviceFlags::default(), u64::from(BLOCK) * 64)
            .map_err(|_| "reserve-check: no device")?;
        if request_bytes(&device) != CHUNK {
            return Err("reserve-check: a request is not what the device takes at once");
        }
        let limits =
            Limits::new(BLOCK, 64, 4, MAX_PARTS, 4).map_err(|_| "reserve-check: no limits")?;
        let name = DiskName::for_index(24).ok_or("reserve-check: no name")?;
        let port = Port::new().map_err(|_| "reserve-check: no port")?;
        let reserve = PayloadReserve::new(CHUNK).ok_or("reserve-check: no reserve")?;
        let disk = Arc::new(RingDisk::new(device, name, limits, port, reserve));
        let all = (0, RESERVE_ELEMENTS as u32);
        if disk.reserve_counts() != all {
            return Err("reserve-check: a new disk's reserve was not all free");
        }

        // (i) The heap refuses; the reserve serves, with the caller's bytes.
        let first = &FIRST;
        let payload = refused_payload(&disk, first, WAIT_NANOS).map_err(
            |_| "reserve-check: a payload the heap refused did not come from the reserve",
        )?;
        let Payload::Reserved { index, len } = payload else {
            return Err("reserve-check: a payload the heap refused was not a reserved one");
        };
        if len != CHUNK || !element_holds(&disk, index, first) {
            return Err("reserve-check: a reserved payload does not hold the caller's bytes");
        }
        let one = disk
            .submit(Request::write(0, 0, 4).sync(), Some(payload), 0)
            .map_err(|_| "reserve-check: a write with a reserved payload was not queued")?;
        let second = &SECOND;
        let payload = refused_payload(&disk, second, WAIT_NANOS)
            .map_err(|_| "reserve-check: the second element was not lent")?;
        let two = disk
            .submit(Request::write(0, 16, 4).sync(), Some(payload), 0)
            .map_err(|_| "reserve-check: the second reserved write was not queued")?;
        if disk.reserve_counts() != (RESERVE_ELEMENTS as u32, 0) {
            return Err("reserve-check: two lent elements were not counted lent");
        }
        let held = hold(&disk);
        if held.len() != 2 {
            return Err("reserve-check: the two writes were not both put on the ring");
        }

        // (ii) Both lent and held: the third waits out its wait, then ENOMEM.
        let began = timer::now_nanos();
        let third = refused_payload(&disk, first, WAIT_NANOS);
        let waited = timer::now_nanos().saturating_sub(began);
        if !matches!(third, Err(Errno::ENOMEM)) {
            return Err(
                "reserve-check: a payload with the heap and the reserve empty was not ENOMEM",
            );
        }
        if waited < WAIT_NANOS {
            return Err("reserve-check: a write with no element answered before its wait was over");
        }
        if waited > WAIT_NANOS.saturating_add(LATE_NANOS) {
            return Err("reserve-check: a write waited past its deadline for an element");
        }

        // (iii) and (iv): the first answered and collected gives its element back.
        let token = *held.first().ok_or("reserve-check: nothing held")?;
        complete(&disk, token);
        disk.collect(one, &mut [])
            .map_err(|_| "reserve-check: a completed reserved write did not answer done")?;
        if disk.reserve_counts() != (1, 1) {
            return Err("reserve-check: a collected write did not give its element back");
        }
        let again = refused_payload(&disk, second, WAIT_NANOS)
            .map_err(|_| "reserve-check: an element given back was not lent again")?;

        // (iv) Abandoned and then answered: the second's caller gives up first.
        if disk.collect(two, &mut []) != Err(Errno::EIO) {
            return Err("reserve-check: a write collected before its answer did not give up");
        }
        let token = *held.get(1).ok_or("reserve-check: one held")?;
        complete(&disk, token);
        if disk.reserve_counts() != (1, 1) {
            return Err(
                "reserve-check: an abandoned write did not give its element back when answered",
            );
        }

        // (vi) Both lent, a writer waiting, and the only completion an
        // abandoned write's (ledger 8328 WR.1).
        abandoned_completion_serves_a_waiter(&disk, second)?;

        // (v) No room in the requests map: refused, nothing queued, element back.
        let queued = disk.state.lock().queue.queued();
        let entries = disk.state.lock().reads.len();
        let task = crate::sched::current_id().ok_or("reserve-check: no task")?;
        crate::fallible::inject_once(task, 1, false);
        let refused = disk.submit(Request::write(0, 32, 4).sync(), Some(again), 0);
        let _ = crate::fallible::stop_injecting();
        if refused != Err(Errno::ENOMEM) {
            return Err("reserve-check: a request the requests map had no room for was not ENOMEM");
        }
        if disk.state.lock().queue.queued() != queued || disk.state.lock().reads.len() != entries {
            return Err("reserve-check: a refused request left something queued or recorded");
        }
        if disk.reserve_counts() != all {
            return Err("reserve-check: a refused request did not give its element back");
        }
        Ok(())
    }
}
